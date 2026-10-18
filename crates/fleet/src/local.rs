//! This machine's side: its hub (hub.toml, hub.db, the fleet id), the addresses another machine might reach it at,
//! pairing codes, and `fleet invite` / `fleet list`.

use crate::{norm_name, Args, Res};
use agent_stats_hub::config::{Config, Paths};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

/// How long a pairing code minted here lives.
pub const CODE_TTL: Duration = Duration::from_secs(600);

/// The hub on this machine. Adding machines mints pairing codes in its database, so it needs one.
pub struct HubCtx {
    pub paths: Paths,
    pub cfg: Config,
    /// The fleet id (`state_dir()/hub-id`): every replica answers its well-known route with it.
    pub hub_id: String,
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

impl HubCtx {
    pub fn open() -> Res<HubCtx> {
        HubCtx::open_at(Paths::from_env())
    }

    pub fn open_at(paths: Paths) -> Res<HubCtx> {
        let id = std::fs::read_to_string(paths.hub_id()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let (Some(hub_id), true) = (id, paths.db().exists()) else {
            return Err("no hub on this machine: run this on a machine of the fleet that runs the hub (a Mac with \
                        Tokiscope, or `agent-stats service install hub`)"
                .into());
        };
        let mut cfg = Config::load(&paths, &paths.hub_toml())?;
        // As `hub pair`: without `advertise`, the address the running hub really listens on beats the config.
        let running = agent_stats_common::read_status("hub")
            .filter(|s| agent_stats_common::pid_alive(s.pid))
            .and_then(|s| s.detail.get("listen").and_then(|l| l.as_str()).map(str::to_string));
        if let (true, Some(l)) = (cfg.advertise.is_empty(), running) {
            cfg.listen = l;
        }
        Ok(HubCtx { paths, cfg, hub_id })
    }

    pub fn db(&self) -> Res<Connection> {
        agent_stats_hub::db::open(&self.paths.db()).map_err(|e| format!("{}: {e}", self.paths.db().display()))
    }

    /// The hub's port (`listen`), 7777 when it can't be read.
    pub fn port(&self) -> u16 {
        self.cfg.listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(7777)
    }

    /// Whether only this machine can reach the hub (it listens on loopback).
    pub fn loopback_only(&self) -> bool {
        let host = self.cfg.listen.rsplit_once(':').map_or(self.cfg.listen.as_str(), |(h, _)| h);
        matches!(host.trim_matches(['[', ']']), "127.0.0.1" | "localhost" | "::1")
    }

    /// A one-time pairing code; `replace` lets it swap the token of that machine name.
    pub fn mint(&self, label: Option<&str>, replace: Option<&str>) -> Res<String> {
        let db = self.db()?;
        agent_stats_hub::auth::create_code_for(&db, label, replace, CODE_TTL, now_ms()).map_err(|e| e.to_string())
    }

    /// Normalized names of the machines this hub knows (reported, or enrolled).
    pub fn member_names(&self) -> HashSet<String> {
        let Ok(db) = self.db() else { return HashSet::new() };
        let mut out = HashSet::new();
        if let Ok(mut st) = db.prepare("SELECT machine FROM machines UNION SELECT machine FROM machine_tokens") {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten().map(|m| norm_name(&m)));
            }
        }
        out
    }

    /// Routes another machine might reach this hub at, best first: what the hub advertises, then its Tailscale name
    /// and addresses, `<hostname>.local`, and its LAN addresses. Loopback routes are left out (useless from there).
    pub fn routes_back(&self) -> Vec<String> {
        let port = self.port();
        let mut routes: Vec<String> = self.cfg.routes();
        if let Some(ts) = crate::discover::tailscale_status() {
            if let Some(me) = ts.me {
                routes.extend(me.dns_name.map(|d| format!("http://{d}:{port}")));
                routes.extend(me.addresses.iter().filter(|a| !a.contains(':')).map(|a| format!("http://{a}:{port}")));
            }
        }
        let host = agent_stats_hub::config::hostname();
        let host = host.strip_suffix(".local").unwrap_or(&host);
        routes.push(format!("http://{host}.local:{port}"));
        routes.extend(lan_ipv4().into_iter().map(|a| format!("http://{a}:{port}")));
        dedupe_routes(routes)
    }
}

/// Routes in order, without repeats or loopback addresses.
pub fn dedupe_routes(routes: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in routes {
        let r = r.trim().trim_end_matches('/').to_string();
        let host = r.split("://").nth(1).unwrap_or(&r).rsplit_once(':').map_or(r.as_str(), |(h, _)| h).to_string();
        let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0");
        if !r.is_empty() && !loopback && !out.iter().any(|o| o.eq_ignore_ascii_case(&r)) {
            out.push(r);
        }
    }
    out
}

/// This machine's IPv4 addresses on its networks (no loopback, no link-local; Tailscale's come from Tailscale).
pub fn lan_ipv4() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(unix)]
    unsafe {
        let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifs) != 0 {
            return out;
        }
        let mut p = ifs;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let o = ip.octets();
                let tailscale = o[0] == 100 && (64..128).contains(&o[1]);
                if !ip.is_loopback() && !ip.is_link_local() && !tailscale && !out.contains(&ip.to_string()) {
                    out.push(ip.to_string());
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifs);
    }
    out
}

