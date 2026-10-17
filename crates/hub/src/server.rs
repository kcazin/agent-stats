//! `hub serve`: the HTTP routes, report ingestion, the 15-second tick (samples, pile, publishing), publishing on
//! change, the event streams, GitHub contributions and the status heartbeat.

use crate::compute::{self, Pile, Recent};
use crate::config::{Config, Paths, PublishTarget};
use crate::http::{Request, Response};
use crate::json::{Obj, J};
use crate::stream::{Broadcaster, Feed, StreamLimits, Ticket};
use crate::validate::{self, Reject};
use rusqlite::{params, Connection};
use std::net::TcpStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

const DASHBOARD: &str = include_str!("dashboard.html");
/// The relay and `/stats` flag a payload older than this as stale.
const STALE_MS: f64 = 120_000.0;
/// After an accepted report, wait this long for others to arrive with it...
const COALESCE: Duration = Duration::from_millis(100);
/// ...and act on changes at most once per this. With publishing on change, it is also the least gap between two
/// publishes to one target.
pub const CHANGE_GAP: Duration = Duration::from_secs(1);
/// `/api/stream` gets `live` at least this often, so time-based states (working, online) age out.
pub const LIVE_EVERY: Duration = Duration::from_secs(15);
/// First line of every stream: how long an EventSource waits before reconnecting.
const RETRY: &[u8] = b"retry: 3000\n\n";

/// The hub's clock. Real time, optionally shifted (`AGENT_STATS_NOW` = the time to start at), or frozen and moved
/// by hand in tests (`AGENT_STATS_TEST_HOOKS=1`, `POST /__test/clock`).
pub struct Clock {
    offset: f64,
    frozen: Mutex<Option<f64>>,
}

fn real_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

impl Clock {
    pub fn real() -> Clock {
        Clock { offset: 0.0, frozen: Mutex::new(None) }
    }
    pub fn from_env(test_hooks: bool) -> Clock {
        let start =
            std::env::var("AGENT_STATS_NOW").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
        match start {
            Some(t) if test_hooks => Clock { offset: 0.0, frozen: Mutex::new(Some(t.floor())) },
            Some(t) => Clock { offset: t.floor() - real_ms(), frozen: Mutex::new(None) },
            None => Clock::real(),
        }
    }
    pub fn now(&self) -> f64 {
        match *self.frozen.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(t) => t,
            None => real_ms() + self.offset,
        }
    }
    pub fn set(&self, t: f64) {
        *self.frozen.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.floor());
    }
}

#[derive(Default, Clone)]
pub struct PublishStatus {
    pub last_ok: Option<f64>,
    pub last_error: Option<String>,
}

/// Runs `gh api graphql …` and returns its stdout. Injectable so tests never call the real `gh`.
pub type GithubFetch = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// One public payload on its way to a target.
struct Job {
    seq: u64,
    at: f64,
    body: Arc<Vec<u8>>,
}

#[derive(Default)]
struct OutboxState {
    /// The newest payload not sent yet; a newer one replaces it.
    pending: Option<Job>,
    /// The `seq` of the last payload sent (or failed).
    done: u64,
}

/// A publish target's queue. One worker thread per target sends from it, so two publishes to the same target are
/// never in flight at once, and payloads that pile up behind a slow target collapse into the newest.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    cv: Condvar,
}

impl Outbox {
    fn put(&self, job: Job) {
        lock(&self.state).pending = Some(job);
        self.cv.notify_all();
    }

