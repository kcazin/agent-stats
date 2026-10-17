//! Small pieces of JavaScript semantics the reporter's counting rules depend on: loosely typed JSON values with
//! JS truthiness, `Date.parse`, `new Date(x)`, Node's `path.basename`, and the reporter's `toolKind`/`repoOf`.

use jiff::tz::TimeZone;
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::sync::OnceLock;

/// A JSON value as the JS reporter sees a scalar field: objects and arrays are only "something truthy".
#[derive(Debug, Clone, PartialEq, Default)]
pub enum JsVal<'a> {
    #[default]
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    Str(Cow<'a, str>),
    Obj,
    Arr,
}

impl<'a> JsVal<'a> {
    pub fn truthy(&self) -> bool {
        match self {
            JsVal::Undef | JsVal::Null => false,
            JsVal::Bool(b) => *b,
            JsVal::Num(n) => *n != 0.0 && !n.is_nan(),
            JsVal::Str(s) => !s.is_empty(),
            JsVal::Obj | JsVal::Arr => true,
        }
    }
    pub fn nullish(&self) -> bool {
        matches!(self, JsVal::Undef | JsVal::Null)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsVal::Str(s) => Some(s),
            _ => None,
        }
    }
    /// The string, when the value is a truthy string.
    pub fn nonempty_str(&self) -> Option<&str> {
        self.as_str().filter(|s| !s.is_empty())
    }
    pub fn is(&self, s: &str) -> bool {
        self.as_str() == Some(s)
    }
    /// `x ?? 0` used as a token count. Strings would concatenate in JS; they count as 0 here.
    pub fn num_or0(&self) -> i64 {
        match self {
            JsVal::Num(n) if n.is_finite() => *n as i64,
            JsVal::Bool(true) => 1,
            _ => 0,
        }
    }
    /// JS `ToNumber`.
    pub fn to_number(&self) -> f64 {
        match self {
            JsVal::Undef => f64::NAN,
            JsVal::Null => 0.0,
            JsVal::Bool(b) => *b as u8 as f64,
            JsVal::Num(n) => *n,
            JsVal::Str(s) => str_to_number(s),
            JsVal::Obj | JsVal::Arr => f64::NAN,
        }
    }
    /// JS `String(x)`.
    pub fn js_string(&self) -> Cow<'_, str> {
        match self {
            JsVal::Undef => "undefined".into(),
            JsVal::Null => "null".into(),
            JsVal::Bool(b) => if *b { "true" } else { "false" }.into(),
            JsVal::Num(n) => num_to_string(*n).into(),
            JsVal::Str(s) => Cow::Borrowed(s),
            JsVal::Obj => "[object Object]".into(),
            JsVal::Arr => "".into(),
        }
    }
    pub fn into_owned(self) -> JsVal<'static> {
        match self {
            JsVal::Str(s) => JsVal::Str(Cow::Owned(s.into_owned())),
            JsVal::Undef => JsVal::Undef,
            JsVal::Null => JsVal::Null,
            JsVal::Bool(b) => JsVal::Bool(b),
            JsVal::Num(n) => JsVal::Num(n),
            JsVal::Obj => JsVal::Obj,
            JsVal::Arr => JsVal::Arr,
        }
    }
}

pub fn num_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

fn str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if t.bytes().any(|c| c.is_ascii_alphabetic() && c != b'e' && c != b'E') {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

struct JsValVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for JsValVisitor<'a> {
    type Value = JsVal<'a>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(JsVal::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(JsVal::Num(v as f64))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(JsVal::Num(v as f64))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(JsVal::Num(v))
    }
    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(JsVal::Str(Cow::Borrowed(v)))
    }
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(JsVal::Str(Cow::Owned(v.to_owned())))
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(JsVal::Str(Cow::Owned(v)))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsVal::Null)
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsVal::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(JsVal::Obj)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(JsVal::Arr)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for JsVal<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(JsValVisitor(PhantomData))
    }
}

