//! `agent-stats update --auto` end to end, in the style of tests/ops.rs: temp dirs, a fake `gh` and fake service
//! managers for the GitHub source, a small in-test HTTP server for the hub source. Nothing touches the real home,
//! services or network.

use agent_stats_ops::auto::{self, AutoRecord, MIRROR_COMPLETE};
use agent_stats_ops::layout::{self, activate, install_version, InstallLock};
use agent_stats_ops::update::{self, Opts};
use agent_stats_ops::{Ctx, Platform};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

static N: AtomicU32 = AtomicU32::new(0);

const TARGET: &str = "x86_64-unknown-linux-gnu";
const OTHER_TARGET: &str = "aarch64-apple-darwin";
const OLD: &str = "2020-01-01T00:00:00Z";

struct Sandbox {
    root: PathBuf,
    ctx: Ctx,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake `agent-stats` that only answers `version`.
fn fake_bin(path: &Path, version: &str) {
    script(path, &format!("[ \"$1\" = version ] && echo \"agent-stats {version} (protocol 2)\"\nexit 0"));
}

fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

impl Sandbox {
    fn new() -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "agent-stats-auto-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        for d in ["home", "data", "state", "bin", "units", "agents", "fake", "src", "config", "remote"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut ctx = Ctx::from_env();
        ctx.platform = Platform::Linux;
        ctx.home = root.join("home");
        ctx.data_dir = root.join("data");
        ctx.state_dir = root.join("state");
        ctx.bin_dir = root.join("bin");
        ctx.unit_dir = root.join("units");
        ctx.agents_dir = root.join("agents");
        ctx.config_dir = root.join("config");
        ctx.update_toml = root.join("config/update.toml");
        ctx.systemctl = root.join("fake/systemctl").to_string_lossy().into();
        ctx.launchctl = root.join("fake/launchctl").to_string_lossy().into();
        ctx.loginctl = root.join("fake/loginctl").to_string_lossy().into();
        ctx.gh = root.join("fake/gh").to_string_lossy().into();
        ctx.uid = 501;
        ctx.user = "tester".into();
        ctx.trial_secs = 3;
        ctx.poll_ms = 50;
        ctx.settle_secs = 0;
        ctx.lock_wait_secs = 1;
        ctx.plutil = "/nonexistent/plutil".into();
        ctx.github_token = None;
        ctx.target = TARGET.into();
        let sb = Sandbox { root, ctx };
        sb.fake_systemctl();
        sb.fake_gh();
        sb
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("log")).unwrap_or_default()
    }

    /// systemctl logs its arguments; `restart` behaves like a service that starts and writes its status file,
    /// unless the version in `bad` is the one now current.
    fn fake_systemctl(&self) {
        let (r, data, state) = (self.root.display(), self.ctx.data_dir.display(), self.ctx.state_dir.display());
        script(
            &self.root.join("fake/systemctl"),
            &format!(
                r#"echo "systemctl $*" >> "{r}/log"
case "$2" in
  restart) role=${{3#agent-stats-}}; role=${{role%.service}}; [ "$role" = reporter ] && role=report
ver=$("{data}/current/agent-stats" version | cut -d' ' -f2)
[ "$ver" = "$(cat "{r}/bad" 2>/dev/null)" ] && exit 0
now=$(date +%s)
printf '{{"role":"%s","version":"%s","pid":%s,"startedAt":%s,"heartbeatAt":%s}}' "$role" "$ver" "{pid}" "$now" "$now" > "{state}/$role-status.json"
  ;;
esac
exit 0"#,
                pid = std::process::id()
            ),
        );
    }

    /// A logged-in `gh` whose latest release is `{root}/latest` (`<tag> <publishedAt>`) and whose assets are the
    /// files in `{root}/remote` (patterns honoured, as gh does).
    fn fake_gh(&self) {
        let r = self.root.display();
        script(
            &self.root.join("fake/gh"),
            &format!(
                r#"echo "gh $*" >> "{r}/log"
case "$1 $2" in
  "auth token") echo gho_test; exit 0;;
  "release view") read tag at < "{r}/latest"; printf '{{"tagName":"%s","publishedAt":"%s"}}\n' "$tag" "$at"; exit 0;;
  "release download") pats=""; while [ $# -gt 0 ]; do
      case "$1" in --dir) dir=$2; shift;; --pattern) pats="$pats $2"; shift;; esac; shift; done
    cd "{r}/remote" || exit 1
    for p in $pats; do for f in $p; do [ -f "$f" ] && cp "$f" "$dir/"; done; done; exit 0;;
