//! A small HTTP/1.1 server: one thread per connection, one request per connection. Every read waits at most the time
//! left to a deadline (a short budget for the request head, a longer one for the body), the header size is capped,
//! the body cap and an optional auth check come per request from the `Policy` and run before any body is read,
//! connections are capped overall and per peer IP (with slots kept back for trusted peers), and an over-the-cap
//! connection gets a 503 straight from the accept thread. Enough for reporters every 15 s, a dashboard and a few
//! `/stats` readers, without an async runtime.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const MAX_BODY: usize = 8 << 20;
const MAX_HEADERS: usize = 16 << 10;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole request head must arrive within this, however the bytes are spaced.
const HEAD_BUDGET: Duration = Duration::from_secs(5);
/// Head and body together, counted from the start of the connection.
const DEADLINE: Duration = Duration::from_secs(30);
/// How long a request for a `heavy` route waits for a free permit before it is answered 503.
const HEAVY_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub head_budget: Duration,
    pub max_connections: usize,
    /// Open connections per peer IP; 0 = unlimited.
    pub per_ip: usize,
    /// Of `max_connections`, kept back for trusted peers (loopback, `trusted_networks`).
    pub reserved: usize,
    /// Requests marked `heavy` that may be read and handled at once.
    pub heavy: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { head_budget: HEAD_BUDGET, max_connections: 64, per_ip: 8, reserved: 8, heavy: 4 }
    }
}

/// What to do with a request once its head is in, before a byte of the body is read.
pub enum Admit {
    /// Read a body of at most `max` bytes (413 above). `heavy` requests take one of `Limits::heavy` permits first.
    Read { max: usize, heavy: bool },
    /// Answer this without reading the body.
    Reject(Response),
}

pub struct Policy {
    /// Peers that may use the reserved slots.
    pub trusted: Box<dyn Fn(IpAddr) -> bool + Send + Sync>,
    pub admit: Box<dyn Fn(&Request) -> Admit + Send + Sync>,
}

impl Policy {
    /// Nobody trusted, every body up to `MAX_BODY`.
    pub fn open() -> Policy {
        Policy { trusted: Box::new(|_| false), admit: Box::new(|_| Admit::Read { max: MAX_BODY, heavy: false }) }
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: IpAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    /// `?name=value` (no percent-decoding beyond what tokens and codes need).
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
    }
    pub fn bearer(&self) -> Option<&str> {
        let v = self.header("authorization")?;
        let (scheme, token) = v.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?
            .split(';')
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

/// Takes the socket of a streamed response once its head is written (see `Response::stream`).
pub type Takeover = Box<dyn FnOnce(TcpStream) + Send>;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A streamed response: no content-length, and after the head the socket goes to this instead of being closed.
    pub stream: Option<Takeover>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
            stream: None,
        }
    }
    /// A 200 whose body is written by `takeover`, which gets the socket after the head (on the connection thread, which
    /// ends, with its connection slot, when `takeover` returns).
    pub fn stream(content_type: &str, takeover: impl FnOnce(TcpStream) + Send + 'static) -> Response {
        Response { stream: Some(Box::new(takeover)), ..Response::new(200, content_type, "") }
    }
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response::new(status, "application/json", body)
    }
    pub fn error(status: u16, msg: &str) -> Response {
        Response::json(status, crate::json::J::Obj(crate::json::Obj::new().with("error", msg)).stringify())
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Response {
        self.headers.push((k.into(), v.into()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Shared {
    handler: Handler,
    policy: Policy,
    limits: Limits,
    active: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    /// Free permits for `heavy` requests.
    heavy: (Mutex<usize>, Condvar),
}

/// Owns one connection slot (and the peer's per-IP count); released on drop, also when
/// the thread never starts or unwinds.
struct Slot {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut m = lock(&self.shared.per_ip);
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
        self.shared.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    fn take_slot(self: &Arc<Self>, ip: IpAddr, trusted: bool) -> Option<Slot> {
        let mut m = lock(&self.per_ip);
        let cap =
            if trusted { self.limits.max_connections } else { self.limits.max_connections - self.limits.reserved };
        if self.active.load(Ordering::SeqCst) >= cap {
            return None;
        }
        let n = m.entry(ip).or_insert(0);
        if self.limits.per_ip > 0 && *n >= self.limits.per_ip {
            return None;
        }
        *n += 1;
        self.active.fetch_add(1, Ordering::SeqCst);
        Some(Slot { shared: self.clone(), ip })
    }
}

/// A permit for one `heavy` request, returned on drop.
struct Permit<'a>(&'a (Mutex<usize>, Condvar));

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *lock(&self.0 .0) += 1;
        self.0 .1.notify_one();
    }
}

fn heavy_permit(shared: &Shared) -> Option<Permit<'_>> {
    let (m, cv) = &shared.heavy;
    let until = Instant::now() + HEAVY_WAIT;
    let mut free = lock(m);
    while *free == 0 {
        let left = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
        free = cv.wait_timeout(free, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    *free -= 1;
    Some(Permit(&shared.heavy))
}

/// Accepts connections until the listener fails for good; then the hub serves nothing, so exit 1 (the service
/// manager restarts it).
pub fn serve(listener: TcpListener, handler: Handler, policy: Policy, limits: Limits) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| serve_loop(listener, handler, policy, limits)));
    eprintln!("hub: accept loop ended, exiting");
    std::process::exit(1)
}

