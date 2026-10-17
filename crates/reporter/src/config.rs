//! The reporter's settings: `config_dir()/config.toml` (machine, label, timezone, `[[hub]]` entries), the legacy
//! environment (`HUB_URL`, `HUB_TOKEN` or `config_dir()/hub_token`, `STATS_MACHINE`, `STATS_LABEL`, `STATS_TZ`),
//! and where the transcripts are. See docs/ARCHITECTURE.md, "config.toml (reporter)".

use agent_stats_common::{config_dir, home, read_secret};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the legacy reporter sent reports when `HUB_URL` was not set.
pub const LEGACY_DEFAULT_URL: &str = "http://mac-mini:7777";
/// The name of the hub the legacy environment defines.
pub const LEGACY_NAME: &str = "default";
/// The zone the JS reporter always counted in: a machine that ran it keeps counting there unless told otherwise.
pub const JS_TZ: &str = "Asia/Tokyo";

/// This machine's IANA zone, else UTC.
pub fn system_tz() -> String {
    jiff::tz::TimeZone::try_system().ok().and_then(|z| z.iana_name().map(str::to_owned)).unwrap_or_else(|| "UTC".into())
}

/// config.toml as written on disk. Unknown keys are ignored (and dropped when `join` rewrites the file).
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct FileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, rename = "hub", skip_serializing_if = "Vec::is_empty")]
    pub hubs: Vec<HubEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HubEntry {
    pub name: String,
    /// The hub's `hubId`, learned at join. Routes are only used when their well-known descriptor has this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub routes: Vec<String>,
    /// Relative to `config_dir()`, or absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
    /// This machine's read-only token for the hub (what the dashboard reads), for a widget; same rules as
    /// `token_file`. The reporter never uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_token_file: Option<String>,
}

/// A hub as the reporter uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hub {
    pub name: String,
    pub id: Option<String>,
    pub routes: Vec<String>,
    pub token: Option<String>,
    /// Where the token comes from, for messages (`HUB_TOKEN` for the legacy env).
    pub token_source: String,
    /// Defined by the legacy environment rather than config.toml.
    pub legacy: bool,
}

impl Hub {
    /// Where this hub's unacknowledged keys live in the state: by id when it has one (a name can be reused for
    /// another hub), by name for id-less legacy hubs.
    pub fn state_key(&self) -> String {
        match &self.id {
            Some(id) => format!("id:{id}"),
            None => self.name.clone(),
        }
    }
}

/// Transcript roots. `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `GROK_HOME` override the defaults under `$HOME`, as
/// they do for the agents themselves (and T3 Code).
#[derive(Debug, Clone, PartialEq)]
pub struct Roots {
    /// `~/.claude/projects`
    pub claude_projects: PathBuf,
    /// `~/.codex` (sessions/ and archived_sessions/ below it)
    pub codex: PathBuf,
    /// `~/.grok` (sessions/ below it)
    pub grok: PathBuf,
}

