//! `GET /stream`: Server-Sent Events of the public snapshot.
//!
//! The connection thread writes the response head and hands the socket to the one broadcaster thread here, which
//! releases its connection slot. The broadcaster owns every stream as a non-blocking socket with its own queue of
//! shared, already framed event bytes: an event is formatted once and every stream's queue points at the same
//! `Arc<[u8]>`. A stream that cannot keep up (backlog over `max_backlog` when a new event is queued, or no byte written
//! for `stall`) is dropped; it reconnects and starts again from a full `stats`.
//!
//! The body is HTTP/1.1 chunked (no content-length), so proxies pass each event through as it is written.

use crate::patch::diff;
use crate::store::{Store, Version};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const MAX_STREAMS: usize = 1000;
pub const PING: Duration = Duration::from_secs(15);
pub const MAX_BACKLOG: usize = 256 * 1024;
pub const STALL: Duration = Duration::from_secs(5);
/// How often the stale flag is re-evaluated and closed streams are reaped.
const SWEEP: Duration = Duration::from_secs(1);
/// Poll interval while some stream has unsent bytes.
const BUSY_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy)]
pub struct StreamLimits {
    pub max_streams: usize,
    pub ping: Duration,
    pub max_backlog: usize,
    pub stall: Duration,
}

impl Default for StreamLimits {
    fn default() -> Self {
        StreamLimits { max_streams: MAX_STREAMS, ping: PING, max_backlog: MAX_BACKLOG, stall: STALL }
    }
}

type Bytes = Arc<[u8]>;

enum Msg {
    Join(TcpStream, Ticket),
    /// A publish was stored: look at the store now rather than at the next sweep.
    Changed,
}

/// One reserved stream slot; released on drop (stream closed, or never handed over).
pub struct Ticket(Arc<AtomicUsize>);

impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The handle the HTTP side holds.
pub struct Streams {
    tx: Sender<Msg>,
    open: Arc<AtomicUsize>,
    max: usize,
}

impl Streams {
    pub fn start(store: Arc<Store>, limits: StreamLimits) -> Result<Streams, String> {
        let (tx, rx) = channel();
        let open = Arc::new(AtomicUsize::new(0));
        let mut b = Broadcaster {
            store,
            limits,
            clients: Vec::new(),
            version: None,
            doc: None,
            stats: None,
            retry: frame(b"retry: 3000\n\n"),
            ping: frame(b": ping\n\n"),
        };
        std::thread::Builder::new()
            .name("relay-stream".into())
            .spawn(move || loop {
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.run(&rx)));
                match ran {
                    Ok(()) => return, // every sender is gone
                    Err(_) => {
                        // Drop every stream (they reconnect) and keep serving new ones.
                        eprintln!("relay: stream broadcaster panicked, dropping its streams");
                        b.clients.clear();
                        (b.version, b.doc, b.stats) = (None, None, None);
                    }
                }
            })
            .map_err(|e| format!("cannot start the stream thread: {e}"))?;
        Ok(Streams { tx, open, max: limits.max_streams })
    }

    /// Reserves a stream slot, `None` when `max_streams` are open.
    pub fn reserve(&self) -> Option<Ticket> {
        if self.open.fetch_add(1, Ordering::SeqCst) >= self.max {
            self.open.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Ticket(self.open.clone()))
    }

    /// Hands a socket whose response head is written over to the broadcaster.
    pub fn hand_over(&self, sock: TcpStream, ticket: Ticket) {
        self.send(Msg::Join(sock, ticket));
    }

    pub fn changed(&self) {
        self.send(Msg::Changed);
    }

    fn send(&self, m: Msg) {
        // A failed send drops the message: the socket closes and the ticket is released.
        let _ = self.tx.send(m);
    }
}

