//! Pairing codes, machine tokens, the admin token and the hub id. Codes and tokens are stored as sha256 hashes;
//! the plain values exist only in the `hub pair` output, the `/pair` answer and the reporter's 0600 file.
//!
//! `hub pair` writes the code straight into hub.db (SQLite's write lock is the lock; WAL lets the running hub keep
//! reading). That way pairing works whether or not the hub is running, needs no admin endpoint, and the code never
//! crosses a socket. `hub revoke` works the same way, and the hub checks tokens against the database on every
//! report, so a revocation takes effect on the next report.

use crate::json::{Obj, J};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Crockford base32 without I, L, O, U: what a person can read off a screen and type.
const CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh `XXXX-XXXX` code (40 bits).
pub fn new_code() -> std::io::Result<String> {
    let hex = agent_stats_common::random_hex(8)?;
    let bytes: Vec<u8> = (0..8).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect();
    let chars: String = bytes.iter().map(|b| CODE_ALPHABET[(*b as usize) % 32] as char).collect();
    Ok(format!("{}-{}", &chars[..4], &chars[4..]))
}

/// Upper-case, no separators, and the letters people confuse with digits mapped to them.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// Codes made with `hub pair --replace <machine>` are bound to that machine and may replace its token. A separate
/// table, so `pair_codes` stays what the JS hub's database tooling expects.
///
/// `view_tokens`: each enrolled machine's read-only token (sha256), for a widget on that machine. It reads what the
/// dashboard reads and nothing else, and lives as long as the machine's own token.
fn ensure_schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS pair_code_machine (code_hash TEXT PRIMARY KEY, machine TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS view_tokens (machine TEXT PRIMARY KEY, token_hash TEXT NOT NULL, created INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS view_tokens_hash ON view_tokens (token_hash);",
    )
}

/// A new read-only token for `machine`, replacing any it had. The caller checks the machine is enrolled.
pub fn issue_view_token(db: &Connection, machine: &str, now: f64) -> rusqlite::Result<String> {
    ensure_schema(db)?;
    let token = agent_stats_common::random_hex(32).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    db.execute(
        "INSERT INTO view_tokens (machine, token_hash, created) VALUES (?, ?, ?)
           ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, created = excluded.created",
        params![machine, sha256_hex(&token), now as i64],
    )?;
    Ok(token)
}

/// The machine a read-only token belongs to, while that machine's own token is not revoked.
pub fn machine_of_view_token(db: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    ensure_schema(db)?;
    let hash = sha256_hex(token);
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT v.machine, v.token_hash FROM view_tokens v JOIN machine_tokens t ON t.machine = v.machine
               WHERE v.token_hash = ? AND t.revoked IS NULL",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.filter(|(_, h)| agent_stats_common::ct_eq(h.as_bytes(), hash.as_bytes())).map(|(m, _)| m))
}

/// Stores a one-time code valid for `ttl`.
pub fn create_code(db: &Connection, label: Option<&str>, ttl: Duration, now: f64) -> rusqlite::Result<String> {
    create_code_for(db, label, None, ttl, now)
}

/// Like `create_code`; with `replace` the code is only good for that machine name, and may replace its token (even a
/// revoked one). Without it, a code never takes over a name that already has a token.
pub fn create_code_for(
    db: &Connection,
    label: Option<&str>,
    replace: Option<&str>,
    ttl: Duration,
    now: f64,
) -> rusqlite::Result<String> {
    ensure_schema(db)?;
    let code = new_code().map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    db.execute("DELETE FROM pair_codes WHERE expires < ? OR used IS NOT NULL", params![now as i64 - 86_400_000])?;
    db.execute("DELETE FROM pair_code_machine WHERE code_hash NOT IN (SELECT code_hash FROM pair_codes)", [])?;
    let hash = sha256_hex(&normalize_code(&code));
    db.execute(
        "INSERT INTO pair_codes (code_hash, label, created, expires) VALUES (?, ?, ?, ?)",
        params![hash, label, now as i64, now as i64 + ttl.as_millis() as i64],
    )?;
    if let Some(m) = replace {
        db.execute("INSERT INTO pair_code_machine VALUES (?, ?)", params![hash, m])?;
    }
    Ok(code)
}