/// `fleet invite`: a one-time line that installs agent-stats and joins this fleet.
pub fn invite(a: &Args) -> Res<()> {
    let ctx = HubCtx::open()?;
    let url = ctx.routes_back().into_iter().next().ok_or("this hub has no address another machine can use")?;
    let code = ctx.mint(a.label.as_deref(), None)?;
    let line = format!(
        "curl -fsSL https://raw.githubusercontent.com/kcazin/agent-stats/main/install.sh | sh -s -- --join {url} \
         --code {code}{}",
        a.label.as_deref().map(|l| format!(" {}", crate::remote::sh_quote(l))).unwrap_or_default()
    );
    let expires = now_ms() + CODE_TTL.as_millis() as f64;
    if a.json {
        println!("{}", json!({"line": line, "url": url, "code": code, "expires": expires as i64}));
    } else {
        println!("{line}");
        eprintln!("Run that line on the new machine within 10 minutes (one use).");
        if ctx.loopback_only() {
            eprintln!(
                "Note: this hub listens on {} only, so no other machine can reach it yet. Open it to the network \
                 (listen and advertise in hub.toml) first.",
                ctx.cfg.listen
            );
        }
    }
    Ok(())
}

/// Nodes that are replicas, with their routes: `fleet_members`, when this hub has that table (fleet hubs).
fn replicas(db: &Connection) -> Vec<(String, Vec<String>)> {
    let has: bool = db
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fleet_members')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !has {
        return Vec::new();
    }
    let Ok(mut st) = db.prepare("SELECT node, routes FROM fleet_members WHERE revoked IS NULL") else {
        return Vec::new();
    };
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)));
    rows.map(|rows| {
        rows.flatten()
            .map(|(n, routes)| {
                let routes = routes
                    .map(|t| serde_json::from_str::<Vec<String>>(&t).unwrap_or_else(|_| vec![t]))
                    .unwrap_or_default();
                (n, routes)
            })
            .collect()
    })
    .unwrap_or_default()
}

/// `fleet list`.
pub fn list(a: &Args) -> Res<()> {
    let ctx = HubCtx::open()?;
    let db = ctx.db()?;
    let now = now_ms();
    let machines = agent_stats_hub::auth::machines(&db).map_err(|e| e.to_string())?;
    let reps = replicas(&db);
    let mut rows: Vec<Value> = Vec::new();
    for m in machines {
        let get = |k: &str| m.get(k);
        let name = get("machine").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let last = get("lastReport").and_then(|v| v.as_f64());
        let rep = reps.iter().find(|(n, _)| *n == name || norm_name(n) == norm_name(&name));
        let s = |k: &str| get(k).and_then(|v| v.as_str()).map(str::to_string);
        rows.push(json!({
            "machine": name,
            "label": s("label"),
            "role": if rep.is_some() { "replica" } else { "leaf" },
            "online": last.is_some_and(|l| now - l < agent_stats_hub::compute::ONLINE),
            "lastReport": last.map(|l| l as i64),
            "version": s("version"),
            "platform": s("platform"),
            "revoked": get("revoked").and_then(|v| v.as_f64()).is_some(),
            "routes": rep.map(|(_, r)| r.clone()).unwrap_or_default(),
        }));
    }
    if a.json {
        println!("{}", Value::Array(rows));
        return Ok(());
    }
    if rows.is_empty() {
        println!("No machines yet. Add one with `agent-stats fleet add --ssh <user@host>` or `fleet invite`.");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<8} {:<10} LABEL", "MACHINE", "ROLE", "STATE", "VERSION");
    for r in &rows {
        let state = if r["revoked"] == true {
            "revoked"
        } else if r["online"] == true {
            "online"
        } else {
            "offline"
        };
        println!(
            "{:<24} {:<8} {:<8} {:<10} {}",
            agent_stats_hub::admin::printable(r["machine"].as_str().unwrap_or("")),
            r["role"].as_str().unwrap_or(""),
            state,
            r["version"].as_str().unwrap_or("-"),
            agent_stats_hub::admin::printable(r["label"].as_str().unwrap_or("-"))
        );
    }
    Ok(())
}

/// The machine a code minted at `since` (ms) was redeemed for: the newest token created since then.
pub fn redeemed_since(db: &Connection, since: f64) -> Option<String> {
    db.query_row(
        "SELECT machine FROM machine_tokens WHERE created >= ? ORDER BY created DESC LIMIT 1",
        params![since as i64],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// When `machine` last reported here (ms).
pub fn last_seen(db: &Connection, machine: &str) -> Option<f64> {
    db.query_row("SELECT last_seen FROM machines WHERE machine = ?", params![machine], |r| r.get::<_, Option<i64>>(0))
        .optional()
        .ok()
        .flatten()
        .flatten()
        .map(|v| v as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_drop_loopback_and_repeats() {
        let r = dedupe_routes(vec![
            "http://127.0.0.1:7777".into(),
            "http://mini.ts.net:7777/".into(),
            "http://MINI.ts.net:7777".into(),
            "http://localhost:7777".into(),
            "http://192.168.1.20:7777".into(),
        ]);
        assert_eq!(r, ["http://mini.ts.net:7777", "http://192.168.1.20:7777"]);
    }

    #[test]
    fn list_reads_replicas_when_the_table_exists() {
        let db = Connection::open_in_memory().unwrap();
        assert!(replicas(&db).is_empty());
        db.execute_batch(
            "CREATE TABLE fleet_members (node TEXT PRIMARY KEY, routes TEXT, revoked INTEGER);
             INSERT INTO fleet_members VALUES ('mini', '[\"http://mini:7777\"]', NULL), ('old', '[]', 5);",
        )
        .unwrap();
        assert_eq!(replicas(&db), vec![("mini".to_string(), vec!["http://mini:7777".to_string()])]);
    }
}
