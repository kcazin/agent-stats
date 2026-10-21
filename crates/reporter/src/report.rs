//! The POST /report body (reporter.mjs's shape plus the protocol 2 fields) and the `--dry` summary.

use crate::git::GitIds;
use crate::js::{basename, basename_ext, dirname, repo_of};
use crate::state::{js_num_opt, Io, State};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::collections::{BTreeSet, HashMap};

/// Sessions written in this window are reported as open.
pub const OPEN_WINDOW: f64 = 30.0 * 60_000.0;
/// Claude sessions written in this window say whether their process still runs (`alive`): the hub counts one
/// whose process runs as working this long into a turn, and one whose process is gone not at all.
pub const ALIVE_WINDOW: f64 = 10.0 * 60_000.0;

#[derive(Serialize)]
pub struct DayRow<'a> {
    pub day: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    pub tokens: i64,
    pub output: i64,
}

#[derive(Serialize)]
pub struct RepoRow<'a> {
    pub day: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<&'a str>,
    pub tokens: i64,
    /// The repository's normalized git remote (`github.com/o/r`), when it has one. Protocol 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<&'a str>,
    /// The tokens by kind, as far as the agents said (a hub before 0.7 ignores it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io: Option<Io>,
}

#[derive(Serialize, Debug, PartialEq, Clone)]
pub struct Session {
    pub id: String,
    pub agent: &'static str,
    pub kind: &'static str,
    pub parent: Option<String>,
    pub project: String,
    pub cwd: Option<String>,
    pub repo: Option<String>,
    pub model: Option<String>,
    #[serde(rename = "lastActivity")]
    pub last_activity: i64,
    pub tokens: i64,
    pub state: Option<String>,
    #[serde(rename = "stateAt", serialize_with = "js_num_opt")]
    pub state_at: Option<f64>,
    pub io: Option<Io>,
    /// Whether the process writing it still runs, when that is known (see [`ALIVE_WINDOW`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alive: Option<bool>,
}

#[derive(Serialize)]
pub struct Body<'a> {
    /// Protocol 2: what this body means (absent = 1, the JS reporter).
    pub protocol: u32,
    pub version: &'a str,
    /// Protocol 2: the machine's stable id (the same value as `machine`, which old hubs key by).
    #[serde(rename = "machineId")]
    pub machine_id: &'a str,
    pub machine: &'a str,
    pub label: &'a str,
    pub platform: &'a str,
    pub at: i64,
    pub full: bool,
    pub days: Vec<DayRow<'a>>,
    pub repos: Vec<RepoRow<'a>>,
    pub sessions: Vec<Session>,
}

/// Node's `process.platform` for this build.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "solaris" | "illumos" => "sunos",
        os => os,
    }
}

fn is_temp_dir(cwd: &str) -> bool {
    // /^(\/private)?\/(tmp|var\/folders)\//
    let c = cwd.strip_prefix("/private").unwrap_or(cwd);
    c.starts_with("/tmp/") || c.starts_with("/var/folders/")
}

/// The thread id at the end of a Codex rollout name: `/([0-9a-f-]{36})\.jsonl$/` on the basename.
fn rollout_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    let b = stem.as_bytes();
    if b.len() < 36 || !b[b.len() - 36..].iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f' | b'-')) {
        return None;
    }
    Some(&stem[stem.len() - 36..])
}

/// The repo name of a working directory: its git remote's last segment, else the folder rules of `repo_of`.
pub fn repo_name(cwd: &str, home: &str, git: Option<&GitIds>) -> String {
    match git.and_then(|g| g.lookup(cwd)) {
        Some(id) => id.name,
        None => repo_of(cwd, home),
    }
}

/// The session a Claude transcript belongs to (a subagent's: its parent's) and the transcript folder it is in:
/// `projects/<folder>/<id>.jsonl` or `projects/<folder>/<id>/subagents/<agent>.jsonl`.
pub fn claude_owner(path: &str) -> (&str, &str) {
    let dir = dirname(path);
    if basename(dir) == "subagents" {
        let session = dirname(dir);
        (basename(session), basename(dirname(session)))
    } else {
        (basename_ext(path, ".jsonl"), basename(dir))
    }
}

