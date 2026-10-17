//! Repository identity from git: a working directory's repository root, its `origin` remote, normalized
//! (`git@github.com:o/r.git` → `github.com/o/r`). The remote is the repo's identity on new hubs; its last segment
//! is the `repo` name. Everything is cached in memory and re-read at most every 10 minutes.
//!
//! Remotes and cwds are private: they go to the hub and nowhere else.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Re-read a repository's config (and re-resolve a cwd's root) at most this often.
const TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    /// Last segment of the remote (`neon` for `github.com/kcazin/neon`).
    pub name: String,
    /// The normalized remote (`github.com/kcazin/neon`).
    pub remote: String,
}

#[derive(Default)]
struct Inner {
    /// cwd → repository root and its config file (None: not in a repository).
    roots: HashMap<String, (Option<(PathBuf, PathBuf)>, Instant)>,
    /// root → its `origin` remote, normalized.
    remotes: HashMap<PathBuf, (Option<String>, Instant)>,
    /// repo name → remote, for every repository counted since the last `take_seen`.
    seen: BTreeMap<String, String>,
}

/// Thread-safe cache of cwd → repository identity.
pub struct GitIds {
    /// Directories at or above this one are never taken as a repository root (a dotfiles repo in `$HOME` would
    /// otherwise claim every cwd under it).
    home: PathBuf,
    inner: Mutex<Inner>,
}

impl GitIds {
    pub fn new(home: &Path) -> GitIds {
        GitIds { home: home.to_path_buf(), inner: Mutex::default() }
    }

    /// The repository identity of `cwd`, when it is inside a git repository with a usable `origin` remote.
    pub fn lookup(&self, cwd: &str) -> Option<RepoId> {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (root, config) = match g.roots.get(cwd) {
            Some((r, at)) if now.duration_since(*at) < TTL => r.clone(),
            _ => {
                let r = find_root(Path::new(cwd), &self.home);
                g.roots.insert(cwd.to_owned(), (r.clone(), now));
                r
            }
        }?;
        let remote = match g.remotes.get(&root) {
            Some((r, at)) if now.duration_since(*at) < TTL => r.clone(),
            _ => {
                let r = fs::read_to_string(config).ok().and_then(|c| origin_url(&c)).and_then(|u| normalize_remote(&u));
                g.remotes.insert(root.clone(), (r.clone(), now));
                r
            }
        }?;
        let name = remote.rsplit('/').next().unwrap_or(&remote).to_owned();
        if g.seen.get(&name) != Some(&remote) {
            g.seen.insert(name.clone(), remote.clone());
        }
        Some(RepoId { name, remote })
    }

    /// Repo name → remote for every repository looked up since the last call.
    pub fn take_seen(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()).seen)
    }
}

/// Walks up from `cwd` to the directory holding `.git`. Returns the root and the config file of the repository
/// (for a worktree or submodule: the `gitdir:` the `.git` file points at, then its `commondir`).
pub fn find_root(cwd: &Path, home: &Path) -> Option<(PathBuf, PathBuf)> {
    let under_home = cwd.starts_with(home) && cwd != home;
    for dir in cwd.ancestors() {
        if (under_home && !dir.starts_with(home)) || dir == home {
            return None;
        }
        let dotgit = dir.join(".git");
        let Ok(md) = fs::metadata(&dotgit) else { continue };
        let gitdir = if md.is_dir() {
            dotgit
        } else {
            let text = fs::read_to_string(&dotgit).ok()?;
            let target = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))?.trim();
            dir.join(target)
        };
        let common = match fs::read_to_string(gitdir.join("commondir")) {
            Ok(c) if !c.trim().is_empty() => gitdir.join(c.trim()),
            _ => gitdir,
        };
        return Some((dir.to_path_buf(), common.join("config")));
    }
    None
}

