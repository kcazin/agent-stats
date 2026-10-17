//! RFC 7386 JSON merge patches, generated (the relay sends them on `GET /stream`; clients apply them).

use serde_json::{Map, Value};

/// The merge patch that turns `old` into `new`: objects are diffed recursively, arrays and scalars replaced whole, a
/// removed key becomes `null`. An empty object means "no change".
///
/// `None` when no merge patch can express the change: a merge patch cannot set a member to `null` (that deletes it),
/// so a changed member that is `null`, or an object with a `null` member outside arrays, needs a full document instead.
pub fn diff(old: &Value, new: &Value) -> Option<Value> {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => diff_objects(a, b).map(Value::Object),
        _ if old == new => Some(Value::Object(Map::new())),
        _ if mergeable(new) && !new.is_object() => Some(new.clone()),
        // A non-object document root replaced by an object (or anything holding a null): only a full document will do.
        _ => None,
    }
}

fn diff_objects(a: &Map<String, Value>, b: &Map<String, Value>) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for k in a.keys().filter(|k| !b.contains_key(*k)) {
        out.insert(k.clone(), Value::Null);
    }
    for (k, nv) in b {
        match a.get(k) {
            Some(ov) if ov == nv => {}
            Some(Value::Object(oa)) if nv.is_object() => {
                out.insert(k.clone(), Value::Object(diff_objects(oa, nv.as_object()?)?));
            }
            _ if mergeable(nv) => {
                out.insert(k.clone(), nv.clone());
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Whether merging `v` into a member that is absent (or not an object) yields exactly `v`: no `null` anywhere except
/// inside arrays, which are replaced whole.
fn mergeable(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(o) => o.values().all(mergeable),
        _ => true,
    }
}

/// Applies a merge patch (RFC 7386 section 2); the client's rule. Test-only: the relay never applies patches.
#[cfg(test)]
pub fn apply(target: &Value, patch: &Value) -> Value {
    let Value::Object(p) = patch else { return patch.clone() };
    let mut out = match target {
        Value::Object(t) => t.clone(),
        _ => Map::new(),
    };
    for (k, v) in p {
        if v.is_null() {
            out.remove(k);
        } else {
            let merged = apply(out.get(k).unwrap_or(&Value::Null), v);
            out.insert(k.clone(), merged);
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(old: Value, new: Value) -> Value {
        let p = diff(&old, &new).expect("expressible");
        assert_eq!(apply(&old, &p), new, "patch {p}");
        p
    }

    #[test]
    fn rfc7386_appendix_a_cases_roundtrip() {
        // (target, result) pairs from RFC 7386 appendix A whose results contain no nulls.
        let cases = [
            (json!({"a":"b"}), json!({"a":"c"})),
            (json!({"a":"b"}), json!({"a":"b","b":"c"})),
            (json!({"a":"b"}), json!({})),
            (json!({"a":"b","b":"c"}), json!({"b":"c"})),
            (json!({"a":["b"]}), json!({"a":"c"})),
            (json!({"a":"c"}), json!({"a":["b"]})),
            (json!({"a":{"b":"c"}}), json!({"a":{"b":"d"}})),
            (json!({"a":[{"b":"c"}]}), json!({"a":[1]})),
            (json!({"e":null}), json!({"e":null,"a":1})),
            (json!({}), json!({"a":{"bb":{}}})),
        ];
        for (old, new) in cases {
            roundtrip(old, new);
        }
    }

    #[test]
    fn shapes() {
        assert_eq!(roundtrip(json!({"a":1}), json!({"a":1})), json!({}));
        assert_eq!(roundtrip(json!({"a":1,"b":2}), json!({"b":2})), json!({"a":null}));
        assert_eq!(
            roundtrip(json!({"u":1,"o":{"x":1,"y":[1,2],"z":{"k":true}}}), json!({"u":2,"o":{"x":1,"y":[1,3],"z":{}}})),
            json!({"u":2,"o":{"y":[1,3],"z":{"k":null}}})
        );
        // arrays are replaced whole, nulls inside them are fine
        assert_eq!(roundtrip(json!({"a":[1,2,3]}), json!({"a":[1,null]})), json!({"a":[1,null]}));
        // scalar to object and back
        assert_eq!(roundtrip(json!({"a":1}), json!({"a":{"b":1}})), json!({"a":{"b":1}}));
        assert_eq!(roundtrip(json!({"a":{"b":1}}), json!({"a":1})), json!({"a":1}));
        // the stale flag the relay injects
        assert_eq!(roundtrip(json!({"updated":1}), json!({"updated":1,"stale":true})), json!({"stale":true}));
        assert_eq!(
            roundtrip(json!({"updated":1,"stale":true}), json!({"updated":2})),
            json!({"updated":2,"stale":null})
        );
    }

    #[test]
    fn unchanged_nulls_are_fine_new_nulls_are_not_expressible() {
        assert_eq!(roundtrip(json!({"a":null,"b":1}), json!({"a":null,"b":2})), json!({"b":2}));
        assert_eq!(diff(&json!({"a":1}), &json!({"a":null})), None);
        assert_eq!(diff(&json!({}), &json!({"a":null})), None);
        assert_eq!(diff(&json!({"a":1}), &json!({"a":{"b":null}})), None);
        assert_eq!(diff(&json!({"o":{"x":1}}), &json!({"o":{"x":null}})), None);
        // a null already there, under an object that changes elsewhere, is fine
        assert_eq!(roundtrip(json!({"o":{"x":null,"y":1}}), json!({"o":{"x":null,"y":2}})), json!({"o":{"y":2}}));
    }

    #[test]
    fn non_object_roots() {
        assert_eq!(diff(&json!(1), &json!(2)), Some(json!(2)));
        assert_eq!(diff(&json!([1]), &json!([1])), Some(json!({})));
        assert_eq!(diff(&json!(1), &json!({"a":1})), None);
    }
}
