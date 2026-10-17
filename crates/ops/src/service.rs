//! `agent-stats service install|uninstall|status`: systemd --user units (Linux) and LaunchAgents (macOS).

use crate::layout::{self, link_bin};
use crate::{Context, Ctx, Platform, Res, ROLES};
use agent_stats_common::{atomic_write, now_secs, pid_alive, Status};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Environment of the legacy JS installs that is worth keeping, so a legacy machine works before any config.toml.
pub const CARRY_ENV: [&str; 8] =
    ["HUB_URL", "STATS_LABEL", "STATS_MACHINE", "STATS_TZ", "HOST", "PORT", "PUBLISH_URL", "GITHUB_USER"];

/// The scheduled `update --auto` job. Not in [`ROLES`]: it writes no heartbeat, so the update health trial and
/// `installed_roles` must never see it.
pub const UPDATER: &str = "updater";
pub const DEFAULT_EVERY_SECS: u64 = 15 * 60;
pub const MIN_EVERY_SECS: u64 = 5 * 60;

pub fn check_role(role: &str) -> Res<()> {
    if ROLES.contains(&role) || role == UPDATER {
        Ok(())
    } else {
        Err(format!("usage: unknown role {role:?} (expected report, hub, relay or updater)"))
    }
}

/// Arguments after the binary path.
pub fn role_args(role: &str) -> &'static [&'static str] {
    match role {
        "report" => &["report"],
        "hub" => &["hub", "serve"],
        UPDATER => &["update", "--auto"],
        _ => &["relay", "serve"],
    }
}

/// `15m`, `1h`, `900s` -> seconds; at least [`MIN_EVERY_SECS`].
pub fn parse_every(s: &str) -> Res<u64> {
    let bad = || format!("usage: --every {s:?} is not a duration (examples: 15m, 1h, 900s)");
    let unit = s.chars().last().ok_or_else(bad)?;
    let mult = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return Err(bad()),
    };
    let n: u64 = s[..s.len() - 1].parse().map_err(|_| bad())?;
    let secs = n.checked_mul(mult).ok_or_else(bad)?;
    if secs < MIN_EVERY_SECS {
        return Err(format!("usage: --every {s} is too short (minimum 5m)"));
    }
    Ok(secs)
}

/// `900` -> `15m`, `3600` -> `1h`, `90` -> `90s`.
pub fn fmt_every(secs: u64) -> String {
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

pub fn timer_name() -> String {
    "agent-stats-updater.timer".into()
}

/// `agent-stats-reporter.service` for report (the legacy unit's name), `agent-stats-<role>.service` otherwise.
pub fn unit_name(role: &str) -> String {
    format!("agent-stats-{}.service", if role == "report" { "reporter" } else { role })
}

pub fn launchd_label(role: &str) -> String {
    format!("dev.agent-stats.{role}")
}

/// Labels of the old installs (`stats/install.sh`); report was called reporter.
pub fn legacy_label(role: &str) -> Option<String> {
    match role {
        "report" => Some("com.kilcazn.agent-stats.reporter".into()),
        "hub" => Some("com.kilcazn.agent-stats.hub".into()),
        _ => None,
    }
}

/// Status files are named after the role; the reporter's may be `report-status.json` or `reporter-status.json`.
pub fn read_role_status(ctx: &Ctx, role: &str) -> Option<Status> {
    let names: &[&str] = if role == "report" { &["report", "reporter"] } else { &[role] };
    names.iter().find_map(|n| {
        let bytes = fs::read(ctx.state_dir.join(format!("{n}-status.json"))).ok()?;
        serde_json::from_slice::<Status>(&bytes).ok()
    })
}

impl Ctx {
    pub fn unit_path(&self, role: &str) -> PathBuf {
        self.unit_dir.join(unit_name(role))
    }
    pub fn plist_path(&self, label: &str) -> PathBuf {
        self.agents_dir.join(format!("{label}.plist"))
    }
    pub fn service_file(&self, role: &str) -> PathBuf {
        match self.platform {
            Platform::Linux => self.unit_path(role),
            Platform::Mac => self.plist_path(&launchd_label(role)),
        }
    }
    pub fn service_installed(&self, role: &str) -> bool {
        self.service_file(role).is_file()
    }
    pub fn timer_path(&self) -> PathBuf {
        self.unit_dir.join(timer_name())
    }
    pub fn log_path(&self, role: &str) -> PathBuf {
        self.state_dir.join("logs").join(format!("{role}.log"))
    }
}

fn run(program: &str, args: &[&str]) -> Res<()> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("`{program} {}` failed ({}): {}", args.join(" "), out.status, err.trim()))
    }
}

fn ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- rendering

