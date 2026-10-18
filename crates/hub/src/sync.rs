//! Replica sync (docs/FLEET.md): the tables a fleet shares, the log of changed keys each hub keeps, the
//! `/sync/changes` page a peer pulls, and the merge that applies a page.
//!
//! Every shared row is owned by one machine and only grows, so merging is a union with fixed rules per table (token
//! counts take the max, descriptive rows the newest, first-seen the earliest). Triggers log each changed key in
//! `sync_log`, one entry per key whose `seq` moves on at every change; an update that changes nothing logs nothing,
//! so two replicas applying each other's rows settle instead of echoing them back and forth.

use crate::json::{Obj, J};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

/// A shared table: its columns (the key first) and which of them change too often to be worth a log entry alone.
pub struct Spec {
    pub t: &'static str,
    pub cols: &'static [&'static str],
    /// How many leading columns are the key.
    pub key: usize,
    /// Columns whose change alone doesn't log the row (it still travels with the next real change).
    quiet: &'static [&'static str],
}

pub const TABLES: &[Spec] = &[
    Spec { t: "usage", cols: &["machine", "day", "agent", "model", "tokens", "output"], key: 4, quiet: &[] },
    Spec { t: "repos", cols: &["machine", "day", "repo", "tokens"], key: 3, quiet: &[] },
    Spec { t: "machines", cols: &["machine", "label", "platform", "last_seen", "sessions"], key: 1, quiet: &[] },
    Spec { t: "machine_meta", cols: &["machine", "version", "protocol", "auth", "updated"], key: 1, quiet: &[] },
    Spec { t: "repo_remotes", cols: &["machine", "repo", "remote", "updated"], key: 2, quiet: &[] },
    Spec { t: "repo_meta", cols: &["repo", "first_seen", "first_day", "first_machine", "remote"], key: 1, quiet: &[] },
    Spec { t: "samples", cols: &["at", "working", "open", "subagents", "machines", "by_agent"], key: 1, quiet: &[] },
    Spec { t: "github", cols: &["day", "count"], key: 1, quiet: &[] },
    Spec {
        t: "machine_tokens",
        cols: &["machine", "token_hash", "label", "created", "last_seen", "revoked"],
        key: 1,
        quiet: &["last_seen"],
    },
    Spec { t: "view_tokens", cols: &["machine", "token_hash", "created"], key: 1, quiet: &[] },
    Spec {
        t: "fleet_members",
        cols: &["node", "routes", "version", "always_on", "token_hash", "updated", "revoked"],
        key: 1,
        quiet: &[],
    },
    Spec {
        t: "join_requests",
        cols: &[
            "id",
            "machine",
            "label",
            "platform",
            "token_hash",
            "view_hash",
            "created",
            "expires",
            "status",
            "decided",
            "by_node",
        ],
        key: 1,
        quiet: &[],
    },
    Spec { t: "purges", cols: &["machine", "from_day", "to_day", "at"], key: 3, quiet: &[] },
    Spec { t: "kv", cols: &["k", "v"], key: 1, quiet: &[] },
];

/// The `kv` keys a fleet shares: the site's pile (the publisher's) and the fleet-wide settings.
pub const KV_KEYS: &[&str] = &["pile", "fleet_config"];
/// Bumped when the triggers change, so new ones replace the old.
const TRIGGERS: &str = "sync1";

const SCHEMA: &str = "
  -- Each shared key that changed, once; seq moves on at every change. Peers pull it in seq order.
  CREATE TABLE IF NOT EXISTS sync_log (seq INTEGER PRIMARY KEY AUTOINCREMENT, tbl TEXT NOT NULL, k TEXT NOT NULL,
    UNIQUE (tbl, k));
  -- Where this hub is in each peer's log (and that log's database id), and when it last reached the peer.
  CREATE TABLE IF NOT EXISTS sync_cursors (peer TEXT PRIMARY KEY, db TEXT, seq INTEGER NOT NULL DEFAULT 0,
    last_ok INTEGER, last_error TEXT, route TEXT);
  -- The replicas of the fleet: their routes, version, publishing preference and replica token (sha256).
  CREATE TABLE IF NOT EXISTS fleet_members (node TEXT PRIMARY KEY, routes TEXT, version TEXT, always_on INTEGER,
    token_hash TEXT, updated INTEGER NOT NULL, revoked INTEGER);
  -- Machines asking to join without a code (docs/FLEET.md, Joining): approved on any replica.
  CREATE TABLE IF NOT EXISTS join_requests (id TEXT PRIMARY KEY, machine TEXT NOT NULL, label TEXT, platform TEXT,
    token_hash TEXT NOT NULL, view_hash TEXT, created INTEGER NOT NULL, expires INTEGER NOT NULL, status TEXT NOT NULL,
    decided INTEGER, by_node TEXT);
  -- `hub purge` as a record, so every replica deletes the same rows.
  CREATE TABLE IF NOT EXISTS purges (machine TEXT, from_day TEXT, to_day TEXT, at INTEGER,
    PRIMARY KEY (machine, from_day, to_day));