esac
exit 1"#
            ),
        );
    }

    /// GitHub's latest release is `v<version>`, published at `at`.
    fn latest(&self, version: &str, at: &str) {
        fs::write(self.root.join("latest"), format!("v{version} {at}\n")).unwrap();
    }

    fn set_bad(&self, v: &str) {
        fs::write(self.root.join("bad"), v).unwrap();
    }

    /// Release archives of `version` for `targets` in `dir`, and a SHA256SUMS listing them (with zeroes for the
    /// checksums unless `good_sums`).
    fn release(&self, dir: &Path, version: &str, targets: &[&str], good_sums: bool) {
        fs::create_dir_all(dir).unwrap();
        let stage = self.root.join(format!("stage-{version}"));
        fs::create_dir_all(&stage).unwrap();
        fake_bin(&stage.join("agent-stats"), version);
        let mut sums = String::new();
        for t in targets {
            let name = update::asset_name(version, t);
            let tar = dir.join(&name);
            let st = std::process::Command::new("tar")
                .arg("-czf")
                .arg(&tar)
                .arg("-C")
                .arg(&stage)
                .arg("agent-stats")
                .status()
                .unwrap();
            assert!(st.success());
            let sum = if good_sums { update::sha256_file(&tar).unwrap() } else { "0".repeat(64) };
            sums.push_str(&format!("{sum}  {name}\n"));
        }
        sums.push_str(&format!("{}  other-file\n", "1".repeat(64)));
        fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    }

    fn install_current(&self, v: &str) {
        let src = self.root.join("src").join(format!("bin-{v}"));
        fake_bin(&src, v);
        install_version(&self.ctx, &src, v).unwrap();
        activate(&self.ctx, v).unwrap();
        layout::record_active(&self.ctx, v).unwrap();
    }

    fn fake_unit(&self, role: &str) {
        fs::write(self.ctx.service_file(role), "x").unwrap();
    }

    fn update_toml(&self, text: &str) {
        fs::write(&self.ctx.update_toml, text).unwrap();
    }

    fn record(&self) -> AutoRecord {
        auto::load_record(&self.ctx).expect("auto-update.json")
    }

    fn current(&self) -> Option<String> {
        self.ctx.current_version()
    }
}

fn outcome(r: &AutoRecord) -> (&str, Option<&str>) {
    (r.outcome.as_str(), r.version.as_deref())
}

// ---------------------------------------------------------------- source = github

#[test]
fn github_up_to_date_records_and_installs_nothing() {
    let sb = Sandbox::new();
    sb.update_toml("source = \"github\"\n");
    sb.install_current("0.3.0");
    sb.latest("0.3.0", OLD);
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("up-to-date", Some("0.3.0")));
    assert_eq!(r.source, "github");
    assert!(r.at > 0 && r.failed.is_empty());
    assert!(!sb.log().contains("release download"), "{}", sb.log());
    // Never a downgrade, and the record has the agreed shape.
    sb.latest("0.2.9", OLD);
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("up-to-date", Some("0.3.0")));
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(sb.ctx.auto_record_file()).unwrap()).unwrap();
    let mut keys: Vec<&str> = raw.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["at", "failed", "message", "outcome", "source", "version"]);
}

#[test]
fn github_release_younger_than_the_soak_time_waits() {
    let sb = Sandbox::new();
    sb.update_toml("source = \"github\"\nsoak_hours = 6\n");
    sb.install_current("0.2.0");
    sb.release(&sb.root.join("remote"), "0.3.0", &[TARGET], true);
    let an_hour_ago = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() - 3600).unwrap();
    sb.latest("0.3.0", &an_hour_ago.to_string());
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("waiting-soak", Some("0.3.0")));
    assert!(r.message.contains("1 h ago") && r.message.contains("6 h"), "{}", r.message);
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));
    // soak_hours = 0 takes it right away.
    sb.update_toml("source = \"github\"\nsoak_hours = 0\n");
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("updated", Some("0.3.0")));
}

