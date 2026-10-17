//! The reporter's subcommands: `report`, `join`, `hubs`, `status`, `doctor`.

use crate::app::{App, Config, Output};
use crate::config::{self, FileConfig, HubEntry, Settings};
use crate::hub::{self, Http, PairRequest};
use agent_stats_common::{config_dir, now_secs, pid_alive, read_status, state_dir, status_path, HEARTBEAT_SECS};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

const TICK: Duration = Duration::from_secs(15);

/// How often the hot transcripts are checked and changes reported; `STATS_TICK_MS` overrides it.
const FAST_TICK: Duration = Duration::from_secs(1);

fn fast_tick() -> Duration {
    std::env::var("STATS_TICK_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map_or(FAST_TICK, Duration::from_millis)
}

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("agent-stats: {msg}");
    ExitCode::from(2)
}

/// `agent-stats <report|join|hubs|status|doctor> [args]`; `cmd` is the first word.
pub fn cli_main(cmd: &str, args: &[String]) -> ExitCode {
    match cmd {
        "report" => report(args),
        "join" => join(args),
        "hubs" => hubs(args),
        "status" => status(args),
        "doctor" => doctor(args),
        _ => fail(&format!("unknown reporter command {cmd}")),
    }
}

// ---- report ---------------------------------------------------------------------------------------------------

