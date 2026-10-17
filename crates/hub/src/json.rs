//! A JSON value that behaves like a JavaScript object: keys keep insertion order (integer-like keys first, as
//! `Object.entries` orders them), numbers are f64 and print like `JSON.stringify`. The public payload must match the
//! JS hub byte for byte, so it is built and printed with this instead of serde_json's sorted map.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt::Write;

#[derive(Clone, Debug, PartialEq, Default)]
pub enum J {
    #[default]
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Obj),
}

/// An insertion-ordered object with JS key ordering.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Obj(pub Vec<(String, J)>);

/// Is `k` a canonical array index ("0", "17", not "01" or "4294967295")? JS objects list those first, ascending.
fn array_index(k: &str) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    k.parse::<u64>().ok().filter(|n| *n < u32::MAX as u64).map(|n| n as u32)
}

impl Obj {
    pub fn new() -> Obj {
        Obj(Vec::new())
    }
    pub fn get(&self, k: &str) -> Option<&J> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, k: &str) -> Option<&mut J> {
        self.0.iter_mut().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    /// `o[k] = v`: replaces in place if present, else inserts where JS would put it.
    pub fn set(&mut self, k: &str, v: J) {
        if let Some(slot) = self.get_mut(k) {
            *slot = v;
            return;
        }
        match array_index(k) {
            Some(n) => {
                let at =
                    self.0.iter().position(|(key, _)| array_index(key).is_none_or(|m| m > n)).unwrap_or(self.0.len());
                self.0.insert(at, (k.to_string(), v));
            }
            None => self.0.push((k.to_string(), v)),
        }
    }
    pub fn with(mut self, k: &str, v: impl Into<J>) -> Obj {
        self.set(k, v.into());
        self
    }
    pub fn remove(&mut self, k: &str) -> Option<J> {
        let i = self.0.iter().position(|(key, _)| key == k)?;
        Some(self.0.remove(i).1)
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

impl From<Obj> for J {
    fn from(o: Obj) -> J {
        J::Obj(o)
    }
}
impl From<f64> for J {
    fn from(n: f64) -> J {
        J::Num(n)
    }
}
impl From<i64> for J {
    fn from(n: i64) -> J {
        J::Num(n as f64)
    }
}
impl From<usize> for J {
    fn from(n: usize) -> J {
        J::Num(n as f64)
    }
}
impl From<bool> for J {
    fn from(b: bool) -> J {
        J::Bool(b)
    }
}
impl From<&str> for J {
    fn from(s: &str) -> J {
        J::Str(s.to_string())
    }
}
impl From<String> for J {
    fn from(s: String) -> J {
        J::Str(s)
    }
}
impl<T: Into<J>> From<Option<T>> for J {
    fn from(o: Option<T>) -> J {
        o.map_or(J::Null, Into::into)
    }
}
impl From<Vec<J>> for J {
    fn from(v: Vec<J>) -> J {
        J::Arr(v)
    }
}

impl J {
    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(o) => o.get(k),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            J::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            J::Obj(o) => Some(o),
            _ => None,
        }
    }
    /// JavaScript truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            J::Null => false,
            J::Bool(b) => *b,
            J::Num(n) => *n != 0.0 && !n.is_nan(),
            J::Str(s) => !s.is_empty(),
            J::Arr(_) | J::Obj(_) => true,
        }
    }

    /// `JSON.stringify(this)`.
    pub fn stringify(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(&js_number(*n)),
            J::Str(s) => write_str(s, out),
            J::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            J::Obj(o) => {
                out.push('{');
                for (i, (k, v)) in o.0.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    /// Parses JSON text, keeping key order.
    pub fn parse(s: &str) -> Result<J, serde_json::Error> {
        serde_json::from_str(s)
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `Number.prototype.toString()` as JSON.stringify uses it (NaN and ±Infinity become null).
pub fn js_number(n: f64) -> String {
    if !n.is_finite() {
        return "null".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    let a = n.abs();
    if (1e-6..1e21).contains(&a) {
        // Rust's Display is the shortest round-trip form without an exponent, like JS in this range.
        return format!("{n}");
    }
    // Exponent form: JS writes 1e+21, 1.5e-7.
    let s = format!("{n:e}");
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

/// `Math.round`: halves round up (towards +∞).
pub fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// Same JSON text? Like `==`, except numbers compare as printed (NaN and ±Infinity are all `null`, -0 is `0`).
fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Num(x), J::Num(y)) => x == y || js_number(*x) == js_number(*y),
        (J::Num(x), J::Null) | (J::Null, J::Num(x)) => !x.is_finite(),
        (J::Arr(x), J::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (J::Obj(x), J::Obj(y)) => {
            x.0.len() == y.0.len() && x.0.iter().zip(&y.0).all(|((k, v), (l, w))| k == l && same(v, w))
        }
        _ => a == b,
    }
}

/// The RFC 7386 merge patch from `from` to `to` (what the hub's `/stream` sends): objects are diffed recursively,
/// arrays and scalars replaced whole, a removed key becomes `null`. An empty object means "no change".
///
/// None when no merge patch can express the change: a patch can't set a member to `null` (that deletes it), so a
/// member that is or becomes `null`, or an object with a `null` member outside arrays, needs the full document.
pub fn merge_patch(from: &J, to: &J) -> Option<J> {
    match (from, to) {
        (J::Obj(a), J::Obj(b)) => diff_objects(a, b).map(J::Obj),
        _ if same(from, to) => Some(J::Obj(Obj::new())),
        (_, J::Obj(_)) => None,
        _ if mergeable(to) => Some(to.clone()),
        _ => None,
    }
}

fn diff_objects(a: &Obj, b: &Obj) -> Option<Obj> {
    let mut p = Obj::new();
    for (k, _) in a.0.iter().filter(|(k, _)| b.get(k).is_none()) {
        p.set(k, J::Null);
    }
    for (k, v) in &b.0 {
        match (a.get(k), v) {
            (Some(old), _) if same(old, v) => {}
            (Some(J::Obj(x)), J::Obj(y)) => {
                let d = diff_objects(x, y)?;
                if !d.0.is_empty() {
                    p.set(k, J::Obj(d));
                }
            }
            _ if mergeable(v) => p.set(k, v.clone()),
            _ => return None,
        }
    }
    Some(p)
}

/// Does merging `v` into an absent (or non-object) member give exactly `v`? Not if it holds a `null` outside arrays
/// (a non-finite number prints as `null` too).
fn mergeable(v: &J) -> bool {
    match v {
        J::Null => false,
        J::Num(n) => n.is_finite(),
        J::Obj(o) => o.0.iter().all(|(_, v)| mergeable(v)),
        _ => true,
    }
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = J;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<J, E> {
                Ok(J::Null)
            }
            fn visit_none<E>(self) -> Result<J, E> {
                Ok(J::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<J, E> {
                Ok(J::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<J, E> {
                Ok(J::Num(n as f64))
            }
            fn visit_u64<E>(self, n: u64) -> Result<J, E> {
                Ok(J::Num(n as f64))
            }
            fn visit_f64<E>(self, n: f64) -> Result<J, E> {
                Ok(J::Num(n))
            }
            fn visit_str<E>(self, s: &str) -> Result<J, E> {
                Ok(J::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<J, E> {
                Ok(J::Str(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element()? {
                    v.push(x);
                }
                Ok(J::Arr(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                // Same result as `Obj::set` per entry (a repeated key keeps its first slot, array-index keys come first,
                // ascending), but linear in the key count: `set` alone is quadratic on a hostile many-key object.
                let mut entries: Vec<(String, J)> = Vec::new();
                let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                while let Some((k, v)) = a.next_entry::<String, J>()? {
                    match seen.get(&k) {
                        Some(&i) => entries[i].1 = v,
                        None => {
                            seen.insert(k.clone(), entries.len());
                            entries.push((k, v));
                        }
                    }
                }
                let (mut indexed, rest): (Vec<_>, Vec<_>) =
                    entries.into_iter().partition(|(k, _)| array_index(k).is_some());
                indexed.sort_by_key(|(k, _)| array_index(k));
                indexed.extend(rest);
                Ok(J::Obj(Obj(indexed)))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_js() {
        assert_eq!(js_number(1e9), "1000000000");
        assert_eq!(js_number(5.0), "5");
        assert_eq!(js_number(-0.0), "0");
        assert_eq!(js_number(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(1.5e-7), "1.5e-7");
        assert_eq!(js_number(1e-6), "0.000001");
        assert_eq!(js_number(123e-20), "1.23e-18");
        assert_eq!(js_number(f64::NAN), "null");
        assert_eq!(js_number(277.77777777777777), "277.77777777777777");
    }

    #[test]
    fn round_like_js() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(0.49999999999999994), 0.0);
        assert_eq!(js_round(1234.4999), 1234.0);
    }

    #[test]
    fn key_order_like_js() {
        let o = Obj::new().with("b", 1i64).with("10", 2i64).with("a", 3i64).with("2", 4i64).with("b", 5i64);
        assert_eq!(J::Obj(o).stringify(), r#"{"2":4,"10":2,"b":5,"a":3}"#);
        let p = J::parse(r#"{"z":1,"a":{"y":[1,2.5,"x\u0001"],"b":null}}"#).unwrap();
        assert_eq!(p.stringify(), r#"{"z":1,"a":{"y":[1,2.5,"x\u0001"],"b":null}}"#);
    }

    #[test]
    fn parsed_objects_order_keys_like_set() {
        let text = r#"{"b":1,"10":2,"a":3,"2":4,"b":5,"01":6,"2":7,"4294967295":8,"0":9}"#;
        let mut expect = Obj::new();
        for (k, v) in
            [("b", 1), ("10", 2), ("a", 3), ("2", 4), ("b", 5), ("01", 6), ("2", 7), ("4294967295", 8), ("0", 9)]
        {
            expect.set(k, J::from(v as i64));
        }
        assert_eq!(J::parse(text).unwrap(), J::Obj(expect));
    }

    /// The client's merge rule (CONTRACT.md), on parsed values.
    fn apply(a: &J, p: &J) -> J {
        let J::Obj(p) = p else { return p.clone() };
        let mut o = match a {
            J::Obj(o) => o.clone(),
            _ => Obj::new(),
        };
        for (k, v) in &p.0 {
            if *v == J::Null {
                o.remove(k);
            } else {
                let cur = o.get(k).cloned().unwrap_or(J::Null);
                o.set(k, apply(&cur, v));
            }
        }
        J::Obj(o)
    }

    fn patch(a: &str, b: &str) -> Option<String> {
        merge_patch(&J::parse(a).unwrap(), &J::parse(b).unwrap()).map(|p| p.stringify())
    }

    #[test]
    fn merge_patch_diffs_objects_and_replaces_the_rest() {
        assert_eq!(patch(r#"{"a":1,"b":{"c":2,"d":3}}"#, r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap(), "{}");
        assert_eq!(
            patch(r#"{"a":1,"b":{"c":2,"d":3}}"#, r#"{"a":2,"b":{"c":2,"d":4}}"#).unwrap(),
            r#"{"a":2,"b":{"d":4}}"#
        );
        // a removed key, nested too, and one that was null
        assert_eq!(patch(r#"{"a":1,"b":{"c":2,"d":3}}"#, r#"{"b":{"c":2}}"#).unwrap(), r#"{"a":null,"b":{"d":null}}"#);
        assert_eq!(patch(r#"{"n":null,"a":1}"#, r#"{"a":1}"#).unwrap(), r#"{"n":null}"#);
        // arrays are replaced whole, nulls inside them kept
        assert_eq!(patch(r#"{"h":[1,2,null]}"#, r#"{"h":[1,null,null]}"#).unwrap(), r#"{"h":[1,null,null]}"#);
        assert_eq!(patch(r#"{"h":[1,null]}"#, r#"{"h":[1,null]}"#).unwrap(), "{}");
        assert_eq!(patch(r#"{"x":[{"a":1}]}"#, r#"{"x":[{"a":1,"b":null}]}"#).unwrap(), r#"{"x":[{"a":1,"b":null}]}"#);
        // type changes: object → scalar, scalar → object, object → array, null → object
        assert_eq!(patch(r#"{"p":{"t":1}}"#, r#"{"p":5}"#).unwrap(), r#"{"p":5}"#);
        assert_eq!(patch(r#"{"p":5}"#, r#"{"p":{"t":1}}"#).unwrap(), r#"{"p":{"t":1}}"#);
        assert_eq!(patch(r#"{"p":{"t":1}}"#, r#"{"p":[1]}"#).unwrap(), r#"{"p":[1]}"#);
        assert_eq!(patch(r#"{"pile":null}"#, r#"{"pile":{"tokens":1}}"#).unwrap(), r#"{"pile":{"tokens":1}}"#);
        // the stale flag: set, then cleared by removing it
        assert_eq!(patch(r#"{"updated":1}"#, r#"{"updated":1,"stale":true}"#).unwrap(), r#"{"stale":true}"#);
        assert_eq!(
            patch(r#"{"updated":1,"stale":true}"#, r#"{"updated":2}"#).unwrap(),
            r#"{"stale":null,"updated":2}"#
        );
        // a null that isn't in an array can't be said in a patch: the full document goes instead
        assert_eq!(patch(r#"{"pile":{"tokens":1}}"#, r#"{"pile":null}"#), None);
        assert_eq!(patch(r#"{"a":1}"#, r#"{"a":1,"n":null}"#), None);
        assert_eq!(patch(r#"{"a":1}"#, r#"{"a":{"b":null}}"#), None);
        assert_eq!(patch(r#"{"a":{"b":1}}"#, r#"{"a":{"b":null}}"#), None);
        assert_eq!(patch(r#"{"pile":null,"a":1}"#, r#"{"pile":null,"a":2}"#).unwrap(), r#"{"a":2}"#);
        // numbers compare as printed
        assert_eq!(merge_patch(&J::Num(f64::NAN), &J::Null).unwrap().stringify(), "{}");
        assert_eq!(merge_patch(&J::Num(-0.0), &J::Num(0.0)).unwrap().stringify(), "{}");
        assert_eq!(merge_patch(&J::Obj(Obj::new().with("a", 1i64)), &J::Obj(Obj::new().with("a", f64::NAN))), None);
        assert_eq!(patch("1", "2").unwrap(), "2");
    }

    #[test]
    fn applying_the_patch_gives_the_new_document() {
        let docs = [
            r#"{"updated":1,"stale":false,"live":{"working":1,"agents":[{"id":"a","parent":null,"io":{"in":1}}]},"pile":null,"projects":{"all":{"neon":5},"last":{"neon":1},"lastMs":15000}}"#,
            r#"{"updated":2,"stale":false,"live":{"working":0,"agents":[]},"pile":{"tokens":3},"projects":{"all":{"neon":6,"other":1}}}"#,
            r#"{"updated":3,"stale":true,"live":{"working":0,"agents":[]},"pile":{"tokens":3,"perHour":2},"projects":{"all":{"other":1},"last":{"other":0},"lastMs":1}}"#,
        ];
        for (i, a) in docs.iter().enumerate() {
            for b in &docs[i + 1..] {
                let (a, b) = (J::parse(a).unwrap(), J::parse(b).unwrap());
                let p = merge_patch(&a, &b).expect("expressible");
                assert_eq!(apply(&a, &p), b, "{} then {}", a.stringify(), p.stringify());
                assert!(p.get("updated").is_some(), "carries updated");
            }
        }
    }

    #[test]
    fn many_keys_parse_in_linear_time() {
        let n = 200_000;
        for numeric in [false, true] {
            let body: Vec<String> =
                (0..n).rev().map(|i| format!("\"{}{i}\":0", if numeric { "" } else { "k" })).collect();
            let text = format!("{{{}}}", body.join(","));
            let t0 = std::time::Instant::now();
            let J::Obj(o) = J::parse(&text).unwrap() else { panic!("not an object") };
            assert_eq!(o.0.len(), n);
            assert!(t0.elapsed() < std::time::Duration::from_secs(1), "took {:?}", t0.elapsed());
        }
    }
}
