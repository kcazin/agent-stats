//! End to end over HTTP: refuse to start without auth, pair a machine, report with its token, scope checks,
//! validation answers, admin-only routes, publishing, the new-repo webhook, `/stats` with CORS, then revoke.

use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::{auth, db, server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
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

fn get(base: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, Vec<(String, String)>) {
    let mut r = agent().get(format!("{base}{path}"));
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let mut res = r.call().unwrap();
    let h = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
    (res.status().as_u16(), res.body_mut().read_to_string().unwrap_or_default(), h)
}

/// A one-request-at-a-time HTTP sink: sends (path, authorization, body) for each POST.
fn capture() -> (String, mpsc::Receiver<(String, String, String)>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            let (mut len, mut auth) = (0, String::new());
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                match k.to_ascii_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap(),
                    "authorization" => auth = v.trim().to_string(),
                    _ => {}
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let _ = tx.send((path, auth, String::from_utf8(body).unwrap()));
        }
    });
    (url, rx)
}

fn report(machine: &str, extra: &str) -> String {
    format!(
        r#"{{"machine":"{machine}","label":"{machine} label","platform":"linux","at":1,"full":true{extra},
        "days":[{{"day":"2026-10-14","agent":"claude","model":"claude-opus-4-5","tokens":1000,"output":10}}],
        "repos":[{{"day":"2026-10-14","repo":"secret-thing","tokens":400,"remote":"github.com/me/secret-thing"}},
                 {{"day":"2026-10-14","repo":"neon","tokens":500}}],
        "sessions":[]}}"#
    )
}

