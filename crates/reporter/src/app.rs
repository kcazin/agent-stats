//! One reporter: configuration, the scan → report → save tick, and the policies around it (when to save, how to
//! log failures, what a full sync sends).

use crate::config::{Hub, Roots};
use crate::git::GitIds;
use crate::hub::{Http, Link};
use crate::ingest::{self, Env};
use crate::js::{basename, decode_uri_component, dirname, DayCache};
use crate::report::{self, Dirty};
use crate::scan::{inode, mtime_ms, Listed, Scanner};
use crate::state::{self, Agent, FileState, Loaded, State};
use agent_stats_common::{now_secs, Status, HEARTBEAT_SECS, PROTOCOL, VERSION};
use jiff::tz::TimeZone;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Resend recent days' totals this often, in case the hub lost them.
const FULL_SYNC: f64 = 10.0 * 60_000.0;
/// A full sync sends the days from this many days back (plus anything not yet acknowledged). The hub keeps the
/// maximum it has seen per day, so older days it already has never need sending again.
const FULL_SYNC_DAYS: i64 = 400;
/// While transcripts keep changing, save at most this often; on exit and after expensive work, save at once.
pub const SAVE_EVERY: Duration = Duration::from_secs(300);
const DAY_MS: f64 = 86_400_000.0;
/// Files modified within this long are stat'ed on every fast tick.
const HOT: f64 = 30.0 * 60_000.0;
/// A hub gets a report at least this often (a little under the 15 s slow tick, so jitter never stretches it past
/// 15 s; the hub calls a machine online for 45 s).
pub const HEARTBEAT: Duration = Duration::from_secs(13);

pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Send,
    Dry,
    Print,
}

pub struct Config {
    pub home: String,
    /// Where the transcripts are.
    pub roots: Roots,
    /// The hubs to report to, in config order (the legacy environment's hub first).
    pub hubs: Vec<Hub>,
    pub machine: String,
    pub label: String,
    pub state_file: PathBuf,
    pub js_state_file: PathBuf,
    pub tz_name: String,
    pub tz: TimeZone,
    pub local: TimeZone,
    pub output: Output,
    /// Log what each tick did and how long it took.
    pub verbose: bool,
    /// Where to keep the status file (`state_dir()/report-status.json`); `None`: don't keep one.
    pub status_file: Option<PathBuf>,
}

#[derive(Default)]
struct TickStats {
    stats: usize,
    reads: usize,
    bytes: u64,
    /// A report went out this tick.
    sent: bool,
}

pub struct App {
    pub cfg: Config,
    pub state: State,
    scanner: Scanner,
    /// The last directory walk, for the fast ticks between walks.
    listed: Vec<Listed>,
    /// Indexes into `listed` of the files modified in the last `HOT` as of the last slow tick.
    hot: Vec<usize>,
    /// Signature of the live sessions (see `sessions_sig`) as of the last tick that looked.
    sig: u64,
    /// Slow ticks so far; the stat tiers count in these.
    tick_no: u64,
    http: Http,
    /// One per configured hub: route, backoff, last result.
    pub links: Vec<Link>,
    /// cwd → git repository identity.
    pub git: GitIds,
    /// `--dry`/`--print`: when the last full body was printed.
    last_full: f64,
    /// State differs from what is on disk.
    changed: bool,
    /// Save at the next opportunity regardless of `SAVE_EVERY`.
    urgent: bool,
    last_save: Option<Instant>,
    save_error: Option<String>,
    read_errors: HashMap<String, String>,
    pub bytes_saved: u64,
    last: TickStats,
    started_at: u64,
    /// What the status file last said about the hubs, and when it was written.
    status_sig: String,
    status_at: Option<Instant>,
}

/// Modified recently enough to be stat'ed on every fast tick.
fn is_hot(idle_ms: f64) -> bool {
    idle_ms < HOT
}

/// Slow ticks to wait before stat'ing a file again: files idle for a day are checked every tick, for a week every
/// other tick, older ones every 4th.
fn stat_gap(idle_ms: f64) -> u64 {
    if idle_ms < DAY_MS {
        1
    } else if idle_ms < 7.0 * DAY_MS {
        2
    } else {
        4
    }
}

/// Whether a hub due for a report gets one now.
fn wants_send(force: bool, unacked: bool, sessions_changed: bool, since_last: Option<Duration>) -> bool {
    force || unacked || sessions_changed || since_last.is_none_or(|d| d >= HEARTBEAT)
}

/// What a hub's view of the live sessions depends on: ids, state, stateAt, model, tokens and io. Order-free
/// (the sessions come out of a map), and not the last-activity time, which moves with every write.
fn sessions_sig(sessions: &[report::Session]) -> u64 {
    let mut sum = 0u64;
    for s in sessions {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&s.id, s.agent, s.kind, &s.parent, &s.model, s.tokens, &s.state).hash(&mut h);
        s.state_at.map(f64::to_bits).hash(&mut h);
        s.io.map(|io| (io.inp, io.cr, io.cw, io.out)).hash(&mut h);
        sum = sum.wrapping_add(h.finish());
    }
    sum ^ sessions.len() as u64
}

