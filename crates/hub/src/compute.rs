//! Everything hub.mjs computes from the database, ported line by line: live machines and sessions, today, the
//! 24-hour history and activity, totals since the first day, projects, the calendar, the pile, and the public
//! payload built from them. Same SQL, same row order, same float arithmetic, so the output matches byte for byte.

use crate::config::Config;
use crate::json::{js_number, js_round, Obj, J};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

pub const ONLINE: f64 = 45_000.0; // a machine is online if it reported this recently
pub const WORKING: f64 = 60_000.0; // a session is working if its transcript was written this recently
/// ...or this recently while its reporter sees its process run: a long tool call writes nothing for minutes.
pub const WORKING_ALIVE: f64 = 600_000.0;
const DAY_MS: f64 = 86_400_000.0;

type R<T> = rusqlite::Result<T>;

/// The calendar day of `ms` in the hub's time zone (`Intl.DateTimeFormat('en-CA', {timeZone})`).
pub fn day_of(cfg: &Config, ms: f64) -> String {
    let ts = jiff::Timestamp::from_millisecond(ms.floor() as i64).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    ts.to_zoned(cfg.tz.clone()).date().to_string()
}

pub fn today(cfg: &Config, now: f64) -> String {
    day_of(cfg, now)
}

pub fn days_ago(cfg: &Config, now: f64, n: f64) -> String {
    day_of(cfg, now - n * DAY_MS)
}

/// Midnight starting `day` in the hub's time zone, in ms (hub.mjs: `Date.parse(day + 'T00:00:00+09:00')`).
fn midnight(cfg: &Config, day: &str) -> f64 {
    day.parse::<jiff::civil::Date>()
        .ok()
        .and_then(|d| d.to_zoned(cfg.tz.clone()).ok())
        .map_or(f64::NAN, |z| z.timestamp().as_millisecond() as f64)
}

/// claude-opus-5-5 → Opus 5.5, gpt-6-astra → GPT-6 Astra, grok-4.6-build → Grok 4.6 Build (hub.mjs `modelName`).
pub fn model_name(id: Option<&J>) -> String {
    let id = match id {
        Some(J::Str(s)) if !s.is_empty() && s != "unknown" => s.as_str(),
        _ => return "Other".into(),
    };
    if let Some(n) = claude_family(id) {
        return n;
    }
    if let Some(n) = gpt(id) {
        return n;
    }
    // `get`, not indexing: byte 5 may fall inside a multi-byte character, and a panic here would abort the hub.
    if id.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("grok-")) {
        return id.split('-').map(cap_first).collect::<Vec<_>>().join(" ");
    }
    id.to_string()
}

/// `w[0].toUpperCase() + w.slice(1)` (an astral first character is a lone surrogate in JS, left as is).
fn cap_first(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) if f.len_utf16() == 1 => f.to_uppercase().chain(c).collect(),
        Some(f) => std::iter::once(f).chain(c).collect(),
        None => String::new(),
    }
}

/// `/(opus|sonnet|haiku|fable)-(\d+)(?:-(\d{1,2}))?(?!\d)/i`
fn claude_family(id: &str) -> Option<String> {
    let b = id.as_bytes();
    for start in 0..b.len() {
        for fam in ["opus", "sonnet", "haiku", "fable"] {
            let end = start + fam.len();
            if end >= b.len() || !b[start..end].eq_ignore_ascii_case(fam.as_bytes()) || b[end] != b'-' {
                continue;
            }
            let d0 = end + 1;
            let d1 = d0 + b[d0..].iter().take_while(|c| c.is_ascii_digit()).count();
            if d1 == d0 {
                continue;
            }
            let digit_at = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
            let mut minor = None;
            if b.get(d1) == Some(&b'-') {
                for len in [2, 1] {
                    let e = d1 + 1 + len;
                    if (d1 + 1..e).all(digit_at) && !digit_at(e) {
                        minor = Some(&id[d1 + 1..e]);
                        break;
                    }
                }
            }
            let word = &id[start..end];
            let mut out = format!("{}{} {}", word[..1].to_uppercase(), &word[1..], &id[d0..d1]);
            if let Some(m) = minor {
                out.push('.');
                out.push_str(m);
            }
            return Some(out);
        }
    }
    None
}

/// `/^gpt-?([\d.]+)(.*)$/i`, then each `-` word capitalised.
fn gpt(id: &str) -> Option<String> {
    if !id.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("gpt")) {
        return None;
    }
    let mut rest = &id[3..];
    if let Some(r) = rest.strip_prefix('-') {
        rest = r;
    }
    let n = rest.bytes().take_while(|c| c.is_ascii_digit() || *c == b'.').count();
    if n == 0 {
        return None;
    }
    let (ver, tail) = rest.split_at(n);
    // `.` does not match line terminators, and `$` needs the end of input.
    if tail.chars().any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')) {
        return None;
    }
    let words: String = tail.split('-').filter(|w| !w.is_empty()).map(|w| format!(" {}", cap_first(w))).collect();
    Some(format!("GPT-{ver}{words}"))
}

