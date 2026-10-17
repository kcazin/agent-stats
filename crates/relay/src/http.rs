//! A deliberately tiny HTTP/1.1 server: one thread per connection, `Connection: close`, hard read deadlines.
//! The relay has a handful of routes and a single trusted publisher, so this beats a dependency with no read timeouts.

use crate::presence::{valid_id, Presence};
use crate::store::{Publish, Store};
use crate::stream::{StreamLimits, Streams};
use crate::{Config, MAX_BODY};
use agent_stats_common::{ct_eq, now_secs, write_status, Status, HEARTBEAT_SECS, PROTOCOL, VERSION, WELL_KNOWN};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole request head (request line + headers) must arrive within this, however the bytes are spaced.
const HEAD_BUDGET: Duration = Duration::from_secs(10);
/// The (authenticated) body gets the longer budget, counted from the start of the connection.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct Limits {
    head_budget: Duration,
    max_connections: usize,
    stream: StreamLimits,
}

struct Request {
    method: String,
    path: String,
    /// Raw query string (no `?`, no fragment); only `/presence` reads it.
    query: String,
    origin: String,
    auth: String,
    content_length: Option<usize>,
    /// Any `Transfer-Encoding` header at all: unsupported (answered 411 on publish).
    transfer_encoding: bool,
}

struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Response {
    fn text(status: u16, body: &str) -> Response {
        Response { status, headers: vec![("content-type", "text/plain; charset=utf-8".into())], body: body.into() }
    }
    fn json(status: u16, body: String) -> Response {
        Response { status, headers: vec![("content-type", "application/json".into())], body }
    }
}

struct Shared {
    cfg: Config,
    store: Arc<Store>,
    streams: Streams,
    presence: Mutex<Presence>,
    active: AtomicUsize,
    limits: Limits,
}

/// Owns one `active` connection slot; released on drop (also when the thread never starts or the handler panics).
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Running {
    pub addr: SocketAddr,
    accept: std::thread::JoinHandle<()>,
}

impl Running {
    /// Blocks while the accept loop lives. If it ever ends the relay serves nothing: exit 1 so the platform restarts it.
    pub fn wait(self) -> ! {
        let _ = self.accept.join();
        eprintln!("relay: accept loop ended, exiting");
        std::process::exit(1)
    }
}

pub fn start(cfg: Config) -> Result<Running, String> {
    start_with(
        cfg,
        Limits { head_budget: HEAD_BUDGET, max_connections: MAX_CONNECTIONS, stream: StreamLimits::default() },
    )
}

fn start_with(cfg: Config, limits: Limits) -> Result<Running, String> {
    std::fs::create_dir_all(&cfg.data).map_err(|e| format!("cannot create data dir {}: {e}", cfg.data.display()))?;
    let listener = TcpListener::bind(&cfg.listen).map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let store = Arc::new(Store::open(&cfg.data, cfg.stale_after));
    let streams = Streams::start(store.clone(), limits.stream)?;
    let status = cfg.status;
    let shared = Arc::new(Shared {
        cfg,
        store,
        streams,
        presence: Mutex::new(Presence::default()),
        active: AtomicUsize::new(0),
        limits,
    });
    eprintln!(
        "relay: listening on {addr}, data {}, cors {:?}, stale after {}s",
        shared.cfg.data.display(),
        shared.cfg.cors,
        shared.cfg.stale_after
    );
    if status {
        let s = shared.clone();
        let started = now_secs();
        std::thread::spawn(move || loop {
            let st = Status {
                role: "relay".into(),
                version: VERSION.into(),
                pid: std::process::id(),
                started_at: started,
                heartbeat_at: now_secs(),
                detail: json!({"listen": addr.to_string(), "updated": s.store.updated()}),
            };
            if let Err(e) = write_status(&st) {
                eprintln!("relay: status write failed: {e}");
            }
            std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS / 2));
        });
    }
    let s = shared.clone();
    let accept = std::thread::Builder::new()
        .name("relay-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let stream = match conn {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("relay: accept failed: {e}");
                        std::thread::sleep(Duration::from_millis(50)); // don't spin on EMFILE and friends
                        continue;
                    }
                };
                if s.active.fetch_add(1, Ordering::SeqCst) >= s.limits.max_connections {
                    s.active.fetch_sub(1, Ordering::SeqCst);
                    // Shed load, but say so; the tiny reply fits the socket buffer, the short timeout covers the rest.
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    let mut w = stream;
                    let _ = respond(&mut w, Response::text(503, "busy"), "GET", "");
                    continue;
                }
                let slot = Slot(s.clone());
                let spawned = std::thread::Builder::new().name("relay-conn".into()).spawn(move || {
                    // The slot is released even if the handler panics (the guard drops during unwinding).
                    let slot = slot;
                    let ran = std::panic::catch_unwind(AssertUnwindSafe(|| handle_conn(&slot.0, stream)));
                    if ran.is_err() {
                        eprintln!("relay: request handler panicked, continuing");
                    }
                });
                if let Err(e) = spawned {
                    // The closure (stream and slot) was dropped with the error: connection closed, slot released.
                    eprintln!("relay: cannot spawn a connection thread, dropping the connection: {e}");
                }
            }
        })
        .map_err(|e| format!("cannot start the accept thread: {e}"))?;
    Ok(Running { addr, accept })
}

