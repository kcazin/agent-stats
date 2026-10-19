//! Checks a `POST /report` body before anything touches the database. The JS hub wrote whatever it was sent; here
//! identity fields (machine, days, repos, session ids, token counts) must be well-formed or the report is refused
//! with 422, and descriptive fields (label, model, cwd, state…) are capped or nulled, so a bad report can't poison
//! the database or the public payload. Days must be real dates near the hub's today, and free text loses control
//! and bidi characters, since it reaches the public payload and the admin's terminal.

use crate::json::{Obj, J};
use jiff::civil::Date;

pub const SUPPORTED_PROTOCOLS: [u32; 2] = [1, 2];
pub const AGENTS: [&str; 3] = ["claude", "codex", "grok"];
pub const KINDS: [&str; 2] = ["main", "subagent"];
pub const STATES: [&str; 8] = ["thinking", "writing", "run", "edit", "read", "delegate", "waiting", "tool"];
/// Per row and day; real data peaks around 4e9, and a hub-wide total must stay far below i64.
const MAX_TOKENS: f64 = 1e12;
/// Timestamps in ms.
const MAX_TIME: f64 = 1e15;
/// Days accepted: this far back from the hub's today (older rows of an honest reporter are skipped) …
const DAYS_BACK: i64 = 401;
const MAX_ROWS: usize = 100_000;
const MAX_SESSIONS: usize = 2_000;

#[derive(Debug)]
pub struct DayRow {
    pub day: String,
    pub agent: String,
    pub model: String,
    pub tokens: i64,
    pub output: i64,
}

#[derive(Debug)]
pub struct RepoRow {
    pub day: String,
    pub repo: String,
    pub tokens: i64,
    pub remote: Option<String>,
    /// The tokens by kind: fresh input, cache reads, cache writes, output (reporters 0.7 on).
    pub io: Option<[i64; 4]>,
}

#[derive(Debug)]
pub struct Report {
    pub machine: String,
    pub label: String,
    pub platform: String,
    pub protocol: u32,
    pub version: Option<String>,
    pub days: Vec<DayRow>,
    pub repos: Vec<RepoRow>,
    /// The sessions as stored (`machines.sessions`): only known fields, in the reporter's key order.
    pub sessions: Vec<J>,
    /// Day and repo rows dropped for being older than the window.
    pub too_old: usize,
}

/// The days a report may carry: real dates from `DAYS_BACK` days before the hub's today to one day after it.
#[derive(Clone, Copy, Debug)]
pub struct DayWindow {
    oldest: Date,
    newest: Date,
}

impl DayWindow {
    pub fn new(today: Date) -> DayWindow {
        let oldest = today.checked_sub(jiff::Span::new().days(DAYS_BACK)).unwrap_or(Date::MIN);
        DayWindow { oldest, newest: today.tomorrow().unwrap_or(today) }
    }
    /// From a `YYYY-MM-DD` today (`compute::today`).
    pub fn around(today: &str) -> Option<DayWindow> {
        today.parse().ok().map(DayWindow::new)
    }
}

#[derive(Debug, PartialEq)]
pub enum Reject {
    /// 422 with a reason.
    Invalid(String),
    /// 426: a protocol this hub doesn't speak.
    Protocol(f64),
}

fn bad<T>(msg: impl Into<String>) -> Result<T, Reject> {
    Err(Reject::Invalid(msg.into()))
}

