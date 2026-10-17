//! The hub's SQLite file. The first six tables are exactly hub.mjs's, so this hub can take over the JS hub's
//! `hub.db` in place (and the JS hub can take it back). Everything new lives in new tables, created with
//! `IF NOT EXISTS`; existing rows are never rewritten or dropped.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// `kv.schema_version`: 2 = the JS hub's tables (1) plus the tables below.
pub const SCHEMA_VERSION: i64 = 2;

const JS_SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS usage (machine TEXT, day TEXT, agent TEXT, model TEXT, tokens INTEGER, output INTEGER,
    PRIMARY KEY (machine, day, agent, model));
  CREATE TABLE IF NOT EXISTS machines (machine TEXT PRIMARY KEY, label TEXT, platform TEXT, last_seen INTEGER, sessions TEXT);
  CREATE TABLE IF NOT EXISTS samples (at INTEGER PRIMARY KEY, working INTEGER, open INTEGER, subagents INTEGER, machines INTEGER);
  CREATE TABLE IF NOT EXISTS github (day TEXT PRIMARY KEY, count INTEGER);
  CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT);
  CREATE TABLE IF NOT EXISTS repos (machine TEXT, day TEXT, repo TEXT, tokens INTEGER, PRIMARY KEY (machine, day, repo));
";

const NEW_SCHEMA: &str = "
  -- Per-machine tokens from `hub pair`; only the sha256 of the token is kept.
  CREATE TABLE IF NOT EXISTS machine_tokens (machine TEXT PRIMARY KEY, token_hash TEXT NOT NULL, label TEXT,
    created INTEGER NOT NULL, last_seen INTEGER, revoked INTEGER);
  CREATE INDEX IF NOT EXISTS machine_tokens_hash ON machine_tokens (token_hash);
  -- One-time pairing codes (sha256 of the normalized code).
  CREATE TABLE IF NOT EXISTS pair_codes (code_hash TEXT PRIMARY KEY, label TEXT, created INTEGER NOT NULL,
    expires INTEGER NOT NULL, used INTEGER);
  -- What each machine's reporter says about itself, and how it authenticated.
  CREATE TABLE IF NOT EXISTS machine_meta (machine TEXT PRIMARY KEY, version TEXT, protocol INTEGER, auth TEXT,
    updated INTEGER);
  -- The git remote each machine reports for a repository name (protocol 2).
  CREATE TABLE IF NOT EXISTS repo_remotes (machine TEXT, repo TEXT, remote TEXT, updated INTEGER,
    PRIMARY KEY (machine, repo));
  -- When the hub first saw each repository (by name). Backfilled from `repos` without notifying.
  CREATE TABLE IF NOT EXISTS repo_meta (repo TEXT PRIMARY KEY, first_seen INTEGER, first_day TEXT,
    first_machine TEXT, remote TEXT);
";

/// The hub's state directory: created 0700, and tightened if an older run left it more open.
#[cfg(unix)]
fn private_dir(dir: &Path) {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    if std::fs::metadata(dir).is_ok_and(|m| m.mode() & 0o077 != 0) {
        chmod(dir, 0o700);
    }
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

#[cfg(unix)]
fn private_file(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path);
}

