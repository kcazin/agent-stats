//! Advertises the hub on the local network as `_agent-stats._tcp` (so `agent-stats fleet discover` and a new
//! Mac's Tokiscope find it), with the system's own tool: `dns-sd -R` on macOS, `avahi-publish-service` on Linux.
//! Best effort: without the tool nothing is advertised and nothing fails. The child is killed when the guard drops.
//! `AGENT_STATS_NO_BONJOUR=1` turns it off.

use std::process::{Child, Command, Stdio};

pub const SERVICE: &str = "_agent-stats._tcp";

/// The advertising process; dropping it withdraws the advert.
pub struct Advert(Child);

impl Drop for Advert {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn on_path(bin: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

/// The command that advertises `name` on `port` with `txt` (`k=v` entries), if this system has one.
pub fn command(name: &str, port: u16, txt: &[(&str, &str)]) -> Option<Command> {
    let kv = txt.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={v}"));
    let port = port.to_string();
    if cfg!(target_os = "macos") && (on_path("dns-sd") || std::path::Path::new("/usr/bin/dns-sd").is_file()) {
        let mut c = Command::new("dns-sd");
        c.args(["-R", name, SERVICE, "local", &port]).args(kv);
        return Some(c);
    }
    if on_path("avahi-publish-service") {
        let mut c = Command::new("avahi-publish-service");
        c.args([name, SERVICE, &port]).args(kv);
        return Some(c);
    }
    None
}

/// Starts advertising; None when there is no tool, it is turned off, or it fails to start.
pub fn advertise(name: &str, port: u16, txt: &[(&str, &str)]) -> Option<Advert> {
    if std::env::var("AGENT_STATS_NO_BONJOUR").is_ok_and(|v| v == "1" || v == "true") {
        return None;
    }
    let mut c = command(name, port, txt)?;
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok().map(Advert)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_carries_the_txt_entries() {
        let Some(c) = command("mini", 7777, &[("hubId", "6f1c"), ("name", "home"), ("empty", "")]) else { return };
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"mini".to_string()) && args.contains(&"7777".to_string()));
        assert!(args.contains(&"hubId=6f1c".to_string()) && args.contains(&"name=home".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("empty")));
    }
}