impl Roots {
    pub fn under(home: &Path) -> Roots {
        Roots {
            claude_projects: home.join(".claude").join("projects"),
            codex: home.join(".codex"),
            grok: home.join(".grok"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub home: String,
    pub roots: Roots,
    pub machine: String,
    pub label: String,
    pub tz_name: String,
    /// Where `tz_name` comes from: `STATS_TZ`, `config.toml`, `state` (the zone existing counts are in) or `system`.
    pub tz_from: &'static str,
    pub hubs: Vec<Hub>,
    pub config_file: PathBuf,
    /// config.toml as read (empty when there is none).
    pub file: FileConfig,
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn read_file(path: &Path) -> Result<FileConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn write_file(path: &Path, cfg: &FileConfig) -> Result<(), String> {
    let text = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let text = format!("# agent-stats reporter config. See docs/ARCHITECTURE.md.\n{text}");
    agent_stats_common::atomic_write(path, text.as_bytes(), Some(0o644)).map_err(|e| format!("{}: {e}", path.display()))
}

/// Resolves a hub entry's token file against the config directory.
pub fn token_path(dir: &Path, token_file: &str) -> PathBuf {
    let p = Path::new(token_file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

/// Route URLs compare without a trailing slash, scheme and host case-insensitively.
pub fn same_route(a: &str, b: &str) -> bool {
    a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// `mac-mini:7777` → `http://mac-mini:7777`; trailing slashes dropped.
pub fn normalize_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/');
    if u.contains("://") {
        u.to_owned()
    } else {
        format!("http://{u}")
    }
}

pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "unknown".into())
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Reads config.toml and the environment.
pub fn load() -> Result<Settings, String> {
    let file = config_file();
    let cfg = read_file(&file)?;
    let mut s = resolve(cfg, file, &config_dir(), &env);
    if s.tz_from == "system" {
        if let Some(tz) = counted_in(&agent_stats_common::state_dir()) {
            (s.tz_name, s.tz_from) = (tz, "state");
        }
    }
    Ok(s)
}

/// The zone this machine's existing counts are in, so a timezone left unset keeps meaning what it meant: the
/// reporter state's, or the JS reporter's fixed zone when only its state is there.
pub fn counted_in(state_dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Tz {
        tz: String,
    }
    match std::fs::read_to_string(state_dir.join("reporter-state.json")) {
        Ok(s) => serde_json::from_str::<Tz>(&s).ok().map(|t| t.tz).filter(|t| !t.is_empty()),
        Err(_) => state_dir.join("state.json").exists().then(|| JS_TZ.to_owned()),
    }
}

/// Combines config.toml with the environment. Environment variables win over the file (an existing service unit
/// keeps meaning what it meant); the file wins over the defaults.
pub fn resolve(file: FileConfig, config_file: PathBuf, dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Settings {
    let home_dir = env("HOME").map(PathBuf::from).unwrap_or_else(home);
    let machine =
        env("STATS_MACHINE").or_else(|| file.machine.clone().filter(|m| !m.is_empty())).unwrap_or_else(|| {
            let h = hostname();
            h.strip_suffix(".local").map(str::to_owned).unwrap_or(h)
        });
    let label = env("STATS_LABEL").or_else(|| file.label.clone().filter(|l| !l.is_empty())).unwrap_or(machine.clone());
    let (tz_name, tz_from) = match (env("STATS_TZ"), file.timezone.clone().filter(|t| !t.is_empty())) {
        (Some(t), _) => (t, "STATS_TZ"),
        (None, Some(t)) => (t, "config.toml"),
        (None, None) => (system_tz(), "system"),
    };
    let mut roots = Roots::under(&home_dir);
    if let Some(d) = env("CLAUDE_CONFIG_DIR") {
        roots.claude_projects = PathBuf::from(d).join("projects");
    }
    if let Some(d) = env("CODEX_HOME") {
        roots.codex = PathBuf::from(d);
    }
    if let Some(d) = env("GROK_HOME") {
        roots.grok = PathBuf::from(d);
    }

    let mut hubs: Vec<Hub> = file
        .hubs
        .iter()
        .map(|h| {
            let path = h.token_file.as_deref().map(|t| token_path(dir, t));
            Hub {
                name: h.name.clone(),
                id: h.id.clone().filter(|i| !i.is_empty()),
                routes: h.routes.iter().map(|r| normalize_url(r)).collect(),
                token: path.as_deref().and_then(read_secret),
                token_source: path.map(|p| p.display().to_string()).unwrap_or_else(|| "(no token_file)".into()),
                legacy: false,
            }
        })
        .collect();

    // The legacy environment defines one implicit hub, unless a configured hub already has its address (after
    // `join` to the same hub, the joined entry with its own token takes over).
    let legacy_token_file = dir.join("hub_token");
    let env_token = env("HUB_TOKEN");
    let url = env("HUB_URL");
    if url.is_some() || env_token.is_some() || legacy_token_file.exists() {
        let url = normalize_url(url.as_deref().unwrap_or(LEGACY_DEFAULT_URL));
        if !hubs.iter().any(|h| h.routes.iter().any(|r| same_route(r, &url))) {
            let (token, token_source) = match env_token {
                Some(t) => (Some(t), "HUB_TOKEN".to_owned()),
                None => (read_secret(&legacy_token_file), legacy_token_file.display().to_string()),
            };
            let name = if hubs.iter().any(|h| h.name == LEGACY_NAME) { "legacy" } else { LEGACY_NAME };
            hubs.insert(0, Hub { name: name.into(), id: None, routes: vec![url], token, token_source, legacy: true });
        }
    }
    Settings {
        home: home_dir.to_string_lossy().into_owned(),
        roots,
        machine,
        label,
        tz_name,
        tz_from,
        hubs,
        config_file,
        file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn envf(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn parses_config_toml() {
        let d = crate::ingest::tests::tmpdir("config");
        let text = r#"
machine = "cachyos-x8664"
label = "Linux workstation"
timezone = "Europe/Paris"
something_new = 1

[[hub]]
name = "home"
id = "6f1c"
routes = ["http://mac-mini:7777/", "100.1.2.3:7777"]
token_file = "secrets/hub-6f1c.token"

[[hub]]
name = "team"
routes = ["https://stats.example.com"]
"#;
        std::fs::write(d.join("config.toml"), text).unwrap();
        agent_stats_common::write_secret(&d.join("secrets/hub-6f1c.token"), "tok").unwrap();
        let file = read_file(&d.join("config.toml")).unwrap();
        assert_eq!(file.hubs.len(), 2);
        let s = resolve(file.clone(), d.join("config.toml"), &d, &envf(&[("HOME", "/h")]));
        assert_eq!(
            (s.machine.as_str(), s.label.as_str(), s.tz_name.as_str()),
            ("cachyos-x8664", "Linux workstation", "Europe/Paris")
        );
        assert_eq!(s.hubs.len(), 2);
        assert_eq!(s.hubs[0].routes, ["http://mac-mini:7777", "http://100.1.2.3:7777"]);
        assert_eq!(s.hubs[0].token.as_deref(), Some("tok"));
        assert_eq!(s.hubs[0].id.as_deref(), Some("6f1c"));
        assert_eq!(s.hubs[1].token, None);
        assert_eq!(s.roots, Roots::under(Path::new("/h")));
        // Environment wins over the file; transcript roots follow the agents' own variables.
        let s = resolve(
            file.clone(),
            d.join("config.toml"),
            &d,
            &envf(&[
                ("HOME", "/h"),
                ("STATS_MACHINE", "m2"),
                ("STATS_TZ", "UTC"),
                ("CLAUDE_CONFIG_DIR", "/c"),
                ("CODEX_HOME", "/x"),
                ("GROK_HOME", "/g"),
            ]),
        );
        assert_eq!((s.machine.as_str(), s.label.as_str(), s.tz_name.as_str()), ("m2", "Linux workstation", "UTC"));
        assert_eq!(s.roots.claude_projects, PathBuf::from("/c/projects"));
        assert_eq!((s.roots.codex.as_path(), s.roots.grok.as_path()), (Path::new("/x"), Path::new("/g")));
        // Round trip.
        write_file(&d.join("out.toml"), &file).unwrap();
        let mut back = read_file(&d.join("out.toml")).unwrap();
        back.hubs[0].routes = file.hubs[0].routes.clone();
        assert_eq!(back, file);
        assert!(read_file(&d.join("missing.toml")).unwrap() == FileConfig::default());
        std::fs::write(d.join("bad.toml"), "[[hub]]\nname = 1\n").unwrap();
        assert!(read_file(&d.join("bad.toml")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unset_timezone_keeps_the_zone_existing_counts_are_in() {
        let d = crate::ingest::tests::tmpdir("counted-in");
        assert_eq!(counted_in(&d), None, "nothing counted yet: the system zone applies");
        std::fs::write(d.join("state.json"), "{}").unwrap();
        assert_eq!(counted_in(&d).as_deref(), Some(JS_TZ), "only the JS reporter's state");
        std::fs::write(d.join("reporter-state.json"), r#"{"v":1,"tz":"Europe/Paris"}"#).unwrap();
        assert_eq!(counted_in(&d).as_deref(), Some("Europe/Paris"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_env_defines_an_implicit_hub() {
        let d = crate::ingest::tests::tmpdir("legacy");
        let cf = d.join("config.toml");
        // Nothing configured: no hub.
        let s = resolve(FileConfig::default(), cf.clone(), &d, &envf(&[("HOME", "/h")]));
        assert!(s.hubs.is_empty());
        assert_eq!((s.tz_name, s.tz_from), (system_tz(), "system"));
        // HUB_URL + HUB_TOKEN, as the golden tests and existing service units run it.
        let s = resolve(
            FileConfig::default(),
            cf.clone(),
            &d,
            &envf(&[("HOME", "/h"), ("HUB_URL", "http://127.0.0.1:1234/"), ("HUB_TOKEN", "t"), ("STATS_LABEL", "L")]),
        );
        assert_eq!(
            s.hubs,
            [Hub {
                name: "default".into(),
                id: None,
                routes: vec!["http://127.0.0.1:1234".into()],
                token: Some("t".into()),
                token_source: "HUB_TOKEN".into(),
                legacy: true
            }]
        );
        assert_eq!(s.label, "L");
        // Only the hub_token file: the old default URL.
        std::fs::write(d.join("hub_token"), "filetok\n").unwrap();
        let s = resolve(FileConfig::default(), cf.clone(), &d, &envf(&[("HOME", "/h")]));
        assert_eq!(s.hubs[0].routes, [LEGACY_DEFAULT_URL]);
        assert_eq!(s.hubs[0].token.as_deref(), Some("filetok"));
        // A joined hub with the same address replaces the implicit one; a different one is added after it.
        let joined = |route: &str| FileConfig {
            hubs: vec![HubEntry {
                name: "home".into(),
                id: Some("abc".into()),
                routes: vec![route.into()],
                token_file: None,
                view_token_file: None,
            }],
            ..Default::default()
        };
        let s = resolve(joined("http://MAC-MINI:7777/"), cf.clone(), &d, &envf(&[("HOME", "/h")]));
        assert_eq!(s.hubs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["home"]);
        let s = resolve(joined("https://stats.example.com"), cf.clone(), &d, &envf(&[("HOME", "/h")]));
        assert_eq!(s.hubs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["default", "home"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
