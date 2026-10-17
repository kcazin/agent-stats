//! `agent-stats update` and `rollback`: fetch (or take a local file), verify, install, switch `current`, restart
//! the services and run the health trial; on failure switch back (decision 7).

use crate::layout::{
    self, activate, install_version, probe_version, record_active, record_update, stop_requested, valid_version,
    InstallLock, SignalGuard, Trial,
};
use crate::service::{installed_roles, read_role_status, restart_service};
use crate::{Context, Ctx, Res};
use agent_stats_common::{now_secs, pid_alive};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default, Debug, Clone)]
pub struct Opts {
    pub version: Option<String>,
    pub from: Option<PathBuf>,
    pub no_restart: bool,
}

pub fn cli(ctx: &Ctx, args: &[String]) -> Res<()> {
    const USAGE: &str =
        "usage: agent-stats update [--version V] [--from <binary-or-tar.gz>] [--no-restart] | update --auto";
    let mut o = Opts::default();
    let mut auto = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--auto" => {
                auto = true;
                i += 1;
            }
            "--version" => {
                o.version = Some(args.get(i + 1).ok_or(USAGE)?.trim_start_matches('v').to_string());
                i += 2;
            }
            "--from" => {
                o.from = Some(PathBuf::from(args.get(i + 1).ok_or(USAGE)?));
                i += 2;
            }
            "--no-restart" => {
                o.no_restart = true;
                i += 1;
            }
            _ => return Err(USAGE.into()),
        }
    }
    if auto {
        // The timer's mode: it picks the version and source itself and always runs the health trial.
        if o.version.is_some() || o.from.is_some() || o.no_restart {
            return Err(format!("{USAGE} (--auto takes no other option)"));
        }
        return crate::auto::run(ctx);
    }
    update(ctx, &o)
}

// ---------------------------------------------------------------- checksums

pub fn sha256_file(path: &Path) -> Res<String> {
    let mut f = fs::File::open(path).ctx(format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).ctx("read")?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks `file` against the `<hex>  <name>` line for `name` in `sums`.
pub fn verify_sum(sums: &str, name: &str, file: &Path) -> Res<()> {
    let want = sums
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (h, n) = (it.next()?, it.next()?);
            (n.trim_start_matches('*') == name).then(|| h.to_ascii_lowercase())
        })
        .next()
        .ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
    let got = sha256_file(file)?;
    if got == want {
        Ok(())
    } else {
        Err(format!("checksum mismatch for {name}: expected {want}, got {got}"))
    }
}

// ---------------------------------------------------------------- sources

/// Scratch for one update (`data_dir/.update-<pid>`), removed when dropped.
pub(crate) struct Workdir(pub(crate) PathBuf);