const REPORT_USAGE: &str = "usage: agent-stats report [--once] [--dry | --print]";

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// What came of taking the state writer's lock.
enum Locked {
    /// Held until the file is closed.
    Held(#[allow(dead_code)] std::fs::File),
    /// The lock can't be taken (the state directory is not writable): go on without it, nothing gets saved anyway.
    Unavailable,
    /// Stopped (SIGTERM) while waiting.
    Stopped,
}

/// Takes `dir/reporter.lock` for a run that writes the state. With `once`, a lock held by another process (the
/// service) is an error; otherwise this waits for it, retrying every `retry`, with one log line.
fn writer_lock(dir: &Path, once: bool, retry: Duration, stop: &AtomicBool) -> Result<Locked, String> {
    let mut logged = false;
    loop {
        match crate::state::lock(dir) {
            Ok(Some(f)) => {
                if logged {
                    eprintln!("agent-stats: got the reporter lock; starting");
                }
                return Ok(Locked::Held(f));
            }
            Ok(None) if once => {
                return Err(format!(
                    "the reporter service is running (it holds {}); use --dry or --print",
                    dir.join(crate::state::LOCK_FILE).display()
                ))
            }
            Ok(None) => {
                if !logged {
                    eprintln!(
                        "agent-stats: another reporter holds {} (two would overwrite each other's state); waiting for \
                         it to exit, retrying every {} s",
                        dir.join(crate::state::LOCK_FILE).display(),
                        retry.as_secs_f64()
                    );
                    logged = true;
                }
                let t = Instant::now();
                while t.elapsed() < retry {
                    if stop.load(Ordering::SeqCst) {
                        return Ok(Locked::Stopped);
                    }
                    std::thread::sleep(retry.min(Duration::from_millis(200)));
                }
            }
            Err(e) => {
                eprintln!(
                    "agent-stats: can't take {}: {e}; going on without it",
                    dir.join(crate::state::LOCK_FILE).display()
                );
                return Ok(Locked::Unavailable);
            }
        }
    }
}

fn report(args: &[String]) -> ExitCode {
    let has = |f: &str| args.iter().any(|a| a == f);
    for a in args {
        if !matches!(a.as_str(), "--once" | "--dry" | "--print") {
            return fail(&format!("unknown argument {a}\n{REPORT_USAGE}"));
        }
    }
    let output = if has("--print") {
        Output::Print
    } else if has("--dry") {
        Output::Dry
    } else {
        Output::Send
    };
    let settings = match config::load() {
        Ok(s) => s,
        Err(e) => return fail(&format!("config: {e}")),
    };
    let tz = match jiff::tz::db().get(&settings.tz_name) {
        Ok(tz) => tz,
        Err(e) => return fail(&format!("timezone {}: {e}", settings.tz_name)),
    };
    if output == Output::Send {
        if settings.hubs.is_empty() {
            return fail(
                "no hub to report to: run `agent-stats join <hub-url> --code CODE` (get a code with `agent-stats hub \
                 pair` on the hub), or set HUB_URL and HUB_TOKEN; --dry / --print work without one",
            );
        }
        if settings.hubs.iter().all(|h| h.token.is_none()) {
            return fail(&format!(
                "no hub token: {} (set HUB_TOKEN, put it in {}, or join again); --dry / --print work without one",
                settings
                    .hubs
                    .iter()
                    .map(|h| format!("{} has none at {}", h.name, h.token_source))
                    .collect::<Vec<_>>()
                    .join(", "),
                config_dir().join("hub_token").display()
            ));
        }
        for h in settings.hubs.iter().filter(|h| h.token.is_none()) {
            eprintln!("agent-stats: hub {} has no token ({}); it is skipped", h.name, h.token_source);
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let dir = state_dir();
    // Only one process may write the state. --dry / --print never write it, so they need no lock.
    let _lock = if output == Output::Send {
        match writer_lock(&dir, has("--once"), TICK, &STOP) {
            Ok(Locked::Stopped) => return ExitCode::SUCCESS,
            Ok(l) => Some(l),
            Err(e) => return fail(&e),
        }
    } else {
        None
    };
    let cfg = Config {
        home: settings.home.clone(),
        roots: settings.roots.clone(),
        hubs: settings.hubs.clone(),
        machine: settings.machine.clone(),
        label: settings.label.clone(),
        state_file: dir.join("reporter-state.json"),
        js_state_file: dir.join("state.json"),
        tz_name: settings.tz_name.clone(),
        tz,
        local: jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC),
        output,
        verbose: std::env::var("STATS_VERBOSE").is_ok_and(|v| !v.is_empty() && v != "0"),
        status_file: Some(status_path("report")),
    };
    let mut app = App::new(cfg);
    if has("--once") {
        let sent = app.tick();
        app.save(true);
        // Like `node reporter.mjs --once`, whose failed report rejects the top-level await.
        return if sent { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }
    let mut seen = mtime(&settings.config_file);
    // A fast tick every `fast` (hot files only, reports what changed); every `slow`, the full tick.
    let fast = fast_tick();
    let slow = TICK.max(fast);
    let mut next_slow = Instant::now();
    loop {
        let started = Instant::now();
        if started >= next_slow {
            next_slow = started + slow;
            // `join` and `hubs remove` take effect without a restart.
            let now = mtime(&settings.config_file);
            if now != seen {
                seen = now;
                match config::load() {
                    Ok(s) => {
                        if s.machine != settings.machine || s.label != settings.label || s.tz_name != settings.tz_name {
                            eprintln!(
                                "agent-stats: config.toml changed machine, label or timezone; that takes a restart"
                            );
                        }
                        eprintln!(
                            "agent-stats: config.toml changed; hubs: {}",
                            s.hubs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ")
                        );
                        app.set_hubs(s.hubs);
                    }
                    Err(e) => {
                        eprintln!("agent-stats: config.toml changed but can't be read, keeping the old hubs: {e}")
                    }
                }
            }
            app.tick_slow();
        } else {
            app.tick_fast();
        }
        app.save(false);
        while started.elapsed() < fast && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50).min(fast));
        }
        if STOP.load(Ordering::SeqCst) {
            app.save(true);
            return ExitCode::SUCCESS;
        }
    }
}

// ---- join -----------------------------------------------------------------------------------------------------

const JOIN_USAGE: &str = "usage: agent-stats join <hub-url> [--code CODE] [--name NAME]";

/// Routes in order, without repeats.
fn dedupe(routes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in routes {
        let r = config::normalize_url(&r);
        if !r.is_empty() && !out.iter().any(|o| config::same_route(o, &r)) {
            out.push(r);
        }
    }
    out
}

