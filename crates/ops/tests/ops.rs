//! End-to-end tests against temp dirs with fake `systemctl` / `launchctl` / `gh` scripts. Nothing here touches the
//! real home, services or network: every path and program lives in the `Ctx` the test builds.

use agent_stats_ops::layout::{self, activate, install_version, link_bin, prune, InstallLock, Trial, SENTINEL};
use agent_stats_ops::service::{self, carried_env};
use agent_stats_ops::update::{self, Opts};
use agent_stats_ops::{Ctx, Platform};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

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

impl Sandbox {
    fn new(platform: Platform) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "agent-stats-ops-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        for d in ["home", "data", "state", "bin", "units", "agents", "fake", "src"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut ctx = Ctx::from_env();
        ctx.platform = platform;
        ctx.home = root.join("home");
        ctx.data_dir = root.join("data");
        ctx.state_dir = root.join("state");
        ctx.bin_dir = root.join("bin");
        ctx.unit_dir = root.join("units");
        ctx.agents_dir = root.join("agents");
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
        ctx.launchd_wait_secs = 2;
        ctx.plutil = "/nonexistent/plutil".into();
        ctx.github_token = None;
        ctx.target = "x86_64-unknown-linux-gnu".into();
        let sb = Sandbox { root, ctx };
        sb.fake_managers();
        sb
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("log")).unwrap_or_default()
    }

    /// systemctl/launchctl log their arguments; on `restart`/`kickstart` they behave like a service that starts
    /// and writes its status file, unless the version in `bad` is the one now current.
    fn fake_managers(&self) {
        let (r, data, state) = (self.root.display(), self.ctx.data_dir.display(), self.ctx.state_dir.display());
        let start = format!(
            r#"ver=$("{data}/current/agent-stats" version | cut -d' ' -f2)
[ "$ver" = "$(cat "{r}/bad" 2>/dev/null)" ] && exit 0
cp "{data}/install-state.json" "{r}/during.json" 2>/dev/null
now=$(date +%s)
pid=$(cat "{r}/pid" 2>/dev/null || echo {pid})
printf '{{"role":"%s","version":"%s","pid":%s,"startedAt":%s,"heartbeatAt":%s}}' "$role" "$ver" "$pid" "$now" "$now" > "{state}/$role-status.json""#,
            pid = std::process::id()
        );
        script(
            &self.root.join("fake/systemctl"),
            &format!(
                r#"echo "systemctl $*" >> "{r}/log"
case "$2" in
  restart) role=${{3#agent-stats-}}; role=${{role%.service}}; [ "$role" = reporter ] && role=report
{start}
  ;;
esac
exit 0"#
            ),
        );
        script(
            &self.root.join("fake/launchctl"),
            &format!(
                r#"echo "launchctl $*" >> "{r}/log"
case "$1" in
  kickstart) role=${{3##*.}}
{start}
  ;;
  print) echo "state = running";;
  bootstrap) case "$3" in *dev.agent-stats*)
    if [ -f "{r}/nogui" ]; then echo "Bootstrap failed: 125: Domain does not support specified action" >&2; exit 125; fi
    [ -f "{r}/failnew" ] && exit 5;; esac;;
esac
exit 0"#
            ),
        );
        script(
            &self.root.join("fake/loginctl"),
            &format!("echo \"loginctl $*\" >> \"{r}/log\"\nexit ${{LINGER_EXIT:-0}}"),
        );
    }

    fn set_bad(&self, v: &str) {
        fs::write(self.root.join("bad"), v).unwrap();
    }

    /// A release archive (+ SHA256SUMS) for `version` in `dir`.
    fn release(&self, dir: &Path, version: &str, good_sums: bool) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let stage = self.root.join(format!("stage-{version}"));
        fs::create_dir_all(&stage).unwrap();
        fake_bin(&stage.join("agent-stats"), version);
        let name = update::asset_name(version, &self.ctx.target);
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
        fs::write(dir.join("SHA256SUMS"), format!("{sum}  {name}\n{}  other-file\n", "1".repeat(64))).unwrap();
        tar
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
}

