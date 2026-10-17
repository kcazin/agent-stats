//! Finding transcripts: the same roots, order and filters as the JS walk, with directory listings cached by mtime.

use crate::config::Roots;
use crate::js::basename;
use crate::state::Agent;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// `stat().mtimeMs` as Node computes it (`sec * 1e3 + nsec / 1e6`), so it compares equal to migrated JS values.
pub fn mtime_ms(m: &Metadata) -> f64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.mtime() as f64 * 1e3 + m.mtime_nsec() as f64 / 1e6
    }
    #[cfg(not(unix))]
    {
        match m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) {
            Some(d) => d.as_secs() as f64 * 1e3 + d.subsec_nanos() as f64 / 1e6,
            None => 0.0,
        }
    }
}

pub fn inode(m: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        0
    }
}

pub struct Listed {
    pub agent: Agent,
    pub path: PathBuf,
    /// The path as the JS reporter keys it (lossy UTF-8).
    pub key: String,
}

type Entries = Arc<Vec<(OsString, bool, bool)>>;

struct DirCache {
    mtime: SystemTime,
    listed_at: SystemTime,
    entries: Entries,
}

/// A directory listing is reused while the directory's mtime is unchanged. Creating, deleting or renaming an
/// entry updates the mtime; appending to a file does not, so files themselves are still stat'ed. A listing taken
/// within 2 s of the mtime is not trusted, for file systems with coarse timestamps.
const DIR_TRUST: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Scanner {
    dirs: HashMap<PathBuf, DirCache>,
    /// Directories listed (rather than reused) during the last walk.
    pub listed: usize,
}

fn sort_key(n: &OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        n.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        n.to_string_lossy().as_bytes().to_vec()
    }
}

impl Scanner {
    /// Every transcript to track, in the JS reporter's order: Claude, Codex sessions, Codex archived sessions
    /// (a basename already seen is skipped), Grok (only `updates.jsonl`).
    pub fn list(&mut self, roots: &Roots) -> Vec<Listed> {
        let roots = [
            (Agent::Claude, roots.claude_projects.clone()),
            (Agent::Codex, roots.codex.join("sessions")),
            (Agent::Codex, roots.codex.join("archived_sessions")),
            (Agent::Grok, roots.grok.join("sessions")),
        ];
        self.listed = 0;
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        let mut codex_names = HashSet::new();
        for (agent, dir) in roots {
            let mut paths = Vec::new();
            self.walk(&dir, &mut paths, &mut visited);
            for path in paths {
                let key = path.to_string_lossy().into_owned();
                let name = basename(&key).to_owned();
                if agent == Agent::Codex && !codex_names.insert(name.clone()) {
                    continue;
                }
                if agent == Agent::Grok && name != "updates.jsonl" {
                    continue;
                }
                out.push(Listed { agent, path, key });
            }
        }
        self.dirs.retain(|d, _| visited.contains(d));
        out
    }

    fn walk(&mut self, dir: &Path, out: &mut Vec<PathBuf>, visited: &mut HashSet<PathBuf>) {
        let Ok(md) = fs::metadata(dir) else { return };
        if !md.is_dir() {
            return;
        }
        visited.insert(dir.to_path_buf());
        let mtime = md.modified().ok();
        let cached = match (self.dirs.get(dir), mtime) {
            (Some(c), Some(m)) if c.mtime == m && c.listed_at.duration_since(m).is_ok_and(|d| d > DIR_TRUST) => {
                Some(c.entries.clone())
            }
            _ => None,
        };
        let entries = match cached {
            Some(e) => e,
            None => {
                let listed_at = SystemTime::now();
                let Ok(rd) = fs::read_dir(dir) else { return };
                let mut v: Vec<(OsString, bool, bool)> = rd
                    .flatten()
                    .filter_map(|e| {
                        let ft = e.file_type().ok()?;
                        Some((e.file_name(), ft.is_dir(), ft.is_file()))
                    })
                    .collect();
                // Node's readdir returns names sorted by strcmp.
                v.sort_by_cached_key(|(n, _, _)| sort_key(n));
                let e: Entries = Arc::new(v);
                self.listed += 1;
                if let Some(m) = mtime {
                    self.dirs.insert(dir.to_path_buf(), DirCache { mtime: m, listed_at, entries: e.clone() });
                }
                e
            }
        };
        for (name, is_dir, is_file) in entries.iter() {
            let p = dir.join(name);
            if *is_dir {
                self.walk(&p, out, visited);
            } else if *is_file && name.to_string_lossy().ends_with(".jsonl") {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_order_and_filters() {
        let d = crate::ingest::tests::tmpdir("walk");
        let mk = |p: &str| {
            let p = d.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "").unwrap();
        };
        mk(".claude/projects/b/s.jsonl");
        mk(".claude/projects/a-b/s.jsonl");
        mk(".claude/projects/a/x.jsonl");
        mk(".claude/projects/a/x/subagents/agent-1.jsonl");
        mk(".claude/projects/a/notes.txt");
        mk(".codex/sessions/2026/10/12/rollout-1.jsonl");
        mk(".codex/archived_sessions/rollout-1.jsonl");
        mk(".codex/archived_sessions/rollout-2.jsonl");
        mk(".grok/sessions/%2Fhome/abc/updates.jsonl");
        mk(".grok/sessions/%2Fhome/abc/events.jsonl");
        let mut s = Scanner::default();
        let rel = |l: &Vec<Listed>| {
            l.iter().map(|x| x.path.strip_prefix(&d).unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>()
        };
        let roots = Roots::under(&d);
        let got = s.list(&roots);
        assert_eq!(
            rel(&got),
            [
                ".claude/projects/a/x/subagents/agent-1.jsonl",
                ".claude/projects/a/x.jsonl",
                ".claude/projects/a-b/s.jsonl",
                ".claude/projects/b/s.jsonl",
                ".codex/sessions/2026/10/12/rollout-1.jsonl",
                ".codex/archived_sessions/rollout-2.jsonl",
                ".grok/sessions/%2Fhome/abc/updates.jsonl",
            ]
        );
        assert!(s.listed > 0);
        // Unchanged directories are not listed again (once their mtime is old enough to trust).
        for c in s.dirs.values_mut() {
            c.listed_at += Duration::from_secs(10);
        }
        let again = s.list(&roots);
        assert_eq!(rel(&again), rel(&got));
        assert_eq!(s.listed, 0);
        // A new file changes its directory's mtime and is found.
        std::thread::sleep(Duration::from_millis(20));
        mk(".claude/projects/b/t.jsonl");
        let third = s.list(&roots);
        assert_eq!(third.len(), got.len() + 1);
        let _ = fs::remove_dir_all(&d);
    }
}