/// Sessions are public only under an opaque id, stable for a session.
pub fn public_id(machine: &str, id: &str) -> String {
    let h = Sha256::digest(format!("{machine}|{id}").as_bytes());
    h.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// `String(x)` for the values sessions carry.
fn js_string(v: Option<&J>) -> String {
    match v {
        None => "undefined".into(),
        Some(J::Null) => "null".into(),
        Some(J::Bool(b)) => b.to_string(),
        Some(J::Num(n)) if n.is_nan() => "NaN".into(),
        Some(J::Num(n)) if n.is_infinite() => if *n > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        Some(J::Num(n)) => js_number(*n),
        Some(J::Str(s)) => s.clone(),
        Some(J::Arr(_)) => String::new(),
        Some(J::Obj(_)) => "[object Object]".into(),
    }
}

/// `a === b` for primitives (undefined = `None`).
fn strict_eq(a: Option<&J>, b: Option<&J>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(J::Null), Some(J::Null)) => true,
        (Some(J::Bool(x)), Some(J::Bool(y))) => x == y,
        (Some(J::Num(x)), Some(J::Num(y))) => x == y,
        (Some(J::Str(x)), Some(J::Str(y))) => x == y,
        _ => false,
    }
}

/// A hashable stand-in for a primitive under `===` (`None` for values that never equal anything, like NaN and objects).
type StrictKey = (u8, String);

fn strict_key(v: Option<&J>) -> Option<StrictKey> {
    Some(match v {
        None => (0, String::new()),
        Some(J::Null) => (1, String::new()),
        Some(J::Bool(b)) => (2, b.to_string()),
        Some(J::Num(n)) if !n.is_nan() => (3, js_number(*n)),
        Some(J::Str(s)) => (4, s.clone()),
        _ => return None,
    })
}

/// `Math.max(a, b)` / `Math.min(a, b)`: NaN wins.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// A TEXT column in string concatenation (`null` for NULL, as node:sqlite returns it).
fn sql_str(v: Option<&str>) -> &str {
    v.unwrap_or("null")
}

fn truthy(v: Option<&J>) -> bool {
    v.is_some_and(J::truthy)
}

/// `v || null`
fn or_null(v: Option<&J>) -> J {
    v.filter(|v| v.truthy()).cloned().unwrap_or(J::Null)
}

pub struct Machine {
    pub machine: String,
    pub label: J,
    pub platform: J,
    pub last_seen: f64,
    pub online: bool,
    pub sessions: Vec<J>,
}

pub struct Live {
    pub machines: Vec<Machine>,
    /// Each `{...s, machine: label, working}`.
    pub sessions: Vec<Obj>,
    pub working: usize,
    pub subagents: usize,
    pub open: usize,
    pub by_agent: Obj,
    /// The first time a working session stops working by time alone (`working_until`), for the streams to say so.
    pub ages_out: Option<f64>,
}

fn working(s: &Obj) -> bool {
    s.get("working") == Some(&J::Bool(true))
}

/// Until when a session works, by its last write: `WORKING` after it, `WORKING_ALIVE` while its process runs. Not at
/// all once its turn ended (`idle`) or its process is gone.
fn working_until(s: &Obj) -> Option<f64> {
    let alive = s.get("alive").and_then(|v| if let J::Bool(b) = v { Some(*b) } else { None });
    if str_of(s, "state") == Some("idle") || alive == Some(false) {
        return None;
    }
    let last = s.get("lastActivity").and_then(J::as_f64)?;
    Some(last + if alive == Some(true) { WORKING_ALIVE } else { WORKING })
}

fn str_of<'a>(s: &'a Obj, k: &str) -> Option<&'a str> {
    s.get(k).and_then(J::as_str)
}

pub fn live(db: &Connection, now: f64) -> R<Live> {
    let mut st = db.prepare("SELECT machine, label, platform, last_seen, sessions FROM machines ORDER BY machine")?;
    let machines: Vec<Machine> = st
        .query_map([], |r| {
            let last_seen = crate::db::num(r, 3)?.unwrap_or(f64::NAN);
            let sessions: Option<String> = r.get(4)?;
            let sessions = match J::parse(sessions.as_deref().filter(|s| !s.is_empty()).unwrap_or("[]")) {
                Ok(J::Arr(a)) => a,
                _ => Vec::new(),
            };
            Ok(Machine {
                machine: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                label: r.get::<_, Option<String>>(1)?.into(),
                platform: r.get::<_, Option<String>>(2)?.into(),
                last_seen,
                online: now - last_seen < ONLINE,
                sessions,
            })
        })?
        .collect::<R<_>>()?;
    let mut sessions: Vec<Obj> = Vec::new();
    let mut ages_out: Option<f64> = None;
    for m in machines.iter().filter(|m| m.online) {
        for s in &m.sessions {
            let J::Obj(o) = s else { continue };
            let mut o = o.clone();
            let until = working_until(&o).filter(|&t| now < t);
            let w = until.is_some();
            if let Some(t) = until {
                ages_out = Some(ages_out.map_or(t, |a| a.min(t)));
            }
            o.set("machine", m.label.clone());
            o.set("working", J::Bool(w));
            sessions.push(o);
        }
    }
    let last = |s: &Obj| s.get("lastActivity").and_then(J::as_f64).unwrap_or(f64::NAN);
    sessions.sort_by(|a, b| (last(b) - last(a)).partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal));
    let kind_n = |k: &str, only_working: bool| {
        sessions.iter().filter(|s| (!only_working || working(s)) && str_of(s, "kind") == Some(k)).count()
    };
    let mut by_agent = Obj::new();
    for a in crate::validate::AGENTS {
        by_agent.set(a, J::from(sessions.iter().filter(|s| working(s) && str_of(s, "agent") == Some(a)).count()));
    }
    Ok(Live {
        working: kind_n("main", true),
        subagents: kind_n("subagent", true),
        open: kind_n("main", false),
        machines,
        sessions,
        by_agent,
        ages_out,
    })
}