pub enum PairError {
    BadCode,
    /// The machine already has a token (live or revoked) and the code wasn't made with `--replace` for it.
    Conflict,
    /// The code was made with `--replace <this machine>` for another name.
    WrongMachine(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for PairError {
    fn from(e: rusqlite::Error) -> Self {
        PairError::Db(e)
    }
}

/// Swaps a valid code for a new token for `machine`. A machine that already has a token is only replaced by a code
/// made with `--replace` for it; a machine on the legacy token has no token yet, so its first pairing needs no flag.
pub fn redeem(
    db: &mut Connection,
    code: &str,
    machine: &str,
    label: Option<&str>,
    now: f64,
) -> Result<String, PairError> {
    ensure_schema(db)?;
    let hash = sha256_hex(&normalize_code(code));
    let tx = db.transaction()?;
    let row: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT code_hash, label FROM pair_codes WHERE code_hash = ? AND used IS NULL AND expires > ?",
            params![hash, now as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((stored, code_label)) = row else { return Err(PairError::BadCode) };
    if !agent_stats_common::ct_eq(stored.as_bytes(), hash.as_bytes()) {
        return Err(PairError::BadCode);
    }
    let bound: Option<String> = tx
        .query_row("SELECT machine FROM pair_code_machine WHERE code_hash = ?", params![hash], |r| r.get(0))
        .optional()?;
    match bound.as_deref() {
        Some(b) if b != machine => return Err(PairError::WrongMachine(b.to_string())),
        Some(_) => {}
        None if is_enrolled(&tx, machine)? => return Err(PairError::Conflict),
        None => {}
    }
    let token = agent_stats_common::random_hex(32).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    tx.execute("UPDATE pair_codes SET used = ? WHERE code_hash = ?", params![now as i64, hash])?;
    tx.execute(
        "INSERT INTO machine_tokens (machine, token_hash, label, created, last_seen, revoked) VALUES (?, ?, ?, ?, NULL, NULL)
           ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, label = excluded.label,
             created = excluded.created, last_seen = NULL, revoked = NULL",
        params![machine, sha256_hex(&token), code_label.as_deref().or(label), now as i64],
    )?;
    tx.commit()?;
    Ok(token)
}

/// Whether `machine` has a row in `machine_tokens`, live or revoked.
pub fn is_enrolled(db: &Connection, machine: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS (SELECT 1 FROM machine_tokens WHERE machine = ?)", params![machine], |r| r.get(0))
}

/// The machine a (non-revoked) token belongs to.
pub fn machine_of_token(db: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    let hash = sha256_hex(token);
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT machine, token_hash FROM machine_tokens WHERE token_hash = ? AND revoked IS NULL",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.filter(|(_, h)| agent_stats_common::ct_eq(h.as_bytes(), hash.as_bytes())).map(|(m, _)| m))
}

pub fn revoke(db: &Connection, machine: &str, now: f64) -> rusqlite::Result<bool> {
    let n = db.execute(
        "UPDATE machine_tokens SET revoked = ? WHERE machine = ? AND revoked IS NULL",
        params![now as i64, machine],
    )?;
    ensure_schema(db)?;
    db.execute("DELETE FROM view_tokens WHERE machine = ?", params![machine])?;
    Ok(n > 0)
}

/// Enrolled machines with a live token, and unexpired unused codes.
pub fn auth_counts(db: &Connection, now: f64) -> rusqlite::Result<(i64, i64)> {
    let machines = db.query_row("SELECT count(*) FROM machine_tokens WHERE revoked IS NULL", [], |r| r.get(0))?;
    let codes =
        db.query_row("SELECT count(*) FROM pair_codes WHERE used IS NULL AND expires > ?", params![now as i64], |r| {
            r.get(0)
        })?;
    Ok((machines, codes))
}

/// `hub machines`: every machine the hub has heard of or enrolled.
pub fn machines(db: &Connection) -> rusqlite::Result<Vec<Obj>> {
    let mut st = db.prepare(
        "WITH ids AS (SELECT machine FROM machines UNION SELECT machine FROM machine_tokens)
         SELECT ids.machine, m.label, m.platform, m.last_seen, t.label, t.created, t.last_seen, t.revoked,
                x.version, x.protocol, x.auth
           FROM ids LEFT JOIN machines m ON m.machine = ids.machine
                    LEFT JOIN machine_tokens t ON t.machine = ids.machine
                    LEFT JOIN machine_meta x ON x.machine = ids.machine
          ORDER BY ids.machine",
    )?;
    let rows = st.query_map([], |r| {
        let label: Option<String> = r.get::<_, Option<String>>(1)?.or(r.get(4)?);
        Ok(Obj::new()
            .with("machine", r.get::<_, String>(0)?)
            .with("label", label)
            .with("platform", r.get::<_, Option<String>>(2)?)
            .with("lastReport", r.get::<_, Option<i64>>(3)?)
            .with("enrolled", r.get::<_, Option<i64>>(5)?.is_some())
            .with("tokenCreated", r.get::<_, Option<i64>>(5)?)
            .with("tokenLastSeen", r.get::<_, Option<i64>>(6)?)
            .with("revoked", r.get::<_, Option<i64>>(7)?)
            .with("version", r.get::<_, Option<String>>(8)?)
            .with("protocol", r.get::<_, Option<i64>>(9)?)
            .with("auth", r.get::<_, Option<String>>(10)?))
    })?;
    rows.collect()
}

/// `/pair` attempts: 5 per client and 200 overall per 10 minutes, then 429 until the window passes. An attempt is
/// reserved before the code is checked (so concurrent requests can't all slip under the cap) and given back when
/// it succeeds. Brute force is hopeless anyway (40-bit one-time codes); the overall cap only keeps one peer from
/// locking everybody else out.
#[derive(Default)]
pub struct PairLimiter {
    attempts: Mutex<HashMap<std::net::IpAddr, Vec<Instant>>>,
}

const WINDOW: Duration = Duration::from_secs(600);
const PER_CLIENT: usize = 5;
const OVERALL: usize = 200;

impl PairLimiter {
    /// Counts an attempt by `ip` if the caps allow it.
    pub fn reserve(&self, ip: std::net::IpAddr) -> bool {
        let mut f = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        f.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < WINDOW);
            !v.is_empty()
        });
        if f.get(&ip).map_or(0, Vec::len) >= PER_CLIENT || f.values().map(Vec::len).sum::<usize>() >= OVERALL {
            return false;
        }
        f.entry(ip).or_default().push(now);
        true
    }
    /// Gives back the attempt `reserve` counted (the pairing worked, or never tried a code).
    pub fn refund(&self, ip: std::net::IpAddr) {
        let mut f = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = f.get_mut(&ip) {
            v.pop();
            if v.is_empty() {
                f.remove(&ip);
            }
        }
    }
}

