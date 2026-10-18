//! A hub as a replica of a fleet (docs/FLEET.md): its own member row, introducing itself to the other replicas,
//! pulling their changes, which replica publishes, the synced fleet-wide settings (`fleet.toml`), and join requests.

use crate::auth::sha256_hex;
use crate::config::{Config, Paths, FLEET_KEYS};
use crate::json::{Obj, J};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A replica not heard from for this long no longer counts for publishing.
pub const ALIVE_MS: f64 = 600_000.0;
/// How often a reachable peer is pulled.
pub const PULL_EVERY: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HELLO_EVERY: Duration = Duration::from_secs(300);
const REPROBE: Duration = Duration::from_secs(300);
/// Rows per `/sync/changes` page, and pages per peer per round.
pub const PAGE: usize = 2000;
pub const PAGES: usize = 25;
/// How long a join request waits for someone to approve it.
pub const REQUEST_TTL_MS: f64 = 600_000.0;

/// One replica's view of another.
#[derive(Default, Debug, Clone)]
pub struct PeerState {
    /// Learned from its descriptor (a seed route doesn't say whose it is until asked).
    pub node: Option<String>,
    pub route: Option<String>,
    probed: Option<Instant>,
    next_try: Option<Instant>,
    backoff: Duration,
    hello: Option<Instant>,
    pub last_error: Option<String>,
}

/// A member row as stored: routes, version, always_on, token_hash, revoked.
type MemberRow = (Option<String>, Option<String>, Option<i64>, Option<String>, Option<i64>);
/// A join request as `decide` reads it: machine, label, token hash, view hash, status, expires.
type RequestRow = (String, Option<String>, String, Option<String>, String, i64);

/// The routes stored in a member row, as a list.
pub fn routes_of(json: Option<&str>) -> Vec<String> {
    json.and_then(|s| J::parse(s).ok())
        .and_then(|j| j.as_arr().map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect()))
        .unwrap_or_default()
}

/// A member row: (node, routes, always_on, revoked).
pub struct Member {
    pub node: String,
    pub routes: Vec<String>,
    pub version: Option<String>,
    pub always_on: bool,
    pub revoked: bool,
    pub last_ok: Option<f64>,
    pub last_error: Option<String>,
}

pub fn members(db: &Connection) -> rusqlite::Result<Vec<Member>> {
    let mut st = db.prepare(
        "SELECT f.node, f.routes, f.version, f.always_on, f.revoked, c.last_ok, c.last_error
           FROM fleet_members f LEFT JOIN sync_cursors c ON c.peer = f.node ORDER BY f.node",
    )?;
    let rows = st.query_map([], |r| {
        Ok(Member {
            node: r.get(0)?,
            routes: routes_of(r.get::<_, Option<String>>(1)?.as_deref()),
            version: r.get(2)?,
            always_on: r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
            revoked: r.get::<_, Option<i64>>(4)?.is_some(),
            last_ok: r.get::<_, Option<i64>>(5)?.map(|v| v as f64),
            last_error: r.get(6)?,
        })
    })?;
    rows.collect()
}