/// `{name, tokens}` summed by key, biggest first (ties keep first-seen order, integer-like names first, as JS).
fn group(rows: &[UsageRow], key: impl Fn(&UsageRow) -> String) -> Vec<(String, f64)> {
    let mut out = Obj::new();
    for r in rows {
        let k = key(r);
        let prev = out.get(&k).and_then(J::as_f64).unwrap_or(0.0);
        out.set(&k, J::Num(prev + r.tokens));
    }
    let mut list: Vec<(String, f64)> = out.0.into_iter().map(|(k, v)| (k, v.as_f64().unwrap_or(0.0))).collect();
    list.sort_by(|a, b| (b.1 - a.1).partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal));
    list
}

fn top_n(list: Vec<(String, f64)>, n: usize) -> Vec<(String, f64)> {
    if list.len() <= n {
        return list;
    }
    let other = list[n - 1..].iter().fold(0.0, |a, b| a + b.1);
    let mut head: Vec<_> = list.into_iter().take(n - 1).collect();
    head.push(("Other".into(), other));
    head
}

fn named(list: Vec<(String, f64)>) -> J {
    J::Arr(list.into_iter().map(|(name, tokens)| Obj::new().with("name", name).with("tokens", tokens).into()).collect())
}

pub struct UsageRow {
    pub machine: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub tokens: f64,
    pub output: f64,
}

pub fn today_rows(db: &Connection, day: &str) -> R<Vec<UsageRow>> {
    let mut st = db.prepare("SELECT machine, agent, model, tokens, output FROM usage WHERE day = ?")?;
    let rows = st.query_map(params![day], |r| {
        Ok(UsageRow {
            machine: r.get(0)?,
            agent: r.get(1)?,
            model: r.get(2)?,
            tokens: crate::db::num(r, 3)?.unwrap_or(0.0),
            output: crate::db::num(r, 4)?.unwrap_or(0.0),
        })
    })?;
    rows.collect()
}

fn sum(rows: &[UsageRow], f: impl Fn(&UsageRow) -> f64) -> f64 {
    rows.iter().fold(0.0, |a, r| a + f(r))
}

fn model_of(r: &UsageRow) -> String {
    model_name(r.model.clone().map(J::Str).as_ref())
}

pub fn calendar(db: &Connection, from: &str) -> R<J> {
    let mut tokens = Obj::new();
    let mut st = db.prepare("SELECT day, total(tokens) AS t FROM usage WHERE day >= ? GROUP BY day")?;
    for r in st.query_map(params![from], |r| Ok((r.get::<_, String>(0)?, crate::db::num(r, 1)?)))? {
        let (d, t) = r?;
        tokens.set(&d, t.into());
    }
    let mut github = Obj::new();
    let mut st = db.prepare("SELECT day, count FROM github WHERE day >= ?")?;
    for r in st.query_map(params![from], |r| Ok((r.get::<_, String>(0)?, crate::db::num(r, 1)?)))? {
        let (d, c) = r?;
        github.set(&d, c.into());
    }
    Ok(Obj::new().with("tokens", tokens).with("github", github).into())
}

fn bucket_start(now: f64) -> (f64, f64, usize) {
    let bucket = 600_000.0;
    let start = ((now - DAY_MS) / bucket).floor() * bucket;
    (start, bucket, ((now - start) / bucket).ceil().max(0.0) as usize)
}

/// Peak agents working per 10-minute bucket, last 24 hours: `[(at, working)]`.
pub fn history(db: &Connection, now: f64) -> R<Vec<(f64, f64)>> {
    let (start, bucket, n) = bucket_start(now);
    let mut out: Vec<(f64, f64)> = (0..n).map(|i| (start + i as f64 * bucket, 0.0)).collect();
    let mut st = db.prepare("SELECT at, working FROM samples WHERE at >= ?")?;
    for r in st.query_map(params![start as i64], |r| Ok((crate::db::num(r, 0)?, crate::db::num(r, 1)?)))? {
        let (at, w) = r?;
        let (Some(at), w) = (at, w.unwrap_or(0.0)) else { continue };
        let i = ((at - start) / bucket).floor();
        if i >= 0.0 && (i as usize) < out.len() {
            let b = &mut out[i as usize];
            b.1 = js_max(b.1, w);
        }
    }
    Ok(out)
}