/// An object field read as `T` when it is an object; anything else (null, a string, an array) is `None`, the way
/// property access on a non-object yields undefined in JS instead of failing the whole line.
#[derive(Debug, Default)]
pub struct Lax<T>(pub Option<T>);

struct LaxVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for LaxVisitor<T> {
    type Value = Lax<T>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| Lax(Some(t)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Lax(None))
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Lax(None))
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Lax<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LaxVisitor(PhantomData))
    }
}

impl<T> Lax<T> {
    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

/// A JSON object read in key order (`Object.entries`). `None` when the value is not an object.
#[derive(Debug, Default)]
pub struct Entries<'a, T>(pub Option<Vec<(Cow<'a, str>, T)>>);

struct EntriesVisitor<'a, T>(PhantomData<(&'a (), T)>);

impl<'de: 'a, 'a, T: Deserialize<'de>> Visitor<'de> for EntriesVisitor<'a, T> {
    type Value = Entries<'a, T>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out: Vec<(Cow<'a, str>, T)> = Vec::new();
        while let Some(k) = map.next_key::<JsVal<'a>>()? {
            let v = map.next_value::<T>()?;
            let k = match k {
                JsVal::Str(s) => s,
                other => Cow::Owned(other.js_string().into_owned()),
            };
            // Duplicate keys: the last value wins, in the first key's place.
            if let Some(slot) = out.iter_mut().find(|(ok, _)| *ok == k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
        Ok(Entries(Some(out)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Entries(None))
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Entries(None))
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de: 'a, 'a, T: Deserialize<'de>> Deserialize<'de> for Entries<'a, T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(EntriesVisitor(PhantomData))
    }
}

// ---- time ---------------------------------------------------------------------------------------------------

