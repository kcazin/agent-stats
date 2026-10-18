//! Two replicas of one fleet over HTTP: the second introduces itself with its reporter's token, each pulls the
//! other, reports to either reach both, one publishes, fleet settings travel, and a join request made on one is
//! approved on the other.

use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::json::J;
use agent_stats_hub::{auth, db, fleet, server};
use std::time::{Duration, Instant};

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

fn get(base: &str, path: &str) -> (u16, String) {
    let mut res = agent().get(format!("{base}{path}")).call().unwrap();
    (res.status().as_u16(), res.body_mut().read_to_string().unwrap_or_default())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn today() -> String {
    jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date().to_string()
}

fn report(machine: &str, tokens: i64) -> String {
    format!(
        r#"{{"machine":"{machine}","label":"{machine}","platform":"linux","at":1,"full":false,"protocol":2,
        "days":[{{"day":"{}","agent":"claude","model":"claude-opus-4-5","tokens":{tokens},"output":1}}],
        "repos":[],"sessions":[]}}"#,
        today()
    )
}

fn opts() -> server::Options {
    server::Options {
        insecure_no_auth: false,
        test_hooks: false,
        clock: server::Clock::real(),
        github: None,
        publish_on_change: false,
        exit_on_config_change: false,
    }
}

fn tokens_of(paths: &Paths, machine: &str) -> i64 {
    let d = db::open(&paths.db()).unwrap();
    d.query_row("SELECT coalesce(sum(tokens), 0) FROM usage WHERE machine = ?", [machine], |r| r.get(0)).unwrap()
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// A machine token for `machine`, as `join` would get it.
fn enroll(paths: &Paths, machine: &str) -> String {
    let mut d = db::open(&paths.db()).unwrap();
    let code = auth::create_code(&d, None, Duration::from_secs(600), 1.0).unwrap();
    auth::redeem(&mut d, &code, machine, None, 2.0).ok().unwrap()
}

#[test]
fn two_replicas_share_everything() {
    let dir = std::env::temp_dir().join(format!("agent-stats-fleet-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths = |n: &str| Paths {
        config_dir: dir.join(n).join("config"),
        state_dir: dir.join(n).join("state"),
        releases_dir: dir.join(n).join("releases"),
    };
    let (pa, pb) = (paths("mini"), paths("air"));
    let (port_a, port_b) = (free_port(), free_port());
    let (url_a, url_b) = (format!("http://127.0.0.1:{port_a}"), format!("http://127.0.0.1:{port_b}"));

    // The mini: the old hub, now the first replica, with the fleet-wide settings.
    std::fs::create_dir_all(&pa.config_dir).unwrap();
    std::fs::write(
        pa.hub_toml(),
        format!("listen = \"127.0.0.1:{port_a}\"\n[fleet]\nnode = \"mini\"\nalways_on = true\n"),
    )
    .unwrap();
    std::fs::write(pa.fleet_toml(), "name = \"home\"\ntimezone = \"UTC\"\n").unwrap();
    let mini_token = enroll(&pa, "mini");
    let air_token = enroll(&pa, "air");
    let (a, _) = agent_stats_hub::start(Config::load(&pa, &pa.hub_toml()).unwrap(), pa.clone(), opts()).unwrap();

    // The Air: same fleet id, seeded with the mini's route, introduced by its reporter's token.
    std::fs::create_dir_all(&pb.state_dir).unwrap();
    std::fs::write(pb.hub_id(), format!("{}\n", a.hub_id)).unwrap();
    agent_stats_common::write_secret(&pb.config_dir.join("secrets/hub.token"), &air_token).unwrap();
    std::fs::write(
        pb.hub_toml(),
        format!(
            "listen = \"127.0.0.1:{port_b}\"\n[fleet]\nnode = \"air\"\nalways_on = false\npeers = [\"{url_a}\"]\n\
             token_file = \"secrets/hub.token\"\n"
        ),
    )
    .unwrap();
    let (b, _) = agent_stats_hub::start(Config::load(&pb, &pb.hub_toml()).unwrap(), pb.clone(), opts()).unwrap();
    assert_eq!(a.hub_id, b.hub_id);

    // The descriptor says which replica answers, and every replica's routes once they know each other.
    let (_, wk) = get(&url_b, "/.well-known/agent-stats");
    assert!(wk.contains(r#""node":"air""#), "{wk}");

    // The fleet's settings reach the Air (its own fleet.toml), and its members learn each other.
    wait("fleet.toml on the Air", || std::fs::read_to_string(pb.fleet_toml()).is_ok_and(|t| t.contains("home")));
    wait("both members on the mini", || fleet::members(&db::open(&pa.db()).unwrap()).unwrap().len() == 2);
    wait("both members on the Air", || fleet::members(&db::open(&pb.db()).unwrap()).unwrap().len() == 2);
    let (_, wk) = get(&url_a, "/.well-known/agent-stats");
    let routes = J::parse(&wk).unwrap().get("fleetRoutes").cloned().unwrap();
    assert_eq!(routes.stringify(), format!(r#"["{url_a}","{url_b}"]"#), "always-on first");

    // A report to either replica reaches the other; tokens enrolled on the mini work on the Air.
    assert_eq!(post(&url_a, "/report", Some(&mini_token), &report("mini", 100)).0, 200);
    wait("mini's usage on the Air", || tokens_of(&pb, "mini") == 100);
    let (st, body) = post(&url_b, "/report", Some(&air_token), &report("air", 70));
    assert_eq!(st, 200, "{body}");
    wait("air's usage on the mini", || tokens_of(&pa, "air") == 70);
    // Counts only grow: an older, smaller count from one side doesn't win.
    assert_eq!(post(&url_b, "/report", Some(&mini_token), &report("mini", 60)).0, 200);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(tokens_of(&pa, "mini"), 100);

    // The always-on mini publishes; the Air knows it.
    let now = server::Clock::real().now();
    assert!(a.publishes(&db::open(&pa.db()).unwrap(), now));
    assert!(!b.publishes(&db::open(&pb.db()).unwrap(), now));

    // A machine asks the Air to join; someone approves on the mini; the Air says approved and takes its token.
    let new_token = "c".repeat(64);
    let th = auth::sha256_hex(&new_token);
    let (st, body) =
        post(&url_b, "/join-request", None, &format!(r#"{{"machine":"nas","label":"NAS","tokenHash":"{th}"}}"#));
    assert_eq!(st, 200, "{body}");
    let req = J::parse(&body).unwrap();
    let id = req.get("id").and_then(J::as_str).unwrap().to_string();
    let digits = req.get("digits").and_then(J::as_str).unwrap().to_string();
    wait("the request on the mini", || {
        fleet::pending(&db::open(&pa.db()).unwrap(), now).unwrap().iter().any(|r| {
            r.get("id").and_then(J::as_str) == Some(id.as_str()) && r.get("digits").and_then(J::as_str) == Some(&digits)
        })
    });
    fleet::decide(&mut db::open(&pa.db()).unwrap(), &id, true, "mini", server::Clock::real().now()).unwrap();
    wait("approved on the Air", || {
        get(&url_b, &format!("/join-request/{id}?key={th}")).1.contains(r#""status":"approved""#)
    });
    wait("the new token on the Air", || post(&url_b, "/report", Some(&new_token), &report("nas", 5)).0 == 200);

    // Revoking on one replica refuses the machine on the other.
    fleet::revoke(&db::open(&pb.db()).unwrap(), "nas", server::Clock::real().now()).unwrap();
    wait("nas refused on the mini", || post(&url_a, "/report", Some(&new_token), &report("nas", 6)).0 == 401);

    drop((a, b));
    let _ = std::fs::remove_dir_all(&dir);
}
