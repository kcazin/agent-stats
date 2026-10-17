//! Install, services, updates and rollback (docs/ARCHITECTURE.md decisions 7 and 8).
//!
//! Everything that touches the machine goes through [`Ctx`]: directories, the service-manager programs and
//! timeouts are all fields, so tests run against temp dirs with fake `systemctl`/`launchctl`/`gh` scripts.

pub mod auto;
pub mod layout;
pub mod service;
pub mod update;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub type Res<T> = Result<T, String>;

pub trait Context<T> {
    fn ctx(self, what: impl std::fmt::Display) -> Res<T>;
}

impl<T, E: std::fmt::Display> Context<T> for Result<T, E> {
    fn ctx(self, what: impl std::fmt::Display) -> Res<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Linux,
    Mac,
}

/// Roles that run as services (the scheduled `updater` is deliberately not one, see `service::UPDATER`): (cli name, systemd unit, launchd label suffix, status-file role names to try).
pub const ROLES: [&str; 3] = ["report", "hub", "relay"];

#[derive(Clone, Debug)]
pub struct Ctx {
    pub platform: Platform,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Where the `agent-stats` symlink goes (`~/.local/bin`).
    pub bin_dir: PathBuf,
    /// systemd user unit directory.
    pub unit_dir: PathBuf,
    /// `~/Library/LaunchAgents`.
    pub agents_dir: PathBuf,
    pub systemctl: String,
    pub launchctl: String,
    pub loginctl: String,
    pub gh: String,
    pub uid: u32,
    pub user: String,
    /// The binary that is running now, and its version; `service install` installs it when nothing is installed.
    pub self_exe: PathBuf,
    pub self_version: String,
    /// Health trial: how long to wait for each role, and how often to look.
    pub trial_secs: u64,
    pub poll_ms: u64,
    /// A restarted role must stay up (same pid) this long after its first good status to pass the trial.
    pub settle_secs: u64,
    /// How long update/rollback/service install wait for `install.lock` before giving up.
    pub lock_wait_secs: u64,
    /// macOS: how long `service install` waits for the new LaunchAgent to run before restoring the old one.
    pub launchd_wait_secs: u64,
    /// `plutil`, used to lint a new plist when it exists.
    pub plutil: String,
    /// `PATH` written into service definitions.
    pub service_path: String,
    /// `config_dir()`: the reporter's config.toml (hubs to update from) and secrets.
    pub config_dir: PathBuf,
    /// `config_dir()/update.toml`, the settings of `update --auto`.
    pub update_toml: PathBuf,
    /// `GITHUB_TOKEN`, `GH_TOKEN`, else `config_dir()/secrets/github.token` (a launchd job may not reach the login
    /// keychain where gh keeps its token).
    pub github_token: Option<String>,
    pub repo: String,
    /// Target triple of this machine, used to pick the release asset.
    pub target: String,
}

pub fn target_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "unsupported"
    }
}

impl Ctx {
    pub fn from_env() -> Ctx {
        let home = agent_stats_common::home();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let config_home = env("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        let platform = if cfg!(target_os = "macos") { Platform::Mac } else { Platform::Linux };
        let bin_dir = home.join(".local").join("bin");
        let mut service_path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display());
        if platform == Platform::Mac {
            service_path = format!("{}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin", bin_dir.display());
        }
        let config_dir = agent_stats_common::config_dir();
        let github_token = env("GITHUB_TOKEN")
            .or_else(|| env("GH_TOKEN"))
            .or_else(|| agent_stats_common::read_secret(&config_dir.join("secrets").join("github.token")));
        Ctx {
            platform,
            data_dir: agent_stats_common::data_dir(),
            state_dir: agent_stats_common::state_dir(),
            bin_dir,
            unit_dir: config_home.join("systemd").join("user"),
            agents_dir: home.join("Library").join("LaunchAgents"),
            home,
            systemctl: "systemctl".into(),
            launchctl: "launchctl".into(),
            loginctl: "loginctl".into(),
            gh: "gh".into(),
            uid: unsafe { libc::getuid() },
            user: env("USER").or_else(|| env("LOGNAME")).unwrap_or_default(),
            self_exe: std::env::current_exe().unwrap_or_default(),
            self_version: agent_stats_common::VERSION.to_string(),
            trial_secs: 90,
            poll_ms: 500,
            settle_secs: 8,
            lock_wait_secs: 300,
            launchd_wait_secs: 10,
            plutil: "plutil".into(),
            service_path,
            update_toml: config_dir.join("update.toml"),
            config_dir,
            github_token,
            repo: env("AGENT_STATS_REPO").unwrap_or_else(|| "kcazin/agent-stats".into()),
            target: target_triple().into(),
        }
    }
}

/// What `cmd` does when an app ships this binary: `None` to go ahead (`service status`), else the exit code after
/// saying why it doesn't. The app's own updates replace the binary, and the app runs the services.
fn managed(app: &str, cmd: &str, args: &[String]) -> Option<ExitCode> {
    let sub = args.first().map(String::as_str);
    let msg = match (cmd, sub) {
        ("service", Some("status")) | ("service", None) => return None,
        // The updater timer of an install made before the app took over: nothing to do, and not a failure.
        ("update", Some("--auto")) => {
            println!("agent-stats here is part of {app}, which updates it; nothing to do");
            return Some(ExitCode::SUCCESS);
        }
        ("update", _) | ("rollback", _) => format!("agent-stats here is part of {app}: update {app} instead"),
        _ => format!("agent-stats here is part of {app}, which runs its services: use {app}'s settings instead"),
    };
    let _ = writeln!(std::io::stderr(), "agent-stats {cmd}: {msg}");
    Some(ExitCode::from(1))
}

/// `agent-stats <service|update|rollback> [args]`; `cmd` is the first word.
pub fn main(cmd: &str, args: &[String]) -> ExitCode {
    let ctx = Ctx::from_env();
    if let Some(app) = agent_stats_common::managed_by() {
        if let Some(r) = managed(&app, cmd, args) {
            return r;
        }
    }
    let r = match cmd {
        "service" => service::cli(&ctx, args),
        "update" => update::cli(&ctx, args),
        "rollback" => update::rollback(&ctx),
        _ => Err(format!("usage: unknown command {cmd}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Not eprintln!: it panics when the terminal is gone (SIGHUP), which would mask the exit code.
            let _ = writeln!(std::io::stderr(), "agent-stats {cmd}: {e}");
            ExitCode::from(if e.starts_with("usage:") { 2 } else { 1 })
        }
    }
}
