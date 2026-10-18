//! `agent-stats fleet`: this machine's place in its fleet (docs/FLEET.md). `create` starts a fleet here, `replica`
//! makes a joined machine (or the old hub) a replica, `request` asks a fleet to let this machine in without a code,
//! `requests`, `approve` and `deny` answer such requests, `status` shows the replicas and which one publishes.

use crate::local::{dedupe_routes, now_ms, HubCtx};
use crate::{Res, USAGE};
use agent_stats_hub::auth::sha256_hex;
use agent_stats_hub::config::{Paths, FLEET_KEYS};
use agent_stats_hub::fleet;
use agent_stats_reporter::config as rcfg;
use serde_json::{json, Value};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Default)]
struct Opts {
    json: bool,
    name: Option<String>,
    label: Option<String>,
    port: Option<u16>,
    leaf: bool,
    force: bool,
    rest: Vec<String>,
    /// Print nothing (`request` reports the outcome of the `replica` it runs).
    silent: bool,
}

fn parse(args: &[String]) -> Res<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().cloned().ok_or(format!("{n} needs a value"));
        match a.as_str() {
            "--json" => o.json = true,
            "--leaf" => o.leaf = true,
            "--force" => o.force = true,
            "--name" => o.name = Some(val("--name")?),
            "--label" => o.label = Some(val("--label")?),
            "--port" => o.port = Some(val("--port")?.parse().map_err(|_| "--port: a port number")?),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => o.rest.push(s.to_string()),
        }
    }
    Ok(o)
}

/// The `agent-stats fleet` subcommands above (`args[0]` is the subcommand).
pub fn main(args: &[String]) -> ExitCode {
    let sub = args[0].as_str();
    let o = match parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("agent-stats fleet {sub}: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let res = match sub {
        "status" => status(&o),
        "create" => create(&o),
        "replica" => replica(&o),
        "request" => request(&o),
        "requests" => requests(&o),
        "approve" => decide(&o, true),
        "deny" => decide(&o, false),
        _ => Err(format!("unknown command {sub}\n\n{USAGE}")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is_empty() => ExitCode::FAILURE,
        Err(e) => {
            if o.json {
                println!("{}", json!({ "error": e }));
            }
            eprintln!("agent-stats fleet {sub}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn emit(o: &Opts, v: Value, human: &str) {
    if o.silent {
    } else if o.json {
        println!("{v}");
    } else {
        println!("{human}");
    }
}

// ---- this machine ----

/// Laptops say no: a desktop or a server should publish for the fleet when one is around.
fn always_on() -> bool {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("pmset").args(["-g", "batt"]).output();
        return !out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("InternalBattery"));
    }
    let batteries = std::fs::read_dir("/sys/class/power_supply")
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("BAT")))
        .unwrap_or(false);
    !batteries
}

/// Addresses the other machines can reach this one at: its Tailscale name and addresses, `<hostname>.local`, its
/// LAN addresses.
fn advertise(port: u16) -> Vec<String> {
    let mut routes = Vec::new();
    if let Some(me) = crate::discover::tailscale_status().and_then(|t| t.me) {
        routes.extend(me.dns_name.map(|d| format!("http://{d}:{port}")));
        routes.extend(me.addresses.iter().filter(|a| !a.contains(':')).map(|a| format!("http://{a}:{port}")));
    }
    let host = agent_stats_hub::config::hostname();
    let host = host.strip_suffix(".local").unwrap_or(&host);
    routes.push(format!("http://{host}.local:{port}"));
    routes.extend(crate::local::lan_ipv4().into_iter().map(|a| format!("http://{a}:{port}")));
    dedupe_routes(routes)
}

