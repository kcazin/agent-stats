//! `agent-stats update --auto`: what the updater timer runs every ~15 minutes. Non-interactive: it decides whether
//! there is something to install, installs it the way `update` does (same verification, health trial and rollback),
//! records the outcome in `data_dir/auto-update.json` and prints one or two lines.
//!
//! The hub machine follows GitHub and keeps a mirror of its active release in `data_dir/releases/<v>/`, which the
//! hub serves to its reporters; every other machine follows a hub, so a reporter never runs a newer version than
//! its hub, and only the hub needs GitHub credentials.

use crate::layout::{cmp_versions, fsync_path, valid_version, InstallLock};
use crate::update::{
    apply, download_assets, fetch, latest_release, recover_orphan, verify_sum, Applied, ApplyError, Source, Via,
    Workdir, MAX_ASSET_BYTES,
};
use crate::{Context, Ctx, Res};
use agent_stats_common::{atomic_write, now_secs, pid_alive};
use agent_stats_reporter::release::{find_route, pick_hub, ReleaseHub};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Mirrored releases kept on the hub.
pub const KEEP_MIRRORED: usize = 2;
/// Marker written last into a mirrored release; the hub serves a release only once it exists.
pub const MIRROR_COMPLETE: &str = ".complete";
const DEFAULT_SOAK_HOURS: f64 = 6.0;

/// `config_dir()/update.toml`; every key is optional and a missing file means the defaults.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct UpdateToml {
    auto: Option<bool>,
    source: Option<String>,
    hub: Option<String>,
    soak_hours: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    GitHub,
    Hub,
}

impl Origin {
    fn name(self) -> &'static str {
        match self {
            Origin::GitHub => "github",
            Origin::Hub => "hub",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub auto: bool,
    pub source: Origin,
    /// source = hub: the joined hub to follow (default: the first in config.toml).
    pub hub: Option<String>,
    /// source = github: a release must have been published this long before it is taken.
    pub soak_hours: f64,
}

/// GitHub on the hub machine, the hub everywhere else.
fn default_source(ctx: &Ctx) -> Origin {
    if ctx.service_installed("hub") {
        Origin::GitHub
    } else {
        Origin::Hub
    }
}

pub fn settings(ctx: &Ctx) -> Res<Settings> {
    let path = &ctx.update_toml;
    let file: UpdateToml = match fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {}", path.display(), e.message()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UpdateToml::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let source = match file.source.as_deref() {
        None => default_source(ctx),
        Some("github") => Origin::GitHub,
        Some("hub") => Origin::Hub,
        Some(s) => return Err(format!("{}: source = {s:?}, expected \"github\" or \"hub\"", path.display())),
    };
    let soak_hours = file.soak_hours.unwrap_or(DEFAULT_SOAK_HOURS);
    if !soak_hours.is_finite() || soak_hours < 0.0 {
        return Err(format!("{}: soak_hours must be a number of hours >= 0", path.display()));
    }
    Ok(Settings { auto: file.auto.unwrap_or(true), source, hub: file.hub.filter(|h| !h.is_empty()), soak_hours })
}

// ---------------------------------------------------------------- bookkeeping

/// `data_dir/auto-update.json`, the outcome of the last run (`service status` shows it). `failed` lists versions that
/// failed their health trial here; `--auto` skips them until a newer version installs.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct AutoRecord {
    pub at: u64,
    /// `github` or `hub`.
    pub source: String,
    /// `up-to-date`, `waiting-soak`, `updated`, `rolled-back`, `skipped-failed`, `disabled` or `error`.
    pub outcome: String,
    /// The candidate version, else the active one.
    pub version: Option<String>,
    pub message: String,
    #[serde(default)]
    pub failed: Vec<String>,
}

impl Ctx {
    pub fn auto_record_file(&self) -> PathBuf {
        self.data_dir.join("auto-update.json")
    }
    pub fn releases_dir(&self) -> PathBuf {
        self.data_dir.join("releases")
    }
}

pub fn load_record(ctx: &Ctx) -> Option<AutoRecord> {
    serde_json::from_slice(&fs::read(ctx.auto_record_file()).ok()?).ok()
}

/// One run's result.
struct Outcome {
    kind: &'static str,
    version: Option<String>,
    message: String,
}