fn serve_loop(listener: TcpListener, handler: Handler, policy: Policy, limits: Limits) {
    let limits = Limits { reserved: limits.reserved.min(limits.max_connections.saturating_sub(1)), ..limits };
    let shared = Arc::new(Shared {
        handler,
        policy,
        limits,
        active: AtomicUsize::new(0),
        per_ip: Mutex::new(HashMap::new()),
        heavy: (Mutex::new(limits.heavy), Condvar::new()),
    });
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("hub: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(50)); // don't spin on EMFILE and friends
                continue;
            }
        };
        let peer = stream.peer_addr().map(|a| a.ip().to_canonical()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let trusted = (shared.policy.trusted)(peer);
        let Some(slot) = shared.take_slot(peer, trusted) else {
            // Shed load, but say so; the tiny reply fits the socket buffer, the short timeout covers the rest.
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = write_response(&mut stream, &Response::error(503, "busy"), false);
            continue;
        };
        let spawned = std::thread::Builder::new().name("hub-conn".into()).spawn(move || {
            let slot = slot;
            connection(stream, peer, &slot.shared);
        });
        if let Err(e) = spawned {
            // The closure (stream and slot) was dropped with the error: connection closed, slot released.
            eprintln!("hub: cannot start a connection thread, dropping the connection: {e}");
        }
    }
}

fn connection(mut stream: TcpStream, peer: IpAddr, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let mut early = false;
    let mut res = match read_request(&stream, peer, shared) {
        Ok((req, _permit)) => {
            std::panic::catch_unwind(AssertUnwindSafe(|| (shared.handler)(req))).unwrap_or_else(|_| {
                eprintln!("hub: request handler panicked, continuing");
                Response::error(500, "internal error")
            })
        }
        Err(res) => {
            early = true;
            res
        }
    };
    if let Some(takeover) = res.stream.take() {
        if write_response(&mut stream, &res, true).is_ok()
            && std::panic::catch_unwind(AssertUnwindSafe(|| takeover(stream))).is_err()
        {
            eprintln!("hub: stream takeover panicked, continuing");
        }
        return;
    }
    let _ = write_response(&mut stream, &res, false);
    if early {
        // The body was not read. Closing with unread data resets the connection and can eat the answer, so take
        // a bounded, time-boxed look at what is still arriving first (a normal report fits).
        let _ = stream.shutdown(std::net::Shutdown::Write);
        drain(&stream, 1 << 20, Duration::from_millis(500));
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn drain(stream: &TcpStream, max: usize, within: Duration) {
    let until = Instant::now() + within;
    let mut buf = [0u8; 8192];
    let mut total = 0;
    while total < max {
        let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return };
        if stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match (&mut &*stream).read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => total += n,
        }
    }
}

