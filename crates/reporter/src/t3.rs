//! Claude Code deletes transcripts after 30 days by default. T3 Code's usage cache keeps one record per API
//! response, `[time, model, session, input, cache read, cache write, output, …, dedupe key]`, including for
//! transcripts that are gone, so the first run takes the history of deleted transcripts from there.
//!
//! The caches are streamed: only the records of deleted, untracked transcripts are kept, in a compact form.

use crate::js::{date_value, fnv64, JsVal};
use crate::lines::Sink;
use crate::state::{Agent, Contrib};
use jiff::tz::TimeZone;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

struct Rec {
    ms: Option<f64>,
    model: Option<usize>,
    n: f64,
    out: f64,
    key: Option<u64>,
}

struct Entry {
    agent: Option<Agent>,
    recs: Option<Vec<Rec>>,
    cache: usize,
}

/// JS `+` on the record's numbers: null is 0, anything else non-numeric poisons the sum.
fn plus(v: &JsVal) -> f64 {
    match v {
        JsVal::Num(n) => *n,
        JsVal::Null => 0.0,
        JsVal::Bool(b) => *b as u8 as f64,
        _ => f64::NAN,
    }
}

fn index(v: &JsVal) -> Option<usize> {
    match v {
        JsVal::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
        JsVal::Str(s)
            if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) && (s.len() == 1 || !s.starts_with('0')) =>
        {
            s.parse().ok()
        }
        _ => None,
    }
}

struct RecSeed<'c> {
    local: &'c TimeZone,
}

impl<'de> DeserializeSeed<'de> for RecSeed<'_> {
    type Value = Option<Rec>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RecSeed<'_> {
    type Value = Option<Rec>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a usage record")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut v: Vec<JsVal> = Vec::with_capacity(9);
        while let Some(x) = seq.next_element::<JsVal>()? {
            if v.len() < 9 {
                v.push(x.into_owned());
            }
        }
        v.resize(9, JsVal::Undef);
        let n = plus(&v[3]) + plus(&v[4]) + plus(&v[5]) + plus(&v[6]);
        Ok(Some(Rec {
            ms: date_value(&v[0], self.local),
            model: index(&v[1]),
            n,
            out: plus(&v[6]),
            key: v[8].truthy().then(|| fnv64(&[v[8].js_string().as_bytes()])),
        }))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

struct RecsSeed<'c> {
    local: &'c TimeZone,
}

impl<'de> DeserializeSeed<'de> for RecsSeed<'_> {
    type Value = Option<Vec<Rec>>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RecsSeed<'_> {
    type Value = Option<Vec<Rec>>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("usage records")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(r) = seq.next_element_seed(RecSeed { local: self.local })? {
            if let Some(r) = r {
                out.push(r);
            }
        }
        Ok(Some(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

struct FileSeed<'c> {
    local: &'c TimeZone,
    cache: usize,
}

impl<'de> DeserializeSeed<'de> for FileSeed<'_> {
    type Value = Entry;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for FileSeed<'_> {
    type Value = Entry;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a cached file")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut e = Entry { agent: None, recs: None, cache: self.cache };
        while let Some(k) = map.next_key::<JsVal>()? {
            match k.as_str() {
                Some("p") => e.agent = map.next_value::<JsVal>()?.as_str().and_then(Agent::parse),
                Some("r") => e.recs = map.next_value_seed(RecsSeed { local: self.local })?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(e)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Entry { agent: None, recs: None, cache: self.cache })
    }
}

struct FilesSeed<'c> {
    local: &'c TimeZone,
    cache: usize,
    skip: &'c dyn Fn(&str) -> bool,
}

impl<'de> DeserializeSeed<'de> for FilesSeed<'_> {
    type Value = Vec<(String, Entry)>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for FilesSeed<'_> {
    type Value = Vec<(String, Entry)>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("cached files")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(k) = map.next_key::<JsVal>()? {
            let path = k.js_string().into_owned();
            // Tracked or still on disk: the transcript itself is (or will be) counted.
            if (self.skip)(&path) {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            out.push((path, map.next_value_seed(FileSeed { local: self.local, cache: self.cache })?));
        }
        Ok(out)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }
}

struct Cache {
    models: Vec<JsVal<'static>>,
    files: Vec<(String, Entry)>,
}

struct TopSeed<'c> {
    local: &'c TimeZone,
    cache: usize,
    skip: &'c dyn Fn(&str) -> bool,
}

impl<'de> DeserializeSeed<'de> for TopSeed<'_> {
    type Value = Cache;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for TopSeed<'_> {
    type Value = Cache;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a T3 Code usage cache")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut c = Cache { models: Vec::new(), files: Vec::new() };
        while let Some(k) = map.next_key::<JsVal>()? {
            match k.as_str() {
                Some("models") => {
                    c.models = match map.next_value::<serde_json::Value>()? {
                        serde_json::Value::Array(a) => a
                            .into_iter()
                            .map(|v| match v {
                                serde_json::Value::String(s) => JsVal::Str(s.into()),
                                serde_json::Value::Number(n) => JsVal::Num(n.as_f64().unwrap_or(f64::NAN)),
                                serde_json::Value::Bool(b) => JsVal::Bool(b),
                                serde_json::Value::Null => JsVal::Null,
                                serde_json::Value::Object(_) => JsVal::Obj,
                                serde_json::Value::Array(_) => JsVal::Arr,
                            })
                            .collect(),
                        _ => Vec::new(),
                    }
                }
                Some("files") => {
                    c.files =
                        map.next_value_seed(FilesSeed { local: self.local, cache: self.cache, skip: self.skip })?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(c)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Cache { models: Vec::new(), files: Vec::new() })
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Cache { models: Vec::new(), files: Vec::new() })
    }
}

