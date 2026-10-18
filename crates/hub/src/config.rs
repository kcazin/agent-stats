//! `hub.toml`, with the legacy JS hub's environment variables as fallbacks (HOST, PORT, HUB_TOKEN, PUBLISH_URL,
//! PUBLISH_TOKEN, GITHUB_USER), so the binary can run under the production unit's environment.

use serde::Deserialize;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the hub reads config and keeps state. Resolved once from the environment (or set by tests).
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// The mirror of release tarballs (`<version>/`), written by the updater and served under `/release`.
    pub releases_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        Paths {
            config_dir: agent_stats_common::config_dir(),
            state_dir: agent_stats_common::state_dir(),
            releases_dir: agent_stats_common::data_dir().join("releases"),
        }
    }
    pub fn db(&self) -> PathBuf {
        self.state_dir.join("hub.db")
    }
    pub fn hub_toml(&self) -> PathBuf {
        self.config_dir.join("hub.toml")
    }
    pub fn admin_token(&self) -> PathBuf {
        self.config_dir.join("secrets").join("hub-admin.token")
    }
    pub fn hub_id(&self) -> PathBuf {
        self.state_dir.join("hub-id")
    }
    /// The fleet-wide settings a replica syncs (docs/FLEET.md).
    pub fn fleet_toml(&self) -> PathBuf {
        self.config_dir.join("fleet.toml")
    }
    /// The token this replica shows the others of its fleet.
    pub fn replica_token(&self) -> PathBuf {
        self.config_dir.join("secrets").join("replica.token")
    }
    /// A path from hub.toml: absolute, or relative to the config directory.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.config_dir.join(p)
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Raw {
    name: Option<String>,
    listen: Option<String>,
    timezone: Option<String>,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    cors_origins: Vec<String>,
    legacy_token_file: Option<String>,
    github_user: Option<String>,
    new_repo_webhook: Option<String>,
    #[serde(default)]
    advertise: Vec<String>,
    trust_loopback: Option<bool>,
    max_connections_per_ip: Option<usize>,
    #[serde(default)]
    trusted_networks: Vec<String>,
    #[serde(default)]
    publish: Vec<RawPublish>,
    #[serde(default)]
    project: Vec<RawProject>,
    unattributed: Option<toml::Table>,
    #[serde(default)]
    pile: RawPile,
    fleet: Option<RawFleet>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFleet {
    node: Option<String>,
    always_on: Option<bool>,
    #[serde(default)]
    peers: Vec<String>,
    token_file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublish {
    name: String,
    url: String,
    token_file: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    key: String,
    name: Option<String>,
    color: Option<String>,
    #[serde(rename = "match", default)]
    globs: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPile {
    per_hour: Option<f64>,
    seed_drain: Option<f64>,
    seed_every_ms: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub key: String,
    pub name: String,
    pub color: String,
    pub globs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PublishTarget {
    pub name: String,
    pub url: String,
    pub token: String,
    pub timeout: Duration,
}

/// `[fleet]`: this hub is a replica of a fleet (docs/FLEET.md).
#[derive(Clone, Debug)]
pub struct FleetConfig {
    /// This replica's name: its machine name.
    pub node: String,
    /// Preferred for publishing (desktops and servers; laptops say false).
    pub always_on: bool,
    /// Routes of other replicas to start syncing from; the members table adds the rest.
    pub peers: Vec<String>,
    /// This machine's reporter token for the fleet, used to introduce this replica to the others the first time.
    pub token_file: Option<PathBuf>,
}

/// hub.toml keys that belong to the fleet rather than to one replica: in fleet mode they come from `fleet.toml`,
/// which replicas sync.
pub const FLEET_KEYS: &[&str] = &[
    "name",
    "timezone",
    "public",
    "cors_origins",
    "github_user",
    "new_repo_webhook",
    "publish",
    "project",
    "unattributed",
    "pile",
];

/// The pile of unprocessed tokens on the site (hub.mjs's PILE_RATE and first-run seed, as config).
#[derive(Clone, Debug)]
pub struct PileConfig {
    /// Trucks clear this many tokens an hour (also published as `pile.perHour`). hub.mjs: 1e9.
    pub per_hour: f64,
    /// First run only: start from today's tokens less `seed_drain` per `seed_every_ms` since midnight. hub.mjs:
    /// 100K per 15 s, the pile the site showed before the hub kept one.
    pub seed_drain: f64,
    pub seed_every_ms: f64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub listen: String,
    pub tz_name: String,
    pub tz: jiff::tz::TimeZone,
    pub public: bool,
    pub cors_origins: Vec<String>,
    pub legacy_token: Option<String>,
    pub github_user: Option<String>,
    pub new_repo_webhook: Option<String>,
    pub advertise: Vec<String>,
    /// Treat 127.0.0.1/::1 clients as admins (dashboard, /api/state). Turn off behind a local reverse proxy.
    pub trust_loopback: bool,
    /// Open connections one untrusted peer IP may hold (0 = unlimited). Raise it behind a reverse proxy that
    /// connects from one address.
    pub max_connections_per_ip: usize,
    /// Peers in these CIDRs are treated like loopback for the read-only admin routes (dashboard, /api/state,
    /// /api/repos, /api/public). Never for /report or /pair. Empty by default.
    pub trusted_networks: Vec<Cidr>,
    pub publish: Vec<PublishTarget>,
    pub projects: Vec<Project>,
    /// How tokens with no repository are split between projects (key, fraction).
    pub unattributed: Vec<(String, f64)>,
    pub pile: PileConfig,
    /// Was there a hub.toml at all? (Without one, the JS hub's secret files are used as they were.)
    pub from_file: bool,
    /// Set when this hub is a replica of a fleet.
    pub fleet: Option<FleetConfig>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Config {
    /// Loads `path` (missing file = defaults + environment). With `[fleet]`, the fleet-wide keys come from
    /// `fleet.toml` beside it when there is one (they win over the same keys in hub.toml).
    pub fn load(paths: &Paths, path: &Path) -> Result<Config, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Config::from_raw(paths, Raw::default(), false)
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut table: toml::Table = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if table.contains_key("fleet") {
            let fleet_file = paths.fleet_toml();
            match std::fs::read_to_string(&fleet_file) {
                Ok(f) => {
                    let fleet: toml::Table =
                        toml::from_str(&f).map_err(|e| format!("{}: {e}", fleet_file.display()))?;
                    for (k, v) in fleet {
                        if !FLEET_KEYS.contains(&k.as_str()) {
                            return Err(format!("{}: {k} is not a fleet-wide key", fleet_file.display()));
                        }
                        table.insert(k, v);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", fleet_file.display())),
            }
        }
        let raw: Raw = toml::Value::Table(table).try_into().map_err(|e| format!("{}: {e}", path.display()))?;
        Config::from_raw(paths, raw, true)
    }

    /// Parses hub.toml text (for tests).
    pub fn parse(paths: &Paths, text: &str) -> Result<Config, String> {
        Config::from_raw(paths, toml::from_str::<Raw>(text).map_err(|e| e.to_string())?, true)
    }

    fn from_raw(paths: &Paths, raw: Raw, from_file: bool) -> Result<Config, String> {
        // Unset: this machine's zone, or Asia/Tokyo without a hub.toml (the JS hub's environment, which hard-coded it).
        let tz_name = raw.timezone.filter(|t| !t.is_empty()).unwrap_or_else(|| {
            if from_file {
                jiff::tz::TimeZone::try_system()
                    .ok()
                    .and_then(|z| z.iana_name().map(str::to_owned))
                    .unwrap_or_else(|| "UTC".into())
            } else {
                "Asia/Tokyo".into()
            }
        });
        let tz = jiff::tz::TimeZone::get(&tz_name).map_err(|e| format!("timezone {tz_name:?}: {e}"))?;
        let listen = raw.listen.unwrap_or_else(|| {
            format!(
                "{}:{}",
                env("HOST").unwrap_or_else(|| "127.0.0.1".into()),
                env("PORT").unwrap_or_else(|| "7777".into())
            )
        });
        // The shared token of the JS hub: legacy_token_file, else $HUB_TOKEN, else (no hub.toml, like hub.mjs)
        // config_dir()/hub_token.
        let legacy_token = match &raw.legacy_token_file {
            Some(f) => Some(
                agent_stats_common::read_secret(&paths.resolve(f))
                    .ok_or_else(|| format!("legacy_token_file {f}: missing or empty"))?,
            ),
            None => env("HUB_TOKEN").or_else(|| {
                if from_file {
                    None
                } else {
                    agent_stats_common::read_secret(&paths.config_dir.join("hub_token"))
                }
            }),
        };
        let mut publish = Vec::new();
        for p in raw.publish {
            let token = match &p.token_file {
                Some(f) => agent_stats_common::read_secret(&paths.resolve(f))
                    .ok_or_else(|| format!("publish {}: token_file {f} missing or empty", p.name))?,
                None => String::new(),
            };
            publish.push(PublishTarget {
                name: p.name,
                url: p.url,
                token,
                timeout: Duration::from_secs(p.timeout_secs.unwrap_or(10)),
            });
        }
        if publish.is_empty() {
            if let Some(url) = env("PUBLISH_URL") {
                let token = env("PUBLISH_TOKEN")
                    .or_else(|| agent_stats_common::read_secret(&paths.config_dir.join("publish_token")))
                    .unwrap_or_default();
                publish.push(PublishTarget { name: "default".into(), url, token, timeout: Duration::from_secs(10) });
            }
        }
        let trusted_networks = raw
            .trusted_networks
            .iter()
            .map(|c| Cidr::parse(c).map_err(|e| format!("trusted_networks: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let mut projects: Vec<Project> = Vec::new();
        for p in raw.project {
            if p.key.is_empty() || p.key == "other" || projects.iter().any(|q| q.key == p.key) {
                return Err(format!("project key {:?} is empty, reserved or repeated", p.key));
            }
            projects.push(Project {
                name: p.name.unwrap_or_else(|| p.key.clone()),
                color: p.color.unwrap_or_default(),
                globs: p.globs,
                key: p.key,
            });
        }
        let mut unattributed = Vec::new();
        match raw.unattributed {
            Some(t) => {
                for (k, v) in t {
                    let f = v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
                    let Some(f) = f.filter(|f| f.is_finite() && *f >= 0.0) else {
                        return Err(format!("unattributed.{k}: not a non-negative number"));
                    };
                    if k != "other" && !projects.iter().any(|p| p.key == k) {
                        return Err(format!("unattributed.{k}: no such project"));
                    }
                    unattributed.push((k, f));
                }
            }
            None => unattributed.push(("other".to_string(), 1.0)),
        }
        let fleet = raw.fleet.map(|f| FleetConfig {
            node: f.node.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
                let h = hostname();
                h.strip_suffix(".local").map(str::to_owned).unwrap_or(h)
            }),
            always_on: f.always_on.unwrap_or(true),
            peers: f.peers,
            token_file: f.token_file.map(|t| paths.resolve(&t)),
        });
        let pile = PileConfig {
            per_hour: raw.pile.per_hour.unwrap_or(1e9),
            seed_drain: raw.pile.seed_drain.unwrap_or(1e5),
            seed_every_ms: raw.pile.seed_every_ms.unwrap_or(15_000.0),
        };
        Ok(Config {
            name: raw.name.unwrap_or_else(|| "hub".into()),
            listen,
            tz_name,
            tz,
            public: raw.public,
            cors_origins: raw.cors_origins,
            legacy_token,
            github_user: raw.github_user.filter(|s| !s.is_empty()).or_else(|| env("GITHUB_USER")),
            new_repo_webhook: raw.new_repo_webhook.filter(|s| !s.is_empty()),
            advertise: raw.advertise,
            trust_loopback: raw.trust_loopback.unwrap_or(true),
            max_connections_per_ip: raw.max_connections_per_ip.unwrap_or(8),
            trusted_networks,
            publish,
            projects,
            unattributed,
            pile,
            from_file,
            fleet,
        })
    }

    /// The public project of a repository: the first project with a matching glob. A glob without `/` is tested
    /// against the repo name only; a glob containing `/` against the normalized remote only
    /// (e.g. `github.com/kcazin/*`).
    pub fn project_of(&self, repo: Option<&str>, remote: Option<&str>) -> Option<&Project> {
        let name = repo.unwrap_or("");
        self.projects.iter().find(|p| {
            p.globs.iter().any(|g| {
                if g.contains('/') {
                    remote.is_some_and(|r| glob_match(g, r))
                } else {
                    glob_match(g, name)
                }
            })
        })
    }

    /// Is this TCP peer inside a `trusted_networks` CIDR?
    pub fn peer_trusted(&self, peer: IpAddr) -> bool {
        self.trusted_networks.iter().any(|c| c.contains(peer))
    }

    /// The project key, `other` when none matches (hub.mjs's `projectOf`).
    pub fn project_key(&self, repo: Option<&str>, remote: Option<&str>) -> &str {
        self.project_of(repo, remote).map_or("other", |p| p.key.as_str())
    }

    /// `projectList` in the public payload, and every key `projects` counts (configured order, then `other`).
    pub fn project_keys(&self) -> Vec<&str> {
        self.projects.iter().map(|p| p.key.as_str()).chain(std::iter::once("other")).collect()
    }

    /// URLs a reporter can reach this hub at: `advertise`, else `http://<listen>`.
    pub fn routes(&self) -> Vec<String> {
        if !self.advertise.is_empty() {
            return self.advertise.clone();
        }
        let host = match self.listen.rsplit_once(':') {
            Some(("0.0.0.0" | "[::]" | "::", port)) => format!("{}:{port}", hostname()),
            _ => self.listen.clone(),
        };
        vec![format!("http://{host}")]
    }
}

/// An IPv4 or IPv6 network, `addr/prefix` (a bare address is a host route).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Result<Cidr, String> {
        let t = text.trim();
        let (a, p) = match t.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (t, None),
        };
        let addr: IpAddr = a.parse().map_err(|_| format!("invalid network {text:?}: bad address"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|n| *n <= max)
                .ok_or_else(|| format!("invalid network {text:?}: prefix must be 0-{max}"))?,
        };
        Ok(Cidr { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 client on a dual-stack socket shows up as ::ffff:a.b.c.d.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let (a, b, bits) = match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(a) as u128, u32::from(b) as u128, 32u32),
            (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a), u128::from(b), 128u32),
            _ => return false,
        };
        let host_bits = bits - self.prefix as u32;
        if host_bits >= 128 {
            return true;
        }
        (a >> host_bits) == (b >> host_bits)
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length; the result is cut at the first NUL or the buffer's end.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r == 0 {
        let n = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..n]) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "localhost".into()
}

/// Case-insensitive glob over the whole string: `*` any run (including empty), `?` one character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*neon*", "Neon-tools"));
        assert!(glob_match("chum*", "chum"));
        assert!(!glob_match("chum*", "my-chum"));
        assert!(glob_match("content-hall*", "content-hall-site"));
        assert!(!glob_match("content-hall*", "my-content-hall"));
        assert!(glob_match("*space-rock*", "the-space-rock"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("github.com/kcazin/*", "github.com/kcazin/neon"));
    }

    fn cfg_with(globs: &str) -> Config {
        let paths = Paths {
            config_dir: "/nonexistent".into(),
            state_dir: "/nonexistent".into(),
            releases_dir: "/nonexistent".into(),
        };
        Config::parse(&paths, &format!("[[project]]\nkey = \"p\"\nmatch = [{globs}]\n")).unwrap()
    }

    #[test]
    fn globs_with_slash_match_remote_only() {
        let c = cfg_with(r#""github.com/kcazin/*", "*neon*""#);
        // Slash glob: remote only.
        assert_eq!(c.project_key(Some("anything"), Some("github.com/kcazin/x")), "p");
        assert_eq!(c.project_key(Some("github.com/kcazin/x"), None), "other");
        assert_eq!(c.project_key(Some("x"), Some("github.com/other/x")), "other");
        // No slash: repo name only, never the remote.
        assert_eq!(c.project_key(Some("my-neon"), None), "p");
        assert_eq!(c.project_key(Some("x"), Some("gitlab.com/o/neon")), "other");
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_v4() {
        let c = Cidr::parse("100.64.0.0/10").unwrap();
        assert!(c.contains(ip("100.64.0.0")));
        assert!(c.contains(ip("100.127.255.255")));
        assert!(!c.contains(ip("100.63.255.255")));
        assert!(!c.contains(ip("100.128.0.0")));
        assert!(!c.contains(ip("::1")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        let h = Cidr::parse("10.1.2.3/32").unwrap();
        assert!(h.contains(ip("10.1.2.3")) && !h.contains(ip("10.1.2.4")));
        assert!(Cidr::parse("10.1.2.3").unwrap().contains(ip("10.1.2.3")));
        // IPv4-mapped IPv6 peers match v4 networks.
        assert!(c.contains(ip("::ffff:100.100.1.1")));
    }

    #[test]
    fn cidr_v6() {
        let c = Cidr::parse("fd7a:115c:a1e0::/48").unwrap();
        assert!(c.contains(ip("fd7a:115c:a1e0::1")));
        assert!(c.contains(ip("fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff")));
        assert!(!c.contains(ip("fd7a:115c:a1e1::1")));
        assert!(!c.contains(ip("100.64.0.1")));
        assert!(Cidr::parse("::/0").unwrap().contains(ip("2001:db8::1")));
        let h = Cidr::parse("2001:db8::5/128").unwrap();
        assert!(h.contains(ip("2001:db8::5")) && !h.contains(ip("2001:db8::6")));
    }

    #[test]
    fn cidr_invalid_rejected_at_load() {
        for bad in ["", "nope", "100.64.0.0/33", "fd7a::/129", "1.2.3.4/-1", "1.2.3.4/x", "1.2.3/8", "/8"] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
        let paths = Paths {
            config_dir: "/nonexistent".into(),
            state_dir: "/nonexistent".into(),
            releases_dir: "/nonexistent".into(),
        };
        let err = Config::parse(&paths, "trusted_networks = [\"100.64.0.0/10\", \"bogus\"]").unwrap_err();
        assert!(err.contains("trusted_networks") && err.contains("bogus"), "{err}");
        assert!(Config::parse(&paths, "").unwrap().trusted_networks.is_empty());
    }

    #[test]
    fn example_parses() {
        let paths = Paths {
            config_dir: "/nonexistent".into(),
            state_dir: "/nonexistent".into(),
            releases_dir: "/nonexistent".into(),
        };
        let text = include_str!("../../../examples/hub.example.toml");
        // Strip the secret-file keys so the test doesn't need them.
        let text: String = text
            .lines()
            .filter(|l| !l.starts_with("legacy_token_file") && !l.starts_with("token_file"))
            .map(|l| format!("{l}\n"))
            .collect();
        let c = Config::parse(&paths, &text).unwrap();
        for (repo, remote, key) in [
            ("my-game", None, "game"),
            ("My-Game-2", None, "game"),
            ("render-engine", None, "game"),
            ("site", Some("github.com/me/site"), "site"),
            ("site", None, "other"),
            ("notes", Some("github.com/me/notes"), "other"),
        ] {
            assert_eq!(c.project_key(Some(repo), remote), key, "{repo} {remote:?}");
        }
        assert_eq!(c.unattributed, vec![("game".to_string(), 0.5), ("other".to_string(), 0.5)]);
        assert_eq!(c.tz_name, "Europe/London");
    }
}
