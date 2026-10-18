//! `fleet add --ssh` and `fleet remove --ssh`: everything done on another machine goes through the system
//! `ssh` (so `~/.ssh/config`, agents, 1Password and Tailscale SSH all work), always with `BatchMode=yes` so it never
//! waits on a prompt, and `StrictHostKeyChecking=accept-new` (a first connection is accepted, a changed key stops
//! it). Each round trip sends one POSIX script on stdin (`sh -s`): a probe (system, existing install, which of our
//! routes answers from there), the install (this repository's install.sh, embedded in the binary, at this machine's
//! version), then the join. A pairing code only ever travels inside a script on stdin, never in an event or a log.

use crate::local::{last_seen, now_ms, redeemed_since, HubCtx};
use crate::out::Out;
use crate::{Args, Res};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// This repository's installer, sent to the target as is.
const INSTALL_SH: &str = include_str!("../../../install.sh");
/// Where an archive uploaded for a machine without internet goes, under its `$HOME`.
const UPLOAD_DIR: &str = ".local/share/agent-stats/.upload";
/// How long to wait for the first report after joining.
const REPORT_WAIT: Duration = Duration::from_secs(60);

/// `s` as one word for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A target ssh will take as a host, not an option.
fn check_target(t: &str) -> Res<()> {
    if t.is_empty() || t.starts_with('-') || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{t:?} is not an SSH target (user@host, host, or a name from ~/.ssh/config)"));
    }
    Ok(())
}

/// The host part of `user@host`.
fn host_of(target: &str) -> &str {
    target.rsplit_once('@').map_or(target, |(_, h)| h)
}

pub struct SshOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn ssh_program() -> String {
    std::env::var("AGENT_STATS_SSH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ssh".into())
}

/// Runs `remote` on `target` with `stdin`, at most `limit`.
fn ssh(target: &str, remote: &str, stdin: &[u8], limit: Duration) -> Res<SshOut> {
    let mut child = Command::new(ssh_program())
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "StrictHostKeyChecking=accept-new"])
        .args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4", target, remote])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run ssh: {e}"))?;
    let (mut o, mut e) = (child.stdout.take().ok_or("no stdout")?, child.stderr.take().ok_or("no stderr")?);
    let ro = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = o.read_to_end(&mut s);
        s
    });
    let re = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = e.read_to_end(&mut s);
        s
    });
    if let Some(mut i) = child.stdin.take() {
        let data = stdin.to_vec();
        // A writer thread: a remote that stops reading must not block us past the deadline.
        std::thread::spawn(move || {
            let _ = i.write_all(&data);
        });
    }
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(s);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = String::from_utf8_lossy(&ro.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&re.join().unwrap_or_default()).into_owned();
    match status {
        Some(s) => Ok(SshOut { code: s.code(), stdout, stderr }),
        None => Err(format!("timed out after {} s", limit.as_secs())),
    }
}

/// What went wrong with an SSH session, from its exit status and stderr: (kind, message, hint).
pub fn classify(target: &str, code: Option<i32>, stderr: &str) -> (&'static str, String, Option<String>) {
    let host = host_of(target);
    let last =
        stderr.lines().rev().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"));
    let msg = last.unwrap_or("ssh failed").to_string();
    let has = |pats: &[&str]| pats.iter().any(|p| stderr.contains(p));
    if has(&["REMOTE HOST IDENTIFICATION HAS CHANGED", "Host key verification failed"]) {
        return (
            "host-key-changed",
            format!("{host}'s SSH host key changed since the last connection"),
            Some(format!("if {host} was reinstalled, forget the old key with: ssh-keygen -R {host}")),
        );
    }
    if has(&["Permission denied", "Too many authentication failures", "no such identity"]) {
        return (
            "needs-key",
            format!("{host} wants a password; agent-stats only uses SSH keys"),
            Some(format!("ssh-copy-id {target}   (or turn on Tailscale SSH on {host})")),
        );
    }
    if has(&["is not recognized as an internal or external command", "is not recognized as the name of a cmdlet"]) {
        return ("unsupported", format!("{host} runs Windows, which agent-stats doesn't support"), None);
    }
    if has(&[
        "Could not resolve hostname",
        "Name or service not known",
        "nodename nor servname",
        "Connection refused",
        "timed out",
        "No route to host",
        "Network is unreachable",
        "Connection reset",
        "Connection closed",
        "port 22: ",
    ]) || code == Some(255)
    {
        return (
            "unreachable",
            msg,
            Some(format!("check that {host} is on and reachable from here (same tailnet or network), with SSH on")),
        );
    }
    ("other", msg, None)
}