/// Reads `path`, or creates it with fresh random contents. Safe when two processes race: the file is written
/// aside and hard-linked into place, so whoever links first wins and everyone reads the same value.
pub fn read_or_create(path: &Path, bytes: usize, secret: bool) -> std::io::Result<String> {
    if let Some(v) = agent_stats_common::read_secret(path) {
        return Ok(v);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let value = agent_stats_common::random_hex(bytes)?;
    let tmp =
        dir.join(format!(".{}.new{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("id"), std::process::id()));
    agent_stats_common::atomic_write(&tmp, format!("{value}\n").as_bytes(), Some(if secret { 0o600 } else { 0o644 }))?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(value),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            agent_stats_common::read_secret(path).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{} is empty", path.display()))
            })
        }
        Err(e) => Err(e),
    }
}

/// `{"hubId", "name", "token", "viewToken", "routes", "timezone"}`
pub fn pair_answer(hub_id: &str, name: &str, token: &str, view_token: &str, routes: &[String], timezone: &str) -> J {
    Obj::new()
        .with("hubId", hub_id)
        .with("name", name)
        .with("token", token)
        .with("viewToken", view_token)
        .with("routes", J::Arr(routes.iter().map(|r| J::Str(r.clone())).collect()))
        .with("timezone", timezone)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = new_code().unwrap();
        assert_eq!(c.len(), 9);
        assert_eq!(&c[4..5], "-");
        assert_eq!(normalize_code("k7q2-9xmo"), "K7Q29XM0");
        assert_eq!(normalize_code(" il-1 "), "111");
    }

    #[test]
    fn pair_redeem_revoke() {
        let dir = std::env::temp_dir().join(format!("agent-stats-hub-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = crate::db::open(&dir.join("hub.db")).unwrap();
        let code = create_code(&db, Some("Laptop"), Duration::from_secs(600), 1000.0).unwrap();
        assert_eq!(auth_counts(&db, 1000.0).unwrap(), (0, 1));
        assert!(matches!(redeem(&mut db, "WRONG-CODE", "m", None, 1001.0), Err(PairError::BadCode)));
        let t = redeem(&mut db, &code.to_lowercase(), "m", None, 1001.0).ok().unwrap();
        assert_eq!(t.len(), 64);
        assert!(matches!(redeem(&mut db, &code, "m2", None, 1002.0), Err(PairError::BadCode)), "one-time");
        assert_eq!(machine_of_token(&db, &t).unwrap().as_deref(), Some("m"));
        assert!(revoke(&db, "m", 2000.0).unwrap());
        assert_eq!(machine_of_token(&db, &t).unwrap(), None);
        let late = create_code(&db, None, Duration::from_secs(1), 0.0).unwrap();
        assert!(matches!(redeem(&mut db, &late, "m", None, 5000.0), Err(PairError::BadCode)), "expired");
        let id = read_or_create(&dir.join("hub-id"), 16, false).unwrap();
        assert_eq!(read_or_create(&dir.join("hub-id"), 16, false).unwrap(), id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_never_takes_over_a_name_without_replace() {
        let dir = std::env::temp_dir().join(format!("agent-stats-hub-auth-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = crate::db::open(&dir.join("hub.db")).unwrap();
        let ttl = Duration::from_secs(600);
        let t1 = redeem_new(&mut db, "mac-mini", ttl);
        // A plain code can't claim an enrolled name, and isn't used up by trying.
        let plain = create_code(&db, None, ttl, 1000.0).unwrap();
        assert!(matches!(redeem(&mut db, &plain, "mac-mini", None, 1001.0), Err(PairError::Conflict)));
        assert_eq!(machine_of_token(&db, &t1).unwrap().as_deref(), Some("mac-mini"), "old token still works");
        assert!(redeem(&mut db, &plain, "laptop", None, 1002.0).is_ok(), "same code, free name");
        // A revoked machine is not silently revived.
        revoke(&db, "laptop", 1003.0).unwrap();
        let plain = create_code(&db, None, ttl, 1000.0).unwrap();
        assert!(matches!(redeem(&mut db, &plain, "laptop", None, 1004.0), Err(PairError::Conflict)));
        // --replace: only for that name, and it swaps the token and un-revokes.
        let rep = create_code_for(&db, None, Some("laptop"), ttl, 1000.0).unwrap();
        assert!(
            matches!(redeem(&mut db, &rep, "mac-mini", None, 1005.0), Err(PairError::WrongMachine(m)) if m == "laptop")
        );
        let t2 = redeem(&mut db, &rep, "laptop", None, 1006.0).ok().unwrap();
        assert_eq!(machine_of_token(&db, &t2).unwrap().as_deref(), Some("laptop"));
        // A legacy machine (no token row) pairs without any flag.
        assert!(!is_enrolled(&db, "old-legacy").unwrap());
        let plain = create_code(&db, None, ttl, 1000.0).unwrap();
        assert!(redeem(&mut db, &plain, "old-legacy", None, 1007.0).is_ok());
        assert!(is_enrolled(&db, "old-legacy").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn redeem_new(db: &mut Connection, machine: &str, ttl: Duration) -> String {
        let code = create_code(db, None, ttl, 1000.0).unwrap();
        redeem(db, &code, machine, None, 1000.5).ok().unwrap()
    }

    #[test]
    fn limiter_reserves_atomically_and_refunds() {
        let l = PairLimiter::default();
        let a: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let b: std::net::IpAddr = "127.0.0.2".parse().unwrap();
        let granted = std::thread::scope(|s| {
            let hs: Vec<_> = (0..32).map(|_| s.spawn(|| l.reserve(a))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).filter(|g| *g).count()
        });
        assert_eq!(granted, PER_CLIENT, "concurrent attempts can't overshoot the per-client cap");
        assert!(!l.reserve(a));
        assert!(l.reserve(b), "another client isn't locked out");
        l.refund(b);
        for i in 0..OVERALL - PER_CLIENT {
            let ip = std::net::IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8]);
            assert!(l.reserve(ip), "{i}");
        }
        assert!(!l.reserve(b), "overall cap");
        l.refund(a);
        assert!(l.reserve(b), "a refund frees a slot");
    }
}
