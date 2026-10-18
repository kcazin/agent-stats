//! The hub role: receives reports, keeps history, serves the dashboard, publishes the public payload.
//! A port of `legacy/hub/hub.mjs` (same database, same payload), plus pairing, per-machine tokens, validation,
//! projects from config and several publish targets. See docs/ARCHITECTURE.md.

pub mod admin;
pub mod auth;
pub mod bonjour;
pub mod compute;
pub mod config;
pub mod db;
pub mod events;
pub mod http;
pub mod json;
pub mod limits;
pub mod release;
pub mod server;
pub mod stream;
pub mod validate;

use config::{Config, Paths};
use json::J;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: agent-stats hub <command>
  serve [--config FILE] [--listen HOST:PORT] [--insecure-no-auth]
                       run the hub (what the service runs)
  pair [--label L] [--ttl 10m] [--replace MACHINE]
                       print a one-time `agent-stats join …` line for a new machine; --replace lets the code
                       swap the token of MACHINE (enrolled or revoked), which a plain code never does
  machines             machines that report here (enrolled and legacy-token ones)
  revoke <machine>     revoke a machine's token
  purge <machine> [--from DAY] [--to DAY]
                       delete that machine's usage and repo rows for the days (history is kept by `revoke`)
  repos [--json]       every repository, its remote, project and tokens
  projects             the project mapping from hub.toml";

struct Args {
    config: Option<String>,
    listen: Option<String>,
    label: Option<String>,
    ttl: Duration,
    replace: Option<String>,
    from: Option<String>,
    to: Option<String>,
    insecure: bool,
    json: bool,
    rest: Vec<String>,
}

fn parse_ttl(s: &str) -> Option<Duration> {
    let (n, mul) = match s.as_bytes().last()? {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = n.parse().ok()?;
    (n > 0 && n * mul <= 86_400).then(|| Duration::from_secs(n * mul))
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        config: None,
        listen: None,
        label: None,
        ttl: Duration::from_secs(600),
        replace: None,
        from: None,
        to: None,
        insecure: false,
        json: false,
        rest: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match x.as_str() {
            "--config" => a.config = Some(val("--config")?),
            "--listen" => a.listen = Some(val("--listen")?),
            "--label" => a.label = Some(val("--label")?),
            "--ttl" => a.ttl = parse_ttl(&val("--ttl")?).ok_or("--ttl: like 10m, 600s or 1h (at most 24h)")?,
            "--replace" => a.replace = Some(val("--replace")?),
            "--from" => a.from = Some(val("--from")?),
            "--to" => a.to = Some(val("--to")?),
            "--insecure-no-auth" => a.insecure = true,
            "--json" => a.json = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.rest.push(s.to_string()),
        }
    }
    Ok(a)
}

fn load(paths: &Paths, a: &Args) -> Result<Config, String> {
    let path = a.config.as_deref().map(std::path::PathBuf::from).unwrap_or_else(|| paths.hub_toml());
    let mut cfg = Config::load(paths, &path)?;
    if let Some(l) = &a.listen {
        cfg.listen = l.clone();
    }
    Ok(cfg)
}