/// Tells about the target, then tries each route given as an argument (`route=<url> <descriptor or nothing>`).
const PROBE: &str = r#"
echo "os=$(uname -s 2>/dev/null)"
echo "arch=$(uname -m 2>/dev/null)"
echo "hostname=$(hostname 2>/dev/null)"
if command -v systemctl >/dev/null 2>&1; then
  if systemctl --user show-environment >/dev/null 2>&1; then echo systemd=yes; else echo systemd=nosession; fi
fi
command -v launchctl >/dev/null 2>&1 && echo launchd=yes
command -v curl >/dev/null 2>&1 && echo curl=yes
command -v wget >/dev/null 2>&1 && echo wget=yes
[ -d "${AGENT_STATS_APPS_DIR:-/Applications}/Tokiscope.app" ] && echo tokiscope=yes
B="$HOME/.local/share/agent-stats/current/agent-stats"
[ -x "$B" ] || B="$HOME/.local/bin/agent-stats"
[ -x "$B" ] || B=$(command -v agent-stats 2>/dev/null || true)
if [ -n "$B" ] && [ -x "$B" ]; then echo "bin=$B"; echo "version=$("$B" version 2>/dev/null | head -n 1)"; fi
C="${AGENT_STATS_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/agent-stats}/config.toml"
if [ -f "$C" ]; then
  sed -n 's/^machine *= *"\(.*\)".*/machine=\1/p; s/^id *= *"\(.*\)".*/hub=\1/p' "$C"
fi
for u in "$@"; do
  b=""
  if command -v curl >/dev/null 2>&1; then
    b=$(curl -fsS --connect-timeout 2 -m 3 "$u/.well-known/agent-stats" 2>/dev/null | tr -d '\n')
  elif command -v wget >/dev/null 2>&1; then
    b=$(wget -q -T 3 -O - "$u/.well-known/agent-stats" 2>/dev/null | tr -d '\n')
  fi
  echo "route=$u $b"
done
"#;

/// What the probe found.
#[derive(Debug, Default, PartialEq)]
pub struct Probe {
    pub os: String,
    pub arch: String,
    pub hostname: String,
    /// `yes`, `nosession` (systemctl without a user manager here), or empty (no systemd).
    pub systemd: String,
    pub launchd: bool,
    pub fetcher: bool,
    pub tokiscope: bool,
    pub bin: Option<String>,
    /// The installed agent-stats version, when there is one.
    pub version: Option<String>,
    /// `machine` from its config.toml.
    pub machine: Option<String>,
    /// Hub ids it already reports to.
    pub hubs: Vec<String>,
    /// Each route tried and what its well-known path answered (empty: nothing).
    pub routes: Vec<(String, String)>,
}

pub fn parse_probe(out: &str) -> Probe {
    let mut p = Probe::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim_end_matches('\r');
        match k {
            "os" => p.os = v.into(),
            "arch" => p.arch = v.into(),
            "hostname" => p.hostname = v.into(),
            "systemd" => p.systemd = v.into(),
            "launchd" => p.launchd = true,
            "curl" | "wget" => p.fetcher = true,
            "tokiscope" => p.tokiscope = true,
            "bin" => p.bin = Some(v.into()),
            // `agent-stats 0.5.0 (protocol 2)`
            "version" => p.version = v.split_whitespace().nth(1).map(str::to_string),
            "machine" if !v.is_empty() => p.machine = Some(v.into()),
            "hub" if !v.is_empty() => p.hubs.push(v.into()),
            "route" => {
                let (u, b) = v.split_once(' ').unwrap_or((v, ""));
                p.routes.push((u.to_string(), b.trim().to_string()));
            }
            _ => {}
        }
    }
    p
}