impl Outcome {
    fn new(kind: &'static str, version: Option<&str>, message: impl Into<String>) -> Outcome {
        Outcome { kind, version: version.map(str::to_owned), message: message.into() }
    }
    fn error(version: Option<&str>, message: impl Into<String>) -> Outcome {
        Outcome::new("error", version, message)
    }
    fn ok(&self) -> bool {
        !matches!(self.kind, "error" | "rolled-back")
    }
}

/// Writes the record and prints the line; Err (non-zero exit) for `error` and `rolled-back`.
fn finish(ctx: &Ctx, source: Origin, o: Outcome, mut failed: Vec<String>) -> Res<()> {
    // A version leaves the failed list once something at least as new is installed (it can never be a candidate
    // again: --auto does not downgrade).
    if let Some(active) = ctx.current_version() {
        failed.retain(|f| cmp_versions(f, &active) == Ordering::Greater);
    }
    failed.sort_by(|a, b| cmp_versions(a, b));
    failed.dedup();
    let message = o.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
    let rec = AutoRecord {
        at: now_secs(),
        source: source.name().into(),
        outcome: o.kind.into(),
        version: o.version.clone(),
        message: message.clone(),
        failed,
    };
    let bytes = serde_json::to_vec_pretty(&rec).ctx("encode auto-update.json")?;
    atomic_write(&ctx.auto_record_file(), &bytes, None).ctx("write auto-update.json")?;
    let line = format!("{} ({}): {message}", o.kind, source.name());
    if o.ok() {
        println!("{line}");
        Ok(())
    } else {
        Err(line)
    }
}

// ---------------------------------------------------------------- the run

pub fn run(ctx: &Ctx) -> Res<()> {
    fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
    let failed = load_record(ctx).map(|r| r.failed).unwrap_or_default();
    let active = ctx.current_version();
    let set = match settings(ctx) {
        Ok(s) => s,
        Err(e) => return finish(ctx, default_source(ctx), Outcome::error(active.as_deref(), e), failed),
    };
    if !set.auto {
        let o = Outcome::new("disabled", active.as_deref(), format!("auto = false in {}", ctx.update_toml.display()));
        return finish(ctx, set.source, o, failed);
    }
    // The timer comes back in a few minutes: never queue behind a running install/update.
    let Some(_lock) = InstallLock::try_acquire(ctx)? else {
        println!("another agent-stats install/update is running; trying again on the next run");
        return Ok(());
    };
    match recover_orphan(ctx) {
        Ok(Some(note)) => println!("{note}"),
        Ok(None) => {}
        Err(e) => return finish(ctx, set.source, Outcome::error(active.as_deref(), e), failed),
    }
    let mut failed = failed;
    let mut o = match set.source {
        Origin::GitHub => from_github(ctx, &set, &mut failed),
        Origin::Hub => from_hub(ctx, &set, &mut failed),
    };
    // The hub keeps the release it runs mirrored for its reporters, whatever happened above (a mirror that failed
    // earlier heals on the next run).
    if set.source == Origin::GitHub && ctx.service_installed("hub") {
        match mirror(ctx) {
            Ok(Some(note)) => println!("{note}"),
            Ok(None) => {}
            // An install happened and is recorded as such; the next run retries the mirror (and reports it).
            Err(e) if o.kind == "updated" => o.message = format!("{}; mirror failed: {e}", o.message),
            Err(e) if o.ok() => o = Outcome::error(o.version.as_deref(), format!("mirror: {e}")),
            Err(e) => o.message = format!("{}; mirror: {e}", o.message),
        }
    }
    finish(ctx, set.source, o, failed)
}

fn newer(a: &str, b: &str) -> bool {
    cmp_versions(a, b) == Ordering::Greater
}

/// Installs `bin` (already verified) and turns the result into an outcome; a rollback lands on the failed list.
fn install(ctx: &Ctx, bin: &Path, v: &str, from: Option<&str>, failed: &mut Vec<String>) -> Outcome {
    match apply(ctx, bin, v, false) {
        Ok(Applied::Updated) | Ok(Applied::Installed) => {
            Outcome::new("updated", Some(v), format!("updated {} -> {v}", from.unwrap_or("(none)")))
        }
        Ok(Applied::AlreadyActive) => Outcome::new("up-to-date", Some(v), format!("agent-stats {v} is active")),
        Err(ApplyError::RolledBack(e)) => {
            failed.push(v.to_string());
            Outcome::new("rolled-back", Some(v), format!("{e}; {v} will be skipped until a newer version"))
        }
        Err(ApplyError::Other(e)) => Outcome::error(Some(v), e),
    }
}