/// Rejects control characters (tab is fine): they break unit files and make plists invalid XML.
fn no_newlines(s: &str) -> Res<()> {
    if s.chars().any(|c| c.is_control() && c != '\t') {
        Err(format!("value contains a control character: {s:?}"))
    } else {
        Ok(())
    }
}

/// systemd rejects `'`, `"`, `\` and control characters in an executable path whatever the quoting; say so before any
/// existing unit is touched.
pub fn check_exec_path(path: &str) -> Res<()> {
    match path.chars().find(|c| matches!(c, '\'' | '"' | '\\') || c.is_control()) {
        Some(c) => Err(format!(
            "systemd cannot run an executable whose path contains {c:?} ({path}); point XDG_DATA_HOME at a directory without such characters and re-run"
        )),
        None => Ok(()),
    }
}

/// A double-quoted systemd word: escapes `\`, `"` and the `%` specifier; `exec` also escapes `$` (variable expansion).
pub fn systemd_quote(s: &str, exec: bool) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '%' => o.push_str("%%"),
            '$' if exec => o.push_str("$$"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

pub fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub fn render_unit(ctx: &Ctx, role: &str, env: &[(String, String)]) -> Res<String> {
    let bin = ctx.current_bin().to_string_lossy().to_string();
    check_exec_path(&bin)?;
    // systemd does no `$` expansion in the executable path, so it is not doubled there; the arguments are constants.
    let mut exec = vec![systemd_quote(&bin, false)];
    exec.extend(role_args(role).iter().map(|a| systemd_quote(a, true)));
    let mut u = String::new();
    u.push_str(&format!("[Unit]\nDescription=agent-stats {role}\nAfter=network-online.target\n\n[Service]\n"));
    u.push_str(&format!("ExecStart={}\n", exec.join(" ")));
    u.push_str(&format!("Environment={}\n", systemd_quote(&format!("PATH={}", ctx.service_path), false)));
    for (k, v) in env {
        no_newlines(k)?;
        no_newlines(v)?;
        u.push_str(&format!("Environment={}\n", systemd_quote(&format!("{k}={v}"), false)));
    }
    u.push_str("Restart=always\nRestartSec=10\n");
    if role == "report" {
        u.push_str("Nice=10\n");
    }
    u.push_str("\n[Install]\nWantedBy=default.target\n");
    Ok(u)
}

/// The updater's oneshot service; the timer in [`render_updater_timer`] starts it. It is a unit of its own (not a
/// child of the reporter) so restarting report/hub during an update never kills the update that is running.
pub fn render_updater_service(ctx: &Ctx) -> Res<String> {
    let bin = ctx.current_bin().to_string_lossy().to_string();
    check_exec_path(&bin)?;
    let mut exec = vec![systemd_quote(&bin, false)];
    exec.extend(role_args(UPDATER).iter().map(|a| systemd_quote(a, true)));
    let mut u = String::new();
    u.push_str(
        "[Unit]\nDescription=agent-stats automatic update\nAfter=network-online.target\n\n[Service]\nType=oneshot\n",
    );
    u.push_str(&format!("ExecStart={}\n", exec.join(" ")));
    u.push_str(&format!("Environment={}\n", systemd_quote(&format!("PATH={}", ctx.service_path), false)));
    u.push_str("Nice=10\n");
    Ok(u)
}

pub fn render_updater_timer(every_secs: u64) -> String {
    format!(
        "[Unit]\nDescription=agent-stats automatic update timer\n\n[Timer]\nOnBootSec=10min\nOnUnitActiveSec={every_secs}s\nRandomizedDelaySec=2min\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
    )
}

/// LaunchAgent that runs `update --auto` every `every_secs`; no KeepAlive, nothing at load.
pub fn render_updater_plist(ctx: &Ctx, every_secs: u64) -> Res<String> {
    let log = xml_escape(&ctx.log_path(UPDATER).to_string_lossy());
    let mut args = vec![ctx.current_bin().to_string_lossy().to_string()];
    args.extend(role_args(UPDATER).iter().map(|s| s.to_string()));
    let mut p = String::new();
    p.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    p.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    p.push_str("<plist version=\"1.0\">\n<dict>\n");
    p.push_str(&format!("  <key>Label</key>\n  <string>{}</string>\n", xml_escape(&launchd_label(UPDATER))));
    p.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for a in &args {
        no_newlines(a)?;
        p.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    p.push_str("  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n");
    p.push_str(&format!("    <key>PATH</key>\n    <string>{}</string>\n", xml_escape(&ctx.service_path)));
    p.push_str("  </dict>\n");
    p.push_str(&format!("  <key>StartInterval</key>\n  <integer>{every_secs}</integer>\n"));
    p.push_str("  <key>RunAtLoad</key>\n  <false/>\n");
    p.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    p.push_str(&format!("  <key>StandardOutPath</key>\n  <string>{log}</string>\n"));
    p.push_str(&format!("  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"));
    p.push_str("</dict>\n</plist>\n");
    Ok(p)
}

pub fn render_plist(ctx: &Ctx, role: &str, env: &[(String, String)]) -> Res<String> {
    let log = ctx.log_path(role).to_string_lossy().to_string();
    let mut args = vec![ctx.current_bin().to_string_lossy().to_string()];
    args.extend(role_args(role).iter().map(|s| s.to_string()));
    let mut p = String::new();
    p.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    p.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    p.push_str("<plist version=\"1.0\">\n<dict>\n");
    p.push_str(&format!("  <key>Label</key>\n  <string>{}</string>\n", xml_escape(&launchd_label(role))));
    p.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for a in &args {
        no_newlines(a)?;
        p.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    p.push_str("  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n");
    p.push_str(&format!("    <key>PATH</key>\n    <string>{}</string>\n", xml_escape(&ctx.service_path)));
    for (k, v) in env {
        no_newlines(k)?;
        no_newlines(v)?;
        p.push_str(&format!("    <key>{}</key>\n    <string>{}</string>\n", xml_escape(k), xml_escape(v)));
    }
    p.push_str("  </dict>\n");
    p.push_str("  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n");
    p.push_str("  <key>ThrottleInterval</key>\n  <integer>10</integer>\n");
    p.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    let log = xml_escape(&log);
    p.push_str(&format!("  <key>StandardOutPath</key>\n  <string>{log}</string>\n"));
    p.push_str(&format!("  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"));
    p.push_str("</dict>\n</plist>\n");
    Ok(p)
}

// ---------------------------------------------------------------- parsing existing definitions

/// Splits systemd words: whitespace separated, `"…"` / `'…'` quoting, backslash escapes, `%%` → `%`.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    have = true;
                }
            }
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => {
                quote = Some(c);
                have = true;
            }
            (None, c) if c.is_whitespace() => {
                if have {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (_, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        words.push(cur);
    }
    words.into_iter().map(|w| w.replace("%%", "%")).collect()
}

pub fn parse_unit_env(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Environment=") {
            for w in split_words(rest) {
                if let Some((k, v)) = w.split_once('=') {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

pub fn parse_plist_env(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(i) = text.find("<key>EnvironmentVariables</key>") else { return out };
    let rest = &text[i..];
    let Some(d) = rest.find("<dict>") else { return out };
    let rest = &rest[d + 6..];
    let section = &rest[..rest.find("</dict>").unwrap_or(rest.len())];
    let mut s = section;
    while let Some(k) = s.find("<key>") {
        let after = &s[k + 5..];
        let Some(ke) = after.find("</key>") else { break };
        let key = xml_unescape(&after[..ke]);
        let after = &after[ke + 6..];
        let next_key = after.find("<key>").unwrap_or(after.len());
        if let Some(v) = after[..next_key].find("<string>") {
            let v = &after[v + 8..];
            if let Some(ve) = v.find("</string>") {
                out.push((key, xml_unescape(&v[..ve])));
            }
        }
        s = &after[next_key..];
    }
    out
}

/// Whitelisted env of the existing definitions for `role` (legacy plist/unit first, then ours; later wins).
pub fn carried_env(ctx: &Ctx, role: &str) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = Vec::new();
    match ctx.platform {
        Platform::Linux => {
            if let Ok(t) = fs::read_to_string(ctx.unit_path(role)) {
                all.extend(parse_unit_env(&t));
            }
        }
        Platform::Mac => {
            for label in legacy_label(role).into_iter().chain([launchd_label(role)]) {
                if let Ok(t) = fs::read_to_string(ctx.plist_path(&label)) {
                    all.extend(parse_plist_env(&t));
                }
            }
        }
    }
    CARRY_ENV
        .iter()
        .filter_map(|k| all.iter().rev().find(|(ek, _)| ek == k).map(|(_, v)| (k.to_string(), v.clone())))
        .collect()
}

// ---------------------------------------------------------------- manager operations

fn uid_domain(ctx: &Ctx) -> String {
    format!("gui/{}", ctx.uid)
}

pub fn restart_service(ctx: &Ctx, role: &str) -> Res<()> {
    match ctx.platform {
        Platform::Linux => run(&ctx.systemctl, &["--user", "restart", &unit_name(role)]),
        Platform::Mac => {
            let target = format!("{}/{}", uid_domain(ctx), launchd_label(role));
            run(&ctx.launchctl, &["kickstart", "-k", &target])
        }
    }
}

pub fn service_running(ctx: &Ctx, role: &str) -> bool {
    match ctx.platform {
        Platform::Linux => ok(&ctx.systemctl, &["--user", "is-active", "--quiet", &unit_name(role)]),
        Platform::Mac => ok(&ctx.launchctl, &["print", &format!("{}/{}", uid_domain(ctx), launchd_label(role))]),
    }
}

/// Roles that have a service definition on this machine.
pub fn installed_roles(ctx: &Ctx) -> Vec<&'static str> {
    ROLES.iter().copied().filter(|r| ctx.service_installed(r)).collect()
}

/// Makes sure a version is installed and `current` points at it. Installs the running binary when it is not
/// itself inside the versions directory; that never moves `current` to a lower version unless `force`.
pub fn ensure_install(ctx: &Ctx, force: bool) -> Res<String> {
    let inside = ctx.self_exe.starts_with(ctx.versions_dir()) || ctx.self_exe.starts_with(ctx.current_link());
    if inside {
        if let Some(v) = ctx.current_version() {
            return Ok(v);
        }
    }
    if !inside {
        let v = ctx.self_version.clone();
        let cur = ctx.current_version();
        if cur.as_deref() != Some(v.as_str()) {
            if let Some(c) = &cur {
                if !force && layout::cmp_versions(&v, c) == std::cmp::Ordering::Less {
                    return Err(format!(
                        "this binary is agent-stats {v} but {c} is active; refusing to move `current` down. Run `agent-stats service install` from the installed binary, or pass --force to switch to {v}"
                    ));
                }
            }
        }
        layout::install_version(ctx, &ctx.self_exe, &v)?;
        if cur.as_deref() != Some(v.as_str()) {
            println!("current: {} -> {v}", cur.as_deref().unwrap_or("(none)"));
            layout::activate(ctx, &v)?;
            layout::record_active(ctx, &v)?;
        }
        return Ok(v);
    }
    ctx.current_version().ok_or_else(|| "no installed version".to_string())
}

pub fn install(ctx: &Ctx, role: &str, label: Option<&str>) -> Res<()> {
    install_opts(ctx, role, label, false)
}

pub fn install_opts(ctx: &Ctx, role: &str, label: Option<&str>, force: bool) -> Res<()> {
    check_role(role)?;
    if role == UPDATER {
        return install_updater(ctx, DEFAULT_EVERY_SECS, force);
    }
    // Render and validate everything first: a bad path or value must fail before anything on disk or in the service
    // manager changes.
    let mut env = carried_env(ctx, role);
    if let Some(l) = label {
        env.retain(|(k, _)| k != "STATS_LABEL");
        env.push(("STATS_LABEL".into(), l.to_string()));
    }
    let rendered = match ctx.platform {
        Platform::Linux => render_unit(ctx, role, &env)?,
        Platform::Mac => render_plist(ctx, role, &env)?,
    };
    let _lock = layout::InstallLock::acquire(ctx)?;
    let version = ensure_install(ctx, force)?;
    layout::prune(ctx, layout::KEEP_VERSIONS);
    println!("agent-stats {version} installed in {}", ctx.version_dir(&version).display());
    if let Some(note) = link_bin(ctx)? {
        println!("note: {note}");
    }
    fs::create_dir_all(ctx.state_dir.join("logs")).ctx("create logs dir")?;
    match ctx.platform {
        Platform::Linux => install_linux(ctx, role, &rendered)?,
        Platform::Mac => install_mac(ctx, role, &rendered)?,
    }
    Ok(())
}

/// Installs the scheduled updater. Unlike the roles it is never restarted or health-trialled: it only runs when the
/// timer (or launchd interval) fires.
pub fn install_updater(ctx: &Ctx, every_secs: u64, force: bool) -> Res<()> {
    if every_secs < MIN_EVERY_SECS {
        return Err(format!("usage: the updater interval must be at least {}", fmt_every(MIN_EVERY_SECS)));
    }
    let rendered = match ctx.platform {
        Platform::Linux => (render_updater_service(ctx)?, render_updater_timer(every_secs)),
        Platform::Mac => (render_updater_plist(ctx, every_secs)?, String::new()),
    };
    let _lock = layout::InstallLock::acquire(ctx)?;
    let version = ensure_install(ctx, force)?;
    layout::prune(ctx, layout::KEEP_VERSIONS);
    println!("agent-stats {version} installed in {}", ctx.version_dir(&version).display());
    if let Some(note) = link_bin(ctx)? {
        println!("note: {note}");
    }
    fs::create_dir_all(ctx.state_dir.join("logs")).ctx("create logs dir")?;
    match ctx.platform {
        Platform::Linux => install_updater_linux(ctx, &rendered.0, &rendered.1)?,
        Platform::Mac => {
            let label = launchd_label(UPDATER);
            lint_plist(ctx, &label, &rendered.0)?;
            let path = ctx.plist_path(&label);
            let domain = uid_domain(ctx);
            atomic_write(&path, rendered.0.as_bytes(), None).ctx("write plist")?;
            let _ = run(&ctx.launchctl, &["bootout", &format!("{domain}/{label}")]);
            run(&ctx.launchctl, &["bootstrap", &domain, &path.to_string_lossy()])
                .map_err(|e| format!("bootstrapping {label} failed: {e}"))?;
            println!("installed {label}");
        }
    }
    println!("the updater runs `agent-stats update --auto` every {}", fmt_every(every_secs));
    Ok(())
}

fn install_updater_linux(ctx: &Ctx, service: &str, timer: &str) -> Res<()> {
    let (spath, tpath) = (ctx.unit_path(UPDATER), ctx.timer_path());
    let name = timer_name();
    atomic_write(&spath, service.as_bytes(), None).ctx("write unit")?;
    atomic_write(&tpath, timer.as_bytes(), None).ctx("write timer")?;
    let started = run(&ctx.systemctl, &["--user", "daemon-reload"])
        .and_then(|_| run(&ctx.systemctl, &["--user", "enable", "--now", &name]));
    if let Err(e) = started {
        let _ = fs::remove_file(&spath);
        let _ = fs::remove_file(&tpath);
        let _ = run(&ctx.systemctl, &["--user", "daemon-reload"]);
        return Err(format!("{name} did not start: {e}; the new units were removed"));
    }
    if !ctx.user.is_empty() && !ok(&ctx.loginctl, &["enable-linger", &ctx.user]) {
        eprintln!(
            "warning: `loginctl enable-linger {u}` failed; the timer stops when you log out (try: sudo loginctl enable-linger {u})",
            u = ctx.user
        );
    }
    println!("installed {name}");
    Ok(())
}

/// Lints a copy of a plist so nothing of the running setup is touched while the file might be invalid.
fn lint_plist(ctx: &Ctx, label: &str, plist: &str) -> Res<()> {
    let lint = ctx.agents_dir.join(format!(".{label}.lint{}.plist", std::process::id()));
    fs::create_dir_all(&ctx.agents_dir).ctx("create LaunchAgents dir")?;
    fs::write(&lint, plist).ctx("write plist")?;
    let linted = Command::new(&ctx.plutil).arg("-lint").arg(&lint).stdin(Stdio::null()).output();
    let _ = fs::remove_file(&lint);
    match linted {
        Ok(o) if !o.status.success() => {
            let msg = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            Err(format!("the generated plist for {label} is invalid (plutil -lint): {}", msg.trim()))
        }
        _ => Ok(()), // lint passed, or there is no plutil (not a Mac): nothing to check with
    }
}

fn install_linux(ctx: &Ctx, role: &str, unit: &str) -> Res<()> {
    let path = ctx.unit_path(role);
    let name = unit_name(role);
    let backup = fs::read(&path).ok();
    atomic_write(&path, unit.as_bytes(), None).ctx("write unit")?;
    let started = (|| -> Res<()> {
        run(&ctx.systemctl, &["--user", "daemon-reload"])?;
        run(&ctx.systemctl, &["--user", "enable", &name])?;
        run(&ctx.systemctl, &["--user", "restart", &name])
    })();
    if let Err(e) = started {
        // Put the previous unit back and start it again so a bad new one does not leave the machine silent.
        let restored = match &backup {
            Some(old) => atomic_write(&path, old, None).is_ok(),
            None => fs::remove_file(&path).is_ok(),
        };
        let _ = run(&ctx.systemctl, &["--user", "daemon-reload"]);
        if backup.is_some() {
            let _ = run(&ctx.systemctl, &["--user", "restart", &name]);
        }
        let how = match (restored, backup.is_some()) {
            (true, true) => "the previous unit was restored and restarted",
            (true, false) => "the new unit was removed",
            (false, _) => "restoring the previous unit failed too",
        };
        return Err(format!("{name} did not start: {e}; {how}"));
    }
    if !ctx.user.is_empty() && !ok(&ctx.loginctl, &["enable-linger", &ctx.user]) {
        eprintln!(
            "warning: `loginctl enable-linger {u}` failed; the service stops when you log out (try: sudo loginctl enable-linger {u})",
            u = ctx.user
        );
    }
    println!("installed {name}");
    Ok(())
}

/// Is the LaunchAgent for `role` running? `launchctl print` says so, or its status file got a fresh heartbeat.
fn mac_running(ctx: &Ctx, role: &str, since: u64) -> bool {
    let target = format!("{}/{}", uid_domain(ctx), launchd_label(role));
    if let Ok(out) = Command::new(&ctx.launchctl).args(["print", &target]).stdin(Stdio::null()).output() {
        let t = String::from_utf8_lossy(&out.stdout);
        if out.status.success() && (t.contains("state = running") || t.lines().any(|l| l.trim().starts_with("pid = ")))
        {
            return true;
        }
    }
    read_role_status(ctx, role).is_some_and(|s| s.heartbeat_at >= since && pid_alive(s.pid))
}

fn install_mac(ctx: &Ctx, role: &str, plist: &str) -> Res<()> {
    let label = launchd_label(role);
    let new_path = ctx.plist_path(&label);
    let domain = uid_domain(ctx);
    lint_plist(ctx, &label, plist)?;
    // Retire the old install first (the hub must free its port) but keep its plist as .bak until the new one runs.
    let prev_new = fs::read(&new_path).ok();
    let mut legacy: Option<(String, PathBuf, PathBuf)> = None;
    if let Some(old) = legacy_label(role) {
        let old_plist = ctx.plist_path(&old);
        if old_plist.exists() {
            let bak = ctx.agents_dir.join(format!("{old}.plist.bak"));
            let _ = run(&ctx.launchctl, &["bootout", &format!("{domain}/{old}")]);
            fs::rename(&old_plist, &bak).ctx(format!("move {} aside", old_plist.display()))?;
            legacy = Some((old, old_plist, bak));
        }
    }
    let restore = |why: &str| -> String {
        let _ = run(&ctx.launchctl, &["bootout", &format!("{domain}/{label}")]);
        match &prev_new {
            Some(old) => {
                let _ = atomic_write(&new_path, old, None);
                let _ = run(&ctx.launchctl, &["bootstrap", &domain, &new_path.to_string_lossy()]);
            }
            None => {
                let _ = fs::remove_file(&new_path);
            }
        }
        let mut how = String::from("nothing was running before");
        if let Some((old, old_plist, bak)) = &legacy {
            let back = fs::rename(bak, old_plist).is_ok()
                && run(&ctx.launchctl, &["bootstrap", &domain, &old_plist.to_string_lossy()]).is_ok();
            how = if back {
                format!("the previous LaunchAgent {old} was restored")
            } else {
                format!("restoring the previous LaunchAgent {old} failed too (its plist is {})", bak.display())
            };
        } else if prev_new.is_some() {
            how = "the previous plist was restored".into();
        }
        format!("{why}; {how}")
    };
    let since = now_secs();
    let boot = (|| -> Res<()> {
        atomic_write(&new_path, plist.as_bytes(), None).ctx("write plist")?;
        let _ = run(&ctx.launchctl, &["bootout", &format!("{domain}/{label}")]);
        run(&ctx.launchctl, &["bootstrap", &domain, &new_path.to_string_lossy()])
    })();
    if let Err(e) = boot {
        let hint = if e.contains("125") || e.contains("Domain does not support") || e.contains("Could not find domain")
        {
            format!(
                " (there is no GUI login session for uid {}: log in to the Mac's desktop or run this from Terminal on it, not over SSH without a logged-in user, then re-run)",
                ctx.uid
            )
        } else {
            String::new()
        };
        return Err(restore(&format!("bootstrapping {label} failed: {e}{hint}")));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ctx.launchd_wait_secs);
    while !mac_running(ctx, role, since) {
        if std::time::Instant::now() >= deadline {
            return Err(restore(&format!("{label} did not start within {} s", ctx.launchd_wait_secs)));
        }
        std::thread::sleep(std::time::Duration::from_millis(ctx.poll_ms));
    }
    if let Some((old, _, bak)) = legacy {
        let _ = fs::remove_file(bak);
        println!("removed legacy LaunchAgent {old}");
    }
    println!("installed {label}");
    Ok(())
}

pub fn uninstall(ctx: &Ctx, role: &str) -> Res<()> {
    check_role(role)?;
    match ctx.platform {
        Platform::Linux => {
            let name = unit_name(role);
            let path = ctx.unit_path(role);
            if !path.exists() {
                println!("{name} is not installed");
                return Ok(());
            }
            if role == UPDATER {
                let _ = run(&ctx.systemctl, &["--user", "disable", "--now", &timer_name()]);
                let _ = fs::remove_file(ctx.timer_path());
            } else {
                let _ = run(&ctx.systemctl, &["--user", "disable", "--now", &name]);
            }
            fs::remove_file(&path).ctx(format!("remove {}", path.display()))?;
            let _ = run(&ctx.systemctl, &["--user", "daemon-reload"]);
            println!("removed {name}");
        }
        Platform::Mac => {
            let label = launchd_label(role);
            let path = ctx.plist_path(&label);
            if !path.exists() {
                println!("{label} is not installed");
                return Ok(());
            }
            let _ = run(&ctx.launchctl, &["bootout", &format!("{}/{label}", uid_domain(ctx))]);
            fs::remove_file(&path).ctx(format!("remove {}", path.display()))?;
            println!("removed {label}");
        }
    }
    Ok(())
}

pub fn status(ctx: &Ctx) -> Res<()> {
    let state = ctx.load_state();
    match ctx.current_version() {
        Some(v) => println!(
            "agent-stats {v} is the active version (previous: {}; installed: {})",
            state.previous.as_deref().unwrap_or("none"),
            ctx.installed_versions().join(", ")
        ),
        None => println!("no versioned install yet (run `agent-stats service install report`)"),
    }
    if let Some(t) = &state.trial {
        let from = t.from.as_deref().unwrap_or("(none)");
        if pid_alive(t.pid) {
            println!("update in progress: {from} -> {} is being verified by pid {}", t.to, t.pid);
        } else {
            println!(
                "WARNING: an update {from} -> {} was interrupted (pid {} is gone) and {} was never verified.\n         Run `agent-stats rollback` (back to {from}) or `agent-stats update` to recover.",
                t.to, t.pid, t.to
            );
        }
    }
    if let Some(u) = &state.last_update {
        println!(
            "last update: {} -> {} {} {}",
            u.from.as_deref().unwrap_or("-"),
            u.to,
            u.status,
            u.reason.as_deref().unwrap_or("")
        );
    }
    for role in ROLES {
        if !ctx.service_installed(role) {
            println!("{role:<7} not installed");
            continue;
        }
        let running = service_running(ctx, role);
        let st = read_role_status(ctx, role);
        let detail = match st {
            Some(s) => {
                let age = now_secs().saturating_sub(s.heartbeat_at);
                let alive = if pid_alive(s.pid) { "" } else { " (pid gone)" };
                format!("v{} heartbeat {age}s ago{alive}", s.version)
            }
            None => "no status file yet".to_string(),
        };
        println!("{role:<7} installed  {}  {detail}", if running { "running" } else { "stopped" });
    }
    println!("{}", updater_summary(ctx, now_secs()));
    if ctx.platform == Platform::Linux && ctx.service_installed(UPDATER) {
        let next = Command::new(&ctx.systemctl)
            .args(["--user", "show", &timer_name(), "-p", "NextElapseUSecRealtime"])
            .stdin(Stdio::null())
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|t| t.trim().strip_prefix("NextElapseUSecRealtime=").map(str::to_string))
            .filter(|v| !v.is_empty() && v != "n/a");
        if let Some(n) = next {
            println!("updater: next run {n}");
        }
    }
    Ok(())
}

/// The interval of the installed updater, read back from its timer / plist.
pub fn updater_interval(ctx: &Ctx) -> Option<u64> {
    match ctx.platform {
        Platform::Linux => {
            let t = fs::read_to_string(ctx.timer_path()).ok()?;
            let v = t.lines().find_map(|l| l.strip_prefix("OnUnitActiveSec="))?;
            v.trim().trim_end_matches('s').parse().ok()
        }
        Platform::Mac => {
            let t = fs::read_to_string(ctx.plist_path(&launchd_label(UPDATER))).ok()?;
            let rest = &t[t.find("<key>StartInterval</key>")?..];
            let rest = &rest[rest.find("<integer>")? + 9..];
            rest[..rest.find("</integer>")?].trim().parse().ok()
        }
    }
}

fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// One line for `service status`: whether the updater is installed, its interval and the last `update --auto` result
/// (`<data_dir>/auto-update.json`; missing or malformed is fine).
pub fn updater_summary(ctx: &Ctx, now: u64) -> String {
    if !ctx.service_installed(UPDATER) {
        return "updater not installed (agent-stats service install updater)".into();
    }
    let every = updater_interval(ctx).map(fmt_every).unwrap_or_else(|| "?".into());
    let last = fs::read(ctx.data_dir.join("auto-update.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            let at = v.get("at")?.as_u64()?;
            let outcome = v.get("outcome")?.as_str()?.to_string();
            let version = v.get("version").and_then(|x| x.as_str()).map(|x| format!(" ({x})")).unwrap_or_default();
            let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
            let msg = if msg.is_empty() || matches!(outcome.as_str(), "up-to-date" | "updated") {
                String::new()
            } else {
                format!(" - {msg}")
            };
            Some(format!("last run {} ago: {outcome}{version}{msg}", ago(now.saturating_sub(at))))
        })
        .unwrap_or_else(|| "no run recorded yet".into());
    format!("updater: every {every}, {last}")
}

pub fn cli(ctx: &Ctx, args: &[String]) -> Res<()> {
    const USAGE: &str =
        "usage: agent-stats service install <report|hub|relay|updater> [--label L] [--every 15m] [--force] | uninstall <role> | status";
    match args.first().map(String::as_str) {
        Some("install") => {
            let role = args.get(1).ok_or(USAGE)?;
            let mut label = None;
            let mut force = false;
            let mut every = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--label" => {
                        label = Some(args.get(i + 1).ok_or("usage: --label needs a value")?.clone());
                        i += 2;
                    }
                    "--force" => {
                        force = true;
                        i += 1;
                    }
                    "--every" => {
                        every = Some(parse_every(args.get(i + 1).ok_or("usage: --every needs a value")?)?);
                        i += 2;
                    }
                    a if a.starts_with("--every=") => {
                        every = Some(parse_every(&a["--every=".len()..])?);
                        i += 1;
                    }
                    a if a.starts_with("--label=") => {
                        label = Some(a["--label=".len()..].to_string());
                        i += 1;
                    }
                    other => return Err(format!("usage: unexpected argument {other:?}\n{USAGE}")),
                }
            }
            if role == UPDATER {
                if label.is_some() {
                    return Err("usage: --label does not apply to the updater".into());
                }
                return install_updater(ctx, every.unwrap_or(DEFAULT_EVERY_SECS), force);
            }
            if every.is_some() {
                return Err("usage: --every only applies to the updater".into());
            }
            install_opts(ctx, role, label.as_deref(), force)
        }
        Some("uninstall") => uninstall(ctx, args.get(1).ok_or(USAGE)?),
        Some("status") => status(ctx),
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        let mut c = Ctx::from_env();
        c.data_dir = PathBuf::from("/data dir/agent-stats");
        c.state_dir = PathBuf::from("/state/agent-stats");
        c.service_path = "/bin:/usr/bin".into();
        c
    }

    #[test]
    fn unit_rendering_quotes_and_carries_env() {
        let env = vec![
            ("HUB_URL".to_string(), "http://h:7777".to_string()),
            ("STATS_LABEL".to_string(), "Kil's \"Mac\" 100% $HOME \\ x".to_string()),
        ];
        let u = render_unit(&ctx(), "report", &env).unwrap();
        assert!(u.contains("ExecStart=\"/data dir/agent-stats/current/agent-stats\" \"report\"\n"), "{u}");
        assert!(u.contains("Environment=\"HUB_URL=http://h:7777\"\n"));
        assert!(u.contains("Environment=\"STATS_LABEL=Kil's \\\"Mac\\\" 100%% $HOME \\\\ x\"\n"), "{u}");
        assert!(u.contains("Restart=always\nRestartSec=10\nNice=10\n"));
        assert!(u.contains("WantedBy=default.target"));
        let hub = render_unit(&ctx(), "hub", &[]).unwrap();
        assert!(hub.contains("\"hub\" \"serve\"") && !hub.contains("Nice="));
        // Round trip through the parser.
        let back = parse_unit_env(&u);
        assert!(back.contains(&env[1]));
        assert!(render_unit(&ctx(), "report", &[("A".into(), "x\ny".into())]).is_err());
    }

    #[test]
    fn plist_rendering_escapes_and_parses_back() {
        let env = vec![("STATS_LABEL".to_string(), "A & B <c> \"d\"".to_string())];
        let p = render_plist(&ctx(), "report", &env).unwrap();
        assert!(p.contains("<string>dev.agent-stats.report</string>"));
        assert!(p.contains("<string>/data dir/agent-stats/current/agent-stats</string>"));
        assert!(p.contains("<string>A &amp; B &lt;c&gt; &quot;d&quot;</string>"), "{p}");
        for needle in [
            "<key>KeepAlive</key>",
            "<integer>10</integer>",
            "<string>Background</string>",
            "/state/agent-stats/logs/report.log",
        ] {
            assert!(p.contains(needle), "{needle}");
        }
        assert_eq!(parse_plist_env(&p).iter().find(|(k, _)| k == "STATS_LABEL").unwrap().1, "A & B <c> \"d\"");
    }

    #[test]
    fn parses_legacy_unit_environment_lines() {
        let t = "[Service]\nEnvironment=\"PATH=/x:/y\"\nEnvironment=\"HUB_URL=http://1.2.3.4:7777\" \"STATS_LABEL=My Mac\"\nEnvironment=STATS_TZ=Asia/Tokyo\n";
        let e = parse_unit_env(t);
        assert!(e.contains(&("HUB_URL".into(), "http://1.2.3.4:7777".into())));
        assert!(e.contains(&("STATS_LABEL".into(), "My Mac".into())));
        assert!(e.contains(&("STATS_TZ".into(), "Asia/Tokyo".into())));
    }
}