fn join(args: &[String]) -> ExitCode {
    let (mut url, mut code, mut name) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--code" => code = it.next().cloned(),
            "--name" => name = it.next().cloned(),
            "-h" | "--help" => {
                println!("{JOIN_USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with("--code=") => code = Some(s["--code=".len()..].to_owned()),
            s if s.starts_with("--name=") => name = Some(s["--name=".len()..].to_owned()),
            s if !s.starts_with('-') && url.is_none() => url = Some(config::normalize_url(s)),
            s => return fail(&format!("unexpected argument {s}\n{JOIN_USAGE}")),
        }
    }
    let Some(url) = url else { return fail(JOIN_USAGE) };
    let http = Http::new();
    let settings = match config::load() {
        Ok(s) => s,
        Err(e) => return fail(&format!("config: {e} (fix or move it aside, then join again)")),
    };
    let Some(code) = code.filter(|c| !c.trim().is_empty()) else {
        return match hub::well_known(&http, &url) {
            Ok(d) if d.role.as_deref().is_none_or(|r| r == "hub") => {
                println!(
                    "{url} is the agent-stats hub {}{}.\nJoining it needs a pairing code. On the hub run:\n\n  \
                     agent-stats hub pair --label \"{}\"\n\nthen here:\n\n  agent-stats join {url} --code <CODE>",
                    d.name.as_deref().map(|n| format!("\"{n}\" ")).unwrap_or_default(),
                    d.hub_id.as_deref().map(|i| format!("(id {i})")).unwrap_or_default(),
                    settings.label
                );
                ExitCode::from(2)
            }
            Ok(d) => fail(&format!("{url} is an agent-stats {}, not a hub", d.role.unwrap_or_default())),
            Err(e) => fail(&format!(
                "{url} does not answer as an agent-stats hub ({e}). Check the address (is the hub running? is it \
                 reachable from here, e.g. over Tailscale?), then: agent-stats join {url} --code <CODE>"
            )),
        };
    };
    let dir = config_dir();
    let j = match pair_and_save(&http, &settings, &dir, &url, &code, name) {
        Ok(j) => j,
        Err(e) => return fail(&e),
    };
    let (name, entry, token_file, replaced) =
        (&j.entry.name, &j.entry, dir.join(j.entry.token_file.as_deref().unwrap_or("")), j.replaced);
    let res = &j.res;
    println!(
        "{} hub \"{name}\" (id {}) as machine \"{}\" (label \"{}\").",
        if replaced { "Re-joined" } else { "Joined" },
        res.hub_id,
        settings.machine,
        settings.label
    );
    if let Some(wanted) = &j.renamed {
        println!(
            "  Another hub is already named \"{wanted}\", so this one is \"{name}\" (choose with --name; the other \
             hub is kept)."
        );
    }
    println!("  routes: {}", entry.routes.join(", "));
    match res.timezone.as_deref() {
        Some(tz) if j.adopted => println!("  timezone: {tz} (the hub's)"),
        Some(tz) if tz != settings.tz_name => println!(
            "  warning: the hub counts days in {tz}, this machine in {} ({}): days near midnight will land on \
             different dates. Set timezone = \"{tz}\" in {} (then restart the reporter) unless that is on purpose.",
            settings.tz_name,
            settings.tz_from,
            settings.config_file.display()
        ),
        _ => {}
    }
    println!("  token:  {} (0600)", token_file.display());
    println!("  config: {}", settings.config_file.display());
    let legacy = settings.hubs.iter().find(|h| h.legacy);
    if let Some(l) = legacy {
        if entry.routes.iter().any(|r| l.routes.iter().any(|lr| config::same_route(r, lr))) {
            println!("  This hub replaces the one from HUB_URL / hub_token; you can drop those settings.");
        } else {
            println!("  The hub from HUB_URL / hub_token ({}) keeps getting reports too.", l.routes.join(", "));
        }
    }
    println!("A running reporter picks this up within 15 s. Check with: agent-stats status (or agent-stats doctor).");
    ExitCode::SUCCESS
}

/// What `join` did.
struct Joined {
    res: hub::PairResponse,
    entry: HubEntry,
    replaced: bool,
    /// The name it would have had, taken by another hub.
    renamed: Option<String>,
    /// Whether this machine took the hub's timezone.
    adopted: bool,
}

