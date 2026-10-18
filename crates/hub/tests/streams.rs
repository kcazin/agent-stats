//! Realtime: `/api/stream` (auth, first events, a report's `live` within a second), the public `/stream` (stats, patch,
//! stale), publishing on change (debounced, nothing tick-only advances), the stream cap and slow clients.

use agent_stats_hub::config::{Config, Paths};
use agent_stats_hub::http::Request;
use agent_stats_hub::json::J;
use agent_stats_hub::{db, server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

// 2026-10-14T03:00:00Z (2026-10-14 in Tokyo).
const NOW: f64 = 1_791_946_800_000.0;

struct Rig {
    hub: Arc<server::Hub>,
    addr: SocketAddr,
    paths: Paths,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str, extra_toml: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("agent-stats-hub-streams-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths =
        Paths { config_dir: dir.join("config"), state_dir: dir.join("state"), releases_dir: dir.join("releases") };
    agent_stats_common::write_secret(&paths.config_dir.join("publish.token"), "pub-secret").unwrap();
    let toml = format!("name = \"test\"\nlisten = \"127.0.0.1:0\"\n{extra_toml}\n");
    let mut cfg = Config::parse(&paths, &toml).unwrap();
    cfg.legacy_token = Some("secret".into());
    let clock = server::Clock::real();
    clock.set(NOW);
    let opts = server::Options {
        insecure_no_auth: false,
        test_hooks: false,
        clock,
        github: None,
        publish_on_change: true,
        exit_on_config_change: false,
    };
    let (hub, addr) = agent_stats_hub::start(cfg, paths.clone(), opts).unwrap();
    Rig { hub, addr, paths, dir }
}

fn request(addr: SocketAddr, method: &str, path: &str, headers: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let head =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

fn report(rig: &Rig, body: &str) {
    let (st, out) = request(rig.addr, "POST", "/report", "Authorization: Bearer secret\r\n", body);
    assert_eq!(st, 200, "{out}");
}

fn session(id: &str, cwd_len: usize) -> String {
    format!(
        r#"{{"id":"{id}","agent":"claude","kind":"main","parent":null,"cwd":"/{}","repo":"neon","model":"claude-opus-4-5",
            "lastActivity":{NOW},"tokens":3,"state":"working","stateAt":{NOW},"io":{{"in":1,"cr":2,"cw":3,"out":4}}}}"#,
        "a".repeat(cwd_len)
    )
}

fn body(machine: &str, tokens: u64, repo: &str, sessions: &[String]) -> String {
    format!(
        r#"{{"machine":"{machine}","label":"{machine}","platform":"linux","days":[{{"day":"2026-10-14","agent":"claude","model":"claude-opus-4-5","tokens":{tokens},"output":1}}],
            "repos":[{{"day":"2026-10-14","repo":"{repo}","tokens":{tokens}}}],"sessions":[{}]}}"#,
        sessions.join(",")
    )
}

/// An SSE response read chunk by chunk (one event per chunk).
struct Stream {
    r: BufReader<TcpStream>,
    head: String,
}

impl Stream {
    fn open(addr: SocketAddr, path: &str, headers: &str) -> Stream {
        Stream::on(TcpStream::connect(addr).unwrap(), path, headers)
    }
    fn on(mut s: TcpStream, path: &str, headers: &str) -> Stream {
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n").as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        Stream { r, head }
    }
    fn status(&self) -> u16 {
        self.head.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0)
    }
    /// The next chunk, or None on timeout or close.
    fn chunk(&mut self, within: Duration) -> Option<String> {
        self.r.get_ref().set_read_timeout(Some(within)).unwrap();
        let mut size = String::new();
        if self.r.read_line(&mut size).ok()? == 0 {
            return None;
        }
        let n = usize::from_str_radix(size.trim(), 16).ok()?;
        let mut data = vec![0; n + 2];
        self.r.get_ref().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        self.r.read_exact(&mut data).ok()?;
        assert_eq!(&data[n..], b"\r\n");
        data.truncate(n);
        Some(String::from_utf8(data).unwrap())
    }
    /// The next event (skipping pings): name and data.
    fn event(&mut self, within: Duration) -> Option<(String, String)> {
        let until = Instant::now() + within;
        loop {
            let left = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
            let c = self.chunk(left)?;
            if c.starts_with(':') {
                continue;
            }
            return Some(parse_event(&c));
        }
    }
    /// Waits for an event named `name` whose data passes `ok`, skipping others.
    fn until(&mut self, name: &str, within: Duration, ok: impl Fn(&str) -> bool) -> Option<String> {
        let until = Instant::now() + within;
        loop {
            let left = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
            let (n, d) = self.event(left)?;
            if n == name && ok(&d) {
                return Some(d);
            }
        }
    }
}