fn cap(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Controls (C0, DEL, C1), line and paragraph separators and bidi controls: never wanted in a name or label.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `s` without its unsafe characters, trimmed and capped at `n` characters.
pub fn clean(s: &str, n: usize) -> String {
    let s: String = s.chars().filter(|c| !is_unsafe_char(*c)).collect();
    cap(s.trim(), n).trim().to_string()
}

/// A non-empty identifier without control or bidi characters, at most `n` characters.
fn ident(v: Option<&J>, what: &str, n: usize) -> Result<String, Reject> {
    match v {
        Some(J::Str(s)) if !s.is_empty() && s.chars().count() <= n && !s.chars().any(is_unsafe_char) => Ok(s.clone()),
        Some(J::Str(s)) if s.is_empty() => bad(format!("{what} is empty")),
        Some(J::Str(_)) => bad(format!("{what} is too long or has control characters")),
        None | Some(J::Null) => bad(format!("{what} is missing")),
        Some(_) => bad(format!("{what} is not a string")),
    }
}

/// A cleaned, capped descriptive string; anything else becomes `None`.
fn text(v: Option<&J>, n: usize) -> Option<String> {
    match v {
        Some(J::Str(s)) => Some(clean(s, n)),
        _ => None,
    }
}

/// Like `text` but not trimmed: a session's `parent` must still equal the parent's `id`.
fn reference(v: Option<&J>, n: usize) -> Option<String> {
    match v {
        Some(J::Str(s)) => Some(cap(&s.chars().filter(|c| !is_unsafe_char(*c)).collect::<String>(), n)),
        _ => None,
    }
}

/// A token count: a finite non-negative integer below 1e12. Missing or null is 0, as `Number(x) || 0` was.
fn count(v: Option<&J>, what: &str) -> Result<i64, Reject> {
    match v {
        None | Some(J::Null) => Ok(0),
        Some(J::Num(n)) if n.is_finite() && *n >= 0.0 && *n < MAX_TOKENS && n.fract() == 0.0 => Ok(*n as i64),
        Some(_) => bad(format!("{what} is not a non-negative integer below 1e12")),
    }
}

/// A timestamp in ms: finite, non-negative, below 1e15.
fn time(v: Option<&J>) -> Option<f64> {
    match v {
        Some(J::Num(n)) if n.is_finite() && *n >= 0.0 && *n < MAX_TIME => Some(*n),
        _ => None,
    }
}

pub fn is_day(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && matches!(s[5..7].parse::<u32>(), Ok(1..=12))
        && matches!(s[8..10].parse::<u32>(), Ok(1..=31))
}

/// A row's day: `Some` inside the window, `None` for a real date older than it (skipped), an error for anything that
/// isn't a real date or lies after the window.
fn day(v: Option<&J>, what: &str, w: &DayWindow) -> Result<Option<String>, Reject> {
    let Some(d) = v.and_then(J::as_str).filter(|d| is_day(d)) else {
        return bad(format!("{what} is not a YYYY-MM-DD day"));
    };
    let Ok(date) = d.parse::<Date>() else { return bad(format!("{what} {d} is not a real date")) };
    if date > w.newest {
        return bad(format!("{what} {d} is in the future"));
    }
    Ok((date >= w.oldest).then(|| d.to_string()))
}

fn list<'a>(r: &'a Obj, key: &str, max: usize) -> Result<&'a [J], Reject> {
    match r.get(key) {
        None | Some(J::Null) => Ok(&[]),
        Some(J::Arr(a)) if a.len() <= max => Ok(a),
        Some(J::Arr(_)) => bad(format!("{key} has too many entries")),
        Some(_) => bad(format!("{key} is not an array")),
    }
}

fn known(v: Option<&J>, set: &[&str]) -> J {
    match v.and_then(J::as_str) {
        Some(s) if set.contains(&s) => J::Str(s.to_string()),
        _ => J::Null,
    }
}