/// Agents working at once, by agent type: the busiest sample in each 10-minute bucket over the last 24 hours, null
/// where nothing was recorded.
pub fn activity(db: &Connection, now: f64) -> R<J> {
    let (start, bucket, n) = bucket_start(now);
    let mut out: Vec<J> = vec![J::Null; n];
    let mut st = db.prepare("SELECT at, working, subagents, by_agent FROM samples WHERE at >= ?")?;
    let rows = st.query_map(params![start as i64], |r| {
        Ok((crate::db::num(r, 0)?, crate::db::num(r, 1)?, crate::db::num(r, 2)?, r.get::<_, Option<String>>(3)?))
    })?;
    for r in rows {
        let (at, w, sub, by_agent) = r?;
        let Some(at) = at else { continue };
        let i = ((at - start) / bucket).floor();
        let total = w.unwrap_or(0.0) + sub.unwrap_or(0.0);
        // A sample past the last bucket (one taken exactly on a bucket boundary) extends the array, as assigning
        // past the end of a JS array does. (Not `!(0.0..10_000.0).contains(&i)`: that would also skip NaN.)
        #[allow(clippy::manual_range_contains)]
        if i < 0.0 || i >= 10_000.0 {
            continue;
        }
        let i = i as usize;
        if i >= out.len() {
            out.resize(i + 1, J::Null);
        }
        if let J::Obj(o) = &out[i] {
            if o.get("total").and_then(J::as_f64).is_some_and(|t| t >= total) {
                continue;
            }
        }
        let mut by = match by_agent.as_deref().map(J::parse) {
            Some(Ok(J::Obj(o))) => o,
            _ => Obj::new(),
        };
        if by_agent.as_deref().is_none_or(str::is_empty) && total != 0.0 && !total.is_nan() {
            by = Obj::new().with("claude", total); // samples from before by_agent existed
        }
        let mut o = Obj::new().with("total", total);
        for (k, v) in by.0 {
            o.set(&k, v);
        }
        out[i] = J::Obj(o);
    }
    Ok(Obj::new().with("start", start).with("bucket", bucket).with("points", out).into())
}

pub struct Since {
    pub day: Option<String>,
    pub tokens: f64,
    pub github: f64,
}

/// Headline totals count from the first recorded day; GitHub over the same days, so the two compare.
pub fn since(db: &Connection, cfg: &Config, now: f64) -> R<Since> {
    let (t, first): (Option<f64>, Option<String>) =
        db.query_row("SELECT total(tokens) AS t, min(day) AS first FROM usage", [], |r| {
            Ok((crate::db::num(r, 0)?, r.get(1)?))
        })?;
    let from = first.clone().unwrap_or_else(|| today(cfg, now));
    let n =
        db.query_row("SELECT sum(count) AS n FROM github WHERE day >= ?", params![from], |r| crate::db::num(r, 0))?;
    Ok(Since {
        day: first,
        tokens: t.filter(|t| *t != 0.0).unwrap_or(0.0),
        github: n.filter(|n| *n != 0.0).unwrap_or(0.0),
    })
}

/// Tokens per project, all time and today. Each machine's day total that its repositories don't account for is
/// split as `[unattributed]` says.
pub fn projects(db: &Connection, cfg: &Config, now: f64) -> R<(Obj, Obj)> {
    let keys = cfg.project_keys();
    let zero = || keys.iter().fold(Obj::new(), |o, k| o.with(k, 0.0));
    let (mut all, mut now_p) = (zero(), zero());
    let d = today(cfg, now);
    let add = |o: &mut Obj, k: &str, v: f64| {
        let prev = o.get(k).and_then(J::as_f64).unwrap_or(f64::NAN);
        o.set(k, J::Num(prev + v));
    };
    let remotes = repo_remotes(db)?;
    let mut known: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut st = db.prepare("SELECT machine, day, repo, tokens FROM repos")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            crate::db::num(r, 3)?.unwrap_or(0.0),
        ))
    })?;
    for r in rows {
        let (machine, day, repo, tokens) = r?;
        let remote = remotes.get(&(machine.clone().unwrap_or_default(), repo.clone().unwrap_or_default()));
        let p = cfg.project_key(repo.as_deref(), remote.map(String::as_str));
        add(&mut all, p, tokens);
        if day.as_deref() == Some(d.as_str()) {
            add(&mut now_p, p, tokens);
        }
        let k = format!("{}|{}", sql_str(machine.as_deref()), sql_str(day.as_deref()));
        *known.entry(k).or_insert(0.0) += tokens;
    }
    let mut st = db.prepare("SELECT machine, day, total(tokens) AS t FROM usage GROUP BY machine, day")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, crate::db::num(r, 2)?.unwrap_or(0.0)))
    })?;
    for r in rows {
        let (machine, day, t) = r?;
        let k = format!("{}|{}", sql_str(machine.as_deref()), sql_str(day.as_deref()));
        let rest = js_max(0.0, t - known.get(&k).copied().unwrap_or(0.0));
        for (p, f) in &cfg.unattributed {
            add(&mut all, p, rest * f);
            if day.as_deref() == Some(d.as_str()) {
                add(&mut now_p, p, rest * f);
            }
        }
    }
    let round =
        |o: Obj| Obj(o.0.into_iter().map(|(k, v)| (k, J::Num(js_round(v.as_f64().unwrap_or(f64::NAN))))).collect());
    Ok((round(all), round(now_p)))
}

/// Today's tokens per project by kind (`{in, cr, cw, out}`), as far as reporters 0.7 on said; a project none of
/// them reported on isn't listed. Unattributed tokens have no split, so they aren't in it.
pub fn projects_io_today(db: &Connection, cfg: &Config, now: f64) -> R<Obj> {
    let remotes = repo_remotes(db)?;
    let mut by: std::collections::BTreeMap<String, [f64; 4]> = std::collections::BTreeMap::new();
    let mut st = db.prepare("SELECT machine, repo, inp, cr, cw, out FROM repo_io WHERE day = ?1")?;
    let rows = st.query_map(params![today(cfg, now)], |r| {
        let n = |i| crate::db::num(r, i).map(|v| v.unwrap_or(0.0));
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, [n(2)?, n(3)?, n(4)?, n(5)?]))
    })?;
    for row in rows {
        let (machine, repo, v) = row?;
        let remote = remotes.get(&(machine.unwrap_or_default(), repo.clone().unwrap_or_default()));
        let p = cfg.project_key(repo.as_deref(), remote.map(String::as_str));
        let t = by.entry(p.to_string()).or_default();
        for i in 0..4 {
            t[i] += v[i];
        }
    }
    Ok(by.into_iter().fold(Obj::new(), |o, (p, v)| {
        o.with(&p, Obj::new().with("in", v[0]).with("cr", v[1]).with("cw", v[2]).with("out", v[3]))
    }))
}