#[test]
fn install_layout_sentinel_and_bin_link() {
    let sb = Sandbox::new(Platform::Linux);
    let src = sb.root.join("src/agent-stats");
    fake_bin(&src, "1.2.3");
    let dir = install_version(&sb.ctx, &src, "1.2.3").unwrap();
    assert_eq!(dir, sb.ctx.data_dir.join("versions/1.2.3"));
    assert_eq!(fs::read_to_string(dir.join(SENTINEL)).unwrap().trim(), "1.2.3");
    assert_eq!(fs::metadata(dir.join("agent-stats")).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(sb.ctx.is_complete("1.2.3"));
    // No staging leftovers.
    assert!(fs::read_dir(sb.ctx.versions_dir()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".staging")));

    activate(&sb.ctx, "1.2.3").unwrap();
    assert_eq!(fs::read_link(sb.ctx.current_link()).unwrap(), Path::new("versions/1.2.3"));
    assert_eq!(sb.ctx.current_version().as_deref(), Some("1.2.3"));

    // ~/.local/bin is created if missing and a note is returned when it is not on PATH.
    fs::remove_dir_all(&sb.ctx.bin_dir).unwrap();
    let note = link_bin(&sb.ctx).unwrap();
    assert!(note.unwrap().contains("not on your PATH"));
    assert_eq!(fs::read_link(sb.ctx.bin_dir.join("agent-stats")).unwrap(), sb.ctx.current_bin());
}

#[test]
fn install_rejects_wrong_version_and_bad_names() {
    let sb = Sandbox::new(Platform::Linux);
    let src = sb.root.join("src/agent-stats");
    fake_bin(&src, "9.9.9");
    let e = install_version(&sb.ctx, &src, "1.0.0").unwrap_err();
    assert!(e.contains("reports version 9.9.9"), "{e}");
    assert!(!sb.ctx.version_dir("1.0.0").exists());
    assert!(install_version(&sb.ctx, &src, "../evil").is_err());
}

#[test]
fn retention_keeps_three_but_never_current_or_previous() {
    let sb = Sandbox::new(Platform::Linux);
    for v in ["0.1.0", "0.2.0", "0.3.0", "0.9.0", "0.10.0"] {
        sb.install_current(v);
    }
    // current = 0.10.0, previous = 0.9.0. Roll current back to 0.1.0 so an old version is protected too.
    activate(&sb.ctx, "0.1.0").unwrap();
    let mut st = sb.ctx.load_state();
    st.previous = Some("0.2.0".into());
    sb.ctx.save_state(&st).unwrap();
    prune(&sb.ctx, 3);
    let left = sb.ctx.installed_versions();
    assert!(left.contains(&"0.1.0".to_string()), "current kept: {left:?}");
    assert!(left.contains(&"0.2.0".to_string()), "previous kept: {left:?}");
    assert_eq!(left, ["0.1.0", "0.2.0", "0.10.0"].map(String::from));

    // Plain case: newest three survive.
    let sb = Sandbox::new(Platform::Linux);
    for v in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.10.0"] {
        sb.install_current(v);
    }
    prune(&sb.ctx, 3);
    assert_eq!(sb.ctx.installed_versions(), ["0.3.0", "0.4.0", "0.10.0"].map(String::from));
}

#[test]
fn update_from_tarball_with_matching_sums_restarts_and_records() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    sb.fake_unit("hub");
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", true);
    update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap();
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.3.0"));
    let st = sb.ctx.load_state();
    assert_eq!((st.active.as_deref(), st.previous.as_deref()), (Some("0.3.0"), Some("0.2.0")));
    let lu = st.last_update.unwrap();
    assert_eq!((lu.status.as_str(), lu.to.as_str(), lu.from.as_deref()), ("ok", "0.3.0", Some("0.2.0")));
    let log = sb.log();
    assert!(log.contains("systemctl --user restart agent-stats-reporter.service"), "{log}");
    assert!(log.contains("systemctl --user restart agent-stats-hub.service"), "{log}");
    assert!(sb.ctx.is_complete("0.2.0"), "previous stays installed");
    assert!(!sb.ctx.data_dir.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".update-")));
}

#[test]
fn update_with_mismatching_sums_changes_nothing() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", false);
    let e = update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap_err();
    assert!(e.contains("checksum mismatch"), "{e}");
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
    assert!(!sb.ctx.version_dir("0.3.0").exists());
    assert!(!sb.log().contains("restart"));
}

#[test]
fn update_from_plain_binary_and_no_restart() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    let bin = sb.root.join("src/new-agent-stats");
    fake_bin(&bin, "0.2.1");
    update::update(&sb.ctx, &Opts { from: Some(bin), no_restart: true, ..Opts::default() }).unwrap();
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.1"));
    assert_eq!(sb.ctx.load_state().last_update.unwrap().status, "installed");
    assert!(!sb.log().contains("restart"));
}