#[test]
fn github_update_installs_restarts_and_records() {
    let sb = Sandbox::new();
    sb.update_toml("source = \"github\"\n");
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    sb.release(&sb.root.join("remote"), "0.3.0", &[TARGET], true);
    sb.latest("0.3.0", OLD);
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("updated", Some("0.3.0")));
    assert!(r.message.contains("0.2.0 -> 0.3.0"), "{}", r.message);
    assert_eq!(sb.current().as_deref(), Some("0.3.0"));
    let lu = sb.ctx.load_state().last_update.unwrap();
    assert_eq!((lu.status.as_str(), lu.to.as_str()), ("ok", "0.3.0"));
    assert!(sb.log().contains("systemctl --user restart agent-stats-reporter.service"), "{}", sb.log());
    // Not a hub machine: no mirror.
    assert!(!sb.ctx.releases_dir().exists());
}

#[test]
fn github_rollback_lands_on_the_failed_list_and_is_skipped_until_a_newer_version() {
    let sb = Sandbox::new();
    sb.update_toml("source = \"github\"\n");
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    sb.release(&sb.root.join("remote"), "0.3.0", &[TARGET], true);
    sb.latest("0.3.0", OLD);
    sb.set_bad("0.3.0");
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.starts_with("rolled-back (github)"), "{e}");
    let r = sb.record();
    assert_eq!(outcome(&r), ("rolled-back", Some("0.3.0")));
    assert_eq!(r.failed, ["0.3.0"]);
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));

    // The next run skips it without downloading, and exits 0.
    fs::remove_file(sb.root.join("log")).unwrap();
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("skipped-failed", Some("0.3.0")));
    assert_eq!(r.failed, ["0.3.0"]);
    assert!(!sb.log().contains("release download"), "{}", sb.log());

    // A manual update still takes it (only --auto skips failed versions); it is fine now.
    fs::remove_file(sb.root.join("bad")).unwrap();
    update::update(&sb.ctx, &Opts { version: Some("0.3.0".into()), ..Opts::default() }).unwrap();
    assert_eq!(sb.current().as_deref(), Some("0.3.0"));
    layout::activate(&sb.ctx, "0.2.0").unwrap();

    // A newer release installs and clears the older entry.
    sb.release(&sb.root.join("remote"), "0.3.1", &[TARGET], true);
    sb.latest("0.3.1", OLD);
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("updated", Some("0.3.1")));
    assert!(r.failed.is_empty(), "{:?}", r.failed);
}

#[test]
fn disabled_records_and_does_nothing() {
    let sb = Sandbox::new();
    sb.update_toml("auto = false\n");
    sb.install_current("0.2.0");
    sb.latest("0.3.0", OLD);
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!(outcome(&r), ("disabled", Some("0.2.0")));
    // Default source off the hub machine is the hub.
    assert_eq!(r.source, "hub");
    assert_eq!(sb.log(), "");
}

#[test]
fn bad_settings_are_an_error_and_the_cli_rejects_mixed_options() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    sb.update_toml("source = \"ftp\"\n");
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains("expected \"github\" or \"hub\""), "{e}");
    assert_eq!(sb.record().outcome, "error");
    sb.update_toml("sauce = \"hub\"\n");
    assert!(auto::run(&sb.ctx).unwrap_err().contains("sauce"));
    for args in [&["--auto", "--version", "0.3.0"][..], &["--from", "x.tar.gz", "--auto"], &["--auto", "--no-restart"]]
    {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert!(update::cli(&sb.ctx, &a).unwrap_err().starts_with("usage:"), "{a:?}");
    }
}