/// The replica that publishes: among `me` and the members heard from in the last ten minutes, always-on ones first,
/// then by name. Every replica works it out from what it has heard, so they agree while they can reach each other.
pub fn publisher(list: &[Member], me: &str, me_always_on: bool, now: f64) -> String {
    let mut cands: Vec<(bool, &str)> = vec![(me_always_on, me)];
    for m in list {
        if m.node != me && !m.revoked && m.last_ok.is_some_and(|t| now - t < ALIVE_MS) {
            cands.push((m.always_on, &m.node));
        }
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    cands[0].1.to_string()
}

/// Every replica's routes, always-on ones first, for reporters to learn (`fleetRoutes` in the descriptor).
pub fn fleet_routes(list: &[Member], me: &str, me_always_on: bool, mine: &[String]) -> Vec<String> {
    let mut nodes: Vec<(bool, &str, Vec<String>)> = vec![(me_always_on, me, mine.to_vec())];
    for m in list.iter().filter(|m| m.node != me && !m.revoked) {
        nodes.push((m.always_on, &m.node, m.routes.clone()));
    }
    nodes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let mut out: Vec<String> = Vec::new();
    for r in nodes.into_iter().flat_map(|n| n.2) {
        if !out.iter().any(|o| o.trim_end_matches('/').eq_ignore_ascii_case(r.trim_end_matches('/'))) {
            out.push(r);
        }
    }
    out
}

/// Writes this replica's member row when something in it changed (routes, version, preference, token).
pub fn ensure_self(db: &Connection, cfg: &Config, token_hash: &str, now: f64) -> rusqlite::Result<()> {
    let Some(f) = &cfg.fleet else { return Ok(()) };
    let routes = J::Arr(cfg.routes().into_iter().map(J::Str).collect()).stringify();
    let here: Option<MemberRow> = db
        .query_row(
            "SELECT routes, version, always_on, token_hash, revoked FROM fleet_members WHERE node = ?",
            params![f.node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let want = (Some(routes.clone()), Some(agent_stats_common::VERSION.to_string()), Some(i64::from(f.always_on)));
    if let Some((r, v, a, t, revoked)) = &here {
        // A revoked replica doesn't put itself back; it has to be added again.
        if revoked.is_some() || ((r.clone(), v.clone(), *a) == want && t.as_deref() == Some(token_hash)) {
            return Ok(());
        }
    }
    db.execute(
        "INSERT INTO fleet_members (node, routes, version, always_on, token_hash, updated, revoked)
           VALUES (?, ?, ?, ?, ?, ?, NULL)
           ON CONFLICT (node) DO UPDATE SET routes = excluded.routes, version = excluded.version,
             always_on = excluded.always_on, token_hash = excluded.token_hash, updated = excluded.updated",
        params![f.node, routes, agent_stats_common::VERSION, i64::from(f.always_on), token_hash, now as i64],
    )?;
    Ok(())
}

/// `POST /sync/hello` from `node`: its member row, unless it was revoked.
pub fn hello(db: &Connection, node: &str, body: &J, now: f64) -> Result<(), (u16, String)> {
    let routes: Vec<J> = body
        .get("routes")
        .and_then(J::as_arr)
        .map(|a| a.iter().filter(|r| r.as_str().is_some_and(|s| s.len() < 300)).take(8).cloned().collect())
        .unwrap_or_default();
    let version = body.get("version").and_then(J::as_str).map(|v| crate::validate::clean(v, 64));
    let always_on = body.get("alwaysOn").is_some_and(J::truthy);
    let Some(token_hash) =
        body.get("tokenHash").and_then(J::as_str).filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Err((422, "tokenHash is missing or invalid".into()));
    };
    let db_err = |e: rusqlite::Error| (500, e.to_string());
    let here: Option<MemberRow> = db
        .query_row(
            "SELECT routes, version, always_on, token_hash, revoked FROM fleet_members WHERE node = ?",
            params![node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(db_err)?;
    let routes = J::Arr(routes).stringify();
    match &here {
        Some((.., Some(_))) => return Err((403, format!("{node} was removed from this fleet"))),
        Some((r, v, a, t, None))
            if r.as_deref() == Some(routes.as_str())
                && *v == version
                && *a == Some(i64::from(always_on))
                && t.as_deref() == Some(token_hash) =>
        {
            return Ok(())
        }
        _ => {}
    }
    db.execute(
        "INSERT INTO fleet_members (node, routes, version, always_on, token_hash, updated, revoked)
           VALUES (?, ?, ?, ?, ?, ?, NULL)
           ON CONFLICT (node) DO UPDATE SET routes = excluded.routes, version = excluded.version,
             always_on = excluded.always_on, token_hash = excluded.token_hash, updated = excluded.updated",
        params![node, routes, version, i64::from(always_on), token_hash, now as i64],
    )
    .map_err(db_err)?;
    Ok(())
}

/// The replica a token belongs to (live member rows only).
pub fn node_of_replica_token(db: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    let hash = sha256_hex(token);
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT node, token_hash FROM fleet_members WHERE token_hash = ? AND revoked IS NULL",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.filter(|(_, h)| agent_stats_common::ct_eq(h.as_bytes(), hash.as_bytes())).map(|(n, _)| n))
}

/// Revokes a machine everywhere it counts: its reporting token, and its member row if it is a replica (both sync).
pub fn revoke(db: &Connection, machine: &str, now: f64) -> rusqlite::Result<bool> {
    let token = crate::auth::revoke(db, machine, now)?;
    let member = db.execute(
        "UPDATE fleet_members SET revoked = ?1, updated = ?1 WHERE node = ?2 AND revoked IS NULL",
        params![now as i64, machine],
    )?;
    Ok(token || member > 0)
}

pub fn cursor(db: &Connection, peer: &str) -> rusqlite::Result<(Option<String>, i64)> {
    Ok(db
        .query_row("SELECT db, seq FROM sync_cursors WHERE peer = ?", params![peer], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .unwrap_or((None, 0)))
}

pub fn save_cursor(db: &Connection, peer: &str, dbid: &str, seq: i64, route: &str, now: f64) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO sync_cursors (peer, db, seq, last_ok, last_error, route) VALUES (?1, ?2, ?3, ?4, NULL, ?5)
           ON CONFLICT (peer) DO UPDATE SET db = ?2, seq = ?3, last_ok = ?4, last_error = NULL, route = ?5",
        params![peer, dbid, seq, now as i64, route],
    )?;
    Ok(())
}

pub fn note_error(db: &Connection, peer: &str, err: &str) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO sync_cursors (peer, seq, last_error) VALUES (?1, 0, ?2)
           ON CONFLICT (peer) DO UPDATE SET last_error = ?2",
        params![peer, err],
    )?;
    Ok(())
}

impl PeerState {
    pub fn due(&self, now: Instant) -> bool {
        self.next_try.is_none_or(|t| now >= t)
    }
    pub fn ok(&mut self, now: Instant) {
        self.backoff = Duration::ZERO;
        self.next_try = Some(now + PULL_EVERY);
        self.last_error = None;
    }
    pub fn failed(&mut self, now: Instant, err: String) {
        self.backoff = (self.backoff * 2).clamp(PULL_EVERY, BACKOFF_MAX);
        self.next_try = Some(now + self.backoff);
        self.route = None;
        self.last_error = Some(err);
    }
    pub fn needs_probe(&self, now: Instant) -> bool {
        self.route.is_none() || self.probed.is_none_or(|p| now.duration_since(p) >= REPROBE)
    }
    pub fn probed(&mut self, route: String, node: String, now: Instant) {
        self.route = Some(route);
        self.node = Some(node);
        self.probed = Some(now);
    }
    pub fn hello_due(&self, now: Instant) -> bool {
        self.hello.is_none_or(|h| now.duration_since(h) >= HELLO_EVERY)
    }
    pub fn said_hello(&mut self, now: Instant) {
        self.hello = Some(now);
    }
}

/// The peers to pull this round: every live member but me (by node), and the seed routes from `[fleet] peers` that
/// no member already covers. Seeds are keyed `seed <url>` until they say whose they are.
pub fn peer_list(
    list: &[Member],
    me: &str,
    seeds: &[String],
    known: &HashMap<String, PeerState>,
) -> Vec<(String, Vec<String>)> {
    let same = |a: &str, b: &str| a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'));
    let mut out: Vec<(String, Vec<String>)> = list
        .iter()
        .filter(|m| m.node != me && !m.revoked && !m.routes.is_empty())
        .map(|m| (m.node.clone(), m.routes.clone()))
        .collect();
    for s in seeds {
        let key = format!("seed {s}");
        let covered = out.iter().any(|(_, rs)| rs.iter().any(|r| same(r, s)))
            || known
                .get(&key)
                .and_then(|p| p.node.as_deref())
                .is_some_and(|n| n == me || out.iter().any(|(m, _)| m == n));
        let revoked = known
            .get(&key)
            .and_then(|p| p.node.as_deref())
            .is_some_and(|n| list.iter().any(|m| m.node == n && m.revoked));
        if !covered && !revoked {
            out.push((key, vec![s.clone()]));
        }
    }
    out
}

// ---- fleet-wide settings ----

/// The publish token files a fleet.toml names (relative to the config directory).
fn token_files(text: &str) -> Vec<String> {
    let Ok(t) = toml::from_str::<toml::Table>(text) else { return Vec::new() };
    t.get("publish")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("token_file").and_then(|f| f.as_str()).map(str::to_owned))
        .collect()
}