fn log(msg: &str) {
    eprintln!("{} {msg}", jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ"));
}

impl App {
    pub fn new(cfg: Config) -> App {
        let (state, urgent) = load_state(&cfg);
        let git = GitIds::new(std::path::Path::new(&cfg.home));
        let mut app = App {
            cfg,
            state,
            scanner: Scanner::default(),
            listed: Vec::new(),
            hot: Vec::new(),
            sig: 0,
            tick_no: 0,
            http: Http::new(),
            links: Vec::new(),
            git,
            last_full: 0.0,
            changed: urgent,
            urgent,
            last_save: None,
            save_error: None,
            read_errors: HashMap::new(),
            bytes_saved: 0,
            last: TickStats::default(),
            started_at: now_secs(),
            status_sig: String::new(),
            status_at: None,
        };
        let hubs = std::mem::take(&mut app.cfg.hubs);
        app.set_hubs(hubs);
        app.write_status(true);
        app
    }

    /// Uses this list of hubs from now on (at start, and when config.toml changes). A hub that is still there
    /// keeps its route, backoff and unacknowledged keys; a new one starts with every key unacknowledged.
    pub fn set_hubs(&mut self, hubs: Vec<Hub>) {
        let mut old: Vec<Link> = std::mem::take(&mut self.links);
        for h in &hubs {
            let link = match old.iter().position(|l| l.hub == *h) {
                Some(i) => old.swap_remove(i),
                None => Link::new(h.clone()),
            };
            self.links.push(link);
        }
        let keys: Vec<(String, &str)> = hubs.iter().map(|h| (h.state_key(), h.name.as_str())).collect();
        let keys: Vec<(&str, &str)> = keys.iter().map(|(k, n)| (k.as_str(), *n)).collect();
        // State from before several hubs (or migrated from the JS reporter): its unacknowledged keys belong to the
        // hub it was reporting to then, the implicit one from HUB_URL / hub_token. Any other hub gets everything.
        let legacy = hubs.iter().find(|h| h.legacy).map(Hub::state_key);
        if self.state.sync_hubs(&keys, legacy.as_deref()) {
            self.changed = true;
            self.urgent = true;
        }
        self.cfg.hubs = hubs;
    }

    /// Walks the directories and brings `state.files` in line with them: new, moved and deleted transcripts.
    fn relist(&mut self) {
        let listed = self.scanner.list(&self.cfg.roots);
        let files = &mut self.state.files;

        let keys: HashSet<&str> = listed.iter().map(|l| l.key.as_str()).collect();
        let gone: Vec<String> = files.keys().filter(|k| !keys.contains(k.as_str())).cloned().collect();
        // Codex moves a session from sessions/ to archived_sessions/: same basename, new path. Carry its state
        // over instead of reading it again from the start (the JS reporter counts it twice).
        let mut moved: HashMap<String, String> = HashMap::new();
        for k in &gone {
            if files[k].agent == Agent::Codex {
                moved.insert(basename(k).to_owned(), k.clone());
            }
        }
        for l in &listed {
            if files.contains_key(&l.key) {
                continue;
            }
            self.changed = true;
            if l.agent == Agent::Codex {
                if let Some(old) = moved.remove(basename(&l.key)) {
                    let f = files.remove(&old).unwrap();
                    files.insert(l.key.clone(), f);
                    continue;
                }
            }
            // Grok keeps several logs per session (sessions/<cwd, URL-encoded>/<id>/); usage is in updates.jsonl.
            let cwd = (l.agent == Agent::Grok).then(|| decode_uri_component(basename(dirname(dirname(&l.key)))));
            files.insert(l.key.clone(), FileState::new(l.agent, cwd));
        }
        // Pruned transcripts leave the file list; their tokens stay in the daily totals.
        for k in gone {
            if files.remove(&k).is_some() {
                self.changed = true;
                self.read_errors.remove(&k);
            }
        }
        self.listed = listed;
    }

    /// Finds changed transcripts and reads what was appended. A slow scan walks the directories and checks every
    /// file on its tier; a fast one only checks the files that are hot.
    fn scan(&mut self, slow: bool) {
        let now = now_ms();
        let tick = self.tick_no;
        if slow {
            self.relist();
            self.tick_no += 1;
        }
        let files = &mut self.state.files;

        // Stat what may have changed. Files idle for a day are checked every other slow tick, for a week every 4th.
        let mut jobs = Vec::new();
        let candidates: Box<dyn Iterator<Item = &Listed> + '_> =
            if slow { Box::new(self.listed.iter()) } else { Box::new(self.hot.iter().map(|&i| &self.listed[i])) };
        for l in candidates {
            let Some(f) = files.get_mut(&l.key) else { continue };
            if if slow { f.next_stat > tick } else { !is_hot(now - f.mtime) } {
                continue;
            }
            let Ok(md) = fs::metadata(&l.path) else { continue };
            self.last.stats += 1;
            let (size, mtime, ino) = (md.len(), mtime_ms(&md), inode(&md));
            if slow {
                f.next_stat = tick + stat_gap(now - mtime);
            }
            if mtime == f.mtime && (size == f.offset || Some(size) == f.seen_size) {
                continue;
            }
            jobs.push((l.key.clone(), l.path.clone(), size, mtime, ino));
        }
        self.last.reads = jobs.len();
        if jobs.is_empty() {
            return;
        }

        let mut work: Vec<_> = jobs
            .into_iter()
            .map(|j| {
                let f = files.remove(&j.0).unwrap();
                (j, f.clone(), f)
            })
            .collect();
        let env = Env { home: &self.cfg.home, git: Some(&self.git), tz: &self.cfg.tz, local: &self.cfg.local, now };
        let run = |(j, _, f): &mut ((String, PathBuf, u64, f64, u64), FileState, FileState)| {
            ingest::ingest(&j.1, f, j.2, j.3, j.4, &env)
        };
        let results: Vec<_> =
            if work.len() >= 4 { work.par_iter_mut().map(run).collect() } else { work.iter_mut().map(run).collect() };
        let mut bytes = 0;
        for ((j, backup, f), res) in work.into_iter().zip(results) {
            match res {
                Ok(out) => {
                    if self.read_errors.remove(&j.0).is_some() {
                        log(&format!("read ok again: {}", j.0));
                    }
                    if out.rewritten {
                        log(&format!("rewritten, read again from the start: {}", j.0));
                        self.urgent = true;
                    }
                    if f.offset != backup.offset
                        || f.mtime != backup.mtime
                        || out.rewritten
                        || f.ino != backup.ino
                        || f.head != backup.head
                    {
                        self.changed = true;
                    }
                    bytes += f.offset.saturating_sub(if out.rewritten { 0 } else { backup.offset });
                    self.state.apply(&out.delta);
                    self.state.files.insert(j.0, f);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if self.read_errors.get(&j.0) != Some(&msg) {
                        log(&format!("read failed {}: {msg}", j.0));
                        self.read_errors.insert(j.0.clone(), msg);
                    }
                    self.state.files.insert(j.0, backup);
                }
            }
        }
        self.last.bytes = bytes;
        // A big read (the first full scan) leaves large freed buffers behind; hand them back.
        if bytes > 64 << 20 {
            self.urgent = true;
            #[cfg(all(target_os = "linux", target_env = "gnu"))]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }

    fn import_t3(&mut self) {
        let home = PathBuf::from(&self.cfg.home);
        let files = &self.state.files;
        let (delta, tokens) = crate::t3::import(
            &home,
            Some(&self.git),
            &|p| files.contains_key(p),
            &self.cfg.tz,
            &self.cfg.local,
            now_ms(),
        );
        if tokens != 0.0 {
            log(&format!("imported {:.2}B tokens of deleted transcripts from T3 Code's usage cache", tokens / 1e9));
        }
        self.state.apply(&delta);
        self.state.t3 = Some(now_ms());
        self.changed = true;
        self.urgent = true;
    }

    /// Repositories seen with a git remote since the last call: remember their remotes for the repo rows.
    fn collect_remotes(&mut self) {
        for (name, remote) in self.git.take_seen() {
            if self.state.remotes.get(&name) != Some(&remote) {
                self.state.remotes.insert(name, remote);
                self.changed = true;
            }
        }
    }

    /// One full scan and report to every hub that is due. Problems are logged and retried later; returns whether
    /// every hub got the report (always true when only printing).
    pub fn tick(&mut self) -> bool {
        self.run(true, true)
    }

    /// The 15 s tick of `report`: walk the directories, check every file on its tier, and report to a hub that
    /// has something new or has not heard from us for `HEARTBEAT`.
    pub fn tick_slow(&mut self) {
        self.run(true, false);
    }

    /// The fast tick: check the hot files only, and report only what changed.
    pub fn tick_fast(&mut self) {
        self.run(false, false);
    }

    fn run(&mut self, slow: bool, force: bool) -> bool {
        let started = now_ms();
        let t0 = Instant::now();
        self.last = TickStats::default();
        self.scan(slow);
        if slow {
            let now = now_ms();
            self.hot = (0..self.listed.len())
                .filter(|&i| self.state.files.get(&self.listed[i].key).is_some_and(|f| is_hot(now - f.mtime)))
                .collect();
        }
        let scanned = t0.elapsed();
        if self.state.t3.is_none() {
            self.import_t3();
        }
        self.collect_remotes();
        let now = now_ms();
        let cutoff =
            DayCache::new(self.cfg.tz.clone()).day(now - FULL_SYNC_DAYS as f64 * DAY_MS).unwrap_or("").to_owned();
        let sent = match self.cfg.output {
            Output::Dry | Output::Print => {
                let full = now - self.last_full > FULL_SYNC;
                if full {
                    self.last_full = now;
                }
                let keys: Vec<String> = self.links.iter().map(|l| l.hub.state_key()).collect();
                let json = self.body(&keys, full, &cutoff, now, |body| match self.cfg.output {
                    Output::Dry => report::dry_summary(&self.state, body, (now - started) as i64).into_bytes(),
                    _ => serde_json::to_vec(body).unwrap_or_default(),
                });
                println!("{}", String::from_utf8_lossy(&json));
                true
            }
            Output::Send => {
                let sent = self.send(now, &cutoff, slow, force);
                // The status file only says something new after a report or on the slow cadence.
                if slow || self.last.sent {
                    self.write_status(false);
                }
                sent
            }
        };
        if self.cfg.verbose {
            log(&format!(
                "{} tick: scan {:.1} ms ({} files, {} dirs listed, {} stat'ed, {} read, {} KiB), total {:.1} ms",
                if slow { "slow" } else { "fast" },
                scanned.as_secs_f64() * 1e3,
                self.state.files.len(),
                self.scanner.listed,
                self.last.stats,
                self.last.reads,
                self.last.bytes >> 10,
                t0.elapsed().as_secs_f64() * 1e3
            ));
        }
        sent
    }

    /// Builds the report body with the keys these hubs (by state key) haven't acknowledged (all rows from `cutoff`
    /// on when `full`) and hands it to `out`.
    fn body<T>(&self, hubs: &[String], full: bool, cutoff: &str, now: f64, out: impl FnOnce(&report::Body) -> T) -> T {
        let mut dirty = Dirty::default();
        for key in hubs {
            if let Some(p) = self.state.hubs.get(key) {
                dirty.days.extend(p.dirty.iter().map(String::as_str));
                dirty.repos.extend(p.dirty_repos.iter().map(String::as_str));
            }
        }
        let (days, repos) = report::rows(&self.state, full, cutoff, &dirty);
        let body = report::Body {
            protocol: PROTOCOL,
            version: VERSION,
            machine_id: &self.cfg.machine,
            machine: &self.cfg.machine,
            label: &self.cfg.label,
            platform: report::platform(),
            at: now as i64,
            full,
            days,
            repos,
            sessions: report::sessions(&self.state, now, &self.cfg.home, Some(&self.git)),
        };
        out(&body)
    }

    /// Sends one body to every hub that is due and wants one (`force`: every due hub), in parallel, and records
    /// each hub's acknowledgement. A hub wants a report when it has unacknowledged keys, when the live sessions
    /// differ from what it was last sent, or when `HEARTBEAT` has passed since the last attempt.
    fn send(&mut self, now: f64, cutoff: &str, slow: bool, force: bool) -> bool {
        let at = Instant::now();
        let ready: Vec<usize> = (0..self.links.len()).filter(|&i| self.links[i].due(at)).collect();
        if ready.is_empty() {
            return false;
        }
        // Sessions only change by a read, or by aging out of the open window, which the slow tick catches.
        if slow || self.last.reads > 0 {
            self.sig = sessions_sig(&report::sessions(&self.state, now, &self.cfg.home, Some(&self.git)));
        }
        let due: Vec<usize> = ready
            .into_iter()
            .filter(|&i| {
                let l = &self.links[i];
                let unacked = self.state.hubs.get(&l.hub.state_key()).is_some_and(|p| !p.is_empty());
                wants_send(force, unacked, l.sent_sig != Some(self.sig), l.last_sent.map(|t| at.duration_since(t)))
            })
            .collect();
        if due.is_empty() {
            return true;
        }
        self.last.sent = true;
        for &i in &due {
            self.links[i].last_sent = Some(at);
        }
        // One body for all of them: the union of what each still needs (a hub keeps the largest value per key, so
        // rows it already has are harmless), and a full sync when any of them is due one.
        let full = due.iter().any(|&i| now - self.links[i].last_full > FULL_SYNC);
        let keys: Vec<String> = due.iter().map(|&i| self.links[i].hub.state_key()).collect();
        let json = self.body(&keys, full, cutoff, now, |b| serde_json::to_vec(b).unwrap_or_default());
        let http = &self.http;
        let mut results: Vec<(usize, Result<(), String>)> = Vec::new();
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for (i, link) in self.links.iter_mut().enumerate() {
                if !due.contains(&i) {
                    continue;
                }
                let json = &json;
                if due.len() == 1 {
                    results.push((i, link.attempt(http, json, at)));
                } else {
                    handles.push((i, s.spawn(move || link.attempt(http, json, at))));
                }
            }
            for (i, h) in handles {
                results.push((i, h.join().unwrap_or_else(|_| Err("sending panicked".into()))));
            }
        });
        self.save_learned_routes();
        let mut all = due.len() == self.links.len();
        for (i, res) in results {
            let link = &mut self.links[i];
            let pending = self.state.hubs.entry(link.hub.state_key()).or_default();
            match res {
                Ok(()) => {
                    link.sent_sig = Some(self.sig);
                    if !pending.is_empty() {
                        *pending = Default::default();
                        self.changed = true;
                    }
                    if full {
                        link.last_full = now;
                    }
                    if link.logged.take().is_some() {
                        log(&format!("hub {}: reachable again via {}", link.hub.name, link.route_url().unwrap_or("?")));
                    }
                }
                Err(e) => {
                    all = false;
                    // Log once per kind of failure, not every 15 s.
                    if link.logged.as_deref() != Some(&e) {
                        log(&format!(
                            "hub {}: report failed: {e} ({} keys waiting; retrying with backoff up to {} s)",
                            link.hub.name,
                            pending.len(),
                            crate::hub::BACKOFF_MAX.as_secs()
                        ));
                        link.logged = Some(e);
                    }
                }
            }
        }
        all
    }

    /// Adds the replica routes hubs taught (see `Link::learn`) to their entries in config.toml, so a restarted
    /// reporter still knows them when the replica it joined is away. Only the entry of the same hub id changes.
    fn save_learned_routes(&mut self) {
        if self.cfg.output != Output::Send || self.links.iter().all(|l| l.learned.is_empty()) {
            return;
        }
        let path = crate::config::config_file();
        let Ok(mut file) = crate::config::read_file(&path) else { return };
        let mut changed = false;
        for l in self.links.iter_mut().filter(|l| !l.learned.is_empty()) {
            let learned = std::mem::take(&mut l.learned);
            let Some(entry) = file.hubs.iter_mut().find(|e| e.id.is_some() && e.id == l.hub.id) else { continue };
            for r in learned {
                if !entry.routes.iter().any(|o| crate::config::same_route(o, &r)) {
                    entry.routes.push(r.clone());
                    changed = true;
                    log(&format!("hub {}: learned another replica's route {r}", l.hub.name));
                }
            }
        }
        if changed {
            if let Err(e) = crate::config::write_file(&path, &file) {
                log(&format!("config: could not save learned routes: {e}"));
            }
        }
        // config.toml now matches what the links have, so the reload it triggers keeps them as they are.
        let hubs: Vec<Hub> = self.links.iter().map(|l| l.hub.clone()).collect();
        self.cfg.hubs = hubs;
    }

    /// Writes the status file at start, when a hub's result changed, and at least every `HEARTBEAT_SECS`.
    pub fn write_status(&mut self, force: bool) {
        let Some(path) = self.cfg.status_file.clone() else { return };
        if self.cfg.output != Output::Send {
            return;
        }
        let sig: String = self
            .links
            .iter()
            .map(|l| format!("{}|{:?}|{:?}|{}\n", l.hub.name, l.route_url(), l.last_error, l.last_ok_at.is_some()))
            .collect();
        let due = force
            || sig != self.status_sig
            || self.status_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(HEARTBEAT_SECS));
        if !due {
            return;
        }
        let hubs: Vec<serde_json::Value> = self
            .links
            .iter()
            .map(|l| {
                serde_json::json!({
                    "name": l.hub.name,
                    "id": l.hub.id,
                    "route": l.route_url(),
                    "lastOkAt": l.last_ok_at,
                    "lastError": l.last_error,
                    "unacked": self.state.hubs.get(&l.hub.state_key()).map_or(0, |p| p.len()),
                })
            })
            .collect();
        let st = Status {
            role: "report".into(),
            version: VERSION.into(),
            pid: std::process::id(),
            started_at: self.started_at,
            heartbeat_at: now_secs(),
            detail: serde_json::json!({"machine": self.cfg.machine, "label": self.cfg.label, "hubs": hubs}),
        };
        let bytes = serde_json::to_vec_pretty(&st).unwrap_or_default();
        match agent_stats_common::atomic_write(&path, &bytes, None) {
            Ok(()) => {
                self.status_sig = sig;
                self.status_at = Some(Instant::now());
            }
            Err(e) => {
                // Retried next tick; logged once.
                if !self.status_sig.starts_with('!') {
                    log(&format!("writing {} failed: {e}", path.display()));
                }
                self.status_sig = "!".into();
            }
        }
    }

    /// Saves if the state changed, at most every `SAVE_EVERY` unless `force` or something expensive happened.
    /// A failed save (disk full, read-only) keeps everything in memory and is retried later.
    pub fn save(&mut self, force: bool) {
        // --dry and --print never write: they may run beside the live service, and two writers would corrupt it.
        if self.cfg.output != Output::Send || !self.changed {
            return;
        }
        let due = force || self.urgent || self.last_save.is_none_or(|t| t.elapsed() >= SAVE_EVERY);
        if !due {
            return;
        }
        self.last_save = Some(Instant::now());
        match state::save(&self.state, &self.cfg.state_file) {
            Ok(n) => {
                self.bytes_saved += n as u64;
                self.changed = false;
                self.urgent = false;
                if self.save_error.take().is_some() {
                    log("state saved again");
                }
            }
            Err(e) => {
                // Retry at the regular pace, not on every tick.
                self.urgent = false;
                let msg = e.to_string();
                if self.save_error.as_deref() != Some(&msg) {
                    log(&format!("saving state failed (kept in memory, will retry): {msg}"));
                    self.save_error = Some(msg);
                }
            }
        }
    }
}

