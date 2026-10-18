//! `fleet discover`: candidates from Tailscale (`tailscale status --json`) and Bonjour (`_agent-stats._tcp`, with
//! `dns-sd` on macOS or `avahi-browse` on Linux), merged by name, then each online one asked for its well-known
//! descriptor on the hub port. Only a suggestion list: nothing here enrolls a machine. Every external command runs
//! under a deadline and is killed when it passes, so the whole thing ends within `--timeout`.

use crate::{norm_name, Args, Res};
use serde_json::{json, Value};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVICE: &str = "_agent-stats._tcp";
const DEFAULT_PORT: u16 = 7777;

/// A machine Tailscale knows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsNode {
    /// The first label of its MagicDNS name (`killians-mac-mini`), else its host name.
    pub name: String,
    /// Its MagicDNS name without the final dot.
    pub dns_name: Option<String>,
    pub os: Option<String>,
    pub online: bool,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsStatus {
    pub me: Option<TsNode>,
    pub peers: Vec<TsNode>,
}

/// The `tailscale` CLI: on PATH, else inside the Mac app.
fn tailscale_bin() -> Option<String> {
    if let Some(b) = std::env::var("AGENT_STATS_TAILSCALE").ok().filter(|s| !s.is_empty()) {
        return Some(b);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let p = dir.join("tailscale");
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    let app = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
    std::path::Path::new(app).is_file().then(|| app.to_string())
}

/// `tailscale status --json`, parsed; None without Tailscale (or when it is stopped).
pub fn tailscale_status() -> Option<TsStatus> {
    let out = run_for(Command::new(tailscale_bin()?).args(["status", "--json"]), Duration::from_secs(3))?;
    parse_tailscale(&out)
}

fn ts_os(os: &str) -> Option<Option<String>> {
    match os.to_ascii_lowercase().as_str() {
        "macos" => Some(Some("macos".into())),
        "linux" => Some(Some("linux".into())),
        "windows" => Some(Some("windows".into())),
        // Phones and TVs can't run agent-stats: not candidates at all.
        "ios" | "android" | "tvos" => None,
        _ => Some(None),
    }
}

fn ts_node(v: &Value) -> Option<TsNode> {
    let os = ts_os(v["OS"].as_str().unwrap_or(""))?;
    let dns_name = v["DNSName"].as_str().map(|d| d.trim_end_matches('.').to_string()).filter(|d| !d.is_empty());
    let name = dns_name
        .as_deref()
        .and_then(|d| d.split('.').next())
        .map(str::to_string)
        .unwrap_or_else(|| norm_name(v["HostName"].as_str().unwrap_or("")));
    if name.is_empty() {
        return None;
    }
    Some(TsNode {
        name,
        dns_name,
        os,
        online: v["Online"].as_bool().unwrap_or(false),
        addresses: v["TailscaleIPs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect(),
    })
}

pub fn parse_tailscale(text: &str) -> Option<TsStatus> {
    let v: Value = serde_json::from_str(text).ok()?;
    let me = v.get("Self").and_then(ts_node).map(|mut n| {
        n.online = true;
        n
    });
    let mut peers: Vec<TsNode> = v["Peer"].as_object().into_iter().flatten().filter_map(|(_, p)| ts_node(p)).collect();
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    Some(TsStatus { me, peers })
}

/// A Bonjour-advertised agent-stats: instance name, host, port and TXT.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Advert {
    pub instance: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub txt: Vec<(String, String)>,
}

impl Advert {
    fn txt(&self, k: &str) -> Option<&str> {
        self.txt.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
}

/// Instance names from `dns-sd -B` output (the column under "Instance Name", `Add` lines only).
pub fn parse_dns_sd_browse(out: &str) -> Vec<String> {
    let mut col = None;
    let mut names: Vec<String> = Vec::new();
    for line in out.lines() {
        if let Some(i) = line.find("Instance Name") {
            col = Some(i);
            continue;
        }
        let mut f = line.split_whitespace();
        if f.nth(1) != Some("Add") {
            continue;
        }
        let name = match col.and_then(|c| line.get(c..)) {
            Some(n) => n.trim().to_string(),
            None => line.split_whitespace().skip(6).collect::<Vec<_>>().join(" "),
        };
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Host, port and TXT from `dns-sd -L` output: `… can be reached at Host.local.:7777 (interface 12)`, then the TXT
/// entries (`hubId=… name=home`) on the next line.
pub fn parse_dns_sd_lookup(instance: &str, out: &str) -> Option<Advert> {
    let mut lines = out.lines();
    let reach = lines.by_ref().find(|l| l.contains(" can be reached at "))?;
    let at = reach.split(" can be reached at ").nth(1)?.split_whitespace().next()?;
    let (host, port) = at.rsplit_once(':')?;
    let txt = lines
        .next()
        .map(|l| {
            l.split_whitespace()
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(Advert {
        instance: instance.to_string(),
        host: Some(host.trim_end_matches('.').to_string()),
        port: port.parse().ok(),
        txt,
    })
}

/// `avahi-browse -rpt` resolved lines: `=;eth0;IPv4;name;_agent-stats._tcp;local;host.local;192.168.1.20;7777;"k=v" …`.
pub fn parse_avahi(out: &str) -> Vec<Advert> {
    let mut ads: Vec<Advert> = Vec::new();
    for line in out.lines().filter(|l| l.starts_with("=;")) {
        let f: Vec<&str> = line.splitn(10, ';').collect();
        if f.len() < 9 || f[4] != SERVICE {
            continue;
        }
        let instance = avahi_unescape(f[3]);
        if ads.iter().any(|a| a.instance == instance) {
            continue;
        }
        let txt = f
            .get(9)
            .map(|t| {
                t.split("\" \"")
                    .map(|s| s.trim_matches('"'))
                    .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        ads.push(Advert { instance, host: Some(f[6].trim_end_matches('.').to_string()), port: f[8].parse().ok(), txt });
    }
    ads
}

/// avahi's `\032` (decimal) escapes.
fn avahi_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            if let Ok(n) = std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("").parse::<u8>() {
                out.push(n);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Runs a command and returns its stdout once it exits, or what it printed by `limit` (then it is killed): `dns-sd`
/// never exits on its own.
fn run_for(cmd: &mut Command, limit: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    reader.join().ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn has(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

/// Bonjour adverts of `_agent-stats._tcp`, within `limit`.
pub fn bonjour(limit: Duration) -> Vec<Advert> {
    if cfg!(target_os = "macos") && has("dns-sd") {
        let browse = limit.mul_f64(0.6);
        let names = run_for(Command::new("dns-sd").args(["-B", SERVICE, "local"]), browse)
            .map(|o| parse_dns_sd_browse(&o))
            .unwrap_or_default();
        let resolve = limit.saturating_sub(browse);
        let handles: Vec<_> = names
            .into_iter()
            .take(32)
            .map(|n| {
                std::thread::spawn(move || {
                    run_for(Command::new("dns-sd").args(["-L", &n, SERVICE, "local"]), resolve)
                        .and_then(|o| parse_dns_sd_lookup(&n, &o))
                        .unwrap_or(Advert { instance: n, ..Default::default() })
                })
            })
            .collect();
        return handles.into_iter().filter_map(|h| h.join().ok()).collect();
    }
    if has("avahi-browse") {
        return run_for(Command::new("avahi-browse").args(["-rpt", SERVICE]), limit)
            .map(|o| parse_avahi(&o))
            .unwrap_or_default();
    }
    Vec::new()
}

/// A merged candidate.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub name: String,
    pub host: Option<String>,
    pub addresses: Vec<String>,
    pub os: Option<String>,
    pub sources: Vec<String>,
    pub online: bool,
    pub me: bool,
    /// Where to ask for the descriptor.
    pub url: Option<String>,
    pub agent_stats: Option<Value>,
    pub member: bool,
}

/// Tailscale nodes and Bonjour adverts, one candidate per machine name.
pub fn merge(ts: Option<&TsStatus>, ads: &[Advert], my_name: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    if let Some(ts) = ts {
        for (n, me) in ts.me.iter().map(|n| (n, true)).chain(ts.peers.iter().map(|n| (n, false))) {
            out.push(Candidate {
                name: n.name.clone(),
                host: n.dns_name.clone(),
                addresses: n.addresses.clone(),
                os: n.os.clone(),
                sources: vec!["tailscale".into()],
                online: n.online,
                me,
                url: n.dns_name.as_ref().map(|d| format!("http://{d}:{DEFAULT_PORT}")),
                ..Default::default()
            });
        }
    }
    for a in ads {
        let name = a.txt("node").map(str::to_string).unwrap_or_else(|| a.instance.clone());
        let key = norm_name(&name);
        let url = a.host.as_ref().map(|h| format!("http://{h}:{}", a.port.unwrap_or(DEFAULT_PORT)));
        let os = a.txt("os").map(str::to_string);
        match out.iter_mut().find(|c| norm_name(&c.name) == key) {
            Some(c) => {
                c.sources.push("bonjour".into());
                c.online = true;
                // On the same network the advertised .local address answers fastest and needs no tailnet.
                if url.is_some() {
                    c.url = url;
                }
                if c.os.is_none() {
                    c.os = os;
                }
            }
            None => out.push(Candidate {
                name: key.clone(),
                host: a.host.clone(),
                os,
                sources: vec!["bonjour".into()],
                online: true,
                me: key == norm_name(my_name),
                url,
                ..Default::default()
            }),
        }
    }
    out
}

/// Asks `url` for its well-known descriptor within `limit`.
pub fn probe(url: &str, limit: Duration) -> Option<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(limit))
        .build()
        .into();
    let mut res = agent.get(format!("{url}{}", agent_stats_common::WELL_KNOWN)).call().ok()?;
    if res.status().as_u16() != 200 {
        return None;
    }
    let d: Value = serde_json::from_str(&res.body_mut().read_to_string().ok()?).ok()?;
    d.get("role")?;
    Some(json!({
        "url": url,
        "role": d["role"],
        "fleetId": d["hubId"],
        "name": d["name"],
        "node": d["node"],
        "version": d["version"],
    }))
}

/// Discovers, probes and marks candidates; at most `limit` in all.
pub fn discover(limit: Duration) -> Vec<Candidate> {
    let start = Instant::now();
    let my_name = agent_stats_hub::config::hostname();
    let ts = std::thread::spawn(tailscale_status);
    let ads = bonjour(limit.mul_f64(0.5));
    let ts = ts.join().ok().flatten();
    let mut cands = merge(ts.as_ref(), &ads, &my_name);
    let left = limit.saturating_sub(start.elapsed()).max(Duration::from_millis(300)).min(Duration::from_millis(1500));
    let handles: Vec<_> = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| c.online && c.os.as_deref() != Some("windows"))
        .filter_map(|(i, c)| c.url.clone().map(|u| (i, u)))
        .map(|(i, u)| std::thread::spawn(move || (i, probe(&u, left))))
        .collect();
    for h in handles {
        if let Ok((i, d)) = h.join() {
            cands[i].agent_stats = d;
        }
    }
    // Members: names this machine's hub knows, or a descriptor with our fleet id.
    let (members, fleet) = match crate::local::HubCtx::open() {
        Ok(ctx) => (ctx.member_names(), Some(ctx.hub_id.clone())),
        Err(_) => Default::default(),
    };
    for c in &mut cands {
        let ours = fleet.is_some() && c.agent_stats.as_ref().and_then(|a| a["fleetId"].as_str()) == fleet.as_deref();
        c.member = ours || members.contains(&norm_name(&c.name));
    }
    // Online first, then by name; this machine last.
    cands.sort_by(|a, b| (a.me, !a.online, &a.name).cmp(&(b.me, !b.online, &b.name)));
    cands
}

pub fn to_json(c: &Candidate) -> Value {
    json!({
        "name": c.name,
        "host": c.host,
        "addresses": c.addresses,
        "os": c.os,
        "sources": c.sources,
        "online": c.online,
        "self": c.me,
        "member": c.member,
        "agentStats": c.agent_stats,
    })
}

/// `fleet discover`.
pub fn run(a: &Args) -> Res<()> {
    let cands = discover(a.timeout.unwrap_or(Duration::from_secs(3)));
    if a.json {
        println!("{}", Value::Array(cands.iter().map(to_json).collect()));
        return Ok(());
    }
    if cands.is_empty() {
        println!("Nothing found (no Tailscale, no Bonjour answers). Add a machine with --ssh user@host instead.");
        return Ok(());
    }
    for c in &cands {
        let what = match (&c.agent_stats, c.member) {
            _ if c.me => "this machine".to_string(),
            (_, true) => "in this fleet".to_string(),
            (Some(d), false) => format!(
                "agent-stats {} ({} \"{}\")",
                d["version"].as_str().unwrap_or("?"),
                d["role"].as_str().unwrap_or("?"),
                d["name"].as_str().unwrap_or("?")
            ),
            (None, false) if c.online => {
                format!("no hub answers; to add it: agent-stats fleet add --ssh <user>@{}", c.name)
            }
            (None, false) => "offline".to_string(),
        };
        println!(
            "{:<28} {:<8} {:<20} {}",
            c.name,
            c.os.as_deref().unwrap_or("?"),
            c.sources.join("+"),
            agent_stats_hub::admin::printable(&what)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = r#"{"Self":{"HostName":"Killian’s MacBook Air","DNSName":"killians-macbook-air.tail1.ts.net.","OS":"macOS","Online":true,"TailscaleIPs":["100.84.65.16","fd7a::1"]},
      "MagicDNSSuffix":"tail1.ts.net",
      "Peer":{"k1":{"HostName":"cachy","DNSName":"cachy.tail1.ts.net.","OS":"linux","Online":true,"TailscaleIPs":["100.90.44.10"]},
              "k2":{"HostName":"localhost","DNSName":"iphone181.tail1.ts.net.","OS":"iOS","Online":true,"TailscaleIPs":["100.93.77.105"]},
              "k3":{"HostName":"Killian’s Mac mini","DNSName":"killians-mac-mini.tail1.ts.net.","OS":"macOS","Online":false,"TailscaleIPs":["100.125.243.52"]}}}"#;

    #[test]
    fn tailscale_status_skips_phones_and_names_by_dns_label() {
        let s = parse_tailscale(TS).unwrap();
        let me = s.me.unwrap();
        assert_eq!(me.name, "killians-macbook-air");
        assert_eq!(me.dns_name.as_deref(), Some("killians-macbook-air.tail1.ts.net"));
        assert_eq!(me.addresses[0], "100.84.65.16");
        let names: Vec<&str> = s.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["cachy", "killians-mac-mini"]);
        assert_eq!(s.peers[0].os.as_deref(), Some("linux"));
        assert!(!s.peers[1].online);
    }

    #[test]
    fn dns_sd_output() {
        let b = "Browsing for _agent-stats._tcp.local\nDATE: ---Sun 18 Oct 2026---\n12:52:22.969  ...STARTING...\n\
                 Timestamp     A/R    Flags  if Domain               Service Type         Instance Name\n\
                 12:52:22.970  Add        3   1 local.               _agent-stats._tcp.   Killians-Mac-mini\n\
                 12:52:22.970  Add        3  12 local.               _agent-stats._tcp.   Killians-Mac-mini\n\
                 12:52:22.971  Add        2  12 local.               _agent-stats._tcp.   cachy box\n\
                 12:52:23.000  Rmv        0  12 local.               _agent-stats._tcp.   gone\n";
        assert_eq!(parse_dns_sd_browse(b), ["Killians-Mac-mini", "cachy box"]);
        let l = "Lookup Killians-Mac-mini._agent-stats._tcp.local\nDATE: ---Sun 18 Oct 2026---\n\
                 12:52:28.728  ...STARTING...\n\
                 12:52:28.728  Killians-Mac-mini._agent-stats._tcp.local. can be reached at Killians-Mac-mini.local.:7777 (interface 12)\n \
                 hubId=6f1c name=home node=Killians-Mac-mini v=0.6.0\n";
        let a = parse_dns_sd_lookup("Killians-Mac-mini", l).unwrap();
        assert_eq!(a.host.as_deref(), Some("Killians-Mac-mini.local"));
        assert_eq!(a.port, Some(7777));
        assert_eq!(a.txt("hubId"), Some("6f1c"));
        assert_eq!(a.txt("v"), Some("0.6.0"));
        assert!(parse_dns_sd_lookup("x", "nothing").is_none());
    }

    #[test]
    fn avahi_output() {
        let o = "+;eth0;IPv4;cachy;_agent-stats._tcp;local\n\
                 =;eth0;IPv4;cachy\\032box;_agent-stats._tcp;local;cachy.local;192.168.1.30;7777;\"hubId=6f1c\" \"name=home\"\n\
                 =;eth0;IPv6;cachy\\032box;_agent-stats._tcp;local;cachy.local;fe80::1;7777;\"hubId=6f1c\"\n";
        let ads = parse_avahi(o);
        assert_eq!(ads.len(), 1);
        assert_eq!(ads[0].instance, "cachy box");
        assert_eq!(ads[0].host.as_deref(), Some("cachy.local"));
        assert_eq!(ads[0].txt("name"), Some("home"));
    }

    #[test]
    fn merge_prefers_the_local_address_and_marks_self() {
        let ts = parse_tailscale(TS).unwrap();
        let ads = vec![Advert {
            instance: "Killians-Mac-mini".into(),
            host: Some("Killians-Mac-mini.local".into()),
            port: Some(7777),
            txt: vec![("node".into(), "Killians-Mac-mini".into())],
        }];
        let c = merge(Some(&ts), &ads, "Killians-MacBook-Air.local");
        let mini = c.iter().find(|c| c.name == "killians-mac-mini").unwrap();
        assert_eq!(mini.sources, ["tailscale", "bonjour"]);
        assert!(mini.online, "Bonjour answered, so it is online even if Tailscale says not");
        assert_eq!(mini.url.as_deref(), Some("http://Killians-Mac-mini.local:7777"));
        assert!(c.iter().find(|c| c.name == "killians-macbook-air").unwrap().me);
        let only_bonjour = merge(None, &ads, "Killians-Mac-mini");
        assert!(only_bonjour[0].me);
    }
}