const MAX_MS: f64 = 8.64e15;

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `Date.parse` for the ISO 8601 forms V8 accepts (the only ones these transcripts use): date-only forms are
/// UTC, date-times without an offset are local time. Other formats (RFC 2822 etc.) give `None` (NaN).
pub fn date_parse(s: &str, local: &TimeZone) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() {
            return None;
        }
        let mut v = 0i64;
        for &c in &b[*i..*i + n] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        *i += n;
        Some(v)
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let neg = b[0] == b'-';
            i = 1;
            let y = digits(&mut i, 6)?;
            if neg && y == 0 {
                return None;
            }
            if neg {
                -y
            } else {
                y
            }
        }
        _ => digits(&mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = digits(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let date_ms = days_from_civil(year, month, day) as f64 * 86_400_000.0;
    if i == b.len() {
        return Some(date_ms).filter(|v| v.abs() <= MAX_MS);
    }
    if !matches!(b[i], b'T' | b't' | b' ') {
        return None;
    }
    i += 1;
    let h = digits(&mut i, 2)?;
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let mi = digits(&mut i, 2)?;
    let (mut sec, mut ms) = (0, 0);
    if b.get(i) == Some(&b':') {
        i += 1;
        sec = digits(&mut i, 2)?;
        if matches!(b.get(i), Some(b'.') | Some(b',')) {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            let frac = &b[start..i.min(start + 3)];
            let mut v = 0i64;
            for k in 0..3 {
                v = v * 10 + frac.get(k).map_or(0, |c| (c - b'0') as i64);
            }
            ms = v;
        }
    }
    if h > 24 || mi > 59 || sec > 59 || (h == 24 && (mi > 0 || sec > 0 || ms > 0)) {
        return None;
    }
    let tod = ((h * 60 + mi) * 60 + sec) * 1000 + ms;
    let offset_ms = if i == b.len() {
        // No offset: local time.
        let dt = jiff::civil::DateTime::new(year as i16, month as i8, day as i8, 0, 0, 0, 0).ok()?;
        let dt = dt.checked_add(jiff::SignedDuration::from_millis(tod)).ok()?;
        let z = dt.to_zoned(local.clone()).ok()?;
        return Some(z.timestamp().as_millisecond() as f64);
    } else {
        match b[i] {
            b'Z' | b'z' => {
                i += 1;
                0
            }
            b'+' | b'-' => {
                let sign = if b[i] == b'+' { 1 } else { -1 };
                i += 1;
                let oh = digits(&mut i, 2)?;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = digits(&mut i, 2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                sign * (oh * 60 + om) * 60_000
            }
            _ => return None,
        }
    };
    if i != b.len() {
        return None;
    }
    let v = date_ms + (tod - offset_ms) as f64;
    Some(v).filter(|v| v.abs() <= MAX_MS)
}

/// The time value of `new Date(x)`, or `None` for an invalid date.
pub fn date_value(v: &JsVal, local: &TimeZone) -> Option<f64> {
    let t = match v {
        JsVal::Str(s) => date_parse(s, local)?,
        JsVal::Num(n) => *n,
        JsVal::Bool(b) => *b as u8 as f64,
        JsVal::Null => 0.0,
        _ => return None,
    };
    if !t.is_finite() || t.abs() > MAX_MS {
        return None;
    }
    Some(t.trunc())
}

/// `Date.parse(ts) || Number(ts) || Date.now()`, the timestamp the reporter gives a session's latest state.
pub fn act_time(v: &JsVal, local: &TimeZone, now: f64) -> f64 {
    if let JsVal::Str(s) = v {
        if let Some(t) = date_parse(s, local) {
            if t != 0.0 {
                return t;
            }
        }
    }
    let n = v.to_number();
    if n != 0.0 && !n.is_nan() {
        return n;
    }
    now
}

/// Calendar days ("YYYY-MM-DD") in the reporting time zone, cached per day so consecutive timestamps are cheap.
pub struct DayCache {
    tz: TimeZone,
    start: i64,
    end: i64,
    label: String,
}

impl DayCache {
    pub fn new(tz: TimeZone) -> Self {
        DayCache { tz, start: 0, end: 0, label: String::new() }
    }
    pub fn day(&mut self, ms: f64) -> Option<&str> {
        if !ms.is_finite() || ms.abs() > MAX_MS {
            return None;
        }
        let ms = ms.trunc() as i64;
        if ms >= self.start && ms < self.end {
            return Some(&self.label);
        }
        let ts = jiff::Timestamp::from_millisecond(ms).ok()?;
        let date = ts.to_zoned(self.tz.clone()).date();
        let start = date.to_zoned(self.tz.clone()).ok()?.timestamp().as_millisecond();
        let end = date.tomorrow().ok()?.to_zoned(self.tz.clone()).ok()?.timestamp().as_millisecond();
        self.start = start;
        self.end = end;
        self.label = format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day());
        Some(&self.label)
    }
}

// ---- paths ----------------------------------------------------------------------------------------------------

/// Node's posix `path.basename`.
pub fn basename(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return "";
    }
    match t.rfind('/') {
        Some(i) => &t[i + 1..],
        None => t,
    }
}

/// `path.basename(p, ext)`.
pub fn basename_ext<'a>(p: &'a str, ext: &str) -> &'a str {
    let b = basename(p);
    match b.strip_suffix(ext) {
        Some(s) if !s.is_empty() => s,
        _ => b,
    }
}

/// Node's posix `path.dirname` for the absolute, normalised paths the scan produces.
pub fn dirname(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => "/",
        Some(i) => &t[..i],
        None => ".",
    }
}

/// `decodeURIComponent`, or the input when it is not valid percent-encoded UTF-8 (where JS would throw).
pub fn decode_uri_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(v) => {
                    out.push(v);
                    i += 3;
                }
                None => return s.to_owned(),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

// ---- the reporter's classification rules -----------------------------------------------------------------------