fn loopback(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn is_loopback_route(r: &str) -> bool {
    let host = r.split("://").nth(1).unwrap_or(r).rsplit_once(':').map_or(r, |(h, _)| h);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn port_of(listen: &str) -> Option<u16> {
    listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

/// Starts (or restarts) a service here, unless an app runs agent-stats (it starts its own jobs).
fn service(role: &str) -> Res<bool> {
    if agent_stats_common::managed_by().is_some() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(exe).args(["service", "install", role]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "agent-stats service install {role}: {}",
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed")
        ));
    }
    Ok(true)
}

/// hub.toml for a replica: what the old file had that belongs to this machine (fleet-wide keys go to fleet.toml),
/// the listen address, the routes it advertises and `[fleet]`. A first comment line (an app's marker) stays first.
fn hub_toml(old: &str, mut local: toml::Table, listen: &str, advertise: &[String], fleet: toml::Table) -> Res<String> {
    local.insert("listen".into(), toml::Value::String(listen.into()));
    if !advertise.is_empty() {
        local.insert(
            "advertise".into(),
            toml::Value::Array(advertise.iter().cloned().map(toml::Value::String).collect()),
        );
    }
    local.insert("fleet".into(), toml::Value::Table(fleet));
    let body = toml::to_string(&local).map_err(|e| e.to_string())?;
    let marker = old.lines().next().filter(|l| l.starts_with('#')).map(|l| format!("{l}\n")).unwrap_or_default();
    Ok(format!(
        "{marker}# A replica of an agent-stats fleet: settings for this machine. The fleet's own settings (name,\n\
         # timezone, projects, publish) are in fleet.toml, which every replica shares. See docs/FLEET.md.\n{body}"
    ))
}

fn fleet_table(node: &str, always_on: bool, peers: &[String], token_file: Option<&str>) -> toml::Table {
    let mut f = toml::Table::new();
    f.insert("node".into(), toml::Value::String(node.into()));
    f.insert("always_on".into(), toml::Value::Boolean(always_on));
    if !peers.is_empty() {
        f.insert("peers".into(), toml::Value::Array(peers.iter().cloned().map(toml::Value::String).collect()));
    }
    if let Some(t) = token_file {
        f.insert("token_file".into(), toml::Value::String(t.into()));
    }
    f
}

fn write(path: &std::path::Path, text: &str) -> Res<()> {
    agent_stats_common::atomic_write(path, text.as_bytes(), Some(0o644)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Puts `http://127.0.0.1:<port>` first in this fleet's entry of the reporter's config.toml, so the reporter reports
/// to its own replica (the others stay, for when it isn't running).
fn loopback_first(file: &mut rcfg::FileConfig, fleet_id: &str, port: u16) {
    if let Some(e) = file.hubs.iter_mut().find(|e| e.id.as_deref() == Some(fleet_id)) {
        e.routes.retain(|r| !is_loopback_route(r));
        e.routes.insert(0, loopback(port));
    }
}

/// A token for this machine straight in the local hub's database (no running hub, no code): what `create` and the
/// old hub's conversion use for the reporter here.
fn enroll_here(paths: &Paths, machine: &str, label: &str, fleet_id: &str) -> Res<(String, String)> {
    let db = agent_stats_hub::db::open(&paths.db()).map_err(|e| e.to_string())?;
    let token = agent_stats_common::random_hex(32).map_err(|e| e.to_string())?;
    let view = agent_stats_common::random_hex(32).map_err(|e| e.to_string())?;
    let now = now_ms() as i64;
    db.execute(
        "INSERT INTO machine_tokens (machine, token_hash, label, created, last_seen, revoked) VALUES (?, ?, ?, ?, NULL, NULL)
           ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, label = excluded.label,
             created = excluded.created, last_seen = NULL, revoked = NULL",
        rusqlite::params![machine, sha256_hex(&token), label, now],
    )
    .map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO view_tokens (machine, token_hash, created) VALUES (?, ?, ?)
           ON CONFLICT (machine) DO UPDATE SET token_hash = excluded.token_hash, created = excluded.created",
        rusqlite::params![machine, sha256_hex(&view), now],
    )
    .map_err(|e| e.to_string())?;
    save_tokens(fleet_id, &token, &view)
}

/// Saves a machine's reporting and view tokens beside config.toml; returns their paths relative to the config dir.
fn save_tokens(fleet_id: &str, token: &str, view: &str) -> Res<(String, String)> {
    let dir = agent_stats_common::config_dir();
    let (t, v) = (format!("secrets/hub-{fleet_id}.token"), format!("secrets/hub-{fleet_id}.view.token"));
    agent_stats_common::write_secret(&dir.join(&t), token).map_err(|e| format!("{t}: {e}"))?;
    agent_stats_common::write_secret(&dir.join(&v), view).map_err(|e| format!("{v}: {e}"))?;
    Ok((t, v))
}

/// Adds (or replaces) the fleet's entry in config.toml; a machine with no timezone of its own takes the fleet's.
fn save_entry(settings: &rcfg::Settings, entry: rcfg::HubEntry, tz: Option<&str>) -> Res<rcfg::FileConfig> {
    let mut file = settings.file.clone();
    file.hubs.retain(|h| h.id != entry.id);
    file.hubs.push(entry);
    if let Some(tz) = tz.filter(|_| settings.tz_from == "system" && file.timezone.is_none()) {
        file.timezone = Some(tz.to_string());
    }
    Ok(file)
}

/// The port to listen on: `--port`, else 7777 when nothing else holds it.
fn pick_port(o: &Opts) -> Res<u16> {
    let port = o.port.unwrap_or(7777);
    let running_hub = agent_stats_common::read_status("hub").is_some_and(|s| agent_stats_common::pid_alive(s.pid));
    if !running_hub && std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
        return Err(format!("port {port} is taken on this machine; choose another with --port"));
    }
    Ok(port)
}

// ---- create / replica ----

fn create(o: &Opts) -> Res<()> {
    let paths = Paths::from_env();
    if paths.hub_toml().exists() {
        return replica(o);
    }
    let settings = rcfg::load()?;
    let port = pick_port(o)?;
    let node = settings.machine.clone();
    let fleet_id = agent_stats_hub::auth::read_or_create(&paths.hub_id(), 16, false).map_err(|e| e.to_string())?;
    let name = o.name.clone().unwrap_or_else(|| "home".into());
    let (token_file, view_file) = enroll_here(&paths, &node, &settings.label, &fleet_id)?;
    let routes = advertise(port);
    let mut file = save_entry(
        &settings,
        rcfg::HubEntry {
            name: name.clone(),
            id: Some(fleet_id.clone()),
            routes: std::iter::once(loopback(port)).chain(routes.iter().cloned()).collect(),
            token_file: Some(token_file.clone()),
            view_token_file: Some(view_file),
        },
        None,
    )?;
    loopback_first(&mut file, &fleet_id, port);
    let mut fleet_toml = toml::Table::new();
    fleet_toml.insert("name".into(), toml::Value::String(name.clone()));
    fleet_toml.insert("timezone".into(), toml::Value::String(settings.tz_name.clone()));
    write(
        &paths.fleet_toml(),
        &format!(
            "# The fleet's settings, shared by every replica: edit them on any one. See docs/FLEET.md.\n{}",
            toml::to_string(&fleet_toml).map_err(|e| e.to_string())?
        ),
    )?;
    let aon = always_on();
    let text = hub_toml(
        "",
        toml::Table::new(),
        &format!("0.0.0.0:{port}"),
        &routes,
        fleet_table(&node, aon, &[], Some(&token_file)),
    )?;
    write(&paths.hub_toml(), &text)?;
    rcfg::write_file(&settings.config_file, &file)?;
    let started = service("hub")?;
    if started && !agent_stats_common::read_status("report").is_some_and(|s| agent_stats_common::pid_alive(s.pid)) {
        service("report")?;
    }
    emit(
        o,
        json!({"fleetId": fleet_id, "name": name, "node": node, "listen": format!("0.0.0.0:{port}"), "startHub": !started, "alwaysOn": aon}),
        &format!(
            "Started fleet \"{name}\" with {node} as its first replica (listening on port {port}{}).\nAdd machines with: \
             agent-stats fleet add --ssh <user@host>, or agent-stats fleet invite",
            if started { "" } else { "; start the hub from the app" }
        ),
    );
    Ok(())
}

/// The fleet this machine's reporter joined: its config.toml entry with an id (`--name` picks one).
fn joined(settings: &rcfg::Settings, name: Option<&str>) -> Res<rcfg::HubEntry> {
    let mut with_id = settings.file.hubs.iter().filter(|h| h.id.is_some() && name.is_none_or(|n| n == h.name));
    let first = with_id.next().cloned();
    if with_id.next().is_some() {
        return Err("this machine reports to several hubs: choose the fleet with --name".into());
    }
    first.ok_or_else(|| {
        "this machine hasn't joined a fleet: `agent-stats fleet request <url>` (or `agent-stats join`) first, or \
         `agent-stats fleet create` to start one"
            .into()
    })
}

fn replica(o: &Opts) -> Res<()> {
    let paths = Paths::from_env();
    let settings = rcfg::load()?;
    let node = settings.machine.clone();
    let aon = always_on();
    let hub_path = paths.hub_toml();
    if let Ok(old) = std::fs::read_to_string(&hub_path) {
        let table: toml::Table = toml::from_str(&old).map_err(|e| format!("{}: {e}", hub_path.display()))?;
        let fleet_id = std::fs::read_to_string(paths.hub_id()).map(|s| s.trim().to_string()).unwrap_or_default();
        if let Some(f) = table.get("fleet").and_then(|f| f.as_table()) {
            let n = f.get("node").and_then(|n| n.as_str()).unwrap_or(&node);
            emit(
                o,
                json!({"fleetId": fleet_id, "node": n, "listen": table.get("listen").and_then(|l| l.as_str()), "startHub": false, "already": true}),
                &format!("{n} is already a replica of this fleet ({fleet_id})."),
            );
            return Ok(());
        }
        if fleet_id.is_empty() {
            return Err(format!(
                "{} exists but this hub has never run (no hub id); start it once first",
                hub_path.display()
            ));
        }
        // The old hub: the fleet's first replica. Its fleet-wide keys move to fleet.toml.
        let (mut local, mut shared) = (toml::Table::new(), toml::Table::new());
        for (k, v) in table {
            if FLEET_KEYS.contains(&k.as_str()) {
                shared.insert(k, v);
            } else {
                local.insert(k, v);
            }
        }
        if !paths.fleet_toml().exists() {
            write(
                &paths.fleet_toml(),
                &format!(
                    "# The fleet's settings, shared by every replica: edit them on any one. See docs/FLEET.md.\n{}",
                    toml::to_string(&shared).map_err(|e| e.to_string())?
                ),
            )?;
        }
        write(&hub_path.with_extension("toml.bak"), &old)?;
        let listen = local.get("listen").and_then(|l| l.as_str()).unwrap_or("127.0.0.1:7777").to_string();
        let port = o.port.or_else(|| port_of(&listen)).unwrap_or(7777);
        let loopback_only =
            listen.starts_with("127.") || listen.starts_with("localhost") || listen.starts_with("[::1]");
        let listen = if loopback_only { format!("0.0.0.0:{port}") } else { listen };
        let advertised: Vec<String> = local
            .get("advertise")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect())
            .filter(|a: &Vec<String>| !a.is_empty())
            .unwrap_or_else(|| advertise(port));
        // The reporter here reports to this hub with its own token; without one (the hub ran alone), enroll it.
        let mut settings = settings;
        let token_file = match settings.file.hubs.iter().find(|h| h.id.as_deref() == Some(fleet_id.as_str())) {
            Some(e) if e.token_file.is_some() => e.token_file.clone(),
            _ => {
                let (t, v) = enroll_here(&paths, &node, &settings.label, &fleet_id)?;
                let name = shared.get("name").and_then(|n| n.as_str()).unwrap_or("home").to_string();
                settings.file = save_entry(
                    &settings,
                    rcfg::HubEntry {
                        name,
                        id: Some(fleet_id.clone()),
                        routes: advertised.clone(),
                        token_file: Some(t.clone()),
                        view_token_file: Some(v),
                    },
                    None,
                )?;
                Some(t)
            }
        };
        let mut file = settings.file.clone();
        loopback_first(&mut file, &fleet_id, port);
        let text = hub_toml(&old, local, &listen, &advertised, fleet_table(&node, aon, &[], token_file.as_deref()))?;
        write(&hub_path, &text)?;
        rcfg::write_file(&settings.config_file, &file)?;
        let restarted = service("hub")?;
        emit(
            o,
            json!({"fleetId": fleet_id, "node": node, "listen": listen, "startHub": !restarted, "restartHub": !restarted, "alwaysOn": aon}),
            &format!(
                "This hub is now the first replica ({node}) of its fleet. Its fleet-wide settings are in {} (the old \
                 hub.toml is in hub.toml.bak).{}",
                paths.fleet_toml().display(),
                if restarted { "" } else { " Restart the hub from the app." }
            ),
        );
        return Ok(());
    }
    // A machine that joined: a replica of that fleet.
    let entry = joined(&settings, o.name.as_deref())?;
    let fleet_id = entry.id.clone().unwrap_or_default();
    if let Ok(here) = std::fs::read_to_string(paths.hub_id()) {
        let here = here.trim();
        if !here.is_empty() && here != fleet_id && paths.db().exists() && !o.force {
            return Err(format!(
                "this machine already has a hub of its own ({here}), not of the fleet {fleet_id}: move {} aside, or \
                 pass --force to make it a replica anyway (its data merges into the fleet)",
                paths.db().display()
            ));
        }
    }
    let port = pick_port(o)?;
    std::fs::create_dir_all(&paths.state_dir).map_err(|e| e.to_string())?;
    write(&paths.hub_id(), &format!("{fleet_id}\n"))?;
    let peers: Vec<String> = entry.routes.iter().filter(|r| !is_loopback_route(r)).cloned().collect();
    let routes = advertise(port);
    let text = hub_toml(
        "",
        toml::Table::new(),
        &format!("0.0.0.0:{port}"),
        &routes,
        fleet_table(&node, aon, &peers, entry.token_file.as_deref()),
    )?;
    write(&hub_path, &text)?;
    let mut file = settings.file.clone();
    loopback_first(&mut file, &fleet_id, port);
    rcfg::write_file(&settings.config_file, &file)?;
    let started = service("hub")?;
    emit(
        o,
        json!({"fleetId": fleet_id, "node": node, "listen": format!("0.0.0.0:{port}"), "startHub": !started, "alwaysOn": aon}),
        &format!(
            "{node} is now a replica of fleet \"{}\": it syncs every machine's history from {} and shows the whole \
             dashboard on its own.{}",
            entry.name,
            peers.join(", "),
            if started { "" } else { " Start the hub from the app." }
        ),
    );
    Ok(())
}