#[cfg(not(unix))]
fn private_file(_: &Path) {}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if std::fs::metadata(path).is_ok_and(|m| m.mode() & 0o7777 != mode) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn chmod(_: &Path, _: u32) {}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    // ":memory:" (tests) is not a file: don't create one, or touch the working directory.
    let on_disk = path != Path::new(":memory:");
    if let Some(dir) = path.parent().filter(|d| on_disk && !d.as_os_str().is_empty()) {
        private_dir(dir);
    }
    // Create the file 0600 first: SQLite gives the -wal and -shm files the main file's mode.
    if on_disk {
        private_file(path);
    }
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let _: String = db.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    for suffix in ["", "-wal", "-shm"] {
        let mut f = path.as_os_str().to_owned();
        f.push(suffix);
        chmod(Path::new(&f), 0o600);
    }
    db.execute_batch(JS_SCHEMA)?;
    // hub.mjs: working agents per agent type, added after the first samples were taken.
    let _ = db.execute_batch("ALTER TABLE samples ADD COLUMN by_agent TEXT");
    db.execute_batch(NEW_SCHEMA)?;
    // Repositories the JS hub (or an earlier run) already knew are not "new".
    db.execute_batch(
        "INSERT OR IGNORE INTO repo_meta (repo, first_seen, first_day, first_machine)
           SELECT repo, NULL, min(day), min(machine) FROM repos WHERE repo IS NOT NULL GROUP BY repo",
    )?;
    let v: Option<String> = db.query_row("SELECT v FROM kv WHERE k = 'schema_version'", [], |r| r.get(0)).optional()?;
    if v.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1) < SCHEMA_VERSION {
        db.execute("INSERT OR REPLACE INTO kv VALUES ('schema_version', ?)", params![SCHEMA_VERSION.to_string()])?;
    }
    Ok(db)
}

pub fn kv_get(db: &Connection, k: &str) -> rusqlite::Result<Option<String>> {
    db.query_row("SELECT v FROM kv WHERE k = ?", params![k], |r| r.get(0)).optional()
}

pub fn kv_set(db: &Connection, k: &str, v: &str) -> rusqlite::Result<()> {
    db.execute("INSERT OR REPLACE INTO kv VALUES (?, ?)", params![k, v]).map(|_| ())
}

/// A column that hub.mjs reads as a JS number: INTEGER or REAL, NULL as `None`.
pub fn num(r: &rusqlite::Row, i: usize) -> rusqlite::Result<Option<f64>> {
    use rusqlite::types::ValueRef;
    Ok(match r.get_ref(i)? {
        ValueRef::Integer(n) => Some(n as f64),
        ValueRef::Real(f) => Some(f),
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|s| s.trim().parse().ok()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_a_js_hub_db_and_keeps_its_rows() {
        let dir = std::env::temp_dir().join(format!("agent-stats-hub-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hub.db");
        {
            // What hub.mjs leaves behind (before by_agent existed).
            let db = Connection::open(&p).unwrap();
            db.execute_batch(JS_SCHEMA).unwrap();
            db.execute_batch(
                "INSERT INTO repos VALUES ('m', '2026-10-01', 'neon', 5);
                 INSERT INTO samples (at, working, open, subagents, machines) VALUES (1, 2, 3, 0, 1);
                 INSERT INTO kv VALUES ('pile', '{\"day\":\"2026-10-01\",\"tokens\":1.5,\"total\":9,\"at\":1}');",
            )
            .unwrap();
        }
        for _ in 0..2 {
            let db = open(&p).unwrap();
            assert_eq!(kv_get(&db, "schema_version").unwrap().as_deref(), Some("2"));
            assert!(kv_get(&db, "pile").unwrap().unwrap().contains("1.5"));
            let n: i64 = db.query_row("SELECT count(*) FROM repo_meta WHERE repo = 'neon'", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 1);
            let w: i64 = db.query_row("SELECT working FROM samples WHERE at = 1", [], |r| r.get(0)).unwrap();
            assert_eq!(w, 2);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_is_0700_and_db_files_0600() {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let dir = std::env::temp_dir().join(format!("agent-stats-hub-db-modes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o755).create(&dir).unwrap();
        let p = dir.join("hub.db");
        std::fs::write(&p, b"").unwrap(); // an older run's world-readable file
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let db = open(&p).unwrap();
        db.execute("INSERT INTO kv VALUES ('a', 'b')", []).unwrap(); // WAL and shm exist while the connection lives
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        for f in ["hub.db", "hub.db-wal", "hub.db-shm"] {
            assert_eq!(std::fs::metadata(dir.join(f)).unwrap().mode() & 0o777, 0o600, "{f}");
        }
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
