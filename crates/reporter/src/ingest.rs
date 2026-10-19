//! Reading what was appended to a transcript since the last scan, whole lines only, with no line-length limit.
//!
//! Lines are streamed through a fixed buffer. Each line is classified by substring before any JSON parsing:
//! lines that can count tokens (or set the model/cwd a later count uses) are parsed at once. Lines that can only
//! change what the session shows (its state, and for Claude its cwd) are just remembered by offset: only the
//! last one that sets the state matters, so at the end of the read the remembered lines are parsed newest first
//! until one does, which is usually the first. A counted line that sets the state makes the earlier ones moot,
//! which keeps the list short; if it still grows long it is settled early the same way.
//! A line longer than the buffer is scanned for the same substrings in a streaming pass and, if it has to be
//! parsed, parsed straight from the file.

use crate::js::fnv64;
use crate::lines::{CLine, GLine, Sink, XLine};
use crate::state::{Agent, Contrib, FileState};
use jiff::tz::TimeZone;
use memchr::memmem::Finder;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// The read buffer; lines longer than this are streamed.
const CHUNK: usize = 1 << 20;
/// A line up to this long is read into memory to be parsed; longer ones are parsed from the file.
const MAX_INMEM: usize = 16 << 20;
/// Remembered lines are settled early past this many.
const MAX_PENDING: usize = 1024;
/// The file's first bytes are hashed to notice a rewrite.
const HEAD: u64 = 4096;

pub struct Env<'e> {
    pub home: &'e str,
    pub git: Option<&'e crate::git::GitIds>,
    pub tz: &'e TimeZone,
    pub local: &'e TimeZone,
    pub now: f64,
}

#[cfg(unix)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    loop {
        match f.read_at(buf, off) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

#[cfg(windows)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, off)
}

fn pread_full(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = pread(f, &mut buf[n..], off + n as u64)?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

struct RangeReader<'f> {
    file: &'f File,
    pos: u64,
    end: u64,
}

impl Read for RangeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min((self.end - self.pos) as usize);
        if n == 0 {
            return Ok(0);
        }
        let k = pread(self.file, &mut buf[..n], self.pos)?;
        self.pos += k as u64;
        Ok(k)
    }
}

/// Substrings that classify a line, per agent.
struct Needles {
    finders: Vec<Finder<'static>>,
    max_len: usize,
}

impl Needles {
    fn new(list: &[&'static [u8]]) -> Self {
        Needles {
            finders: list.iter().map(|n| Finder::new(*n)).collect(),
            max_len: list.iter().map(|n| n.len()).max().unwrap_or(1),
        }
    }
    fn flags(&self, hay: &[u8]) -> u32 {
        let mut f = 0;
        for (i, n) in self.finders.iter().enumerate() {
            if n.find(hay).is_some() {
                f |= 1 << i;
            }
        }
        f
    }
}

// Claude
const C_USAGE: u32 = 1;
const C_CWD: u32 = 2;
const C_ASSISTANT: u32 = 4;
const C_TOOL_RESULT: u32 = 8;
// Codex
const X_MUST: u32 = 1 | 2 | 4;
const X_RESPONSE_ITEM: u32 = 8;
// Grok
const G_TURN: u32 = 1;
const G_UPDATE: u32 = 2;

fn needles(agent: Agent) -> Needles {
    match agent {
        Agent::Claude => Needles::new(&[b"\"usage\"", b"\"cwd\"", b"\"assistant\"", b"\"tool_result\""]),
        Agent::Codex => Needles::new(&[b"token_count", b"turn_context", b"session_meta", b"response_item"]),
        Agent::Grok => Needles::new(&[b"turn_completed", b"sessionUpdate"]),
    }
}

/// One line: its bytes when they fit in the buffer, else its position and precomputed substring flags.
struct LineRef<'b> {
    off: u64,
    len: u64,
    data: Option<&'b [u8]>,
    flags: u32,
}

/// Calls `cb` for every complete line in `[start, end)` and returns the offset just past the last newline.
fn for_each_line(file: &File, start: u64, end: u64, needles: &Needles, mut cb: impl FnMut(LineRef)) -> io::Result<u64> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.with(|b| {
        let mut buf = b.borrow_mut();
        // Small reads (the usual tick) get a small buffer; the first one big enough is kept for the thread.
        let want = CHUNK.min((end - start) as usize + 1).max(4096);
        if buf.len() < want {
            buf.resize(want, 0);
        }
        lines_in(file, start, end, needles, &mut buf[..want], &mut cb)
    })
}