";

fn key_expr(spec: &Spec, row: &str) -> String {
    let parts: Vec<String> = spec.cols[..spec.key].iter().map(|c| format!("{row}.{c}")).collect();
    format!("json_array({})", parts.join(", "))
}

fn kv_only(spec: &Spec, row: &str) -> String {
    if spec.t == "kv" {
        let keys: Vec<String> = KV_KEYS.iter().map(|k| format!("'{k}'")).collect();
        format!("{row}.k IN ({})", keys.join(", "))
    } else {
        "1".into()
    }
}

/// The two triggers of a table: after an insert, and after an update that changed a column worth syncing.
fn triggers(spec: &Spec) -> String {
    let t = spec.t;
    let log = format!(
        "DELETE FROM sync_log WHERE tbl = '{t}' AND k = {k}; INSERT INTO sync_log (tbl, k) VALUES ('{t}', {k});",
        k = key_expr(spec, "NEW")
    );
    let changed: Vec<String> =
        spec.cols.iter().filter(|c| !spec.quiet.contains(c)).map(|c| format!("OLD.{c} IS NOT NEW.{c}")).collect();
    format!(
        "CREATE TRIGGER IF NOT EXISTS {TRIGGERS}_{t}_i AFTER INSERT ON {t} WHEN {only} BEGIN {log} END;
         CREATE TRIGGER IF NOT EXISTS {TRIGGERS}_{t}_u AFTER UPDATE ON {t} WHEN {only} AND ({changed}) BEGIN {log} END;",
        only = kv_only(spec, "NEW"),
        changed = changed.join(" OR ")
    )
}

/// Creates the sync tables and triggers, logs every existing row once, and gives the database its id.
pub fn ensure_schema(db: &Connection) -> rusqlite::Result<()> {
    crate::auth::ensure_schema(db)?;
    db.execute_batch(SCHEMA)?;
    for spec in TABLES {
        db.execute_batch(&triggers(spec))?;
    }
    if crate::db::kv_get(db, "sync_db")?.is_none() {
        let id = agent_stats_common::random_hex(8).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        // Rows from before sync existed: logged once, so a new replica's first pull gets everything. Samples only
        // for the last two days (the charts show one).
        let tx = db.unchecked_transaction()?;
        for spec in TABLES {
            let since = if spec.t == "samples" {
                format!(" AND at >= {}", agent_stats_common::now_secs() as i64 * 1000 - 2 * 86_400_000)
            } else {
                String::new()
            };
            tx.execute_batch(&format!(
                "INSERT OR IGNORE INTO sync_log (tbl, k) SELECT '{t}', {k} FROM {t} WHERE {only}{since}",
                t = spec.t,
                k = key_expr(spec, spec.t),
                only = kv_only(spec, spec.t)
            ))?;
        }
        crate::db::kv_set(&tx, "sync_db", &id)?;
        tx.commit()?;
    }
    Ok(())
}

/// This database's id: a peer whose cursor was for another id (the file was replaced) starts again from 0.
pub fn db_id(db: &Connection) -> rusqlite::Result<String> {
    Ok(crate::db::kv_get(db, "sync_db")?.unwrap_or_default())
}

pub fn spec(t: &str) -> Option<&'static Spec> {
    TABLES.iter().find(|s| s.t == t)
}