enum LineErr {
    Timeout,
    TooLarge,
    Eof,
}

/// Reads one `\n`-terminated line (kept in `line`, terminator included). Every read waits at most the time left
/// until `deadline`, so a client dripping bytes cannot outlive it. `used` counts the head bytes so far.
fn read_line_limited(
    r: &mut BufReader<&TcpStream>,
    line: &mut Vec<u8>,
    used: &mut usize,
    deadline: Instant,
) -> Result<(), LineErr> {
    line.clear();
    loop {
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(LineErr::Timeout)?;
        r.get_ref().set_read_timeout(Some(left.min(READ_TIMEOUT))).map_err(|_| LineErr::Timeout)?;
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(_) => return Err(LineErr::Timeout),
        };
        if buf.is_empty() {
            return Err(LineErr::Eof);
        }
        let (n, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        *used += n;
        if *used > MAX_HEADERS {
            return Err(LineErr::TooLarge);
        }
        line.extend_from_slice(&buf[..n]);
        r.consume(n);
        if done {
            return Ok(());
        }
    }
}

fn read_request<'a>(
    stream: &TcpStream,
    peer: IpAddr,
    shared: &'a Shared,
) -> Result<(Request, Option<Permit<'a>>), Response> {
    let started = Instant::now();
    let body_deadline = started + DEADLINE;
    let head_deadline = (started + shared.limits.head_budget).min(body_deadline);
    let mut r = BufReader::new(stream);
    let (mut line, mut used) = (Vec::new(), 0);
    let mut lines: Vec<String> = Vec::new();
    loop {
        read_line_limited(&mut r, &mut line, &mut used, head_deadline).map_err(|e| match e {
            LineErr::Timeout => Response::error(408, "request timeout"),
            LineErr::TooLarge => Response::error(431, "headers too large"),
            LineErr::Eof => Response::error(400, "incomplete request"),
        })?;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            if lines.is_empty() {
                continue; // tolerate a stray CRLF before the request line
            }
            break;
        }
        lines.push(text.to_string());
    }
    let mut first = lines[0].split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("/").to_string());
    let mut headers = Vec::new();
    for l in &lines[1..] {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut req = Request { method, path, query, headers, body: Vec::new(), peer };
    if req.header("transfer-encoding").is_some() {
        return Err(Response::error(411, "send a Content-Length"));
    }
    let len = match req.header("content-length") {
        None => 0,
        Some(v) => v.parse::<usize>().map_err(|_| Response::error(400, "bad content-length"))?,
    };
    // Auth and the body cap come first: nothing of an unauthorized or oversized body is read.
    let (max, heavy) = match (shared.policy.admit)(&req) {
        Admit::Reject(res) => return Err(res),
        Admit::Read { max, heavy } => (max, heavy),
    };
    if len > max {
        return Err(Response::error(413, "too large"));
    }
    let permit = if heavy { Some(heavy_permit(shared).ok_or_else(|| Response::error(503, "busy"))?) } else { None };
    if len > 0 {
        if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            let _ = (&mut &*stream).write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let mut body = Vec::with_capacity(len.min(1 << 20));
        while body.len() < len {
            let left = body_deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| Response::error(408, "request timeout"))?;
            r.get_ref()
                .set_read_timeout(Some(left.min(READ_TIMEOUT)))
                .map_err(|_| Response::error(408, "request timeout"))?;
            let buf = r.fill_buf().map_err(|_| Response::error(408, "request timeout"))?;
            if buf.is_empty() {
                return Err(Response::error(400, "incomplete body"));
            }
            let n = buf.len().min(len - body.len());
            body.extend_from_slice(&buf[..n]);
            r.consume(n);
        }
        req.body = body;
    }
    Ok((req, permit))
}

