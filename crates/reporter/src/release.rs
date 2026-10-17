//! What `agent-stats update --auto` needs from the reporter's settings when it updates from a hub: which joined hub
//! to follow, its token, and the first of its routes that really is that hub (same check as reporting: the
//! well-known descriptor must carry the hubId learned at join, so the token never goes to another machine).

use crate::config::{read_file, resolve};
use crate::hub::{check, well_known, Http};
use std::path::Path;

/// A joined hub, as the updater uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseHub {
    pub name: String,
    pub id: String,
    pub routes: Vec<String>,
    pub token: String,
}

/// The route that answered as this hub, and the version its descriptor reports (if any).
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub url: String,
    pub version: Option<String>,
}

/// Reads `<config_dir>/config.toml` (and the environment, as the reporter does) and picks the hub named `name`, or
/// the first hub in config.toml. The implicit hub of the legacy environment has no hubId, so it is never followed.
pub fn pick_hub(config_dir: &Path, name: Option<&str>) -> Result<ReleaseHub, String> {
    let file = config_dir.join("config.toml");
    let cfg = read_file(&file)?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let settings = resolve(cfg, file.clone(), config_dir, &env);
    let hub = match name {
        Some(n) => settings.hubs.iter().find(|h| h.name == n).ok_or_else(|| {
            let known: Vec<&str> = settings.hubs.iter().map(|h| h.name.as_str()).collect();
            format!("no hub named {n:?} in {} (joined: {})", file.display(), list(&known))
        })?,
        None => settings
            .hubs
            .iter()
            .find(|h| !h.legacy)
            .ok_or_else(|| format!("no joined hub in {} (run `agent-stats join` first)", file.display()))?,
    };
    let id = hub.id.clone().ok_or_else(|| {
        format!("hub {:?} has no hubId, so its routes cannot be verified; join it again with a pairing code", hub.name)
    })?;
    let token = hub.token.clone().ok_or_else(|| format!("hub {:?} has no token ({})", hub.name, hub.token_source))?;
    if hub.routes.is_empty() {
        return Err(format!("hub {:?} has no routes", hub.name));
    }
    Ok(ReleaseHub { name: hub.name.clone(), id, routes: hub.routes.clone(), token })
}

fn list(names: &[&str]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// The first route, in order, whose well-known descriptor is this hub. Err lists why each route was refused.
pub fn find_route(hub: &ReleaseHub) -> Result<Route, String> {
    let http = Http::new();
    let mut errors = Vec::new();
    for r in &hub.routes {
        match well_known(&http, r).and_then(|d| check(&d, &hub.id).map(|()| d)) {
            Ok(d) => return Ok(Route { url: r.clone(), version: d.version }),
            Err(e) => errors.push(format!("{r}: {e}")),
        }
    }
    Err(format!("no route reaches hub {:?} ({})", hub.name, errors.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::tests::{dead_url, hub_server};

    #[test]
    fn picks_by_name_or_first_and_verifies_routes() {
        let d = crate::ingest::tests::tmpdir("release");
        let other = hub_server("other", 200);
        let right = hub_server("abc", 200);
        let dead = dead_url();
        let text = format!(
            "[[hub]]\nname = \"home\"\nid = \"abc\"\nroutes = [\"{dead}\", \"{}\", \"{}\"]\ntoken_file = \"secrets/h.token\"\n\n\
             [[hub]]\nname = \"team\"\nroutes = [\"{}\"]\n",
            other.url, right.url, right.url
        );
        std::fs::write(d.join("config.toml"), text).unwrap();
        agent_stats_common::write_secret(&d.join("secrets/h.token"), "tok").unwrap();
        let h = pick_hub(&d, None).unwrap();
        assert_eq!((h.name.as_str(), h.id.as_str(), h.token.as_str()), ("home", "abc", "tok"));
        assert_eq!(find_route(&h).unwrap().url, right.url);
        // The other hub's route is refused, with a reason.
        let wrong = ReleaseHub { routes: vec![other.url.clone()], ..h };
        assert!(find_route(&wrong).unwrap_err().contains("is hub other, expected abc"));
        assert!(pick_hub(&d, Some("team")).unwrap_err().contains("no hubId"));
        assert!(pick_hub(&d, Some("nope")).unwrap_err().contains("joined: home, team"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