fn parse_event(c: &str) -> (String, String) {
    let mut name = String::new();
    let mut data = Vec::new();
    for line in c.lines() {
        if let Some(n) = line.strip_prefix("event: ") {
            name = n.into();
        } else if let Some(d) = line.strip_prefix("data: ") {
            data.push(d);
        } else if let Some(r) = line.strip_prefix("retry: ") {
            name = "retry".into();
            data.push(r);
        }
    }
    (name, data.join("\n"))
}

fn wait_for(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !ok() {
        assert!(Instant::now() < until, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
fn api_stream_takes_the_same_auth_as_api_state() {
    let rig = rig("auth", "public = false\ntrusted_networks = [\"100.64.0.0/10\"]");
    let admin = format!("Bearer {}", rig.hub.admin_token);
    let cookie = format!("agent_stats_admin={}", rig.hub.admin_token);
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("127.0.0.1", vec![("host", "localhost:7777")]),
        ("127.0.0.1", vec![("host", "evil.example:17881")]),
        ("127.0.0.1", vec![]),
        ("127.0.0.1", vec![("host", "localhost"), ("x-forwarded-for", "8.8.8.8")]),
        ("100.100.1.2", vec![("host", "localhost:7777")]),
        ("100.100.1.2", vec![("host", "rebind.attacker.test")]),
        ("8.8.8.8", vec![("host", "localhost")]),
        ("8.8.8.8", vec![("host", "rebind.attacker.test"), ("authorization", &admin)]),
        ("8.8.8.8", vec![("authorization", "Bearer wrong")]),
        ("8.8.8.8", vec![("cookie", &cookie)]),
        ("8.8.8.8", vec![("cookie", "agent_stats_admin=wrong")]),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (peer, h) in &cases {
        let state = rig.hub.handle(req("GET", "/api/state", peer, h)).status;
        let res = rig.hub.handle(req("GET", "/api/stream", peer, h));
        assert_eq!(res.status, state, "{peer} {h:?}");
        assert_eq!(res.stream.is_some(), state == 200, "{peer} {h:?}");
        seen.insert(state);
    }
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), [200, 401], "both outcomes covered");
    assert_eq!(rig.hub.handle(req("POST", "/api/stream", "127.0.0.1", &[("host", "localhost")])).status, 405);
    // not public: no `/stream`
    assert_eq!(rig.hub.handle(req("GET", "/stream", "127.0.0.1", &[("host", "localhost")])).status, 404);
    // and over the wire: the 401 is a plain answer, the 200 a stream
    let (st, _) =
        request(rig.addr, "GET", "/api/stream", "Authorization: Bearer wrong\r\nX-Forwarded-For: 1.2.3.4\r\n", "");
    assert_eq!(st, 401);
}

#[test]
fn api_stream_sends_live_then_repos_and_a_report_within_a_second() {
    let rig = rig("private", "public = true");
    report(&rig, &body("alpha", 10, "neon", &[session("s1", 4)]));
    let mut s = Stream::open(rig.addr, "/api/stream", "");
    assert_eq!(s.status(), 200, "{}", s.head);
    let head = s.head.to_ascii_lowercase();
    for h in [
        "content-type: text/event-stream",
        "cache-control: no-store",
        "x-accel-buffering: no",
        "transfer-encoding: chunked",
    ] {
        assert!(head.contains(h), "{h} in {head}");
    }
    assert!(!head.contains("content-length"), "{head}");
    assert_eq!(s.chunk(Duration::from_secs(5)).unwrap(), "retry: 3000\n\n");
    let (name, live) = s.event(Duration::from_secs(5)).unwrap();
    assert_eq!(name, "live");
    let (name, repos) = s.event(Duration::from_secs(5)).unwrap();
    assert_eq!(name, "repos");
    // exactly `now` and `sessions` of /api/state, and the /api/repos body
    let (_, state) = request(rig.addr, "GET", "/api/state", "", "");
    let state = J::parse(&state).unwrap();
    let live = J::parse(&live).unwrap();
    assert_eq!(live.as_obj().unwrap().keys().collect::<Vec<_>>(), ["now", "sessions"]);
    assert_eq!(live.get("now"), state.get("now"));
    assert_eq!(live.get("sessions"), state.get("sessions"));
    assert!(live.get("sessions").unwrap().stringify().contains(r#""id":"s1""#));
    assert_eq!(repos, request(rig.addr, "GET", "/api/repos", "", "").1);

    // a report with a new session: `live` within a second; its new repository: `repos`
    std::thread::sleep(Duration::from_millis(1100));
    let t = Instant::now();
    report(&rig, &body("alpha", 20, "chum", &[session("s1", 4), session("s2", 4)]));
    s.until("live", Duration::from_secs(3), |d| d.contains(r#""id":"s2""#)).expect("live with s2");
    assert!(t.elapsed() < Duration::from_millis(1000), "took {:?}", t.elapsed());
    let repos = s.until("repos", Duration::from_secs(3), |_| true).expect("repos");
    assert!(repos.contains(r#""repo":"chum""#), "{repos}");
    // a stream holds no connection slot or thread: the hub still serves while it is open
    assert_eq!(request(rig.addr, "GET", "/.well-known/agent-stats", "", "").0, 200);
}

/// Each POST body and when it arrived.
fn sink() -> (String, mpsc::Receiver<(Instant, String)>) {
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
            let at = Instant::now();
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
            let _ = tx.send((at, String::from_utf8(body).unwrap()));
        }
    });
    (url, rx)
}

fn samples(rig: &Rig) -> i64 {
    db::open(&rig.paths.db()).unwrap().query_row("SELECT count(*) FROM samples", [], |r| r.get(0)).unwrap()
}

fn at(j: &J, path: &[&str]) -> String {
    path.iter().try_fold(j, |v, k| v.get(k)).map(J::stringify).unwrap_or_else(|| "undefined".into())
}

#[test]
fn reports_publish_within_a_second_and_the_public_stream_follows() {
    let (url, published) = sink();
    let rig = rig(
        "public",
        &format!(
            "public = true\n[[publish]]\nname = \"sink\"\nurl = \"{url}/publish\"\ntoken_file = \"publish.token\""
        ),
    );
    // no payload yet: only `retry`
    let mut s = Stream::open(rig.addr, "/stream", "Origin: https://nowhere.example\r\n");
    assert_eq!(s.status(), 200, "{}", s.head);
    assert!(!s.head.to_ascii_lowercase().contains("access-control-allow-origin"), "{}", s.head);
    assert_eq!(s.chunk(Duration::from_secs(5)).unwrap(), "retry: 3000\n\n");
    assert!(s.chunk(Duration::from_millis(300)).is_none());

    // a report is published within a second, with no tick: the stream's first document, and /stats
    let t = Instant::now();
    report(&rig, &body("alpha", 10, "neon", &[session("s1", 4)]));
    let (arrived, first) = published.recv_timeout(Duration::from_secs(3)).expect("published on change");
    assert!(arrived - t < Duration::from_secs(1), "{:?}", arrived - t);
    let doc = s.until("stats", Duration::from_secs(3), |_| true).expect("stats");
    assert_eq!(doc, first, "fresh: no `stale` member");
    let (_, stats) = request(rig.addr, "GET", "/stats", "", "");
    assert_eq!(at(&J::parse(&stats).unwrap(), &["updated"]), at(&J::parse(&first).unwrap(), &["updated"]));
    assert_eq!(samples(&rig), 0, "a change-triggered publish takes no sample");

    // reports in a burst: at most one publish a second, the last one carries the last report
    for i in 0..5 {
        report(&rig, &body("alpha", 20 + i, "neon", &[session("s1", 4)]));
    }
    let mut times = vec![arrived];
    let mut last = String::new();
    while let Ok((at, b)) = published.recv_timeout(Duration::from_millis(2500)) {
        times.push(at);
        last = b;
    }
    assert!(times.len() <= 4, "{} publishes for 6 reports in ~0 s", times.len());
    for w in times.windows(2) {
        assert!(w[1] - w[0] >= Duration::from_millis(950), "{:?} apart", w[1] - w[0]);
    }
    assert_eq!(at(&J::parse(&last).unwrap(), &["today", "tokens"]), "24");

    // ticks: samples and the recent-activity window move; a publish between ticks repeats the last tick's
    rig.hub.clock.set(NOW + 15_000.0);
    rig.hub.tick();
    rig.hub.clock.set(NOW + 30_000.0);
    report(&rig, &body("alpha", 40, "neon", &[session("s1", 4)]));
    std::thread::sleep(Duration::from_millis(1200)); // let the change publish go before the tick
    rig.hub.tick();
    let mut bodies: Vec<String> = Vec::new();
    while let Ok((_, b)) = published.recv_timeout(Duration::from_millis(500)) {
        bodies.push(b);
    }
    let tick = J::parse(bodies.last().expect("tick published")).unwrap();
    assert_eq!(samples(&rig), 2);
    assert_ne!(at(&tick, &["projects", "last"]), "undefined", "second tick has last");
    rig.hub.clock.set(NOW + 35_000.0);
    report(&rig, &body("alpha", 50, "neon", &[session("s1", 4)]));
    let (_, between) = published.recv_timeout(Duration::from_secs(3)).expect("published on change");
    let between = J::parse(&between).unwrap();
    assert_eq!(at(&between, &["updated"]), "1791946835000");
    for p in [&["projects", "last"][..], &["projects", "lastMs"], &["live", "agents"]] {
        let (a, b) = (at(&between, p), at(&tick, p));
        if p[1] == "agents" {
            assert_eq!(b.matches("\"flow\"").count(), a.matches("\"flow\"").count(), "flow carried");
        } else {
            assert_eq!(a, b, "{p:?}");
        }
    }
    assert_eq!(samples(&rig), 2, "still two");

    // the stream: a patch per publish, with `updated`; `stale` set by the timer, cleared by the next payload
    let patch = s.until("patch", Duration::from_secs(3), |d| d.contains("1791946835000")).expect("patch");
    assert!(J::parse(&patch).unwrap().get("updated").is_some(), "{patch}");
    rig.hub.clock.set(NOW + 35_000.0 + 121_000.0);
    let stale = s.until("patch", Duration::from_secs(3), |_| true).expect("stale patch");
    assert_eq!(stale, r#"{"stale":true}"#);
    report(&rig, &body("alpha", 60, "neon", &[session("s1", 4)]));
    let fresh = s.until("patch", Duration::from_secs(3), |_| true).expect("fresh patch");
    let fresh = J::parse(&fresh).unwrap();
    assert_eq!(fresh.get("stale"), Some(&J::Null), "{}", fresh.stringify());
    assert!(fresh.get("updated").is_some());
}

#[test]
fn public_stream_cors_is_the_stats_cors() {
    let rig = rig("cors", "public = true\ncors_origins = [\"https://site.example\"]");
    for path in ["/stats", "/stream"] {
        let res = rig.hub.handle(req("OPTIONS", path, "8.8.8.8", &[("origin", "https://site.example")]));
        assert_eq!(res.status, 204);
        let h: Vec<_> = res.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        assert!(h.contains(&"access-control-allow-origin: https://site.example".to_string()), "{path} {h:?}");
        assert!(h.contains(&"access-control-allow-methods: GET".to_string()), "{path} {h:?}");
    }
    let s = Stream::open(rig.addr, "/stream", "Origin: https://site.example\r\n");
    let head = s.head.to_ascii_lowercase();
    assert!(
        head.contains("access-control-allow-origin: https://site.example") && head.contains("vary: origin"),
        "{head}"
    );
}

#[test]
fn streams_have_their_own_cap_and_hold_no_connection_slot() {
    let rig = rig("cap", "public = true");
    // Opening one holds a connection slot until its first events are queued; a burst can find this IP's 8 slots
    // taken for a moment (503 busy from the accept thread), so try again then.
    let open = || loop {
        let s = Stream::open(rig.addr, "/api/stream", "");
        if !s.head.contains("content-length: 16") {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut held: Vec<Stream> = (0..32).map(|_| open()).collect();
    for s in &held {
        assert_eq!(s.status(), 200, "{}", s.head);
    }
    let over = open();
    assert_eq!(over.status(), 503, "{}", over.head);
    assert!(over.head.contains("content-length: 28"), "too many streams: {}", over.head);
    // 32 streams from one IP, and the 8 connection slots per IP are all free
    for _ in 0..10 {
        assert_eq!(request(rig.addr, "GET", "/.well-known/agent-stats", "", "").0, 200);
    }
    held.pop();
    wait_for("a place frees up", Duration::from_secs(3), || {
        let s = open();
        let ok = s.status() == 200;
        if ok {
            held.push(s);
        }
        ok
    });
    wait_for("32 open", Duration::from_secs(2), || rig.hub.streams_open() == 32);
}

/// A client whose receive window is tiny, so the hub's writes to it back up at once.
#[cfg(target_os = "linux")]
fn tiny_window(addr: SocketAddr) -> TcpStream {
    use std::os::fd::FromRawFd;
    let SocketAddr::V4(a) = addr else { panic!("v4") };
    // SAFETY: a fresh socket, options of the right size, a sockaddr_in for connect; the fd goes to TcpStream.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(fd >= 0);
        let v: libc::c_int = 2048;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&v as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        let sa = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: a.port().to_be(),
            sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
            sin_zero: [0; 8],
        };
        let r = libc::connect(
            fd,
            (&sa as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(r, 0);
        TcpStream::from_raw_fd(fd)
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_client_that_stops_reading_is_dropped_and_the_others_carry_on() {
    let rig = rig("slow", "public = true");
    // ~2 MB of sessions: one `live` event is far more than the socket buffers hold
    let big: Vec<String> = (0..1900).map(|i| session(&format!("s{i}"), 1000)).collect();
    report(&rig, &body("alpha", 10, "neon", &big));
    let mut good = Stream::open(rig.addr, "/api/stream", "");
    let _slow = Stream::on(tiny_window(rig.addr), "/api/stream", ""); // reads the head, then nothing
    good.until("repos", Duration::from_secs(10), |_| true).expect("first events");
    wait_for("both open", Duration::from_secs(2), || rig.hub.streams_open() == 2);
    // the next event finds the slow one still more than 256 KiB behind
    report(&rig, &body("alpha", 11, "neon", &big));
    good.until("live", Duration::from_secs(10), |_| true).expect("live");
    wait_for("the slow client dropped", Duration::from_secs(3), || rig.hub.streams_open() == 1);

    // one that stops reading with less than 256 KiB pending: dropped once nothing could be written for 5 s
    let small: Vec<String> = (0..200).map(|i| session(&format!("t{i}"), 1000)).collect();
    report(&rig, &body("alpha", 12, "neon", &small));
    good.until("live", Duration::from_secs(10), |d| d.contains(r#""id":"t0""#)).expect("small live");
    let _stalled = Stream::on(tiny_window(rig.addr), "/api/stream", "");
    wait_for("both open", Duration::from_secs(2), || rig.hub.streams_open() == 2);
    let t = Instant::now();
    wait_for("the stalled client dropped", Duration::from_secs(9), || rig.hub.streams_open() == 1);
    assert!(t.elapsed() >= Duration::from_secs(4), "dropped after {:?}", t.elapsed());
    // the reader that kept up is still served
    report(&rig, &body("alpha", 13, "neon", &small));
    good.until("live", Duration::from_secs(5), |_| true).expect("still live");
}