fn from_github(ctx: &Ctx, set: &Settings, failed: &mut Vec<String>) -> Outcome {
    let active = ctx.current_version();
    let a = active.as_deref();
    let rel = match latest_release(ctx, Via::pick(ctx)) {
        Ok(r) => r,
        Err(e) => return Outcome::error(a, format!("latest release of {}: {e}", ctx.repo)),
    };
    let v = rel.version.as_str();
    if let Some(cur) = a.filter(|cur| !newer(v, cur)) {
        let msg = if cur == v {
            format!("agent-stats {cur} is the latest release")
        } else {
            format!("agent-stats {cur} is active; the latest release is {v}")
        };
        return Outcome::new("up-to-date", Some(cur), msg);
    }
    let Some(published) = rel.published_at else {
        return Outcome::error(Some(v), format!("GitHub did not say when {v} was published"));
    };
    let soak = Duration::from_secs_f64(set.soak_hours * 3600.0).as_secs();
    let age = now_secs().saturating_sub(published);
    if age < soak {
        return Outcome::new(
            "waiting-soak",
            Some(v),
            format!("{v} was published {} ago; waiting until it is {} old", hours(age), hours(soak)),
        );
    }
    if failed.iter().any(|f| f == v) {
        return Outcome::new("skipped-failed", Some(v), skipped(v, a));
    }
    let work = Workdir::new(ctx);
    match fetch(ctx, Source::GitHub(Some(v)), Some(v), &work.0) {
        Ok((bin, _)) => install(ctx, &bin, v, a, failed),
        Err(e) => Outcome::error(Some(v), e),
    }
}

fn hours(secs: u64) -> String {
    let h = (secs as f64 / 360.0).round() / 10.0;
    if h >= 10.0 || h.fract() == 0.0 {
        format!("{h:.0} h")
    } else {
        format!("{h:.1} h")
    }
}

fn skipped(v: &str, active: Option<&str>) -> String {
    format!(
        "{v} failed its health check here before; staying on {} until a newer version",
        active.unwrap_or("the active version")
    )
}

// ---------------------------------------------------------------- source = hub

/// `GET <route>/release`.
#[derive(Deserialize, Debug)]
struct HubRelease {
    version: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
}

/// Requests to the hub carry its token, so redirects are never followed (the token must not reach another host).
fn hub_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(format!("agent-stats/{}", agent_stats_common::VERSION))
        .build()
        .into()
}

fn hub_get(agent: &ureq::Agent, hub: &ReleaseHub, url: &str) -> Res<ureq::Body> {
    let res = agent
        .get(url)
        .header("Authorization", format!("Bearer {}", hub.token))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = res.status().as_u16();
    if status == 200 {
        return Ok(res.into_body());
    }
    let why = match status {
        300..=399 => " (a redirect; not followed)".to_string(),
        401 => " (token rejected: join the hub again with a new pairing code)".into(),
        403 => " (this token may not download releases)".into(),
        404 => " (not there: the hub may predate release mirroring, or has not mirrored this release yet)".into(),
        _ => String::new(),
    };
    Err(format!("GET {url}: hub answered {status}{why}"))
}

/// Saves `url` as `dir/name`, refusing more than `limit` bytes.
fn hub_download(agent: &ureq::Agent, hub: &ReleaseHub, url: &str, dir: &Path, name: &str, limit: u64) -> Res<PathBuf> {
    let body = hub_get(agent, hub, url)?;
    let path = dir.join(name);
    let mut reader = body.into_with_config().limit(limit).reader();
    let mut f = fs::File::create(&path).ctx(format!("create {name}"))?;
    std::io::copy(&mut reader, &mut f).ctx(format!("download {name}"))?;
    Ok(path)
}