/// The whole response, or with `streamed` only the head (no content-length: the body runs until the connection closes).
fn write_response(stream: &mut TcpStream, res: &Response, streamed: bool) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", res.status, reason(res.status));
    for (k, v) in &res.headers {
        if !v.contains(['\r', '\n']) {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    if !streamed {
        out.push_str(&format!("content-length: {}\r\n", res.body.len()));
    }
    out.push_str("connection: close\r\n\r\n");
    stream.write_all(out.as_bytes())?;
    stream.write_all(&res.body)?;
    stream.flush()
}

/// Binds `addr` (host:port); port 0 picks a free one.
pub fn bind(addr: &str) -> io::Result<(TcpListener, SocketAddr)> {
    let l = TcpListener::bind(addr)?;
    let a = l.local_addr()?;
    Ok((l, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn ok_handler() -> Handler {
        Arc::new(|req| {
            if req.path == "/boom" {
                panic!("injected");
            }
            Response::json(200, format!("{{\"len\":{}}}", req.body.len()))
        })
    }

    fn start(policy: Policy, limits: Limits) -> SocketAddr {
        let (listener, addr) = bind("127.0.0.1:0").unwrap();
        let handler = ok_handler();
        std::thread::spawn(move || serve_loop(listener, handler, policy, limits));
        addr
    }

    #[test]
    fn handler_panic_answers_500_and_the_server_keeps_serving() {
        let addr = start(Policy::open(), Limits::default());
        for _ in 0..(Limits::default().max_connections + 5) {
            let r = get(addr, "/boom");
            assert!(r.starts_with("HTTP/1.1 500"), "{r}");
            assert!(r.contains("\"error\""), "{r}");
        }
        // more panics than connection slots, so a leaked slot would answer 503 "busy" here
        let r = get(addr, "/fine");
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("{\"len\":0}"), "{r}");
    }

    #[test]
    fn header_drip_is_cut_off_at_the_head_budget() {
        let limits = Limits { head_budget: Duration::from_secs(2), ..Limits::default() };
        let addr = start(Policy::open(), limits);
        let mut s = TcpStream::connect(addr).unwrap();
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
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 408"), "{}", String::from_utf8_lossy(&out));
    }

    /// ::1 is "trusted" here; 127.0.0.1 is an ordinary peer (the listener is dual-stack).
    fn trust_v6_loopback() -> Policy {
        Policy { trusted: Box::new(|ip| ip == IpAddr::from(std::net::Ipv6Addr::LOCALHOST)), ..Policy::open() }
    }

    #[test]
    fn per_ip_cap_answers_503_while_a_trusted_peer_still_gets_in() {
        let limits = Limits { per_ip: 3, max_connections: 8, reserved: 2, ..Limits::default() };
        let (listener, addr) = bind("[::]:0").unwrap();
        let handler = ok_handler();
        std::thread::spawn(move || serve_loop(listener, handler, trust_v6_loopback(), limits));
        let other = SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let held: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(other).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(200));
        let t0 = Instant::now();
        assert!(get(other, "/healthz").starts_with("HTTP/1.1 503"));
        assert!(t0.elapsed() < Duration::from_secs(1));
        let trusted = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port()));
        assert!(get(trusted, "/healthz").starts_with("HTTP/1.1 200"));
        drop(held);
        std::thread::sleep(Duration::from_millis(300));
        assert!(get(other, "/healthz").starts_with("HTTP/1.1 200"), "slots come back");
    }

    #[test]
    fn untrusted_peers_leave_the_reserved_slots_free() {
        let limits = Limits { per_ip: 0, max_connections: 4, reserved: 2, ..Limits::default() };
        let (listener, addr) = bind("[::]:0").unwrap();
        let handler = ok_handler();
        std::thread::spawn(move || serve_loop(listener, handler, trust_v6_loopback(), limits));
        let other = SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let held: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(other).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(200));
        assert!(get(other, "/healthz").starts_with("HTTP/1.1 503"));
        assert!(
            get(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port())), "/healthz").starts_with("HTTP/1.1 200")
        );
        drop(held);
    }

    fn post(addr: SocketAddr, path: &str, extra: &str, body_len: usize) -> (String, Duration) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let t0 = Instant::now();
        s.write_all(format!("POST {path} HTTP/1.1\r\nHost: x\r\n{extra}Content-Length: {body_len}\r\n\r\n").as_bytes())
            .unwrap();
        let chunk = vec![b' '; 64 << 10];
        let mut sent = 0;
        while sent < body_len {
            let n = chunk.len().min(body_len - sent);
            if s.write_all(&chunk[..n]).is_err() {
                break; // the answer came early and the server stopped reading
            }
            sent += n;
        }
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        (out, t0.elapsed())
    }

    fn caps() -> Policy {
        Policy {
            admit: Box::new(|req| match req.path.as_str() {
                "/pair" => Admit::Read { max: 4096, heavy: false },
                "/report" if req.header("authorization").is_none() => {
                    Admit::Reject(Response::error(401, "unauthorized"))
                }
                "/report" => Admit::Read { max: MAX_BODY, heavy: true },
                _ => Admit::Read { max: 64 << 10, heavy: false },
            }),
            ..Policy::open()
        }
    }

    #[test]
    fn body_caps_and_auth_before_body() {
        let addr = start(caps(), Limits::default());
        let (r, t) = post(addr, "/pair", "", 5 << 10);
        assert!(r.starts_with("HTTP/1.1 413"), "{r}");
        assert!(t < Duration::from_secs(1), "{t:?}");
        let (r, _) = post(addr, "/pair", "", 4 << 10);
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let (r, _) = post(addr, "/other", "", 65 << 10);
        assert!(r.starts_with("HTTP/1.1 413"), "{r}");
        // unauthenticated 8 MiB report: 401 from the head alone (a server that tried to read the body would wait here)
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let t0 = Instant::now();
        s.write_all(format!("POST /report HTTP/1.1\r\nHost: x\r\nContent-Length: {MAX_BODY}\r\n\r\n").as_bytes())
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 401"), "{out}");
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        // and when the client does send it all, the answer still gets through
        let (r, t) = post(addr, "/report", "", MAX_BODY);
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        assert!(t < Duration::from_secs(2), "{t:?}");
        let (r, _) = post(addr, "/report", "Authorization: Bearer x\r\n", 1 << 20);
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("{\"len\":1048576}"), "{r}");
    }

    #[test]
    fn heavy_requests_wait_for_a_permit_then_get_503() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let (listener, addr) = bind("127.0.0.1:0").unwrap();
        let handler: Handler = Arc::new(move |_| {
            let _ = lock(&rx).recv_timeout(Duration::from_secs(10));
            Response::json(200, "{}")
        });
        let limits = Limits { heavy: 1, ..Limits::default() };
        std::thread::spawn(move || serve_loop(listener, handler, caps(), limits));
        let first = std::thread::spawn(move || post(addr, "/report", "Authorization: Bearer x\r\n", 10).0);
        std::thread::sleep(Duration::from_millis(300));
        let (r, t) = post(addr, "/report", "Authorization: Bearer x\r\n", 10);
        assert!(r.starts_with("HTTP/1.1 503"), "{r}");
        assert!(t >= HEAVY_WAIT && t < HEAVY_WAIT + Duration::from_secs(2), "{t:?}");
        tx.send(()).unwrap();
        assert!(first.join().unwrap().starts_with("HTTP/1.1 200"));
    }
}
