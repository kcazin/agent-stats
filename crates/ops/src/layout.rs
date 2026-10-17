//! The versioned install layout: `data_dir/versions/<v>/agent-stats` (+ `.install-complete`), `data_dir/current`,
//! `~/.local/bin/agent-stats`, retention, and `install-state.json`.

use crate::{Context, Ctx, Platform, Res};
use agent_stats_common::{atomic_write, now_secs, pid_alive};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SENTINEL: &str = ".install-complete";
pub const BIN_NAME: &str = "agent-stats";
pub const KEEP_VERSIONS: usize = 3;

/// Versions become directory names, so keep them to a safe alphabet.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && !v.starts_with('.')
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Numeric-aware comparison (0.10.0 > 0.9.0).
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| -> Vec<(u64, String)> {
        s.split(['.', '-', '+']).map(|p| (p.parse::<u64>().unwrap_or(0), p.to_string())).collect()
    };
    parts(a).cmp(&parts(b))
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LastUpdate {
    pub from: Option<String>,
    pub to: String,
    /// `ok`, `installed` (not restarted), `rolled-back`, `failed`.
    pub status: String,
    pub at: u64,
    pub reason: Option<String>,
}

/// An update whose health trial is running (or was cut short): written before `current` moves, cleared with the
/// outcome. A record whose `pid` is gone is an orphan: nobody verified `to`, so the next command rolls back to `from`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Trial {
    pub from: Option<String>,
    pub to: String,
    pub pid: u32,
    pub started: u64,
    /// `previous` before the update, restored by a rollback.
    pub previous: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstallState {
    pub active: Option<String>,
    pub previous: Option<String>,
    pub last_update: Option<LastUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<Trial>,
}

impl Ctx {
    pub fn versions_dir(&self) -> PathBuf {
        self.data_dir.join("versions")
    }
    pub fn version_dir(&self, v: &str) -> PathBuf {
        self.versions_dir().join(v)
    }
    pub fn current_link(&self) -> PathBuf {
        self.data_dir.join("current")
    }
    pub fn current_bin(&self) -> PathBuf {
        self.current_link().join(BIN_NAME)
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("install-state.json")
    }

    pub fn is_complete(&self, v: &str) -> bool {
        let d = self.version_dir(v);
        fs::read_to_string(d.join(SENTINEL)).map(|s| s.trim() == v).unwrap_or(false) && d.join(BIN_NAME).is_file()
    }