#[test]
fn a_held_install_lock_is_not_waited_for() {
    let mut sb = Sandbox::new();
    sb.update_toml("source = \"github\"\n");
    sb.install_current("0.2.0");
    sb.latest("0.3.0", OLD);
    sb.ctx.lock_wait_secs = 300;
    let held = InstallLock::acquire(&sb.ctx).unwrap();
    let started = std::time::Instant::now();
    auto::run(&sb.ctx).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(auto::load_record(&sb.ctx).is_none(), "outcome unchanged");
    drop(held);
}

// ---------------------------------------------------------------- the mirror (hub machine)

#[test]
fn hub_machine_mirrors_its_release_heals_and_keeps_two() {
    let sb = Sandbox::new();
    sb.fake_unit("hub"); // default source: github
    sb.install_current("0.3.0");
    sb.latest("0.3.0", OLD);
    let remote = sb.root.join("remote");
    sb.release(&remote, "0.3.0", &[TARGET, OTHER_TARGET], true);
    // Older mirrors and a dead process's scratch.
    let rel = sb.ctx.releases_dir();
    for v in ["0.1.0", "0.2.0"] {
        fs::create_dir_all(rel.join(v)).unwrap();
        fs::write(rel.join(v).join(MIRROR_COMPLETE), v).unwrap();
    }
    let stale = rel.join(format!(".tmp-0.2.0-{}", dead_pid()));
    fs::create_dir_all(&stale).unwrap();

    // One archive SHA256SUMS lists is missing: no mirror, an error, nothing half-built left behind.
    fs::remove_file(remote.join(update::asset_name("0.3.0", OTHER_TARGET))).unwrap();
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains("mirror") && e.contains(OTHER_TARGET), "{e}");
    assert_eq!(sb.record().outcome, "error");
    assert!(!rel.join("0.3.0").exists());
    assert!(fs::read_dir(&rel).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(&format!(".tmp-0.3.0-{}", std::process::id()))));

    // The next run heals it, though there is nothing to update.
    sb.release(&remote, "0.3.0", &[TARGET, OTHER_TARGET], true);
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("up-to-date", Some("0.3.0")));
    let m = rel.join("0.3.0");
    let mut files: Vec<String> =
        fs::read_dir(&m).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(
        files,
        [
            MIRROR_COMPLETE.to_string(),
            "SHA256SUMS".into(),
            update::asset_name("0.3.0", OTHER_TARGET),
            update::asset_name("0.3.0", TARGET),
        ]
    );
    let sums = fs::read_to_string(m.join("SHA256SUMS")).unwrap();
    for t in [TARGET, OTHER_TARGET] {
        let name = update::asset_name("0.3.0", t);
        update::verify_sum(&sums, &name, &m.join(&name)).unwrap();
    }
    // Kept: the active release and the newest other one.
    let mut left: Vec<String> =
        fs::read_dir(&rel).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["0.2.0", "0.3.0"]);

    // A complete mirror is not downloaded again.
    fs::remove_file(sb.root.join("log")).unwrap();
    auto::run(&sb.ctx).unwrap();
    assert!(!sb.log().contains("release download"), "{}", sb.log());
}

#[test]
fn managed_hub_mirrors_the_version_it_runs_and_installs_nothing() {
    let mut sb = Sandbox::new();
    sb.ctx.self_version = "0.3.0".into();
    // An install from before the app took over, still on 0.2.0, and a newer release: neither matters.
    sb.install_current("0.2.0");
    sb.latest("0.4.0", OLD);
    let remote = sb.root.join("remote");
    sb.release(&remote, "0.3.0", &[TARGET, OTHER_TARGET], true);
    let rel = sb.ctx.releases_dir();

    // No hub here: nothing at all.
    auto::managed_run(&sb.ctx, "Tokiscope").unwrap();
    assert!(!rel.exists());
    assert!(auto::load_record(&sb.ctx).is_none());

    // A hub: the running version is mirrored for its reporters, once.
    fs::write(sb.ctx.config_dir.join("hub.toml"), "").unwrap();
    auto::managed_run(&sb.ctx, "Tokiscope").unwrap();
    assert!(rel.join("0.3.0").join(MIRROR_COMPLETE).is_file());
    assert!(!rel.join("0.2.0").exists() && !rel.join("0.4.0").exists());
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"), "nothing installed");
    fs::remove_file(sb.root.join("log")).unwrap();
    auto::managed_run(&sb.ctx, "Tokiscope").unwrap();
    assert!(!sb.log().contains("release download"), "{}", sb.log());
}

