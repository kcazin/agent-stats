//! `GET /release` and `GET /release/<version>/<file>`: auth, the advertised version and targets, byte-identical
//! downloads of a large tarball, and 404 for everything that is not exactly a file of this hub's complete mirror.

use agent_stats_common::VERSION;
use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::{auth, db, server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const NOW: f64 = 1_791_946_800_000.0;

struct Rig {
    base: String,
    addr: std::net::SocketAddr,
    token: String,
    releases: PathBuf,
    dir: PathBuf,
    _hub: std::sync::Arc<server::Hub>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("agent-stats-hub-release-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths =
        Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), releases_dir: dir.join("releases") };
    let toml = "name = \"test\"\nlisten = \"127.0.0.1:0\"\ntrust_loopback = false\n";
    let cfg = Config::parse(&paths, toml).unwrap();
    let clock = server::Clock::real();
    clock.set(NOW);
    let opts =
        server::Options { insecure_no_auth: false, test_hooks: false, clock, github: None, publish_on_change: false };
    let (hub, addr) = agent_stats_hub::start(cfg, paths.clone(), opts).unwrap();
    let base = format!("http://{addr}");
    let code = {
        let d = db::open(&paths.db()).unwrap();
        auth::create_code(&d, Some("Laptop"), Duration::from_secs(600), NOW).unwrap()
    };
    let mut res = agent()
        .post(format!("{base}/pair"))
        .header("content-type", "application/json")
        .send(format!(r#"{{"code":"{code}","machine":"laptop"}}"#))
        .unwrap();
    let body = res.body_mut().read_to_string().unwrap();
    let token =
        agent_stats_hub::json::J::parse(&body).unwrap().get("token").and_then(|t| t.as_str()).unwrap().to_string();
    Rig { base, addr, token, releases: paths.releases_dir, dir, _hub: hub }
}

/// A raw GET (the path goes out exactly as written): status, head and body.
fn raw_get(rig: &Rig, path: &str, token: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(rig.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}\r\n").as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").expect("response head") + 4;
    let head = String::from_utf8_lossy(&out[..at]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, out[at..].to_vec())
}

fn get(rig: &Rig, path: &str) -> (u16, String, Vec<u8>) {
    raw_get(rig, path, Some(&rig.token))
}

fn mirror(rig: &Rig, version: &str, complete: bool) -> PathBuf {
    let dir = rig.releases.join(version);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SHA256SUMS"), "abc  agent-stats\n").unwrap();
    if complete {
        std::fs::write(dir.join(".complete"), "").unwrap();
    }
    dir
}

fn tarball(version: &str, target: &str) -> String {
    format!("agent-stats-{version}-{target}.tar.gz")
}

/// Deterministic, incompressible-looking bytes.
fn blob(len: usize) -> Vec<u8> {
    let mut x = 0x9e3779b97f4a7c15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn needs_a_token() {
    let r = rig("auth");
    mirror(&r, VERSION, true);
    assert_eq!(raw_get(&r, "/release", None).0, 401);
    assert_eq!(raw_get(&r, "/release", Some("nope")).0, 401);
    assert_eq!(raw_get(&r, &format!("/release/{VERSION}/SHA256SUMS"), None).0, 401);
    assert_eq!(raw_get(&r, &format!("/release/{VERSION}/SHA256SUMS"), Some("nope")).0, 401);
    // the admin token works too
    let admin = std::fs::read_to_string(r.dir.join("config/secrets/hub-admin.token")).unwrap();
    assert_eq!(raw_get(&r, "/release", Some(admin.trim())).0, 200);
    // GET only
    let mut res = agent()
        .post(format!("{}/release", r.base))
        .header("authorization", format!("Bearer {}", r.token))
        .send("")
        .unwrap();
    assert_eq!(res.status().as_u16(), 405);
    let _ = res.body_mut().read_to_string();
}

#[test]
fn advertises_only_its_own_complete_version() {
    let r = rig("advert");
    let (st, _, body) = get(&r, "/release");
    assert_eq!((st, text(&body).as_str()), (200, r#"{"version":null,"targets":[]}"#));

    // another version, complete, and this one without `.complete`: still nothing
    let other = mirror(&r, "99.0.0", true);
    std::fs::write(other.join(tarball("99.0.0", "x86_64-unknown-linux-gnu")), b"x").unwrap();
    let dir = mirror(&r, VERSION, false);
    std::fs::write(dir.join(tarball(VERSION, "x86_64-unknown-linux-gnu")), b"x").unwrap();
    assert_eq!(text(&get(&r, "/release").2), r#"{"version":null,"targets":[]}"#);

    std::fs::write(dir.join(".complete"), "").unwrap();
    for t in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "aarch64-unknown-linux-gnu"] {
        std::fs::write(dir.join(tarball(VERSION, t)), b"x").unwrap();
    }
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    std::fs::write(dir.join(tarball("0.0.1", "x86_64-apple-darwin")), b"x").unwrap();
    let (st, head, body) = get(&r, "/release");
    assert_eq!(st, 200);
    assert!(head.to_ascii_lowercase().contains("application/json"), "{head}");
    assert_eq!(
        text(&body),
        format!(
            r#"{{"version":"{VERSION}","targets":["aarch64-apple-darwin","aarch64-unknown-linux-gnu","x86_64-unknown-linux-gnu"]}}"#
        )
    );
}

#[test]
fn downloads_are_byte_identical() {
    let r = rig("download");
    let dir = mirror(&r, VERSION, true);
    let big = blob(20 << 20);
    let name = tarball(VERSION, "x86_64-unknown-linux-gnu");
    std::fs::write(dir.join(&name), &big).unwrap();

    let (st, head, body) = get(&r, &format!("/release/{VERSION}/{name}"));
    assert_eq!(st, 200);
    let head = head.to_ascii_lowercase();
    assert!(head.contains("content-type: application/gzip"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert!(head.contains(&format!("content-length: {}", big.len())), "{head}");
    assert!(body == big, "20 MB tarball differs ({} bytes back)", body.len());

    let (st, head, body) = get(&r, &format!("/release/{VERSION}/SHA256SUMS"));
    assert_eq!((st, text(&body).as_str()), (200, "abc  agent-stats\n"));
    assert!(head.to_ascii_lowercase().contains("content-type: text/plain; charset=utf-8"), "{head}");
}

#[test]
fn everything_else_is_404() {
    let r = rig("404");
    let dir = mirror(&r, VERSION, true);
    let name = tarball(VERSION, "aarch64-apple-darwin");
    std::fs::write(dir.join(&name), b"tar").unwrap();
    std::fs::write(dir.join("secret.txt"), b"no").unwrap();
    std::fs::write(r.dir.join("outside"), b"no").unwrap();
    let other = mirror(&r, "99.0.0", true);
    std::fs::write(other.join(tarball("99.0.0", "aarch64-apple-darwin")), b"tar").unwrap();
    let nocomplete = mirror(&r, "98.0.0", false);
    std::fs::write(nocomplete.join(tarball("98.0.0", "aarch64-apple-darwin")), b"tar").unwrap();

    assert_eq!(get(&r, &format!("/release/{VERSION}/{name}")).0, 200);
    let bad = [
        // another version, with and without a complete mirror, and one that was never mirrored
        format!("/release/99.0.0/{}", tarball("99.0.0", "aarch64-apple-darwin")),
        format!("/release/98.0.0/{}", tarball("98.0.0", "aarch64-apple-darwin")),
        format!("/release/{VERSION}/{}", tarball("99.0.0", "aarch64-apple-darwin")),
        format!("/release/0.0.1/{}", tarball("0.0.1", "aarch64-apple-darwin")),
        // a missing target, an invalid one
        format!("/release/{VERSION}/{}", tarball(VERSION, "x86_64-apple-darwin")),
        format!("/release/{VERSION}/{}", tarball(VERSION, "Aarch64")),
        format!("/release/{VERSION}/{}", tarball(VERSION, "a--b")),
        format!("/release/{VERSION}/{}", tarball(VERSION, "-a")),
        format!("/release/{VERSION}/{}", tarball(VERSION, "")),
        // unknown names, including real files that are not release files
        format!("/release/{VERSION}/secret.txt"),
        format!("/release/{VERSION}/.complete"),
        format!("/release/{VERSION}/sha256sums"),
        format!("/release/{VERSION}/"),
        format!("/release/{VERSION}"),
        "/release/".to_string(),
        // traversal, plain and percent-encoded
        format!("/release/../{VERSION}/SHA256SUMS"),
        format!("/release/{VERSION}/../outside"),
        format!("/release/{VERSION}/../{VERSION}/SHA256SUMS"),
        format!("/release/%2e%2e/{VERSION}/SHA256SUMS"),
        format!("/release/{VERSION}/%2e%2e/outside"),
        format!("/release/{VERSION}/%2e%2e%2foutside"),
        format!("/release/{VERSION}/..%2foutside"),
        format!("/release/{VERSION}/{name}/extra"),
        format!("/release/{VERSION}/./{name}"),
        format!("/release//{VERSION}/{name}"),
    ];
    for p in &bad {
        let (st, _, body) = get(&r, p);
        assert_eq!(st, 404, "{p}: {}", text(&body));
    }
    // and no dot-dot path ever leaks the file outside the mirror
    assert!(Path::new(&r.dir.join("outside")).exists());
}