#[test]
fn health_trial_failure_rolls_back_automatically() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    sb.set_bad("0.4.0"); // the "service" never publishes a status for this version
    let tar = sb.release(&sb.root.join("rel"), "0.4.0", true);
    let e = update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap_err();
    assert!(e.contains("rolled back to 0.2.0"), "{e}");
    assert!(e.contains("report:"), "{e}");
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
    let st = sb.ctx.load_state();
    assert_eq!(st.active.as_deref(), Some("0.2.0"));
    let lu = st.last_update.unwrap();
    assert_eq!((lu.status.as_str(), lu.to.as_str()), ("rolled-back", "0.4.0"));
    assert!(lu.reason.unwrap().contains("no healthy status"));
    assert_eq!(
        sb.log().matches("--user restart agent-stats-reporter.service").count(),
        2,
        "restarted forward and back"
    );
    // The old version's service is healthy again.
    assert_eq!(service::read_role_status(&sb.ctx, "report").unwrap().version, "0.2.0");
}

#[test]
fn stale_status_with_old_version_fails_the_trial() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    // Status written by "the old process" and never refreshed: restart does nothing for 0.5.0.
    sb.set_bad("0.5.0");
    fs::write(
        sb.ctx.state_dir.join("report-status.json"),
        format!(
            r#"{{"role":"report","version":"0.2.0","pid":{},"startedAt":1,"heartbeatAt":{}}}"#,
            std::process::id(),
            agent_stats_common::now_secs()
        ),
    )
    .unwrap();
    let tar = sb.release(&sb.root.join("rel"), "0.5.0", true);
    let e = update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap_err();
    assert!(e.contains("still reporting version 0.2.0"), "{e}");
}

#[test]
fn rollback_switches_to_previous() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.install_current("0.3.0");
    sb.fake_unit("report");
    update::rollback(&sb.ctx).unwrap();
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
    let st = sb.ctx.load_state();
    assert_eq!((st.active.as_deref(), st.previous.as_deref()), (Some("0.2.0"), Some("0.3.0")));
    assert_eq!(service::read_role_status(&sb.ctx, "report").unwrap().version, "0.2.0");
}

