//! The hub's defences against a misbehaving or stolen reporter: new-repo floods, token overflow, bad days, the
//! legacy token impersonating enrolled machines, pairing takeovers and races, control characters.

use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::{auth, db, server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

// 2026-10-14T03:00:00Z (2026-10-14 in Tokyo).
const NOW: f64 = 1_791_946_800_000.0;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn post(base: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let mut r = agent().post(format!("{base}{path}")).header("content-type", "application/json");
    if let Some(t) = token {
        r = r.header("authorization", format!("Bearer {t}"));
    }
    let mut res = r.send(body).unwrap();
    (res.status().as_u16(), res.body_mut().read_to_string().unwrap_or_default())
}

fn get(base: &str, path: &str) -> (u16, String) {
    let mut res = agent().get(format!("{base}{path}")).call().unwrap();
    (res.status().as_u16(), res.body_mut().read_to_string().unwrap_or_default())
}

/// A sink that answers 200 and sends each POST body.
fn sink() -> (String, mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let _ = tx.send(String::from_utf8(body).unwrap());
        }
    });
    (url, rx)
}

struct Rig {
    hub: std::sync::Arc<server::Hub>,
    base: String,
    paths: Paths,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str, extra_toml: &str, insecure: bool) -> Rig {
    let dir = std::env::temp_dir().join(format!("agent-stats-hub-limits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths =
        Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), releases_dir: dir.join("releases") };
    agent_stats_common::write_secret(&paths.config_dir.join("legacy.token"), "legacy-secret").unwrap();
    let toml =
        format!("name = \"test\"\nlisten = \"127.0.0.1:0\"\npublic = true\ntrust_loopback = false\n{extra_toml}\n");
    let cfg = Config::parse(&paths, &toml).unwrap();
    let clock = server::Clock::real();
    clock.set(NOW);
    let opts = server::Options {
        insecure_no_auth: insecure,
        test_hooks: false,
        clock,
        github: None,
        publish_on_change: false,
        exit_on_config_change: false,
    };
    let (hub, addr) = agent_stats_hub::start(cfg, paths.clone(), opts).unwrap();
    Rig { hub, base: format!("http://{addr}"), paths, dir }
}

fn code(rig: &Rig, replace: Option<&str>) -> String {
    let d = db::open(&rig.paths.db()).unwrap();
    auth::create_code_for(&d, None, replace, Duration::from_secs(600), NOW).unwrap()
}