/// Live sessions: transcripts written in the last 30 minutes, outside temp dirs. `alive`: by transcript path,
/// whether the Claude sessions' processes run, as far as known.
pub fn sessions(
    state: &State,
    now: f64,
    home: &str,
    git: Option<&GitIds>,
    alive: &HashMap<String, bool>,
) -> Vec<Session> {
    // Codex threads by id, to find a subagent's parent session.
    let mut codex_by_id: HashMap<&str, &str> = HashMap::new();
    for path in state.files.keys() {
        let b = basename(path);
        if let Some(id) = rollout_id(b) {
            codex_by_id.insert(id, basename_ext(path, ".jsonl"));
        }
    }
    let sep = std::path::MAIN_SEPARATOR;
    let sub_dir = format!("{sep}subagents{sep}");
    let mut out = Vec::new();
    for (path, f) in &state.files {
        if now - f.mtime > OPEN_WINDOW {
            continue;
        }
        let cwd = f.cwd.as_deref().filter(|c| !c.is_empty());
        // One-shot helper calls (e.g. T3 naming a thread) run in temp dirs; they count as tokens, not as agents.
        if is_temp_dir(cwd.unwrap_or("")) {
            continue;
        }
        let sub = path.contains(&sub_dir) || f.sub;
        let claude = f.agent == crate::state::Agent::Claude;
        // Claude Code keeps a subagent's transcript in <session>/subagents/, next to <session>.jsonl.
        let parent = if !sub {
            None
        } else if claude {
            Some(basename(dirname(dirname(path))).to_owned())
        } else {
            f.parent.as_deref().and_then(|p| codex_by_id.get(p)).map(|s| s.to_string())
        };
        out.push(Session {
            id: if f.agent == crate::state::Agent::Grok {
                basename(dirname(path))
            } else {
                basename_ext(path, ".jsonl")
            }
            .to_owned(),
            agent: f.agent.name(),
            kind: if sub { "subagent" } else { "main" },
            parent,
            project: match cwd {
                Some(c) => basename(c).to_owned(),
                None => basename(dirname(if sub { dirname(dirname(path)) } else { path })).to_owned(),
            },
            cwd: cwd.map(str::to_owned),
            repo: cwd.map(|c| repo_name(c, home, git)),
            model: f.model.clone().filter(|m| !m.is_empty()),
            last_activity: (f.mtime + 0.5).floor() as i64,
            tokens: f.tokens,
            state: f.state.clone().filter(|s| !s.is_empty()),
            state_at: f.state_at.filter(|t| *t != 0.0 && !t.is_nan()),
            io: f.io,
            alive: alive.get(path).copied(),
        });
    }
    out
}

fn split3(k: &str) -> (&str, Option<&str>, Option<&str>) {
    let mut it = k.split('|');
    (it.next().unwrap_or(""), it.next(), it.next())
}

/// Keys some hub has not acknowledged yet.
#[derive(Default)]
pub struct Dirty<'a> {
    pub days: BTreeSet<&'a str>,
    pub repos: BTreeSet<&'a str>,
}

/// Rows for the report: the dirty keys, or on a full sync every key from `cutoff` on plus the dirty ones.
pub fn rows<'a>(state: &'a State, full: bool, cutoff: &str, dirty: &Dirty) -> (Vec<DayRow<'a>>, Vec<RepoRow<'a>>) {
    let want = |k: &str, set: &BTreeSet<&str>| (full && split3(k).0 >= cutoff) || set.contains(k);
    let days = state
        .days
        .iter()
        .filter(|(k, _)| want(k, &dirty.days))
        .map(|(k, v)| {
            let (day, agent, model) = split3(k);
            DayRow { day, agent, model, tokens: v.tokens, output: v.output }
        })
        .collect();
    let repos = state
        .repos
        .iter()
        .filter(|(k, _)| want(k, &dirty.repos))
        .map(|(k, v)| {
            let (day, repo, _) = split3(k);
            let remote = repo.and_then(|r| state.remotes.get(r)).map(String::as_str);
            RepoRow { day, repo, tokens: v.tokens, remote, io: Some(v.io).filter(|io| !io.is_zero()) }
        })
        .collect();
    (days, repos)
}