/// `agent-stats hub <subcommand> [args]`.
pub fn main(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if matches!(cmd, "help" | "--help" | "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let a = match parse_args(&args[1..]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("agent-stats hub {cmd}: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let paths = Paths::from_env();
    let res = match cmd {
        "serve" => serve(paths, &a),
        "pair" => pair(paths, &a),
        "machines" => machines(paths, &a),
        "revoke" => revoke(paths, &a),
        "purge" => purge(paths, &a),
        "repos" => repos(paths, &a),
        "projects" => projects(paths, &a),
        _ => Err(format!("unknown command {cmd}\n\n{USAGE}")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agent-stats hub {cmd}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn now_ms() -> f64 {
    server::Clock::real().now()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v == "true")
}

/// Starts the hub on `cfg.listen` and returns once it is accepting connections (the server runs on background
/// threads). Used by `hub serve` and by the integration tests.
pub fn start(
    cfg: Config,
    paths: Paths,
    opts: server::Options,
) -> Result<(Arc<server::Hub>, std::net::SocketAddr), String> {
    let mut hub = server::Hub::open(cfg, paths, opts)?;
    // A fresh hub has nobody enrolled yet: every report is refused until `hub pair` (or a legacy token), so starting
    // is safe and lets the service come up before the first machine joins.
    if let Err(msg) = hub.check_auth_configured() {
        eprintln!("hub: {msg}; until then every report is refused");
    }
    let (listener, addr) = http::bind(&hub.cfg.listen).map_err(|e| format!("listen on {}: {e}", hub.cfg.listen))?;
    hub.listen = addr.to_string();
    let hub = hub.into_arc();
    let (h, t, a) = (hub.clone(), hub.clone(), hub.clone());
    let policy = http::Policy {
        trusted: Box::new(move |ip| ip.is_loopback() || t.cfg.peer_trusted(ip)),
        admit: Box::new(move |req| a.admit(req)),
    };
    let limits = http::Limits { per_ip: hub.cfg.max_connections_per_ip, ..Default::default() };
    // If the accept loop ever ends, `serve` exits the process (status 1) so the service manager restarts it.
    std::thread::Builder::new()
        .name("hub-http".into())
        .spawn(move || http::serve(listener, Arc::new(move |req| h.handle(req)), policy, limits))
        .map_err(|e| e.to_string())?;
    // Reports wake this: publish on change (if on) and the streams' events, plus the streams' timers.
    let h = hub.clone();
    std::thread::Builder::new()
        .name("hub-events".into())
        .spawn(move || {
            let mut last = None;
            let mut next_live = std::time::Instant::now() + server::LIVE_EVERY;
            loop {
                let until = (std::time::Instant::now() + Duration::from_secs(1)).min(next_live);
                if h.wait_change(last, until) {
                    last = Some(std::time::Instant::now());
                    next_live = std::time::Instant::now() + server::LIVE_EVERY;
                    guarded("change", || h.on_change());
                } else if std::time::Instant::now() >= next_live {
                    next_live = std::time::Instant::now() + server::LIVE_EVERY;
                    guarded("stream timer", || h.feed_private());
                }
                guarded("stream timer", || h.feed_public());
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((hub, addr))
}

/// Runs one iteration of a background loop; a panic is logged and the loop carries on at the next tick.
fn guarded(what: &str, f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        eprintln!("hub: {what} panicked, continuing");
    }
}

fn serve(paths: Paths, a: &Args) -> Result<(), String> {
    let cfg = load(&paths, a)?;
    let test_hooks = env_flag("AGENT_STATS_TEST_HOOKS");
    let tick_ms: u64 = std::env::var("AGENT_STATS_TICK_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(15_000);
    let github = cfg.github_user.is_some().then(server::gh_fetch);
    let opts = server::Options {
        insecure_no_auth: a.insecure,
        test_hooks,
        clock: server::Clock::from_env(test_hooks),
        github,
        publish_on_change: tick_ms != 0,
    };
    let admin = paths.admin_token();
    let (hub, addr) = start(cfg, paths, opts)?;
    eprintln!("hub {} ({}) on http://{addr}", hub.cfg.name, hub.hub_id);
    eprintln!("hub: admin token for the dashboard from other machines: {}", admin.display());
    if a.insecure {
        eprintln!("hub: WARNING --insecure-no-auth: anyone who can reach {addr} can report");
    }
    if hub.cfg.legacy_token.is_some() {
        eprintln!("hub: WARNING legacy_token is set: its holder can report as any machine that has no token of its own; remove it once every machine is paired");
    }
    if test_hooks {
        eprintln!("hub: WARNING test hooks on (AGENT_STATS_TEST_HOOKS): loopback clients can set the clock");
    }
    for t in &hub.cfg.publish {
        eprintln!("hub: publishing to {} ({})", t.name, t.url);
    }
    let _ = agent_stats_common::write_status(&hub.status());
    // On the local network as _agent-stats._tcp, unless only this machine can reach it. Lives as long as `serve`.
    let node = config::hostname();
    let node = node.strip_suffix(".local").unwrap_or(&node).to_string();
    let os = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let _bonjour = (!addr.ip().is_loopback())
        .then(|| {
            let txt = [
                ("hubId", hub.hub_id.as_str()),
                ("name", hub.cfg.name.as_str()),
                ("node", node.as_str()),
                ("v", agent_stats_common::VERSION),
                ("os", os),
            ];
            bonjour::advertise(&node, addr.port(), &txt)
        })
        .flatten();
    // GitHub contributions: now, then hourly.
    if hub.cfg.github_user.is_some() {
        let h = hub.clone();
        std::thread::Builder::new()
            .name("hub-github".into())
            .spawn(move || loop {
                guarded("github fetch", || h.fetch_github());
                std::thread::sleep(Duration::from_secs(3600));
            })
            .map_err(|e| format!("github thread: {e}"))?;
    }
    // Status heartbeat, well inside HEARTBEAT_SECS.
    let h = hub.clone();
    std::thread::Builder::new()
        .name("hub-heartbeat".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(agent_stats_common::HEARTBEAT_SECS / 2));
            guarded("status heartbeat", || {
                if let Err(e) = agent_stats_common::write_status(&h.status()) {
                    eprintln!("hub: status file: {e}");
                }
            });
        })
        .map_err(|e| format!("heartbeat thread: {e}"))?;
    if tick_ms == 0 {
        loop {
            std::thread::park();
        }
    }
    // Like setInterval: a tick every TICK, measured from the start.
    let start = std::time::Instant::now();
    let mut n: u32 = 1;
    loop {
        let next = start + Duration::from_millis(tick_ms) * n;
        if let Some(wait) = next.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
        guarded("tick", || hub.tick());
        n = n.saturating_add(1);
        // A long stall (sleep, a hung publish) shouldn't cause a burst of catch-up ticks.
        while start + Duration::from_millis(tick_ms) * n < std::time::Instant::now() {
            n = n.saturating_add(1);
        }
    }
}

fn open_db(paths: &Paths) -> Result<rusqlite::Connection, String> {
    db::open(&paths.db()).map_err(|e| format!("{}: {e}", paths.db().display()))
}

fn pair(paths: Paths, a: &Args) -> Result<(), String> {
    let mut cfg = load(&paths, a)?;
    let db = open_db(&paths)?;
    if let Some(m) = a.replace.as_deref().filter(|m| m.is_empty() || m.chars().any(validate::is_unsafe_char)) {
        return Err(format!("--replace {}: not a machine name", admin::printable(m)));
    }
    let code = auth::create_code_for(&db, a.label.as_deref(), a.replace.as_deref(), a.ttl, now_ms())
        .map_err(|e| e.to_string())?;
    // The advertised route, else the address the running hub actually listens on (its status file), else config.
    let running = agent_stats_common::read_status("hub")
        .filter(|s| agent_stats_common::pid_alive(s.pid))
        .and_then(|s| s.detail.get("listen").and_then(|l| l.as_str()).map(str::to_string));
    if let (true, Some(l)) = (cfg.advertise.is_empty(), running) {
        cfg.listen = l;
    }
    let url = cfg.routes().into_iter().next().unwrap_or_else(|| format!("http://{}", cfg.listen));
    let mins = a.ttl.as_secs().div_ceil(60);
    println!("agent-stats join {url} --code {code}");
    eprintln!(
        "One-time code{}, valid for {mins} minute{}. Run the line above on the new machine.",
        a.label.as_deref().map(|l| format!(" for {}", admin::printable(l))).unwrap_or_default(),
        if mins == 1 { "" } else { "s" }
    );
    if let Some(m) = &a.replace {
        eprintln!("The code only works for the machine name {}, and replaces its token.", admin::printable(m));
    }
    Ok(())
}

fn ago(ms: Option<f64>, now: f64) -> String {
    let Some(ms) = ms else { return "never".into() };
    let s = ((now - ms) / 1000.0).max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=172_799 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn table(rows: Vec<Vec<String>>) {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let w: Vec<usize> =
        (0..n).map(|i| rows.iter().map(|r| r.get(i).map_or(0, |c| c.chars().count())).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{c:<width$}", width = w[i])).collect();
        println!("{}", line.join("  ").trim_end());
    }
}

fn s(v: Option<&J>) -> String {
    match v {
        Some(J::Str(s)) => admin::printable(s),
        Some(J::Num(n)) => json::js_number(*n),
        Some(J::Bool(b)) => b.to_string(),
        _ => "-".into(),
    }
}

fn machines(paths: Paths, a: &Args) -> Result<(), String> {
    let db = open_db(&paths)?;
    let list = auth::machines(&db).map_err(|e| e.to_string())?;
    if a.json {
        println!("{}", J::Arr(list.into_iter().map(J::Obj).collect()).stringify());
        return Ok(());
    }
    let now = now_ms();
    let mut rows = vec![["MACHINE", "LABEL", "AUTH", "LAST REPORT", "VERSION", "PROTOCOL"].map(String::from).to_vec()];
    for m in &list {
        let auth = if m.get("revoked").is_some_and(J::truthy) {
            "revoked".to_string()
        } else if m.get("enrolled").is_some_and(J::truthy) {
            "token".into()
        } else {
            m.get("auth").and_then(J::as_str).unwrap_or("unknown").to_string()
        };
        rows.push(vec![
            s(m.get("machine")),
            s(m.get("label")),
            auth,
            ago(m.get("lastReport").and_then(J::as_f64), now),
            s(m.get("version")),
            s(m.get("protocol")),
        ]);
    }
    if list.is_empty() {
        println!("No machines yet. `agent-stats hub pair` prints the line to enroll one.");
    } else {
        table(rows);
    }
    Ok(())
}

fn revoke(paths: Paths, a: &Args) -> Result<(), String> {
    let [machine] = a.rest.as_slice() else { return Err("usage: agent-stats hub revoke <machine>".into()) };
    let db = open_db(&paths)?;
    if auth::revoke(&db, machine, now_ms()).map_err(|e| e.to_string())? {
        println!("Revoked {machine}'s token. Its reports are refused from now on; history is kept.");
        Ok(())
    } else {
        Err(format!(
            "{machine} has no active token (machines on the shared legacy token can't be revoked one by one; \
             rotate legacy_token_file instead)"
        ))
    }
}

fn purge(paths: Paths, a: &Args) -> Result<(), String> {
    let [machine] = a.rest.as_slice() else {
        return Err("usage: agent-stats hub purge <machine> [--from DAY] [--to DAY]".into());
    };
    let day = |name: &str, v: &Option<String>| -> Result<Option<String>, String> {
        match v {
            None => Ok(None),
            Some(d) if validate::is_day(d) && d.parse::<jiff::civil::Date>().is_ok() => Ok(Some(d.clone())),
            Some(d) => Err(format!("{name} {}: not a YYYY-MM-DD day", admin::printable(d))),
        }
    };
    let (from, to) = (day("--from", &a.from)?, day("--to", &a.to)?);
    if let (Some(f), Some(t)) = (&from, &to) {
        if f > t {
            return Err(format!("--from {f} is after --to {t}"));
        }
    }
    let mut db = open_db(&paths)?;
    let p = admin::purge(&mut db, machine, from.as_deref(), to.as_deref()).map_err(|e| e.to_string())?;
    println!(
        "Deleted {} usage rows and {} repository rows of {} ({} to {}).",
        p.usage,
        p.repos,
        admin::printable(machine),
        from.as_deref().unwrap_or("the beginning"),
        to.as_deref().unwrap_or("the end")
    );
    Ok(())
}

fn repos(paths: Paths, a: &Args) -> Result<(), String> {
    let cfg = load(&paths, a)?;
    let db = open_db(&paths)?;
    let list = server::repos(&db, &cfg, now_ms()).map_err(|e| e.to_string())?;
    if a.json {
        println!("{}", J::Arr(list.into_iter().map(J::Obj).collect()).stringify());
        return Ok(());
    }
    let mut rows =
        vec![["REPO", "PROJECT", "TODAY", "ALL TIME", "FIRST", "MACHINES", "REMOTE"].map(String::from).to_vec()];
    for r in &list {
        let machines: Vec<String> =
            r.get("machines").and_then(J::as_arr).into_iter().flatten().map(|m| s(Some(m))).collect();
        rows.push(vec![
            s(r.get("repo")),
            r.get("project").and_then(J::as_str).unwrap_or("(unassigned)").to_string(),
            s(r.get("tokensToday")),
            s(r.get("tokensAll")),
            s(r.get("firstDay")),
            machines.join(","),
            s(r.get("remote")),
        ]);
    }
    table(rows);
    Ok(())
}

fn projects(paths: Paths, a: &Args) -> Result<(), String> {
    let cfg = load(&paths, a)?;
    if cfg.projects.is_empty() {
        println!("No [[project]] in hub.toml: every repository counts as `other`.");
    }
    for p in &cfg.projects {
        println!("{:<12} {:<20} {:<8} {}", p.key, p.name, p.color, p.globs.join(" "));
    }
    let split: Vec<String> = cfg.unattributed.iter().map(|(k, f)| format!("{k} {}", json::js_number(*f))).collect();
    println!("tokens without a repository: {}", split.join(", "));
    if let Ok(db) = open_db(&paths) {
        if let Ok(list) = server::repos(&db, &cfg, now_ms()) {
            let mut by: Vec<(String, Vec<String>)> =
                cfg.project_keys().iter().map(|k| (k.to_string(), Vec::new())).collect();
            for r in &list {
                let k = r.get("project").and_then(J::as_str).unwrap_or("other").to_string();
                if let Some(e) = by.iter_mut().find(|(key, _)| *key == k) {
                    e.1.push(s(r.get("repo")));
                }
            }
            println!();
            for (k, repos) in by {
                println!("{k}: {}", if repos.is_empty() { "-".into() } else { repos.join(", ") });
            }
        }
    }
    Ok(())
}