/// `(machine, repo) → remote`, from protocol-2 reports.
pub fn repo_remotes(db: &Connection) -> R<std::collections::HashMap<(String, String), String>> {
    let mut st = db.prepare("SELECT machine, repo, remote FROM repo_remotes WHERE remote IS NOT NULL")?;
    let rows = st.query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?;
    rows.collect()
}

/// The pile of unprocessed tokens on the site, kept in `kv.pile`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pile {
    pub day: String,
    pub tokens: f64,
    pub total: f64,
    pub at: f64,
}

impl Pile {
    pub fn load(db: &Connection) -> Option<Pile> {
        let v = J::parse(&crate::db::kv_get(db, "pile").ok()??).ok()?;
        Some(Pile {
            day: v.get("day")?.as_str()?.to_string(),
            tokens: v.get("tokens")?.as_f64()?,
            total: v.get("total")?.as_f64()?,
            at: v.get("at")?.as_f64()?,
        })
    }
    fn to_json(&self) -> J {
        Obj::new()
            .with("day", self.day.as_str())
            .with("tokens", self.tokens)
            .with("total", self.total)
            .with("at", self.at)
            .into()
    }
}

/// Every new token lands on the pile and trucks clear it at `per_hour`; it starts empty each day.
pub fn update_pile(db: &Connection, cfg: &Config, pile: &mut Option<Pile>, now: f64) -> R<()> {
    let s = since(db, cfg, now)?;
    let day = today(cfg, now);
    let today_tokens = sum(&today_rows(db, &day)?, |r| r.tokens);
    let rate = cfg.pile.per_hour / 3_600_000.0;
    let next = match pile.take() {
        None => {
            // First run: start from the pile the site showed before this existed.
            let m = midnight(cfg, &day);
            let drained = ((now - m) / cfg.pile.seed_every_ms).floor() * cfg.pile.seed_drain;
            Pile { tokens: js_max(0.0, today_tokens - drained), day, total: s.tokens, at: now }
        }
        Some(p) if p.day != day => Pile { day, tokens: 0.0, total: s.tokens, at: now },
        Some(p) => {
            let added = js_max(0.0, s.tokens - p.total);
            let cleared = (now - p.at) * rate;
            Pile {
                day,
                tokens: js_max(0.0, js_min(today_tokens, p.tokens + added - cleared)),
                total: s.tokens,
                at: now,
            }
        }
    };
    crate::db::kv_set(db, "pile", &next.to_json().stringify())?;
    *pile = Some(next);
    Ok(())
}

/// `a.agent.localeCompare(b.agent) || (a.kind === 'main' ? -1 : 1)`, sorted the way V8 sorts short arrays (this
/// comparator is inconsistent, so the algorithm decides the order): an initial run, then binary insertion.
fn sort_agents(v: &mut [Obj]) {
    let cmp = |a: &Obj, b: &Obj| -> i32 {
        let (x, y) = (str_of(a, "agent").unwrap_or(""), str_of(b, "agent").unwrap_or(""));
        match x.cmp(y) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Equal => {
                if str_of(a, "kind") == Some("main") {
                    -1
                } else {
                    1
                }
            }
        }
    };
    let n = v.len();
    if n < 2 {
        return;
    }
    if n >= 64 {
        // V8 merges runs here; with this comparator that groups by agent, mains (latest first) then the rest.
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&i, &j| {
            let (a, b) = (&v[i], &v[j]);
            str_of(a, "agent").unwrap_or("").cmp(str_of(b, "agent").unwrap_or("")).then_with(|| {
                let (ma, mb) = (str_of(a, "kind") == Some("main"), str_of(b, "kind") == Some("main"));
                mb.cmp(&ma).then(if ma { j.cmp(&i) } else { i.cmp(&j) })
            })
        });
        let sorted: Vec<Obj> = idx.into_iter().map(|i| v[i].clone()).collect();
        v.clone_from_slice(&sorted);
        return;
    }
    // CountAndMakeRun(0, n)
    let mut run = 2;
    let descending = cmp(&v[1], &v[0]) < 0;
    while run < n {
        let o = cmp(&v[run], &v[run - 1]);
        if (descending && o >= 0) || (!descending && o < 0) {
            break;
        }
        run += 1;
    }
    if descending {
        v[..run].reverse();
    }
    // BinaryInsertionSort(0, run, n)
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + ((right - left) >> 1);
            if cmp(&v[start], &v[mid]) < 0 {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        v[left..=start].rotate_right(1);
    }
}