pub fn report(body: &J, window: &DayWindow) -> Result<Report, Reject> {
    let Some(r) = body.as_obj() else { return bad("body is not an object") };
    let protocol = match r.get("protocol") {
        None | Some(J::Null) => 1,
        Some(J::Num(n)) if n.fract() == 0.0 && *n >= 1.0 => {
            if *n > *SUPPORTED_PROTOCOLS.iter().max().unwrap() as f64 {
                return Err(Reject::Protocol(*n));
            }
            *n as u32
        }
        Some(_) => return bad("protocol is not a positive integer"),
    };
    let machine = ident(r.get("machine"), "machine", 128)?;
    if let Some(v) = r.get("machineId") {
        if !matches!(v, J::Null) {
            ident(Some(v), "machineId", 128)?;
        }
    }
    let label = text(r.get("label"), 128).filter(|s| !s.is_empty()).unwrap_or_else(|| machine.clone());
    let platform = text(r.get("platform"), 32).unwrap_or_default();
    let version = text(r.get("version"), 64);

    let mut too_old = 0;
    let mut days = Vec::new();
    for (i, d) in list(r, "days", MAX_ROWS)?.iter().enumerate() {
        let Some(d) = d.as_obj() else { return bad(format!("days[{i}] is not an object")) };
        let agent = match d.get("agent") {
            None | Some(J::Null) => "unknown".to_string(),
            v => ident(v, &format!("days[{i}].agent"), 32)?,
        };
        let model = match d.get("model") {
            Some(J::Str(s)) => Some(clean(s, 128)).filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".to_string()),
            None | Some(J::Null) => "unknown".to_string(),
            Some(_) => return bad(format!("days[{i}].model is not a string")),
        };
        let day = day(d.get("day"), &format!("days[{i}].day"), window)?;
        let tokens = count(d.get("tokens"), &format!("days[{i}].tokens"))?;
        let output = count(d.get("output"), &format!("days[{i}].output"))?;
        match day {
            Some(day) => days.push(DayRow { day, agent, model, tokens, output }),
            None => too_old += 1,
        }
    }

    let mut repos = Vec::new();
    for (i, d) in list(r, "repos", MAX_ROWS)?.iter().enumerate() {
        let Some(d) = d.as_obj() else { return bad(format!("repos[{i}] is not an object")) };
        let day = day(d.get("day"), &format!("repos[{i}].day"), window)?;
        let tokens = count(d.get("tokens"), &format!("repos[{i}].tokens"))?;
        // A row without a repository says nothing (and hub.mjs's NULL key would never upsert).
        if matches!(d.get("repo"), None | Some(J::Null)) {
            continue;
        }
        let repo = ident(d.get("repo"), &format!("repos[{i}].repo"), 256)?;
        let remote = match d.get("remote") {
            None | Some(J::Null) => None,
            v => Some(ident(v, &format!("repos[{i}].remote"), 512)?),
        };
        let io = match d.get("io") {
            Some(J::Obj(io)) => {
                let mut v = [0; 4];
                for (n, k) in ["in", "cr", "cw", "out"].into_iter().enumerate() {
                    v[n] = count(io.get(k), &format!("repos[{i}].io.{k}"))?;
                }
                Some(v)
            }
            _ => None,
        };
        match day {
            Some(day) => repos.push(RepoRow { day, repo, tokens, remote, io }),
            None => too_old += 1,
        }
    }

    let mut sessions = Vec::new();
    for (i, s) in list(r, "sessions", MAX_SESSIONS)?.iter().enumerate() {
        let Some(s) = s.as_obj() else { return bad(format!("sessions[{i}] is not an object")) };
        let id = ident(s.get("id"), &format!("sessions[{i}].id"), 256)?;
        let Some(last) = time(s.get("lastActivity")) else {
            return bad(format!("sessions[{i}].lastActivity is not a time in ms"));
        };
        let io = match s.get("io") {
            Some(J::Obj(io)) => {
                let mut o = Obj::new();
                for k in ["in", "cr", "cw", "out"] {
                    o.set(k, J::Num(count(io.get(k), &format!("sessions[{i}].io.{k}"))? as f64));
                }
                J::Obj(o)
            }
            _ => J::Null,
        };
        let mut o = Obj::new()
            .with("id", id)
            .with("agent", known(s.get("agent"), &AGENTS))
            .with("kind", known(s.get("kind"), &KINDS))
            .with("parent", reference(s.get("parent"), 256))
            .with("project", text(s.get("project"), 256))
            .with("cwd", text(s.get("cwd"), 1024))
            .with("repo", text(s.get("repo"), 256))
            .with("model", text(s.get("model"), 128))
            .with("lastActivity", J::Num(last))
            .with("tokens", J::Num(count(s.get("tokens"), &format!("sessions[{i}].tokens"))? as f64))
            .with("state", known(s.get("state"), &STATES))
            .with("stateAt", time(s.get("stateAt")))
            .with("io", io);
        if let Some(remote) = text(s.get("remote"), 512) {
            o.set("remote", J::Str(remote));
        }
        sessions.push(J::Obj(o));
    }
    Ok(Report { machine, label, platform, protocol, version, days, repos, sessions, too_old })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> DayWindow {
        DayWindow::around("2026-10-14").unwrap()
    }
    fn ok(s: &str) -> Report {
        report(&J::parse(s).unwrap(), &window()).unwrap()
    }
    fn err(s: &str) -> Reject {
        report(&J::parse(s).unwrap(), &window()).unwrap_err()
    }

    #[test]
    fn repo_rows_may_carry_their_split() {
        let r = ok(
            r#"{"machine":"m","repos":[{"day":"2026-10-12","repo":"a","tokens":10,"io":{"in":1,"cr":6,"cw":2,"out":1}},
            {"day":"2026-10-12","repo":"b","tokens":3}]}"#,
        );
        assert_eq!(r.repos[0].io, Some([1, 6, 2, 1]));
        assert_eq!(r.repos[1].io, None);
        assert!(matches!(
            err(
                r#"{"machine":"m","repos":[{"day":"2026-10-12","repo":"a","tokens":1,"io":{"in":-1,"cr":0,"cw":0,"out":0}}]}"#
            ),
            Reject::Invalid(_)
        ));
    }

    #[test]
    fn accepts_the_js_reporter_shape() {
        let r = ok(r#"{"machine":"m","label":"","platform":"linux","at":1,"full":true,
            "days":[{"day":"2026-10-12","agent":"claude","model":"","tokens":10,"output":2}],
            "repos":[{"day":"2026-10-12","repo":"neon","tokens":5},{"day":"2026-10-12","tokens":1}],
            "sessions":[{"id":"a","agent":"gemini","kind":"main","parent":null,"project":"p","cwd":"/x","repo":"neon",
              "model":"claude-opus-4-5","lastActivity":1791676860000,"tokens":3,"state":"bogus","stateAt":null,
              "io":{"in":1,"cr":2,"cw":3,"out":4,"extra":9},"junk":1}]}"#);
        assert_eq!((r.machine.as_str(), r.label.as_str(), r.protocol), ("m", "m", 1));
        assert_eq!(r.days[0].model, "unknown");
        assert_eq!(r.repos.len(), 1);
        let s = r.sessions[0].stringify();
        assert!(s.contains(r#""agent":null"#) && s.contains(r#""state":null"#) && !s.contains("junk"), "{s}");
        assert!(s.contains(r#""io":{"in":1,"cr":2,"cw":3,"out":4}"#), "{s}");
    }

    #[test]
    fn refuses_bad_reports() {
        assert_eq!(err(r#"{"machine":"m","protocol":3}"#), Reject::Protocol(3.0));
        assert!(matches!(err(r#"{"machine":""}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","days":[{"day":"2026-13-01","agent":"claude"}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","days":[{"day":"2026-10-01","tokens":-1}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","days":[{"day":"2026-10-01","tokens":1.5}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","days":[{"day":"2026-10-01","tokens":1e16}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","days":[{"day":"2026-10-01","tokens":"5"}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","sessions":[{"id":"x"}]}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"{"machine":"m","protocol":"2"}"#), Reject::Invalid(_)));
        assert!(matches!(err(r#"[]"#), Reject::Invalid(_)));
    }

    #[test]
    fn days_must_be_real_and_recent() {
        let one = |d: &str| format!(r#"{{"machine":"m","days":[{{"day":"{d}","agent":"claude","tokens":1}}]}}"#);
        assert_eq!(ok(&one("2026-10-15")).days.len(), 1, "tomorrow (a reporter ahead of the hub)");
        assert_eq!(ok(&one("2025-09-08")).days.len(), 1, "401 days back");
        for d in ["2026-10-16", "9999-12-31", "2026-02-31", "2026-04-31", "2025-02-29", "0000-00-00", "2026-1-01"] {
            assert!(matches!(err(&one(d)), Reject::Invalid(_)), "{d}");
        }
        // Older than the window: skipped, so an honest reporter's long history doesn't wedge its reports.
        let r = ok(&one("2025-09-07"));
        assert_eq!((r.days.len(), r.too_old), (0, 1));
        let r = ok(
            r#"{"machine":"m","repos":[{"day":"0001-01-01","repo":"x","tokens":1},{"day":"2026-10-14","repo":"y"}]}"#,
        );
        assert_eq!((r.repos.len(), r.too_old), (1, 1));
        assert!(matches!(err(r#"{"machine":"m","repos":[{"day":"2026-02-30","repo":"x"}]}"#), Reject::Invalid(_)));
    }

    #[test]
    fn token_counts_are_capped_at_1e12() {
        let one = |t: &str| format!(r#"{{"machine":"m","days":[{{"day":"2026-10-14","tokens":{t}}}]}}"#);
        assert_eq!(ok(&one("999999999999")).days[0].tokens, 999_999_999_999);
        assert!(matches!(err(&one("1000000000000")), Reject::Invalid(_)));
        assert!(matches!(err(&one("999999999999999")), Reject::Invalid(_)));
    }

    #[test]
    fn free_text_loses_control_and_bidi_characters() {
        let r = ok("{\"machine\":\"m\",\"label\":\"  La\\u001b[31mp\\u202e\\u2028 \\u0085top\\u007f \",\"platform\":\"li\\u0000nux\",
            \"days\":[{\"day\":\"2026-10-14\",\"agent\":\"claude\",\"model\":\"\\u202ex\\u200fy\"},
                      {\"day\":\"2026-10-14\",\"agent\":\"codex\",\"model\":\"\\u0007\"}],
            \"sessions\":[{\"id\":\"a\",\"lastActivity\":1,\"cwd\":\"/x\\u001b]0;pwn\\u0007\",\"model\":\"m\\u2066\\u2069\"}]}");
        assert_eq!((r.label.as_str(), r.platform.as_str()), ("La[31mp top", "linux"));
        assert_eq!((r.days[0].model.as_str(), r.days[1].model.as_str()), ("xy", "unknown"));
        let s = r.sessions[0].stringify();
        assert!(s.contains(r#""cwd":"/x]0;pwn""#) && s.contains(r#""model":"m""#), "{s}");
        // Identifiers are refused, not cleaned.
        assert!(matches!(err("{\"machine\":\"m\\u202e\"}"), Reject::Invalid(_)));
        assert!(matches!(
            err("{\"machine\":\"m\",\"repos\":[{\"day\":\"2026-10-14\",\"repo\":\"a\\u2029b\"}]}"),
            Reject::Invalid(_)
        ));
        // A label that is only controls falls back to the machine name.
        assert_eq!(ok("{\"machine\":\"m\",\"label\":\"\\u001b \\u200e\"}").label, "m");
        // The cap applies after cleaning.
        let long = format!("{{\"machine\":\"m\",\"label\":\"{}\"}}", "\\u001b".repeat(300) + &"x".repeat(200));
        assert_eq!(ok(&long).label.len(), 128);
    }
}
