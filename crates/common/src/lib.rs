//! Shared by every role (report, hub, relay, ops): where files live, how secrets and state are written, the
//! status file each long-running role keeps, and the wire constants. See docs/ARCHITECTURE.md.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Version of this build, from Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Reporter → hub protocol. The legacy JS reporter sends no `protocol` field and counts as 1.
/// 2 adds: `protocol`, `version`, `machineId`, `repos[].remote`. A hub answers 426 to protocols it doesn't speak.
pub const PROTOCOL: u32 = 2;
/// Well-known path every hub and relay answers with a small JSON descriptor (id, role, version, protocol).
pub const WELL_KNOWN: &str = "/.well-known/agent-stats";

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    env_dir("HOME").unwrap_or_else(|| PathBuf::from("."))
}

/// `$AGENT_STATS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/agent-stats`, else `~/.config/agent-stats`. Holds config.toml,
/// hub.toml and secrets (0700 directory, 0600 files).
pub fn config_dir() -> PathBuf {
    env_dir("AGENT_STATS_CONFIG_DIR")
        .unwrap_or_else(|| env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join("agent-stats"))
}

/// `$XDG_STATE_HOME/agent-stats`, else `~/.local/state/agent-stats`. Reporter state, hub.db, status files, logs.
/// (The reporter has always used XDG_STATE_HOME; keep it.)
pub fn state_dir() -> PathBuf {
    env_dir("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local").join("state")).join("agent-stats")
}

/// `$XDG_DATA_HOME/agent-stats`, else `~/.local/share/agent-stats`. Installed versions live in `versions/<v>/`,
/// with `current` pointing at the active one.
pub fn data_dir() -> PathBuf {
    env_dir("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local").join("share")).join("agent-stats")
}

/// Writes `bytes` to `path` atomically: unique temp file in the same directory (created exclusively, mode set before
/// any data is written), fsync, rename, fsync the directory.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut attempt = 0;
    let (tmp, mut f) = loop {
        let tmp = dir.join(format!(".{name}.tmp{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode.unwrap_or(0o644));
        }
        match opts.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 5 => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    let written = (|| {
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(m))?;
        }
        f.write_all(bytes)?;
        f.sync_all()
    })();
    drop(f);
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Writes a secret as a 0600 file inside a 0700 directory.
pub fn write_secret(path: &Path, value: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    atomic_write(path, format!("{}\n", value.trim()).as_bytes(), Some(0o600))
}

/// Reads a secret file, trimmed. `None` if missing or empty.
pub fn read_secret(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Constant-time comparison for tokens.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Random bytes from the OS, hex-encoded.
pub fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    let mut f = fs::File::open("/dev/urandom")?;
    io::Read::read_exact(&mut f, &mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// One per long-running role, at `state_dir()/<role>-status.json`. Written at start, whenever something in it
/// changes, and at least every `HEARTBEAT_SECS` (that's the liveness signal `agent-stats update` waits for).
/// `status`/`doctor` and the update health trial read it; nothing else depends on it.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub role: String,
    pub version: String,
    pub pid: u32,
    pub started_at: u64,
    pub heartbeat_at: u64,
    /// Role-specific details (reporter: hubs with lastOkAt/lastError; hub: listen address, machines; relay: updated).
    #[serde(default)]
    pub detail: serde_json::Value,
}

pub const HEARTBEAT_SECS: u64 = 60;

pub fn status_path(role: &str) -> PathBuf {
    state_dir().join(format!("{role}-status.json"))
}

pub fn write_status(s: &Status) -> io::Result<()> {
    atomic_write(&status_path(&s.role), serde_json::to_vec_pretty(s).unwrap_or_default().as_slice(), None)
}

pub fn read_status(role: &str) -> Option<Status> {
    serde_json::from_slice(&fs::read(status_path(role)).ok()?).ok()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Is a process with this pid alive? (EPERM counts as alive.)
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(0, _) hits our own process group and kill(-n, _) other groups: never a single pid.
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn secret_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agent-stats-common-{}", std::process::id()));
        let p = dir.join("secrets").join("t.token");
        write_secret(&p, "  hello \n").unwrap();
        assert_eq!(read_secret(&p).as_deref(), Some("hello"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pid_alive_rejects_group_pids() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
        assert!(!pid_alive(i32::MAX as u32 + 1));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_mode_and_unique_temps() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agent-stats-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.token");
        // A world-readable leftover at the old-style temp name must not leak into the secret's mode.
        let old = dir.join(format!(".s.token.tmp{}", std::process::id()));
        fs::write(&old, "junk").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&p, b"secret", Some(0o600)).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&p).unwrap(), b"secret");
        // Concurrent writers to one path each get their own temp file.
        let q = dir.join("c.json");
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let q = q.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        atomic_write(&q, format!("{i}").repeat(4096).as_bytes(), None).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let got = fs::read_to_string(&q).unwrap();
        assert_eq!(got.len(), 4096);
        assert!(got.bytes().all(|b| b == got.as_bytes()[0]));
        let _ = fs::remove_dir_all(dir);
    }
}

// --- lane ops ---
/// Size-capped log rotation: when `path` is at least `max_bytes`, shift `path.(N-1)` → `path.N` … `path` → `path.1`
/// and drop what falls past `keep`. Best effort; errors are ignored (logging must never stop a role).
pub fn rotate_log(path: &Path, max_bytes: u64, keep: usize) {
    let Ok(meta) = fs::metadata(path) else { return };
    if meta.len() < max_bytes {
        return;
    }
    let numbered = |n: usize| {
        let mut s = path.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    };
    if keep == 0 {
        let _ = fs::remove_file(path);
        return;
    }
    let _ = fs::remove_file(numbered(keep));
    for n in (1..keep).rev() {
        let _ = fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = fs::rename(path, numbered(1));
}

#[cfg(test)]
mod rotate_log_tests {
    use super::*;

    #[test]
    fn rotates_and_caps() {
        let dir = std::env::temp_dir().join(format!("agent-stats-rotate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("report.log");
        let at = |n: usize| dir.join(format!("report.log.{n}"));
        for round in 0..5 {
            fs::write(&log, format!("round {round} {}", "x".repeat(20))).unwrap();
            rotate_log(&log, 10, 3);
        }
        assert!(!log.exists());
        assert!(fs::read_to_string(at(1)).unwrap().starts_with("round 4"));
        assert!(fs::read_to_string(at(2)).unwrap().starts_with("round 3"));
        assert!(fs::read_to_string(at(3)).unwrap().starts_with("round 2"));
        assert!(!at(4).exists());
        // Below the threshold nothing moves; a missing file is fine.
        fs::write(&log, "small").unwrap();
        rotate_log(&log, 10, 3);
        assert!(log.exists());
        rotate_log(&dir.join("nope.log"), 10, 3);
        let _ = fs::remove_dir_all(dir);
    }
}