struct Ordered<'a>(&'a [(&'a str, i64)]);

impl Serialize for Ordered<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

#[derive(Serialize)]
struct Summary<'a> {
    machine: &'a str,
    #[serde(rename = "scanMs")]
    scan_ms: i64,
    files: usize,
    tokens: i64,
    #[serde(rename = "dayRows")]
    day_rows: usize,
    repos: Ordered<'a>,
    sessions: usize,
}

/// The JS `--dry` output: what a tick found, top 15 repositories by tokens.
pub fn dry_summary(state: &State, body: &Body, scan_ms: i64) -> String {
    let mut by_repo: Vec<(&str, i64)> = Vec::new();
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for r in &body.repos {
        let name = r.repo.unwrap_or("undefined");
        match idx.get(name) {
            Some(&i) => by_repo[i].1 += r.tokens,
            None => {
                idx.insert(name, by_repo.len());
                by_repo.push((name, r.tokens));
            }
        }
    }
    by_repo.sort_by_key(|r| std::cmp::Reverse(r.1));
    by_repo.truncate(15);
    let s = Summary {
        machine: body.machine,
        scan_ms,
        files: state.files.len(),
        tokens: state.days.values().map(|v| v.tokens).sum(),
        day_rows: body.days.len(),
        repos: Ordered(&by_repo),
        sessions: body.sessions.len(),
    };
    serde_json::to_string_pretty(&s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Agent, FileState, Totals};

    #[test]
    fn session_rules() {
        let mut s = State::new("Asia/Tokyo");
        let now = 1_800_000_000_000.0;
        let mut add = |path: &str, agent: Agent, f: &dyn Fn(&mut FileState)| {
            let mut fs = FileState::new(agent, None);
            fs.mtime = now - 1000.4;
            f(&mut fs);
            s.files.insert(path.to_owned(), fs);
        };
        add("/h/.claude/projects/-h-Dev-chum/abc.jsonl", Agent::Claude, &|f| {
            f.cwd = Some("/h/Dev/chum".into());
            f.state = Some("run".into());
            f.state_at = Some(now - 5.0);
            f.tokens = 9;
        });
        add("/h/.claude/projects/-h-Dev-chum/abc/subagents/agent-1.jsonl", Agent::Claude, &|_| {});
        add("/h/.claude/projects/-h-Dev-chum/old.jsonl", Agent::Claude, &|f| f.mtime = now - OPEN_WINDOW - 1.0);
        add("/h/.claude/projects/-tmp-x/t.jsonl", Agent::Claude, &|f| f.cwd = Some("/private/tmp/x".into()));
        add(
            "/h/.codex/sessions/2026/10/12/rollout-2026-10-12T17-43-27-01a12f47-f5c2-70d1-93db-6ec3c8e36497.jsonl",
            Agent::Codex,
            &|_| {},
        );
        add(
            "/h/.codex/sessions/2026/10/12/rollout-2026-10-12T17-50-00-01a12f47-0000-70d1-93db-6ec3c8e36497.jsonl",
            Agent::Codex,
            &|f| {
                f.sub = true;
                f.parent = Some("01a12f47-f5c2-70d1-93db-6ec3c8e36497".into());
                f.cwd = Some("/h/.t3/worktrees/space-rock-ios/b".into());
            },
        );
        add("/h/.grok/sessions/%2Fh%2FDev%2Fpark/01a0/updates.jsonl", Agent::Grok, &|f| {
            f.cwd = Some("/h/Dev/park".into())
        });
        let out = sessions(&s, now, "/h", None, &HashMap::new());
        assert_eq!(out.len(), 5);
        let by_id: HashMap<&str, &Session> = out.iter().map(|x| (x.id.as_str(), x)).collect();
        let main = by_id["abc"];
        assert_eq!((main.kind, main.project.as_str(), main.repo.as_deref()), ("main", "chum", Some("chum")));
        assert_eq!(main.last_activity, (now - 1000.0) as i64);
        let sub = by_id["agent-1"];
        assert_eq!(
            (sub.kind, sub.parent.as_deref(), sub.project.as_str(), sub.cwd.as_deref()),
            ("subagent", Some("abc"), "-h-Dev-chum", None)
        );
        let csub = by_id["rollout-2026-10-12T17-50-00-01a12f47-0000-70d1-93db-6ec3c8e36497"];
        assert_eq!(csub.parent.as_deref(), Some("rollout-2026-10-12T17-43-27-01a12f47-f5c2-70d1-93db-6ec3c8e36497"));
        assert_eq!(csub.repo.as_deref(), Some("space-rock-ios"));
        let codex_main = by_id["rollout-2026-10-12T17-43-27-01a12f47-f5c2-70d1-93db-6ec3c8e36497"];
        assert_eq!(codex_main.project, "12");
        let g = by_id["01a0"];
        assert_eq!((g.agent, g.project.as_str()), ("grok", "park"));
        let json = serde_json::to_string(main).unwrap();
        assert!(json.contains(r#""stateAt":1799999999995"#), "{json}");
        assert!(json.contains(r#""io":null"#));
    }

    #[test]
    fn full_sync_is_bounded_but_keeps_dirty_rows() {
        let mut s = State::new("Asia/Tokyo");
        for d in ["2024-01-01", "2025-06-01", "2026-10-01"] {
            s.days.insert(format!("{d}|claude|m"), Totals { tokens: 1, output: 0 });
            s.repos.insert(format!("{d}|chum"), crate::state::RepoTotals { tokens: 1, ..Default::default() });
        }
        s.remotes.insert("chum".into(), "github.com/o/chum".into());
        let mut dirty = Dirty::default();
        dirty.days.insert("2024-01-01|claude|m");
        let (days, repos) = rows(&s, true, "2025-01-01", &dirty);
        assert_eq!(days.iter().map(|r| r.day).collect::<Vec<_>>(), ["2024-01-01", "2025-06-01", "2026-10-01"]);
        assert_eq!(repos.len(), 2);
        assert_eq!(
            serde_json::to_string(&repos[0]).unwrap(),
            r#"{"day":"2025-06-01","repo":"chum","tokens":1,"remote":"github.com/o/chum"}"#
        );
        let (days, repos) = rows(&s, false, "2025-01-01", &dirty);
        assert_eq!(days.len(), 1);
        assert!(repos.is_empty());
        assert_eq!(
            serde_json::to_string(&days[0]).unwrap(),
            r#"{"day":"2024-01-01","agent":"claude","model":"m","tokens":1,"output":0}"#
        );
    }
}