fn lines_in(
    file: &File,
    start: u64,
    end: u64,
    needles: &Needles,
    buf: &mut [u8],
    cb: &mut impl FnMut(LineRef),
) -> io::Result<u64> {
    let mut pos = start; // file offset of buf[0]
    let mut have = 0usize;
    loop {
        if have == buf.len() {
            match huge_line(file, pos, have, end, buf, needles)? {
                Some((len, flags)) => {
                    cb(LineRef { off: pos, len, data: None, flags });
                    pos += len + 1;
                    have = 0;
                    continue;
                }
                None => return Ok(pos),
            }
        }
        let want = ((buf.len() - have) as u64).min(end.saturating_sub(pos + have as u64)) as usize;
        if want == 0 {
            return Ok(pos);
        }
        let n = pread(file, &mut buf[have..have + want], pos + have as u64)?;
        if n == 0 {
            return Ok(pos); // the file shrank under us; the rest waits for the next scan
        }
        have += n;
        let mut s = 0;
        while let Some(i) = memchr::memchr(b'\n', &buf[s..have]) {
            if i > 0 {
                cb(LineRef { off: pos + s as u64, len: i as u64, data: Some(&buf[s..s + i]), flags: 0 });
            }
            s += i + 1;
        }
        if s > 0 {
            buf.copy_within(s..have, 0);
            pos += s as u64;
            have -= s;
        }
    }
}

/// A line longer than the buffer, starting at `start` with `have` bytes already in `buf`: finds its end and its
/// substring flags without holding it. `None` when it is not finished yet.
fn huge_line(
    file: &File,
    start: u64,
    have: usize,
    end: u64,
    buf: &mut [u8],
    needles: &Needles,
) -> io::Result<Option<(u64, u32)>> {
    let keep = needles.max_len - 1;
    let mut flags = needles.flags(&buf[..have]);
    let mut tail: Vec<u8> = buf[have.saturating_sub(keep)..have].to_vec();
    let mut cur = start + have as u64;
    loop {
        let want = (buf.len() as u64).min(end.saturating_sub(cur)) as usize;
        if want == 0 {
            return Ok(None);
        }
        let n = pread(file, &mut buf[..want], cur)?;
        if n == 0 {
            return Ok(None);
        }
        let nl = memchr::memchr(b'\n', &buf[..n]);
        let seg = &buf[..nl.unwrap_or(n)];
        let mut joint = tail.clone();
        joint.extend_from_slice(&seg[..seg.len().min(keep)]);
        flags |= needles.flags(&joint) | needles.flags(seg);
        if let Some(i) = nl {
            return Ok(Some((cur - start + i as u64, flags)));
        }
        if seg.len() >= keep {
            tail.clear();
            tail.extend_from_slice(&seg[seg.len() - keep..]);
        } else {
            tail.extend_from_slice(seg);
            let cut = tail.len().saturating_sub(keep);
            tail.drain(..cut);
        }
        cur += n as u64;
    }
}