    /// Waits until payload `seq` (or a newer one) has been sent, at most until `until`.
    fn wait_done(&self, seq: u64, until: Instant) {
        let mut st = lock(&self.state);
        while st.done < seq {
            let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return };
            st = self.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The worker: sends the newest pending payload, `gap` at least after the previous send started.
    fn run(&self, agent: &ureq::Agent, t: &PublishTarget, gap: Duration, status: &Mutex<Vec<PublishStatus>>, i: usize) {
        let mut started: Option<Instant> = None;
        loop {
            {
                let mut st = lock(&self.state);
                while st.pending.is_none() {
                    st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            if let Some(s) = started {
                std::thread::sleep(gap.saturating_sub(s.elapsed()));
            }
            let Some(job) = lock(&self.state).pending.take() else { continue };
            started = Some(Instant::now());
            let r = publish_one(agent, t, &job.body);
            if let Some(s) = lock(status).get_mut(i) {
                match r {
                    Ok(()) => {
                        s.last_ok = Some(job.at);
                        s.last_error = None;
                    }
                    Err(e) => s.last_error = Some(format!("{} {e}", iso(job.at))),
                }
            }
            let mut st = lock(&self.state);
            st.done = st.done.max(job.seq);
            self.cv.notify_all();
        }
    }
}

/// Reports accepted since the last change-triggered run.
#[derive(Default)]
struct Changes {
    since: Option<Instant>,
}

/// What `/stream` clients were last sent: the payload's time, its `stale` flag, and the document itself.
type PublicDoc = (f64, bool, J);

pub struct Hub {
    pub cfg: Config,
    pub paths: Paths,
    pub hub_id: String,
    pub admin_token: String,
    pub insecure_no_auth: bool,
    pub test_hooks: bool,
    pub clock: Clock,
    pub listen: String,
    pub started: u64,
    db: Mutex<Connection>,
    pile: Mutex<Option<Pile>>,
    recent: Mutex<Recent>,
    publish: Arc<Mutex<Vec<PublishStatus>>>,
    /// One per publish target, same order.
    outboxes: Vec<Arc<Outbox>>,
    /// The latest public payload (as published), for `GET /stats` and `/stream`.
    latest: Mutex<Option<(f64, String)>>,
    /// Held while a payload is built, stored and queued, so the newest payload is always queued last. Counts payloads.
    compose: Mutex<u64>,
    publish_on_change: bool,
    changes: (Mutex<Changes>, Condvar),
    streams: Arc<Broadcaster>,
    /// What `/stream` clients have (None while there are none); held while one joins or an event goes out.
    public_feed: Mutex<Option<PublicDoc>>,
    /// The `repos` body `/api/stream` clients have; held while one joins or an event goes out.
    private_feed: Mutex<Option<String>>,
    /// The `Arc` this hub lives in (`into_arc`), for the streams that outlive a request.
    me: Weak<Hub>,
    limiter: crate::auth::PairLimiter,
    /// The `new_repo_webhook` worker, if one is configured.
    new_repos: Option<crate::events::Notifier>,
    github: Option<GithubFetch>,
    tick_lock: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Options {
    pub insecure_no_auth: bool,
    pub test_hooks: bool,
    pub clock: Clock,
    pub github: Option<GithubFetch>,
    /// Publish within a second of an accepted report (off for `AGENT_STATS_TICK_MS=0`: only ticks publish).
    pub publish_on_change: bool,
}

impl Hub {
    pub fn open(cfg: Config, paths: Paths, opts: Options) -> Result<Hub, String> {
        let db = crate::db::open(&paths.db()).map_err(|e| format!("{}: {e}", paths.db().display()))?;
        let hub_id = crate::auth::read_or_create(&paths.hub_id(), 16, false)
            .map_err(|e| format!("{}: {e}", paths.hub_id().display()))?;
        let admin_token = crate::auth::read_or_create(&paths.admin_token(), 32, true)
            .map_err(|e| format!("{}: {e}", paths.admin_token().display()))?;
        let pile = Pile::load(&db);
        let n = cfg.publish.len();
        let http: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .build()
            .into();
        let new_repos = cfg.new_repo_webhook.clone().and_then(|url| {
            crate::events::Notifier::start(url, http.clone())
                .map_err(|e| eprintln!("hub: new_repo_webhook disabled, no worker thread: {e}"))
                .ok()
        });
        let publish = Arc::new(Mutex::new(vec![PublishStatus::default(); n]));
        // Ticks alone are 15 s apart; publishing on change spaces publishes to a target by CHANGE_GAP.
        let gap = if opts.publish_on_change { CHANGE_GAP } else { Duration::ZERO };
        let mut outboxes = Vec::new();
        for (i, t) in cfg.publish.iter().enumerate() {
            let o = Arc::new(Outbox::default());
            let (ob, t, agent, st) = (o.clone(), t.clone(), http.clone(), publish.clone());
            std::thread::Builder::new()
                .name("hub-publish".into())
                .spawn(move || ob.run(&agent, &t, gap, &st, i))
                .map_err(|e| format!("publish thread: {e}"))?;
            outboxes.push(o);
        }
        // A hub that takes any number of connections per IP (it sits behind a proxy) takes any number of streams too.
        let per_ip = if cfg.max_connections_per_ip == 0 { 0 } else { StreamLimits::default().per_ip };
        let streams = Broadcaster::start(StreamLimits { per_ip, ..StreamLimits::default() })
            .map_err(|e| format!("stream thread: {e}"))?;
        Ok(Hub {
            listen: cfg.listen.clone(),
            cfg,
            paths,
            hub_id,
            admin_token,
            insecure_no_auth: opts.insecure_no_auth,
            test_hooks: opts.test_hooks,
            clock: opts.clock,
            started: agent_stats_common::now_secs(),
            db: Mutex::new(db),
            pile: Mutex::new(pile),
            recent: Mutex::new(Recent::default()),
            publish,
            outboxes,
            latest: Mutex::new(None),
            compose: Mutex::new(0),
            publish_on_change: opts.publish_on_change,
            changes: Default::default(),
            streams,
            public_feed: Mutex::new(None),
            private_feed: Mutex::new(None),
            me: Weak::new(),
            limiter: Default::default(),
            new_repos,
            github: opts.github,
            tick_lock: Mutex::new(()),
        })
    }

    /// Moves the hub into its `Arc` (streams keep a weak reference to it).
    pub fn into_arc(self) -> Arc<Hub> {
        Arc::new_cyclic(|me| Hub { me: me.clone(), ..self })
    }

    /// Whether any reporter can authenticate yet (enrolled machines, live pairing codes or a legacy token).
    pub fn check_auth_configured(&self) -> Result<(), String> {
        if self.insecure_no_auth || self.cfg.legacy_token.is_some() {
            return Ok(());
        }
        let (machines, codes) =
            crate::auth::auth_counts(&lock(&self.db), self.clock.now()).map_err(|e| e.to_string())?;
        if machines + codes > 0 {
            return Ok(());
        }
        Err("no way for reporters to authenticate: run `agent-stats hub pair` to enroll a machine, set \
             legacy_token_file in hub.toml (or HUB_TOKEN), or pass --insecure-no-auth"
            .into())
    }

    // ---- ingestion ----

    fn ingest(&self, r: &validate::Report, auth: &str) -> rusqlite::Result<()> {
        let now = self.clock.now();
        let today = compute::today(&self.cfg, now);
        let mut new_repos: Vec<(String, Option<String>)> = Vec::new();
        let mut refused = 0usize;
        {
            let mut db = lock(&self.db);
            let tx = db.transaction()?;
            {
                let mut usage = tx.prepare_cached(
                    "INSERT INTO usage VALUES (?, ?, ?, ?, ?, ?)
                       ON CONFLICT DO UPDATE SET tokens = max(tokens, excluded.tokens), output = max(output, excluded.output)",
                )?;
                for d in &r.days {
                    usage.execute(params![r.machine, d.day, d.agent, d.model, d.tokens, d.output])?;
                }
                let mut repo = tx.prepare_cached(
                    "INSERT INTO repos VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET tokens = max(tokens, excluded.tokens)",
                )?;
                let mut remote = tx.prepare_cached(
                    "INSERT INTO repo_remotes VALUES (?, ?, ?, ?)
                       ON CONFLICT DO UPDATE SET remote = excluded.remote, updated = excluded.updated",
                )?;
                let mut meta = tx.prepare_cached(
                    "INSERT OR IGNORE INTO repo_meta (repo, first_seen, first_day, first_machine, remote) VALUES (?, ?, ?, ?, ?)",
                )?;
                // A report may add only so many repositories the hub has never seen; rows of the rest are dropped.
                let mut known = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM repo_meta WHERE repo = ?)")?;
                let mut verdict: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
                let mut added = 0usize;
                for d in &r.repos {
                    let accept = match verdict.get(d.repo.as_str()) {
                        Some(a) => *a,
                        None => {
                            let old: bool = known.query_row(params![d.repo], |row| row.get(0))?;
                            let a = old || added < crate::events::MAX_NEW_PER_REPORT;
                            added += usize::from(!old && a);
                            refused += usize::from(!a);
                            verdict.insert(&d.repo, a);
                            a
                        }
                    };
                    if !accept {
                        continue;
                    }
                    repo.execute(params![r.machine, d.day, d.repo, d.tokens])?;
                    if let Some(rm) = &d.remote {
                        remote.execute(params![r.machine, d.repo, rm, now as i64])?;
                    }
                    if meta.execute(params![d.repo, now as i64, d.day, r.machine, d.remote])? > 0 {
                        new_repos.push((d.repo.clone(), d.remote.clone()));
                    }
                }
                tx.execute(
                    "INSERT INTO machines VALUES (?, ?, ?, ?, ?)
                       ON CONFLICT DO UPDATE SET label = excluded.label, platform = excluded.platform,
                         last_seen = excluded.last_seen, sessions = excluded.sessions",
                    params![r.machine, r.label, r.platform, now as i64, J::Arr(r.sessions.clone()).stringify()],
                )?;
                tx.execute(
                    "INSERT INTO machine_meta VALUES (?, ?, ?, ?, ?)
                       ON CONFLICT DO UPDATE SET version = coalesce(excluded.version, version),
                         protocol = excluded.protocol, auth = excluded.auth, updated = excluded.updated",
                    params![r.machine, r.version, r.protocol, auth, now as i64],
                )?;
                if auth == "token" {
                    tx.execute(
                        "UPDATE machine_tokens SET last_seen = ? WHERE machine = ?",
                        params![now as i64, r.machine],
                    )?;
                }
            }
            tx.commit()?;
        }
        if refused > 0 {
            eprintln!(
                "hub: {} sent {refused} more unseen repositories than the {} allowed per report; their rows were dropped",
                r.machine,
                crate::events::MAX_NEW_PER_REPORT
            );
        }
        if !new_repos.is_empty() {
            crate::events::announce(&self.cfg, self.new_repos.as_ref(), r, now, &today, new_repos);
        }
        Ok(())
    }

    fn report(&self, req: &Request) -> Response {
        if req.method != "POST" {
            return Response::error(405, "method not allowed");
        }
        // Who is this? The legacy shared token (any machine), a machine token (only its machine), or nobody.
        let token = req.bearer();
        let mut token_machine = None;
        let auth = match token {
            _ if self.insecure_no_auth && token.is_none() => "none",
            Some(t)
                if self
                    .cfg
                    .legacy_token
                    .as_deref()
                    .is_some_and(|l| agent_stats_common::ct_eq(t.as_bytes(), l.as_bytes())) =>
            {
                "legacy"
            }
            Some(t) => match crate::auth::machine_of_token(&lock(&self.db), t) {
                Ok(Some(m)) => {
                    token_machine = Some(m);
                    "token"
                }
                Ok(None) if self.insecure_no_auth => "none",
                Ok(None) => return Response::error(401, "unauthorized"),
                Err(e) => return Response::error(500, &e.to_string()),
            },
            None => return Response::error(401, "unauthorized"),
        };
        let body = match std::str::from_utf8(&req.body).ok().map(J::parse) {
            Some(Ok(b)) => b,
            _ => return Response::error(422, "body is not JSON"),
        };
        let Some(window) = validate::DayWindow::around(&compute::today(&self.cfg, self.clock.now())) else {
            return Response::error(500, "the hub's clock is out of range");
        };
        let r = match validate::report(&body, &window) {
            Ok(r) => r,
            Err(Reject::Protocol(p)) => {
                let supported: Vec<J> = validate::SUPPORTED_PROTOCOLS.iter().map(|p| J::from(*p as i64)).collect();
                return Response::json(
                    426,
                    J::Obj(
                        Obj::new().with("error", format!("protocol {p} not supported")).with("supported", supported),
                    )
                    .stringify(),
                );
            }
            Err(Reject::Invalid(why)) => return Response::error(422, &why),
        };
        if let Some(m) = token_machine {
            if m != r.machine {
                return Response::error(403, &format!("this token reports for {m}, not {}", r.machine));
            }
        }
        // The shared token must not speak for a machine that has its own (enrolled, or revoked on purpose).
        if auth == "legacy" {
            match crate::auth::is_enrolled(&lock(&self.db), &r.machine) {
                Ok(false) => {}
                Ok(true) => {
                    return Response::error(
                        403,
                        &format!("{} has its own token; the shared legacy token can't report for it", r.machine),
                    )
                }
                Err(e) => return Response::error(500, &e.to_string()),
            }
        }
        match self.ingest(&r, auth) {
            Ok(()) => {
                self.note_change();
                Response::json(200, r#"{"ok":true}"#)
            }
            Err(e) => {
                eprintln!("hub: report from {}: {e}", r.machine);
                Response::error(500, &e.to_string())
            }
        }
    }

    fn pair(&self, req: &Request) -> Response {
        if req.method != "POST" {
            return Response::error(405, "method not allowed");
        }
        if !self.limiter.reserve(req.peer) {
            return Response::error(429, "too many failed pairing attempts; wait a few minutes");
        }
        let body = std::str::from_utf8(&req.body).ok().and_then(|s| J::parse(s).ok()).unwrap_or(J::Null);
        let code = body.get("code").and_then(J::as_str).unwrap_or("");
        let machine = body.get("machine").and_then(J::as_str).unwrap_or("");
        if machine.is_empty() || machine.chars().count() > 128 || machine.chars().any(validate::is_unsafe_char) {
            self.limiter.refund(req.peer); // no code was tried
            return Response::error(422, "machine is missing or invalid");
        }
        let label = body.get("label").and_then(J::as_str).map(|s| validate::clean(s, 128)).filter(|s| !s.is_empty());
        let version = body.get("version").and_then(J::as_str).map(|v| validate::clean(v, 64));
        let now = self.clock.now();
        let res = crate::auth::redeem(&mut lock(&self.db), code, machine, label.as_deref(), now);
        match res {
            Ok(token) => {
                self.limiter.refund(req.peer);
                eprintln!("hub: paired {machine}{}", label.as_deref().map(|l| format!(" ({l})")).unwrap_or_default());
                let _ = lock(&self.db).execute(
                    "INSERT INTO machine_meta (machine, version, protocol, auth, updated) VALUES (?, ?, NULL, 'token', ?)
                       ON CONFLICT DO UPDATE SET version = coalesce(excluded.version, version), auth = 'token'",
                    params![machine, version, now as i64],
                );
                Response::json(
                    200,
                    crate::auth::pair_answer(&self.hub_id, &self.cfg.name, &token, &self.cfg.routes(), &self.cfg.tz_name)
                        .stringify(),
                )
            }
            Err(crate::auth::PairError::BadCode) => {
                                Response::error(401, "unknown, used or expired code")
            }
            Err(crate::auth::PairError::Conflict) => Response::error(
                409,
                &format!(
                    "{machine} is already enrolled here; ask the hub admin to run `agent-stats hub pair --replace {machine}` \
                     and join with that code"
                ),
            ),
            Err(crate::auth::PairError::WrongMachine(m)) => {
                Response::error(409, &format!("this code is only for the machine named {m}"))
            }
            Err(crate::auth::PairError::Db(e)) => {
                self.limiter.refund(req.peer);
                Response::error(500, &e.to_string())
            }
        }
    }

    // ---- reading ----

    pub fn public_payload(&self, now: f64) -> rusqlite::Result<Obj> {
        let pile = lock(&self.pile).clone();
        compute::public_payload(&lock(&self.db), &self.cfg, pile.as_ref(), now)
    }

    fn publish_json(&self) -> (J, J) {
        let st = lock(&self.publish).clone();
        let targets: Vec<J> = self
            .cfg
            .publish
            .iter()
            .zip(&st)
            .map(|(t, s)| {
                Obj::new()
                    .with("name", t.name.as_str())
                    .with("url", t.url.as_str())
                    .with("lastOk", s.last_ok)
                    .with("lastError", s.last_error.clone())
                    .into()
            })
            .collect();
        // The dashboard (and hub.mjs) show one target: the first, or "Publishing off".
        let first = match targets.first() {
            Some(t) => t.clone(),
            None => Obj::new().with("url", "").with("lastOk", J::Null).with("lastError", J::Null).into(),
        };
        (first, J::Arr(targets))
    }

    pub fn internal_state(&self, now: f64) -> rusqlite::Result<Obj> {
        let (first, all) = self.publish_json();
        compute::internal_state(&lock(&self.db), &self.cfg, now, first, all)
    }

    /// Every repository the hub knows, with remote, project, tokens and machines.
    pub fn repos(&self, now: f64) -> rusqlite::Result<Vec<Obj>> {
        repos(&lock(&self.db), &self.cfg, now)
    }

    // ---- the tick ----

    pub fn tick(&self) {
        let _one = lock(&self.tick_lock);
        let now = self.clock.now();
        let res: rusqlite::Result<Option<u64>> = (|| {
            let db = lock(&self.db);
            let l = compute::live(&db, now)?;
            {
                let mut pile = lock(&self.pile);
                compute::update_pile(&db, &self.cfg, &mut pile, now)?;
            }
            db.execute(
                "INSERT OR REPLACE INTO samples (at, working, open, subagents, machines, by_agent) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    now as i64,
                    l.working as i64,
                    l.open as i64,
                    l.subagents as i64,
                    l.machines.iter().filter(|m| m.online).count() as i64,
                    J::Obj(l.by_agent.clone()).stringify()
                ],
            )?;
            db.execute("DELETE FROM samples WHERE at < ?", params![(now - 30.0 * 86_400_000.0) as i64])?;
            drop(db);
            self.compose(now, true)
        })();
        let seq = match res {
            Ok(Some(seq)) => seq,
            Ok(None) => return,
            Err(e) => {
                eprintln!("hub: tick failed: {e}");
                return;
            }
        };
        // Like before the publish queues: the tick ends once its payload is out (or failed) everywhere, so
        // `/__test/tick` answers after the publish and the status reflect it.
        for (o, t) in self.outboxes.iter().zip(&self.cfg.publish) {
            o.wait_done(seq, Instant::now() + t.timeout + CHANGE_GAP + Duration::from_secs(1));
        }
    }

    /// Builds the public payload at `now`, makes it the latest (`/stats`, `/stream`) and queues it for every publish
    /// target; returns its sequence number. On the tick (`tick`) the payload's recent-activity window moves on; between
    /// ticks it repeats the last tick's, so `projects.last`, `lastMs` and `flow` keep their meaning.
    fn compose(&self, now: f64, tick: bool) -> rusqlite::Result<Option<u64>> {
        if self.cfg.publish.is_empty() && !self.cfg.public {
            return Ok(None);
        }
        let mut seq = lock(&self.compose);
        let mut d = self.public_payload(now)?;
        if tick {
            lock(&self.recent).with_last(&mut d);
        } else {
            lock(&self.recent).carry_last(&mut d);
        }
        let body = J::Obj(d).stringify();
        *lock(&self.latest) = Some((now, body.clone()));
        *seq += 1;
        let body = Arc::new(body.into_bytes());
        for o in &self.outboxes {
            o.put(Job { seq: *seq, at: now, body: body.clone() });
        }
        self.feed_public();
        Ok(Some(*seq))
    }

    // ---- changes and streams ----

    /// An accepted report: wake the change thread.
    fn note_change(&self) {
        let (m, cv) = &self.changes;
        lock(m).since.get_or_insert_with(Instant::now);
        cv.notify_all();
    }

    /// Waits until reports are waiting to be acted on (true; at least COALESCE after the first, CHANGE_GAP after
    /// `last`, the previous run) or until `until` (false).
    pub fn wait_change(&self, last: Option<Instant>, until: Instant) -> bool {
        let (m, cv) = &self.changes;
        let mut st = lock(m);
        loop {
            let now = Instant::now();
            let due = st.since.map(|s| {
                let due = s + COALESCE;
                last.map_or(due, |l| due.max(l + CHANGE_GAP))
            });
            if due.is_some_and(|d| d <= now) {
                st.since = None;
                return true;
            }
            let wake = due.map_or(until, |d| d.min(until));
            let Some(left) = wake.checked_duration_since(now).filter(|d| !d.is_zero()) else { return false };
            st = cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// After reports: publish (if on) and tell the streams. Samples, the pile and the recent-activity window are left
    /// to the tick.
    pub fn on_change(&self) {
        if self.publish_on_change {
            if let Err(e) = self.compose(self.clock.now(), false) {
                eprintln!("hub: publish on change failed: {e}");
            }
        }
        self.feed_private();
    }

    /// `/stream` clients: the latest payload as a document, with `"stale": true` once it is old (as the relay's: no
    /// `stale` member while fresh, so clearing it is the patch `{"stale":null}`).
    fn public_doc(&self) -> Option<PublicDoc> {
        let (at, body) = lock(&self.latest).clone()?;
        let stale = self.clock.now() - at > STALE_MS;
        match J::parse(&body) {
            Ok(J::Obj(mut o)) => {
                if stale {
                    o.set("stale", J::Bool(true));
                }
                Some((at, stale, J::Obj(o)))
            }
            _ => None,
        }
    }

    /// The event that takes `/stream` clients from `prev` to `doc`: a patch, the full document when a patch can't say
    /// it, nothing when nothing changed.
    fn public_event(prev: Option<&J>, doc: &J) -> Option<Vec<u8>> {
        match prev.map(|p| crate::json::merge_patch(p, doc)) {
            Some(Some(J::Obj(p))) if p.0.is_empty() => None,
            Some(Some(p)) => Some(crate::stream::event("patch", &p.stringify())),
            _ => Some(crate::stream::event("stats", &doc.stringify())),
        }
    }

    /// Sends `/stream` clients what changed since their document: a new payload, or `stale` flipping. Cheap when
    /// neither happened (called every second).
    pub fn feed_public(&self) {
        if !self.cfg.public {
            return;
        }
        let mut feed = lock(&self.public_feed);
        if self.streams.count(Feed::Public) == 0 {
            *feed = None;
            return;
        }
        if let (Some((at, stale, _)), Some((now_at, _))) = (feed.as_ref(), lock(&self.latest).as_ref()) {
            if at == now_at && *stale == (self.clock.now() - at > STALE_MS) {
                return;
            }
        }
        let Some(doc) = self.public_doc() else { return };
        if let Some(ev) = Hub::public_event(feed.as_ref().map(|f| &f.2), &doc.2) {
            self.streams.send(Feed::Public, &ev);
        }
        *feed = Some(doc);
    }

    /// `/api/stream`'s two events: `live` (`now` and `sessions` of `/api/state`) and `repos` (`/api/repos`).
    fn private_events(&self) -> rusqlite::Result<(Vec<u8>, String)> {
        let now = self.clock.now();
        let db = lock(&self.db);
        let l = compute::live(&db, now)?;
        let live = Obj::new().with("now", now).with("sessions", J::Arr(l.sessions.into_iter().map(J::Obj).collect()));
        let repos = J::Arr(repos(&db, &self.cfg, now)?.into_iter().map(J::Obj).collect()).stringify();
        Ok((crate::stream::event("live", &J::Obj(live).stringify()), repos))
    }

    /// Sends `/api/stream` clients `live`, and `repos` if it changed. After reports and every LIVE_EVERY.
    pub fn feed_private(&self) {
        let mut feed = lock(&self.private_feed);
        if self.streams.count(Feed::Private) == 0 {
            *feed = None;
            return;
        }
        match self.private_events() {
            Ok((live, repos)) => {
                self.streams.send(Feed::Private, &live);
                if feed.as_deref() != Some(repos.as_str()) {
                    self.streams.send(Feed::Private, &crate::stream::event("repos", &repos));
                    *feed = Some(repos);
                }
            }
            Err(e) => eprintln!("hub: stream events failed: {e}"),
        }
    }

    /// Hands a stream's socket to the broadcaster with its first events, under its feed's lock so it neither misses
    /// nor reorders one.
    fn join(&self, sock: TcpStream, feed: Feed, ticket: Ticket) {
        let mut first = vec![RETRY.to_vec()];
        match feed {
            Feed::Public => {
                let mut f = lock(&self.public_feed);
                let doc = self.public_doc();
                if let Some((_, _, d)) = &doc {
                    first.push(crate::stream::event("stats", &d.stringify()));
                }
                // Everyone on the feed must be at the same document; a newer one goes to the others first.
                if let (Some(old), Some(new)) = (f.as_ref(), doc.as_ref()) {
                    if let Some(ev) = Hub::public_event(Some(&old.2), &new.2) {
                        self.streams.send(Feed::Public, &ev);
                    }
                }
                if doc.is_some() {
                    *f = doc;
                }
                self.streams.join(sock, feed, ticket, &first);
            }
            Feed::Private => {
                let mut f = lock(&self.private_feed);
                let (live, repos) = match self.private_events() {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("hub: stream events failed: {e}");
                        return; // closes it; the client retries
                    }
                };
                if f.as_ref().is_some_and(|r| *r != repos) {
                    self.streams.send(Feed::Private, &crate::stream::event("repos", &repos));
                }
                first.push(live);
                first.push(crate::stream::event("repos", &repos));
                *f = Some(repos);
                self.streams.join(sock, feed, ticket, &first);
            }
        }
    }

    /// Open streams, both feeds.
    pub fn streams_open(&self) -> usize {
        self.streams.count(Feed::Private) + self.streams.count(Feed::Public)
    }

    /// `GET /api/stream` (auth checked by the caller) or `GET /stream`: a place under the stream cap, then the
    /// response head; the socket then goes to `join`.
    fn open_stream(&self, req: &Request, feed: Feed) -> Response {
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        // Like the reserved connection slots: the admin, and loopback or `trusted_networks` peers not behind a proxy.
        let privileged = self.is_admin(req)
            || (req.header("x-forwarded-for").is_none() && (req.peer.is_loopback() || self.cfg.peer_trusted(req.peer)));
        let Some(ticket) = self.streams.ticket(req.peer, privileged) else {
            return Response::error(503, "too many streams");
        };
        let me = self.me.clone();
        Response::stream("text/event-stream", move |sock| {
            if let Some(hub) = me.upgrade() {
                hub.join(sock, feed, ticket);
            }
        })
        .header("cache-control", "no-store")
        .header("x-accel-buffering", "no")
        .header("transfer-encoding", "chunked")
    }

    pub fn fetch_github(&self) {
        let (Some(user), Some(fetch)) = (self.cfg.github_user.as_deref(), self.github.as_ref()) else { return };
        let res = fetch(user).and_then(|out| parse_github(&out));
        match res {
            Ok(days) => {
                let mut db = lock(&self.db);
                let r: rusqlite::Result<()> = (|| {
                    let tx = db.transaction()?;
                    {
                        let mut ins = tx.prepare(
                            "INSERT INTO github VALUES (?, ?) ON CONFLICT DO UPDATE SET count = excluded.count",
                        )?;
                        for (d, n) in days {
                            ins.execute(params![d, n])?;
                        }
                    }
                    tx.commit()
                })();
                if let Err(e) = r {
                    eprintln!("hub: github store failed: {e}");
                }
            }
            Err(e) => eprintln!("hub: github fetch failed: {e}"),
        }
    }

    pub fn status(&self) -> agent_stats_common::Status {
        let now = self.clock.now();
        let online = lock(&self.db)
            .query_row(
                "SELECT count(*) FROM machines WHERE ? - last_seen < ?",
                params![now as i64, compute::ONLINE as i64],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let (_, targets) = self.publish_json();
        agent_stats_common::Status {
            role: "hub".into(),
            version: agent_stats_common::VERSION.into(),
            pid: std::process::id(),
            started_at: self.started,
            heartbeat_at: agent_stats_common::now_secs(),
            detail: serde_json::from_str(
                &J::Obj(
                    Obj::new()
                        .with("hubId", self.hub_id.as_str())
                        .with("name", self.cfg.name.as_str())
                        .with("listen", self.listen.as_str())
                        .with("machinesOnline", online)
                        .with("publish", targets),
                )
                .stringify(),
            )
            .unwrap_or_default(),
        }
    }

    // ---- routing ----

    pub(crate) fn is_admin(&self, req: &Request) -> bool {
        // The TCP peer only; a forwarded-for header means a proxy is in between, so neither rule applies.
        // Token-less access also needs a `Host` that is really this hub (DNS rebinding); otherwise the token rules.
        if req.header("x-forwarded-for").is_none()
            && ((self.cfg.trust_loopback && req.peer.is_loopback()) || self.cfg.peer_trusted(req.peer))
            && crate::limits::host_allowed(&self.cfg, req.header("host"))
        {
            return true;
        }
        let t = req.bearer().or_else(|| req.cookie("agent_stats_admin"));
        t.is_some_and(|t| agent_stats_common::ct_eq(t.as_bytes(), self.admin_token.as_bytes()))
    }

    /// Called with the request head, before the body is read: its size cap, and for `/report` a cheap check that
    /// the bearer token is known at all (the full check, including which machine it may report for, runs in `report`).
    pub fn admit(&self, req: &Request) -> crate::http::Admit {
        use crate::http::Admit;
        use crate::limits::{OTHER_BODY, PAIR_BODY};
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/report") => {
                if !self.token_known(req) {
                    return Admit::Reject(Response::error(401, "unauthorized"));
                }
                Admit::Read { max: crate::http::MAX_BODY, heavy: true }
            }
            ("POST", "/pair") => Admit::Read { max: PAIR_BODY, heavy: false },
            _ => Admit::Read { max: OTHER_BODY, heavy: false },
        }
    }

    fn token_known(&self, req: &Request) -> bool {
        let token = req.bearer();
        if self.insecure_no_auth {
            return true;
        }
        let Some(t) = token else { return false };
        if self.cfg.legacy_token.as_deref().is_some_and(|l| agent_stats_common::ct_eq(t.as_bytes(), l.as_bytes())) {
            return true;
        }
        // A database error is left for `report` to answer.
        crate::auth::machine_of_token(&lock(&self.db), t).map_or(true, |m| m.is_some())
    }

    /// A machine or legacy token that is certainly valid; unlike [`Self::token_known`], a database error says no.
    pub(crate) fn token_valid(&self, req: &Request) -> bool {
        if self.insecure_no_auth {
            return true;
        }
        let Some(t) = req.bearer() else { return false };
        if self.cfg.legacy_token.as_deref().is_some_and(|l| agent_stats_common::ct_eq(t.as_bytes(), l.as_bytes())) {
            return true;
        }
        matches!(crate::auth::machine_of_token(&lock(&self.db), t), Ok(Some(_)))
    }

    fn cors(&self, req: &Request, res: Response) -> Response {
        let res = res.header("vary", "Origin");
        match req.header("origin") {
            Some(o) if self.cfg.cors_origins.iter().any(|c| c == o) => res.header("access-control-allow-origin", o),
            _ => res,
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        let json = |r: rusqlite::Result<Obj>| match r {
            Ok(o) => Response::json(200, J::Obj(o).stringify()),
            Err(e) => Response::error(500, &e.to_string()),
        };
        let path = req.path.as_str();
        match path {
            "/report" => self.report(&req),
            "/pair" => self.pair(&req),
            "/release" => self.release(&req),
            p if p.starts_with("/release/") => self.release_file(&req),
            agent_stats_common::WELL_KNOWN => Response::json(
                200,
                J::Obj(
                    Obj::new()
                        .with("role", "hub")
                        .with("hubId", self.hub_id.as_str())
                        .with("name", self.cfg.name.as_str())
                        .with("version", agent_stats_common::VERSION)
                        .with("protocol", agent_stats_common::PROTOCOL as i64)
                        .with("routes", J::Arr(self.cfg.routes().into_iter().map(J::Str).collect()))
                        .with("timezone", self.cfg.tz_name.as_str()),
                )
                .stringify(),
            ),
            "/stats" if self.cfg.public => {
                if req.method == "OPTIONS" {
                    return self.cors(
                        &req,
                        Response::new(204, "text/plain", "").header("access-control-allow-methods", "GET"),
                    );
                }
                let latest = lock(&self.latest).clone();
                let res = match latest {
                    None => Response::new(503, "text/plain", "no data yet"),
                    Some((at, body)) => match J::parse(&body) {
                        Ok(J::Obj(mut o)) => {
                            o.set("stale", J::Bool(self.clock.now() - at > STALE_MS));
                            Response::json(200, J::Obj(o).stringify()).header("cache-control", "public, max-age=5")
                        }
                        _ => Response::error(500, "bad snapshot"),
                    },
                };
                self.cors(&req, res)
            }
            "/stream" if self.cfg.public => {
                if req.method == "OPTIONS" {
                    return self.cors(
                        &req,
                        Response::new(204, "text/plain", "").header("access-control-allow-methods", "GET"),
                    );
                }
                self.cors(&req, self.open_stream(&req, Feed::Public))
            }
            "/api/public" if self.cfg.public || self.is_admin(&req) => json(self.public_payload(self.clock.now())),
            "/api/state" if self.is_admin(&req) => json(self.internal_state(self.clock.now())),
            "/api/stream" if self.is_admin(&req) => self.open_stream(&req, Feed::Private),
            "/api/repos" if self.is_admin(&req) => match self.repos(self.clock.now()) {
                Ok(v) => Response::json(200, J::Arr(v.into_iter().map(J::Obj).collect()).stringify()),
                Err(e) => Response::error(500, &e.to_string()),
            },
            "/" | "/index.html" => {
                // `/?token=<admin token>` once sets a cookie, so the dashboard's own fetches carry it.
                if let Some(t) = req.query_param("token") {
                    if agent_stats_common::ct_eq(t.as_bytes(), self.admin_token.as_bytes()) {
                        return Response::new(303, "text/plain", "")
                            .header("location", "/")
                            .header("set-cookie", format!("agent_stats_admin={t}; HttpOnly; SameSite=Strict; Path=/"));
                    }
                }
                if self.is_admin(&req) {
                    Response::new(200, "text/html; charset=utf-8", DASHBOARD)
                } else {
                    Response::new(
                        401,
                        "text/plain; charset=utf-8",
                        "This dashboard shows private paths. Open it from the hub itself, or as /?token=<admin token> \
                         (secrets/hub-admin.token in the hub's config directory).\n",
                    )
                }
            }
            "/api/state" | "/api/repos" | "/api/public" | "/api/stream" => Response::error(401, "unauthorized"),
            p if self.test_hooks && p.starts_with("/__test/") && req.peer.is_loopback() => self.test_hook(&req),
            _ => Response::error(404, "not found"),
        }
    }

    fn test_hook(&self, req: &Request) -> Response {
        match req.path.as_str() {
            "/__test/clock" => {
                let t = std::str::from_utf8(&req.body)
                    .ok()
                    .and_then(|s| J::parse(s).ok())
                    .and_then(|b| b.get("now").and_then(J::as_f64));
                match t {
                    Some(t) => {
                        self.clock.set(t);
                        Response::json(200, r#"{"ok":true}"#)
                    }
                    None => Response::error(422, "now"),
                }
            }
            "/__test/tick" => {
                self.tick();
                Response::json(200, r#"{"ok":true}"#)
            }
            "/__test/github" => {
                self.fetch_github();
                Response::json(200, r#"{"ok":true}"#)
            }
            _ => Response::error(404, "not found"),
        }
    }
}

fn publish_one(agent: &ureq::Agent, t: &PublishTarget, body: &[u8]) -> Result<(), String> {
    let res = agent
        .post(&t.url)
        .config()
        .timeout_global(Some(t.timeout))
        .build()
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", t.token))
        .send(body)
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let _ = res.into_body().read_to_vec();
    if !(200..300).contains(&status) {
        return Err(format!("relay answered {status}"));
    }
    Ok(())
}

/// `new Date(ms).toISOString()`
pub fn iso(ms: f64) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|_| "Invalid Date".into())
}

/// `data.user.contributionsCollection.contributionCalendar.weeks[].contributionDays[]` → (date, count).
pub fn parse_github(out: &str) -> Result<Vec<(String, i64)>, String> {
    let v = J::parse(out).map_err(|e| e.to_string())?;
    let weeks = ["data", "user", "contributionsCollection", "contributionCalendar", "weeks"]
        .iter()
        .try_fold(&v, |v, k| v.get(k))
        .and_then(J::as_arr)
        .ok_or("unexpected answer from GitHub")?;
    let mut days = Vec::new();
    for w in weeks {
        for d in w.get("contributionDays").and_then(J::as_arr).into_iter().flatten() {
            let (Some(date), Some(n)) =
                (d.get("date").and_then(J::as_str), d.get("contributionCount").and_then(J::as_f64))
            else {
                continue;
            };
            if validate::is_day(date) && (0.0..1e9).contains(&n) {
                days.push((date.to_string(), n as i64));
            }
        }
    }
    Ok(days)
}

/// The default GitHub fetch: `gh api graphql` (the gh CLI's login), 30 s timeout. `$AGENT_STATS_GH` overrides
/// the `gh` binary.
pub fn gh_fetch() -> GithubFetch {
    Arc::new(|user: &str| {
        let query = "query($u: String!) { user(login: $u) { contributionsCollection {
    contributionCalendar { weeks { contributionDays { date contributionCount } } } } } }";
        let gh = std::env::var("AGENT_STATS_GH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "gh".into());
        let mut child = std::process::Command::new(gh)
            .args(["api", "graphql", "-f", &format!("query={query}"), "-f", &format!("u={user}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = std::thread::Builder::new().name("hub-gh".into()).spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
            s
        });
        let reader = match reader {
            Ok(r) => r,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("gh reader thread: {e}"));
            }
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break s;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("gh timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = reader.join().unwrap_or_default();
        if !status.success() {
            return Err(format!("gh exited with {status}"));
        }
        Ok(out)
    })
}

/// Every repository, with remote, project (null = unassigned), tokens today and all time, first seen, machines.
pub fn repos(db: &Connection, cfg: &Config, now: f64) -> rusqlite::Result<Vec<Obj>> {
    let today = compute::today(cfg, now);
    let mut st = db.prepare(
        "SELECT r.repo, total(r.tokens), total(CASE WHEN r.day = ?1 THEN r.tokens ELSE 0 END), min(r.day),
                group_concat(DISTINCT r.machine), m.first_seen, m.first_machine,
                coalesce((SELECT remote FROM repo_remotes x WHERE x.repo = r.repo ORDER BY updated DESC LIMIT 1), m.remote)
           FROM repos r LEFT JOIN repo_meta m ON m.repo = r.repo
          WHERE r.repo IS NOT NULL
          GROUP BY r.repo ORDER BY total(r.tokens) DESC",
    )?;
    let rows = st.query_map(params![today], |r| {
        let repo: String = r.get(0)?;
        let remote: Option<String> = r.get(7)?;
        let machines: Vec<J> = r
            .get::<_, Option<String>>(4)?
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(J::from)
            .collect();
        let project = cfg.project_of(Some(&repo), remote.as_deref()).map(|p| p.key.clone());
        Ok(Obj::new()
            .with("repo", repo)
            .with("remote", remote)
            .with("project", project)
            .with("tokensToday", crate::db::num(r, 2)?.unwrap_or(0.0))
            .with("tokensAll", crate::db::num(r, 1)?.unwrap_or(0.0))
            .with("firstDay", r.get::<_, Option<String>>(3)?)
            .with("firstSeen", r.get::<_, Option<i64>>(5)?)
            .with("firstMachine", r.get::<_, Option<String>>(6)?)
            .with("machines", machines))
    })?;
    rows.collect()
}