fn to_j(v: ValueRef) -> J {
    match v {
        ValueRef::Integer(n) => J::Num(n as f64),
        ValueRef::Real(f) => J::Num(f),
        ValueRef::Text(t) => J::Str(String::from_utf8_lossy(t).into_owned()),
        _ => J::Null,
    }
}

fn to_value(j: &J) -> Value {
    match j {
        J::Num(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::Integer(*f as i64),
        J::Num(f) => Value::Real(*f),
        J::Str(s) => Value::Text(s.clone()),
        J::Bool(b) => Value::Integer(i64::from(*b)),
        J::Null => Value::Null,
        other => Value::Text(other.stringify()),
    }
}

fn where_key(spec: &Spec, first: usize) -> String {
    let w: Vec<String> =
        spec.cols[..spec.key].iter().enumerate().map(|(i, c)| format!("{c} IS ?{}", first + i)).collect();
    w.join(" AND ")
}

/// The current row of a logged key (gone: None).
fn fetch(db: &Connection, spec: &Spec, k: &str) -> rusqlite::Result<Option<Vec<J>>> {
    let w: Vec<String> =
        spec.cols[..spec.key].iter().enumerate().map(|(i, c)| format!("{c} IS json_extract(?1, '$[{i}]')")).collect();
    let sql = format!("SELECT {} FROM {} WHERE {} LIMIT 1", spec.cols.join(", "), spec.t, w.join(" AND "));
    db.prepare_cached(&sql)?
        .query_row(params![k], |r| Ok((0..spec.cols.len()).map(|i| r.get_ref(i).map(to_j)).collect::<Result<_, _>>()))
        .optional()?
        .transpose()
}

/// One page of changes after `since`: `(rows, seq of the last entry, more after it)`.
pub fn changes(db: &Connection, since: i64, limit: usize) -> rusqlite::Result<(Vec<J>, i64, bool)> {
    let mut st = db.prepare_cached("SELECT seq, tbl, k FROM sync_log WHERE seq > ? ORDER BY seq LIMIT ?")?;
    let mut log: Vec<(i64, String, String)> = st
        .query_map(params![since, limit as i64 + 1], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let more = log.len() > limit;
    log.truncate(limit);
    let mut rows = Vec::with_capacity(log.len());
    let mut last = since;
    for (seq, t, k) in log {
        last = seq;
        let Some(spec) = spec(&t) else { continue };
        if let Some(v) = fetch(db, spec, &k)? {
            rows.push(J::Obj(Obj::new().with("t", t).with("v", J::Arr(v))));
        }
    }
    Ok((rows, last, more))
}

/// What a page changed here, beyond rows.
#[derive(Default, Debug, PartialEq)]
pub struct Applied {
    pub rows: usize,
    /// The pile came from another replica (the in-memory one must be reloaded).
    pub pile: bool,
    /// Newer fleet-wide settings arrived.
    pub fleet_config: bool,
    /// A join request appeared or was decided.
    pub requests: bool,
}

/// The newest `at` (pile) or `stamp` (fleet_config) of a kv value.
fn kv_stamp(k: &str, v: Option<&str>) -> f64 {
    let field = if k == "pile" { "at" } else { "stamp" };
    v.and_then(|v| J::parse(v).ok()).and_then(|j| j.get(field).and_then(J::as_f64)).unwrap_or(f64::NEG_INFINITY)
}

/// The merge of an existing row with an incoming one (`?1…?N` = the incoming columns). Each statement matches only
/// when it would change something, so the count of rows it touched says whether anything changed.
fn merge_sql(t: &str) -> &'static [&'static str] {
    match t {
        "usage" => &["UPDATE usage SET tokens = max(coalesce(tokens, 0), coalesce(?5, 0)),
                        output = max(coalesce(output, 0), coalesce(?6, 0))
                       WHERE machine IS ?1 AND day IS ?2 AND agent IS ?3 AND model IS ?4
                         AND (coalesce(?5, 0) > coalesce(tokens, 0) OR coalesce(?6, 0) > coalesce(output, 0))"],
        "repos" => &["UPDATE repos SET tokens = max(coalesce(tokens, 0), coalesce(?4, 0))
                       WHERE machine IS ?1 AND day IS ?2 AND repo IS ?3 AND coalesce(?4, 0) > coalesce(tokens, 0)"],
        "machines" => &["UPDATE machines SET label = ?2, platform = ?3, last_seen = ?4, sessions = ?5
                          WHERE machine IS ?1 AND coalesce(last_seen, 0) < coalesce(?4, 0)"],
        "machine_meta" => &["UPDATE machine_meta SET version = coalesce(?2, version), protocol = ?3, auth = ?4, updated = ?5
                              WHERE machine IS ?1 AND coalesce(updated, 0) < coalesce(?5, 0)"],
        "repo_remotes" => &["UPDATE repo_remotes SET remote = ?3, updated = ?4
                              WHERE machine IS ?1 AND repo IS ?2 AND coalesce(updated, 0) < coalesce(?4, 0)"],
        // NULL first_seen means "known before first_seen was recorded": the earliest of all.
        "repo_meta" => &["UPDATE repo_meta SET first_seen = ?2, first_day = ?3, first_machine = ?4, remote = coalesce(?5, remote)
                           WHERE repo IS ?1 AND first_seen IS NOT NULL AND (?2 IS NULL OR ?2 < first_seen)"],
        "github" => &["UPDATE github SET count = max(coalesce(count, 0), coalesce(?2, 0)) WHERE day IS ?1 AND coalesce(?2, 0) > coalesce(count, 0)"],
        "machine_tokens" => &[
            // A newer token replaces the row (re-paired somewhere); the same token gathers revocation and last use.
            "UPDATE machine_tokens SET token_hash = ?2, label = ?3, created = ?4, last_seen = ?5, revoked = ?6
              WHERE machine IS ?1 AND created < ?4",
            "UPDATE machine_tokens SET revoked = coalesce(revoked, ?6), label = coalesce(?3, label),
                    last_seen = CASE WHEN ?5 IS NULL THEN last_seen WHEN last_seen IS NULL OR last_seen < ?5 THEN ?5
                                     ELSE last_seen END
              WHERE machine IS ?1 AND created = ?4 AND token_hash = ?2
                AND ((revoked IS NULL AND ?6 IS NOT NULL) OR (?3 IS NOT NULL AND label IS NOT ?3)
                     OR (?5 IS NOT NULL AND (last_seen IS NULL OR last_seen < ?5)))",
        ],
        "view_tokens" => &["UPDATE view_tokens SET token_hash = ?2, created = ?3 WHERE machine IS ?1 AND created < ?3"],
        "fleet_members" => &["UPDATE fleet_members SET routes = ?2, version = ?3, always_on = ?4, token_hash = ?5,
                                updated = ?6, revoked = ?7
                               WHERE node IS ?1 AND updated < ?6"],
        "join_requests" => &["UPDATE join_requests SET status = ?9, decided = ?10, by_node = ?11
                               WHERE id IS ?1 AND status = 'pending' AND ?9 IS NOT 'pending'"],
        _ => &[],
    }
}

fn apply_row(tx: &Transaction, spec: &Spec, vals: &[Value], out: &mut Applied) -> rusqlite::Result<()> {
    if spec.t == "kv" {
        let (Value::Text(k), Value::Text(v)) = (&vals[0], &vals[1]) else { return Ok(()) };
        if !KV_KEYS.contains(&k.as_str()) {
            return Ok(());
        }
        let here = crate::db::kv_get(tx, k)?;
        if kv_stamp(k, Some(v)) > kv_stamp(k, here.as_deref()) {
            crate::db::kv_set(tx, k, v)?;
            out.rows += 1;
            out.pile |= k == "pile";
            out.fleet_config |= k == "fleet_config";
        }
        return Ok(());
    }
    let exists: bool = tx
        .prepare_cached(&format!("SELECT EXISTS (SELECT 1 FROM {} WHERE {})", spec.t, where_key(spec, 1)))?
        .query_row(params_from_iter(&vals[..spec.key]), |r| r.get(0))?;
    let n = if exists {
        let mut n = 0;
        for sql in merge_sql(spec.t) {
            n += tx.prepare_cached(sql)?.execute(params_from_iter(vals))?;
        }
        n
    } else {
        let marks: Vec<String> = (1..=spec.cols.len()).map(|i| format!("?{i}")).collect();
        let n = tx
            .prepare_cached(&format!(
                "INSERT INTO {} ({}) VALUES ({})",
                spec.t,
                spec.cols.join(", "),
                marks.join(", ")
            ))?
            .execute(params_from_iter(vals))?;
        if spec.t == "purges" {
            for table in ["usage", "repos"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE machine = ?1 AND (?2 IS NULL OR day >= ?2) AND (?3 IS NULL OR day <= ?3)"
                    ),
                    params_from_iter(&vals[..3]),
                )?;
            }
        }
        n
    };
    if n > 0 {
        out.rows += 1;
        out.requests |= spec.t == "join_requests";
    }
    Ok(())
}

/// Merges a page of rows from a peer. Rows of tables this version doesn't know, or of the wrong shape, are skipped.
pub fn apply(db: &mut Connection, rows: &[J]) -> rusqlite::Result<Applied> {
    let mut out = Applied::default();
    let tx = db.transaction()?;
    for row in rows {
        let (Some(t), Some(v)) = (row.get("t").and_then(J::as_str), row.get("v").and_then(J::as_arr)) else { continue };
        let Some(spec) = spec(t) else { continue };
        if v.len() != spec.cols.len() || v[..spec.key].iter().any(|k| matches!(k, J::Arr(_) | J::Obj(_))) {
            continue;
        }
        let vals: Vec<Value> = v.iter().map(to_value).collect();
        apply_row(&tx, spec, &vals, &mut out)?;
    }
    tx.commit()?;
    Ok(out)
}

/// Every sample older than `cutoff` (ms) goes, and its log entries with it.
pub fn prune_samples(db: &Connection, cutoff: i64) -> rusqlite::Result<()> {
    db.execute("DELETE FROM samples WHERE at < ?", params![cutoff])?;
    db.execute(
        "DELETE FROM sync_log WHERE tbl = 'samples' AND CAST(json_extract(k, '$[0]') AS INTEGER) < ?",
        params![cutoff],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::db::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn pull(from: &Connection, to: &mut Connection, since: i64) -> (i64, Applied) {
        let (rows, last, more) = changes(from, since, 10_000).unwrap();
        assert!(!more);
        (last, apply(to, &rows).unwrap())
    }

    #[test]
    fn usage_merges_by_max_and_settles() {
        let (a, mut b) = (db(), db());
        a.execute_batch(
            "INSERT INTO usage VALUES ('mini', '2026-10-01', 'claude', 'opus', 100, 10);
             INSERT INTO usage VALUES ('mini', '2026-10-01', NULL, NULL, 5, 1);
             INSERT INTO repos VALUES ('mini', '2026-10-01', 'neon', 100);",
        )
        .unwrap();
        b.execute("INSERT INTO usage VALUES ('mini', '2026-10-01', 'claude', 'opus', 150, 5)", []).unwrap();
        let (seq, ap) = pull(&a, &mut b, 0);
        assert_eq!(ap.rows, 3);
        let (t, o): (i64, i64) = b
            .query_row("SELECT tokens, output FROM usage WHERE agent = 'claude'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((t, o), (150, 10));
        // A NULL in the key matches itself instead of adding a row.
        let mut a2 = a;
        let (_, ap) = pull(&b, &mut a2, 0);
        assert_eq!(ap.rows, 1, "only the row b had higher");
        let n: i64 = a2.query_row("SELECT count(*) FROM usage", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        // Applying the same page again changes nothing and logs nothing new.
        let before: i64 = a2.query_row("SELECT max(seq) FROM sync_log", [], |r| r.get(0)).unwrap();
        let (rows, _, _) = changes(&b, 0, 10_000).unwrap();
        assert_eq!(apply(&mut a2, &rows).unwrap().rows, 0);
        let after: i64 = a2.query_row("SELECT max(seq) FROM sync_log", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
        assert!(seq > 0);
    }

    #[test]
    fn cursors_only_bring_what_changed() {
        let (a, mut b) = (db(), db());
        a.execute("INSERT INTO usage VALUES ('m', '2026-10-01', 'claude', 'x', 1, 1)", []).unwrap();
        let (seq, _) = pull(&a, &mut b, 0);
        assert_eq!(changes(&a, seq, 100).unwrap().0.len(), 0);
        a.execute("UPDATE usage SET tokens = 2", []).unwrap();
        let (rows, _, _) = changes(&a, seq, 100).unwrap();
        assert_eq!(rows.len(), 1);
        // An update that changes nothing is not logged.
        let (_, s2, _) = changes(&a, 0, 100).unwrap();
        a.execute("UPDATE usage SET tokens = 2", []).unwrap();
        assert_eq!(changes(&a, s2, 100).unwrap().0.len(), 0);
    }

    #[test]
    fn newest_wins_for_machines_and_tokens_and_revocation_sticks() {
        let (a, mut b) = (db(), db());
        a.execute_batch(
            "INSERT INTO machines VALUES ('air', 'Air', 'darwin', 2000, '[]');
             INSERT INTO machine_tokens VALUES ('air', 'h1', 'Air', 100, NULL, NULL);",
        )
        .unwrap();
        b.execute_batch(
            "INSERT INTO machines VALUES ('air', 'Old', 'darwin', 1000, '[]');
             INSERT INTO machine_tokens VALUES ('air', 'h1', 'Air', 100, 50, 777);",
        )
        .unwrap();
        pull(&a, &mut b, 0);
        let label: String = b.query_row("SELECT label FROM machines", [], |r| r.get(0)).unwrap();
        assert_eq!(label, "Air");
        let revoked: Option<i64> = b.query_row("SELECT revoked FROM machine_tokens", [], |r| r.get(0)).unwrap();
        assert_eq!(revoked, Some(777), "a revocation is not undone by an older copy");
        let mut a = a;
        pull(&b, &mut a, 0);
        let revoked: Option<i64> = a.query_row("SELECT revoked FROM machine_tokens", [], |r| r.get(0)).unwrap();
        assert_eq!(revoked, Some(777), "and it reaches the other side");
        // Re-pairing (a newer token) replaces the revoked row.
        a.execute("UPDATE machine_tokens SET token_hash = 'h2', created = 200, revoked = NULL", []).unwrap();
        pull(&a, &mut b, 0);
        let (h, r): (String, Option<i64>) =
            b.query_row("SELECT token_hash, revoked FROM machine_tokens", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((h.as_str(), r), ("h2", None));
    }

    #[test]
    fn kv_pile_and_purges() {
        let (a, mut b) = (db(), db());
        crate::db::kv_set(&a, "pile", r#"{"day":"2026-10-01","tokens":1,"total":2,"at":500}"#).unwrap();
        crate::db::kv_set(&a, "secret", "not shared").unwrap();
        crate::db::kv_set(&b, "pile", r#"{"day":"2026-10-01","tokens":9,"total":9,"at":400}"#).unwrap();
        let (_, ap) = pull(&a, &mut b, 0);
        assert!(ap.pile);
        assert!(crate::db::kv_get(&b, "pile").unwrap().unwrap().contains("\"at\":500"));
        assert_eq!(crate::db::kv_get(&b, "secret").unwrap(), None);
        b.execute("INSERT INTO usage VALUES ('gone', '2026-10-01', 'claude', 'x', 1, 1)", []).unwrap();
        a.execute("INSERT INTO purges VALUES ('gone', NULL, NULL, 1)", []).unwrap();
        pull(&a, &mut b, 0);
        let n: i64 = b.query_row("SELECT count(*) FROM usage WHERE machine = 'gone'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn existing_rows_are_logged_once_and_unknown_tables_are_skipped() {
        let dir = std::env::temp_dir().join(format!("agent-stats-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hub.db");
        {
            // A database from before sync: rows, no log.
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                "CREATE TABLE usage (machine TEXT, day TEXT, agent TEXT, model TEXT, tokens INTEGER, output INTEGER,
                   PRIMARY KEY (machine, day, agent, model));
                 INSERT INTO usage VALUES ('m', '2026-10-01', 'claude', 'x', 1, 1);",
            )
            .unwrap();
        }
        let a = crate::db::open(&path).unwrap();
        let (rows, _, _) = changes(&a, 0, 100).unwrap();
        assert_eq!(rows.len(), 1);
        let mut b = db();
        let page =
            vec![J::parse(r#"{"t":"future_table","v":[1]}"#).unwrap(), J::parse(r#"{"t":"usage","v":[1]}"#).unwrap()];
        assert_eq!(apply(&mut b, &page).unwrap().rows, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