/// Reads one `\n`-terminated line. Every read waits at most the time left until `deadline`, so a client dripping
/// bytes cannot outlive it.
fn read_line_limited(r: &mut BufReader<TcpStream>, used: &mut usize, deadline: Instant) -> Result<String, ()> {
    let mut line = Vec::new();
    loop {
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(())?;
        r.get_ref().set_read_timeout(Some(left.min(IO_TIMEOUT))).map_err(|_| ())?;
        let buf = r.fill_buf().map_err(|_| ())?;
        if buf.is_empty() {
            return Err(()); // EOF mid-line
        }
        let (n, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        *used += n;
        if *used > MAX_HEADER_BYTES {
            return Err(());
        }
        line.extend_from_slice(&buf[..n]);
        r.consume(n);
        if done {
            let line = String::from_utf8(line).map_err(|_| ())?;
            return Ok(line.trim_end_matches(['\r', '\n']).to_string());
        }
    }
}

fn parse_head(reader: &mut BufReader<TcpStream>, deadline: Instant) -> Result<Request, ()> {
    let mut used = 0;
    let line = read_line_limited(reader, &mut used, deadline)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().ok_or(())?, parts.next().ok_or(())?);
    let target = target.split('#').next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut req = Request {
        method: method.to_string(),
        path,
        query,
        origin: String::new(),
        auth: String::new(),
        content_length: None,
        transfer_encoding: false,
    };
    loop {
        let h = read_line_limited(reader, &mut used, deadline)?;
        if h.is_empty() {
            return Ok(req);
        }
        let Some((k, v)) = h.split_once(':') else { return Err(()) };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "origin" => req.origin = v.to_string(),
            "authorization" => req.auth = v.to_string(),
            "content-length" => {
                // Digits only (no sign, no spaces), and repeated headers must agree.
                if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(());
                }
                let n: usize = v.parse().map_err(|_| ())?;
                if req.content_length.is_some_and(|prev| prev != n) {
                    return Err(());
                }
                req.content_length = Some(n);
            }
            "transfer-encoding" => req.transfer_encoding = true,
            _ => {}
        }
    }
}

fn handle_conn(shared: &Shared, stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let started = Instant::now();
    let deadline = started + REQUEST_DEADLINE;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let req = match parse_head(&mut reader, (started + shared.limits.head_budget).min(deadline)) {
        Ok(r) => r,
        Err(()) => return respond(&mut writer, Response::text(400, "bad request"), "GET", ""),
    };
    if req.method == "GET" && req.path == "/stream" {
        return open_stream(shared, &req, writer);
    }
    let (method, origin) = (req.method.clone(), req.origin.clone());
    let res = route(shared, &req, &mut reader, deadline);
    respond(&mut writer, res, &method, &origin)
}

/// `GET /stream`: writes the response head here (blocking, bounded by the write timeout), then the socket belongs to
/// the broadcaster and this connection's thread and slot are released.
fn open_stream(shared: &Shared, req: &Request, mut w: TcpStream) -> std::io::Result<()> {
    let Some(ticket) = shared.streams.reserve() else {
        let mut res = Response::text(503, "too many streams");
        res.headers.extend(cors_headers(shared, &req.origin));
        return respond(&mut w, res, "GET", "");
    };
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nx-accel-buffering: no\r\n",
    );
    for (k, v) in cors_headers(shared, &req.origin) {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
    w.write_all(head.as_bytes())?;
    shared.streams.hand_over(w, ticket);
    Ok(())
}