/// `remote.origin.url` from a git config file's text.
pub fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    let mut url = None;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let header = rest.split(']').next().unwrap_or("").trim();
            // `[remote "origin"]`: the section name is case-insensitive, the subsection is not.
            in_origin = match header.split_once(char::is_whitespace) {
                Some((sec, sub)) => sec.eq_ignore_ascii_case("remote") && sub.trim() == "\"origin\"",
                None => header.eq_ignore_ascii_case("remote.origin"),
            };
            continue;
        }
        if !in_origin {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim().eq_ignore_ascii_case("url") {
            // The last one wins, as in git.
            url = Some(config_value(v));
        }
    }
    url.filter(|u| !u.is_empty())
}

/// A git config value as git reads it: `;` and `#` outside double quotes start a comment, quotes are removed,
/// `\"`, `\\`, `\t` and `\n` are escapes, and unquoted whitespace at the ends is dropped.
pub fn config_value(raw: &str) -> String {
    let mut out = String::new();
    // Length of `out` up to the last character that was quoted or not whitespace.
    let mut keep = 0;
    let (mut quoted, mut chars) = (false, raw.trim_start().chars());
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                keep = out.len();
            }
            ';' | '#' if !quoted => break,
            '\\' => {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('b') => {
                        out.pop();
                    }
                    Some(c) => out.push(c),
                    None => {}
                }
                keep = out.len();
            }
            c => {
                out.push(c);
                if quoted || !c.is_whitespace() {
                    keep = out.len();
                }
            }
        }
    }
    out.truncate(keep);
    out
}