#[test]
fn pair_report_revoke() {
    let dir = std::env::temp_dir().join(format!("agent-stats-hub-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths =
        Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), releases_dir: dir.join("releases") };
    let (sink, published) = capture();
    let toml = format!(
        r##"name = "test"
listen = "127.0.0.1:0"
public = true
cors_origins = ["https://site.example"]
trust_loopback = false
new_repo_webhook = "{sink}/new-repo"
advertise = ["http://hub.example:7777"]
[[publish]]
name = "sink"
url = "{sink}/publish"
token_file = "publish.token"
[[project]]
key = "neon"
name = "Neon"
color = "#abcdef"
match = ["*neon*"]
"##
    );
    agent_stats_common::write_secret(&paths.config_dir.join("publish.token"), "pub-secret").unwrap();
    let cfg = || Config::parse(&paths, &toml).unwrap();
    // 2026-10-14T03:00:00Z, so "2026-10-14" is today in Tokyo.
    let now = 1_791_946_800_000.0;
    let opts = || server::Options {
        insecure_no_auth: false,
        test_hooks: false,
        clock: server::Clock::real(),
        github: None,
        publish_on_change: false,
        exit_on_config_change: false,
    };
    let opts_at = |t: f64| {
        let c = server::Clock::real();
        c.set(t);
        server::Options { clock: c, ..opts() }
    };

    // No legacy token, no machines, no codes: the hub starts (so its service can come up before anyone joins),
    // says how to enroll a machine, and refuses every report.
    let (empty, _) = agent_stats_hub::start(cfg(), paths.clone(), opts()).expect("starts with nobody enrolled");
    let err = empty.check_auth_configured().expect_err("nobody can authenticate yet");
    assert!(err.contains("hub pair"), "{err}");

    // `hub pair` writes the code into the database directly.
    let code = {
        let d = db::open(&paths.db()).unwrap();
        auth::create_code(&d, Some("Laptop"), Duration::from_secs(600), now).unwrap()
    };
    let (hub, addr) = agent_stats_hub::start(cfg(), paths.clone(), opts_at(now)).unwrap();
    let base = format!("http://{addr}");

    let (st, wk, _) = get(&base, "/.well-known/agent-stats", &[]);
    assert_eq!(st, 200);
    assert!(wk.contains(&format!(r#""hubId":"{}""#, hub.hub_id)) && wk.contains(r#""role":"hub""#), "{wk}");
    assert!(wk.contains(r#""routes":["http://hub.example:7777"]"#), "{wk}");

    // Pairing: a wrong code, then the right one (once).
    assert_eq!(post(&base, "/pair", None, r#"{"code":"AAAA-AAAA","machine":"laptop"}"#).0, 401);
    let (st, body) = post(
        &base,
        "/pair",
        None,
        &format!(r#"{{"code":"{}","machine":"laptop","label":"x","version":"0.2.0"}}"#, code.to_lowercase()),
    );
    assert_eq!(st, 200, "{body}");
    let ans = agent_stats_hub::json::J::parse(&body).unwrap();
    let token = ans.get("token").and_then(|t| t.as_str()).unwrap().to_string();
    assert_eq!(token.len(), 64);
    let view = ans.get("viewToken").and_then(|t| t.as_str()).unwrap().to_string();
    assert!(view.len() == 64 && view != token);
    assert!(ans.get("timezone").and_then(|t| t.as_str()).is_some_and(|tz| !tz.is_empty()), "{body}");
    assert_eq!(ans.get("hubId").and_then(|t| t.as_str()), Some(hub.hub_id.as_str()));
    assert_eq!(
        post(&base, "/pair", None, &format!(r#"{{"code":"{code}","machine":"other"}}"#)).0,
        401,
        "codes are one-time"
    );

    // Reporting with the machine token: only for its own machine.
    assert_eq!(post(&base, "/report", None, &report("laptop", "")).0, 401);
    assert_eq!(post(&base, "/report", Some("nope"), &report("laptop", "")).0, 401);
    let (st, body) = post(&base, "/report", Some(&token), &report("laptop", r#","protocol":2,"version":"0.2.0""#));
    assert_eq!((st, body.as_str()), (200, r#"{"ok":true}"#));
    assert_eq!(post(&base, "/report", Some(&token), &report("desktop", "")).0, 403);
    let (st, body) = post(&base, "/report", Some(&token), &report("laptop", r#","protocol":3"#));
    assert_eq!((st, body.as_str()), (426, r#"{"error":"protocol 3 not supported","supported":[1,2]}"#));
    let (st, body) = post(&base, "/report", Some(&token), r#"{"machine":"laptop","days":[{"day":"yesterday"}]}"#);
    assert_eq!(st, 422, "{body}");
    assert!(body.contains("days[0].day"), "{body}");
    assert_eq!(post(&base, "/report", Some(&token), "{").0, 422);

    // The new repositories were announced as one event (the webhook is fire-and-forget, so wait for it).
    let (path, _, event) = published.recv_timeout(Duration::from_secs(10)).expect("new_repo_webhook");
    assert_eq!(path, "/new-repo");
    assert!(event.contains(r#""machine":"laptop""#) && event.contains(r#""count":2,"more":0"#), "{event}");
    assert!(
        event.contains(
            r#"{"repo":"secret-thing","remote":"github.com/me/secret-thing","tokensToday":400,"project":null}"#
        ),
        "{event}"
    );
    assert!(event.contains(r#"{"repo":"neon","remote":null,"tokensToday":500,"project":"neon"}"#), "{event}");

    // Admin routes: no loopback trust here, so the admin token is needed.
    assert_eq!(get(&base, "/api/state", &[]).0, 401);
    assert_eq!(get(&base, "/", &[]).0, 401);
    let bearer = format!("Bearer {}", hub.admin_token);
    let (st, state, _) = get(&base, "/api/state", &[("authorization", &bearer)]);
    assert_eq!(st, 200);
    assert!(state.contains(r#""machine":"laptop""#) && state.contains(r#""version":"0.2.0""#), "{state}");
    let (st, repos, _) = get(&base, "/api/repos", &[("authorization", &bearer)]);
    assert_eq!(st, 200);
    assert!(repos.contains(r#""repo":"secret-thing","remote":"github.com/me/secret-thing","project":null"#), "{repos}");
    assert!(repos.contains(r#""repo":"neon","remote":null,"project":"neon""#), "{repos}");
    assert_eq!(get(&base, "/", &[("cookie", &format!("agent_stats_admin={}", hub.admin_token))]).0, 200);

    // A machine's read-only token reads what the dashboard reads; its reporting token doesn't, and the read-only
    // one can't report.
    let viewer = format!("Bearer {view}");
    for path in ["/api/state", "/api/repos", "/"] {
        assert_eq!(get(&base, path, &[("authorization", &viewer)]).0, 200, "{path}");
        assert_eq!(get(&base, path, &[("authorization", &format!("Bearer {token}"))]).0, 401, "{path}");
    }
    assert_eq!(post(&base, "/report", Some(&view), &report("laptop", "")).0, 401);
    // A machine paired before view tokens asks for one with its own token, which replaces the old one.
    assert_eq!(post(&base, "/view-token", None, "").0, 401);
    assert_eq!(post(&base, "/view-token", Some(&view), "").0, 401);
    let (st, body) = post(&base, "/view-token", Some(&token), "");
    assert_eq!(st, 200, "{body}");
    let view2 =
        agent_stats_hub::json::J::parse(&body).unwrap().get("viewToken").and_then(|t| t.as_str()).unwrap().to_string();
    assert_eq!(get(&base, "/api/state", &[("authorization", &viewer)]).0, 401, "replaced");
    let viewer = format!("Bearer {view2}");
    assert_eq!(get(&base, "/api/state", &[("authorization", &viewer)]).0, 200);

    // A tick publishes the public payload (no repository names in it) and fills /stats.
    assert_eq!(get(&base, "/stats", &[]).0, 503);
    hub.tick();
    let (path, auth, body) = published.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!((path.as_str(), auth.as_str()), ("/publish", "Bearer pub-secret"));
    assert!(!body.contains("secret-thing"), "{body}");
    assert!(body.contains(r##""projectList":[{"key":"neon","name":"Neon","color":"#abcdef"}]"##), "{body}");
    // 1000 tokens today: 500 to neon by name, 400 to an unassigned repo, 100 unattributed (all to other by default).
    assert!(body.contains(r#""today":{"neon":500,"other":500}"#), "{body}");
    let (st, stats, h) = get(&base, "/stats", &[("origin", "https://site.example")]);
    assert_eq!(st, 200);
    assert!(stats.ends_with(r#","stale":false}"#), "{stats}");
    assert!(h.iter().any(|(k, v)| k == "access-control-allow-origin" && v == "https://site.example"), "{h:?}");
    assert!(h.iter().any(|(k, v)| k == "vary" && v == "Origin"));
    let (_, _, h) = get(&base, "/stats", &[("origin", "https://evil.example")]);
    assert!(!h.iter().any(|(k, _)| k == "access-control-allow-origin"));
    let st = hub.internal_state(now).unwrap();
    let pt = agent_stats_hub::json::J::Obj(st).stringify();
    assert!(pt.contains(r#""publishTargets":[{"name":"sink","#) && pt.contains(r#""lastError":null"#), "{pt}");

    // `hub machines` sees it, `hub revoke` stops it.
    {
        let d = db::open(&paths.db()).unwrap();
        let list = auth::machines(&d).unwrap();
        assert_eq!(list.len(), 1);
        assert!(agent_stats_hub::json::J::Obj(list[0].clone()).stringify().contains(r#""enrolled":true"#));
        assert!(auth::revoke(&d, "laptop", now).unwrap());
    }
    assert_eq!(post(&base, "/report", Some(&token), &report("laptop", "")).0, 401);
    assert_eq!(get(&base, "/api/state", &[("authorization", &viewer)]).0, 401, "revoked with the machine");

    // Failed pairing attempts are rate-limited.
    let mut last = 0;
    for _ in 0..6 {
        last = post(&base, "/pair", None, r#"{"code":"BBBB-BBBB","machine":"x"}"#).0;
    }
    assert_eq!(last, 429);

    // Bodies are judged by their head, before they are read: a revoked token is a 401 whatever the length,
    // and an oversized /pair body a 413.
    for (target, auth, len, status) in [
        ("/report", format!("authorization: Bearer {token}\r\n"), 9 << 20, "401"),
        ("/pair", String::new(), 5 << 10, "413"),
    ] {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write!(s, "POST {target} HTTP/1.1\r\nhost: x\r\n{auth}content-length: {len}\r\n\r\n").unwrap();
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        assert!(answer.starts_with(&format!("HTTP/1.1 {status}")), "{answer}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