/// The release target for `uname -s` / `uname -m`.
pub fn target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-gnu"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// The first route whose descriptor has our fleet id.
pub fn pick_route(routes: &[(String, String)], hub_id: &str) -> Option<String> {
    routes.iter().find_map(|(u, body)| {
        let d: serde_json::Value = serde_json::from_str(body).ok()?;
        (d["hubId"].as_str() == Some(hub_id)).then(|| u.clone())
    })
}

/// `0.5.10` > `0.5.9`; anything unparsable is old.
pub fn version_at_least(have: &str, want: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect() };
    parse(have) >= parse(want)
}

/// The machine name the target's reporter will use: its config.toml's `machine`, else its hostname without `.local`
/// (the reporter's default).
pub fn machine_name(p: &Probe) -> String {
    match &p.machine {
        Some(m) => m.clone(),
        None => p.hostname.strip_suffix(".local").unwrap_or(&p.hostname).to_string(),
    }
}

fn describe(p: &Probe) -> String {
    let service = match (p.systemd.as_str(), p.launchd) {
        ("yes", _) => " · systemd",
        ("nosession", _) => " · systemd (no user session yet)",
        (_, true) => " · launchd",
        _ => "",
    };
    let have = match &p.version {
        Some(v) => format!("agent-stats {v} installed"),
        None => "no agent-stats yet".into(),
    };
    let os = if p.os == "Darwin" { "macOS" } else { p.os.as_str() };
    format!("{os} {}{service} · {have}", p.arch)
}

/// Routes to try from the target: `AGENT_STATS_TEST_ROUTES` (comma-separated, for tests), else what this machine
/// can be reached at.
fn routes_to_try(ctx: &HubCtx) -> Vec<String> {
    if let Some(r) = std::env::var("AGENT_STATS_TEST_ROUTES").ok().filter(|s| !s.is_empty()) {
        return r.split(',').map(str::to_string).collect();
    }
    ctx.routes_back()
}

/// The release archive and SHA256SUMS of `version` for `triple`, downloaded here and verified (for a target that
/// can't reach GitHub itself).
fn fetch_release(version: &str, triple: &str) -> Res<(String, Vec<u8>, Vec<u8>)> {
    use sha2::{Digest, Sha256};
    let repo = std::env::var("AGENT_STATS_REPO").ok().filter(|s| !s.is_empty()).unwrap_or("kcazin/agent-stats".into());
    let base = format!("https://github.com/{repo}/releases/download/v{version}");
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).build().into();
    let get = |url: &str| -> Res<Vec<u8>> {
        let mut r = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
        r.body_mut().with_config().limit(200 << 20).read_to_vec().map_err(|e| format!("{url}: {e}"))
    };
    let sums = get(&format!("{base}/SHA256SUMS"))?;
    let asset = format!("agent-stats-{version}-{triple}.tar.gz");
    let want = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|l| l.strip_suffix(&format!("  {asset}")).map(str::to_string))
        .ok_or(format!("release {version} has no build for {triple}"))?;
    let bytes = get(&format!("{base}/{asset}"))?;
    let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        return Err(format!("checksum mismatch for {asset}"));
    }
    Ok((asset, bytes, sums))
}

/// The last lines of a failed command's output, for a message.
fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

/// Joins with the code inside the script (stdin), and installs the reporter service unless one is there.
/// `relabel`: the label was given (`--label`, or kept by `--replace`), so it goes into the reporter's service even
/// when one is installed already.
fn join_script(url: &str, code: &str, label: &str, relabel: bool) -> String {
    format!(
        r#"set -eu
URL={url}
CODE={code}
LABEL={label}
RELABEL={relabel}
B="$HOME/.local/share/agent-stats/current/agent-stats"
[ -x "$B" ] || B="$HOME/.local/bin/agent-stats"
[ -x "$B" ] || B=$(command -v agent-stats)
"$B" join "$URL" --code "$CODE" >&2
if [ "$(uname -s)" = Darwin ]; then f="$HOME/Library/LaunchAgents/dev.agent-stats.report.plist"
else f="${{XDG_CONFIG_HOME:-$HOME/.config}}/systemd/user/agent-stats-reporter.service"; fi
if [ ! -f "$f" ] || [ "$RELABEL" = 1 ]; then "$B" service install report --label "$LABEL" >&2; fi
if [ "$(uname -s)" = Darwin ]; then echo service=launchd
elif loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q '=yes'; then echo service=systemd-linger
else echo service=systemd; fi
"#,
        url = sh_quote(url),
        code = sh_quote(code),
        label = sh_quote(label),
        relabel = u8::from(relabel)
    )
}