/// Swaps the code for a token, saves it under `dir/secrets/`, and adds the hub's entry to `settings.config_file`,
/// replacing the entry of the same hub (same id, or an id-less entry with the same route). A name already used by
/// another hub gets a suffix (`hub-2`); an explicit `--name` that is taken is refused.
fn pair_and_save(
    http: &Http,
    settings: &Settings,
    dir: &Path,
    url: &str,
    code: &str,
    name: Option<String>,
) -> Result<Joined, String> {
    let req = PairRequest {
        code: code.trim(),
        machine: &settings.machine,
        label: &settings.label,
        platform: crate::report::platform(),
        version: agent_stats_common::VERSION,
    };
    let res = hub::pair(http, url, &req).map_err(|e| format!("pairing with {url} failed: {e}"))?;
    let rel = format!("secrets/hub-{}.token", res.hub_id);
    let token_file = dir.join(&rel);
    agent_stats_common::write_secret(&token_file, &res.token)
        .map_err(|e| format!("writing {}: {e}", token_file.display()))?;
    let routes = dedupe(std::iter::once(url.to_owned()).chain(res.routes.iter().cloned()));
    let mut file = settings.file.clone();
    // The entry this hub replaces: the same id, or an id-less entry with one of its routes (a hub configured by
    // hand before it had an id). A different hub that only shares the name is kept.
    let same = |h: &HubEntry| match h.id.as_deref() {
        Some(id) => id == res.hub_id,
        None => h.routes.iter().any(|r| routes.iter().any(|n| config::same_route(r, n))),
    };
    let old = file.hubs.iter().position(same);
    let wanted = match (&name, old) {
        (Some(n), _) if !n.trim().is_empty() => {
            let n = n.trim().to_owned();
            if let Some(other) = file.hubs.iter().find(|h| h.name == n && !same(h)) {
                return Err(format!(
                    "another hub is already named {n} (id {}, {}): choose another --name, or remove it first with                      `agent-stats hubs remove {n}`",
                    other.id.as_deref().unwrap_or("-"),
                    other.routes.join(", ")
                ));
            }
            n
        }
        // Re-joining keeps the name it had.
        (_, Some(i)) => file.hubs[i].name.clone(),
        _ => res.name.clone().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "hub".into()),
    };
    let taken = |n: &str| file.hubs.iter().any(|h| h.name == n && !same(h));
    let name = if taken(&wanted) {
        (2..).map(|i| format!("{wanted}-{i}")).find(|n| !taken(n)).unwrap_or_default()
    } else {
        wanted.clone()
    };
    let entry = HubEntry { name: name.clone(), id: Some(res.hub_id.clone()), routes, token_file: Some(rel) };
    let replaced = old.is_some();
    match old {
        Some(i) => {
            file.hubs[i] = entry.clone();
            // Any other entry for the same hub (an older duplicate) goes too.
            let mut seen = false;
            file.hubs.retain(|h| {
                let dup = h.id.as_deref() == Some(&res.hub_id);
                let keep = !dup || !seen;
                seen |= dup;
                keep
            });
        }
        None => file.hubs.push(entry.clone()),
    }
    let renamed = (name != wanted).then_some(wanted);
    // Days must be cut in the same zone on both sides. A machine with no zone of its own (none set, no counts yet)
    // takes the hub's.
    let adopted = match &res.timezone {
        Some(tz) if settings.tz_from == "system" && file.timezone.is_none() && jiff::tz::db().get(tz).is_ok() => {
            file.timezone = Some(tz.clone());
            true
        }
        _ => false,
    };
    config::write_file(&settings.config_file, &file).map_err(|e| format!("writing config: {e}"))?;
    Ok(Joined { res, entry, replaced, renamed, adopted })
}

// ---- hubs -----------------------------------------------------------------------------------------------------

fn hubs(args: &[String]) -> ExitCode {
    let settings = match config::load() {
        Ok(s) => s,
        Err(e) => return fail(&format!("config: {e}")),
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if settings.hubs.is_empty() {
                println!("No hubs. Add one with: agent-stats join <hub-url> --code CODE");
                return ExitCode::SUCCESS;
            }
            for h in &settings.hubs {
                println!(
                    "{}{}\n  id:     {}\n  routes: {}\n  token:  {} ({})",
                    h.name,
                    if h.legacy { "  (from HUB_URL / HUB_TOKEN / hub_token)" } else { "" },
                    h.id.as_deref().unwrap_or("-"),
                    h.routes.join(", "),
                    h.token_source,
                    if h.token.is_some() { "present" } else { "MISSING" }
                );
            }
            ExitCode::SUCCESS
        }
        Some("remove") => {
            let Some(name) = args.get(1) else { return fail("usage: agent-stats hubs remove <name>") };
            let mut file: FileConfig = settings.file.clone();
            let Some(i) = file.hubs.iter().position(|h| &h.name == name) else {
                if settings.hubs.iter().any(|h| &h.name == name && h.legacy) {
                    return fail(&format!(
                        "{name} comes from the environment: unset HUB_URL / HUB_TOKEN in the service, or remove {}",
                        config_dir().join("hub_token").display()
                    ));
                }
                return fail(&format!("no hub named {name} (see agent-stats hubs list)"));
            };
            let gone = file.hubs.remove(i);
            if let Err(e) = config::write_file(&settings.config_file, &file) {
                return fail(&format!("writing config: {e}"));
            }
            if let Some(t) = gone.token_file.as_deref() {
                let p = config::token_path(&config_dir(), t);
                // Another entry may share the file (same hub under two names).
                if !file.hubs.iter().any(|h| h.token_file.as_deref() == Some(t)) {
                    let _ = std::fs::remove_file(&p);
                }
            }
            println!("Removed hub {name}. A running reporter stops reporting to it within 15 s.");
            println!("(The hub still has this machine's data; revoke its token there with `agent-stats hub revoke`.)");
            ExitCode::SUCCESS
        }
        Some(other) => fail(&format!("unknown hubs command {other}\nusage: agent-stats hubs [list | remove <name>]")),
    }
}