/// One HTTP chunk.
fn frame(payload: &[u8]) -> Bytes {
    let mut out = format!("{:x}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out.into()
}

/// One SSE event, framed as a chunk. A multi-line `data` becomes one `data:` line per line (clients join them with `\n`).
fn event(name: &str, data: &str) -> Bytes {
    let mut s = format!("event: {name}\n");
    for line in data.split('\n') {
        s.push_str("data: ");
        s.push_str(line.strip_suffix('\r').unwrap_or(line));
        s.push('\n');
    }
    s.push('\n');
    frame(s.as_bytes())
}

struct Client {
    sock: TcpStream,
    queue: VecDeque<(Bytes, usize)>,
    backlog: usize,
    /// Set while bytes are queued: when the queue became non-empty or last made progress.
    waiting_since: Option<Instant>,
    /// The version whose document this stream was last sent (`None`: nothing yet).
    sent: Option<Version>,
    dead: bool,
    _ticket: Ticket,
}

impl Client {
    fn push(&mut self, ev: &Bytes, now: Instant, max_backlog: usize) {
        if self.backlog > max_backlog {
            self.dead = true;
        }
        if self.dead {
            return;
        }
        self.queue.push_back((ev.clone(), 0));
        self.backlog += ev.len();
        self.waiting_since.get_or_insert(now);
    }

    fn flush(&mut self, now: Instant, stall: Duration) {
        while let Some((buf, off)) = self.queue.front_mut() {
            match self.sock.write(&buf[*off..]) {
                Ok(0) => self.dead = true,
                Ok(n) => {
                    *off += n;
                    self.backlog -= n;
                    self.waiting_since = Some(now);
                    if *off == buf.len() {
                        self.queue.pop_front();
                    }
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.dead = true,
            }
            break;
        }
        if self.queue.is_empty() {
            self.waiting_since = None;
        } else if self.waiting_since.is_some_and(|t| now.duration_since(t) > stall) {
            self.dead = true;
        }
    }

    /// A client never sends anything after its request: EOF or an error means it is gone. Stray bytes are discarded.
    fn check_closed(&mut self) {
        let mut buf = [0u8; 1024];
        for _ in 0..16 {
            match self.sock.read(&mut buf) {
                Ok(0) => self.dead = true,
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.dead = true,
            }
            return;
        }
    }
}

struct Broadcaster {
    store: Arc<Store>,
    limits: StreamLimits,
    clients: Vec<Client>,
    /// The version last broadcast, its parsed document, and its `stats` event.
    version: Option<Version>,
    doc: Option<Value>,
    stats: Option<Bytes>,
    retry: Bytes,
    ping: Bytes,
}

impl Broadcaster {
    fn run(&mut self, rx: &std::sync::mpsc::Receiver<Msg>) {
        let start = Instant::now();
        let (mut next_ping, mut next_sweep) = (start + self.limits.ping, start);
        loop {
            let now = Instant::now();
            let mut wait = next_ping.min(next_sweep).saturating_duration_since(now);
            if self.clients.iter().any(|c| c.backlog > 0) {
                wait = wait.min(BUSY_POLL);
            }
            let (mut changed, mut joins) = (false, Vec::new());
            let mut take = |m: Msg| match m {
                Msg::Join(sock, ticket) => joins.push((sock, ticket)),
                Msg::Changed => changed = true,
            };
            match rx.recv_timeout(wait) {
                Ok(m) => take(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(m) = rx.try_recv() {
                take(m);
            }
            let now = Instant::now();
            let sweep = now >= next_sweep;
            if changed || sweep || !joins.is_empty() {
                self.refresh(now);
            }
            for (sock, ticket) in joins {
                self.join(sock, ticket, now);
            }
            if sweep {
                self.clients.iter_mut().for_each(Client::check_closed);
                next_sweep = now + SWEEP;
            }
            if now >= next_ping {
                let ping = self.ping.clone();
                self.push_all(&ping, now);
                next_ping = now + self.limits.ping;
            }
            for c in &mut self.clients {
                c.flush(now, self.limits.stall);
            }
            self.clients.retain(|c| !c.dead); // dropping a client closes its socket and frees its slot
        }
    }

    fn push_all(&mut self, ev: &Bytes, now: Instant) {
        for c in &mut self.clients {
            c.push(ev, now, self.limits.max_backlog);
        }
    }

    fn join(&mut self, sock: TcpStream, ticket: Ticket, now: Instant) {
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        let _ = sock.set_nodelay(true);
        let mut c = Client {
            sock,
            queue: VecDeque::new(),
            backlog: 0,
            waiting_since: None,
            sent: None,
            dead: false,
            _ticket: ticket,
        };
        c.push(&self.retry, now, self.limits.max_backlog);
        if let Some(stats) = &self.stats {
            c.push(stats, now, self.limits.max_backlog);
            c.sent = self.version;
        }
        self.clients.push(c);
    }

    /// Picks up a new document or a stale flip: one patch for every stream that has the previous document, a full
    /// `stats` for the others (and for everyone when the change is not expressible as a merge patch).
    fn refresh(&mut self, now: Instant) {
        let Some(version) = self.store.version() else { return };
        if self.version == Some(version) {
            return;
        }
        let Some((version, body)) = self.store.versioned_snapshot() else { return };
        let new: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let stats = event("stats", &body);
        let patch = match &self.doc {
            Some(old) if !new.is_null() => diff(old, &new),
            _ => None,
        };
        let patch = patch.map(|p| match p.as_object().is_some_and(|o| o.is_empty()) {
            true => None,
            false => Some(event("patch", &p.to_string())),
        });
        let prev = self.version;
        for c in &mut self.clients {
            let ev = match &patch {
                _ if c.sent.is_none() || c.sent != prev => Some(&stats),
                Some(p) => p.as_ref(),
                None => Some(&stats),
            };
            if let Some(ev) = ev {
                c.push(ev, now, self.limits.max_backlog);
            }
            c.sent = Some(version);
        }
        self.version = Some(version);
        self.doc = Some(new);
        self.stats = Some(stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        assert_eq!(&*frame(b": ping\n\n"), b"8\r\n: ping\n\n\r\n");
        assert_eq!(&*event("stats", "{\"a\":1}"), b"1c\r\nevent: stats\ndata: {\"a\":1}\n\n\r\n");
        let multi = event("stats", "{\r\n\"a\":1\n}");
        assert!(std::str::from_utf8(&multi).unwrap().contains("data: {\ndata: \"a\":1\ndata: }\n\n"));
    }
}