fn pair(rig: &Rig, code: &str, machine: &str) -> (u16, String) {
    post(&rig.base, "/pair", None, &format!(r#"{{"code":"{code}","machine":"{machine}"}}"#))
}

fn token_of(body: &str) -> String {
    agent_stats_hub::json::J::parse(body).unwrap().get("token").and_then(|t| t.as_str()).unwrap().to_string()
}

fn report(machine: &str, label: &str, days: &str, repos: &str) -> String {
    format!(
        r#"{{"machine":"{machine}","label":"{label}","platform":"linux","days":[{days}],"repos":[{repos}],"sessions":[]}}"#
    )
}

fn day(d: &str, tokens: &str, model: &str) -> String {
    format!(r#"{{"day":"{d}","agent":"claude","model":"{model}","tokens":{tokens},"output":1}}"#)
}

fn count(rig: &Rig, sql: &str) -> i64 {
    db::open(&rig.paths.db()).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn a_flood_of_new_repositories_is_capped_and_is_one_event() {
    let (url, events) = sink();
    let rig = rig("flood", &format!("new_repo_webhook = \"{url}/new\""), true);
    let repos: Vec<String> =
        (0..1000).map(|i| format!(r#"{{"day":"2026-10-14","repo":"flood-{i}","tokens":{}}}"#, i + 1)).collect();
    let body = report("m", "m", &day("2026-10-14", "5", "x"), &repos.join(","));
    let t = std::time::Instant::now();
    assert_eq!(post(&rig.base, "/report", None, &body).0, 200);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(count(&rig, "SELECT count(*) FROM repo_meta WHERE repo LIKE 'flood-%'"), 200);
    assert_eq!(
        count(&rig, "SELECT count(DISTINCT repo) FROM repos WHERE repo LIKE 'flood-%'"),
        200,
        "rows dropped too"
    );
    let ev = events.recv_timeout(Duration::from_secs(10)).expect("one event");
    assert!(ev.contains(r#""count":200,"more":150"#), "{}", &ev[..200.min(ev.len())]);
    assert_eq!(ev.matches(r#""repo":"flood-"#).count(), 50, "50 names listed");
    assert!(events.recv_timeout(Duration::from_millis(500)).is_err(), "not one event per repository");
    // The same report again: the first 200 are known, so the next 200 get in.
    assert_eq!(post(&rig.base, "/report", None, &body).0, 200);
    assert_eq!(count(&rig, "SELECT count(*) FROM repo_meta WHERE repo LIKE 'flood-%'"), 400);
}

#[test]
fn no_webhook_means_nothing_is_queued() {
    let rig = rig("nohook", "", true);
    let body = report("m", "m", &day("2026-10-14", "5", "x"), r#"{"day":"2026-10-14","repo":"a","tokens":3}"#);
    assert_eq!(post(&rig.base, "/report", None, &body).0, 200);
    assert_eq!(count(&rig, "SELECT count(*) FROM repo_meta WHERE repo = 'a'"), 1);
}

#[test]
fn huge_sums_cannot_break_the_payload() {
    let rig = rig("overflow", "", true);
    // What a 1e15-per-row report used to leave behind: SQLite's sum() of these raises "integer overflow".
    {
        let d = db::open(&rig.paths.db()).unwrap();
        for i in 0..20 {
            d.execute(
                "INSERT INTO usage VALUES ('evil', '2026-10-14', 'claude', ?, 999999999999999, 999999999999999)",
                [format!("m{i}")],
            )
            .unwrap();
        }
        d.execute_batch("INSERT INTO usage VALUES ('evil', '2026-10-13', 'claude', 'x', 9223372036854775807, 1)")
            .unwrap();
    }
    let (st, body) = get(&rig.base, "/api/public");
    assert_eq!(st, 200, "{body}");
    assert!(rig.hub.public_payload(NOW).is_ok() && rig.hub.internal_state(NOW).is_ok());
    rig.hub.tick();
    // New rows are capped at 1e12.
    let ok = report("m", "m", &day("2026-10-14", "999999999999", "big"), "");
    assert_eq!(post(&rig.base, "/report", None, &ok).0, 200);
    let bad = report("m", "m", &day("2026-10-14", "1000000000000", "big"), "");
    assert_eq!(post(&rig.base, "/report", None, &bad).0, 422);
}

#[test]
fn days_must_be_real_and_recent() {
    let rig = rig("days", "", true);
    let send = |d: &str| post(&rig.base, "/report", None, &report("m", "m", &day(d, "5", "x"), "")).0;
    assert_eq!(send("2026-10-14"), 200);
    assert_eq!(send("2026-10-15"), 200, "tomorrow");
    for bad in ["2026-10-16", "9999-12-31", "2026-02-31", "0000-01-01x", "2025-13-01"] {
        assert_eq!(send(bad), 422, "{bad}");
    }
    assert_eq!(send("0000-01-01"), 200, "long ago: accepted, but skipped");
    assert_eq!(count(&rig, "SELECT count(*) FROM usage WHERE day < '2025-01-01' OR day > '2026-10-15'"), 0);
}

#[test]
fn the_legacy_token_cannot_speak_for_an_enrolled_machine() {
    let rig = rig("legacy", "legacy_token_file = \"legacy.token\"", false);
    let legacy = "legacy-secret";
    let rep = |m: &str| report(m, m, &day("2026-10-14", "5", "x"), "");
    assert_eq!(post(&rig.base, "/report", Some(legacy), &rep("oldbox")).0, 200);
    // The migration path: a machine that reported on the legacy token pairs without any flag.
    let (st, ans) = pair(&rig, &code(&rig, None), "oldbox");
    assert_eq!(st, 200, "{ans}");
    let token = token_of(&ans);
    let (st, body) = post(&rig.base, "/report", Some(legacy), &rep("oldbox"));
    assert_eq!(st, 403, "{body}");
    assert!(body.contains("own token"), "{body}");
    assert_eq!(post(&rig.base, "/report", Some(&token), &rep("oldbox")).0, 200);
    assert_eq!(post(&rig.base, "/report", Some(legacy), &rep("newbox")).0, 200, "other machines still use it");
    // Revoked machines stay off limits for the legacy token.
    {
        let d = db::open(&rig.paths.db()).unwrap();
        assert!(auth::revoke(&d, "oldbox", NOW).unwrap());
    }
    assert_eq!(post(&rig.base, "/report", Some(legacy), &rep("oldbox")).0, 403);
}

#[test]
fn a_code_cannot_take_over_an_enrolled_machine_unless_made_for_it() {
    let rig = rig("takeover", "", false);
    let (st, ans) = pair(&rig, &code(&rig, None), "mac-mini");
    assert_eq!(st, 200, "{ans}");
    let first = token_of(&ans);
    let rep = report("mac-mini", "mac-mini", &day("2026-10-14", "5", "x"), "");

    let plain = code(&rig, None);
    let (st, body) = pair(&rig, &plain, "mac-mini");
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("hub pair --replace mac-mini"), "{body}");
    assert_eq!(post(&rig.base, "/report", Some(&first), &rep).0, 200, "its token still works");
    assert_eq!(pair(&rig, &plain, "laptop").0, 200, "the same code still pairs a free name");

    // A code made for a machine is only good for that name.
    let rep_code = code(&rig, Some("mac-mini"));
    assert_eq!(pair(&rig, &rep_code, "laptop").0, 409);
    let (st, ans) = pair(&rig, &rep_code, "mac-mini");
    assert_eq!(st, 200, "{ans}");
    assert_eq!(post(&rig.base, "/report", Some(&first), &rep).0, 401, "the old token is gone");
    assert_eq!(post(&rig.base, "/report", Some(&token_of(&ans)), &rep).0, 200);

    // A revoked machine comes back only through --replace.
    {
        let d = db::open(&rig.paths.db()).unwrap();
        assert!(auth::revoke(&d, "laptop", NOW).unwrap());
    }
    assert_eq!(pair(&rig, &code(&rig, None), "laptop").0, 409);
    assert_eq!(pair(&rig, &code(&rig, Some("laptop")), "laptop").0, 200);
}

#[test]
fn concurrent_bad_pairings_cannot_overshoot_the_cap() {
    // Every client here is 127.0.0.1: lift the per-IP connection cap, this test is about the pairing limiter.
    let rig = rig("race", "max_connections_per_ip = 0", false);
    let answers: Vec<u16> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..24).map(|_| s.spawn(|| pair(&rig, "ZZZZ-ZZZZ", "x").0)).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(answers.iter().filter(|s| **s == 401).count(), 5, "{answers:?}");
    assert_eq!(answers.iter().filter(|s| **s == 429).count(), 19, "{answers:?}");
}

#[test]
fn free_text_cannot_carry_control_characters_into_the_payload() {
    let rig = rig("ctl", "", true);
    let label = r"Lap\u001b[31mRED\u001b[0m \u202e<b>x</b>";
    let body = report("m", label, &day("2026-10-14", "5", r"mod\u0007el\u2028"), "");
    assert_eq!(post(&rig.base, "/report", None, &body).0, 200);
    let (_, public) = get(&rig.base, "/api/public");
    assert!(public.contains(r#""name":"Lap[31mRED[0m <b>x</b>""#), "{public}");
    assert!(public.contains(r#""name":"model""#), "{public}");
    for bad in [r"\u001b", r"\u202e", r"\u2028", r"\u0007"] {
        assert!(!public.contains(bad), "{bad} in {public}");
    }
    // /pair's own free text, and the machine name.
    let c = code(&rig, None);
    let (st, _) = post(&rig.base, "/pair", None, &format!(r#"{{"code":"{c}","machine":"a\u202eb"}}"#));
    assert_eq!(st, 422);
    let (st, _) = post(
        &rig.base,
        "/pair",
        None,
        &format!(r#"{{"code":"{c}","machine":"ok","label":"x\u001by","version":"1\u0007"}}"#),
    );
    assert_eq!(st, 200);
    assert_eq!(count(&rig, "SELECT count(*) FROM machine_tokens WHERE label = 'xy'"), 1);
    assert_eq!(count(&rig, "SELECT count(*) FROM machine_meta WHERE machine = 'ok' AND version = '1'"), 1);
}