/// hub.mjs `publicPayload()` (plus `projectList`), at time `now`, with the hub's current pile.
pub fn public_payload(db: &Connection, cfg: &Config, pile: Option<&Pile>, now: f64) -> R<Obj> {
    let l = live(db, now)?;
    let day = today(cfg, now);
    let rows = today_rows(db, &day)?;
    let recent: Vec<&Machine> = l.machines.iter().filter(|m| now - m.last_seen < 7.0 * DAY_MS).collect();
    let working_by =
        |label: &J| l.sessions.iter().filter(|s| working(s) && strict_eq(s.get("machine"), Some(label))).count();
    // A session is listed if it works or a working one on the same machine names it as parent (one pass, not a scan
    // per session: a machine may report 2,000 sessions).
    let parents: std::collections::HashSet<(StrictKey, StrictKey)> = l
        .sessions
        .iter()
        .filter(|c| working(c))
        .filter_map(|c| Some((strict_key(c.get("machine"))?, strict_key(c.get("parent"))?)))
        .collect();
    let mut agents: Vec<Obj> = l
        .sessions
        .iter()
        .filter(|s| {
            working(s)
                || strict_key(s.get("machine")).zip(strict_key(s.get("id"))).is_some_and(|k| parents.contains(&k))
        })
        .map(|s| {
            let machine = js_string(s.get("machine"));
            let mut a = Obj::new().with("id", public_id(&machine, &js_string(s.get("id"))));
            a.set(
                "parent",
                if truthy(s.get("parent")) {
                    J::Str(public_id(&machine, &js_string(s.get("parent"))))
                } else {
                    J::Null
                },
            );
            // `agent: s.agent` — JSON.stringify drops undefined.
            for k in ["agent", "kind"] {
                if let Some(v) = s.get(k) {
                    a.set(k, v.clone());
                }
            }
            a.set("model", J::Str(model_name(s.get("model"))));
            let repo = if truthy(s.get("repo")) { s.get("repo") } else { s.get("project") };
            let repo = repo.filter(|v| v.truthy()).map(|v| js_string(Some(v)));
            a.set("project", J::Str(cfg.project_key(repo.as_deref(), str_of(s, "remote")).to_string()));
            a.set("state", if working(s) { or_null(s.get("state")) } else { J::Str("delegate".into()) });
            a.set("stateAt", or_null(s.get("stateAt")));
            a.set("io", or_null(s.get("io")));
            a
        })
        .collect();
    sort_agents(&mut agents);

    let live_j = Obj::new()
        .with("working", l.working)
        .with("subagents", l.subagents)
        .with("open", l.open)
        .with("byAgent", l.by_agent.clone())
        .with("machinesOnline", recent.iter().filter(|m| m.online).count())
        .with("machinesTotal", recent.len())
        .with("agents", J::Arr(agents.into_iter().map(J::Obj).collect()));

    let mut by_machine: Vec<(Obj, f64)> = recent
        .iter()
        .map(|m| {
            let t =
                rows.iter().filter(|r| r.machine.as_deref() == Some(m.machine.as_str())).fold(0.0, |a, r| a + r.tokens);
            let o = Obj::new()
                .with("name", m.label.clone())
                .with("online", m.online)
                .with("working", working_by(&m.label))
                .with("tokens", t);
            (o, t)
        })
        .collect();
    by_machine.sort_by(|a, b| (b.1 - a.1).partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal));
    let today_j = Obj::new()
        .with("day", day.as_str())
        .with("tokens", sum(&rows, |r| r.tokens))
        .with("output", sum(&rows, |r| r.output))
        .with("byModel", named(top_n(group(&rows, model_of), 5)))
        .with("byMachine", J::Arr(by_machine.into_iter().map(|(o, _)| J::Obj(o)).collect()));

    let s = since(db, cfg, now)?;
    let (all, today_p) = projects(db, cfg, now)?;
    // `todayIo` only once some reporter gave a split, so the payload stays hub.mjs's until then.
    let io_today = projects_io_today(db, cfg, now)?;
    let mut projects_j = Obj::new().with("all", all).with("today", today_p);
    if !io_today.0.is_empty() {
        projects_j.set("todayIo", J::Obj(io_today));
    }
    let pile_j = match pile {
        Some(p) => Obj::new().with("tokens", js_round(p.tokens)).with("perHour", cfg.pile.per_hour).into(),
        None => J::Null,
    };
    let project_list: Vec<J> = cfg
        .projects
        .iter()
        .map(|p| {
            Obj::new().with("key", p.key.as_str()).with("name", p.name.as_str()).with("color", p.color.as_str()).into()
        })
        .collect();
    Ok(Obj::new()
        .with("updated", now)
        .with("live", live_j)
        .with("today", today_j)
        .with("history", J::Arr(history(db, now)?.into_iter().map(|(_, w)| J::Num(w)).collect()))
        .with("activity", activity(db, now)?)
        .with("since", Obj::new().with("day", s.day).with("tokens", s.tokens).with("github", s.github))
        .with("pile", pile_j)
        .with("projects", projects_j)
        .with("calendar", calendar(db, &days_ago(cfg, now, 380.0))?)
        .with("projectList", J::Arr(project_list)))
}

/// What happened lately (hub.mjs `withLast`): tokens per project over the last minute, and each agent's token split
/// since the previous publish. Both advance only on the tick; a publish between ticks repeats the last tick's values.
#[derive(Default)]
pub struct Recent {
    seen: Vec<(f64, Obj, Obj)>,
    /// What the last tick added: `projects.last`, `projects.lastMs`, and each agent's `flow` by id.
    added: Option<(J, J, std::collections::HashMap<String, J>)>,
}