#[test]
fn update_downloads_with_gh_and_verifies() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    let rel = sb.root.join("remote");
    sb.release(&rel, "0.3.0", true);
    script(
        &sb.root.join("fake/gh"),
        &format!(
            r#"echo "gh $*" >> "{r}/log"
case "$1 $2" in
  "auth token") echo gho_test; exit 0;;
  "release view") echo '{{"tagName":"v0.3.0","publishedAt":"2026-01-01T00:00:00Z"}}'; exit 0;;
  "release download") while [ $# -gt 0 ]; do [ "$1" = --dir ] && dir=$2; shift; done
    cp "{rel}"/* "$dir"/; exit 0;;
esac
exit 1"#,
            r = sb.root.display(),
            rel = rel.display()
        ),
    );
    update::update(&sb.ctx, &Opts::default()).unwrap();
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.3.0"));
    let log = sb.log();
    assert!(log.contains("release download v0.3.0 --repo kcazin/agent-stats --pattern agent-stats-0.3.0-x86_64-unknown-linux-gnu.tar.gz"), "{log}");
}

#[test]
fn service_install_linux_carries_legacy_env_and_installs_running_binary() {
    let sb = Sandbox::new(Platform::Linux);
    fs::write(
        sb.ctx.unit_path("report"),
        "[Service]\nExecStart=\"/usr/bin/node\" \"/old/reporter.mjs\"\nEnvironment=\"PATH=/legacy\"\nEnvironment=\"HUB_URL=http://100.1.2.3:7777\"\nEnvironment=\"STATS_LABEL=Old Label\"\nEnvironment=\"STATS_MACHINE=cachyos\"\nEnvironment=\"UNRELATED=1\"\n",
    )
    .unwrap();
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    service::install(&ctx, "report", Some("New \"Label\"")).unwrap();

    assert_eq!(ctx.current_version().as_deref(), Some("0.7.0"));
    let unit = fs::read_to_string(ctx.unit_path("report")).unwrap();
    assert!(
        unit.contains(&format!("ExecStart=\"{}/current/agent-stats\" \"report\"", ctx.data_dir.display())),
        "{unit}"
    );
    assert!(unit.contains("Environment=\"HUB_URL=http://100.1.2.3:7777\""));
    assert!(unit.contains("Environment=\"STATS_MACHINE=cachyos\""));
    assert!(unit.contains("Environment=\"STATS_LABEL=New \\\"Label\\\"\""), "{unit}");
    assert!(!unit.contains("Old Label") && !unit.contains("UNRELATED") && !unit.contains("/legacy"));
    assert!(unit.contains("Nice=10") && unit.contains("RestartSec=10") && unit.contains("Restart=always"));
    let log = sb.log();
    let order: Vec<usize> = [
        "--user daemon-reload",
        "--user enable agent-stats-reporter.service",
        "--user restart agent-stats-reporter.service",
        "loginctl enable-linger tester",
    ]
    .iter()
    .map(|n| log.find(n).unwrap_or_else(|| panic!("missing {n} in {log}")))
    .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{log}");
    assert!(ctx.bin_dir.join("agent-stats").is_symlink());

    // Re-install keeps the (now carried) env without --label.
    service::install(&ctx, "report", None).unwrap();
    assert!(fs::read_to_string(ctx.unit_path("report")).unwrap().contains("STATS_LABEL=New \\\"Label\\\""));

    service::uninstall(&ctx, "report").unwrap();
    assert!(!ctx.unit_path("report").exists());
    assert!(sb.log().contains("disable --now agent-stats-reporter.service"));
}

#[test]
fn hub_unit_uses_serve_and_names() {
    let sb = Sandbox::new(Platform::Linux);
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    service::install(&ctx, "hub", None).unwrap();
    service::install(&ctx, "relay", None).unwrap();
    let hub = fs::read_to_string(ctx.unit_dir.join("agent-stats-hub.service")).unwrap();
    assert!(hub.contains("\"hub\" \"serve\"") && !hub.contains("Nice="));
    assert!(fs::read_to_string(ctx.unit_dir.join("agent-stats-relay.service"))
        .unwrap()
        .contains("\"relay\" \"serve\""));
    assert!(service::install(&ctx, "bogus", None).unwrap_err().starts_with("usage:"));
}

#[test]
fn linger_failure_only_warns() {
    let sb = Sandbox::new(Platform::Linux);
    script(&sb.root.join("fake/loginctl"), "exit 1");
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    service::install(&ctx, "report", None).unwrap();
    assert!(ctx.unit_path("report").exists());
}

#[test]
fn service_install_macos_replaces_legacy_plist() {
    let sb = Sandbox::new(Platform::Mac);
    let legacy = sb.ctx.plist_path("com.kilcazn.agent-stats.reporter");
    fs::write(
        &legacy,
        r#"<?xml version="1.0"?><plist version="1.0"><dict>
  <key>Label</key><string>com.kilcazn.agent-stats.reporter</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>/x</string>
    <key>HUB_URL</key><string>http://100.101.102.103:7777</string>
    <key>STATS_LABEL</key><string>MacBook &amp; Co</string>
  </dict>
  <key>KeepAlive</key><true/></dict></plist>"#,
    )
    .unwrap();
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    assert_eq!(carried_env(&ctx, "report").len(), 2);
    service::install(&ctx, "report", None).unwrap();

    assert!(!legacy.exists(), "legacy plist deleted");
    assert!(!ctx.agents_dir.join("com.kilcazn.agent-stats.reporter.plist.bak").exists(), "backup dropped on success");
    let plist = fs::read_to_string(ctx.plist_path("dev.agent-stats.report")).unwrap();
    assert!(
        plist.contains("<string>http://100.101.102.103:7777</string>")
            && plist.contains("<string>MacBook &amp; Co</string>"),
        "{plist}"
    );
    assert!(plist.contains("<string>report</string>") && plist.contains("<key>ThrottleInterval</key>"));
    let log = sb.log();
    let boot_old = log.find("launchctl bootout gui/501/com.kilcazn.agent-stats.reporter").expect(&log);
    let boot_new = log.find("launchctl bootstrap gui/501").expect(&log);
    assert!(boot_old < boot_new, "{log}");
    assert!(!log.contains("loginctl"));

    // Restart path used by update.
    sb.install_current("0.7.1");
    update::rollback(&ctx).unwrap();
    assert!(sb.log().contains("launchctl kickstart -k gui/501/dev.agent-stats.report"));
    service::uninstall(&ctx, "report").unwrap();
    assert!(!ctx.plist_path("dev.agent-stats.report").exists());
}

#[test]
fn service_status_reports_without_failing() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    service::status(&sb.ctx).unwrap();
}

// ---------------------------------------------------------------- review fixes

fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// Leaves the sandbox as an update 0.2.0 -> 0.3.0 killed in the middle of its trial.
fn orphan_trial(sb: &Sandbox) {
    sb.install_current("0.2.0");
    sb.install_current("0.3.0");
    let mut st = sb.ctx.load_state();
    st.trial =
        Some(Trial { from: Some("0.2.0".into()), to: "0.3.0".into(), pid: dead_pid(), started: 1, previous: None });
    sb.ctx.save_state(&st).unwrap();
}

#[test]
fn trial_record_exists_during_the_trial_and_is_gone_after() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", true);
    update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap();
    let during = fs::read_to_string(sb.root.join("during.json")).unwrap();
    assert!(during.contains("\"trial\"") && during.contains("\"to\": \"0.3.0\""), "{during}");
    let st = sb.ctx.load_state();
    assert!(st.trial.is_none());
    assert!(!fs::read_to_string(sb.ctx.state_file()).unwrap().contains("trial"));
}

#[test]
fn orphaned_trial_is_rolled_back_by_rollback() {
    let sb = Sandbox::new(Platform::Linux);
    orphan_trial(&sb);
    sb.fake_unit("report");
    update::rollback(&sb.ctx).unwrap();
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
    let st = sb.ctx.load_state();
    assert!(st.trial.is_none());
    assert_eq!(st.active.as_deref(), Some("0.2.0"));
    assert_eq!(st.last_update.unwrap().status, "rolled-back");
    assert!(sb.log().contains("restart agent-stats-reporter.service"));
}

#[test]
fn update_to_the_orphaned_version_does_not_claim_it_is_already_active() {
    let sb = Sandbox::new(Platform::Linux);
    orphan_trial(&sb);
    sb.fake_unit("report");
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", true);
    update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap();
    // Rolled back to 0.2.0 first, then trialled 0.3.0 for real: two restarts, ends verified.
    assert_eq!(sb.log().matches("--user restart agent-stats-reporter.service").count(), 2, "{}", sb.log());
    let st = sb.ctx.load_state();
    assert!(st.trial.is_none());
    assert_eq!((st.active.as_deref(), st.last_update.unwrap().status.as_str()), (Some("0.3.0"), "ok"));
}

#[test]
fn service_status_reports_an_orphaned_trial() {
    let sb = Sandbox::new(Platform::Linux);
    orphan_trial(&sb);
    service::status(&sb.ctx).unwrap();
    assert!(sb.ctx.load_state().trial.is_some(), "status only reports");
}

#[test]
fn trial_fails_when_the_process_dies_after_its_first_heartbeat() {
    let mut sb = Sandbox::new(Platform::Linux);
    sb.ctx.settle_secs = 3;
    sb.ctx.trial_secs = 5;
    sb.install_current("0.2.0");
    sb.fake_unit("report");
    // The "service" is a process that exits 1 s after it wrote its status.
    let mut child = std::process::Command::new("sleep").arg("1").spawn().unwrap();
    fs::write(sb.root.join("pid"), child.id().to_string()).unwrap();
    let reaper = std::thread::spawn(move || child.wait().unwrap());
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", true);
    let e = update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap_err();
    reaper.join().unwrap();
    assert!(e.contains("stopped or restarted"), "{e}");
    assert!(e.contains("rolled back to 0.2.0"), "{e}");
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
}

#[test]
fn install_lock_is_exclusive_and_update_waits_then_fails() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    let held = InstallLock::acquire(&sb.ctx).unwrap();
    let e = InstallLock::acquire(&sb.ctx).err().unwrap();
    assert!(e.contains("another agent-stats install/update") && e.contains(&std::process::id().to_string()), "{e}");
    let tar = sb.release(&sb.root.join("rel"), "0.3.0", true);
    assert!(update::update(&sb.ctx, &Opts { from: Some(tar), ..Opts::default() }).unwrap_err().contains("another"));
    assert!(update::rollback(&sb.ctx).unwrap_err().contains("another"));
    assert_eq!(sb.ctx.current_version().as_deref(), Some("0.2.0"));
    drop(held);
    InstallLock::acquire(&sb.ctx).unwrap();
}

#[test]
fn prune_removes_scratch_of_dead_processes_only() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    let (dead, me) = (dead_pid(), std::process::id());
    for d in [format!(".update-{dead}"), format!(".update-{me}"), format!(".install-{dead}")] {
        fs::create_dir_all(sb.ctx.data_dir.join(&d).join("extract")).unwrap();
    }
    fs::write(sb.ctx.data_dir.join(format!(".current.tmp{dead}")), "x").unwrap();
    prune(&sb.ctx, 3);
    let left: Vec<String> =
        fs::read_dir(&sb.ctx.data_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert!(left.contains(&format!(".update-{me}")), "{left:?}");
    assert!(!left.iter().any(|n| n.contains(&dead.to_string())), "{left:?}");
}

#[test]
fn install_never_replaces_a_complete_version() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.2.0");
    let marker = sb.ctx.version_dir("0.2.0").join("marker");
    fs::write(&marker, "x").unwrap();
    let src = sb.root.join("src/again");
    fake_bin(&src, "0.2.0");
    install_version(&sb.ctx, &src, "0.2.0").unwrap();
    assert!(marker.exists(), "complete version reused, not replaced");
    // prune never drops what `current` points at, even if its sentinel is damaged.
    fs::remove_file(sb.ctx.version_dir("0.2.0").join(SENTINEL)).unwrap();
    prune(&sb.ctx, 3);
    assert!(marker.exists());
}

#[test]
fn systemd_paths_systemd_rejects_fail_before_anything_changes() {
    let sb = Sandbox::new(Platform::Linux);
    for bad in ["it's", "q\"x", "back\\slash", "tab\tx", "nl\nx"] {
        assert!(service::check_exec_path(&format!("/d/{bad}/agent-stats")).is_err(), "{bad}");
    }
    assert!(service::check_exec_path("/d with space/100%/$x/agent-stats").is_ok());
    let old_unit = "[Service]\nExecStart=/old\nEnvironment=HUB_URL=http://h\n";
    fs::write(sb.ctx.unit_path("report"), old_unit).unwrap();
    let mut ctx = sb.ctx.clone();
    ctx.data_dir = sb.root.join("da'ta");
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("XDG_DATA_HOME"), "{e}");
    assert_eq!(fs::read_to_string(ctx.unit_path("report")).unwrap(), old_unit);
    assert_eq!(sb.log(), "", "no systemctl call");
    // `$` is not doubled in the executable path.
    let mut c = sb.ctx.clone();
    c.data_dir = sb.root.join("d$x");
    let u = service::render_unit(&c, "report", &[]).unwrap();
    assert!(u.contains("ExecStart=\"") && u.contains("d$x/current/agent-stats\" \"report\""), "{u}");
}

#[test]
fn failed_unit_start_restores_and_restarts_the_previous_unit() {
    let sb = Sandbox::new(Platform::Linux);
    let old_unit = "[Service]\nExecStart=/old\n";
    fs::write(sb.ctx.unit_path("report"), old_unit).unwrap();
    // Restarting fails while the new unit is in place, works for the restored one.
    script(
        &sb.root.join("fake/systemctl"),
        &format!(
            "echo \"systemctl $*\" >> \"{r}/log\"\nif [ \"$2\" = restart ] && grep -q current/agent-stats \"{u}\"; then echo boom >&2; exit 1; fi\nexit 0",
            r = sb.root.display(),
            u = sb.ctx.unit_path("report").display()
        ),
    );
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("did not start") && e.contains("previous unit was restored"), "{e}");
    assert_eq!(fs::read_to_string(ctx.unit_path("report")).unwrap(), old_unit);
    assert_eq!(sb.log().matches("--user restart agent-stats-reporter.service").count(), 2, "{}", sb.log());
}

#[test]
fn service_install_refuses_to_move_current_down_without_force() {
    let sb = Sandbox::new(Platform::Linux);
    sb.install_current("0.9.0");
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("refusing") && e.contains("0.9.0") && e.contains("--force"), "{e}");
    assert_eq!(ctx.current_version().as_deref(), Some("0.9.0"));
    assert!(!ctx.unit_path("report").exists());
    service::install_opts(&ctx, "report", None, true).unwrap();
    assert_eq!(ctx.current_version().as_deref(), Some("0.7.0"));
    // A newer running binary moves it up without being asked.
    ctx.self_version = "0.10.0".into();
    fake_bin(&ctx.self_exe, "0.10.0");
    service::install(&ctx, "report", None).unwrap();
    assert_eq!(ctx.current_version().as_deref(), Some("0.10.0"));
}

fn mac_ctx(sb: &Sandbox) -> (Ctx, PathBuf) {
    let legacy = sb.ctx.plist_path("com.kilcazn.agent-stats.reporter");
    fs::write(&legacy, "<plist><dict><key>EnvironmentVariables</key><dict><key>HUB_URL</key><string>http://h:1</string></dict></dict></plist>").unwrap();
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    (ctx, legacy)
}

#[test]
fn macos_failed_bootstrap_restores_the_legacy_agent() {
    let sb = Sandbox::new(Platform::Mac);
    let (ctx, legacy) = mac_ctx(&sb);
    let before = fs::read_to_string(&legacy).unwrap();
    fs::write(sb.root.join("failnew"), "").unwrap();
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("previous LaunchAgent") && e.contains("was restored"), "{e}");
    assert_eq!(fs::read_to_string(&legacy).unwrap(), before);
    assert!(!ctx.plist_path("dev.agent-stats.report").exists());
    assert!(!ctx.agents_dir.join("com.kilcazn.agent-stats.reporter.plist.bak").exists());
    let log = sb.log();
    let last = log.rfind("launchctl bootstrap gui/501").unwrap();
    assert!(log[last..].contains("com.kilcazn.agent-stats.reporter.plist"), "legacy re-bootstrapped last: {log}");
}

#[test]
fn macos_without_a_gui_session_says_so() {
    let sb = Sandbox::new(Platform::Mac);
    let (ctx, legacy) = mac_ctx(&sb);
    fs::write(sb.root.join("nogui"), "").unwrap();
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("no GUI login session") && e.contains("was restored"), "{e}");
    assert!(legacy.exists());
}

#[test]
fn macos_agent_that_never_runs_is_rolled_back() {
    let sb = Sandbox::new(Platform::Mac);
    let (ctx, legacy) = mac_ctx(&sb);
    // `print` fails for the new label and no status ever shows up.
    script(
        &sb.root.join("fake/launchctl"),
        &format!("echo \"launchctl $*\" >> \"{}/log\"\n[ \"$1\" = print ] && exit 113\nexit 0", sb.root.display()),
    );
    let e = service::install(&ctx, "report", None).unwrap_err();
    assert!(e.contains("did not start within 2 s") && e.contains("was restored"), "{e}");
    assert!(legacy.exists() && !ctx.plist_path("dev.agent-stats.report").exists());
}

// ---------------------------------------------------------------- scheduled updater

fn updater_ctx(sb: &Sandbox) -> Ctx {
    let me = sb.root.join("src/me");
    fake_bin(&me, "0.7.0");
    let mut ctx = sb.ctx.clone();
    ctx.self_exe = me;
    ctx.self_version = "0.7.0".into();
    ctx
}

#[test]
fn updater_every_parsing_and_minimum() {
    assert_eq!(service::parse_every("15m").unwrap(), 900);
    assert_eq!(service::parse_every("1h").unwrap(), 3600);
    assert_eq!(service::parse_every("900s").unwrap(), 900);
    assert_eq!(service::parse_every("5m").unwrap(), 300);
    for bad in ["4m", "299s", "0m", "", "m", "15", "1d", "x5m", "-5m"] {
        assert!(service::parse_every(bad).unwrap_err().starts_with("usage:"), "{bad}");
    }
    assert_eq!(service::fmt_every(900), "15m");
    assert_eq!(service::fmt_every(7200), "2h");
    assert_eq!(service::fmt_every(330), "330s");
}

#[test]
fn updater_linux_install_renders_units_and_enables_the_timer() {
    let sb = Sandbox::new(Platform::Linux);
    let ctx = updater_ctx(&sb);
    let args: Vec<String> = ["install", "updater", "--every", "1h"].iter().map(|s| s.to_string()).collect();
    service::cli(&ctx, &args).unwrap();

    let svc = fs::read_to_string(ctx.unit_dir.join("agent-stats-updater.service")).unwrap();
    let bin = format!("{}/current/agent-stats", ctx.data_dir.display());
    assert!(svc.contains("Type=oneshot\n"), "{svc}");
    assert!(svc.contains(&format!("ExecStart=\"{bin}\" \"update\" \"--auto\"\n")), "{svc}");
    assert!(svc.contains(&format!("Environment=\"PATH={}\"\n", ctx.service_path)), "{svc}");
    assert!(!svc.contains("Restart="));
    let timer = fs::read_to_string(ctx.unit_dir.join("agent-stats-updater.timer")).unwrap();
    for needle in [
        "OnBootSec=10min\n",
        "OnUnitActiveSec=3600s\n",
        "RandomizedDelaySec=2min\n",
        "Persistent=true\n",
        "[Install]\nWantedBy=timers.target\n",
    ] {
        assert!(timer.contains(needle), "{needle} in {timer}");
    }
    let log = sb.log();
    let order: Vec<usize> =
        ["--user daemon-reload", "--user enable --now agent-stats-updater.timer", "loginctl enable-linger"]
            .iter()
            .map(|n| log.find(n).unwrap_or_else(|| panic!("missing {n} in {log}")))
            .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{log}");
    assert!(!log.contains("restart"), "{log}");
    assert!(ctx.service_installed("updater"));
    assert_eq!(service::updater_interval(&ctx), Some(3600));

    service::uninstall(&ctx, "updater").unwrap();
    assert!(!ctx.unit_dir.join("agent-stats-updater.service").exists());
    assert!(!ctx.unit_dir.join("agent-stats-updater.timer").exists());
    assert!(sb.log().contains("--user disable --now agent-stats-updater.timer"));
}

#[test]
fn updater_default_interval_and_cli_validation() {
    let sb = Sandbox::new(Platform::Linux);
    let ctx = updater_ctx(&sb);
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(service::cli(&ctx, &a(&["install", "updater", "--every", "2m"])).unwrap_err().starts_with("usage:"));
    assert!(service::cli(&ctx, &a(&["install", "updater", "--label", "x"])).unwrap_err().starts_with("usage:"));
    assert!(service::cli(&ctx, &a(&["install", "hub", "--every", "10m"])).unwrap_err().starts_with("usage:"));
    assert!(!ctx.unit_dir.join("agent-stats-updater.timer").exists());
    service::cli(&ctx, &a(&["install", "updater"])).unwrap();
    assert_eq!(service::updater_interval(&ctx), Some(900));
}

#[test]
fn updater_is_not_a_role_for_the_update_trial() {
    let sb = Sandbox::new(Platform::Linux);
    let ctx = updater_ctx(&sb);
    service::install(&ctx, "report", None).unwrap();
    service::install(&ctx, "updater", None).unwrap();
    assert!(ctx.service_installed("updater"));
    assert_eq!(service::installed_roles(&ctx), vec!["report"]);
    // An update restarts the report service only and does not wait for an updater heartbeat.
    sb.install_current("0.7.1");
    update::rollback(&ctx).unwrap();
    assert!(!sb.log().contains("restart agent-stats-updater"), "{}", sb.log());
}

#[test]
fn updater_macos_plist_and_bootstrap() {
    let sb = Sandbox::new(Platform::Mac);
    let ctx = updater_ctx(&sb);
    service::install_updater(&ctx, 1800, false).unwrap();
    let plist = fs::read_to_string(ctx.plist_path("dev.agent-stats.updater")).unwrap();
    let bin = format!("{}/current/agent-stats", ctx.data_dir.display());
    for needle in [
        "<string>dev.agent-stats.updater</string>".to_string(),
        format!("<string>{bin}</string>\n    <string>update</string>\n    <string>--auto</string>"),
        "<key>StartInterval</key>\n  <integer>1800</integer>".to_string(),
        "<key>RunAtLoad</key>\n  <false/>".to_string(),
        format!("<string>{}</string>", ctx.service_path),
        format!("<string>{}</string>", ctx.log_path("updater").display()),
    ] {
        assert!(plist.contains(&needle), "{needle} in {plist}");
    }
    assert!(!plist.contains("KeepAlive"));
    let log = sb.log();
    assert!(log.contains("launchctl bootstrap gui/501"), "{log}");
    assert!(!log.contains("kickstart") && !log.contains("launchctl print"), "{log}");
    assert_eq!(service::updater_interval(&ctx), Some(1800));
    assert!(service::installed_roles(&ctx).is_empty());

    service::uninstall(&ctx, "updater").unwrap();
    assert!(!ctx.plist_path("dev.agent-stats.updater").exists());
    assert!(sb.log().contains("launchctl bootout gui/501/dev.agent-stats.updater"));
}

#[test]
fn updater_status_with_and_without_auto_update_json() {
    let sb = Sandbox::new(Platform::Linux);
    let ctx = updater_ctx(&sb);
    assert!(service::updater_summary(&ctx, 1000).contains("not installed"));
    service::install(&ctx, "updater", None).unwrap();
    assert_eq!(service::updater_summary(&ctx, 1000), "updater: every 15m, no run recorded yet");

    let file = ctx.data_dir.join("auto-update.json");
    fs::write(&file, r#"{"at": 280, "source": "github", "outcome": "up-to-date", "version": "0.4.0", "message": "x"}"#)
        .unwrap();
    assert_eq!(service::updater_summary(&ctx, 1000), "updater: every 15m, last run 12 min ago: up-to-date (0.4.0)");
    fs::write(&file, r#"{"at": 970, "source": "hub", "outcome": "error", "version": null, "message": "no network"}"#)
        .unwrap();
    assert_eq!(service::updater_summary(&ctx, 1000), "updater: every 15m, last run 30 s ago: error - no network");
    fs::write(&file, "{not json").unwrap();
    assert_eq!(service::updater_summary(&ctx, 1000), "updater: every 15m, no run recorded yet");
    service::status(&ctx).unwrap();
}
