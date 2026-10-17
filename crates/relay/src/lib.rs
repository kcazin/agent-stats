//! The relay role: keeps the latest public snapshot a hub publishes and serves it at `GET /stats`, and counts open tabs at `GET /presence`.
//! Drop-in replacement for the Bun relay (same `<data>/stats.json`). See docs/ARCHITECTURE.md and docs/deploy.md.

mod http;
mod patch;
mod presence;
mod store;
mod stream;

use agent_stats_common::{read_secret, state_dir};
use std::path::PathBuf;
use std::process::ExitCode;

pub use http::{start, Running};
pub use store::Store;

pub const MAX_BODY: usize = 512 * 1024;
pub const DEFAULT_STALE_AFTER: u64 = 120;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub data: PathBuf,
    /// Empty = no token configured: every publish is refused (fail closed).
    pub token: String,
    pub cors: Vec<String>,
    pub stale_after: u64,
    pub status: bool,
}

const USAGE: &str = "usage: agent-stats relay serve [--listen ADDR] [--data DIR] [--token-file FILE] [--cors ORIGINS] \
[--stale-after SECS] [--no-status]
  env fallbacks: PORT (listen 0.0.0.0:$PORT, default 8080), DATA_DIR, PUBLISH_TOKEN, CORS_ORIGINS";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let (mut listen, mut data, mut token_file, mut cors, mut stale) = (None, None, None, None, None);
    let mut status = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        if key == "--no-status" {
            status = false;
            continue;
        }
        if !matches!(key, "--listen" | "--data" | "--token-file" | "--cors" | "--stale-after") {
            return Err(format!("unknown argument {a}\n{USAGE}"));
        }
        let v = match inline {
            Some(v) => v,
            None => it.next().cloned().ok_or_else(|| format!("{key} needs a value"))?,
        };
        match key {
            "--listen" => listen = Some(v),
            "--data" => data = Some(v),
            "--token-file" => token_file = Some(v),
            "--cors" => cors = Some(v),
            _ => stale = Some(v),
        }
    }
    let listen = listen
        .or_else(|| env("LISTEN"))
        .unwrap_or_else(|| format!("0.0.0.0:{}", env("PORT").unwrap_or_else(|| "8080".into())));
    let data = match data.or_else(|| env("DATA_DIR")) {
        Some(d) => PathBuf::from(d),
        None if std::path::Path::new("/data").is_dir() => PathBuf::from("/data"),
        None => state_dir(),
    };
    let token = match token_file {
        Some(f) => {
            read_secret(std::path::Path::new(&f)).ok_or_else(|| format!("token file {f} is missing or empty"))?
        }
        None => env("PUBLISH_TOKEN").unwrap_or_default(),
    };
    let cors = cors
        .or_else(|| env("CORS_ORIGINS"))
        .or_else(|| env("ALLOWED_ORIGINS"))
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let stale_after = match stale {
        Some(s) => s.parse().map_err(|_| format!("--stale-after: not a number: {s}"))?,
        None => DEFAULT_STALE_AFTER,
    };
    Ok(Config { listen, data, token, cors, stale_after, status })
}

/// `agent-stats relay <subcommand> [args]`.
pub fn main(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("serve") => {}
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("agent-stats relay: unknown subcommand {other}\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let cfg = match parse_args(&args[1..]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agent-stats relay: {e}");
            return ExitCode::from(2);
        }
    };
    if cfg.token.is_empty() {
        eprintln!(
            "relay: warning: no publish token configured (PUBLISH_TOKEN / --token-file); every publish will be refused"
        );
    }
    if cfg.cors.is_empty() {
        eprintln!(
            "relay: warning: no CORS origins (CORS_ORIGINS / ALLOWED_ORIGINS / --cors): only http://localhost pages can \
read /stats and /stream from a browser (the Bun relay defaulted to https://kilcazn.com,https://www.kilcazn.com)"
        );
    }
    match start(cfg) {
        Ok(running) => running.wait(),
        Err(e) => {
            eprintln!("agent-stats relay: {e}");
            ExitCode::FAILURE
        }
    }
}