fn respond(w: &mut TcpStream, res: Response, method: &str, _origin: &str) -> std::io::Result<()> {
    let reason = match res.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", res.status);
    for (k, v) in &res.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", res.body.len()));
    w.write_all(head.as_bytes())?;
    if method != "HEAD" {
        w.write_all(res.body.as_bytes())?;
    }
    w.flush()?;
    let _ = w.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn cors_headers(shared: &Shared, origin: &str) -> Vec<(&'static str, String)> {
    let mut h = vec![("vary", "Origin".to_string())];
    if !origin.is_empty() && origin_allowed(&shared.cfg.cors, origin) {
        h.push(("access-control-allow-origin", origin.to_string()));
    }
    h
}

fn origin_allowed(list: &[String], origin: &str) -> bool {
    if list.iter().any(|o| o == origin) {
        return true;
    }
    // http://localhost or http://localhost:<digits>, nothing else.
    match origin.strip_prefix("http://localhost") {
        Some("") => true,
        Some(rest) => rest.strip_prefix(':').is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

fn authorized(shared: &Shared, auth: &str) -> bool {
    let token = &shared.cfg.token;
    if token.is_empty() {
        return false;
    }
    let expected = format!("Bearer {token}");
    ct_eq(auth.as_bytes(), expected.as_bytes())
}

fn route(shared: &Shared, req: &Request, reader: &mut BufReader<TcpStream>, deadline: Instant) -> Response {
    let get = req.method == "GET" || req.method == "HEAD";
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/publish") => publish(shared, req, reader, deadline),
        (_, "/stats") if get => {
            let mut res = match shared.store.snapshot() {
                Some(body) => {
                    let mut r = Response::json(200, body);
                    r.headers.push(("cache-control", "public, max-age=5".into()));
                    r
                }
                None => Response::text(503, "no data yet"),
            };
            res.headers.extend(cors_headers(shared, &req.origin));
            res
        }
        ("OPTIONS", "/stats") => preflight(shared, &req.origin),
        (_, "/presence") if get => {
            let mut res = presence(shared, &req.query);
            res.headers.push(("cache-control", "no-store".into()));
            res.headers.extend(cors_headers(shared, &req.origin));
            res
        }
        ("OPTIONS", "/presence") => preflight(shared, &req.origin),
        ("OPTIONS", "/stream") => preflight(shared, &req.origin),
        (_, "/healthz") if get => Response::text(200, "ok"),
        (_, "/") if get => Response::text(200, "ok"),
        (_, WELL_KNOWN) if get => {
            let mut r = Response::json(
                200,
                json!({"role":"relay","version":VERSION,"protocol":PROTOCOL,"updated":shared.store.updated()})
                    .to_string(),
            );
            r.headers.extend(cors_headers(shared, &req.origin));
            r
        }
        (_, "/publish" | "/stats" | "/presence" | "/stream" | "/healthz" | "/") => {
            Response::text(405, "method not allowed")
        }
        _ => Response::text(404, "not found"),
    }
}

fn preflight(shared: &Shared, origin: &str) -> Response {
    let mut res = Response::text(204, "");
    res.headers.extend(cors_headers(shared, origin));
    res.headers.push(("access-control-allow-methods", "GET, OPTIONS".into()));
    res.headers.push(("access-control-max-age", "600".into()));
    res
}

/// `GET /presence[?id=<id>]`: an `id` records it; none just counts; a malformed one is a 400.
fn presence(shared: &Shared, query: &str) -> Response {
    let id = query.split('&').find_map(|kv| kv.strip_prefix("id="));
    if id.is_some_and(|id| !valid_id(id)) {
        return Response::text(400, "bad id");
    }
    let now = Instant::now();
    let mut p = shared.presence.lock().unwrap_or_else(|e| e.into_inner());
    let online = match id {
        Some(id) => p.touch(id, now),
        None => p.count(now),
    };
    Response::json(200, format!("{{\"online\":{online}}}"))
}

fn publish(shared: &Shared, req: &Request, reader: &mut BufReader<TcpStream>, deadline: Instant) -> Response {
    if !authorized(shared, &req.auth) {
        return Response::text(401, "unauthorized");
    }
    if req.transfer_encoding {
        return Response::text(411, "content-length required");
    }
    let Some(len) = req.content_length else { return Response::text(411, "content-length required") };
    if len > MAX_BODY {
        return Response::text(413, "too large");
    }
    let mut body = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
            return Response::text(408, "request timeout");
        };
        if reader.get_ref().set_read_timeout(Some(left.min(IO_TIMEOUT))).is_err() {
            return Response::text(408, "request timeout");
        }
        match reader.read(&mut body[got..]) {
            Ok(0) => return Response::text(400, "truncated body"),
            Ok(n) => got += n,
            Err(_) => return Response::text(408, "request timeout"),
        }
    }
    let Ok(text) = String::from_utf8(body) else { return Response::text(422, "body is not utf-8") };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(obj)) => match shared.store.publish(text, obj) {
            Publish::Stored => {
                shared.streams.changed();
                Response::json(200, "{\"ok\":true}".into())
            }
            Publish::IgnoredOlder => Response::json(200, "{\"ok\":true,\"ignored\":\"older\"}".into()),
            Publish::Future => Response::json(400, "{\"ok\":false,\"error\":\"updated is in the future\"}".into()),
            Publish::Persist(e) => {
                eprintln!("relay: persist failed, snapshot not advanced: {e}");
                Response::text(500, "persist failed")
            }
        },
        Ok(_) => Response::text(422, "body must be a JSON object"),
        Err(_) => Response::text(422, "body is not valid JSON"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Reply {
        status: u16,
        headers: String,
        body: String,
    }
    impl Reply {
        fn header(&self, name: &str) -> Option<String> {
            self.headers.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-stats-relay-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(data: &std::path::Path) -> Config {
        Config {
            listen: "127.0.0.1:0".into(),
            data: data.to_path_buf(),
            token: "secret".into(),
            cors: vec!["https://example.com".into()],
            stale_after: 120,
            status: false,
        }
    }

    fn raw(addr: SocketAddr, req: &str) -> Reply {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Reply { status, headers: head.to_string(), body: body.to_string() }
    }

    fn post(addr: SocketAddr, token: Option<&str>, body: &str) -> Reply {
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        raw(addr, &format!("POST /publish HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\r\n{body}", body.len()))
    }

    fn get(addr: SocketAddr, path: &str, origin: Option<&str>) -> Reply {
        let o = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
        raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n{o}\r\n"))
    }

    fn now() -> u64 {
        now_secs()
    }

    #[test]
    fn auth_none_wrong_right_and_empty_token() {
        let d = tmp("auth");
        let a = start(cfg(&d)).unwrap().addr;
        let body = format!("{{\"updated\":{}}}", now());
        assert_eq!(post(a, None, &body).status, 401);
        assert_eq!(post(a, Some("nope"), &body).status, 401);
        assert_eq!(post(a, Some("secret"), &body).status, 200);
        assert_eq!(get(a, "/stats", None).status, 200);

        let d2 = tmp("auth-empty");
        let mut c = cfg(&d2);
        c.token = String::new();
        let a2 = start(c).unwrap().addr;
        assert_eq!(post(a2, Some(""), &body).status, 401);
        assert_eq!(
            raw(a2, "POST /publish HTTP/1.1\r\nAuthorization: Bearer \r\nContent-Length: 2\r\n\r\n{}").status,
            401
        );
    }

    #[test]
    fn no_data_then_roundtrip_and_headers() {
        let d = tmp("rt");
        let a = start(cfg(&d)).unwrap().addr;
        let r = get(a, "/stats", None);
        assert_eq!(r.status, 503);
        assert_eq!(r.body, "no data yet");
        assert_eq!(r.header("vary").as_deref(), Some("Origin"));
        let body = format!("{{\"updated\":{},\"live\":1}}", now());
        assert_eq!(post(a, Some("secret"), &body).body, "{\"ok\":true}");
        let r = get(a, "/stats", None);
        assert_eq!(r.body, body);
        assert_eq!(r.header("cache-control").as_deref(), Some("public, max-age=5"));
        assert_eq!(r.header("content-type").as_deref(), Some("application/json"));
        assert_eq!(std::fs::read_to_string(d.join("stats.json")).unwrap(), body);
        assert_eq!(get(a, "/healthz", None).body, "ok");
        let wk: Value = serde_json::from_str(&get(a, WELL_KNOWN, None).body).unwrap();
        assert_eq!(wk["role"], "relay");
        assert_eq!(wk["protocol"], PROTOCOL);
        assert_eq!(get(a, "/nope", None).status, 404);
    }

    #[test]
    fn presence_counts_distinct_ids() {
        let d = tmp("presence");
        let a = start(cfg(&d)).unwrap().addr;
        assert_eq!(get(a, "/presence", None).body, "{\"online\":0}");
        let r = get(a, "/presence?id=aaaaaaaa", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"online\":1}");
        assert_eq!(r.header("content-type").as_deref(), Some("application/json"));
        assert_eq!(get(a, "/presence?id=bbbbbbbb&x=1", None).body, "{\"online\":2}");
        assert_eq!(get(a, "/presence?x=1&id=aaaaaaaa", None).body, "{\"online\":2}");
        // no id: count only, records nothing
        assert_eq!(get(a, "/presence", None).body, "{\"online\":2}");
        let r = raw(a, "HEAD /presence?id=cccccccc HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!((r.status, r.body.as_str()), (200, ""));
        assert_eq!(get(a, "/presence", None).body, "{\"online\":3}");
    }

    #[test]
    fn presence_without_id_records_nothing() {
        let d = tmp("presence-noid");
        let a = start(cfg(&d)).unwrap().addr;
        assert_eq!(get(a, "/presence", None).body, "{\"online\":0}");
        assert_eq!(get(a, "/presence?other=1", None).body, "{\"online\":0}");
        assert_eq!(get(a, "/presence?id=aaaaaaaa", None).body, "{\"online\":1}");
    }

    #[test]
    fn presence_rejects_bad_ids() {
        let d = tmp("presence-bad");
        let a = start(cfg(&d)).unwrap().addr;
        for q in ["id=", "id=short", "id=abcdefg!", "id=abcdefg%20", &format!("id={}", "a".repeat(65))] {
            let r = get(a, &format!("/presence?{q}"), None);
            assert_eq!((r.status, r.body.as_str()), (400, "bad id"), "{q}");
        }
        assert_eq!(get(a, "/presence", None).body, "{\"online\":0}");
    }

    #[test]
    fn presence_headers_cors_and_methods() {
        let d = tmp("presence-cors");
        let a = start(cfg(&d)).unwrap().addr;
        let r = get(a, "/presence?id=aaaaaaaa", Some("https://example.com"));
        assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
        assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("https://example.com"));
        assert_eq!(r.header("vary").as_deref(), Some("Origin"));
        let r = get(a, "/presence?id=aaaaaaaa", Some("https://evil.example"));
        assert_eq!(r.status, 200);
        assert_eq!(r.header("access-control-allow-origin"), None);
        assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
        let r = raw(a, "OPTIONS /presence HTTP/1.1\r\nOrigin: https://example.com\r\n\r\n");
        assert_eq!(r.status, 204);
        assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("https://example.com"));
        assert_eq!(r.header("access-control-allow-methods").as_deref(), Some("GET, OPTIONS"));
        assert_eq!(raw(a, "POST /presence?id=aaaaaaaa HTTP/1.1\r\nContent-Length: 0\r\n\r\n").status, 405);
    }

    #[test]
    fn size_limit_by_content_length_without_sending_body() {
        let d = tmp("size");
        let a = start(cfg(&d)).unwrap().addr;
        let r = raw(
            a,
            &format!(
                "POST /publish HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            ),
        );
        assert_eq!(r.status, 413);
        // unauthenticated oversize is refused as 401, not read
        let r = raw(a, &format!("POST /publish HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1));
        assert_eq!(r.status, 401);
        // exactly at the cap is fine
        let pad = "x".repeat(MAX_BODY - 40);
        let body = format!("{{\"updated\":{},\"pad\":\"{pad}\"}}", now());
        assert!(body.len() <= MAX_BODY);
        assert_eq!(post(a, Some("secret"), &body).status, 200);
    }

    #[test]
    fn rejects_bad_json_and_non_objects() {
        let d = tmp("bad");
        let a = start(cfg(&d)).unwrap().addr;
        assert_eq!(post(a, Some("secret"), "not json").status, 422);
        assert_eq!(post(a, Some("secret"), "[1,2]").status, 422);
        assert_eq!(post(a, Some("secret"), "\"s\"").status, 422);
        assert_eq!(get(a, "/stats", None).status, 503);
    }

    #[test]
    fn older_updated_is_ignored() {
        let d = tmp("older");
        let a = start(cfg(&d)).unwrap().addr;
        let t = now();
        let new = format!("{{\"updated\":{t},\"v\":\"new\"}}");
        assert_eq!(post(a, Some("secret"), &new).status, 200);
        let r = post(a, Some("secret"), &format!("{{\"updated\":{},\"v\":\"old\"}}", t - 10));
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"ok\":true,\"ignored\":\"older\"}");
        assert_eq!(get(a, "/stats", None).body, new);
        assert_eq!(std::fs::read_to_string(d.join("stats.json")).unwrap(), new);
        // equal and newer are accepted; millisecond epochs compare consistently
        assert_eq!(post(a, Some("secret"), &format!("{{\"updated\":{},\"v\":\"b\"}}", t + 5)).body, "{\"ok\":true}");
    }

    #[test]
    fn future_publish_is_rejected() {
        let d = tmp("future");
        let a = start(cfg(&d)).unwrap().addr;
        let t = now();
        let r = post(a, Some("secret"), &format!("{{\"updated\":{},\"v\":\"bad\"}}", t + 86_400));
        assert_eq!(r.status, 400);
        assert_eq!(r.body, "{\"ok\":false,\"error\":\"updated is in the future\"}");
        assert_eq!(r.header("content-type").as_deref(), Some("application/json"));
        // milliseconds too; nothing stored
        assert_eq!(post(a, Some("secret"), &format!("{{\"updated\":{}}}", (t + 86_400) * 1000)).status, 400);
        assert_eq!(get(a, "/stats", None).status, 503);
        assert!(!d.join("stats.json").exists());
        // a small skew is fine, and real updates keep flowing
        assert_eq!(post(a, Some("secret"), &format!("{{\"updated\":{}}}", t + 60)).status, 200);
    }

    #[test]
    fn future_doc_on_disk_does_not_block_real_publishes() {
        let d = tmp("future-disk");
        let t = now();
        std::fs::write(d.join("stats.json"), format!("{{\"updated\":{},\"v\":\"future\"}}", t + 86_400)).unwrap();
        let a = start(cfg(&d)).unwrap().addr;
        let fresh = format!("{{\"updated\":{t},\"v\":\"real\"}}");
        assert_eq!(post(a, Some("secret"), &fresh).body, "{\"ok\":true}");
        assert_eq!(get(a, "/stats", None).body, fresh);
        // and ordering works normally again afterwards
        let r = post(a, Some("secret"), &format!("{{\"updated\":{}}}", t - 10));
        assert_eq!(r.body, "{\"ok\":true,\"ignored\":\"older\"}");
    }

    #[test]
    fn stale_flag_injected_when_old() {
        let server = |name: &str| {
            let mut c = cfg(&tmp(name));
            c.stale_after = 60;
            start(c).unwrap().addr
        };
        let stale = |a: SocketAddr| serde_json::from_str::<Value>(&get(a, "/stats", None).body).unwrap();
        let a = server("stale-fresh");
        post(a, Some("secret"), &format!("{{\"updated\":{},\"k\":1}}", now()));
        assert!(stale(a).get("stale").is_none());
        let a = server("stale-old");
        post(a, Some("secret"), &format!("{{\"updated\":{},\"k\":2}}", now() - 3600));
        let v = stale(a);
        assert_eq!(v["stale"], true);
        assert_eq!(v["k"], 2);
        // a milliseconds `updated` works too
        let a = server("stale-ms");
        post(a, Some("secret"), &format!("{{\"updated\":{},\"k\":3}}", (now() - 3600) * 1000));
        assert_eq!(stale(a)["stale"], true);
        // a publisher-supplied `stale` never sticks
        let a = server("stale-sneaky");
        post(a, Some("secret"), &format!("{{\"updated\":{},\"stale\":true}}", now()));
        assert!(stale(a).get("stale").is_none());
    }

    #[test]
    fn cors_allow_deny_localhost() {
        let d = tmp("cors");
        let a = start(cfg(&d)).unwrap().addr;
        post(a, Some("secret"), &format!("{{\"updated\":{}}}", now()));
        let allow = |o: &str| get(a, "/stats", Some(o)).header("access-control-allow-origin");
        assert_eq!(allow("https://example.com").as_deref(), Some("https://example.com"));
        assert_eq!(allow("http://localhost:3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(allow("http://localhost").as_deref(), Some("http://localhost"));
        assert_eq!(allow("https://evil.com"), None);
        assert_eq!(allow("https://example.com.evil.com"), None);
        assert_eq!(allow("http://localhost.evil.com"), None);
        assert_eq!(allow("http://localhost:abc"), None);
        assert_eq!(allow("https://localhost:3000"), None);
        let r = get(a, "/stats", Some("https://evil.com"));
        assert_eq!(r.header("vary").as_deref(), Some("Origin"));
        // default config: nothing allowed but localhost
        let d2 = tmp("cors-empty");
        let mut c = cfg(&d2);
        c.cors.clear();
        let a2 = start(c).unwrap().addr;
        assert_eq!(get(a2, "/stats", Some("https://kilcazn.com")).header("access-control-allow-origin"), None);
        assert!(get(a2, "/stats", Some("http://localhost:5173")).header("access-control-allow-origin").is_some());
        // 503 carries CORS headers too
        assert!(get(a2, "/stats", Some("http://localhost:1")).header("access-control-allow-origin").is_some());
    }

    #[test]
    fn reloads_old_relay_file_and_bad_file_is_ignored() {
        let d = tmp("reload");
        let t = now();
        let old = format!("{{\"updated\":{t},\"live\":{{\"a\":1}},\"projects\":[]}}");
        std::fs::write(d.join("stats.json"), &old).unwrap();
        let a = start(cfg(&d)).unwrap().addr;
        assert_eq!(get(a, "/stats", None).body, old);
        // an older publish after reload is ignored
        let r = post(a, Some("secret"), &format!("{{\"updated\":{}}}", t - 5));
        assert_eq!(r.body, "{\"ok\":true,\"ignored\":\"older\"}");

        let d2 = tmp("reload-bad");
        std::fs::write(d2.join("stats.json"), "garbage").unwrap();
        let a2 = start(cfg(&d2)).unwrap().addr;
        assert_eq!(get(a2, "/stats", None).status, 503);

        // a stored `stale` (never ours) is dropped on load, so the injected one can't duplicate it
        let d3 = tmp("reload-stale");
        std::fs::write(d3.join("stats.json"), format!("{{\"updated\":{},\"stale\":true,\"a\":1}}", t - 3600)).unwrap();
        let a3 = start(cfg(&d3)).unwrap().addr;
        let body = get(a3, "/stats", None).body;
        assert_eq!(body.matches("stale").count(), 1, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["a"], 1);
    }

    #[test]
    fn persist_failure_is_500_and_memory_not_advanced() {
        let d = tmp("persist");
        let a = start(cfg(&d)).unwrap().addr;
        let t = now();
        let first = format!("{{\"updated\":{t},\"v\":1}}");
        assert_eq!(post(a, Some("secret"), &first).status, 200);
        // make the target unwritable: replace stats.json by a directory
        std::fs::remove_file(d.join("stats.json")).unwrap();
        std::fs::create_dir(d.join("stats.json")).unwrap();
        assert_eq!(post(a, Some("secret"), &format!("{{\"updated\":{},\"v\":2}}", t + 1)).status, 500);
        assert_eq!(get(a, "/stats", None).body, first);
    }

    #[test]
    fn missing_content_length_and_stalled_body() {
        let d = tmp("framing");
        let a = start(cfg(&d)).unwrap().addr;
        let r = raw(a, "POST /publish HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n");
        assert_eq!(r.status, 411);
        // declared body longer than sent, then the client half-closes: 400 not a hang
        let mut s = TcpStream::connect(a).unwrap();
        s.write_all(b"POST /publish HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: 50\r\n\r\n{}").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    #[test]
    fn header_drip_is_cut_off_at_the_head_budget() {
        let d = tmp("drip");
        let limits = Limits {
            head_budget: Duration::from_secs(2),
            max_connections: MAX_CONNECTIONS,
            stream: StreamLimits::default(),
        };
        let a = start_with(cfg(&d), limits).unwrap().addr;
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        s.write_all(b"GET /stats HTTP/1.1\r\nX-Drip: ").unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut closed = false;
        while t0.elapsed() < Duration::from_secs(8) {
            if s.write_all(b"a").is_err() {
                closed = true;
                break;
            }
            let mut buf = [0u8; 256];
            match s.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(900));
        }
        assert!(closed, "still open after {:?}", t0.elapsed());
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 400"), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn over_the_cap_gets_a_503_and_slots_come_back() {
        let d = tmp("cap");
        let limits =
            Limits { head_budget: Duration::from_secs(2), max_connections: 2, stream: StreamLimits::default() };
        let a = start_with(cfg(&d), limits).unwrap().addr;
        let held: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(a).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(200));
        let t0 = Instant::now();
        let r = get(a, "/healthz", None);
        assert_eq!(r.status, 503);
        assert_eq!(r.header("connection").as_deref(), Some("close"));
        assert!(t0.elapsed() < Duration::from_secs(1));
        drop(held);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(get(a, "/healthz", None).status, 200);
    }

    #[test]
    fn strict_content_length_and_no_transfer_encoding() {
        let d = tmp("strict");
        let a = start(cfg(&d)).unwrap().addr;
        let body = format!("{{\"updated\":{}}}", now());
        let n = body.len();
        let send = |headers: &str| {
            raw(a, &format!("POST /publish HTTP/1.1\r\nAuthorization: Bearer secret\r\n{headers}\r\n{body}")).status
        };
        assert_eq!(send(&format!("Content-Length: {n}\r\nContent-Length: 1000\r\n")), 400);
        assert_eq!(send(&format!("Content-Length: 1000\r\nContent-Length: {n}\r\n")), 400);
        assert_eq!(send(&format!("Content-Length: +{n}\r\n")), 400);
        assert_eq!(send("Content-Length:\r\n"), 400);
        assert_eq!(send(&format!("Content-Length: {} {}\r\n", n / 10, n % 10)), 400);
        assert_eq!(send(&format!("Content-Length: -{n}\r\n")), 400);
        assert_eq!(send(&format!("Content-Length: {n}\r\nTransfer-Encoding: identity\r\n")), 411);
        assert_eq!(
            send(&format!("Transfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\nContent-Length: {n}\r\n")),
            411
        );
        // identical repeats are harmless
        assert_eq!(send(&format!("Content-Length: {n}\r\nContent-Length: {n}\r\n")), 200);
    }

    #[test]
    fn args_and_env_fallbacks() {
        let a: Vec<String> = [
            "--listen",
            "127.0.0.1:9",
            "--data=/x",
            "--cors",
            "https://a.com/, https://b.com",
            "--stale-after",
            "30",
            "--no-status",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let c = crate::parse_args(&a).unwrap();
        assert_eq!(c.listen, "127.0.0.1:9");
        assert_eq!(c.data, PathBuf::from("/x"));
        assert_eq!(c.cors, vec!["https://a.com", "https://b.com"]);
        assert_eq!(c.stale_after, 30);
        assert!(!c.status);
        assert!(crate::parse_args(&["--bogus".to_string()]).is_err());
    }

    // ---- GET /stream ----

    /// One SSE block: its `event`, joined `data`, comment lines and `retry`.
    #[derive(Debug, Default)]
    struct Ev {
        event: String,
        data: String,
        comments: Vec<String>,
        retry: Option<String>,
    }

    /// A test-side `/stream` client: un-chunks the body and splits it into SSE blocks.
    struct Sse {
        s: TcpStream,
        status: u16,
        head: String,
        /// Chunk-encoded bytes not yet decoded.
        raw: Vec<u8>,
        /// Decoded body bytes not yet split into blocks.
        body: Vec<u8>,
    }

    impl Sse {
        fn open(addr: SocketAddr, origin: Option<&str>) -> Sse {
            let mut s = TcpStream::connect(addr).unwrap();
            let o = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
            s.write_all(format!("GET /stream HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n{o}\r\n").as_bytes())
                .unwrap();
            let mut sse = Sse { s, status: 0, head: String::new(), raw: Vec::new(), body: Vec::new() };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(i) = sse.raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    sse.head = String::from_utf8(sse.raw[..i].to_vec()).unwrap();
                    sse.raw.drain(..i + 4);
                    break;
                }
                assert!(sse.fill(deadline), "no response head");
            }
            sse.status = sse.head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            sse
        }

        fn header(&self, name: &str) -> Option<String> {
            Reply { status: self.status, headers: self.head.clone(), body: String::new() }.header(name)
        }

        /// Reads more bytes; false on EOF, error or deadline.
        fn fill(&mut self, deadline: Instant) -> bool {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return false;
            };
            self.s.set_read_timeout(Some(left)).unwrap();
            let mut buf = [0u8; 65536];
            match self.s.read(&mut buf) {
                Ok(0) | Err(_) => false,
                Ok(n) => {
                    self.raw.extend_from_slice(&buf[..n]);
                    true
                }
            }
        }

        fn dechunk(&mut self) {
            loop {
                let Some(i) = self.raw.windows(2).position(|w| w == b"\r\n") else { return };
                let len = usize::from_str_radix(std::str::from_utf8(&self.raw[..i]).unwrap(), 16).unwrap();
                if self.raw.len() < i + 2 + len + 2 {
                    return;
                }
                assert_eq!(&self.raw[i + 2 + len..i + 4 + len], b"\r\n", "bad chunk framing");
                self.body.extend_from_slice(&self.raw[i + 2..i + 2 + len]);
                self.raw.drain(..i + 4 + len);
            }
        }

        /// The next SSE block, or `None` when nothing arrives within `wait` (or the stream ended).
        fn next(&mut self, wait: Duration) -> Option<Ev> {
            let deadline = Instant::now() + wait;
            loop {
                self.dechunk();
                if let Some(i) = self.body.windows(2).position(|w| w == b"\n\n") {
                    let block = String::from_utf8(self.body[..i].to_vec()).unwrap();
                    self.body.drain(..i + 2);
                    let mut ev = Ev::default();
                    let mut data = Vec::new();
                    for line in block.lines() {
                        if let Some(c) = line.strip_prefix(':') {
                            ev.comments.push(c.trim().to_string());
                        } else if let Some((k, v)) = line.split_once(':') {
                            let v = v.strip_prefix(' ').unwrap_or(v).to_string();
                            match k {
                                "event" => ev.event = v,
                                "data" => data.push(v),
                                "retry" => ev.retry = Some(v),
                                _ => {}
                            }
                        }
                    }
                    ev.data = data.join("\n");
                    return Some(ev);
                }
                if !self.fill(deadline) {
                    return None;
                }
            }
        }

        /// The next `stats` or `patch` event, skipping pings.
        fn next_data(&mut self, wait: Duration) -> Option<Ev> {
            let deadline = Instant::now() + wait;
            loop {
                let ev = self.next(deadline.saturating_duration_since(Instant::now()))?;
                if !ev.event.is_empty() {
                    return Some(ev);
                }
            }
        }
    }

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Applies `ev` (a `stats` or a `patch`) to the client-side document, as the widget does.
    fn apply_ev(doc: &mut Value, ev: &Ev) {
        match ev.event.as_str() {
            "stats" => *doc = json(&ev.data),
            "patch" => *doc = crate::patch::apply(doc, &json(&ev.data)),
            other => panic!("unexpected event {other}"),
        }
    }

    fn stream_limits(f: impl FnOnce(&mut StreamLimits)) -> Limits {
        let mut stream = StreamLimits::default();
        f(&mut stream);
        Limits { head_budget: HEAD_BUDGET, max_connections: MAX_CONNECTIONS, stream }
    }

    const WAIT: Duration = Duration::from_secs(3);

    #[test]
    fn stream_headers_cors_retry_and_initial_stats() {
        let d = tmp("stream-head");
        let a = start(cfg(&d)).unwrap().addr;
        // no document yet: retry, then nothing
        let mut s = Sse::open(a, Some("https://example.com"));
        assert_eq!(s.status, 200);
        assert_eq!(s.header("content-type").as_deref(), Some("text/event-stream"));
        assert_eq!(s.header("cache-control").as_deref(), Some("no-store"));
        assert_eq!(s.header("x-accel-buffering").as_deref(), Some("no"));
        assert_eq!(s.header("access-control-allow-origin").as_deref(), Some("https://example.com"));
        assert_eq!(s.header("vary").as_deref(), Some("Origin"));
        assert_eq!(s.header("content-length"), None);
        let first = s.next(WAIT).unwrap();
        assert_eq!(first.retry.as_deref(), Some("3000"));
        assert!(first.event.is_empty());
        assert!(s.next(Duration::from_millis(300)).is_none());
        // the first publish reaches a stream that had nothing as a full stats
        let body = format!("{{\"updated\":{},\"live\":{{\"n\":1}}}}", now());
        assert_eq!(post(a, Some("secret"), &body).status, 200);
        let ev = s.next_data(WAIT).unwrap();
        assert_eq!((ev.event.as_str(), ev.data.as_str()), ("stats", body.as_str()));

        // CORS is the /stats allowlist
        let s = Sse::open(a, Some("https://evil.example"));
        assert_eq!(s.status, 200);
        assert_eq!(s.header("access-control-allow-origin"), None);
        let s = Sse::open(a, Some("http://localhost:5173"));
        assert_eq!(s.header("access-control-allow-origin").as_deref(), Some("http://localhost:5173"));
        let r = raw(a, "OPTIONS /stream HTTP/1.1\r\nOrigin: https://example.com\r\n\r\n");
        assert_eq!(r.status, 204);
        assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("https://example.com"));
        assert_eq!(raw(a, "POST /stream HTTP/1.1\r\nContent-Length: 0\r\n\r\n").status, 405);

        // a stream opened after a publish: retry, then the very bytes /stats serves
        let mut s = Sse::open(a, None);
        assert_eq!(s.next(WAIT).unwrap().retry.as_deref(), Some("3000"));
        let ev = s.next(WAIT).unwrap();
        assert_eq!(ev.event, "stats");
        assert_eq!(ev.data, get(a, "/stats", None).body);
    }

    #[test]
    fn stream_patches_apply_to_exactly_the_new_stats() {
        let d = tmp("stream-patch");
        let t = now();
        let first = format!(
            "{{\"updated\":{t},\"live\":{{\"n\":1,\"agents\":[{{\"a\":null}}],\"gone\":true}},\"today\":{{\"tokens\":5}},\"x\":1.1}}"
        );
        std::fs::write(d.join("stats.json"), &first).unwrap();
        let a = start(cfg(&d)).unwrap().addr;
        let mut s = Sse::open(a, None);
        let mut other = Sse::open(a, None);
        let mut doc = Value::Null;
        let ev = s.next_data(WAIT).unwrap();
        assert_eq!(ev.event, "stats");
        apply_ev(&mut doc, &ev);
        assert_eq!(doc, json(&first));
        apply_ev(&mut Value::Null, &other.next_data(WAIT).unwrap());

        let bodies = [
            format!("{{\"updated\":{},\"live\":{{\"n\":2,\"agents\":[{{\"a\":null}},{{\"b\":1}}]}},\"today\":{{\"tokens\":5}},\"x\":0.30000000000000004}}", t + 1),
            format!("{{\"updated\":{},\"live\":{{\"n\":2,\"agents\":[]}},\"today\":{{\"tokens\":9,\"new\":{{\"k\":[1]}}}},\"x\":1e300}}", t + 2),
            // a member that becomes null cannot be a merge patch: a full stats instead
            format!("{{\"updated\":{},\"live\":null,\"today\":{{\"tokens\":9}},\"x\":1}}", t + 3),
            format!("{{\"updated\":{},\"live\":{{\"n\":3}},\"today\":{{\"tokens\":9}},\"x\":1}}", t + 4),
        ];
        let mut kinds = Vec::new();
        for body in &bodies {
            assert_eq!(post(a, Some("secret"), body).body, "{\"ok\":true}");
            let ev = s.next_data(WAIT).unwrap();
            kinds.push(ev.event.clone());
            if ev.event == "patch" {
                assert!(json(&ev.data).get("updated").is_some(), "patch without updated: {}", ev.data);
            }
            apply_ev(&mut doc, &ev);
            assert_eq!(doc, json(&get(a, "/stats", None).body));
            // the second stream gets the very same event bytes
            let ev2 = other.next_data(WAIT).unwrap();
            assert_eq!((ev2.event, ev2.data), (ev.event, ev.data));
        }
        assert_eq!(kinds, ["patch", "patch", "stats", "patch"]);
        // an older publish and an identical republish send nothing
        post(a, Some("secret"), &format!("{{\"updated\":{}}}", t - 100));
        post(a, Some("secret"), &bodies[3]);
        assert!(s.next_data(Duration::from_millis(500)).is_none());
    }

    #[test]
    fn stream_stale_flips_are_pushed() {
        let d = tmp("stream-stale");
        let mut c = cfg(&d);
        c.stale_after = 1;
        let a = start(c).unwrap().addr;
        let body = format!("{{\"updated\":{},\"k\":1}}", now());
        post(a, Some("secret"), &body);
        let mut s = Sse::open(a, None);
        let mut doc = Value::Null;
        let ev = s.next_data(WAIT).unwrap();
        assert_eq!(ev.event, "stats");
        apply_ev(&mut doc, &ev);
        assert!(doc.get("stale").is_none());
        // by the timer, no publish
        let ev = s.next_data(Duration::from_secs(5)).unwrap();
        assert_eq!((ev.event.as_str(), json(&ev.data)), ("patch", serde_json::json!({"stale":true})));
        apply_ev(&mut doc, &ev);
        assert_eq!(doc, json(&get(a, "/stats", None).body));
        // a fresh publish clears it (merge patch: the member is removed)
        let t1 = now() + 1;
        let fresh = format!("{{\"updated\":{t1},\"k\":2}}");
        post(a, Some("secret"), &fresh);
        let ev = s.next_data(WAIT).unwrap();
        assert_eq!(json(&ev.data), serde_json::json!({"stale":null,"updated":t1,"k":2}));
        apply_ev(&mut doc, &ev);
        assert_eq!(doc, json(&fresh));
    }

    #[test]
    fn stream_pings() {
        let d = tmp("stream-ping");
        let a = start_with(cfg(&d), stream_limits(|l| l.ping = Duration::from_millis(200))).unwrap().addr;
        let mut s = Sse::open(a, None);
        assert_eq!(s.next(WAIT).unwrap().retry.as_deref(), Some("3000"));
        for _ in 0..3 {
            let ev = s.next(Duration::from_secs(1)).unwrap();
            assert_eq!(ev.comments, ["ping"]);
            assert!(ev.event.is_empty() && ev.data.is_empty());
        }
    }

    #[test]
    fn stream_cap_is_separate_from_connections_and_slots_come_back() {
        let d = tmp("stream-cap");
        let mut limits = stream_limits(|l| l.max_streams = 3);
        limits.max_connections = 2;
        let a = start_with(cfg(&d), limits).unwrap().addr;
        // three streams while only two connection slots exist: streams don't hold them
        let mut held: Vec<Sse> = (0..3).map(|_| Sse::open(a, None)).collect();
        assert!(held.iter().all(|s| s.status == 200));
        assert_eq!(get(a, "/healthz", None).status, 200);
        let r = Sse::open(a, Some("https://example.com"));
        assert_eq!(r.status, 503);
        assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("https://example.com"));
        // a closed stream is noticed (EOF) and frees its slot
        held.pop();
        let t0 = Instant::now();
        loop {
            let s = Sse::open(a, None);
            if s.status == 200 {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "slot never came back");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(held[0].next(WAIT).is_some());
    }

    #[test]
    fn a_client_that_never_reads_is_dropped_without_stalling_the_others() {
        let d = tmp("stream-slow");
        let a = start_with(cfg(&d), stream_limits(|l| l.max_streams = 2)).unwrap().addr;
        let t = now();
        post(a, Some("secret"), &format!("{{\"updated\":{t}}}"));
        let slow = Sse::open(a, None); // reads its head, then nothing more
        let mut fast = Sse::open(a, None);
        assert_eq!(fast.next_data(WAIT).unwrap().event, "stats");
        let t0 = Instant::now();
        let mut dropped = false;
        for i in 0..200 {
            // ~400 KB of fresh text per publish: every patch is big
            let pad: String = (0..400_000).map(|j| (b'a' + ((i + j) % 26) as u8) as char).collect();
            let body = format!("{{\"updated\":{},\"pad\":\"{pad}\"}}", t + 1 + i as u64);
            assert_eq!(post(a, Some("secret"), &body).status, 200);
            let sent = Instant::now();
            let ev = fast.next_data(WAIT).expect("the fast stream stalled");
            assert_eq!(ev.event, "patch");
            assert!(sent.elapsed() < Duration::from_secs(2), "fast stream took {:?}", sent.elapsed());
            assert_eq!(json(&ev.data)["pad"].as_str().map(str::len), Some(400_000));
            if Sse::open(a, None).status == 200 {
                dropped = true;
                break;
            }
        }
        assert!(dropped, "the slow stream was never dropped");
        assert!(t0.elapsed() < Duration::from_secs(60));
        // the slow client sees its stream end once it drains what the kernel holds
        let mut slow = slow;
        let deadline = Instant::now() + Duration::from_secs(10);
        while slow.fill(deadline) {
            slow.raw.clear();
        }
        assert!(Instant::now() < deadline, "slow stream still open");
    }

    #[test]
    fn a_stalled_client_is_dropped_after_the_stall_timeout() {
        let d = tmp("stream-stall");
        let limits = stream_limits(|l| {
            l.max_streams = 1;
            l.stall = Duration::from_millis(500);
            l.max_backlog = usize::MAX / 2;
        });
        let a = start_with(cfg(&d), limits).unwrap().addr;
        let t = now();
        let slow = Sse::open(a, None);
        for i in 0..40 {
            let pad = "x".repeat(400_000) + &i.to_string();
            post(a, Some("secret"), &format!("{{\"updated\":{},\"pad\":\"{pad}\"}}", t + i));
        }
        // the kernel buffers fill, nothing moves for 500 ms: the stream is dropped and its slot freed
        let t0 = Instant::now();
        while Sse::open(a, None).status != 200 {
            assert!(t0.elapsed() < Duration::from_secs(5), "stalled stream never dropped");
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(slow);
    }
}