impl Workdir {
    pub(crate) fn new(ctx: &Ctx) -> Workdir {
        Workdir(ctx.data_dir.join(format!(".update-{}", std::process::id())))
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn asset_name(version: &str, target: &str) -> String {
    format!("agent-stats-{version}-{target}.tar.gz")
}

/// Largest release file accepted from GitHub or a hub.
pub(crate) const MAX_ASSET_BYTES: u64 = 500 * 1024 * 1024;

fn extract(tarball: &Path, into: &Path) -> Res<PathBuf> {
    fs::create_dir_all(into).ctx("create extract dir")?;
    let out = Command::new("tar").arg("-xzf").arg(tarball).arg("-C").arg(into).output().ctx("run tar")?;
    if !out.status.success() {
        return Err(format!("tar failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let bin = into.join(layout::BIN_NAME);
    let meta =
        fs::symlink_metadata(&bin).map_err(|_| format!("{} has no `agent-stats` at its root", tarball.display()))?;
    if !meta.is_file() {
        return Err("`agent-stats` in the archive is not a regular file".into());
    }
    Ok(bin)
}

fn is_archive(p: &Path) -> bool {
    let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    n.ends_with(".tar.gz") || n.ends_with(".tgz")
}

fn from_local(ctx: &Ctx, from: &Path, work: &Path) -> Res<PathBuf> {
    let _ = ctx;
    if !from.is_file() {
        return Err(format!("--from {}: no such file", from.display()));
    }
    if !is_archive(from) {
        return Ok(from.to_path_buf());
    }
    let sums = from.with_file_name("SHA256SUMS");
    match fs::read_to_string(&sums) {
        Ok(text) => {
            let name = from.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            verify_sum(&text, name, from)?;
            println!("checksum ok ({})", sums.display());
        }
        Err(_) => eprintln!("warning: no SHA256SUMS next to {}; archive not verified", from.display()),
    }
    extract(from, &work.join("extract"))
}

/// Where the new binary comes from. `update --auto` from a hub downloads and verifies the archive itself, then
/// hands it over as `Local`, exactly like `update --from <tarball>`.
pub(crate) enum Source<'a> {
    /// A binary or a `.tar.gz` (checked against a `SHA256SUMS` next to it).
    Local(&'a Path),
    /// A GitHub release: this version, or the latest.
    GitHub(Option<&'a str>),
}

/// Fetches and unpacks the new binary into `work`; returns it and its version. With `want`, the binary must
/// report that version.
pub(crate) fn fetch(ctx: &Ctx, src: Source, want: Option<&str>, work: &Path) -> Res<(PathBuf, String)> {
    let (bin, version, what) = match src {
        Source::Local(p) => {
            let bin = from_local(ctx, p, work)?;
            let v = probe_version(&bin)?;
            (bin, v, p.display().to_string())
        }
        Source::GitHub(requested) => {
            let (tarball, v) = download(ctx, requested, &work.join("dl"))?;
            (extract(&tarball, &work.join("extract"))?, v, format!("release v{}", requested.unwrap_or("(latest)")))
        }
    };
    match want {
        Some(w) if w != version => Err(format!("--version {w} but {what} is version {version}")),
        _ => Ok((bin, version)),
    }
}

// ---------------------------------------------------------------- GitHub releases

/// `gh auth token` rather than `gh auth status`, which fails when any stored account is broken, even an inactive one.
fn gh_ready(ctx: &Ctx) -> bool {
    Command::new(&ctx.gh)
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// How GitHub is reached: `gh` when it is logged in, else its HTTPS API (with `ctx.github_token` when there is one,
/// which a private fork needs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Via {
    Gh,
    Https,
}

impl Via {
    pub(crate) fn pick(ctx: &Ctx) -> Via {
        if gh_ready(ctx) {
            Via::Gh
        } else {
            Via::Https
        }
    }
    fn label(self) -> &'static str {
        match self {
            Via::Gh => "gh",
            Via::Https => "https",
        }
    }
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(600))).build().into()
}

fn http_get(ctx: &Ctx, url: &str, accept: &str) -> Res<ureq::http::Response<ureq::Body>> {
    let mut req = http_agent().get(url).header("User-Agent", "agent-stats").header("Accept", accept);
    if let Some(t) = &ctx.github_token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    req.call().map_err(|e| format!("GET {url}: {e}"))
}

fn http_json(ctx: &Ctx, url: &str) -> Res<serde_json::Value> {
    let mut body = http_get(ctx, url, "application/vnd.github+json")?.into_body();
    let text = body.read_to_string().ctx(format!("read {url}"))?;
    serde_json::from_str(&text).ctx("parse GitHub response")
}

/// Runs `gh <args>` (with `--dir <dir> --clobber` for a download) and returns its stdout.
fn run_gh(ctx: &Ctx, args: &[&str], dir: Option<&Path>) -> Res<String> {
    let mut cmd = Command::new(&ctx.gh);
    cmd.args(args).stdin(Stdio::null());
    if let Some(d) = dir {
        cmd.arg("--dir").arg(d).arg("--clobber");
    }
    let out = cmd.output().ctx("run gh")?;
    if !out.status.success() {
        let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        return Err(format!("gh {what} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A published GitHub release.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Release {
    pub version: String,
    /// Unix seconds; `None` when GitHub did not say.
    pub published_at: Option<u64>,
}

fn parse_time(s: Option<&str>) -> Option<u64> {
    s?.parse::<jiff::Timestamp>().ok().and_then(|t| u64::try_from(t.as_second()).ok())
}

/// The latest release of `ctx.repo`: its tag (without the `v`) and when it was published.
pub(crate) fn latest_release(ctx: &Ctx, via: Via) -> Res<Release> {
    let (tag, published) = match via {
        Via::Gh => {
            let out = run_gh(ctx, &["release", "view", "--repo", &ctx.repo, "--json", "tagName,publishedAt"], None)?;
            let j: serde_json::Value = serde_json::from_str(out.trim()).ctx("parse gh release view")?;
            (j["tagName"].as_str().map(str::to_owned), parse_time(j["publishedAt"].as_str()))
        }
        Via::Https => {
            let j = http_json(ctx, &format!("https://api.github.com/repos/{}/releases/latest", ctx.repo))?;
            (j["tag_name"].as_str().map(str::to_owned), parse_time(j["published_at"].as_str()))
        }
    };
    let version = tag.ok_or("latest release has no tag")?.trim_start_matches('v').to_string();
    if !valid_version(&version) {
        return Err(format!("invalid version {version:?}"));
    }
    Ok(Release { version, published_at: published })
}

/// `name` matches `pattern`, a file name with at most one `*`.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((pre, post)) => name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post),
        None => pattern == name,
    }
}

/// Downloads the assets of release `v<version>` whose names match `patterns` (file names, at most one `*` each)
/// into `dir`. Returns the names downloaded, sorted; every pattern must match at least one asset.
pub(crate) fn download_assets(ctx: &Ctx, via: Via, version: &str, patterns: &[&str], dir: &Path) -> Res<Vec<String>> {
    fs::create_dir_all(dir).ctx("create download dir")?;
    let tag = format!("v{version}");
    let mut names = match via {
        Via::Gh => {
            let mut args = vec!["release", "download", tag.as_str(), "--repo", ctx.repo.as_str()];
            for p in patterns {
                args.extend(["--pattern", p]);
            }
            run_gh(ctx, &args, Some(dir))?;
            fs::read_dir(dir)
                .ctx("read download dir")?
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| patterns.iter().any(|p| glob_match(p, n)))
                .collect()
        }
        Via::Https => {
            let rel = http_json(ctx, &format!("https://api.github.com/repos/{}/releases/tags/{tag}", ctx.repo))?;
            let assets: Vec<(String, u64)> = rel["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| Some((x["name"].as_str()?.to_owned(), x["id"].as_u64()?)))
                .filter(|(n, _)| !n.contains(['/', '\\']) && !n.starts_with('.'))
                .filter(|(n, _)| patterns.iter().any(|p| glob_match(p, n)))
                .collect();
            for (name, id) in &assets {
                let url = format!("https://api.github.com/repos/{}/releases/assets/{id}", ctx.repo);
                let resp = http_get(ctx, &url, "application/octet-stream")?;
                let mut reader = resp.into_body().into_with_config().limit(MAX_ASSET_BYTES).reader();
                let mut f = fs::File::create(dir.join(name)).ctx(format!("create {name}"))?;
                std::io::copy(&mut reader, &mut f).ctx(format!("download {name}"))?;
            }
            assets.into_iter().map(|(n, _)| n).collect::<Vec<_>>()
        }
    };
    names.sort();
    if let Some(p) = patterns.iter().find(|p| !names.iter().any(|n| glob_match(p, n))) {
        return Err(format!("release {tag} has no asset {p}"));
    }
    Ok(names)
}

/// Downloads the release asset and SHA256SUMS into `dir`, returns the tarball path and the version.
fn download(ctx: &Ctx, requested: Option<&str>, dir: &Path) -> Res<(PathBuf, String)> {
    let via = Via::pick(ctx);
    let version = match requested {
        Some(v) => v.to_string(),
        None => latest_release(ctx, via)?.version,
    };
    if !valid_version(&version) {
        return Err(format!("invalid version {version:?}"));
    }
    let asset = asset_name(&version, &ctx.target);
    println!("downloading {asset} from {} ({})", ctx.repo, via.label());
    download_assets(ctx, via, &version, &[&asset, "SHA256SUMS"], dir)?;
    let tarball = dir.join(&asset);
    let sums = fs::read_to_string(dir.join("SHA256SUMS")).ctx("read SHA256SUMS")?;
    verify_sum(&sums, &asset, &tarball)?;
    println!("checksum ok");
    Ok((tarball, version))
}

// ---------------------------------------------------------------- the health trial

/// Restarts every installed role and waits for each to publish a status with `version`, a live pid and a heartbeat
/// from after the restart, then to stay up (same pid) for `settle_secs`. Err is a one-line reason.
pub fn restart_and_trial(ctx: &Ctx, version: &str) -> Result<Vec<&'static str>, String> {
    trial(ctx, version, false)
}

/// As [`restart_and_trial`]; with `interruptible`, SIGHUP/SIGINT/SIGTERM (see `SignalGuard`) fail the trial.
fn trial(ctx: &Ctx, version: &str, interruptible: bool) -> Result<Vec<&'static str>, String> {
    let roles = installed_roles(ctx);
    if roles.is_empty() {
        return Ok(roles);
    }
    let interrupted = || interruptible && stop_requested();
    let restarted_at = now_secs();
    for r in &roles {
        if interrupted() {
            return Err("interrupted by a signal".into());
        }
        restart_service(ctx, r).map_err(|e| format!("restarting {r}: {e}"))?;
    }
    let started = Instant::now();
    let settle = Duration::from_secs(ctx.settle_secs);
    let mut pending: Vec<&str> = roles.clone();
    // role -> (pid, when its first good status was seen)
    let mut seen: HashMap<&str, (u32, Instant)> = HashMap::new();
    let mut last: Vec<(String, String)> = Vec::new();
    while !pending.is_empty() {
        if interrupted() {
            return Err("interrupted by a signal".into());
        }
        last.clear();
        let mut died: Option<String> = None;
        pending.retain(|role| {
            let st = read_role_status(ctx, role);
            if let Some((pid, at)) = seen.get(role) {
                // Already good once: it has to stay that way until the settle time is over.
                return match st {
                    Some(s) if s.version == version && s.pid == *pid && pid_alive(*pid) => {
                        if at.elapsed() >= settle {
                            false
                        } else {
                            last.push((role.to_string(), "settling".into()));
                            true
                        }
                    }
                    _ => {
                        died = Some(format!(
                            "{role}: pid {pid} stopped or restarted within {} s of its first heartbeat",
                            ctx.settle_secs
                        ));
                        false
                    }
                };
            }
            let why = match st {
                None => "no status file".to_string(),
                Some(s) if s.version != version => format!("still reporting version {}", s.version),
                Some(s) if !pid_alive(s.pid) => format!("pid {} is not alive", s.pid),
                Some(s) if s.heartbeat_at < restarted_at => "heartbeat older than the restart".to_string(),
                Some(s) => {
                    seen.insert(role, (s.pid, Instant::now()));
                    if settle.is_zero() {
                        return false;
                    }
                    "settling".to_string()
                }
            };
            last.push((role.to_string(), why));
            true
        });
        if let Some(d) = died {
            return Err(d);
        }
        if pending.is_empty() {
            break;
        }
        let waiting_for_first = pending.iter().any(|r| !seen.contains_key(r));
        if waiting_for_first && started.elapsed() >= Duration::from_secs(ctx.trial_secs) {
            let detail: Vec<String> = last.iter().map(|(r, w)| format!("{r}: {w}")).collect();
            return Err(format!("no healthy status within {} s ({})", ctx.trial_secs, detail.join("; ")));
        }
        std::thread::sleep(Duration::from_millis(ctx.poll_ms));
    }
    for r in &roles {
        let _ = writeln!(std::io::stdout(), "{r}: running {version}");
    }
    Ok(roles)
}

// ---------------------------------------------------------------- update / rollback

/// Under the install lock, any trial record is an orphan (its process died without finishing): switch back to the
/// version it came from, restart and verify it. Returns what happened, if anything.
pub fn recover_orphan(ctx: &Ctx) -> Res<Option<String>> {
    let mut st = ctx.load_state();
    let Some(t) = st.trial.take() else { return Ok(None) };
    let say =
        format!("found an interrupted update {} -> {} (pid {})", t.from.as_deref().unwrap_or("(none)"), t.to, t.pid);
    let _ = writeln!(std::io::stdout(), "{say}; it was never verified, rolling back");
    match t.from.as_deref().filter(|f| ctx.is_complete(f)) {
        Some(prev) => {
            activate(ctx, prev)?;
            st.active = Some(prev.to_string());
            st.previous = t.previous.clone();
            ctx.save_state(&st)?;
            let r = restart_and_trial(ctx, prev);
            let (status, reason) = match &r {
                Ok(_) => ("rolled-back", "update was interrupted; rolled back".to_string()),
                Err(e) => ("failed", format!("update was interrupted; rolled back but {prev} is unhealthy: {e}")),
            };
            record_update(ctx, Some(prev.to_string()), &t.to, status, Some(reason))?;
            r.map_err(|e| format!("{say}; rolled back to {prev} but the health check failed: {e}"))?;
            Ok(Some(format!("{say}; rolled back to {prev}")))
        }
        None => {
            ctx.save_state(&st)?;
            record_update(
                ctx,
                t.from.clone(),
                &t.to,
                "failed",
                Some("update was interrupted; nothing to roll back to".into()),
            )?;
            Ok(Some(format!("{say}; there is no previous version to go back to, so {} stays active", t.to)))
        }
    }
}

pub fn update(ctx: &Ctx, o: &Opts) -> Res<()> {
    fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
    let _lock = InstallLock::acquire(ctx)?;
    if let Some(note) = recover_orphan(ctx)? {
        println!("{note}");
    }
    let work = Workdir::new(ctx);
    let src = match &o.from {
        Some(p) => Source::Local(p),
        None => Source::GitHub(o.version.as_deref()),
    };
    let (bin, version) = fetch(ctx, src, o.version.as_deref(), &work.0)?;
    apply(ctx, &bin, &version, o.no_restart)?;
    Ok(())
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    AlreadyActive,
    /// Switched without a restart (`--no-restart`).
    Installed,
    Updated,
}

/// Why [`apply`] failed. `RolledBack`: the new version failed its health trial and the previous one is active
/// again (`update --auto` remembers such a version and skips it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyError {
    RolledBack(String),
    Other(String),
}

impl From<String> for ApplyError {
    fn from(e: String) -> Self {
        ApplyError::Other(e)
    }
}

impl From<ApplyError> for String {
    fn from(e: ApplyError) -> Self {
        match e {
            ApplyError::RolledBack(e) | ApplyError::Other(e) => e,
        }
    }
}

/// Installs `bin` as `version`, switches `current`, restarts the services and runs the health trial, switching back
/// on failure. The caller holds the install lock and has recovered any orphaned trial.
pub(crate) fn apply(ctx: &Ctx, bin: &Path, version: &str, no_restart: bool) -> Result<Applied, ApplyError> {
    let version = version.to_string();
    let old = ctx.current_version();
    if old.as_deref() == Some(version.as_str()) {
        println!("agent-stats {version} is already the active version");
        return Ok(Applied::AlreadyActive);
    }
    install_version(ctx, bin, &version)?;
    let before = ctx.load_state();
    if no_restart {
        activate(ctx, &version)?;
        record_active(ctx, &version)?;
        if let Some(note) = layout::link_bin(ctx)? {
            println!("note: {note}");
        }
        record_update(ctx, old, &version, "installed", Some("not restarted (--no-restart)".into()))?;
        println!("agent-stats {version} is active; services keep running the old binary until restarted");
        layout::prune(ctx, layout::KEEP_VERSIONS);
        return Ok(Applied::Installed);
    }
    // From here until the outcome is recorded, a trial record says "`to` is unverified". Signals only set a flag
    // that the trial polls, so an interrupted update rolls back; a hard kill leaves the record for the next command.
    let _signals = SignalGuard::install();
    let mut marked = before.clone();
    marked.trial = Some(Trial {
        from: old.clone(),
        to: version.clone(),
        pid: std::process::id(),
        started: now_secs(),
        previous: before.previous.clone(),
    });
    ctx.save_state(&marked)?;
    let switched = (|| -> Res<()> {
        activate(ctx, &version)?;
        record_active(ctx, &version)?;
        if let Some(note) = layout::link_bin(ctx)? {
            println!("note: {note}");
        }
        Ok(())
    })();
    if let Err(e) = switched {
        if let Some(prev) = &old {
            let _ = activate(ctx, prev);
        }
        let _ = ctx.save_state(&before);
        return Err(e.into());
    }
    match trial(ctx, &version, true) {
        Ok(_) => {
            record_update(ctx, old.clone(), &version, "ok", None)?;
            layout::prune(ctx, layout::KEEP_VERSIONS);
            println!("updated {} -> {version}", old.as_deref().unwrap_or("(none)"));
            Ok(Applied::Updated)
        }
        Err(reason) => match old {
            Some(prev) => {
                let _ = activate(ctx, &prev);
                let _ = ctx.save_state(&before);
                let again = trial(ctx, &prev, false);
                record_update(ctx, Some(prev.clone()), &version, "rolled-back", Some(reason.clone()))?;
                let extra = match again {
                    Ok(_) => String::new(),
                    Err(e) => format!(" (and {prev} did not come back cleanly: {e})"),
                };
                Err(ApplyError::RolledBack(format!(
                    "update to {version} failed the health check: {reason}; rolled back to {prev}{extra}"
                )))
            }
            None => {
                record_update(ctx, None, &version, "failed", Some(reason.clone()))?;
                Err(ApplyError::Other(format!("update to {version} failed the health check: {reason}; there was no previous version to go back to")))
            }
        },
    }
}

pub fn rollback(ctx: &Ctx) -> Res<()> {
    fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
    let _lock = InstallLock::acquire(ctx)?;
    // An interrupted update is rolled back first; that is the rollback the user is asking for.
    if let Some(note) = recover_orphan(ctx)? {
        println!("{note}");
        return Ok(());
    }
    let state = ctx.load_state();
    let prev = state.previous.clone().ok_or("no previous version recorded")?;
    if !ctx.is_complete(&prev) {
        return Err(format!("previous version {prev} is no longer installed"));
    }
    let cur = ctx.current_version();
    activate(ctx, &prev)?;
    record_active(ctx, &prev)?;
    println!("switched {} -> {prev}", cur.as_deref().unwrap_or("(none)"));
    match restart_and_trial(ctx, &prev) {
        Ok(_) => {
            record_update(ctx, cur, &prev, "ok", Some("rollback".into()))?;
            Ok(())
        }
        Err(reason) => {
            record_update(ctx, cur, &prev, "failed", Some(format!("rollback: {reason}")))?;
            Err(format!("rolled back to {prev} but the health check failed: {reason}"))
        }
    }
}