/// Reads T3 Code's usage caches and returns the tokens of transcripts that no longer exist and aren't tracked,
/// as a delta for the totals, and the number of tokens imported.
pub fn import(
    home: &Path,
    git: Option<&crate::git::GitIds>,
    tracked: &dyn Fn(&str) -> bool,
    tz: &TimeZone,
    local: &TimeZone,
    now: f64,
) -> (Contrib, f64) {
    let skip = |p: &str| tracked(p) || std::fs::metadata(p).is_ok();
    let mut models: Vec<Vec<JsVal<'static>>> = Vec::new();
    let mut entries: Vec<(String, Entry)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for name in ["usage-scan-cache.json", "usage-scan-cache-v5.json"] {
        let Ok(file) = File::open(home.join(".t3").join("userdata").join(name)) else { continue };
        let mut de = serde_json::Deserializer::from_reader(BufReader::with_capacity(256 << 10, file));
        let seed = TopSeed { local, cache: models.len(), skip: &skip };
        let Ok(c) = seed.deserialize(&mut de).and_then(|c| de.end().map(|_| c)) else { continue };
        models.push(c.models);
        // A later cache replaces an earlier one's entry for the same file, in the earlier one's place.
        for (path, e) in c.files {
            match at.get(&path) {
                Some(&i) => entries[i].1 = e,
                None => {
                    at.insert(path.clone(), entries.len());
                    entries.push((path, e));
                }
            }
        }
    }
    let mut delta = Contrib::default();
    let mut scratch = Contrib::default();
    let home_s = home.to_string_lossy();
    let mut sink = Sink::new(&mut scratch, Some(&mut delta), &home_s, tz, local, now).with_git(git);
    let mut keys: HashSet<u64> = HashSet::new();
    let mut tokens = 0.0;
    for (_, e) in &entries {
        let (Some(agent), Some(recs)) = (e.agent, e.recs.as_ref()) else { continue };
        for r in recs {
            if let Some(k) = r.key {
                if !keys.insert(k) {
                    continue;
                }
            }
            let model = r.model.and_then(|m| models[e.cache].get(m));
            if model.is_some_and(|m| m.is("<synthetic>")) {
                continue;
            }
            let model = match model {
                Some(m) if m.truthy() => m.js_string().into_owned(),
                _ => String::new(),
            };
            if r.n.is_finite() {
                // T3 Code's cache says the output, not the rest of the split.
                let out = if r.out.is_finite() { r.out as i64 } else { 0 };
                sink.count(
                    r.ms,
                    agent.name(),
                    &model,
                    r.n as i64,
                    crate::state::Io { out, ..Default::default() },
                    None,
                );
                if r.ms.is_some() {
                    tokens += r.n;
                }
            }
        }
    }
    drop(sink);
    (delta, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_only_deleted_untracked_transcripts_once() {
        let d = crate::ingest::tests::tmpdir("t3");
        let ud = d.join(".t3").join("userdata");
        std::fs::create_dir_all(&ud).unwrap();
        let live = d.join("live.jsonl");
        std::fs::write(&live, "").unwrap();
        let live = live.to_string_lossy().into_owned();
        // 2026-08-25T03:09:44Z
        let rec = |key: &str, m: i64| format!(r#"[1787644184000,{m},0,1,2,3,4,0,"{key}",null,0,null]"#);
        let old = format!(
            r#"{{"version":4,"models":["claude-opus-5-5","<synthetic>"],"files":{{
              "/gone/a.jsonl":{{"p":"claude","r":[{}]}},
              "/gone/b.jsonl":{{"p":"claude","r":[{}]}}
            }}}}"#,
            rec("k1", 0),
            rec("k9", 0)
        );
        let v5 = format!(
            r#"{{"sources":[],"version":5,"models":["x","claude-opus-5-5","<synthetic>"],"sessions":{{}},"files":{{
              "{live}":{{"p":"claude","r":[{}]}},
              "/gone/b.jsonl":{{"p":"claude","r":[{},{},{}]}},
              "/gone/c.jsonl":{{"p":"codex","r":[{},null,"x"]}},
              "/gone/d.jsonl":{{"p":"cursor","r":[{}]}},
              "/tracked.jsonl":{{"p":"claude","r":[{}]}}
            }}}}"#,
            rec("k2", 1),
            rec("k1", 1),
            rec("k3", 1),
            rec("k4", 2),
            rec("k5", 0),
            rec("k6", 1),
            rec("k7", 1)
        );
        std::fs::write(ud.join("usage-scan-cache.json"), old).unwrap();
        std::fs::write(ud.join("usage-scan-cache-v5.json"), v5).unwrap();
        let tz = crate::ingest::tests::tokyo();
        let (delta, tokens) = import(&d, None, &|p| p == "/tracked.jsonl", &tz, &tz, 0.0);
        // a: k1 (opus). b (replaced by v5): k1 again (deduped), k3 (opus), k4 (synthetic, skipped). c: k5 (model "x").
        assert_eq!(delta.days["2026-08-25|claude|claude-opus-5-5"], crate::state::Totals { tokens: 20, output: 8 });
        assert_eq!(delta.days["2026-08-25|codex|x"], crate::state::Totals { tokens: 10, output: 4 });
        assert_eq!(delta.days.len(), 2);
        assert!(delta.repos.is_empty());
        assert_eq!(tokens, 30.0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
