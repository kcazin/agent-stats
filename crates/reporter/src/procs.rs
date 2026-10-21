//! The Claude Code processes running on this machine, to tell a session whose process is gone (killed, quit) from
//! one that is only quiet (a long tool call writes nothing for minutes).
//!
//! A process says which session it writes with `--session-id <id>` (how T3 Code and the SDK start each thread) or
//! `--resume <id>`. One that names none (`claude` in a terminal, `--continue`), resumes (the transcript may go on
//! under another id) or forks may write any session of its folder: Claude Code keeps a folder's transcripts in
//! `projects/<the cwd with every character but a letter or digit a dash>`, so that is what it claims.

use std::collections::HashSet;

#[derive(Default, Debug)]
pub struct Procs {
    /// Session ids the processes name.
    ids: HashSet<String>,
    /// Transcript folder names of the processes that may write a session they don't name.
    dirs: Vec<String>,
}

/// Claude Code's folder name for a cwd: every UTF-16 unit but an ASCII letter or digit becomes `-`.
pub fn folder_of(cwd: &str) -> String {
    let mut s = String::with_capacity(cwd.len());
    for c in cwd.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                s.push('-');
            }
        }
    }
    s
}

/// Claude Code cuts a folder name past this many characters and adds a hash.
const FOLDER_MAX: usize = 200;

fn same_folder(a: &str, b: &str) -> bool {
    a == b || (a.len() > FOLDER_MAX && b.len() > FOLDER_MAX && a.as_bytes()[..FOLDER_MAX] == b.as_bytes()[..FOLDER_MAX])
}

fn base(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}

/// Whether a command line runs Claude Code: the `claude` executable, or a JS runtime running its CLI.
pub fn is_claude(args: &[String]) -> bool {
    let cli = |s: &str| base(s) == "claude" || s.contains("@anthropic-ai/claude-code");
    match args {
        [] => false,
        [a, ..] if cli(a) => true,
        [a, b, ..] => matches!(base(a), "node" | "bun" | "deno") && cli(b),
        _ => false,
    }
}

/// The session a Claude Code command line names, and whether it may write others in its folder.
pub fn claims(args: &[String]) -> (Option<String>, bool) {
    let (mut id, mut loose) = (None, false);
    // The value after a flag, unless it is the next flag.
    let value = |i: usize| args.get(i + 1).filter(|v| !v.starts_with('-')).cloned();
    for (i, a) in args.iter().enumerate().skip(1) {
        if let Some(v) = a.strip_prefix("--session-id=") {
            id = Some(v.to_owned());
        } else if a == "--session-id" {
            id = value(i);
        } else if let Some(v) = a.strip_prefix("--resume=") {
            id = Some(v.to_owned());
            loose = true;
        } else if a == "--resume" || a == "-r" {
            id = value(i).or(id);
            loose = true;
        } else if a == "--continue" || a == "-c" || a == "--fork-session" {
            loose = true;
        }
    }
    let loose = loose || id.is_none();
    (id.filter(|s| !s.is_empty()), loose)
}

impl Procs {
    /// From each process's command line and, asked only for those that need it, its cwd.
    pub fn from(list: impl IntoIterator<Item = Vec<String>>, mut cwd_of: impl FnMut(usize) -> Option<String>) -> Procs {
        let mut p = Procs::default();
        for (i, args) in list.into_iter().enumerate() {
            if !is_claude(&args) {
                continue;
            }
            let (id, loose) = claims(&args);
            if let Some(id) = id {
                p.ids.insert(id);
            }
            if loose {
                if let Some(cwd) = cwd_of(i) {
                    p.dirs.push(folder_of(&cwd));
                }
            }
        }
        p
    }

    /// Whether a running process may be writing session `id`, kept in the transcript folder `folder`.
    pub fn owns(&self, id: &str, folder: &str) -> bool {
        self.ids.contains(id) || self.dirs.iter().any(|d| same_folder(d, folder))
    }

    /// This machine's processes; `None` where they can't be listed.
    pub fn scan() -> Option<Procs> {
        let pids = os::pids()?;
        let mut kept: Vec<i32> = Vec::new();
        let mut args: Vec<Vec<String>> = Vec::new();
        for pid in pids {
            if !os::may_be_claude(pid) {
                continue;
            }
            if let Some(a) = os::args(pid) {
                kept.push(pid);
                args.push(a);
            }
        }
        Some(Procs::from(args, |i| os::cwd(kept[i])))
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{c_int, c_void, CStr};

    pub fn pids() -> Option<Vec<i32>> {
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return None;
        }
        // Room for processes started since.
        let mut v = vec![0 as libc::pid_t; n as usize + 64];
        let bytes = (v.len() * std::mem::size_of::<libc::pid_t>()) as c_int;
        let n = unsafe { libc::proc_listallpids(v.as_mut_ptr() as *mut c_void, bytes) };
        if n <= 0 {
            return None;
        }
        v.truncate(n as usize);
        v.retain(|&p| p > 0);
        Some(v)
    }

