//! The one snapshot the relay holds: raw JSON text in memory and in `<data>/stats.json`.

use agent_stats_common::{atomic_write, now_secs};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Doc {
    body: String,
    /// `updated` in seconds, when the document carries a numeric one (ms epochs are normalized).
    updated: Option<f64>,
    updated_raw: Option<Value>,
    /// Staleness reference when `updated` is missing: when we got (or loaded) the document.
    received: u64,
    /// Bumped on every stored publish (0 = loaded from disk); with the stale flag it identifies what `/stats` serves.
    generation: u64,
}

/// What `/stats` would serve right now: the document's generation and whether it is stale. Equal versions, same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub generation: u64,
    pub stale: bool,
}

pub struct Store {
    file: PathBuf,
    stale_after: u64,
    /// Read side: only ever locked for a pointer clone or swap, never across I/O.
    doc: Mutex<Option<Arc<Doc>>>,
    /// Serializes publishes, so the ordering check and the file write happen under one lock without blocking readers.
    /// Holds the last generation handed out.
    writer: Mutex<u64>,
}

pub enum Publish {
    Stored,
    IgnoredOlder,
    /// `updated` is further ahead of our clock than `MAX_FUTURE_SECS`: refused, nothing stored.
    Future,
    Persist(String),
}

/// Clock skew tolerated on `updated`; beyond it a document is "from the future".
const MAX_FUTURE_SECS: f64 = 300.0;

fn secs(v: &Value) -> Option<f64> {
    let n = v.as_f64()?;
    Some(if n > 1e11 { n / 1000.0 } else { n })
}

fn make_doc(body: String, obj: &serde_json::Map<String, Value>, received: u64) -> Doc {
    let updated_raw = obj.get("updated").filter(|v| v.is_number()).cloned();
    Doc { updated: updated_raw.as_ref().and_then(secs), updated_raw, body, received, generation: 0 }
}

/// A document that carries its own `stale` would collide with the injected one: drop it.
fn strip_stale(body: String, obj: &mut serde_json::Map<String, Value>) -> String {
    match obj.remove("stale") {
        Some(_) => serde_json::to_string(&obj).unwrap_or(body),
        None => body,
    }
}

impl Store {
    /// Loads `<data>/stats.json` if present (the old relay wrote the plain JSON body there).
    pub fn open(data: &std::path::Path, stale_after: u64) -> Store {
        let file = data.join("stats.json");
        let mut doc = None;
        if let Ok(text) = std::fs::read_to_string(&file) {
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(mut obj)) => {
                    let mtime = std::fs::metadata(&file)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or_else(now_secs);
                    let text = strip_stale(text, &mut obj);
                    doc = Some(Arc::new(make_doc(text, &obj, mtime)));
                }
                _ => eprintln!("relay: ignoring {}: not a JSON object", file.display()),
            }
        }
        Store { file, stale_after, doc: Mutex::new(doc), writer: Mutex::new(0) }
    }

    /// `body` must already be a parsed JSON object (`obj`).
    pub fn publish(&self, body: String, mut obj: serde_json::Map<String, Value>) -> Publish {
        let body = strip_stale(body, &mut obj);
        let now = now_secs();
        let mut new = make_doc(body, &obj, now);
        if new.updated.is_some_and(|n| n > now as f64 + MAX_FUTURE_SECS) {
            return Publish::Future;
        }
        let mut generation = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.doc.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // A stored doc dated from the future (clock skew, old file) must not block real publishes.
        let cur = current.as_ref().and_then(|d| d.updated).filter(|c| *c <= now as f64 + MAX_FUTURE_SECS);
        if let (Some(cur), Some(n)) = (cur, new.updated) {
            if n < cur {
                return Publish::IgnoredOlder;
            }
        }
        if let Err(e) = atomic_write(&self.file, new.body.as_bytes(), None) {
            return Publish::Persist(e.to_string());
        }
        *generation += 1;
        new.generation = *generation;
        *self.doc.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(new));
        Publish::Stored
    }

    /// The JSON to serve, with `"stale":true` spliced in when it is older than `stale_after`.
    pub fn snapshot(&self) -> Option<String> {
        self.versioned_snapshot().map(|(_, body)| body)
    }

    /// `snapshot()` together with the version it is; `version()` is the cheap check for "did it change".
    pub fn versioned_snapshot(&self) -> Option<(Version, String)> {
        let doc = self.current()?; // Arc clone; formatting is outside the lock
        let version = self.version_of(&doc);
        Some((version, if version.stale { inject_stale(&doc.body) } else { doc.body.clone() }))
    }

    pub fn version(&self) -> Option<Version> {
        self.current().map(|doc| self.version_of(&doc))
    }

    fn current(&self) -> Option<Arc<Doc>> {
        self.doc.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn version_of(&self, doc: &Doc) -> Version {
        let reference = doc.updated.unwrap_or(doc.received as f64);
        Version { generation: doc.generation, stale: now_secs() as f64 - reference > self.stale_after as f64 }
    }

    pub fn updated(&self) -> Value {
        let g = self.doc.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().and_then(|d| d.updated_raw.clone()).unwrap_or(Value::Null)
    }
}

/// Inserts `"stale":true` before the final `}` of a JSON object text, without re-serializing it.
fn inject_stale(body: &str) -> String {
    let trimmed = body.trim_end();
    let Some(close) = trimmed.rfind('}') else { return body.to_string() };
    let inner = &trimmed[..close];
    let empty = inner.trim_end().ends_with('{');
    format!("{}{}\"stale\":true}}", inner, if empty { "" } else { "," })
}

#[cfg(test)]
mod tests {
    use super::inject_stale;

    #[test]
    fn injects() {
        assert_eq!(inject_stale("{\"a\":1}\n"), "{\"a\":1,\"stale\":true}");
        assert_eq!(inject_stale("{ }"), "{ \"stale\":true}");
        let v: serde_json::Value = serde_json::from_str(&inject_stale("{\"a\":{\"b\":2}}")).unwrap();
        assert_eq!(v["stale"], true);
    }
}