fn from_hub(ctx: &Ctx, set: &Settings, failed: &mut Vec<String>) -> Outcome {
    let active = ctx.current_version();
    let a = active.as_deref();
    let hub = match pick_hub(&ctx.config_dir, set.hub.as_deref()) {
        Ok(h) => h,
        Err(e) => return Outcome::error(a, e),
    };
    let route = match find_route(&hub) {
        Ok(r) => r,
        Err(e) => return Outcome::error(a, e),
    };
    let agent = hub_agent();
    let url = format!("{}/release", route.url);
    let rel: HubRelease = match hub_get(&agent, &hub, &url)
        .and_then(|b| b.into_with_config().limit(1 << 20).read_to_string().ctx(format!("read {url}")))
        .and_then(|t| serde_json::from_str(&t).map_err(|_| format!("{url} is not a release descriptor")))
    {
        Ok(r) => r,
        Err(e) => return Outcome::error(a, e),
    };
    let name = &hub.name;
    let Some(v) = rel.version.as_deref() else {
        return Outcome::new("up-to-date", a, format!("hub {name} has no release to offer yet"));
    };
    if !valid_version(v) {
        return Outcome::error(a, format!("hub {name} offers an invalid version {v:?}"));
    }
    if let Some(cur) = a.filter(|cur| !newer(v, cur)) {
        let msg = if cur == v {
            format!("agent-stats {cur} is what hub {name} offers")
        } else {
            format!("agent-stats {cur} is active; hub {name} offers {v}")
        };
        return Outcome::new("up-to-date", Some(cur), msg);
    }
    // The protocol rule: never newer than the hub itself runs.
    if let Some(hv) = route.version.as_deref().filter(|hv| newer(v, hv)) {
        return Outcome::error(Some(v), format!("hub {name} runs {hv} but offers {v}; not taking a newer version"));
    }
    if !rel.targets.contains(&ctx.target) {
        return Outcome::error(
            Some(v),
            format!(
                "hub {name} offers {v} but not for {} (it has: {})",
                ctx.target,
                if rel.targets.is_empty() { "none".into() } else { rel.targets.join(", ") }
            ),
        );
    }
    if failed.iter().any(|f| f == v) {
        return Outcome::new("skipped-failed", Some(v), skipped(v, a));
    }
    let work = Workdir::new(ctx);
    let dl = work.0.join("dl");
    let asset = crate::update::asset_name(v, &ctx.target);
    let base = format!("{}/release/{v}", route.url);
    println!("downloading {asset} from hub {name} ({})", route.url);
    let tarball = (|| -> Res<PathBuf> {
        fs::create_dir_all(&dl).ctx("create download dir")?;
        let sums = hub_download(&agent, &hub, &format!("{base}/SHA256SUMS"), &dl, "SHA256SUMS", 1 << 20)?;
        let tarball = hub_download(&agent, &hub, &format!("{base}/{asset}"), &dl, &asset, MAX_ASSET_BYTES)?;
        verify_sum(&fs::read_to_string(&sums).ctx("read SHA256SUMS")?, &asset, &tarball)?;
        Ok(tarball)
    })();
    let tarball = match tarball {
        Ok(t) => t,
        Err(e) => return Outcome::error(Some(v), e),
    };
    // From here on it is `update --from <tarball>` (which checks the SHA256SUMS next to it again).
    match fetch(ctx, Source::Local(&tarball), Some(v), &work.0) {
        Ok((bin, _)) => install(ctx, &bin, v, a, failed),
        Err(e) => Outcome::error(Some(v), e),
    }
}

// ---------------------------------------------------------------- the release mirror (hub machine)

/// Makes sure `releases/<active>/` holds SHA256SUMS and every `agent-stats-<v>-*.tar.gz` of the active release,
/// each verified, with [`MIRROR_COMPLETE`] written last. Built in `.tmp-<v>-<pid>` and renamed into place. Returns
/// a note when it built one. Callers hold the install lock.
pub fn mirror(ctx: &Ctx) -> Res<Option<String>> {
    let Some(v) = ctx.current_version() else {
        return Err("no active version to mirror".into());
    };
    mirror_version(ctx, &v)
}

/// What `update --auto` does when an app ships this binary: the app updates it, so there is nothing to install,
/// but a hub here still mirrors the release it runs for its reporters (by the hub.toml in the config dir).
pub fn managed_run(ctx: &Ctx, app: &str) -> Res<()> {
    if !ctx.config_dir.join("hub.toml").is_file() {
        println!("agent-stats here is part of {app}, which updates it; nothing to do");
        return Ok(());
    }
    fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
    let Some(_lock) = InstallLock::try_acquire(ctx)? else {
        println!("another agent-stats install/update is running; trying again on the next run");
        return Ok(());
    };
    match mirror_version(ctx, &ctx.self_version)? {
        Some(note) => println!("{note}"),
        None => println!("agent-stats here is part of {app}, which updates it; {} is mirrored", ctx.self_version),
    }
    Ok(())
}