/// The lowercase words of a name, split on anything that is not a letter or digit and on camelCase boundaries:
/// `MultiEdit` -> multi, edit; `mcp__srv__list_tools` -> mcp, srv, list, tools.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// What a tool call is doing, folded into a few coarse kinds so nothing specific leaves the machine.
///
/// Names are matched on whole words, never on substrings (`mcp__srv__list_tools` is not a read because of "ls").
/// MCP tools (`mcp__server__tool`) are classified by the leading verb of the tool part.
pub fn tool_kind(name: &str) -> &'static str {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let tool = rest.rsplit("__").next().unwrap_or("");
        return match words(tool).first().map(String::as_str) {
            Some("list" | "get" | "read" | "search" | "fetch" | "find") => "read",
            Some("create" | "update" | "write" | "delete" | "edit" | "set") => "edit",
            _ => "tool",
        };
    }
    let w = words(name);
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    let first = w.first().copied().unwrap_or("");
    let any = |l: &[&str]| w.iter().any(|x| l.contains(x));
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    if matches!(first, "bash" | "shell" | "exec" | "execute" | "run") || pair("local", "shell") && first == "local" {
        "run"
    } else if any(&[
        "edit",
        "edits",
        "multiedit",
        "notebookedit",
        "write",
        "todowrite",
        "patch",
        "notebook",
        "delete",
        "move",
    ]) {
        "edit"
    } else if any(&["read", "grep", "glob", "search", "websearch", "fetch", "webfetch", "ls", "find", "view"]) {
        "read"
    } else if matches!(first, "task" | "agent" | "spawn") || any(&["subagent", "delegate"]) || pair("sub", "agent") {
        "delegate"
    } else if any(&["question", "askuser", "askuserquestion"]) || pair("ask", "user") {
        "waiting"
    } else {
        "tool"
    }
}

/// `toolKind(x)` for a loosely typed name: undefined is the default `''`.
pub fn tool_kind_of(v: &JsVal) -> &'static str {
    match v {
        JsVal::Undef => tool_kind(""),
        v => tool_kind(&v.js_string()),
    }
}

struct RepoRes {
    t3: regex_lite::Regex,
    codex: regex_lite::Regex,
    claude: regex_lite::Regex,
    dev: regex_lite::Regex,
    scratch: regex_lite::Regex,
}

fn repo_res() -> &'static RepoRes {
    static R: OnceLock<RepoRes> = OnceLock::new();
    R.get_or_init(|| RepoRes {
        t3: regex_lite::Regex::new(r"/\.t3/worktrees/([^/]+)").unwrap(),
        codex: regex_lite::Regex::new(r"/\.codex/worktrees/[^/]+/([^/]+)").unwrap(),
        claude: regex_lite::Regex::new(r"/([^/]+)/\.claude/worktrees/").unwrap(),
        dev: regex_lite::Regex::new(r"^~/(?:Dev|dev|Developer|Projects|projects|src|code|Documents)/([^/]+)").unwrap(),
        scratch: regex_lite::Regex::new(r"^~/\.t3/scratch/").unwrap(),
    })
}

/// The repository a working directory belongs to, seeing through worktrees.
pub fn repo_of(cwd: &str, home: &str) -> String {
    let p: Cow<str> = match cwd.strip_prefix(home) {
        Some(rest) => format!("~{rest}").into(),
        None => cwd.into(),
    };
    let r = repo_res();
    for re in [&r.t3, &r.codex, &r.claude, &r.dev] {
        if let Some(c) = re.captures(&p) {
            return c[1].to_owned();
        }
    }
    if r.scratch.is_match(&p) {
        return "scratch".into();
    }
    let b = basename(&p);
    if b.is_empty() {
        p.into_owned()
    } else {
        b.to_owned()
    }
}