impl Recent {
    pub fn with_last(&mut self, d: &mut Obj) {
        self.added = None;
        let updated = d.get("updated").and_then(J::as_f64).unwrap_or(f64::NAN);
        let all = d.get("projects").and_then(|p| p.get("all")).and_then(J::as_obj).cloned().unwrap_or_default();
        let mut io = Obj::new();
        if let Some(agents) = d.get("live").and_then(|l| l.get("agents")).and_then(J::as_arr) {
            for a in agents {
                if let (Some(id), Some(v)) = (a.get("id"), a.get("io").filter(|v| v.truthy())) {
                    io.set(&js_string(Some(id)), v.clone());
                }
            }
        }
        let prev = self.seen.last().map(|(_, _, io)| io.clone());
        let old = self.seen.first().map(|(at, all, _)| (*at, all.clone()));
        self.seen.push((updated, all.clone(), io));
        while self.seen.len() > 1 && updated - self.seen[0].0 > 60_000.0 {
            self.seen.remove(0);
        }
        let (Some(prev), Some((old_at, old_all))) = (prev, old) else { return };
        let mut last = Obj::new();
        for (k, v) in &all.0 {
            let o = old_all.get(k).filter(|v| v.truthy()).and_then(J::as_f64).unwrap_or(0.0);
            last.set(k, J::Num(js_max(0.0, v.as_f64().unwrap_or(f64::NAN) - o)));
        }
        if let Some(J::Obj(p)) = d.get_mut("projects") {
            p.set("last", J::Obj(last.clone()));
            p.set("lastMs", J::Num(updated - old_at));
        }
        let mut flows = std::collections::HashMap::new();
        if let Some(J::Obj(l)) = d.get_mut("live") {
            if let Some(J::Arr(agents)) = l.get_mut("agents") {
                for a in agents.iter_mut() {
                    let J::Obj(a) = a else { continue };
                    let (Some(cur), Some(id)) =
                        (a.get("io").filter(|v| v.truthy()).cloned(), a.get("id").map(|id| js_string(Some(id))))
                    else {
                        continue;
                    };
                    let Some(p) = prev.get(&id) else { continue };
                    let n = |o: &J, k: &str| o.get(k).filter(|v| v.truthy()).and_then(J::as_f64).unwrap_or(0.0);
                    let mut flow = Obj::new();
                    for k in ["in", "cr", "cw", "out"] {
                        flow.set(k, J::Num(js_max(0.0, n(&cur, k) - n(p, k))));
                    }
                    flows.insert(id, J::Obj(flow.clone()));
                    a.set("flow", J::Obj(flow));
                }
            }
        }
        self.added = Some((J::Obj(last), J::Num(updated - old_at), flows));
    }

    /// Puts the last tick's `projects.last`, `projects.lastMs` and agents' `flow` on a payload built between ticks,
    /// where `with_last` would have put them, without moving the window.
    pub fn carry_last(&self, d: &mut Obj) {
        let Some((last, ms, flows)) = &self.added else { return };
        if let Some(J::Obj(p)) = d.get_mut("projects") {
            p.set("last", last.clone());
            p.set("lastMs", ms.clone());
        }
        let Some(J::Obj(l)) = d.get_mut("live") else { return };
        let Some(J::Arr(agents)) = l.get_mut("agents") else { return };
        for a in agents.iter_mut() {
            let J::Obj(a) = a else { continue };
            if !a.get("io").is_some_and(J::truthy) {
                continue;
            }
            if let Some(f) = a.get("id").and_then(|id| flows.get(&js_string(Some(id)))) {
                a.set("flow", f.clone());
            }
        }
    }
}

/// hub.mjs `labels()`: machine → label.
fn labels(db: &Connection) -> R<std::collections::HashMap<String, Option<String>>> {
    let mut st = db.prepare("SELECT machine, label FROM machines")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
    rows.collect()
}

/// hub.mjs `internalState()`, for the private dashboard (plus machine metadata).
pub fn internal_state(db: &Connection, cfg: &Config, now: f64, publish: J, publish_targets: J) -> R<Obj> {
    let l = live(db, now)?;
    let rows = today_rows(db, &today(cfg, now))?;
    let names = labels(db)?;
    let mut daily = Obj::new();
    let mut st =
        db.prepare("SELECT day, machine, total(tokens) AS t FROM usage WHERE day >= ? GROUP BY day, machine")?;
    let q = st.query_map(params![days_ago(cfg, now, 89.0)], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, crate::db::num(r, 2)?))
    })?;
    for r in q {
        let (day, machine, t) = r?;
        let machine = machine.unwrap_or_default();
        let name = names.get(&machine).cloned().flatten().filter(|s| !s.is_empty()).unwrap_or(machine);
        if daily.get(&day).is_none() {
            daily.set(&day, J::Obj(Obj::new()));
        }
        if let Some(J::Obj(o)) = daily.get_mut(&day) {
            o.set(&name, t.into());
        }
    }
    let week = db
        .query_row("SELECT total(tokens) AS t FROM usage WHERE day >= ?", params![days_ago(cfg, now, 6.0)], |r| {
            crate::db::num(r, 0)
        })?
        .unwrap_or(0.0);
    let s = since(db, cfg, now)?;
    let meta = machine_meta(db)?;
    let machines: Vec<J> = l
        .machines
        .iter()
        .map(|m| {
            let mut o = Obj::new()
                .with("machine", m.machine.as_str())
                .with("label", m.label.clone())
                .with("platform", m.platform.clone())
                .with("lastSeen", m.last_seen)
                .with("online", m.online)
                .with(
                    "tokensToday",
                    rows.iter()
                        .filter(|r| r.machine.as_deref() == Some(m.machine.as_str()))
                        .fold(0.0, |a, r| a + r.tokens),
                )
                .with(
                    "working",
                    l.sessions.iter().filter(|s| working(s) && strict_eq(s.get("machine"), Some(&m.label))).count(),
                );
            if let Some(x) = meta.get(&m.machine) {
                for (k, v) in &x.0 {
                    o.set(k, v.clone());
                }
            }
            o.into()
        })
        .collect();
    let today_j = Obj::new()
        .with("tokens", sum(&rows, |r| r.tokens))
        .with("output", sum(&rows, |r| r.output))
        .with("byModel", named(group(&rows, model_of)))
        .with("byAgent", named(group(&rows, |r| sql_str(r.agent.as_deref()).to_string())));
    Ok(Obj::new()
        .with("now", now)
        .with("machines", machines)
        .with("sessions", J::Arr(l.sessions.into_iter().map(J::Obj).collect()))
        .with("working", l.working)
        .with("subagents", l.subagents)
        .with("open", l.open)
        .with("byAgent", l.by_agent)
        .with("today", today_j)
        .with("week", week)
        .with("allTime", s.tokens)
        .with("firstDay", s.day)
        .with("daily", daily)
        .with("timezone", cfg.tz_name.as_str())
        .with(
            "history",
            J::Arr(
                history(db, now)?
                    .into_iter()
                    .map(|(at, w)| Obj::new().with("at", at).with("working", w).into())
                    .collect(),
            ),
        )
        .with("publish", publish)
        .with("publishTargets", publish_targets))
}