// ---- status ---------------------------------------------------------------------------------------------------

fn ago(secs: Option<u64>) -> String {
    let Some(t) = secs.filter(|t| *t > 0) else { return "never".into() };
    let d = now_secs().saturating_sub(t);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn status(_args: &[String]) -> ExitCode {
    let path = status_path("report");
    let Some(st) = read_status("report") else {
        println!("No reporter status at {}: the reporter is not running here (or never ran).", path.display());
        println!("Start it with `agent-stats service install report`, or run `agent-stats doctor`.");
        return ExitCode::from(1);
    };
    let alive = pid_alive(st.pid);
    let age = now_secs().saturating_sub(st.heartbeat_at);
    let fresh = age <= 2 * HEARTBEAT_SECS + 15;
    let d = &st.detail;
    println!(
        "reporter {} pid {} ({}), started {}, heartbeat {}{}",
        st.version,
        st.pid,
        if alive { "running" } else { "NOT RUNNING" },
        ago(Some(st.started_at)),
        ago(Some(st.heartbeat_at)),
        if fresh { "" } else { " (STALE)" }
    );
    println!("machine {} ({})", d["machine"].as_str().unwrap_or("?"), d["label"].as_str().unwrap_or("?"));
    let hubs = d["hubs"].as_array().cloned().unwrap_or_default();
    if hubs.is_empty() {
        println!("no hubs");
    } else {
        let rows: Vec<[String; 5]> = hubs
            .iter()
            .map(|h| {
                [
                    h["name"].as_str().unwrap_or("?").to_owned(),
                    h["route"].as_str().unwrap_or("-").to_owned(),
                    ago(h["lastOkAt"].as_u64()),
                    h["unacked"].as_u64().unwrap_or(0).to_string(),
                    h["lastError"].as_str().unwrap_or("").to_owned(),
                ]
            })
            .collect();
        let head = ["HUB", "ROUTE", "LAST OK", "UNACKED", "ERROR"];
        let w: Vec<usize> =
            (0..4).map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0).max(head[i].len())).collect();
        let line = |r: [&str; 5]| {
            format!(
                "{:w0$}  {:w1$}  {:w2$}  {:>w3$}  {}",
                r[0],
                r[1],
                r[2],
                r[3],
                r[4],
                w0 = w[0],
                w1 = w[1],
                w2 = w[2],
                w3 = w[3]
            )
            .trim_end()
            .to_owned()
        };
        println!("{}", line(head));
        for r in &rows {
            println!("{}", line([&r[0], &r[1], &r[2], &r[3], &r[4]]));
        }
    }
    let healthy = alive && fresh && hubs.iter().all(|h| h["lastError"].is_null());
    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

// ---- doctor ---------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    fails: usize,
    warns: usize,
}

impl Report {
    fn ok(&mut self, msg: &str) {
        println!("  ok    {msg}");
    }
    fn warn(&mut self, msg: &str, fix: &str) {
        self.warns += 1;
        println!("  warn  {msg}");
        if !fix.is_empty() {
            println!("        fix: {fix}");
        }
    }
    fn fail(&mut self, msg: &str, fix: &str) {
        self.fails += 1;
        println!("  FAIL  {msg}");
        if !fix.is_empty() {
            println!("        fix: {fix}");
        }
    }
}

