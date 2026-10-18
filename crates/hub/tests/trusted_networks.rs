//! A peer inside `trusted_networks` reads the admin routes like loopback; nobody gets write routes that way.

use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::http::Request;
use agent_stats_hub::server::{Clock, Hub, Options};
use std::net::IpAddr;

fn req(method: &str, path: &str, peer: &str, headers: &[(&str, &str)]) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body: Vec::new(),
        peer: peer.parse::<IpAddr>().unwrap(),
    }
}

#[test]
fn trusted_peer_reads_admin_routes_untrusted_does_not() {
    let dir = std::env::temp_dir().join(format!("agent-stats-hub-trusted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths =
        Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), releases_dir: dir.join("releases") };
    let toml = r#"
listen = "127.0.0.1:0"
public = false
trusted_networks = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]
"#;
    let mut cfg = Config::parse(&paths, toml).unwrap();
    cfg.legacy_token = Some("secret".into());
    let opts = Options {
        insecure_no_auth: false,
        test_hooks: false,
        clock: Clock::real(),
        github: None,
        publish_on_change: false,
        exit_on_config_change: false,
    };
    let hub = Hub::open(cfg, paths, opts).unwrap();

    for path in ["/api/state", "/api/repos", "/api/public", "/"] {
        for trusted in ["100.100.1.2", "fd7a:115c:a1e0:ab12::1"] {
            assert_eq!(
                hub.handle(req("GET", path, trusted, &[("host", "localhost:7777")])).status,
                200,
                "{path} {trusted}"
            );
            // Token-less access needs a Host that is this hub (DNS rebinding): none, or someone else's name, is refused,
            // and the admin token still works from anywhere.
            for host in [&[][..], &[("host", "rebind.attacker.test:7777")][..]] {
                assert_eq!(hub.handle(req("GET", path, trusted, host)).status, 401, "{path} from {trusted} {host:?}");
            }
            // Behind a proxy the TCP peer says nothing about the real client.
            let r = req("GET", path, trusted, &[("host", "localhost"), ("x-forwarded-for", "8.8.8.8")]);
            assert_eq!(hub.handle(r).status, 401, "{path} from {trusted} with XFF");
        }
        for untrusted in ["8.8.8.8", "100.128.0.1", "fd7a:115c:a1e1::1", "192.168.1.5"] {
            assert_eq!(hub.handle(req("GET", path, untrusted, &[])).status, 401, "{path} from {untrusted}");
        }
    }

    let admin = format!("Bearer {}", hub.admin_token);
    let r = req("GET", "/api/state", "100.100.1.2", &[("host", "rebind.attacker.test"), ("authorization", &admin)]);
    assert_eq!(hub.handle(r).status, 200);
    let r = req("GET", "/api/state", "127.0.0.1", &[("host", "evil.example:17881")]);
    assert_eq!(hub.handle(r).status, 401);
    let r = req("GET", "/api/state", "127.0.0.1", &[("host", "127.0.0.1:17881")]);
    assert_eq!(hub.handle(r).status, 200);

    // Write routes are not covered by trust: /report still wants a token.
    let mut r = req("POST", "/report", "100.100.1.2", &[("content-type", "application/json")]);
    r.body = b"{}".to_vec();
    assert_eq!(hub.handle(r).status, 401);
    let _ = std::fs::remove_dir_all(&dir);
}