/// Reporter version/protocol and how each machine authenticated.
fn machine_meta(db: &Connection) -> R<std::collections::HashMap<String, Obj>> {
    let mut st = db.prepare(
        "SELECT m.machine, m.version, m.protocol, m.auth, t.label, t.created, t.revoked
           FROM machine_meta m LEFT JOIN machine_tokens t ON t.machine = m.machine",
    )?;
    let rows = st.query_map([], |r| {
        let o = Obj::new()
            .with("version", r.get::<_, Option<String>>(1)?)
            .with("protocol", r.get::<_, Option<i64>>(2)?)
            .with("auth", r.get::<_, Option<String>>(3)?)
            .with("enrolled", r.get::<_, Option<i64>>(5)?.is_some())
            .with("revoked", r.get::<_, Option<i64>>(6)?.is_some());
        Ok((r.get::<_, String>(0)?, o))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_like_hub_mjs() {
        let m = |s: &str| model_name(Some(&J::Str(s.into())));
        assert_eq!(m("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(m("claude-opus-4-5-20251101"), "Opus 4.5");
        assert_eq!(m("claude-sonnet-4-20250514"), "Sonnet 4");
        assert_eq!(m("claude-3-5-haiku-20241022"), "Haiku 20241022");
        assert_eq!(m("opus-45-123"), "Opus 45");
        assert_eq!(m("sonnet-4-5x"), "Sonnet 4.5");
        assert_eq!(m("gpt-5..1--a"), "GPT-5..1 A");
        assert_eq!(m("OPUS-4-123"), "OPUS 4");
        assert_eq!(m("claude-haiku-4-5"), "Haiku 4.5");
        assert_eq!(m("gpt-6-astra"), "GPT-6 Astra");
        assert_eq!(m("gpt-5.3-codex"), "GPT-5.3 Codex");
        assert_eq!(m("gpt5.5"), "GPT-5.5");
        assert_eq!(m("gpt-x"), "gpt-x");
        assert_eq!(m("grok-4.6-build"), "Grok 4.6 Build");
        assert_eq!(m("grok-code-fast"), "Grok Code Fast");
        assert_eq!(m("unknown"), "Other");
        // Multi-byte characters where the prefixes are checked must not panic (the hub runs with panic = abort).
        for s in ["grö-x", "grok", "gp€", "g€", "€", "opus-€", "gpt-5-ö", "グロック-1", "grok-ö"] {
            let _ = m(s);
        }
        assert_eq!(m("grok-ö"), "Grok Ö");
        assert_eq!(m(""), "Other");
        assert_eq!(model_name(None), "Other");
        assert_eq!(m("<synthetic>"), "<synthetic>");
    }

    #[test]
    fn public_ids_are_sha256_prefixes() {
        // createHash('sha256').update('Fixture|abc').digest('hex').slice(0, 10)
        assert_eq!(public_id("Fixture", "abc").len(), 10);
        assert_eq!(public_id("a", "b"), &format!("{:x}", Sha256::digest(b"a|b"))[..10]);
    }

    #[test]
    fn agents_sort_like_v8() {
        let a = |agent: &str, kind: &str, id: &str| Obj::new().with("agent", agent).with("kind", kind).with("id", id);
        let mut v = vec![
            a("grok", "main", "g1"),
            a("claude", "subagent", "s1"),
            a("claude", "main", "m1"),
            a("claude", "main", "m2"),
            a("codex", "main", "c1"),
            a("claude", "subagent", "s2"),
        ];
        sort_agents(&mut v);
        let ids: Vec<&str> = v.iter().map(|o| str_of(o, "id").unwrap()).collect();
        // Worked through V8's CountAndMakeRun + BinaryInsertionSort by hand.
        assert_eq!(ids, ["m2", "m1", "s1", "s2", "c1", "g1"]);
    }
}