fn doctor(_args: &[String]) -> ExitCode {
    let mut r = Report::default();
    println!("config");
    let file = config::config_file();
    let settings: Settings = match config::load() {
        Ok(s) => {
            if file.exists() {
                r.ok(&format!("{} parses", file.display()));
            } else {
                r.ok(&format!("no {} (using defaults and the environment)", file.display()));
            }
            s
        }
        Err(e) => {
            r.fail(&e, "fix the syntax, or move the file aside and `agent-stats join` again");
            println!("\n1 problem; fix it and run `agent-stats doctor` again.");
            return ExitCode::from(1);
        }
    };
    let mut names = std::collections::HashSet::new();
    for h in &settings.file.hubs {
        if !names.insert(h.name.as_str()) {
            r.fail(&format!("two [[hub]] entries are named {}", h.name), "rename or remove one in config.toml");
        }
    }
    r.ok(&format!("machine \"{}\", label \"{}\"", settings.machine, settings.label));
    match jiff::tz::db().get(&settings.tz_name) {
        Ok(_) => r.ok(&format!("timezone {} (from {}; must match the hub's)", settings.tz_name, settings.tz_from)),
        Err(e) => r.fail(&format!("timezone {}: {e}", settings.tz_name), "use an IANA name such as Europe/Paris"),
    }

    println!("transcripts");
    let listed = crate::scan::Scanner::default().list(&settings.roots);
    let count = |a: crate::state::Agent| listed.iter().filter(|l| l.agent == a).count();
    for (agent, root, var) in [
        (crate::state::Agent::Claude, &settings.roots.claude_projects, "CLAUDE_CONFIG_DIR"),
        (crate::state::Agent::Codex, &settings.roots.codex, "CODEX_HOME"),
        (crate::state::Agent::Grok, &settings.roots.grok, "GROK_HOME"),
    ] {
        if root.is_dir() {
            r.ok(&format!("{}: {} ({} transcripts)", agent.name(), root.display(), count(agent)));
        } else {
            println!("  -     {}: {} does not exist (set {var} if it lives elsewhere)", agent.name(), root.display());
        }
    }
    if listed.is_empty() {
        r.warn(
            "no transcripts found",
            "run an agent once, or point CLAUDE_CONFIG_DIR / CODEX_HOME / GROK_HOME at them",
        );
    }

    println!("hubs");
    if settings.hubs.is_empty() {
        r.fail(
            "no hub configured",
            "agent-stats join <hub-url> --code CODE (code from `agent-stats hub pair` on the hub)",
        );
    }
    let http = Http::new();
    for h in &settings.hubs {
        println!("  {}{}", h.name, if h.legacy { " (from HUB_URL / HUB_TOKEN / hub_token)" } else { "" });
        match &h.token {
            Some(_) => {
                r.ok(&format!("token from {}", h.token_source));
                #[cfg(unix)]
                if !h.legacy || h.token_source != "HUB_TOKEN" {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(m) = std::fs::metadata(&h.token_source) {
                        let mode = m.permissions().mode() & 0o777;
                        if mode & 0o077 != 0 {
                            r.warn(
                                &format!("{} is mode {mode:o}, readable by others", h.token_source),
                                &format!("chmod 600 {}", h.token_source),
                            );
                        }
                    }
                }
            }
            None => r.fail(
                &format!("no token ({})", h.token_source),
                &if h.legacy {
                    "set HUB_TOKEN or write the token to the hub_token file".to_owned()
                } else {
                    format!("join again: agent-stats join {} --code CODE", h.routes.first().map_or("<url>", |s| s))
                },
            ),
        }
        let mut reachable = false;
        for route in &h.routes {
            match hub::well_known(&http, route) {
                Ok(d) => {
                    let proto = d.protocol.unwrap_or(1);
                    match h.id.as_deref().map(|id| hub::check(&d, id)) {
                        Some(Err(e)) => r.fail(
                            &format!("{route} {e}"),
                            "this address now points at another hub: fix the route in config.toml or join again",
                        ),
                        _ => {
                            reachable = true;
                            r.ok(&format!(
                                "{route} answers: {} {} (id {}, version {}, protocol {proto})",
                                d.role.as_deref().unwrap_or("hub"),
                                d.name.as_deref().unwrap_or("?"),
                                d.hub_id.as_deref().unwrap_or("?"),
                                d.version.as_deref().unwrap_or("?")
                            ));
                            if proto < agent_stats_common::PROTOCOL {
                                r.warn(
                                    &format!(
                                        "the hub speaks protocol {proto}, this reporter {}",
                                        agent_stats_common::PROTOCOL
                                    ),
                                    "update the hub (agent-stats update on the hub machine)",
                                );
                            }
                            if let Some(tz) = d.timezone.as_deref().filter(|tz| *tz != settings.tz_name) {
                                r.warn(
                                    &format!("the hub counts days in {tz}, this machine in {}", settings.tz_name),
                                    &format!(
                                        "set timezone = \"{tz}\" in {}, then restart the reporter",
                                        settings.config_file.display()
                                    ),
                                );
                            }
                        }
                    }
                }
                Err(e) if h.id.is_none() => {
                    // A hub from the legacy environment may be the JS hub, which has no descriptor.
                    println!("  -     {route}: no descriptor ({e}); fine for the old JS hub, see `status` for reports");
                }
                Err(e) => r.warn(&format!("{route}: {e}"), ""),
            }
        }
        if !reachable && h.id.is_some() {
            r.fail(
                &format!("no route of {} answers as hub {}", h.name, h.id.as_deref().unwrap_or("?")),
                "is the hub running and reachable from here (Tailscale up? `agent-stats hub serve` running)?",
            );
        }
    }

    println!("service");
    match read_status("report") {
        None => r.fail(
            &format!("no status file at {}: the reporter is not running", status_path("report").display()),
            "agent-stats service install report",
        ),
        Some(st) => {
            let age = now_secs().saturating_sub(st.heartbeat_at);
            if !pid_alive(st.pid) {
                r.fail(
                    &format!("reporter pid {} is not running (last heartbeat {})", st.pid, ago(Some(st.heartbeat_at))),
                    "agent-stats service install report (or restart the service)",
                );
            } else if age > 2 * HEARTBEAT_SECS + 15 {
                r.fail(
                    &format!("reporter pid {} is alive but its heartbeat is {age}s old", st.pid),
                    "restart the service; check its logs",
                );
            } else {
                r.ok(&format!(
                    "reporter {} running (pid {}, heartbeat {})",
                    st.version,
                    st.pid,
                    ago(Some(st.heartbeat_at))
                ));
            }
            for hs in st.detail["hubs"].as_array().into_iter().flatten() {
                if let Some(e) = hs["lastError"].as_str() {
                    r.warn(&format!("hub {}: {e}", hs["name"].as_str().unwrap_or("?")), "");
                }
            }
        }
    }
    println!();
    if r.fails == 0 && r.warns == 0 {
        println!("All good.");
        ExitCode::SUCCESS
    } else {
        println!("{} problem(s), {} warning(s).", r.fails, r.warns);
        if r.fails == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_routes() {
        assert_eq!(
            dedupe(["http://a:1/".into(), "a:1".into(), "https://b".into(), "HTTP://A:1".into()]),
            ["http://a:1", "https://b"]
        );
    }

    #[test]
    fn join_saves_the_token_and_the_hub() {
        use crate::hub::tests::serve;
        let d = crate::ingest::tests::tmpdir("join");
        let fake = serve(|r| {
            match r.path.as_str() {
            "/pair" if r.body.contains(r#""code":"K7Q2-9XMD""#) => (
                200,
                r#"{"hubId":"6f1c","name":"home","token":"secret-tok","routes":["http://mac-mini:7777","http://100.1.2.3:7777"]}"#.into(),
            ),
            "/pair" => (403, r#"{"error":"invalid or expired code"}"#.into()),
            _ => (404, String::new()),
        }
        });
        let cf = d.join("config.toml");
        std::fs::write(&cf, "machine = \"m1\"\n\n[[hub]]\nname = \"home\"\nid = \"old\"\nroutes = [\"http://x\"]\n")
            .unwrap();
        let settings =
            config::resolve(config::read_file(&cf).unwrap(), cf.clone(), &d, &|k| (k == "HOME").then(|| "/h".into()));
        let http = Http::new();
        let e = pair_and_save(&http, &settings, &d, &fake.url, "WRONG", None).err().unwrap();
        assert!(e.contains("403") && e.contains("expired"), "{e}");
        let j = pair_and_save(&http, &settings, &d, &fake.url, " K7Q2-9XMD ", None).unwrap();
        // The existing "home" is another hub (id old): it is kept, and this one gets a free name.
        assert!(!j.replaced);
        assert_eq!((j.entry.name.as_str(), j.renamed.as_deref()), ("home-2", Some("home")));
        let req = fake.reqs.lock().unwrap().last().unwrap().clone();
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!((body["machine"].as_str(), body["code"].as_str()), (Some("m1"), Some("K7Q2-9XMD")));
        assert_eq!(body["version"].as_str(), Some(agent_stats_common::VERSION));
        // The token is a 0600 file; config.toml has one entry for this hub, the url used first.
        let tf = d.join("secrets/hub-6f1c.token");
        assert_eq!(agent_stats_common::read_secret(&tf).as_deref(), Some("secret-tok"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&tf).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let file = config::read_file(&cf).unwrap();
        assert_eq!(file.machine.as_deref(), Some("m1"));
        assert_eq!(file.hubs.len(), 2);
        assert_eq!((file.hubs[0].name.as_str(), file.hubs[0].id.as_deref()), ("home", Some("old")));
        assert_eq!(file.hubs[1].id.as_deref(), Some("6f1c"));
        assert_eq!(file.hubs[1].routes, [fake.url.as_str(), "http://mac-mini:7777", "http://100.1.2.3:7777"]);
        let s = config::resolve(file, cf.clone(), &d, &|k| (k == "HOME").then(|| "/h".into()));
        assert_eq!(s.hubs[1].token.as_deref(), Some("secret-tok"));
        // Joining the same hub again replaces its entry (same id) and keeps its name.
        let j = pair_and_save(&http, &s, &d, &fake.url, "K7Q2-9XMD", None).unwrap();
        assert!(j.replaced && j.renamed.is_none());
        assert_eq!(j.entry.name, "home-2");
        let file = config::read_file(&cf).unwrap();
        assert_eq!(file.hubs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["home", "home-2"]);
        // An explicit --name taken by another hub is refused; a free one is used.
        let s = config::resolve(file, cf.clone(), &d, &|k| (k == "HOME").then(|| "/h".into()));
        let e = pair_and_save(&http, &s, &d, &fake.url, "K7Q2-9XMD", Some("home".into())).err().unwrap();
        assert!(e.contains("already named home") && e.contains("--name"), "{e}");
        let j = pair_and_save(&http, &s, &d, &fake.url, "K7Q2-9XMD", Some("mini".into())).unwrap();
        assert!(j.replaced);
        let file = config::read_file(&cf).unwrap();
        assert_eq!(file.hubs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["home", "mini"]);
        // An id-less entry (configured by hand) with one of the hub's routes is the same hub: replaced.
        std::fs::write(&cf, "[[hub]]\nname = \"hub\"\nroutes = [\"http://100.1.2.3:7777/\"]\n").unwrap();
        let s =
            config::resolve(config::read_file(&cf).unwrap(), cf.clone(), &d, &|k| (k == "HOME").then(|| "/h".into()));
        let j = pair_and_save(&http, &s, &d, &fake.url, "K7Q2-9XMD", None).unwrap();
        assert!(j.replaced && j.renamed.is_none());
        let file = config::read_file(&cf).unwrap();
        assert_eq!(file.hubs.len(), 1);
        assert_eq!((file.hubs[0].name.as_str(), file.hubs[0].id.as_deref()), ("hub", Some("6f1c")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_state_writer_at_a_time() {
        let d = crate::ingest::tests::tmpdir("lock");
        let stop = AtomicBool::new(false);
        let Ok(Locked::Held(service)) = writer_lock(&d, false, Duration::from_millis(50), &stop) else { panic!() };
        // `report --once` beside the service: refused, with what to do instead.
        let e = writer_lock(&d, true, Duration::from_millis(50), &stop).err().unwrap();
        assert!(e.contains("the reporter service is running") && e.contains("--dry or --print"), "{e}");
        // A second service waits until the first exits.
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(service);
        });
        let started = Instant::now();
        assert!(matches!(writer_lock(&d, false, Duration::from_millis(50), &stop), Ok(Locked::Held(_))));
        assert!(started.elapsed() >= Duration::from_millis(100));
        t.join().unwrap();
        // Stopped while waiting.
        let _held = crate::state::lock(&d).unwrap().unwrap();
        stop.store(true, Ordering::SeqCst);
        assert!(matches!(writer_lock(&d, false, Duration::from_millis(50), &stop), Ok(Locked::Stopped)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