/// `git@github.com:o/r.git`, `https://user:pw@github.com/o/r`, `ssh://git@github.com:22/o/r.git` →
/// `github.com/o/r`. Credentials, ports, query strings and fragments (`?token=…`) are dropped, the host is
/// lowercased. Local paths (`/srv/git/r.git`,
/// `file://…`) have no stable identity across machines and give `None`.
pub fn normalize_remote(url: &str) -> Option<String> {
    // Nothing after `?` or `#` is part of the repository's identity, and it may be a secret.
    let url = url.split(['?', '#']).next().unwrap_or("").trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        (authority, path)
    } else {
        // scp-like `[user@]host:path`; a colon after the first slash means it is a local path.
        let (host, path) = url.split_once(':')?;
        if host.contains('/') || host.is_empty() {
            return None;
        }
        (host, path)
    };
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        // `[::1]:22` or `host:22`; leave a bare IPv6 literal alone.
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) && !h.ends_with(':') => h,
        _ => host,
    };
    let host = host.to_ascii_lowercase();
    let path = path.trim_start_matches('~').trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remotes() {
        for (u, want) in [
            ("git@github.com:o/r.git", Some("github.com/o/r")),
            ("https://github.com/o/r", Some("github.com/o/r")),
            ("https://github.com/o/r/", Some("github.com/o/r")),
            ("https://user:secret@GitHub.com/o/r.git", Some("github.com/o/r")),
            ("ssh://git@github.com:22/o/r.git", Some("github.com/o/r")),
            ("ssh://git@host:2222/~me/r.git", Some("host/me/r")),
            ("git://example.org/a/b/c.git", Some("example.org/a/b/c")),
            ("gh-work:team/app.git", Some("gh-work/team/app")),
            ("https://github.com/o/r.git?token=SECRET", Some("github.com/o/r")),
            ("https://github.com/o/r#readme", Some("github.com/o/r")),
            ("https://github.com/o/r/?x=1#y", Some("github.com/o/r")),
            ("git@github.com:o/r.git?ref=main", Some("github.com/o/r")),
            ("/srv/git/r.git", None),
            ("file:///srv/git/r.git", None),
            ("./relative/r", None),
            ("", None),
        ] {
            assert_eq!(normalize_remote(u).as_deref(), want, "{u}");
        }
    }

    #[test]
    fn reads_origin_url() {
        let cfg = r#"
[core]
	bare = false
[remote "upstream"]
	url = git@github.com:up/r.git
[Remote "origin"]
	url = "git@github.com:me/r.git"
	fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
	remote = origin
"#;
        assert_eq!(origin_url(cfg).as_deref(), Some("git@github.com:me/r.git"));
        assert_eq!(origin_url("[remote \"upstream\"]\nurl = x\n"), None);
        // Inline comments are not part of the value; quoted `#` and `;` are.
        for (line, want) in [
            ("url = https://github.com/o/r.git ; mirror", "https://github.com/o/r.git"),
            ("url = https://github.com/o/r.git # comment", "https://github.com/o/r.git"),
            ("url = https://github.com/o/r.git#frag", "https://github.com/o/r.git"),
            ("url = \"https://h/o/r;x#y\" # c", "https://h/o/r;x#y"),
            ("url = \"a b \" ", "a b "),
            ("url = a\\\"b", "a\"b"),
        ] {
            assert_eq!(origin_url(&format!("[remote \"origin\"]\n{line}\n")).as_deref(), Some(want), "{line}");
        }
        assert_eq!(
            origin_url("[remote \"origin\"]\nurl = https://github.com/o/r.git?token=SECRET ; old\n")
                .and_then(|u| normalize_remote(&u))
                .as_deref(),
            Some("github.com/o/r")
        );
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn plain_repo_worktree_and_home_boundary() {
        let d = crate::ingest::tests::tmpdir("git");
        let home = d.join("home");
        // A plain repository, looked up from a subdirectory.
        write(&home.join("Dev/neon/.git/config"), "[remote \"origin\"]\n\turl = git@github.com:kcazin/neon.git\n");
        fs::create_dir_all(home.join("Dev/neon/src/deep")).unwrap();
        // A linked worktree of it: `.git` file → gitdir → commondir.
        write(&home.join("Dev/neon/.git/worktrees/wt1/commondir"), "../..\n");
        write(
            &home.join(".t3/worktrees/neon-ios/wt1/.git"),
            &format!("gitdir: {}\n", home.join("Dev/neon/.git/worktrees/wt1").display()),
        );
        // A relative gitdir (submodule style), no commondir.
        write(&home.join("Dev/app/.git/modules/lib/config"), "[remote \"origin\"]\nurl = https://gitlab.com/x/lib\n");
        write(&home.join("Dev/app/lib/.git"), "gitdir: ../.git/modules/lib\n");
        // No remote.
        write(&home.join("Dev/local/.git/config"), "[core]\n");
        // A dotfiles repo in $HOME must not claim everything below it.
        write(&home.join(".git/config"), "[remote \"origin\"]\nurl = git@github.com:me/dotfiles.git\n");
        fs::create_dir_all(home.join("notes")).unwrap();

        let ids = GitIds::new(&home);
        let s = |p: &str| home.join(p).to_string_lossy().into_owned();
        let neon = Some(RepoId { name: "neon".into(), remote: "github.com/kcazin/neon".into() });
        assert_eq!(ids.lookup(&s("Dev/neon/src/deep")), neon);
        assert_eq!(ids.lookup(&s("Dev/neon")), neon);
        assert_eq!(ids.lookup(&s(".t3/worktrees/neon-ios/wt1")), neon);
        assert_eq!(ids.lookup(&s("Dev/app/lib")).unwrap().remote, "gitlab.com/x/lib");
        assert_eq!(ids.lookup(&s("Dev/local")), None);
        assert_eq!(ids.lookup(&s("notes")), None);
        assert_eq!(ids.lookup(&home.to_string_lossy()), None);
        // A cwd that no longer exists (a deleted worktree) under a live repository still resolves.
        assert_eq!(ids.lookup(&s("Dev/neon/gone/away")), neon);
        assert_eq!(ids.lookup(&s("Dev/missing")), None);
        let seen = ids.take_seen();
        assert_eq!(seen.get("neon").map(String::as_str), Some("github.com/kcazin/neon"));
        assert_eq!(seen.get("lib").map(String::as_str), Some("gitlab.com/x/lib"));
        assert!(ids.take_seen().is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
