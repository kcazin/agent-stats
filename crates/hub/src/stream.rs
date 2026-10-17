//! Server-Sent Event streams (`/api/stream`, `/stream`). Once a stream's response head is written, its socket leaves
//! the connection thread (which ends and frees its connection slot) for one broadcaster thread that owns every stream.
//!
//! The sockets are non-blocking and each has its own output buffer: an event is appended to every buffer of its feed
//! and the thread writes what each socket takes, waiting in `poll(2)`. A client that stops reading costs a buffer, never
//! a blocked write, so it can't hold up the others; it is dropped when its backlog passes 256 KiB or when nothing of it
//! could be written for 5 s, and its reconnect starts from a fresh full document. (Blocking writes with a short timeout
//! would be simpler, but one stuck client would then delay every event to every other client by that timeout.)
//! Streams have their own cap, apart from the connection slots, and a `: ping` comment goes to all of them every 15 s.
//! The body is chunked (one chunk per event) and never terminated: the stream ends when the connection closes.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A client is dropped when this much is waiting for it...
pub const MAX_BACKLOG: usize = 256 << 10;
/// ...or when none of its backlog could be written for this long.
pub const STALL: Duration = Duration::from_secs(5);
pub const PING: Duration = Duration::from_secs(15);
/// A client sends nothing after its request; one that keeps sending this much is dropped.
const MAX_INBOUND: usize = 64 << 10;
/// The kernel's send buffer per stream, so a stalled client shows up in its backlog instead of in kernel memory.
const SNDBUF: libc::c_int = 64 << 10;

#[derive(Clone, Copy, Debug)]
pub struct StreamLimits {
    /// Open streams in all.
    pub max: usize,
    /// Of `max`, kept back for privileged clients (admin, loopback, `trusted_networks`).
    pub reserved: usize,
    /// Streams per peer IP for everyone else; 0 = unlimited.
    pub per_ip: usize,
}

impl Default for StreamLimits {
    fn default() -> StreamLimits {
        StreamLimits { max: 32, reserved: 8, per_ip: 4 }
    }
}

/// Which events a stream gets: the private dashboard feed (`/api/stream`) or the public one (`/stream`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    Private,
    Public,
}

/// One SSE event: `event: <name>` and the data, one `data:` line per line.
pub fn event(name: &str, data: &str) -> Vec<u8> {
    let mut out = format!("event: {name}\n");
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line.trim_end_matches('\r'));
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Counts {
    open: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// One stream's place under the cap (and its peer's per-IP count, if it counts there); given back on drop.
pub struct Ticket {
    counts: Arc<Mutex<Counts>>,
    ip: Option<IpAddr>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut c = lock(&self.counts);
        c.open -= 1;
        if let Some(ip) = self.ip {
            if let Some(n) = c.per_ip.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    c.per_ip.remove(&ip);
                }
            }
        }
    }
}

struct Client {
    sock: TcpStream,
    feed: Feed,
    buf: Vec<u8>,
    /// Bytes of `buf` already written.
    off: usize,
    /// Since when nothing of the backlog could be written.
    stuck: Option<Instant>,
    inbound: usize,
    dead: bool,
    _ticket: Ticket,
}