    /// Complete installed versions, oldest first.
    pub fn installed_versions(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.versions_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| valid_version(n) && self.is_complete(n))
            .collect();
        v.sort_by(|a, b| cmp_versions(a, b));
        v
    }

    /// The version `current` points at, if it is a complete install.
    pub fn current_version(&self) -> Option<String> {
        let target = fs::read_link(self.current_link()).ok()?;
        let v = target.file_name()?.to_str()?.to_string();
        self.is_complete(&v).then_some(v)
    }

    pub fn load_state(&self) -> InstallState {
        fs::read(self.state_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_state(&self, s: &InstallState) -> Res<()> {
        let bytes = serde_json::to_vec_pretty(s).ctx("encode install-state.json")?;
        atomic_write(&self.state_file(), &bytes, None).ctx("write install-state.json")
    }
}

/// Runs `<bin> version` and returns the version it reports (`agent-stats <v> (protocol N)`).
pub fn probe_version(bin: &Path) -> Res<String> {
    let started = Instant::now();
    let mut child = loop {
        match Command::new(bin).arg("version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        {
            Ok(c) => break c,
            // Another thread forked while our copy was still open for writing: retry briefly.
            Err(e) if e.raw_os_error() == Some(26) && started.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(format!("cannot run {} version: {e}", bin.display())),
        }
    };
    loop {
        match child.try_wait().ctx("wait for version check")? {
            Some(status) => {
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut so, &mut out);
                }
                if !status.success() {
                    return Err(format!("{} version exited with {status}", bin.display()));
                }
                let mut it = out.split_whitespace();
                return match (it.next(), it.next()) {
                    (Some("agent-stats"), Some(v)) => Ok(v.to_string()),
                    _ => Err(format!("unexpected `version` output from {}: {:?}", bin.display(), out.trim())),
                };
            }
            None if started.elapsed() > Duration::from_secs(20) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} version did not finish in 20 s", bin.display()));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// fsyncs a file or directory so a rename or write survives power loss.
pub fn fsync_path(p: &Path) -> Res<()> {
    fs::File::open(p).and_then(|f| f.sync_all()).ctx(format!("fsync {}", p.display()))
}

/// Installs `src` as `versions/<version>/agent-stats`: stage, chmod, strip quarantine, verify, fsync, rename into
/// place. An already complete version is never touched (callers hold the install lock; this re-checks anyway).
pub fn install_version(ctx: &Ctx, src: &Path, version: &str) -> Res<PathBuf> {
    if !valid_version(version) {
        return Err(format!("invalid version {version:?}"));
    }
    let dest = ctx.version_dir(version);
    if ctx.is_complete(version) {
        return Ok(dest);
    }
    let versions = ctx.versions_dir();
    fs::create_dir_all(&versions).ctx(format!("create {}", versions.display()))?;
    let staging = versions.join(format!(".staging-{version}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).ctx(format!("create {}", staging.display()))?;
    let result = (|| -> Res<()> {
        let staged = staging.join(BIN_NAME);
        fs::copy(src, &staged).ctx(format!("copy {}", src.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).ctx("chmod 755")?;
        }
        if ctx.platform == Platform::Mac {
            let _ = Command::new("xattr").args(["-d", "com.apple.quarantine"]).arg(&staged).output();
        }
        let got = probe_version(&staged)?;
        if got != version {
            return Err(format!("binary reports version {got}, expected {version}"));
        }
        fsync_path(&staged)?;
        let marker = staging.join(SENTINEL);
        fs::write(&marker, format!("{version}\n")).ctx("write sentinel")?;
        fsync_path(&marker)?;
        fsync_path(&staging)?;
        if ctx.is_complete(version) {
            return Ok(()); // someone else finished it meanwhile: reuse, never replace
        }
        if dest.exists() {
            fs::remove_dir_all(&dest).ctx(format!("remove incomplete {}", dest.display()))?;
        }
        fs::rename(&staging, &dest).ctx(format!("move into {}", dest.display()))?;
        fsync_path(&versions)
    })();
    if result.is_err() || staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.map(|()| dest)
}

/// Points `current` at `versions/<v>` atomically (temp symlink, then rename over the old one).
pub fn activate(ctx: &Ctx, v: &str) -> Res<()> {
    if !ctx.is_complete(v) {
        return Err(format!("version {v} is not installed"));
    }
    fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
    let tmp = ctx.data_dir.join(format!(".current.tmp{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(Path::new("versions").join(v), &tmp).ctx("create symlink")?;
    fs::rename(&tmp, ctx.current_link()).ctx("switch current")?;
    fsync_path(&ctx.data_dir)
}

/// Records `v` as the active version (the old active one becomes `previous`).
pub fn record_active(ctx: &Ctx, v: &str) -> Res<InstallState> {
    let mut s = ctx.load_state();
    if s.active.as_deref() != Some(v) {
        s.previous = s.active.take();
        s.active = Some(v.to_string());
    }
    ctx.save_state(&s)?;
    Ok(s)
}

pub fn record_update(ctx: &Ctx, from: Option<String>, to: &str, status: &str, reason: Option<String>) -> Res<()> {
    let mut s = ctx.load_state();
    s.last_update = Some(LastUpdate { from, to: to.into(), status: status.into(), at: now_secs(), reason });
    s.trial = None;
    ctx.save_state(&s)
}

/// `~/.local/bin/agent-stats` → `data_dir/current/agent-stats`. Returns a note for the user, if any.
pub fn link_bin(ctx: &Ctx) -> Res<Option<String>> {
    fs::create_dir_all(&ctx.bin_dir).ctx(format!("create {}", ctx.bin_dir.display()))?;
    let link = ctx.bin_dir.join(BIN_NAME);
    let tmp = ctx.bin_dir.join(format!(".{BIN_NAME}.tmp{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(ctx.current_bin(), &tmp).ctx("create symlink")?;
    fs::rename(&tmp, &link).ctx(format!("replace {}", link.display()))?;
    let _ = fsync_path(&ctx.bin_dir);
    let on_path =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d == ctx.bin_dir)).unwrap_or(false);
    Ok((!on_path).then(|| {
        format!(
            "{} is not on your PATH; add it to your shell profile to run `agent-stats` directly",
            ctx.bin_dir.display()
        )
    }))
}

/// Keeps the newest `keep` versions, never deleting `current`, `previous` or the versions of a running trial;
/// removes incomplete leftovers and scratch of dead processes. Callers hold the install lock.
pub fn prune(ctx: &Ctx, keep: usize) {
    let state = ctx.load_state();
    let current = ctx.current_version();
    let linked =
        fs::read_link(ctx.current_link()).ok().and_then(|t| t.file_name().map(|n| n.to_string_lossy().to_string()));
    let protected = |v: &str| {
        current.as_deref() == Some(v)
            || linked.as_deref() == Some(v)
            || state.previous.as_deref() == Some(v)
            || state.trial.as_ref().is_some_and(|t| t.to == v || t.from.as_deref() == Some(v))
    };
    let installed = ctx.installed_versions();
    let slots = keep.saturating_sub(installed.iter().filter(|v| protected(v)).count());
    let mut kept = 0;
    for v in installed.iter().rev() {
        if protected(v) {
            continue;
        }
        if kept < slots {
            kept += 1;
        } else {
            let _ = fs::remove_dir_all(ctx.version_dir(v));
        }
    }
    for e in fs::read_dir(ctx.versions_dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let stale = if let Some(rest) = name.strip_prefix(".staging-") {
            // `.staging-<v>-<pid>`: stale once that pid is gone.
            rest.rsplit('-').next().and_then(|p| p.parse::<u32>().ok()).map(|p| !pid_alive(p)).unwrap_or(true)
        } else {
            valid_version(&name) && !ctx.is_complete(&name) && !protected(&name)
        };
        if stale {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    prune_dead_scratch(&ctx.data_dir);
    prune_dead_scratch(&ctx.bin_dir);
}

/// Removes `.update-<pid>`, `.install-<pid>` and `.<name>.tmp<pid>` entries in `dir` whose pid is gone.
fn prune_dead_scratch(dir: &Path) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let pid = if let Some(p) = name.strip_prefix(".update-").or_else(|| name.strip_prefix(".install-")) {
            p.parse::<u32>().ok()
        } else if name.starts_with('.') {
            name.rsplit_once(".tmp").and_then(|(_, p)| p.parse::<u32>().ok())
        } else {
            None
        };
        if pid.is_some_and(|p| p != std::process::id() && !pid_alive(p)) {
            let path = e.path();
            let _ =
                if path.is_dir() && !path.is_symlink() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }
}

// ---------------------------------------------------------------- locking

/// An exclusive `flock` on `data_dir/install.lock`, held by update, rollback and service install so they never
/// interleave. Released when dropped (or when the process dies).
pub struct InstallLock(#[allow(dead_code)] fs::File);

impl InstallLock {
    /// Waits up to `ctx.lock_wait_secs` for the lock, saying so once; then fails with the holder's pid.
    pub fn acquire(ctx: &Ctx) -> Res<InstallLock> {
        Self::lock(ctx, true)?.ok_or_else(|| "lock not taken".into())
    }

    /// Does not queue: `None` when another process holds the lock (`update --auto` comes back on its next run).
    /// It still retries for a moment, quietly: a process forked by another thread holds a copy of the lock until it
    /// execs.
    pub fn try_acquire(ctx: &Ctx) -> Res<Option<InstallLock>> {
        Self::lock(ctx, false)
    }

    fn lock(ctx: &Ctx, wait: bool) -> Res<Option<InstallLock>> {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        fs::create_dir_all(&ctx.data_dir).ctx("create data dir")?;
        let path = ctx.data_dir.join("install.lock");
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ctx(format!("open {}", path.display()))?;
        let started = Instant::now();
        let mut said = false;
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let _ = f.set_len(0);
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                return Ok(Some(InstallLock(f)));
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) && err.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("lock {}: {err}", path.display()));
            }
            if !wait {
                if started.elapsed() >= Duration::from_secs(2) {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let holder = fs::read_to_string(&path).unwrap_or_default();
            let holder = holder.trim();
            let who = if holder.is_empty() { String::new() } else { format!(" (pid {holder})") };
            if started.elapsed() >= Duration::from_secs(ctx.lock_wait_secs) {
                return Err(format!(
                    "another agent-stats install/update{who} is still running after {} s; try again when it finishes",
                    ctx.lock_wait_secs
                ));
            }
            if !said {
                let _ = writeln!(std::io::stderr(), "another agent-stats install/update{who} is running, waiting…");
                said = true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

// ---------------------------------------------------------------- signals

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// While alive, SIGHUP/SIGINT/SIGTERM only set a flag (see [`stop_requested`]) instead of killing the process, so
/// the health trial can roll back. The previous handlers come back on drop.
pub struct SignalGuard(Vec<(libc::c_int, libc::sighandler_t)>);

impl SignalGuard {
    pub fn install() -> SignalGuard {
        STOP.store(false, std::sync::atomic::Ordering::SeqCst);
        let handler: extern "C" fn(libc::c_int) = on_signal;
        let old = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM]
            .into_iter()
            .map(|sig| (sig, unsafe { libc::signal(sig, handler as libc::sighandler_t) }))
            .collect();
        SignalGuard(old)
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, old) in &self.0 {
            unsafe { libc::signal(*sig, *old) };
        }
    }
}

pub fn stop_requested() -> bool {
    STOP.load(std::sync::atomic::Ordering::SeqCst)
}