#[test]
fn hub_machine_mirrors_the_version_it_just_installed() {
    let sb = Sandbox::new();
    sb.fake_unit("hub");
    sb.install_current("0.2.0");
    sb.release(&sb.root.join("remote"), "0.3.0", &[TARGET, OTHER_TARGET], true);
    sb.latest("0.3.0", OLD);
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("updated", Some("0.3.0")));
    assert!(sb.ctx.releases_dir().join("0.3.0").join(MIRROR_COMPLETE).is_file());
}

// ---------------------------------------------------------------- source = hub

/// A request the fake hub received: path and Authorization header.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A fake hub on 127.0.0.1: the well-known descriptor (`hub_id`, running `hub_version`), `GET /release` answering
/// `release` (with the right token only), and `/release/<v>/<file>` from `files`.
fn fake_hub(hub_id: &str, hub_version: &str, release: &str, files: PathBuf) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (s2, id, ver, release) = (seen.clone(), hub_id.to_string(), hub_version.to_string(), release.to_string());
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut rd = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if rd.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut auth = None;
            loop {
                let mut h = String::new();
                if rd.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("authorization") {
                        auth = Some(v.trim().to_string());
                    }
                }
            }
            s2.lock().unwrap().push((path.clone(), auth.clone()));
            let authed = auth.as_deref() == Some("Bearer tok");
            let (status, body): (u16, Vec<u8>) = if path == "/.well-known/agent-stats" {
                (200, format!(r#"{{"role":"hub","hubId":"{id}","version":"{ver}","protocol":2}}"#).into_bytes())
            } else if !authed {
                (401, b"{\"error\":\"unauthorized\"}".to_vec())
            } else if path == "/release" {
                (200, release.clone().into_bytes())
            } else if let Some(rest) = path.strip_prefix("/release/") {
                match rest.split_once('/').and_then(|(_, f)| fs::read(files.join(f)).ok()) {
                    Some(b) => (200, b),
                    None => (404, Vec::new()),
                }
            } else {
                (404, Vec::new())
            };
            let _ = write!(s, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    (url, seen)
}

impl Sandbox {
    /// Joins this machine to the hub `home` (id `hub1`, token `tok`) at `route`.
    fn join(&self, route: &str) {
        let text = format!(
            "[[hub]]\nname = \"home\"\nid = \"hub1\"\nroutes = [\"{route}\"]\ntoken_file = \"secrets/hub-hub1.token\"\n"
        );
        fs::write(self.ctx.config_dir.join("config.toml"), text).unwrap();
        agent_stats_common::write_secret(&self.ctx.config_dir.join("secrets/hub-hub1.token"), "tok").unwrap();
    }
}

fn offer(v: &str, targets: &[&str]) -> String {
    serde_json::json!({ "version": v, "targets": targets }).to_string()
}

#[test]
fn hub_update_downloads_with_the_token_verifies_and_installs() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    let files = sb.root.join("hubfiles");
    sb.release(&files, "0.3.0", &[TARGET], true);
    let (url, seen) = fake_hub("hub1", "0.3.0", &offer("0.3.0", &[TARGET, OTHER_TARGET]), files);
    sb.join(&url);
    auto::run(&sb.ctx).unwrap();
    let r = sb.record();
    assert_eq!((outcome(&r), r.source.as_str()), (("updated", Some("0.3.0")), "hub"));
    assert_eq!(sb.current().as_deref(), Some("0.3.0"));
    let seen = seen.lock().unwrap().clone();
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"/release/0.3.0/SHA256SUMS"), "{paths:?}");
    assert!(paths.contains(&format!("/release/0.3.0/{}", update::asset_name("0.3.0", TARGET)).as_str()));
    assert!(seen.iter().filter(|(p, _)| p.starts_with("/release")).all(|(_, a)| a.as_deref() == Some("Bearer tok")));
    // Then it is what the hub offers.
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("up-to-date", Some("0.3.0")));
}