impl Client {
    fn backlog(&self) -> usize {
        self.buf.len() - self.off
    }
    /// Queues one chunk. A client still more than MAX_BACKLOG behind when the next one comes is dropped (so one event
    /// bigger than that still goes out whole to a client that keeps up).
    fn push(&mut self, bytes: &[u8]) {
        if self.backlog() > MAX_BACKLOG {
            self.dead = true;
            return;
        }
        self.append(bytes);
    }
    fn append(&mut self, bytes: &[u8]) {
        if self.off > 0 && self.off == self.buf.len() {
            self.buf.clear();
            self.off = 0;
        }
        self.buf.extend_from_slice(format!("{:x}\r\n", bytes.len()).as_bytes());
        self.buf.extend_from_slice(bytes);
        self.buf.extend_from_slice(b"\r\n");
    }
    /// Writes what the socket takes.
    fn flush(&mut self, now: Instant) {
        let before = self.off;
        while self.off < self.buf.len() {
            match (&self.sock).write(&self.buf[self.off..]) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        if self.off == self.buf.len() {
            self.buf.clear();
            self.off = 0;
            self.stuck = None;
        } else if self.off > before {
            self.stuck = Some(now);
        } else if now.duration_since(*self.stuck.get_or_insert(now)) > STALL {
            self.dead = true;
        }
    }
    /// Reads and discards whatever the client sends; end of stream or an error means it left.
    fn drain(&mut self) {
        let mut scratch = [0u8; 4096];
        loop {
            match (&self.sock).read(&mut scratch) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.inbound += n;
                    if self.inbound > MAX_INBOUND {
                        self.dead = true;
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

pub struct Broadcaster {
    limits: StreamLimits,
    counts: Arc<Mutex<Counts>>,
    /// Only the broadcaster thread removes clients, so it can poll a snapshot of the list and find them again by index.
    clients: Mutex<Vec<Client>>,
    /// A byte here wakes the thread (new client, new event).
    wake: UnixStream,
}

impl Broadcaster {
    /// Starts the broadcaster thread.
    pub fn start(limits: StreamLimits) -> io::Result<Arc<Broadcaster>> {
        let (wake, wakeup) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wakeup.set_nonblocking(true)?;
        let b = Arc::new(Broadcaster {
            limits: StreamLimits { reserved: limits.reserved.min(limits.max), ..limits },
            counts: Default::default(),
            clients: Mutex::new(Vec::new()),
            wake,
        });
        let me = b.clone();
        std::thread::Builder::new().name("hub-stream".into()).spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.run(wakeup)));
            // Without this thread no stream gets anything: drop them all, so they reconnect (and get 503s) rather than
            // hang. New streams are refused from now on.
            eprintln!("hub: stream broadcaster stopped; streams are off");
            lock(&me.counts).open = usize::MAX / 2;
            lock(&me.clients).clear();
        })?;
        Ok(b)
    }

    /// A place for one more stream, or None when the cap is reached. `privileged` clients may use the reserved places
    /// and have no per-IP cap.
    pub fn ticket(&self, ip: IpAddr, privileged: bool) -> Option<Ticket> {
        let mut c = lock(&self.counts);
        let cap = if privileged { self.limits.max } else { self.limits.max - self.limits.reserved };
        if c.open >= cap {
            return None;
        }
        let ip = (!privileged && self.limits.per_ip > 0).then_some(ip);
        if let Some(ip) = ip {
            let n = c.per_ip.entry(ip).or_insert(0);
            if *n >= self.limits.per_ip {
                return None;
            }
            *n += 1;
        }
        c.open += 1;
        Some(Ticket { counts: self.counts.clone(), ip })
    }

    /// Hands a stream (response head already written) to the broadcaster; `first` (one chunk each) is what it gets
    /// before any later event. Callers that must not miss or reorder events hold their feed's lock across building
    /// `first` and this.
    pub fn join(&self, sock: TcpStream, feed: Feed, ticket: Ticket, first: &[Vec<u8>]) {
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        // SAFETY: setsockopt on an open socket, with a c_int option value of the size passed.
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&SNDBUF as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let mut c =
            Client { sock, feed, buf: Vec::new(), off: 0, stuck: None, inbound: 0, dead: false, _ticket: ticket };
        for chunk in first {
            c.append(chunk); // however big: a client is behind only once it hasn't taken them
        }
        lock(&self.clients).push(c);
        self.wake();
    }

    /// Queues `bytes` for every stream of `feed`.
    pub fn send(&self, feed: Feed, bytes: &[u8]) {
        let mut any = false;
        for c in lock(&self.clients).iter_mut().filter(|c| c.feed == feed && !c.dead) {
            c.push(bytes);
            any = true;
        }
        if any {
            self.wake();
        }
    }

    /// Open streams of `feed` (so nobody computes events for no one).
    pub fn count(&self, feed: Feed) -> usize {
        lock(&self.clients).iter().filter(|c| c.feed == feed && !c.dead).count()
    }

    fn wake(&self) {
        let _ = (&self.wake).write(&[1]); // a full pipe already means "wake up"
    }

    fn run(&self, wakeup: UnixStream) {
        let mut next_ping = Instant::now() + PING;
        let mut fds: Vec<libc::pollfd> = Vec::new();
        loop {
            let now = Instant::now();
            let pending = {
                let mut clients = lock(&self.clients);
                if now >= next_ping {
                    for c in clients.iter_mut() {
                        c.push(b": ping\n\n");
                    }
                    next_ping = now + PING;
                }
                for c in clients.iter_mut().filter(|c| !c.dead && c.backlog() > 0) {
                    c.flush(now);
                }
                clients.retain(|c| !c.dead);
                fds.clear();
                fds.push(libc::pollfd { fd: wakeup.as_raw_fd(), events: libc::POLLIN, revents: 0 });
                for c in clients.iter() {
                    let out = if c.backlog() > 0 { libc::POLLOUT } else { 0 };
                    fds.push(libc::pollfd { fd: c.sock.as_raw_fd(), events: libc::POLLIN | out, revents: 0 });
                }
                clients.iter().any(|c| c.backlog() > 0)
            };
            // With a backlog, wake up at least every second to notice a stall.
            let mut wait = next_ping.saturating_duration_since(Instant::now());
            if pending {
                wait = wait.min(Duration::from_secs(1));
            }
            let ms = wait.as_millis().clamp(1, 60_000) as libc::c_int;
            // SAFETY: `fds` is a live, correctly sized array of pollfd; the fds stay open until the clients are removed,
            // which only this thread does, after poll returns.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            if r < 0 {
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    std::thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
            if fds[0].revents != 0 {
                let mut scratch = [0u8; 64];
                while matches!((&wakeup).read(&mut scratch), Ok(n) if n > 0) {}
            }
            let mut clients = lock(&self.clients);
            for (c, p) in clients.iter_mut().zip(&fds[1..]) {
                if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                    c.dead = true;
                } else if p.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    c.drain();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_framed_line_by_line() {
        assert_eq!(event("live", r#"{"a":1}"#), b"event: live\ndata: {\"a\":1}\n\n");
        assert_eq!(event("x", "a\nb\r\nc"), b"event: x\ndata: a\ndata: b\ndata: c\n\n");
    }

    #[test]
    fn tickets_cap_streams_per_ip_and_keep_the_reserve() {
        let b = Broadcaster::start(StreamLimits { max: 6, reserved: 2, per_ip: 2 }).unwrap();
        let (x, y, z) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let mut held = vec![b.ticket(x, false).unwrap(), b.ticket(x, false).unwrap()];
        assert!(b.ticket(x, false).is_none(), "per IP");
        held.push(b.ticket(y, false).unwrap());
        held.push(b.ticket(y, false).unwrap());
        assert!(b.ticket(z, false).is_none(), "the last two are reserved");
        held.push(b.ticket(x, true).unwrap());
        held.push(b.ticket(x, true).unwrap());
        assert!(b.ticket(x, true).is_none(), "full");
        held.truncate(1);
        assert!(b.ticket(x, false).is_some(), "given back on drop");
    }
}