/// `fleet add --ssh <target>`. Errors are reported as events; the returned `Err("")` only sets the exit code.
pub fn add(a: &Args) -> Res<()> {
    let out = Out { json: a.json };
    let fail = |step: &str, kind: &str, msg: &str, hint: Option<&str>| -> Res<()> {
        out.error(step, kind, msg, hint);
        Err(String::new())
    };
    let Some(target) = a.ssh.as_deref() else { return Err("usage: agent-stats fleet add --ssh <user@host>".into()) };
    if let Err(e) = check_target(target) {
        return fail("connect", "other", &e, None);
    }
    if let Some(l) = &a.label {
        if l.trim().is_empty() || l.chars().any(char::is_control) {
            return fail("connect", "other", "--label: empty or has control characters", None);
        }
    }
    let ctx = match HubCtx::open() {
        Ok(c) => c,
        Err(e) => return fail("connect", "other", &e, None),
    };
    let host = host_of(target);

    // connect + probe, which also tries our routes from there.
    out.running("connect", &format!("ssh {target}"));
    let routes = routes_to_try(&ctx);
    let remote = std::iter::once("sh -s --".to_string()).chain(routes.iter().map(|r| sh_quote(r))).collect::<Vec<_>>();
    let res = match ssh(target, &remote.join(" "), PROBE.as_bytes(), Duration::from_secs(60)) {
        Ok(r) => r,
        Err(e) => return fail("connect", "unreachable", &format!("{host}: {e}"), None),
    };
    if res.code != Some(0) {
        let (kind, msg, hint) = classify(target, res.code, &res.stderr);
        return fail("connect", kind, &msg, hint.as_deref());
    }
    let p = parse_probe(&res.stdout);
    if p.os.is_empty() || p.os.starts_with("MINGW") || p.os.starts_with("MSYS") || p.os.starts_with("CYGWIN") {
        return fail("connect", "unsupported", &format!("{host} is not a Linux or macOS machine"), None);
    }
    if p.tokiscope {
        return fail(
            "connect",
            "unsupported",
            &format!("Tokiscope is installed on {host}: open it there and choose Join (it runs agent-stats itself)"),
            None,
        );
    }
    let Some(triple) = target_triple(&p.os, &p.arch) else {
        return fail(
            "connect",
            "unsupported",
            &format!("{host} is {} {}, which has no agent-stats build", p.os, p.arch),
            None,
        );
    };
    if !p.fetcher {
        return fail(
            "connect",
            "unsupported",
            &format!("{host} has neither curl nor wget"),
            Some("install curl there"),
        );
    }
    out.ok("connect", &describe(&p));

    // route back to this hub.
    let Some(route) = pick_route(&p.routes, &ctx.hub_id) else {
        let tried: Vec<&str> = p.routes.iter().map(|(u, _)| u.as_str()).collect();
        let hint = if ctx.loopback_only() {
            format!(
                "this hub listens on {} only: open it to the network (Add Machine… in Tokiscope does, or set listen \
                 = \"0.0.0.0:{}\" in hub.toml) and try again",
                ctx.cfg.listen,
                ctx.port()
            )
        } else {
            format!(
                "make sure {host} can reach this machine (same tailnet or network, no firewall on port {})",
                ctx.port()
            )
        };
        let msg = if tried.is_empty() {
            "no address of this machine to try".to_string()
        } else {
            format!("none of these answered from {host}: {}", tried.join(", "))
        };
        return fail("route", "no-route-back", &msg, Some(&hint));
    };
    out.ok("route", &format!("{route} answered from {host}"));

    let name = machine_name(&p);
    let enrolled = ctx.db().and_then(|db| agent_stats_hub::auth::is_enrolled(&db, &name).map_err(|e| e.to_string()));
    // A machine added again keeps its label, unless --label gives another.
    let kept = if a.replace { ctx.db().ok().and_then(|db| crate::local::label_of(&db, &name)) } else { None };
    let relabel = a.label.is_some() || kept.is_some();
    let label = a.label.clone().or(kept).unwrap_or_else(|| name.clone());
    match enrolled {
        Ok(true) if !a.replace => {
            return fail(
                "join",
                "taken",
                &format!("a machine named {name} is already in this fleet"),
                Some("if this is the same machine, reinstalled, add it again with --replace"),
            )
        }
        Err(e) => return fail("join", "other", &e, None),
        _ => {}
    }

    // install, unless what is there is at least this version.
    let version = agent_stats_common::VERSION;
    match p.version.as_deref() {
        Some(v) if version_at_least(v, version) => out.ok("install", &format!("agent-stats {v} already installed")),
        _ => {
            out.running("install", &format!("agent-stats {version} on {host}"));
            let cmd = format!("sh -s -- --version {} {}", sh_quote(version), sh_quote(&label));
            let r = match ssh(target, &cmd, INSTALL_SH.as_bytes(), Duration::from_secs(600)) {
                Ok(r) => r,
                Err(e) => return fail("install", "other", &format!("install on {host}: {e}"), None),
            };
            let r = if r.code == Some(3) {
                // No release from there: bring it from here.
                out.running("install", &format!("{host} can't download it; sending agent-stats {version} from here"));
                let (asset, bytes, sums) = match fetch_release(version, triple) {
                    Ok(x) => x,
                    Err(e) => {
                        return fail(
                            "install",
                            "other",
                            &format!("neither {host} nor this machine could download it: {e}"),
                            None,
                        )
                    }
                };
                for (file, data) in [("SHA256SUMS", &sums), (asset.as_str(), &bytes)] {
                    let up =
                        format!("umask 077; mkdir -p \"$HOME/{UPLOAD_DIR}\" && cat > \"$HOME/{UPLOAD_DIR}/{file}\"");
                    match ssh(target, &up, data, Duration::from_secs(600)) {
                        Ok(u) if u.code == Some(0) => {}
                        Ok(u) => {
                            return fail("install", "other", &format!("upload to {host}: {}", tail(&u.stderr)), None)
                        }
                        Err(e) => return fail("install", "other", &format!("upload to {host}: {e}"), None),
                    }
                }
                let cmd = format!(
                    "sh -s -- --version {} --archive \"$HOME/{UPLOAD_DIR}/{asset}\" {}",
                    sh_quote(version),
                    sh_quote(&label)
                );
                let r = ssh(target, &cmd, INSTALL_SH.as_bytes(), Duration::from_secs(600));
                let _ = ssh(target, &format!("rm -rf \"$HOME/{UPLOAD_DIR}\""), b"", Duration::from_secs(30));
                match r {
                    Ok(r) => r,
                    Err(e) => return fail("install", "other", &format!("install on {host}: {e}"), None),
                }
            } else {
                r
            };
            if r.code != Some(0) {
                return fail("install", "other", &format!("install on {host} failed: {}", tail(&r.stderr)), None);
            }
            out.ok("install", &format!("agent-stats {version}, SHA256 verified"));
        }
    }

    // join: a code minted here, used there.
    out.running("join", &format!("joining {name} to this fleet"));
    let start = now_ms();
    let code = match ctx.mint(Some(&label), a.replace.then_some(name.as_str())) {
        Ok(c) => c,
        Err(e) => return fail("join", "other", &format!("pairing code: {e}"), None),
    };
    let r = match ssh(target, "sh -s", join_script(&route, &code, &label, relabel).as_bytes(), Duration::from_secs(120))
    {
        Ok(r) => r,
        Err(e) => return fail("join", "other", &format!("join on {host}: {e}"), None),
    };
    if r.code != Some(0) {
        let t = tail(&r.stderr);
        if t.contains("409") {
            return fail(
                "join",
                "taken",
                &format!("{name} is already enrolled: {t}"),
                Some("add it again with --replace"),
            );
        }
        return fail("join", "other", &format!("join on {host} failed: {t}"), None);
    }
    let service = match r.stdout.lines().find_map(|l| l.strip_prefix("service=")) {
        Some("launchd") => "LaunchAgent, starts at login",
        Some("systemd-linger") => "systemd user service, runs without login (linger)",
        Some("systemd") => "systemd user service (starts at login; linger is off)",
        _ => "reporter service",
    };
    out.ok("join", service);

    // first report.
    out.running("report", "waiting for its first report");
    let deadline = Instant::now() + REPORT_WAIT;
    let mut seen = None;
    while Instant::now() < deadline {
        if let Ok(db) = ctx.db() {
            let who = redeemed_since(&db, start).unwrap_or_else(|| name.clone());
            if last_seen(&db, &who).is_some_and(|t| t >= start) {
                seen = Some(who);
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let Some(machine) = seen else {
        return fail(
            "report",
            "other",
            &format!("{name} joined, but no report arrived within {} s", REPORT_WAIT.as_secs()),
            Some(&format!("on {host}: agent-stats status (and agent-stats doctor)")),
        );
    };
    out.ok("report", "first report received");
    out.done(&machine, &label);
    Ok(())
}

/// Removes this hub's entry from the target's reporter, and the reporter service when no hub is left.
fn uninstall_script(hub_id: &str) -> String {
    format!(
        r#"set -u
ID={id}
B="$HOME/.local/share/agent-stats/current/agent-stats"
[ -x "$B" ] || B="$HOME/.local/bin/agent-stats"
[ -x "$B" ] || B=$(command -v agent-stats 2>/dev/null || true)
[ -n "$B" ] || {{ echo "no agent-stats there" >&2; exit 0; }}
C="${{AGENT_STATS_CONFIG_DIR:-${{XDG_CONFIG_HOME:-$HOME/.config}}/agent-stats}}/config.toml"
if [ -f "$C" ]; then
  name=$(awk -v id="$ID" '/^\[\[hub\]\]/{{n=""}} /^name *=/{{v=$0; sub(/^name *= *"/,"",v); sub(/".*/,"",v); n=v}} /^id *=/{{v=$0; sub(/^id *= *"/,"",v); sub(/".*/,"",v); if (v==id) {{print n; exit}}}}' "$C")
  [ -z "$name" ] || "$B" hubs remove "$name" >&2
fi
if [ ! -f "$C" ] || ! grep -q '^\[\[hub\]\]' "$C"; then
  "$B" service uninstall report >&2 || true
  "$B" service uninstall updater >&2 || true
  echo uninstalled=yes
fi
"#,
        id = sh_quote(hub_id)
    )
}

/// `fleet remove <machine> [--ssh target]`.
pub fn remove(a: &Args) -> Res<()> {
    let [machine] = a.rest.as_slice() else {
        return Err("usage: agent-stats fleet remove <machine> [--ssh <user@host>]".into());
    };
    let out = Out { json: a.json };
    let ctx = HubCtx::open()?;
    let db = ctx.db()?;
    let revoked = agent_stats_hub::fleet::revoke(&db, machine, now_ms()).map_err(|e| e.to_string())?;
    let detail = if revoked {
        format!("{machine}'s token is revoked; its history stays")
    } else {
        format!("{machine} had no active token")
    };
    out.ok("revoke", &detail);
    if let Some(target) = a.ssh.as_deref() {
        check_target(target)?;
        out.running("uninstall", &format!("ssh {target}"));
        let r = ssh(target, "sh -s", uninstall_script(&ctx.hub_id).as_bytes(), Duration::from_secs(120))?;
        if r.code != Some(0) {
            let (kind, msg, hint) = classify(target, r.code, &r.stderr);
            out.error("uninstall", kind, &msg, hint.as_deref());
            return Err(String::new());
        }
        let what = if r.stdout.contains("uninstalled=yes") {
            "reporter stopped and uninstalled"
        } else {
            "removed this fleet; it still reports to its other hubs"
        };
        out.ok("uninstall", what);
    }
    if a.json {
        println!("{}", serde_json::json!({"step": "done", "state": "ok", "machine": machine}));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_targets() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert!(check_target("me@box").is_ok());
        assert!(check_target("-oProxyCommand=x").is_err());
        assert!(check_target("a b").is_err());
        assert_eq!(host_of("me@box.ts.net"), "box.ts.net");
    }

    #[test]
    fn ssh_errors_map_to_kinds() {
        let k = |code, e: &str| classify("me@box", code, e).0;
        assert_eq!(k(Some(255), "me@box: Permission denied (publickey,password)."), "needs-key");
        assert_eq!(
            classify("me@box", Some(255), "Permission denied").2.unwrap(),
            "ssh-copy-id me@box   (or turn on Tailscale SSH on box)"
        );
        assert_eq!(
            k(
                Some(255),
                "@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed."
            ),
            "host-key-changed"
        );
        assert_eq!(k(Some(255), "ssh: Could not resolve hostname box: nodename nor servname provided"), "unreachable");
        assert_eq!(k(Some(255), "ssh: connect to host box port 22: Operation timed out"), "unreachable");
        assert_eq!(k(Some(1), "'sh' is not recognized as an internal or external command,"), "unsupported");
        assert_eq!(k(Some(1), "something else"), "other");
    }

    #[test]
    fn probe_output() {
        let o = "os=Linux\narch=x86_64\nhostname=cachy\nsystemd=yes\ncurl=yes\nbin=/home/k/.local/bin/agent-stats\n\
                 version=agent-stats 0.4.0 (protocol 2)\nhub=6f1c\nroute=http://mini.ts.net:7777 {\"role\":\"hub\",\"hubId\":\"6f1c\"}\n\
                 route=http://192.168.1.2:7777 \n";
        let p = parse_probe(o);
        assert_eq!((p.os.as_str(), p.arch.as_str(), p.hostname.as_str()), ("Linux", "x86_64", "cachy"));
        assert_eq!(p.version.as_deref(), Some("0.4.0"));
        assert_eq!(p.hubs, ["6f1c"]);
        assert!(p.fetcher && !p.launchd && !p.tokiscope);
        assert_eq!(p.routes.len(), 2);
        assert_eq!(pick_route(&p.routes, "6f1c").as_deref(), Some("http://mini.ts.net:7777"));
        assert_eq!(pick_route(&p.routes, "other"), None);
        assert_eq!(machine_name(&p), "cachy");
        assert_eq!(describe(&p), "Linux x86_64 · systemd · agent-stats 0.4.0 installed");
    }

    #[test]
    fn routes_pick_by_fleet_id_not_by_order() {
        let routes = vec![
            ("http://a:7777".to_string(), r#"{"role":"hub","hubId":"someone-else"}"#.to_string()),
            ("http://b:7777".to_string(), "<html>".to_string()),
            ("http://c:7777".to_string(), r#"{"role":"hub","hubId":"ours"}"#.to_string()),
        ];
        assert_eq!(pick_route(&routes, "ours").as_deref(), Some("http://c:7777"));
    }

    #[test]
    fn triples_and_versions() {
        assert_eq!(target_triple("Linux", "aarch64"), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(target_triple("Darwin", "arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_triple("Linux", "armv7l"), None);
        assert_eq!(target_triple("FreeBSD", "amd64"), None);
        assert!(version_at_least("0.5.10", "0.5.9"));
        assert!(version_at_least("0.6.0", "0.6.0"));
        assert!(!version_at_least("0.5.0", "0.6.0"));
        assert!(!version_at_least("garbage", "0.1.0"));
    }

    /// Downloads a real release from GitHub: `cargo test -p agent-stats-fleet -- --ignored`.
    #[test]
    #[ignore]
    fn fetches_and_verifies_a_release() {
        let (asset, bytes, sums) = fetch_release("0.5.0", "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset, "agent-stats-0.5.0-x86_64-unknown-linux-gnu.tar.gz");
        assert!(bytes.len() > 100_000 && String::from_utf8_lossy(&sums).contains(&asset));
    }

    #[test]
    fn the_join_script_carries_the_code_only_inside() {
        let s = join_script("http://mini:7777", "K7Q2-9XMD", "Bob's box", false);
        assert!(s.contains("CODE='K7Q2-9XMD'"));
        assert!(s.contains(r"LABEL='Bob'\''s box'"));
        assert!(s.contains("RELABEL=0"));
    }
}