/// Parses one line with `$ty` and evaluates `$body` with it bound to `$r`. Bytes that are not valid UTF-8 are
/// decoded lossily first, as Node's `toString('utf8')` would. Lines that are not JSON are skipped.
macro_rules! with_parsed {
    ($bytes:expr, $ty:ident, |$r:ident| $body:expr) => {{
        let bytes: &[u8] = $bytes;
        match serde_json::from_slice::<$ty>(bytes) {
            Ok($r) => Some($body),
            Err(_) if std::str::from_utf8(bytes).is_err() => {
                let s = String::from_utf8_lossy(bytes).into_owned();
                match serde_json::from_str::<$ty>(&s) {
                    Ok($r) => Some($body),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }};
}

/// Parses the line at `[off, off+len)` straight from the file.
macro_rules! with_parsed_at {
    ($file:expr, $off:expr, $len:expr, $ty:ident, |$r:ident| $body:expr) => {{
        let (file, off, len): (&File, u64, u64) = ($file, $off, $len);
        if len as usize <= MAX_INMEM {
            let mut v = vec![0u8; len as usize];
            match pread_full(file, &mut v, off) {
                Ok(n) if n == v.len() => with_parsed!(&v, $ty, |$r| $body),
                _ => None,
            }
        } else {
            let rd = BufReader::with_capacity(64 << 10, RangeReader { file, pos: off, end: off + len });
            let mut de = serde_json::Deserializer::from_reader(rd);
            match <$ty as serde::Deserialize>::deserialize(&mut de) {
                Ok($r) => Some($body),
                Err(_) => None,
            }
        }
    }};
}

struct Proc<'p, 's> {
    file: &'p File,
    f: &'p mut FileState,
    sink: Sink<'s>,
    needles: Needles,
    /// Big lines after the last parsed state change that may set the state.
    pending_act: Vec<(u64, u64)>,
    /// Claude: big lines after the last parsed cwd that may set the cwd.
    pending_cwd: Vec<(u64, u64)>,
}

impl Proc<'_, '_> {
    fn flags(&self, l: &LineRef) -> u32 {
        match l.data {
            Some(d) => self.needles.flags(d),
            None => l.flags,
        }
    }

    fn defer_act(&mut self, l: &LineRef) {
        self.pending_act.push((l.off, l.len));
        if self.pending_act.len() > MAX_PENDING {
            self.resolve_act();
        }
    }

    fn line(&mut self, l: LineRef) {
        let fl = self.flags(&l);
        match self.f.agent {
            Agent::Claude => {
                if fl & C_USAGE == 0 {
                    // Any other line may set the state or the cwd (nearly all carry a cwd).
                    if fl & C_CWD != 0 {
                        self.pending_cwd.push((l.off, l.len));
                        if self.pending_cwd.len() > MAX_PENDING {
                            self.resolve_cwd();
                        }
                    }
                    if fl & (C_ASSISTANT | C_TOOL_RESULT) != 0 {
                        self.defer_act(&l);
                    }
                    return;
                }
                match l.data {
                    Some(d) => with_parsed!(d, CLine, |r| self.claude(&r)),
                    None => with_parsed_at!(self.file, l.off, l.len, CLine, |r| self.claude(&r)),
                };
            }
            Agent::Codex => {
                if fl & X_MUST == 0 {
                    if fl & X_RESPONSE_ITEM != 0 {
                        self.defer_act(&l);
                    }
                    return;
                }
                match l.data {
                    Some(d) => with_parsed!(d, XLine, |r| self.codex(&r)),
                    None => with_parsed_at!(self.file, l.off, l.len, XLine, |r| self.codex(&r)),
                };
            }
            Agent::Grok => {
                if fl & G_TURN == 0 {
                    if fl & G_UPDATE != 0 {
                        self.defer_act(&l);
                    }
                    return;
                }
                match l.data {
                    Some(d) => with_parsed!(d, GLine, |r| self.grok(&r)),
                    None => with_parsed_at!(self.file, l.off, l.len, GLine, |r| self.grok(&r)),
                };
            }
        }
    }

    fn claude(&mut self, r: &CLine) {
        // if (r.cwd) f.cwd = r.cwd
        match r.cwd() {
            Some(cwd) => {
                if self.f.cwd.as_deref() != Some(cwd) {
                    self.f.cwd = Some(cwd.to_owned());
                }
                self.pending_cwd.clear();
            }
            // A count is about to use f.cwd: settle it from the deferred lines first.
            None if r.counts() => self.resolve_cwd(),
            None => {}
        }
        if r.apply_act(self.f, &self.sink) {
            self.pending_act.clear();
        }
        r.apply_usage(self.f, &mut self.sink);
    }

    fn codex(&mut self, r: &XLine) {
        if r.act().is_some() {
            self.pending_act.clear();
        }
        r.apply(self.f, &mut self.sink);
    }

    fn grok(&mut self, r: &GLine) {
        if r.act().is_some() {
            self.pending_act.clear();
        }
        r.apply(self.f, &mut self.sink);
    }

    fn resolve_cwd(&mut self) {
        while let Some((off, len)) = self.pending_cwd.pop() {
            let found = with_parsed_at!(self.file, off, len, CLine, |r| r.cwd().map(str::to_owned)).flatten();
            if let Some(cwd) = found {
                self.f.cwd = Some(cwd);
                break;
            }
        }
        self.pending_cwd.clear();
    }

    fn resolve_act(&mut self) {
        while let Some((off, len)) = self.pending_act.pop() {
            let (file, f, sink) = (self.file, &mut *self.f, &self.sink);
            let done = match f.agent {
                Agent::Claude => with_parsed_at!(file, off, len, CLine, |r| r.apply_act(f, sink)),
                Agent::Codex => with_parsed_at!(file, off, len, XLine, |r| r.apply_act(f, sink)),
                Agent::Grok => with_parsed_at!(file, off, len, GLine, |r| r.apply_act(f, sink)),
            };
            if done == Some(true) {
                break;
            }
        }
        self.pending_act.clear();
    }

    fn finish(&mut self) {
        self.resolve_cwd();
        self.resolve_act();
    }
}

/// Reads `[from, size)` of the file into `f`, counting into `sink`. Returns the new offset.
fn read_range(file: &File, f: &mut FileState, from: u64, size: u64, sink: Sink) -> io::Result<u64> {
    let mut p = Proc { file, needles: needles(f.agent), f, sink, pending_act: Vec::new(), pending_cwd: Vec::new() };
    let needles = needles(p.f.agent);
    let end = for_each_line(file, from, size, &needles, |l| p.line(l))?;
    p.finish();
    Ok(end)
}

fn head_hash(file: &File, len: u64) -> io::Result<u64> {
    let mut b = vec![0u8; len as usize];
    let n = pread_full(file, &mut b, 0)?;
    Ok(fnv64(&[&b[..n]]))
}

pub struct Outcome {
    /// What to add to the global totals.
    pub delta: Contrib,
    pub rewritten: bool,
}

/// Whether the file at `f.offset` is still the file that was read: same inode, not shorter, a newline where the
/// last read stopped, and the same first bytes.
fn still_same(file: &File, f: &FileState, size: u64, ino: u64) -> io::Result<bool> {
    if f.offset == 0 {
        return Ok(true);
    }
    if size < f.offset || (ino != 0 && f.ino != 0 && ino != f.ino) {
        return Ok(false);
    }
    let mut b = [0u8; 1];
    if pread_full(file, &mut b, f.offset - 1)? != 1 || b[0] != b'\n' {
        return Ok(false);
    }
    if let Some((len, h)) = f.head {
        if head_hash(file, len)? != h {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Brings `f` up to date with the file at `path`, whose current size is `size`.
///
/// A rewritten file (shorter, replaced, or changed before the read offset) is read again from the start into a
/// fresh parse state. Its tokens then count as the larger of what it held before and what it holds now, per day,
/// model and repo: content that was already counted is not counted twice, content that was cut stays counted
/// (those tokens were spent), and content beyond the old one is added.
pub fn ingest(path: &Path, f: &mut FileState, size: u64, mtime: f64, ino: u64, env: &Env) -> io::Result<Outcome> {
    let file = File::open(path)?;
    let mut delta = Contrib::default();
    let rewritten = !still_same(&file, f, size, ino)?;
    if rewritten {
        let keep_cwd = if f.agent == Agent::Grok { f.cwd.clone() } else { None };
        let old = std::mem::replace(f, FileState::new(f.agent, keep_cwd));
        let mut fresh = Contrib::default();
        let sink = Sink::new(&mut fresh, None, env.home, env.tz, env.local, env.now).with_git(env.git);
        f.offset = read_range(&file, f, 0, size, sink)?;
        // Per key, the larger of the old and new contribution.
        let mut merged = old.contrib.clone();
        for (k, t) in fresh.days {
            let o = merged.days.entry(k.clone()).or_default();
            let (dt, dout) = ((t.tokens - o.tokens).max(0), (t.output - o.output).max(0));
            if !old.baseline && (dt != 0 || dout != 0) {
                delta.add_day(&k, dt, dout);
            }
            o.tokens += dt;
            o.output += dout;
        }
        for (k, t) in fresh.repos {
            let o = merged.repos.entry(k.clone()).or_default();
            let d = t.beyond(o);
            if !old.baseline && !d.is_zero() {
                delta.add_repo(&k, d);
            }
            *o += d;
        }
        f.contrib = merged;
        f.tokens = f.tokens.max(old.tokens);
        if let (Some(n), Some(o)) = (f.io.as_mut(), old.io) {
            n.inp = n.inp.max(o.inp);
            n.cr = n.cr.max(o.cr);
            n.cw = n.cw.max(o.cw);
            n.out = n.out.max(o.out);
        } else if f.io.is_none() {
            f.io = old.io;
        }
        f.next_stat = old.next_stat;
    } else {
        // The sink holds the contributions while `f` is being read, so take them out for the read.
        let mut contrib = std::mem::take(&mut f.contrib);
        let sink = Sink::new(&mut contrib, Some(&mut delta), env.home, env.tz, env.local, env.now).with_git(env.git);
        let from = f.offset;
        f.offset = read_range(&file, f, from, size, sink)?;
        f.contrib = contrib;
    }
    if ino != 0 {
        f.ino = ino;
    }
    let hl = f.offset.min(HEAD);
    if hl > 0 && f.head.map(|(l, _)| l) != Some(hl) {
        f.head = Some((hl, head_hash(&file, hl)?));
    }
    f.mtime = mtime;
    f.seen_size = Some(size);
    Ok(Outcome { delta, rewritten })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    pub fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-stats-rs-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn tokyo() -> TimeZone {
        jiff::tz::db().get("Asia/Tokyo").unwrap()
    }

    fn meta(p: &Path) -> (u64, f64, u64) {
        let m = std::fs::metadata(p).unwrap();
        (m.len(), crate::scan::mtime_ms(&m), crate::scan::inode(&m))
    }

    pub fn run(p: &Path, f: &mut FileState) -> Outcome {
        let (tz, local) = (tokyo(), tokyo());
        let env = Env { home: "/home/k", git: None, tz: &tz, local: &local, now: 1.0 };
        let (size, mtime, ino) = meta(p);
        ingest(p, f, size, mtime, ino, &env).unwrap()
    }

    fn write(p: &Path, s: &str) {
        std::fs::write(p, s).unwrap();
    }
    fn append(p: &Path, s: &str) {
        std::fs::OpenOptions::new().append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
    }

    fn claude(id: &str, ts: &str, inp: i64, out: i64, extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","cwd":"/home/k/Dev/chum/x","timestamp":"{ts}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[{{"type":"text","text":"hi"}}{extra}],"usage":{{"input_tokens":{inp},"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":{out}}}}}}}"#
        ) + "\n"
    }

    #[test]
    fn claude_counts_dedupes_and_tracks_state() {
        let d = tmpdir("claude");
        let p = d.join("s.jsonl");
        let mut s = claude("m1", "2026-10-12T14:59:00Z", 5, 7, "");
        s += &claude("m1", "2026-10-12T14:59:00Z", 5, 7, r#",{"type":"tool_use","name":"Bash"}"#); // same response, streamed
        s += &claude("m2", "2026-10-12T15:00:01Z", 1, 2, "");
        s += r#"{"type":"user","cwd":"/home/k/Dev/chum/x","timestamp":"2026-10-12T15:00:02Z","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        s += "\n";
        s += "not json\n";
        s += r#"{"type":"assistant","timestamp":"2026-10-12T15:00:03Z","message":{"id":"m3","model":"<synthetic>","content":[{"type":"text","text":"x"}],"usage":{"input_tokens":0,"output_tokens":0}}}"#;
        s += "\n{\"partial\":";
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|claude-opus-5-5"], crate::state::Totals { tokens: 122, output: 7 });
        assert_eq!(o.delta.days["2026-10-13|claude|claude-opus-5-5"], crate::state::Totals { tokens: 113, output: 2 });
        assert_eq!(o.delta.repos["2026-10-12|chum"].tokens, 122);
        assert_eq!(o.delta.repos["2026-10-12|chum"].io, crate::state::Io { inp: 5, cr: 100, cw: 10, out: 7 });
        assert_eq!(f.tokens, 235);
        assert_eq!(f.io, Some(crate::state::Io { inp: 6, cr: 200, cw: 20, out: 9 }));
        assert_eq!(f.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(f.state.as_deref(), Some("writing"));
        assert_eq!(f.state_at, Some(1791817203000.0));
        assert_eq!(f.cwd.as_deref(), Some("/home/k/Dev/chum/x"));
        // The partial last line is left for the next read.
        assert_eq!(f.offset as usize, s.len() - "{\"partial\":".len());
        append(&p, "\"x\"}\n");
        let o = run(&p, &mut f);
        assert!(o.delta.days.is_empty());
        assert_eq!(f.offset as usize, s.len() + 5);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn big_lines_are_deferred_but_still_set_state_and_cwd() {
        let d = tmpdir("claude-big");
        let p = d.join("s.jsonl");
        let big = "y".repeat(200 << 10);
        let mut s = claude("m1", "2026-10-12T01:00:00Z", 5, 7, "");
        s += &format!(
            r#"{{"type":"user","cwd":"/home/k/Dev/neon","timestamp":"2026-10-12T01:00:05Z","message":{{"content":[{{"type":"tool_result","content":"{big}"}}]}}}}"#
        );
        s += "\n";
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        run(&p, &mut f);
        assert_eq!(f.state.as_deref(), Some("thinking"));
        assert_eq!(f.state_at, Some(1791766805000.0));
        assert_eq!(f.cwd.as_deref(), Some("/home/k/Dev/neon"));
        // A counted line without its own cwd takes the cwd of the deferred line before it.
        let mut s2 = format!(r#"{{"type":"user","cwd":"/home/k/Dev/space-rock","message":{{"content":"{big}"}}}}"#);
        s2 += "\n";
        s2 += r#"{"type":"assistant","timestamp":"2026-10-12T02:00:00Z","message":{"id":"m9","content":[{"type":"thinking"}],"usage":{"input_tokens":3}}}"#;
        s2 += "\n";
        append(&p, &s2);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.repos["2026-10-12|space-rock"].tokens, 3);
        assert_eq!(f.state.as_deref(), Some("thinking"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lines_longer_than_the_buffer_are_streamed() {
        let d = tmpdir("huge");
        let p = d.join("s.jsonl");
        let big = "z".repeat(MAX_INMEM + (3 << 20));
        // A huge counted line: parsed straight from the file.
        let mut s = format!(
            r#"{{"type":"assistant","cwd":"/home/k/Dev/chum","timestamp":"2026-10-12T01:00:00Z","message":{{"id":"h","model":"m","content":[{{"type":"text","text":"{big}"}}],"usage":{{"input_tokens":40,"output_tokens":2}}}}}}"#
        );
        s += "\n";
        s += &claude("m2", "2026-10-12T01:00:01Z", 1, 1, "");
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|m"].tokens, 42);
        assert_eq!(o.delta.days["2026-10-12|claude|claude-opus-5-5"].tokens, 112);
        assert_eq!(f.offset as usize, s.len());
        // A huge unfinished line is not consumed.
        append(&p, &format!(r#"{{"type":"user","x":"{big}"#));
        run(&p, &mut f);
        assert_eq!(f.offset as usize, s.len());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_counts_last_usage_and_skips_repeats() {
        let d = tmpdir("codex");
        let p = d.join("rollout-2026-10-12T17-43-27-01a12f47-f5c2-70d1-93db-6ec3c8e36497.jsonl");
        let tc = |ts: &str, total: i64, last: Option<(i64, i64, i64)>| {
            let last = match last {
                Some((i, c, o)) => format!(
                    r#","last_token_usage":{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o},"total_tokens":{}}}"#,
                    i + o
                ),
                None => String::new(),
            };
            format!(
                r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":{total}}}{last}}}}}}}"#
            ) + "\n"
        };
        let mut s = String::new();
        s += r#"{"timestamp":"2026-10-12T08:43:28.298Z","type":"session_meta","payload":{"id":"x","cwd":"/home/k/.t3/worktrees/space-rock-ios/b","parent_thread_id":"01a03840-db2a-7672-a0a1-9c65b4fc1e1b"}}"#;
        s += "\n";
        s += r#"{"timestamp":"2026-10-12T08:43:29Z","type":"turn_context","payload":{"cwd":"/home/k/.t3/worktrees/space-rock-ios/b","model":"gpt-6-astra"}}"#;
        s += "\n";
        s += &tc("2026-10-12T08:44:00Z", 1000, Some((900, 800, 100)));
        s += &tc("2026-10-12T08:44:00Z", 1000, Some((900, 800, 100))); // repeated
        s += &tc("2026-10-12T08:45:00Z", 1500, None); // no last: the difference
        s += r#"{"timestamp":"2026-10-12T08:45:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}"}}"#;
        s += "\n";
        write(&p, &s);
        let mut f = FileState::new(Agent::Codex, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|codex|gpt-6-astra"], crate::state::Totals { tokens: 1500, output: 100 });
        assert_eq!(o.delta.repos["2026-10-12|space-rock-ios"].tokens, 1500);
        assert_eq!(f.total, 1500);
        assert_eq!(f.io, Some(crate::state::Io { inp: 100, cr: 800, cw: 0, out: 100 }));
        assert_eq!(f.state.as_deref(), Some("run"));
        assert!(f.sub);
        assert_eq!(f.parent.as_deref(), Some("01a03840-db2a-7672-a0a1-9c65b4fc1e1b"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn grok_counts_per_model_and_dedupes_prompts() {
        let d = tmpdir("grok");
        let p = d.join("updates.jsonl");
        let turn = |pid: &str, ts: i64| {
            format!(
                r#"{{"timestamp":{ts},"method":"_x.ai/session/update","params":{{"update":{{"sessionUpdate":"turn_completed","prompt_id":"{pid}","usage":{{"inputTokens":43300,"outputTokens":932,"totalTokens":44232,"cachedReadTokens":37760,"cacheCreationTokens":0,"modelUsage":{{"grok-4.6-build":{{"inputTokens":43300,"outputTokens":932,"totalTokens":44232,"cachedReadTokens":37760}},"grok-4.6-mini":{{"inputTokens":10,"outputTokens":5}}}}}}}}}}}}"#
            ) + "\n"
        };
        let mut s = String::new();
        s += r#"{"timestamp":1787644160,"params":{"update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"agentTimestampMs":1}}}"#;
        s += "\n";
        s += r#"{"timestamp":1787644161,"params":{"update":{"sessionUpdate":"tool_call","kind":"edit","_meta":{"agentTimestampMs":1787644161500}}}}"#;
        s += "\n";
        s += &turn("p1", 1787644184);
        s += &turn("p1", 1787644184);
        write(&p, &s);
        let mut f = FileState::new(Agent::Grok, Some("/home/k/Dev/llm-park".into()));
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-08-25|grok|grok-4.6-build"], crate::state::Totals { tokens: 44232, output: 932 });
        assert_eq!(o.delta.days["2026-08-25|grok|grok-4.6-mini"], crate::state::Totals { tokens: 15, output: 5 });
        assert_eq!(o.delta.repos["2026-08-25|llm-park"].tokens, 44247);
        assert_eq!(f.model.as_deref(), Some("grok-4.6-mini"));
        assert_eq!(f.state.as_deref(), Some("edit"));
        assert_eq!(f.state_at, Some(1787644161500.0));
        assert_eq!(f.io, Some(crate::state::Io { inp: 5550, cr: 37760, cw: 0, out: 937 }));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn truncated_transcript_is_not_double_counted() {
        let d = tmpdir("trunc");
        let p = d.join("s.jsonl");
        // 70 distinct responses: more than the 64-id window, so the JS reporter recounts them after a rewrite.
        let lines: Vec<String> = (0..70).map(|i| claude(&format!("m{i}"), "2026-10-12T01:00:00Z", 1, 0, "")).collect();
        write(&p, &lines.concat());
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        let key = "2026-10-12|claude|claude-opus-5-5";
        assert_eq!(o.delta.days[key].tokens, 70 * 111);
        // Rewritten shorter: the first 60 responses remain.
        write(&p, &lines[..60].concat());
        let o = run(&p, &mut f);
        assert!(o.rewritten);
        assert!(o.delta.days.is_empty(), "nothing new: {:?}", o.delta);
        assert_eq!(f.contrib.days[key].tokens, 70 * 111, "cut content stays counted");
        assert_eq!(f.offset as usize, lines[..60].concat().len());
        // Then appended to: only the new responses count.
        append(&p, &claude("n1", "2026-10-12T02:00:00Z", 1, 0, ""));
        let o = run(&p, &mut f);
        assert!(!o.rewritten);
        assert_eq!(o.delta.days[key].tokens, 111);
        // Rewritten in place with the same length but different leading content.
        let mut other = lines[..60].concat().into_bytes();
        other[20] = b'X';
        std::fs::write(&p, &other).unwrap();
        append(&p, &claude("n1", "2026-10-12T02:00:00Z", 1, 0, ""));
        let o = run(&p, &mut f);
        assert!(o.rewritten);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rewrite_of_a_migrated_file_counts_nothing_already_there() {
        let d = tmpdir("trunc-migrated");
        let p = d.join("s.jsonl");
        let lines: Vec<String> = (0..10).map(|i| claude(&format!("m{i}"), "2026-10-12T01:00:00Z", 1, 0, "")).collect();
        write(&p, &lines.concat());
        let mut f = FileState::new(Agent::Claude, None);
        f.offset = lines.concat().len() as u64;
        f.baseline = true;
        write(&p, &lines[..5].concat());
        let o = run(&p, &mut f);
        assert!(o.rewritten);
        assert!(o.delta.days.is_empty());
        assert!(!f.baseline);
        append(&p, &claude("n", "2026-10-12T01:00:00Z", 1, 0, ""));
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|claude-opus-5-5"].tokens, 111);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn claude_nocwd(id: Option<&str>, ts: &str, model: &str, inp: i64, cw: i64, cr: i64, out: i64) -> String {
        let id = id.map(|i| format!(r#""id":"{i}","#)).unwrap_or_default();
        format!(
            r#"{{"type":"assistant","cwd":"/home/k/Dev/chum","timestamp":"{ts}","message":{{{id}"model":"{model}","content":[{{"type":"text"}}],"usage":{{"input_tokens":{inp},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"output_tokens":{out}}}}}}}"#
        ) + "\n"
    }

    #[test]
    fn streamed_message_counts_at_its_largest_usage_in_its_first_bucket() {
        let d = tmpdir("stream-max");
        let p = d.join("s.jsonl");
        // 2026-10-12T14:59:59Z is 23:59:59 in Tokyo (day 10-12); the later lines are on the next day and model.
        let mut s = claude_nocwd(Some("m1"), "2026-10-12T14:59:59Z", "opus", 3, 10, 100, 5);
        s += &claude_nocwd(Some("m1"), "2026-10-12T15:00:01Z", "sonnet", 3, 10, 100, 60);
        s += &claude_nocwd(Some("m1"), "2026-10-12T15:00:02Z", "sonnet", 3, 12, 90, 60); // cw grew, cr "shrank"
        s += &claude_nocwd(Some("m1"), "2026-10-12T15:00:03Z", "sonnet", 3, 12, 100, 60); // equal to the max: nothing
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        // Counted at the largest of each field: in 3, cw 12, cr 100, out 60 = 175, all on the first line's bucket.
        assert_eq!(o.delta.days.len(), 1, "{:?}", o.delta.days);
        assert_eq!(o.delta.days["2026-10-12|claude|opus"], crate::state::Totals { tokens: 175, output: 60 });
        assert_eq!(o.delta.repos["2026-10-12|chum"].tokens, 175);
        assert_eq!(f.tokens, 175);
        assert_eq!(f.io, Some(crate::state::Io { inp: 3, cr: 100, cw: 12, out: 60 }));
        assert_eq!(f.model.as_deref(), Some("sonnet"));
        // Later in the same file, after a restart (state through JSON): the growth still goes to the first bucket.
        let mut f: FileState = serde_json::from_slice(&serde_json::to_vec(&f).unwrap()).unwrap();
        append(&p, &claude_nocwd(Some("m1"), "2026-10-13T01:00:00Z", "sonnet", 3, 12, 100, 70));
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|opus"], crate::state::Totals { tokens: 10, output: 10 });
        assert_eq!(o.delta.days.len(), 1);
        assert_eq!(f.tokens, 185);
        // A new id on the later day is its own bucket.
        append(&p, &claude_nocwd(Some("m2"), "2026-10-13T01:00:01Z", "sonnet", 1, 0, 0, 1));
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-13|claude|sonnet"], crate::state::Totals { tokens: 2, output: 1 });
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_ids_are_not_recounted_after_many_others() {
        let d = tmpdir("many-ids");
        let p = d.join("s.jsonl");
        let mut s = String::new();
        for i in 0..300 {
            s += &claude_nocwd(Some(&format!("m{i}")), "2026-10-12T01:00:00Z", "opus", 1, 0, 0, 1);
        }
        // m0 is replayed after 299 other ids (far past the old 64-entry ring), with more output.
        s += &claude_nocwd(Some("m0"), "2026-10-12T01:00:01Z", "opus", 1, 0, 0, 4);
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|opus"], crate::state::Totals { tokens: 600 + 3, output: 300 + 3 });
        assert_eq!(f.msgs.len(), 300);
        assert_eq!(f.buckets.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lines_without_a_message_id_each_count_once() {
        let d = tmpdir("no-id");
        let p = d.join("s.jsonl");
        let mut s = claude_nocwd(None, "2026-10-12T01:00:00Z", "opus", 1, 0, 0, 1);
        s += &claude_nocwd(None, "2026-10-12T01:00:01Z", "opus", 100, 0, 0, 0);
        s += &claude_nocwd(None, "2026-10-12T01:00:02Z", "opus", 100, 0, 0, 0); // an identical line is another line
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|claude|opus"], crate::state::Totals { tokens: 202, output: 1 });
        assert!(f.msgs.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn migrated_ids_are_skipped_and_new_ids_use_the_max_rule() {
        let d = tmpdir("migrated-ids");
        let p = d.join("s.jsonl");
        let mut s = claude_nocwd(Some("old"), "2026-10-12T01:00:00Z", "opus", 5, 0, 0, 1);
        s += &claude_nocwd(Some("old"), "2026-10-12T01:00:01Z", "opus", 5, 0, 0, 50);
        s += &claude_nocwd(Some("new"), "2026-10-12T01:00:02Z", "opus", 1, 0, 0, 1);
        s += &claude_nocwd(Some("new"), "2026-10-12T01:00:03Z", "opus", 1, 0, 0, 9);
        write(&p, &s);
        let mut f = FileState::new(Agent::Claude, None);
        f.ids.push(crate::js::id_hash(&crate::js::JsVal::Str("old".into())).unwrap());
        f.baseline = true;
        let o = run(&p, &mut f);
        // The migrated id counts nothing (it was counted before); the new one counts at 1 + 9.
        assert_eq!(o.delta.days["2026-10-12|claude|opus"], crate::state::Totals { tokens: 10, output: 9 });
        assert_eq!(f.msgs.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_fork_without_last_usage_takes_the_first_total_as_baseline() {
        let d = tmpdir("codex-fork");
        let p = d.join("rollout-fork.jsonl");
        let ev = |ts: &str, total: i64| {
            format!(
                r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":{total}}}}}}}}}"#
            ) + "\n"
        };
        let meta = |parent: &str| {
            format!(
                r#"{{"timestamp":"2026-10-12T01:00:00Z","type":"session_meta","payload":{{"id":"x","cwd":"/home/k/Dev/chum","parent_thread_id":"{parent}"}}}}{}"#,
                "\n"
            )
        };
        let mut s =
            meta("par") + r#"{"timestamp":"2026-10-12T01:00:00Z","type":"turn_context","payload":{"model":"gpt-6"}}"#;
        s += "\n";
        s += &ev("2026-10-12T01:00:01Z", 4300);
        s += &ev("2026-10-12T01:00:02Z", 4350);
        write(&p, &s);
        let mut f = FileState::new(Agent::Codex, None);
        let o = run(&p, &mut f);
        assert_eq!(o.delta.days["2026-10-12|codex|gpt-6"].tokens, 50);
        assert_eq!(f.total, 4350);
        assert_eq!(f.tokens, 50);
        // Not a fork: the first total is real usage.
        let p2 = d.join("rollout-plain.jsonl");
        let plain = s.replace(r#","parent_thread_id":"par""#, "");
        write(&p2, &plain);
        let mut f2 = FileState::new(Agent::Codex, None);
        let o = run(&p2, &mut f2);
        assert_eq!(o.delta.days["2026-10-12|codex|gpt-6"].tokens, 4350);
        let _ = std::fs::remove_dir_all(&d);
    }
}