/// A token file path another replica may write: relative, inside the config directory.
fn safe_rel(p: &str) -> bool {
    let path = std::path::Path::new(p);
    !p.is_empty() && path.is_relative() && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// This replica's fleet.toml and the publish tokens it names: `(text, [(file, token)])`.
pub fn local_settings(paths: &Paths) -> Option<(String, Vec<(String, String)>)> {
    let text = std::fs::read_to_string(paths.fleet_toml()).ok()?;
    let secrets = token_files(&text)
        .into_iter()
        .filter(|f| safe_rel(f))
        .filter_map(|f| agent_stats_common::read_secret(&paths.resolve(&f)).map(|t| (f, t)))
        .collect();
    Some((text, secrets))
}

pub fn settings_hash(text: &str, secrets: &[(String, String)]) -> String {
    let mut s = text.to_string();
    let mut sorted = secrets.to_vec();
    sorted.sort();
    for (f, t) in sorted {
        s.push_str(&format!("\0{f}\0{t}"));
    }
    sha256_hex(&s)
}

fn entry_settings(v: &J) -> Option<(String, Vec<(String, String)>)> {
    let text = v.get("text").and_then(J::as_str)?.to_string();
    let secrets = v
        .get("secrets")
        .and_then(J::as_obj)
        .map(|o| o.0.iter().filter_map(|(f, t)| t.as_str().map(|t| (f.clone(), t.to_string()))).collect())
        .unwrap_or_default();
    Some((text, secrets))
}

/// After a start or an edit: publishes this replica's fleet.toml to the fleet when it differs from what the fleet
/// last had. Returns the hash of the settings in the file (None without one).
pub fn share_local_settings(db: &Connection, paths: &Paths, node: &str, now: f64) -> rusqlite::Result<Option<String>> {
    let Some((text, secrets)) = local_settings(paths) else { return Ok(None) };
    let h = settings_hash(&text, &secrets);
    if crate::db::kv_get(db, "fleet_config_local")?.as_deref() == Some(h.as_str()) {
        return Ok(Some(h));
    }
    let shared = crate::db::kv_get(db, "fleet_config")?.and_then(|v| J::parse(&v).ok());
    let same = shared.as_ref().and_then(entry_settings).is_some_and(|(t, s)| settings_hash(&t, &s) == h);
    if !same {
        let mut sec = Obj::new();
        for (f, t) in &secrets {
            sec.set(f, J::Str(t.clone()));
        }
        let entry = Obj::new().with("stamp", now).with("by", node).with("text", text).with("secrets", J::Obj(sec));
        crate::db::kv_set(db, "fleet_config", &J::Obj(entry).stringify())?;
    }
    crate::db::kv_set(db, "fleet_config_local", &h)?;
    Ok(Some(h))
}

/// Writes the fleet's settings (newer ones from another replica) into fleet.toml and the token files, if they load
/// with this replica's hub.toml. Returns their hash, or why they were refused.
pub fn adopt_settings(db: &Connection, paths: &Paths) -> Result<Option<String>, String> {
    let Some(v) = crate::db::kv_get(db, "fleet_config").map_err(|e| e.to_string())?.and_then(|v| J::parse(&v).ok())
    else {
        return Ok(None);
    };
    let Some((text, secrets)) = entry_settings(&v) else { return Ok(None) };
    let h = settings_hash(&text, &secrets);
    if crate::db::kv_get(db, "fleet_config_local").map_err(|e| e.to_string())?.as_deref() == Some(h.as_str()) {
        return Ok(Some(h));
    }
    let table: toml::Table = toml::from_str(&text).map_err(|e| format!("fleet settings: {e}"))?;
    if let Some(k) = table.keys().find(|k| !FLEET_KEYS.contains(&k.as_str())) {
        return Err(format!("fleet settings: {k} is not a fleet-wide key"));
    }
    let file = paths.fleet_toml();
    let old = std::fs::read(&file).ok();
    for (f, t) in &secrets {
        if !safe_rel(f) {
            return Err(format!("fleet settings: token file {f} is not a relative path"));
        }
        agent_stats_common::write_secret(&paths.resolve(f), t).map_err(|e| format!("{f}: {e}"))?;
    }
    agent_stats_common::atomic_write(&file, text.as_bytes(), Some(0o644))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    if let Err(e) = Config::load(paths, &paths.hub_toml()) {
        match old {
            Some(o) => {
                let _ = agent_stats_common::atomic_write(&file, &o, Some(0o644));
            }
            None => {
                let _ = std::fs::remove_file(&file);
            }
        }
        return Err(format!("fleet settings don't load here, kept the old ones: {e}"));
    }
    crate::db::kv_set(db, "fleet_config_local", &h).map_err(|e| e.to_string())?;
    Ok(Some(h))
}

// ---- join requests ----

/// The 4 digits both screens show: from the request id and the requester's token hash, so a request someone else
/// made (another token) shows other digits.
pub fn digits(id: &str, token_hash: &str) -> String {
    let h = sha256_hex(&format!("{id}:{token_hash}"));
    let n = u32::from_str_radix(&h[..8], 16).unwrap_or(0) % 10_000;
    format!("{n:04}")
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `POST /join-request`: `{machine, label, platform, tokenHash, viewHash}` → `(id, digits, expires)`.
pub fn create_request(db: &Connection, body: &J, now: f64) -> Result<(String, String, f64), (u16, String)> {
    let machine = body.get("machine").and_then(J::as_str).unwrap_or("");
    if machine.is_empty() || machine.chars().count() > 128 || machine.chars().any(crate::validate::is_unsafe_char) {
        return Err((422, "machine is missing or invalid".into()));
    }
    let token_hash =
        body.get("tokenHash").and_then(J::as_str).filter(|h| is_hash(h)).ok_or((422, "tokenHash".to_string()))?;
    let view_hash = body.get("viewHash").and_then(J::as_str).filter(|h| is_hash(h));
    let label = body.get("label").and_then(J::as_str).map(|s| crate::validate::clean(s, 128)).filter(|s| !s.is_empty());
    let platform = body.get("platform").and_then(J::as_str).map(|s| crate::validate::clean(s, 32));
    let db_err = |e: rusqlite::Error| (500, e.to_string());
    let pending: i64 = db
        .query_row(
            "SELECT count(*) FROM join_requests WHERE status = 'pending' AND expires > ?",
            params![now as i64],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if pending >= 20 {
        return Err((429, "too many join requests waiting; try again in a few minutes".into()));
    }
    let id = agent_stats_common::random_hex(8).map_err(|e| (500, e.to_string()))?;
    let expires = now + REQUEST_TTL_MS;
    db.execute(
        "INSERT INTO join_requests (id, machine, label, platform, token_hash, view_hash, created, expires, status)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
        params![id, machine, label, platform, token_hash, view_hash, now as i64, expires as i64],
    )
    .map_err(db_err)?;
    let d = digits(&id, token_hash);
    Ok((id, d, expires))
}

/// A request's status for its maker, who proves it with the token hash: pending, approved, denied or expired.
pub fn request_status(db: &Connection, id: &str, key: &str, now: f64) -> rusqlite::Result<Option<String>> {
    let row: Option<(String, String, i64)> = db
        .query_row("SELECT token_hash, status, expires FROM join_requests WHERE id = ?", params![id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    Ok(row.filter(|(h, ..)| agent_stats_common::ct_eq(h.as_bytes(), key.as_bytes())).map(|(_, s, exp)| {
        if s == "pending" && (exp as f64) <= now {
            "expired".into()
        } else {
            s
        }
    }))
}

/// Pending, unexpired requests: `[{id, machine, label, platform, digits, at, replaces}]`.
pub fn pending(db: &Connection, now: f64) -> rusqlite::Result<Vec<Obj>> {
    let mut st = db.prepare(
        "SELECT r.id, r.machine, r.label, r.platform, r.token_hash, r.created,
                EXISTS (SELECT 1 FROM machine_tokens t WHERE t.machine = r.machine)
           FROM join_requests r WHERE r.status = 'pending' AND r.expires > ? ORDER BY r.created",
    )?;
    let rows = st.query_map(params![now as i64], |r| {
        let id: String = r.get(0)?;
        let hash: String = r.get(4)?;
        Ok(Obj::new()
            .with("id", id.as_str())
            .with("machine", r.get::<_, String>(1)?)
            .with("label", r.get::<_, Option<String>>(2)?)
            .with("platform", r.get::<_, Option<String>>(3)?)
            .with("digits", digits(&id, &hash))
            .with("at", r.get::<_, i64>(5)?)
            .with("replaces", r.get::<_, bool>(6)?))
    })?;
    rows.collect()
}

/// Approves (enrolls the machine with the token it made, replacing any it had) or denies a pending request.
pub fn decide(db: &mut Connection, id: &str, approve: bool, by: &str, now: f64) -> Result<String, String> {
    let tx = db.transaction().map_err(|e| e.to_string())?;
    let row: Option<RequestRow> = tx
        .query_row(
            "SELECT machine, label, token_hash, view_hash, status, expires FROM join_requests WHERE id = ?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((machine, label, token_hash, view_hash, status, expires)) = row else {
        return Err(format!("no join request {id}"));
    };
    if status != "pending" {
        return Err(format!("request {id} was already {status}"));
    }
    if (expires as f64) <= now {
        return Err(format!("request {id} expired; ask again from the new machine"));
    }
    let e = |e: rusqlite::Error| e.to_string();
    tx.execute(
        "UPDATE join_requests SET status = ?, decided = ?, by_node = ? WHERE id = ?",
        params![if approve { "approved" } else { "denied" }, now as i64, by, id],
    )
    .map_err(e)?;
    if approve {
        tx.execute(
            "INSERT INTO machine_tokens (machine, token_hash, label, created, last_seen, revoked) VALUES (?, ?, ?, ?, NULL, NULL)
               ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, label = excluded.label,
                 created = excluded.created, last_seen = NULL, revoked = NULL",
            params![machine, token_hash, label, now as i64],
        )
        .map_err(e)?;
        if let Some(v) = view_hash {
            tx.execute(
                "INSERT INTO view_tokens (machine, token_hash, created) VALUES (?, ?, ?)
                   ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, created = excluded.created",
                params![machine, v, now as i64],
            )
            .map_err(e)?;
        }
        // A machine added again after being removed is a member again (it has a new token now).
        tx.execute("DELETE FROM fleet_members WHERE node = ? AND revoked IS NOT NULL", params![machine]).map_err(e)?;
    }
    tx.commit().map_err(e)?;
    Ok(machine)
}

/// `hub purge` as a record every replica applies.
pub fn record_purge(
    db: &Connection,
    machine: &str,
    from: Option<&str>,
    to: Option<&str>,
    now: f64,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO purges (machine, from_day, to_day, at) SELECT ?1, ?2, ?3, ?4
          WHERE NOT EXISTS (SELECT 1 FROM purges WHERE machine IS ?1 AND from_day IS ?2 AND to_day IS ?3)",
        params![machine, from, to, now as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(node: &str, always_on: bool, last_ok: Option<f64>) -> Member {
        Member {
            node: node.into(),
            routes: vec![format!("http://{node}:7777")],
            version: None,
            always_on,
            revoked: false,
            last_ok,
            last_error: None,
        }
    }

    #[test]
    fn the_always_on_replica_heard_from_publishes() {
        let now = 1_000_000_000.0;
        let list = vec![m("mac-mini", true, Some(now - 5_000.0)), m("air", false, None)];
        assert_eq!(publisher(&list, "air", false, now), "mac-mini");
        assert_eq!(publisher(&list, "mac-mini", true, now), "mac-mini");
        // Not heard from for ten minutes: the Air takes over.
        let list = vec![m("mac-mini", true, Some(now - ALIVE_MS - 1.0))];
        assert_eq!(publisher(&list, "air", false, now), "air");
        // Two laptops: by name.
        let list = vec![m("zed", false, Some(now))];
        assert_eq!(publisher(&list, "air", false, now), "air");
        assert_eq!(
            fleet_routes(&[m("mac-mini", true, None)], "air", false, &["http://air:7777".into()]),
            ["http://mac-mini:7777", "http://air:7777"]
        );
    }

    #[test]
    fn join_requests_are_approved_with_the_requesters_own_token() {
        let mut db = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        let token = "a".repeat(64);
        let th = sha256_hex(&token);
        let body =
            J::parse(&format!(r#"{{"machine":"air","label":"Air","tokenHash":"{th}","viewHash":"{th}"}}"#)).unwrap();
        let (id, d, _) = create_request(&db, &body, 1000.0).unwrap();
        assert_eq!(d, digits(&id, &th));
        assert_eq!(request_status(&db, &id, &th, 2000.0).unwrap().as_deref(), Some("pending"));
        assert_eq!(request_status(&db, &id, "wrong", 2000.0).unwrap(), None);
        assert_eq!(pending(&db, 2000.0).unwrap().len(), 1);
        decide(&mut db, &id, true, "mac-mini", 3000.0).unwrap();
        assert_eq!(crate::auth::machine_of_token(&db, &token).unwrap().as_deref(), Some("air"));
        assert_eq!(request_status(&db, &id, &th, 4000.0).unwrap().as_deref(), Some("approved"));
        assert!(decide(&mut db, &id, true, "mac-mini", 3000.0).is_err());
        let (id2, ..) = create_request(&db, &body, 1000.0).unwrap();
        assert_eq!(request_status(&db, &id2, &th, 1000.0 + REQUEST_TTL_MS).unwrap().as_deref(), Some("expired"));
    }

    #[test]
    fn hello_registers_and_revocation_is_final() {
        let db = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        let h = sha256_hex("replica");
        let body = J::parse(&format!(
            r#"{{"routes":["http://air:7777"],"version":"0.6.0","alwaysOn":false,"tokenHash":"{h}"}}"#
        ))
        .unwrap();
        hello(&db, "air", &body, 1.0).unwrap();
        assert_eq!(node_of_replica_token(&db, "replica").unwrap().as_deref(), Some("air"));
        assert!(revoke(&db, "air", 2.0).unwrap());
        assert_eq!(node_of_replica_token(&db, "replica").unwrap(), None);
        assert_eq!(hello(&db, "air", &body, 3.0).unwrap_err().0, 403);
    }
}