/// Loads this reporter's state, or migrates the JS reporter's, or starts empty. Returns whether to save soon.
fn load_state(cfg: &Config) -> (State, bool) {
    match state::load(&cfg.state_file) {
        Loaded::State(mut s) => {
            if s.tz != cfg.tz_name {
                log(&format!(
                    "state has days in {}, now counting in {}; existing days are kept as they are",
                    s.tz, cfg.tz_name
                ));
                s.tz = cfg.tz_name.clone();
                return (s, true);
            }
            return (s, false);
        }
        Loaded::Unusable(why) => {
            if cfg.output == Output::Send {
                let aside = cfg.state_file.with_extension(format!("json.unusable-{}", now_ms() as u64));
                log(&format!("ignoring {}: {why} (moved to {})", cfg.state_file.display(), aside.display()));
                let _ = fs::rename(&cfg.state_file, &aside);
            } else {
                log(&format!("ignoring {}: {why} (left in place)", cfg.state_file.display()));
            }
            // This reporter's state existed, so the JS one was migrated long ago: counted again from the transcripts.
            return (State::new(&cfg.tz_name), false);
        }
        Loaded::Missing => {}
    }
    match state::migrate_js(&cfg.js_state_file, &cfg.tz_name) {
        Ok(Some(s)) => {
            log(&format!(
                "migrated the JS reporter's state from {}: {} files, {} day rows, {} repo rows{}",
                cfg.js_state_file.display(),
                s.files.len(),
                s.days.len(),
                s.repos.len(),
                if s.t3.is_some() { ", T3 Code import already done" } else { "" }
            ));
            (s, true)
        }
        Ok(None) => (State::new(&cfg.tz_name), false),
        Err(e) => {
            log(&format!(
                "could not read the JS reporter's state {}: {e}; starting empty",
                cfg.js_state_file.display()
            ));
            (State::new(&cfg.tz_name), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::hub::tests::{dead_url, hub, hub_server, serve, Fake};

    fn cfg(home: &std::path::Path, out: Output) -> Config {
        cfg_hubs(home, out, vec![hub("default", None, &[&dead_url()])])
    }

    fn cfg_hubs(home: &std::path::Path, out: Output, hubs: Vec<Hub>) -> Config {
        let tz = crate::ingest::tests::tokyo();
        Config {
            home: home.to_string_lossy().into_owned(),
            roots: Roots::under(home),
            hubs,
            machine: "m".into(),
            label: "m".into(),
            state_file: home.join("state").join("reporter-state.json"),
            js_state_file: home.join("state").join("state.json"),
            tz_name: "Asia/Tokyo".into(),
            tz: tz.clone(),
            local: tz,
            output: out,
            verbose: false,
            status_file: None,
        }
    }

    fn write_transcript(home: &std::path::Path) -> PathBuf {
        let p = home.join(".claude/projects/-h-Dev-chum/s.jsonl");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(
            &p,
            r#"{"type":"assistant","cwd":"/h/Dev/chum","timestamp":"2026-10-12T01:00:00Z","message":{"id":"a","model":"m","content":[{"type":"text"}],"usage":{"input_tokens":5}}}
"#,
        )
        .unwrap();
        p
    }

    #[test]
    fn state_is_not_rewritten_when_nothing_changed() {
        let home = crate::ingest::tests::tmpdir("nochange");
        write_transcript(&home);
        let mut app = App::new(cfg(&home, Output::Send));
        app.tick();
        app.save(true);
        let sf = home.join("state/reporter-state.json");
        let first = fs::metadata(&sf).unwrap().modified().unwrap();
        assert_eq!(app.state.days["2026-10-12|claude|m"].tokens, 5);
        assert!(app.state.t3.is_some());
        std::thread::sleep(Duration::from_millis(30));
        // Same process, more ticks.
        for _ in 0..6 {
            app.tick();
            app.save(true);
        }
        // A fresh process on the same state.
        let mut app2 = App::new(cfg(&home, Output::Send));
        app2.tick();
        app2.save(true);
        assert_eq!(fs::metadata(&sf).unwrap().modified().unwrap(), first);
        assert_eq!(app.bytes_saved, fs::metadata(&sf).unwrap().len());
        assert_eq!(app2.bytes_saved, 0);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn failed_reports_keep_dirty_rows_and_saves_survive_errors() {
        let home = crate::ingest::tests::tmpdir("offline");
        write_transcript(&home);
        let mut app = App::new(cfg(&home, Output::Send));
        app.tick();
        assert!(app.state.hubs["default"].dirty.contains("2026-10-12|claude|m"));
        assert!(app.links[0].last_error.is_some());
        // Saving into a path that cannot be a directory fails without losing anything.
        app.cfg.state_file = home.join(".claude/projects/-h-Dev-chum/s.jsonl/x/state.json");
        app.save(true);
        assert!(app.save_error.is_some());
        assert!(app.changed);
        assert_eq!(app.state.days["2026-10-12|claude|m"].tokens, 5);
        // Back to a writable place: the dirty rows persist across a restart.
        app.cfg.state_file = home.join("state/reporter-state.json");
        app.save(true);
        let app2 = App::new(cfg(&home, Output::Send));
        assert!(app2.state.hubs["default"].dirty.contains("2026-10-12|claude|m"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn migrates_js_state_v3() {
        let home = crate::ingest::tests::tmpdir("migrate");
        let p = write_transcript(&home);
        let len = fs::metadata(&p).unwrap().len();
        fs::create_dir_all(home.join("state")).unwrap();
        let js = serde_json::json!({
            "v": 3, "t3": 1791894193808u64,
            "days": {"2025-01-01|claude|old": {"tokens": 1000, "output": 10}, "2026-10-12|claude|m": {"tokens": 5, "output": 0}},
            "repos": {"2025-01-01|gone": 1000},
            "files": {p.to_string_lossy(): {"agent": "claude", "offset": len, "mtime": 1.5, "ids": ["a", null], "total": 0, "tokens": 5,
                "cwd": "/h/Dev/chum", "model": "m", "state": "writing", "stateAt": 1791763200000u64, "io": {"in": 5, "cr": 0, "cw": 0, "out": 0}}}
        });
        fs::write(home.join("state/state.json"), js.to_string()).unwrap();
        let mut app = App::new(cfg(&home, Output::Send));
        assert_eq!(app.state.days.len(), 2);
        assert_eq!(app.state.files[&*p.to_string_lossy()].ids.len(), 2);
        app.tick();
        // Nothing recounted: the file was read up to its end already; history from deleted transcripts is kept.
        assert_eq!(app.state.days["2026-10-12|claude|m"].tokens, 5);
        assert_eq!(app.state.days["2025-01-01|claude|old"].tokens, 1000);
        assert_eq!(app.state.repos["2025-01-01|gone"].tokens, 1000);
        assert_eq!(app.state.t3, Some(1791894193808.0));
        // The JS state file is left alone, for a rollback.
        assert!(home.join("state/state.json").exists());
        app.save(true);
        assert!(home.join("state/reporter-state.json").exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_older_state_is_counted_again_from_the_transcripts() {
        let home = crate::ingest::tests::tmpdir("older-state");
        write_transcript(&home);
        fs::create_dir_all(home.join("state")).unwrap();
        // A state of the last version, and the JS reporter's from long before: the transcripts win, not the JS state.
        fs::write(home.join("state/reporter-state.json"), r#"{"v":1,"tz":"Asia/Tokyo","repos":{"2026-10-12|chum":5}}"#)
            .unwrap();
        fs::write(home.join("state/state.json"), r#"{"v":3,"repos":{"2020-01-01|old":9}}"#).unwrap();
        let mut app = App::new(cfg(&home, Output::Send));
        assert!(app.state.repos.is_empty());
        app.tick();
        assert_eq!(
            app.state.repos["2026-10-12|chum"],
            crate::state::RepoTotals { tokens: 5, io: crate::state::Io { inp: 5, ..Default::default() } }
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn dry_and_print_never_write_state() {
        for out in [Output::Dry, Output::Print] {
            let home = crate::ingest::tests::tmpdir("readonly");
            write_transcript(&home);
            let mut app = App::new(cfg(&home, out));
            app.tick();
            app.save(true);
            assert!(app.changed, "the in-memory state did change");
            assert!(!home.join("state").exists(), "no state directory or file was created");
            // An existing state, even an unusable one, is left alone (the live service may be using it).
            fs::create_dir_all(home.join("state")).unwrap();
            fs::write(home.join("state/reporter-state.json"), "not json").unwrap();
            let mut app = App::new(cfg(&home, out));
            app.tick();
            app.save(true);
            assert_eq!(fs::read_to_string(home.join("state/reporter-state.json")).unwrap(), "not json");
            assert_eq!(fs::read_dir(home.join("state")).unwrap().count(), 1);
            let _ = fs::remove_dir_all(&home);
        }
    }

    fn reports(f: &Fake) -> Vec<serde_json::Value> {
        f.reqs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/report")
            .map(|r| serde_json::from_str(&r.body).unwrap())
            .collect()
    }

    #[test]
    fn each_hub_acknowledges_on_its_own() {
        let home = crate::ingest::tests::tmpdir("multihub");
        let p = write_transcript(&home);
        let good = hub_server("good", 200);
        let bad = hub_server("bad", 500);
        let status = home.join("state/report-status.json");
        let mut c = cfg_hubs(
            &home,
            Output::Send,
            vec![hub("bad", Some("bad"), &[&bad.url]), hub("good", Some("good"), &[&good.url])],
        );
        c.status_file = Some(status.clone());
        let mut app = App::new(c);
        assert!(status.exists(), "status is written at start");
        assert!(!app.tick(), "one hub failed");
        // The good hub got the body (protocol 2) and acknowledged it; the failing one keeps its keys.
        let got = reports(&good);
        assert_eq!(got.len(), 1);
        let b = &got[0];
        assert_eq!(
            (b["protocol"].as_u64(), b["machineId"].as_str(), b["machine"].as_str()),
            (Some(2), Some("m"), Some("m"))
        );
        assert_eq!(b["version"].as_str(), Some(VERSION));
        assert_eq!(b["days"].as_array().unwrap().len(), 1);
        assert!(app.state.hubs["id:good"].is_empty());
        assert!(app.state.hubs["id:bad"].dirty.contains("2026-10-12|claude|m"));
        assert_eq!(reports(&bad).len(), 1);
        let st: serde_json::Value = serde_json::from_slice(&fs::read(&status).unwrap()).unwrap();
        let hubs = st["detail"]["hubs"].as_array().unwrap();
        assert_eq!(hubs[0]["name"], "bad");
        assert!(hubs[0]["lastError"].as_str().unwrap().contains("500"));
        assert_eq!(hubs[0]["unacked"], 2);
        assert!(hubs[1]["lastOkAt"].as_u64().is_some());
        assert_eq!(hubs[1]["route"].as_str(), Some(good.url.as_str()));
        // The failing hub backs off and is skipped; the good one keeps receiving new rows, without the old ones.
        fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(br#"{"type":"assistant","cwd":"/h/Dev/chum","timestamp":"2026-10-13T01:00:00Z","message":{"id":"b","model":"m","content":[{"type":"text"}],"usage":{"input_tokens":7}}}
"#)
            .unwrap();
        app.links[0].next_try = Some(Instant::now() + Duration::from_secs(60));
        app.links[1].last_full = now_ms();
        let bad_before = bad.reqs.lock().unwrap().len();
        app.tick();
        assert_eq!(bad.reqs.lock().unwrap().len(), bad_before, "a hub in backoff is not contacted");
        let got = reports(&good);
        assert_eq!(got.len(), 2);
        let days: Vec<&str> = got[1]["days"].as_array().unwrap().iter().map(|d| d["day"].as_str().unwrap()).collect();
        assert_eq!(days, ["2026-10-13"]);
        assert_eq!(got[1]["full"], false);
        assert_eq!(app.state.hubs["id:bad"].dirty.len(), 2);
        // Once the failing hub is back, it gets everything it missed.
        let fixed = hub_server("bad", 200);
        app.links[0].next_try = None;
        app.links[0].route = None;
        app.links[0].hub.routes = vec![fixed.url.clone()];
        app.tick();
        assert!(app.state.hubs["id:bad"].is_empty());
        let last = reports(&fixed).pop().unwrap();
        assert_eq!(last["days"].as_array().unwrap().len(), 2, "{last}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn hubs_added_later_get_everything_and_legacy_dirty_moves_to_the_first_hub() {
        let home = crate::ingest::tests::tmpdir("addhub");
        write_transcript(&home);
        // State from before several hubs, with one unacknowledged key.
        let mut st = State::new("Asia/Tokyo");
        st.days.insert("2026-10-01|claude|m".into(), crate::state::Totals { tokens: 3, output: 0 });
        st.days.insert("2026-10-02|claude|m".into(), crate::state::Totals { tokens: 4, output: 0 });
        st.dirty.insert("2026-10-02|claude|m".into());
        st.t3 = Some(1.0);
        state::save(&st, &home.join("state/reporter-state.json")).unwrap();
        let first = serve(|r| if r.path == "/report" { (200, r#"{"ok":true}"#.into()) } else { (404, String::new()) });
        let mut app = App::new(cfg_hubs(&home, Output::Send, vec![hub("default", None, &[&first.url])]));
        assert_eq!(app.state.hubs["default"].dirty.iter().collect::<Vec<_>>(), ["2026-10-02|claude|m"]);
        assert!(app.state.dirty.is_empty());
        assert!(app.tick());
        // A hub joined later (config.toml changed) starts with every key unacknowledged.
        let second = hub_server("two", 200);
        let mut hubs = app.cfg.hubs.clone();
        hubs.push(hub("two", Some("two"), &[&second.url]));
        app.set_hubs(hubs);
        assert_eq!(app.state.hubs["id:two"].dirty.len(), 3);
        app.links[0].last_full = now_ms();
        app.links[1].last_full = now_ms();
        assert!(app.tick());
        let got = reports(&second);
        assert_eq!(got[0]["days"].as_array().unwrap().len(), 3);
        assert!(app.state.hubs.values().all(|p| p.is_empty()));
        // A removed hub is forgotten.
        app.set_hubs(vec![app.cfg.hubs[1].clone()]);
        assert_eq!(app.state.hubs.keys().collect::<Vec<_>>(), ["id:two"]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repo_rows_carry_the_git_remote() {
        let home = crate::ingest::tests::tmpdir("remote");
        let repo = home.join("Dev/chum-folder");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:kcazin/chum.git\n").unwrap();
        let p = home.join(".claude/projects/-x/s.jsonl");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let cwd = repo.join("src").to_string_lossy().into_owned();
        fs::write(&p, format!(r#"{{"type":"assistant","cwd":"{cwd}","timestamp":"2026-10-12T01:00:00Z","message":{{"id":"a","model":"m","content":[{{"type":"text"}}],"usage":{{"input_tokens":5}}}}}}
"#)).unwrap();
        let h = serve(|r| if r.path == "/report" { (200, r#"{"ok":true}"#.into()) } else { (404, String::new()) });
        let mut app = App::new(cfg_hubs(&home, Output::Send, vec![hub("default", None, &[&h.url])]));
        assert!(app.tick());
        let b = reports(&h).pop().unwrap();
        assert_eq!(
            b["repos"],
            serde_json::json!([{"day": "2026-10-12", "repo": "chum", "tokens": 5, "remote": "github.com/kcazin/chum",
                "io": {"in": 5, "cr": 0, "cw": 0, "out": 0}}])
        );
        // Sessions use the same name (this one's cwd is under the temp dir, so it isn't listed as a session).
        assert_eq!(report::repo_name(&cwd, &app.cfg.home, Some(&app.git)), "chum");
        assert_eq!(app.state.remotes["chum"], "github.com/kcazin/chum");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ack_state_is_keyed_by_hub_id_so_a_new_hub_with_a_reused_name_gets_everything() {
        let home = crate::ingest::tests::tmpdir("hubid");
        // State written when acks were keyed by name: "hub" (id abc) has acknowledged everything.
        let mut st = State::new("Asia/Tokyo");
        st.days.insert("2026-10-01|claude|m".into(), crate::state::Totals { tokens: 3, output: 0 });
        st.repos.insert("2026-10-01|r".into(), crate::state::RepoTotals { tokens: 3, ..Default::default() });
        st.hubs.insert("hub".into(), Default::default());
        st.t3 = Some(1.0);
        state::save(&st, &home.join("state/reporter-state.json")).unwrap();
        let mut app = App::new(cfg_hubs(&home, Output::Send, vec![hub("hub", Some("abc"), &[&dead_url()])]));
        // Migrated to the id key, acks kept.
        assert_eq!(app.state.hubs.keys().collect::<Vec<_>>(), ["id:abc"]);
        assert!(app.state.hubs["id:abc"].is_empty());
        // `join` replaced it with another hub that is also called "hub": that hub has nothing yet.
        app.set_hubs(vec![hub("hub", Some("xyz"), &[&dead_url()])]);
        assert_eq!(app.state.hubs.keys().collect::<Vec<_>>(), ["id:xyz"]);
        assert_eq!(app.state.hubs["id:xyz"].len(), 2);
        // Renaming a hub keeps its acks.
        app.state.hubs.get_mut("id:xyz").unwrap().dirty.clear();
        app.set_hubs(vec![hub("renamed", Some("xyz"), &[&dead_url()])]);
        assert_eq!(app.state.hubs["id:xyz"].len(), 1);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn after_js_migration_only_the_legacy_hub_inherits_and_other_hubs_get_the_whole_history() {
        let home = crate::ingest::tests::tmpdir("jsinherit");
        fs::create_dir_all(home.join("state")).unwrap();
        // Older than the 400 days a full sync covers.
        let js = serde_json::json!({
            "v": 3, "t3": 1.0,
            "days": {"2023-01-01|claude|old": {"tokens": 1000, "output": 10}, "2026-10-12|claude|m": {"tokens": 5, "output": 0}},
            "repos": {"2023-01-01|gone": 1000},
            "files": {}
        });
        fs::write(home.join("state/state.json"), js.to_string()).unwrap();
        // Only a joined hub: it starts with every key.
        let joined = hub_server("abc", 200);
        let mut app = App::new(cfg_hubs(&home, Output::Send, vec![hub("home", Some("abc"), &[&joined.url])]));
        assert_eq!(app.state.hubs["id:abc"].len(), 3);
        assert!(app.tick());
        let b = reports(&joined).pop().unwrap();
        let days: Vec<&str> = b["days"].as_array().unwrap().iter().map(|d| d["day"].as_str().unwrap()).collect();
        assert!(days.contains(&"2023-01-01"), "{days:?}");
        // With the legacy hub (the one the JS reporter sent to) configured too: only it inherits the JS set.
        let mut app = App::new(cfg_hubs(
            &home,
            Output::Send,
            vec![hub("default", None, &[&dead_url()]), hub("home", Some("abc"), &[&joined.url])],
        ));
        assert!(app.state.hubs["default"].is_empty());
        assert_eq!(app.state.hubs["id:abc"].len(), 3);
        app.save(true);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn send_decision_and_tiers() {
        let s = Duration::from_secs;
        // Nothing new: wait for the heartbeat. Anything new, or a forced tick: send now.
        assert!(!wants_send(false, false, false, Some(s(1))));
        assert!(!wants_send(false, false, false, Some(HEARTBEAT - Duration::from_millis(1))));
        assert!(wants_send(false, false, false, Some(HEARTBEAT)));
        assert!(wants_send(false, false, false, None));
        assert!(wants_send(false, true, false, Some(s(1))));
        assert!(wants_send(false, false, true, Some(s(1))));
        assert!(wants_send(true, false, false, Some(s(1))));
        // Hot files: modified within 30 minutes. The slow tiers count whole days.
        assert!(is_hot(0.0) && is_hot(HOT - 1.0) && !is_hot(HOT));
        assert_eq!(stat_gap(HOT), 1);
        assert_eq!(stat_gap(DAY_MS - 1.0), 1);
        assert_eq!(stat_gap(DAY_MS), 2);
        assert_eq!(stat_gap(7.0 * DAY_MS), 4);
    }

    fn session(id: &str, state: &str, tokens: i64) -> report::Session {
        report::Session {
            id: id.into(),
            agent: "claude",
            kind: "main",
            parent: None,
            project: "p".into(),
            cwd: None,
            repo: None,
            model: Some("m".into()),
            last_activity: 1,
            tokens,
            state: Some(state.into()),
            state_at: Some(5.0),
            io: None,
        }
    }

    #[test]
    fn session_signature_follows_what_the_widget_shows() {
        let a = vec![session("a", "working", 1), session("b", "idle", 2)];
        let swapped = vec![session("b", "idle", 2), session("a", "working", 1)];
        assert_eq!(sessions_sig(&a), sessions_sig(&swapped), "order does not matter");
        let mut touched = a.clone();
        touched[0].last_activity = 99;
        assert_eq!(sessions_sig(&a), sessions_sig(&touched), "last activity alone is not a change");
        let mut state = a.clone();
        state[0].state = Some("idle".into());
        assert_ne!(sessions_sig(&a), sessions_sig(&state));
        let mut tokens = a.clone();
        tokens[1].tokens = 3;
        assert_ne!(sessions_sig(&a), sessions_sig(&tokens));
        let mut io = a.clone();
        io[1].io = Some(crate::state::Io { inp: 1, ..Default::default() });
        assert_ne!(sessions_sig(&a), sessions_sig(&io));
        assert_ne!(sessions_sig(&a), sessions_sig(&a[..1]));
    }

    #[test]
    fn fast_ticks_report_changes_and_heartbeats_only() {
        let home = crate::ingest::tests::tmpdir("fast");
        let p = write_transcript(&home);
        let good = hub_server("good", 200);
        let mut app = App::new(cfg_hubs(&home, Output::Send, vec![hub("good", Some("good"), &[&good.url])]));
        assert!(app.tick());
        assert_eq!(reports(&good).len(), 1);
        // Nothing changed: a fast tick stays quiet.
        app.tick_fast();
        app.tick_fast();
        assert_eq!(reports(&good).len(), 1);
        // New tokens in a hot file go out on the next fast tick, without waiting for the slow one.
        fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(br#"{"type":"assistant","cwd":"/h/Dev/chum","timestamp":"2026-10-12T02:00:00Z","message":{"id":"b","model":"m","content":[{"type":"text"}],"usage":{"input_tokens":7}}}
"#)
            .unwrap();
        app.tick_fast();
        assert_eq!(reports(&good).len(), 2);
        assert!(app.state.hubs["id:good"].is_empty());
        app.tick_fast();
        assert_eq!(reports(&good).len(), 2);
        // A heartbeat is due once HEARTBEAT has passed since the last attempt.
        app.links[0].last_sent = Some(Instant::now() - HEARTBEAT);
        app.tick_fast();
        assert_eq!(reports(&good).len(), 3);
        // A hub in backoff stays skipped, however much is new.
        app.links[0].next_try = Some(Instant::now() + Duration::from_secs(60));
        app.links[0].last_sent = None;
        app.tick_fast();
        assert_eq!(reports(&good).len(), 3);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_new_transcript_is_found_by_the_slow_tick_and_hot_after() {
        let home = crate::ingest::tests::tmpdir("newfile");
        let _ = write_transcript(&home);
        let mut app = App::new(cfg(&home, Output::Send));
        app.tick_slow();
        assert_eq!(app.state.files.len(), 1);
        let q = home.join(".claude/projects/-h-Dev-chum/t.jsonl");
        fs::write(&q, "").unwrap();
        // Fast ticks do not list directories.
        app.tick_fast();
        assert_eq!(app.state.files.len(), 1);
        app.tick_slow();
        assert_eq!(app.state.files.len(), 2);
        assert_eq!(app.hot.len(), 2);
        let _ = fs::remove_dir_all(&home);
    }
}