#[test]
fn hub_checksum_mismatch_is_an_error_and_installs_nothing() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let files = sb.root.join("hubfiles");
    sb.release(&files, "0.3.0", &[TARGET], false);
    let (url, _) = fake_hub("hub1", "0.3.0", &offer("0.3.0", &[TARGET]), files);
    sb.join(&url);
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.starts_with("error (hub)") && e.contains("checksum mismatch"), "{e}");
    assert_eq!(sb.record().outcome, "error");
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));
    assert!(!sb.ctx.version_dir("0.3.0").exists());
    assert!(sb.record().failed.is_empty(), "a bad download is not a failed version");
}

#[test]
fn hub_with_the_wrong_id_gets_no_token() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let files = sb.root.join("hubfiles");
    sb.release(&files, "0.3.0", &[TARGET], true);
    let (url, seen) = fake_hub("someone-else", "0.3.0", &offer("0.3.0", &[TARGET]), files);
    sb.join(&url);
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains("is hub someone-else, expected hub1"), "{e}");
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().all(|(p, a)| p == "/.well-known/agent-stats" && a.is_none()), "{seen:?}");
}

#[test]
fn hub_with_nothing_to_offer_is_up_to_date() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let (url, _) = fake_hub("hub1", "0.3.0", r#"{"version":null,"targets":[]}"#, sb.root.join("hubfiles"));
    sb.join(&url);
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("up-to-date", Some("0.2.0")));
}

#[test]
fn never_newer_than_the_hub() {
    // The hub offers an older version than this machine runs: no downgrade.
    let sb = Sandbox::new();
    sb.install_current("0.4.0");
    let files = sb.root.join("hubfiles");
    sb.release(&files, "0.3.0", &[TARGET], true);
    let (url, _) = fake_hub("hub1", "0.3.0", &offer("0.3.0", &[TARGET]), files);
    sb.join(&url);
    auto::run(&sb.ctx).unwrap();
    assert_eq!(outcome(&sb.record()), ("up-to-date", Some("0.4.0")));
    assert_eq!(sb.current().as_deref(), Some("0.4.0"));

    // A hub offering a version newer than it runs itself is refused.
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let files = sb.root.join("hubfiles");
    sb.release(&files, "0.3.5", &[TARGET], true);
    let (url, _) = fake_hub("hub1", "0.3.0", &offer("0.3.5", &[TARGET]), files);
    sb.join(&url);
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains("runs 0.3.0 but offers 0.3.5"), "{e}");
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));

    // An archive whose binary is not the advertised version is refused.
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let mixed = sb.root.join("mixed");
    sb.release(&mixed, "0.3.5", &[TARGET], true);
    let advertised = update::asset_name("0.3.0", TARGET);
    fs::rename(mixed.join(update::asset_name("0.3.5", TARGET)), mixed.join(&advertised)).unwrap();
    let sum = update::sha256_file(&mixed.join(&advertised)).unwrap();
    fs::write(mixed.join("SHA256SUMS"), format!("{sum}  {advertised}\n")).unwrap();
    let (url, _) = fake_hub("hub1", "0.3.0", &offer("0.3.0", &[TARGET]), mixed);
    sb.join(&url);
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains("is version 0.3.5"), "{e}");
    assert_eq!(sb.current().as_deref(), Some("0.2.0"));
    assert!(!sb.ctx.version_dir("0.3.5").exists());
}

#[test]
fn hub_without_this_target_is_an_error() {
    let sb = Sandbox::new();
    sb.install_current("0.2.0");
    let (url, _) = fake_hub("hub1", "0.3.0", &offer("0.3.0", &[OTHER_TARGET]), sb.root.join("hubfiles"));
    sb.join(&url);
    let e = auto::run(&sb.ctx).unwrap_err();
    assert!(e.contains(&format!("not for {TARGET}")) && e.contains(OTHER_TARGET), "{e}");
}