/// FNV-1a, for compact message-id windows and dedupe keys.
pub fn fnv64(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for &c in *p {
            h ^= c as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// A dedupe id (`f.ids.includes(id)` uses SameValueZero): `None` for objects, which never compare equal.
pub fn id_hash(v: &JsVal) -> Option<u64> {
    match v {
        JsVal::Str(s) => Some(fnv64(&[b"s:", s.as_bytes()])),
        JsVal::Num(n) => Some(fnv64(&[b"n:", num_to_string(*n).as_bytes()])),
        JsVal::Bool(b) => Some(fnv64(&[if *b { b"b:true" } else { b"b:false" }])),
        // A window loaded from JS state holds `null` where JS had `undefined`.
        JsVal::Null | JsVal::Undef => Some(fnv64(&[b"null"])),
        JsVal::Obj | JsVal::Arr => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokyo() -> TimeZone {
        jiff::tz::db().get("Asia/Tokyo").unwrap()
    }

    #[test]
    fn date_parse_matches_v8() {
        let tz = tokyo();
        assert_eq!(date_parse("2026-10-12T08:43:28.298Z", &tz), Some(1791794608298.0));
        assert_eq!(date_parse("2026-10-12T08:43:28Z", &tz), Some(1791794608000.0));
        assert_eq!(date_parse("2026-10-12", &tz), Some(1791763200000.0));
        assert_eq!(date_parse("2026-10-12T08:43:28.298+09:00", &tz), Some(1791762208298.0));
        assert_eq!(date_parse("2026-10-12T08:43:28.29812345Z", &tz), Some(1791794608298.0));
        // No offset: local time (Tokyo here).
        assert_eq!(date_parse("2026-10-12T08:43:28", &tz), Some(1791762208000.0));
        assert_eq!(date_parse("1787644159582", &tz), None);
        assert_eq!(date_parse("nope", &tz), None);
        assert_eq!(date_parse("2026-02-30T00:00:00Z", &tz), None);
    }

    #[test]
    fn act_time_falls_back_like_js() {
        let tz = tokyo();
        assert_eq!(act_time(&JsVal::Num(1787644159582.0), &tz, 5.0), 1787644159582.0);
        assert_eq!(act_time(&JsVal::Str("2026-10-12T08:43:28Z".into()), &tz, 5.0), 1791794608000.0);
        assert_eq!(act_time(&JsVal::Undef, &tz, 5.0), 5.0);
        assert_eq!(act_time(&JsVal::Num(f64::NAN), &tz, 5.0), 5.0);
        assert_eq!(act_time(&JsVal::Str("".into()), &tz, 5.0), 5.0);
    }

    #[test]
    fn day_bucketing_uses_the_zone() {
        let mut d = DayCache::new(tokyo());
        // 2026-08-25T03:09:44Z is the 25th in Tokyo; 15:00Z is already the 26th.
        assert_eq!(d.day(1787644184000.0), Some("2026-08-25"));
        assert_eq!(d.day(date_parse("2026-08-25T14:59:59.999Z", &tokyo()).unwrap()), Some("2026-08-25"));
        assert_eq!(d.day(date_parse("2026-08-25T15:00:00Z", &tokyo()).unwrap()), Some("2026-08-26"));
        assert_eq!(d.day(-1.0), Some("1970-01-01"));
        assert_eq!(d.day(f64::NAN), None);
        let mut u = DayCache::new(TimeZone::UTC);
        assert_eq!(u.day(date_parse("2026-08-25T15:00:00Z", &tokyo()).unwrap()), Some("2026-08-25"));
        let mut ny = DayCache::new(jiff::tz::db().get("America/New_York").unwrap());
        assert_eq!(ny.day(date_parse("2026-08-25T03:00:00Z", &tokyo()).unwrap()), Some("2026-08-24"));
    }

    #[test]
    fn tool_kinds() {
        for (n, k) in [
            ("Bash", "run"),
            ("exec_command", "run"),
            ("local_shell_call", "run"),
            ("Edit", "edit"),
            ("MultiEdit", "edit"),
            ("apply_patch", "edit"),
            ("NotebookEdit", "edit"),
            ("Read", "read"),
            ("Grep", "read"),
            ("WebFetch", "read"),
            ("mcp__x__list_projects", "read"),
            ("mcp__srv__list_tools", "read"),
            ("mcp__srv__do", "tool"),
            ("mcp__srv__create_issue", "edit"),
            ("mcp__srv__getIssue", "read"),
            ("mcp__srv__preview_click", "tool"),
            ("mcp__srv__delete_thing", "edit"),
            ("list_tools", "tool"),
            ("ExitPlanMode", "tool"),
            ("Glob", "read"),
            ("WebSearch", "read"),
            ("BashOutput", "run"),
            ("execute", "run"),
            ("MultiEdit", "edit"),
            ("delegate_task", "delegate"),
            ("ask_user", "waiting"),
            ("fields_of_tools", "tool"),
            ("Please read this", "read"),
            ("view_image", "read"),
            ("Task", "delegate"),
            ("Agent", "delegate"),
            ("spawn_agent", "delegate"),
            ("AskUserQuestion", "waiting"),
            ("TodoWrite", "edit"),
            ("", "tool"),
            ("web_search_call", "read"),
            ("custom_tool_call", "tool"),
        ] {
            assert_eq!(tool_kind(n), k, "{n}");
        }
        assert_eq!(tool_kind_of(&JsVal::Null), "tool");
    }

    #[test]
    fn repos() {
        let h = "/home/k";
        assert_eq!(repo_of("/home/k/.t3/worktrees/space-rock-ios/cos-r191", h), "space-rock-ios");
        assert_eq!(repo_of("/home/k/.codex/worktrees/abcd/chum/sub", h), "chum");
        assert_eq!(repo_of("/home/k/Dev/kilcazn.com/.claude/worktrees/agent-1", h), "kilcazn.com");
        assert_eq!(repo_of("/home/k/Dev/content-hall/web/src", h), "content-hall");
        assert_eq!(repo_of("/home/k/code/x", h), "x");
        assert_eq!(repo_of("/home/k/.t3/scratch/abc", h), "scratch");
        assert_eq!(repo_of("/home/k", h), "~");
        assert_eq!(repo_of("/home/k/other/thing", h), "thing");
        assert_eq!(repo_of("/srv/app/", h), "app");
        assert_eq!(repo_of("/", h), "/");
        // startsWith, as in JS.
        assert_eq!(repo_of("/home/k2/Dev/x", h), "x");
        assert_eq!(repo_of("/private/tmp/a/.claude/worktrees/b", h), "a");
    }

    #[test]
    fn paths() {
        assert_eq!(basename("/a/b/c.jsonl"), "c.jsonl");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(basename_ext("/a/b/c.jsonl", ".jsonl"), "c");
        assert_eq!(basename_ext("/a/.jsonl", ".jsonl"), ".jsonl");
        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(decode_uri_component("%2Fhome%2Fkillian%2FDev"), "/home/killian/Dev");
        assert_eq!(decode_uri_component("%E3%81%82"), "あ");
        assert_eq!(decode_uri_component("%zz"), "%zz");
        assert_eq!(decode_uri_component("%FF"), "%FF");
    }

    #[test]
    fn jsval_parsing() {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct T<'a> {
            #[serde(borrow)]
            a: JsVal<'a>,
            #[serde(borrow)]
            b: JsVal<'a>,
            #[serde(borrow)]
            c: JsVal<'a>,
            #[serde(borrow)]
            d: JsVal<'a>,
            #[serde(borrow)]
            e: Lax<Inner<'a>>,
            #[serde(borrow)]
            f: Lax<Inner<'a>>,
        }
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Inner<'a> {
            #[serde(borrow)]
            x: JsVal<'a>,
        }
        let t: T = serde_json::from_str(r#"{"a":"s","b":null,"c":{"q":[1,2]},"e":{"x":3},"f":"str","z":[1,{"a":2}]}"#)
            .unwrap();
        assert!(t.a.is("s"));
        assert_eq!(t.b, JsVal::Null);
        assert_eq!(t.c, JsVal::Obj);
        assert_eq!(t.d, JsVal::Undef);
        assert_eq!(t.e.get().unwrap().x, JsVal::Num(3.0));
        assert!(t.f.get().is_none());
    }
}