// ---- join requests ----

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

fn get_json(agent: &ureq::Agent, url: &str) -> Res<(u16, Value)> {
    let mut res = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn request(o: &Opts) -> Res<()> {
    let [url] = o.rest.as_slice() else {
        return Err("usage: agent-stats fleet request <url> [--label L] [--leaf]".into());
    };
    let url = rcfg::normalize_url(url);
    let agent = http();
    let (st, d) = get_json(&agent, &format!("{url}{}", agent_stats_common::WELL_KNOWN))
        .map_err(|e| format!("{url} doesn't answer as agent-stats ({e})"))?;
    let fleet_id = d["hubId"]
        .as_str()
        .filter(|_| st == 200 && d["role"] == "hub")
        .ok_or(format!("{url} is not an agent-stats hub"))?;
    let settings = rcfg::load()?;
    let label = o.label.clone().unwrap_or_else(|| settings.label.clone());
    let token = agent_stats_common::random_hex(32).map_err(|e| e.to_string())?;
    let view = agent_stats_common::random_hex(32).map_err(|e| e.to_string())?;
    let th = sha256_hex(&token);
    let body = json!({
        "machine": settings.machine, "label": label, "platform": agent_stats_reporter::platform(),
        "version": agent_stats_common::VERSION, "tokenHash": th, "viewHash": sha256_hex(&view),
    });
    let mut res = agent
        .post(format!("{url}/join-request"))
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("{url}: {e}"))?;
    let status = res.status().as_u16();
    let ans: Value = serde_json::from_str(&res.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    match status {
        200 => {}
        404 => {
            return Err(format!("{url} runs an agent-stats without join requests: update it, or ask for a code there"))
        }
        _ => return Err(format!("{url} answered {status}: {}", ans["error"].as_str().unwrap_or(""))),
    }
    let (Some(id), Some(digits)) = (ans["id"].as_str(), ans["digits"].as_str()) else {
        return Err(format!("{url} answered without a request id"));
    };
    let expires = ans["expires"].as_f64().unwrap_or(now_ms() + fleet::REQUEST_TTL_MS);
    let fleet_name = ans["name"].as_str().or(d["name"].as_str()).unwrap_or("hub").to_string();
    emit(
        o,
        json!({"state": "pending", "id": id, "digits": digits, "expires": expires, "fleetId": fleet_id, "name": fleet_name}),
        &format!(
            "Asked fleet \"{fleet_name}\" to let {} in. Approve it on any Mac of the fleet (Tokiscope), or there with\n  \
             agent-stats fleet approve {id}\nBoth show the code {} {}. Waiting…",
            settings.machine,
            &digits[..2],
            &digits[2..]
        ),
    );
    // Until someone decides, or the request expires (a replica that is briefly away doesn't end it).
    let answer = loop {
        std::thread::sleep(Duration::from_secs(2));
        match get_json(&agent, &format!("{url}/join-request/{id}?key={th}")) {
            Ok((200, v)) if v["status"] != "pending" => break v,
            _ if now_ms() > expires + 30_000.0 => break json!({"status": "expired"}),
            _ => {}
        }
    };
    let status = answer["status"].as_str().unwrap_or("expired");
    if status != "approved" {
        emit(o, json!({"state": status}), &format!("The request was {status}."));
        return Err(String::new());
    }
    let (token_file, view_file) = save_tokens(fleet_id, &token, &view)?;
    let mut routes = vec![url.clone()];
    routes
        .extend(answer["routes"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(rcfg::normalize_url)));
    let mut seen: Vec<String> = Vec::new();
    routes.retain(|r| {
        let new = !seen.iter().any(|s| rcfg::same_route(s, r));
        seen.push(r.clone());
        new
    });
    let file = save_entry(
        &settings,
        rcfg::HubEntry {
            name: fleet_name.clone(),
            id: Some(fleet_id.to_string()),
            routes,
            token_file: Some(token_file),
            view_token_file: Some(view_file),
        },
        answer["timezone"].as_str(),
    )?;
    rcfg::write_file(&settings.config_file, &file)?;
    let mut start_hub = false;
    if !o.leaf {
        // Quietly: the outcome is one line below.
        let quiet =
            Opts { silent: true, port: o.port, force: o.force, name: Some(fleet_name.clone()), ..Default::default() };
        replica(&quiet)?;
        start_hub = agent_stats_common::managed_by().is_some();
    }
    emit(
        o,
        json!({"state": "approved", "fleetId": fleet_id, "name": fleet_name, "replica": !o.leaf, "startHub": start_hub}),
        &format!(
            "Approved: {} is in fleet \"{fleet_name}\"{}.",
            settings.machine,
            if o.leaf { " (reporting only)" } else { " as a replica" }
        ),
    );
    Ok(())
}

fn node_here(ctx: &HubCtx) -> String {
    ctx.cfg.fleet.as_ref().map(|f| f.node.clone()).unwrap_or_else(|| {
        let h = agent_stats_hub::config::hostname();
        h.strip_suffix(".local").map(str::to_owned).unwrap_or(h)
    })
}

fn requests(o: &Opts) -> Res<()> {
    let ctx = HubCtx::open()?;
    let list = fleet::pending(&ctx.db()?, now_ms()).map_err(|e| e.to_string())?;
    if o.json {
        let v: Vec<Value> = list
            .into_iter()
            .map(|r| serde_json::from_str(&agent_stats_hub::json::J::Obj(r).stringify()).unwrap_or(Value::Null))
            .collect();
        println!("{}", Value::Array(v));
        return Ok(());
    }
    if list.is_empty() {
        println!("No join requests waiting.");
    }
    for r in list {
        let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("-").to_string();
        let d = s("digits");
        println!(
            "{}  {} ({}){}  code {} {}   agent-stats fleet approve {}",
            s("id"),
            s("label"),
            s("machine"),
            if r.get("replaces").is_some_and(|v| v.truthy()) { ", replaces the machine of that name" } else { "" },
            &d[..2.min(d.len())],
            &d[2.min(d.len())..],
            s("id")
        );
    }
    Ok(())
}

fn decide(o: &Opts, approve: bool) -> Res<()> {
    let [id] = o.rest.as_slice() else {
        return Err(format!("usage: agent-stats fleet {} <id>", if approve { "approve" } else { "deny" }));
    };
    let ctx = HubCtx::open()?;
    let mut db = ctx.db()?;
    let machine = fleet::decide(&mut db, id, approve, &node_here(&ctx), now_ms())?;
    emit(
        o,
        json!({"ok": true, "id": id, "machine": machine, "approved": approve}),
        &if approve {
            format!("Approved {machine}: it joins within a few seconds.")
        } else {
            format!("Denied {machine}'s request.")
        },
    );
    Ok(())
}

// ---- status ----

fn status(o: &Opts) -> Res<()> {
    let ctx = HubCtx::open()?;
    let db = ctx.db()?;
    let now = now_ms();
    let list = fleet::members(&db).map_err(|e| e.to_string())?;
    let me = ctx.cfg.fleet.as_ref().map(|f| f.node.clone());
    let publisher =
        me.as_deref().map(|m| fleet::publisher(&list, m, ctx.cfg.fleet.as_ref().is_some_and(|f| f.always_on), now));
    let members: Vec<Value> = list
        .iter()
        .map(|m| {
            let this = me.as_deref() == Some(m.node.as_str());
            json!({
                "node": m.node, "routes": m.routes, "alwaysOn": m.always_on, "version": m.version, "revoked": m.revoked,
                "self": this, "lastSync": m.last_ok, "lastError": m.last_error,
                "reachable": this || m.last_ok.is_some_and(|t| now - t < 60_000.0),
            })
        })
        .collect();
    if o.json {
        println!(
            "{}",
            json!({"fleetId": ctx.hub_id, "name": ctx.cfg.name, "node": me, "replica": me.is_some(), "publisher": publisher, "members": members})
        );
        return Ok(());
    }
    println!("fleet \"{}\" ({})", ctx.cfg.name, ctx.hub_id);
    match &me {
        Some(m) => println!("this machine: replica {m}; publishing: {}", publisher.as_deref().unwrap_or("-")),
        None => println!("this machine: the hub (not a replica yet: agent-stats fleet replica)"),
    }
    for m in &list {
        let state = if m.revoked {
            "removed".to_string()
        } else if me.as_deref() == Some(m.node.as_str()) {
            "this machine".to_string()
        } else {
            match (m.last_ok, &m.last_error) {
                (Some(t), _) if now - t < 60_000.0 => "in sync".to_string(),
                (_, Some(e)) => format!("unreachable: {e}"),
                (Some(t), None) => format!("last synced {}s ago", ((now - t) / 1000.0) as u64),
                (None, None) => "not reached yet".to_string(),
            }
        };
        println!("  {:<24} {:<8} {}  {}", m.node, if m.always_on { "always" } else { "" }, state, m.routes.join(", "));
    }
    Ok(())
}
