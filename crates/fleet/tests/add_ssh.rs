//! `machines add --ssh` end to end against a real hub on loopback, with a fake `ssh` that runs the remote scripts
//! here under a temporary `$HOME`, and a fake `agent-stats` there that pairs and reports with curl.

use agent_stats_fleet::{remote, Args};
use agent_stats_hub::config::{Config, Paths};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agent-stats-fleet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const FAKE_SSH: &str = r#"#!/bin/sh
# Skips ssh's options, drops the target, runs the remote command here with HOME=$FAKE_HOME.
while [ $# -gt 0 ]; do case $1 in -T) shift ;; -o) shift 2 ;; *) break ;; esac; done
shift
export HOME="$FAKE_HOME"
exec sh -c "$*"
"#;

/// An agent-stats that is "already installed" (a newer version, so nothing is downloaded) and joins with curl.
const FAKE_AGENT_STATS: &str = r#"#!/bin/sh
case $1 in
  version) echo "agent-stats 9.9.9 (protocol 2)" ;;
  join)
    URL=$2; CODE=$4; M=$(hostname); M=${M%.local}
    R=$(curl -fsS -X POST -H 'content-type: application/json' \
      -d "{\"code\":\"$CODE\",\"machine\":\"$M\",\"label\":\"$M\",\"platform\":\"test\",\"version\":\"9.9.9\"}" "$URL/pair") || exit 1
    T=$(printf '%s' "$R" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    AT=$(($(date +%s) * 1000))
    curl -fsS -X POST -H "authorization: Bearer $T" -H 'content-type: application/json' \
      -d "{\"protocol\":2,\"machine\":\"$M\",\"label\":\"$M\",\"platform\":\"test\",\"version\":\"9.9.9\",\"at\":$AT,\"full\":false,\"days\":[],\"repos\":[],\"sessions\":[]}" \
      "$URL/report" >/dev/null || exit 1
    echo "joined $URL" ;;
  service) echo "fake service $*" ;;
esac
"#;

#[test]
fn add_over_ssh_installs_joins_and_waits_for_the_first_report() {
    let root = tmpdir("add");
    let (config_dir, state_root, home) = (root.join("config"), root.join("state"), root.join("home"));
    for d in [&config_dir, &state_root, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::env::set_var("AGENT_STATS_CONFIG_DIR", &config_dir);
    std::env::set_var("XDG_STATE_HOME", &state_root);
    std::env::set_var("AGENT_STATS_NO_BONJOUR", "1");
    std::env::set_var("AGENT_STATS_APPS_DIR", root.join("no-apps"));
    let paths = Paths::from_env();
    let cfg = Config::parse(&paths, "name = \"home\"\nlisten = \"127.0.0.1:0\"\n").unwrap();
    let opts = agent_stats_hub::server::Options {
        insecure_no_auth: false,
        test_hooks: false,
        clock: agent_stats_hub::server::Clock::real(),
        github: None,
        publish_on_change: false,
        exit_on_config_change: false,
    };
    let (_hub, addr) = agent_stats_hub::start(cfg, paths.clone(), opts).unwrap();

    let ssh = root.join("bin").join("ssh");
    script(&ssh, FAKE_SSH);
    script(&home.join(".local/bin/agent-stats"), FAKE_AGENT_STATS);
    std::env::set_var("AGENT_STATS_SSH", &ssh);
    std::env::set_var("FAKE_HOME", &home);

    // No route answers: the machine is never touched past the probe.
    std::env::set_var("AGENT_STATS_TEST_ROUTES", "http://127.0.0.1:1");
    let a = Args { ssh: Some("me@box".into()), json: true, ..Default::default() };
    assert_eq!(remote::add(&a), Err(String::new()));

    // The hub answers: installed (already, newer), joined, reported.
    std::env::set_var("AGENT_STATS_TEST_ROUTES", format!("http://127.0.0.1:1,http://{addr}"));
    assert_eq!(remote::add(&a), Ok(()));
    let db = agent_stats_hub::db::open(&paths.db()).unwrap();
    let host = agent_stats_hub::config::hostname();
    let name = host.strip_suffix(".local").unwrap_or(&host).to_string();
    assert!(agent_stats_hub::auth::is_enrolled(&db, &name).unwrap());
    assert!(agent_stats_fleet::local::last_seen(&db, &name).is_some());

    // Again: the name is taken, unless --replace.
    assert_eq!(remote::add(&a), Err(String::new()));
    let a = Args { replace: true, ..a };
    assert_eq!(remote::add(&a), Ok(()));
    let _ = std::fs::remove_dir_all(&root);
}
