//! Request limits that need the hub's configuration: body caps per route and the `Host` allow-list that keeps
//! token-less (loopback / `trusted_networks`) access from being reachable by DNS rebinding.

use crate::config::{hostname, Config};

/// `/pair` takes a code, a machine name and a label.
pub const PAIR_BODY: usize = 4 << 10;
/// Anything else that reads a body (the test hooks).
pub const OTHER_BODY: usize = 64 << 10;

/// The host part of a `Host` header or URL authority, lower-case, without port, brackets kept for IPv6, no
/// trailing dot.
fn host_part(authority: &str) -> String {
    let a = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let h = match a.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(a, |(ip, _)| &a[..ip.len() + 2]),
        None => a.split(':').next().unwrap_or(a),
    };
    h.trim_end_matches('.').to_ascii_lowercase()
}

fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    host_part(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

/// May a token-less peer use the dashboard and read-only API with this `Host` header? Only for names that are
/// really this hub: localhost, loopback addresses, the listen host, `advertise` hosts, the machine's own name
/// (and `.local`), MagicDNS (`*.ts.net`) and IP literals. A rebound attacker hostname is none of those.
pub fn host_allowed(cfg: &Config, host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let h = host_part(host);
    if h.is_empty() {
        return false;
    }
    if matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]") || h.ends_with(".ts.net") {
        return true;
    }
    // An IP literal can't be rebound: the browser was pointed at an address, not at a name an attacker controls.
    if h.parse::<std::net::Ipv4Addr>().is_ok()
        || h.strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok())
    {
        return true;
    }
    let name = hostname().to_ascii_lowercase();
    if h == name || h == format!("{name}.local") {
        return true;
    }
    if cfg.listen.rsplit_once(':').is_some_and(|(l, _)| host_part(l) == h) {
        return true;
    }
    cfg.advertise.iter().any(|u| url_host(u) == h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn cfg(toml: &str) -> Config {
        Config::parse(
            &Paths {
                config_dir: "/nonexistent".into(),
                state_dir: "/nonexistent".into(),
                releases_dir: "/nonexistent".into(),
            },
            toml,
        )
        .unwrap()
    }

    #[test]
    fn host_allow_list() {
        let c = cfg("listen = \"100.64.0.9:7777\"\nadvertise = [\"http://stats.example.org:7777\", \"https://u:p@Hub.Example.net/x\"]");
        let ok = |h: &str| host_allowed(&c, Some(h));
        for h in [
            "localhost",
            "LOCALHOST:7777",
            "127.0.0.1:1234",
            "[::1]:7777",
            "[::1]",
            "100.64.0.9:7777",
            "stats.example.org",
            "hub.example.net:443",
            "mac-mini.tail1234.ts.net:7777",
            "localhost.",
            "100.101.102.103:7777",
            "[fd7a:115c:a1e0::1]:7777",
        ] {
            assert!(ok(h), "{h}");
        }
        let me = hostname().to_ascii_lowercase();
        assert!(ok(&format!("{me}:7777")) && ok(&format!("{me}.local")));
        for h in ["rebind.attacker.test", "evil.example:17881", "127.0.0.1.evil.test", "ts.net.evil.test", "", "x@y"] {
            assert!(!ok(h), "{h}");
        }
        assert!(!host_allowed(&c, None));
    }
}