    /// The kernel's name for the process: the executable's file name. Claude Code's native build is a file named
    /// for its version (`2.1.305`), run as `claude`; the npm one runs in node or bun.
    pub fn may_be_claude(pid: i32) -> bool {
        let mut buf = [0u8; 64];
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        if n <= 0 {
            return false;
        }
        let name = &buf[..n as usize];
        name == b"claude"
            || name.starts_with(b"node")
            || name == b"bun"
            || name == b"deno"
            || (name.first().is_some_and(u8::is_ascii_digit) && name.iter().all(|c| c.is_ascii_digit() || *c == b'.'))
    }

    /// `KERN_PROCARGS2`: argc, the executable path, padding, then argv (and the environment, not read).
    pub fn args(pid: i32) -> Option<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
        if r != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let r = unsafe {
            libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut c_void, &mut size, std::ptr::null_mut(), 0)
        };
        if r != 0 || size < 4 {
            return None;
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?).max(0) as usize;
        let mut rest = &buf[4..];
        // The executable path, then NULs up to argv[0].
        let end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[end..];
        let start = rest.iter().position(|&b| b != 0)?;
        rest = &rest[start..];
        let mut out = Vec::with_capacity(argc);
        for part in rest.split(|&b| b == 0).take(argc) {
            out.push(String::from_utf8_lossy(part).into_owned());
        }
        Some(out)
    }

    pub fn cwd(pid: i32) -> Option<String> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as c_int;
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut c_void, size)
        };
        if n != size {
            return None;
        }
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) };
        Some(path.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;

    pub fn pids() -> Option<Vec<i32>> {
        let rd = fs::read_dir("/proc").ok()?;
        Some(rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect())
    }

    pub fn may_be_claude(_pid: i32) -> bool {
        true
    }

    pub fn args(pid: i32) -> Option<Vec<String>> {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
        if raw.is_empty() {
            return None;
        }
        Some(raw.split(|&b| b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
    }

    pub fn cwd(pid: i32) -> Option<String> {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()?.to_str().map(str::to_owned)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod os {
    pub fn pids() -> Option<Vec<i32>> {
        None
    }
    pub fn may_be_claude(_pid: i32) -> bool {
        false
    }
    pub fn args(_pid: i32) -> Option<Vec<String>> {
        None
    }
    pub fn cwd(_pid: i32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn claude_command_lines() {
        assert!(is_claude(&v("claude --output-format stream-json")));
        assert!(is_claude(&v("/Users/me/.local/bin/claude")));
        assert!(is_claude(&v("node /opt/homebrew/bin/claude -c")));
        assert!(is_claude(&v("node /x/node_modules/@anthropic-ai/claude-code/cli.js --resume abc")));
        assert!(!is_claude(&v("/Applications/Claude.app/Contents/MacOS/Claude")));
        assert!(!is_claude(&v("node server.js claude")));
        assert!(!is_claude(&v("vim claude")));
    }

    #[test]
    fn what_a_process_claims() {
        let c = |s: &str| claims(&v(s));
        assert_eq!(c("claude --verbose --session-id=abc --model x"), (Some("abc".into()), false));
        assert_eq!(c("claude --session-id abc"), (Some("abc".into()), false));
        assert_eq!(c("claude --resume abc"), (Some("abc".into()), true));
        assert_eq!(c("claude -r abc --fork-session"), (Some("abc".into()), true));
        assert_eq!(c("claude --resume --verbose"), (None, true));
        assert_eq!(c("claude -c"), (None, true));
        assert_eq!(c("claude"), (None, true));
    }

    #[test]
    fn folders_and_ownership() {
        assert_eq!(
            folder_of("/Users/me/.t3/worktrees/llm-widget/t3-64eaf8b5"),
            "-Users-me--t3-worktrees-llm-widget-t3-64eaf8b5"
        );
        assert_eq!(folder_of("/a/é/😀"), "-a-----");
        let list = [v("claude --session-id=s1"), v("claude"), v("bash -c claude"), v("claude -c")];
        let mut asked = Vec::new();
        let p = Procs::from(list, |i| {
            asked.push(i);
            Some(if i == 1 { "/srv/one" } else { "/srv/two" }.to_owned())
        });
        assert_eq!(asked, [1, 3], "only the processes that claim a folder are asked their cwd");
        assert!(p.owns("s1", "-elsewhere"));
        assert!(p.owns("other", "-srv-one"), "a process without an id claims its folder");
        assert!(p.owns("other", "-srv-two"));
        assert!(!p.owns("other", "-srv-three"));
        assert!(!Procs::from([v("claude --session-id=s1")], |_| None).owns("s2", "-srv-one"));
        let long = "x".repeat(250);
        assert!(same_folder(&long, &format!("{}-h4sh", &long[..240])));
    }
}