/// [`mirror`] for version `v`.
fn mirror_version(ctx: &Ctx, v: &str) -> Res<Option<String>> {
    let v = v.to_string();
    let releases = ctx.releases_dir();
    let dest = releases.join(&v);
    if dest.join(MIRROR_COMPLETE).is_file() {
        prune_mirror(ctx, &v);
        return Ok(None);
    }
    fs::create_dir_all(&releases).ctx(format!("create {}", releases.display()))?;
    let tmp = releases.join(format!(".tmp-{v}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).ctx(format!("create {}", tmp.display()))?;
    let built = (|| -> Res<usize> {
        let pattern = format!("agent-stats-{v}-*.tar.gz");
        let names = download_assets(ctx, Via::pick(ctx), &v, &[&pattern, "SHA256SUMS"], &tmp)?;
        let sums = fs::read_to_string(tmp.join("SHA256SUMS")).ctx("read SHA256SUMS")?;
        let tarballs: Vec<&String> = names.iter().filter(|n| *n != "SHA256SUMS").collect();
        // Every archive SHA256SUMS lists must be here: a partial mirror would offer targets it cannot serve.
        let prefix = format!("agent-stats-{v}-");
        for listed in sums.lines().filter_map(|l| l.split_whitespace().nth(1)).map(|n| n.trim_start_matches('*')) {
            if listed.starts_with(&prefix) && listed.ends_with(".tar.gz") && !tarballs.iter().any(|t| *t == listed) {
                return Err(format!("SHA256SUMS lists {listed} but the release has no such asset"));
            }
        }
        for t in &tarballs {
            verify_sum(&sums, t, &tmp.join(t))?;
        }
        for n in &names {
            fsync_path(&tmp.join(n))?;
        }
        let marker = tmp.join(MIRROR_COMPLETE);
        fs::write(&marker, format!("{v}\n")).ctx("write .complete")?;
        fsync_path(&marker)?;
        fsync_path(&tmp)?;
        if dest.exists() {
            fs::remove_dir_all(&dest).ctx(format!("remove incomplete {}", dest.display()))?;
        }
        fs::rename(&tmp, &dest).ctx(format!("move into {}", dest.display()))?;
        fsync_path(&releases)?;
        Ok(tarballs.len())
    })();
    if built.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    let n = built?;
    prune_mirror(ctx, &v);
    Ok(Some(format!("mirrored {v} for reporters ({n} targets) in {}", dest.display())))
}

/// Keeps `active` and the newest other complete mirror ([`KEEP_MIRRORED`] in all); removes the rest, incomplete
/// version directories, and `.tmp-<v>-<pid>` of processes that are gone.
fn prune_mirror(ctx: &Ctx, active: &str) {
    let releases = ctx.releases_dir();
    let mut complete = Vec::new();
    for e in fs::read_dir(&releases).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        if let Some(rest) = name.strip_prefix(".tmp-") {
            let pid = rest.rsplit('-').next().and_then(|p| p.parse::<u32>().ok());
            if pid.is_none_or(|p| p != std::process::id() && !pid_alive(p)) {
                let _ = fs::remove_dir_all(&path);
            }
        } else if valid_version(&name) && path.is_dir() {
            if path.join(MIRROR_COMPLETE).is_file() {
                complete.push(name);
            } else {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
    complete.sort_by(|a, b| cmp_versions(b, a));
    let keep_others = KEEP_MIRRORED.saturating_sub(1);
    let mut kept = 0;
    for v in complete.iter().filter(|v| v.as_str() != active) {
        if kept < keep_others {
            kept += 1;
        } else {
            let _ = fs::remove_dir_all(releases.join(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.9.9", "0.10.0"));
        assert!(!newer("0.4.1", "0.4.1"));
        assert!(newer("0.4.10", "0.4.9"));
        assert!(newer("0.6.0", "0.6.0-rc.1"));
        assert!(newer("0.6.0-rc.1", "0.5.0"));
        assert!(newer("0.6.0-rc.10", "0.6.0-rc.9"));
        assert!(newer("0.6.1-rc.1", "0.6.0"));
        assert!(!newer("0.6.0-rc.1", "0.6.0"));
        assert!(!newer("0.6.0+build.2", "0.6.0"));
    }

    #[test]
    fn hours_reads_well() {
        assert_eq!(hours(3600 * 6), "6 h");
        assert_eq!(hours(5400), "1.5 h");
        assert_eq!(hours(3600 * 30 + 100), "30 h");
        assert_eq!(hours(3603), "1 h");
    }
}
