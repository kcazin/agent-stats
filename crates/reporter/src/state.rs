//! Persistent state: daily totals, per-repo totals, per hub the keys it has not acknowledged yet ("dirty"), and per
//! transcript the read offset, parse state and what the file has contributed to each total.

use crate::js::{fnv64, JsVal};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Bump when counting changes; older state is then rebuilt from the transcripts.
pub const V: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Grok,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Grok => "grok",
        }
    }
    pub fn parse(s: &str) -> Option<Agent> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "grok" => Some(Agent::Grok),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Totals {
    pub tokens: i64,
    pub output: i64,
}

/// A session's running token split: fresh input, cache reads, cache writes, output.
#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Io {
    #[serde(rename = "in")]
    pub inp: i64,
    pub cr: i64,
    pub cw: i64,
    pub out: i64,
}

/// Token totals keyed like the global ones: "day|agent|model" and "day|repo".
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
pub struct Contrib {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub days: BTreeMap<String, Totals>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, i64>,
}

impl Contrib {
    pub fn is_empty(&self) -> bool {
        self.days.is_empty() && self.repos.is_empty()
    }
    pub fn add_day(&mut self, key: &str, tokens: i64, output: i64) {
        match self.days.get_mut(key) {
            Some(v) => {
                v.tokens += tokens;
                v.output += output;
            }
            None => {
                self.days.insert(key.to_owned(), Totals { tokens, output });
            }
        }
    }
    pub fn add_repo(&mut self, key: &str, tokens: i64) {
        match self.repos.get_mut(key) {
            Some(v) => *v += tokens,
            None => {
                self.repos.insert(key.to_owned(), tokens);
            }
        }
    }
}

pub fn js_num<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.fract() == 0.0 && v.abs() < 9.007e15 {
        s.serialize_i64(*v as i64)
    } else {
        s.serialize_f64(*v)
    }
}

pub fn js_num_opt<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => js_num(v, s),
        None => s.serialize_none(),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileState {
    pub agent: Agent,
    /// Bytes read, always just past a newline.
    pub offset: u64,
    /// `stat` mtime in ms, computed the way Node computes `mtimeMs`, so values migrated from the JS state compare.
    #[serde(serialize_with = "js_num")]
    pub mtime: f64,
    /// Inode when known; a different inode at the same path means the file was replaced.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ino: u64,
    /// Length and hash of the file's first bytes (up to 4 KiB), to notice a rewritten transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<(u64, u64)>,
    /// The last 64 message / prompt ids seen (hashed), to skip repeats of the same API response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<u64>,
    /// Claude: every message id counted so far with the usage counted for it, so a later line of the same streamed
    /// response only adds the growth. Each entry is `[id hash, bucket, input, cache write, cache read, output]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub msgs: Vec<Msg>,
    /// Claude: where the messages in `msgs` were counted (day, model, cwd), referenced by index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<Bucket>,
    /// Runtime only: id hash -> index in `msgs`, built on first use.
    #[serde(skip)]
    msg_idx: Option<HashMap<u64, u32>>,
    /// Codex: the session's running total at the last token_count event.
    #[serde(default, skip_serializing_if = "is_zero_i")]
    pub total: i64,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, rename = "stateAt", skip_serializing_if = "Option::is_none", serialize_with = "js_num_opt")]
    pub state_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<Io>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sub: bool,
    /// What this file has added to the global totals, so a rewritten file can be re-read without double counting.
    #[serde(default, skip_serializing_if = "Contrib::is_empty")]
    pub contrib: Contrib,
    /// Migrated from the JS state: `contrib` only covers what was read after the migration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub baseline: bool,

    /// Runtime only: the tick at which to stat this file next.
    #[serde(skip)]
    pub next_stat: u64,
    /// Runtime only: the size at the last read (a trailing partial line leaves it above `offset`).
    #[serde(skip)]
    pub seen_size: Option<u64>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}
fn is_zero_i(v: &i64) -> bool {
    *v == 0
}

pub const ID_WINDOW: usize = 64;

/// `[id hash, bucket, input, cache write, cache read, output]`.
pub type Msg = (u64, u32, i64, i64, i64, i64);

/// The usage of a Claude response: input, cache write, cache read, output.
pub type Usage4 = [i64; 4];

/// Where a message was counted: day key, model, cwd.
pub type Bucket = (String, String, Option<String>);

impl FileState {
    pub fn new(agent: Agent, cwd: Option<String>) -> Self {
        FileState {
            agent,
            offset: 0,
            mtime: 0.0,
            ino: 0,
            head: None,
            ids: Vec::new(),
            msgs: Vec::new(),
            buckets: Vec::new(),
            msg_idx: None,
            total: 0,
            tokens: 0,
            model: None,
            cwd,
            state: None,
            state_at: None,
            io: None,
            parent: None,
            sub: false,
            contrib: Contrib::default(),
            baseline: false,
            next_stat: 0,
            seen_size: None,
        }
    }

    /// `if (f.ids.includes(id)) return true; f.ids.push(id); if (f.ids.length > 64) f.ids.shift();`
    pub fn seen_id(&mut self, id: &JsVal) -> bool {
        let Some(h) = crate::js::id_hash(id) else { return false };
        if self.ids.contains(&h) {
            return true;
        }
        self.ids.push(h);
        if self.ids.len() > ID_WINDOW {
            self.ids.remove(0);
        }
        false
    }

    /// Whether `h` is a message id remembered from the JS state (the last 64 ids, with no usage recorded).
    pub fn migrated_id(&self, h: u64) -> bool {
        self.ids.contains(&h)
    }

    /// The usage already counted for message `h` and its bucket `(day, model, cwd)`.
    pub fn msg(&mut self, h: u64) -> Option<(Usage4, Bucket)> {
        if self.msg_idx.is_none() {
            self.msg_idx = Some(self.msgs.iter().enumerate().map(|(i, m)| (m.0, i as u32)).collect());
        }
        let i = *self.msg_idx.as_ref().unwrap().get(&h)? as usize;
        let m = &self.msgs[i];
        Some(([m.2, m.3, m.4, m.5], self.buckets[m.1 as usize].clone()))
    }

    /// Records message `h` as counted at `u` in `bucket`, or raises what is recorded to `u` (per field).
    pub fn set_msg(&mut self, h: u64, u: Usage4, bucket: &Bucket) {
        if self.msg_idx.is_none() {
            self.msg_idx = Some(self.msgs.iter().enumerate().map(|(i, m)| (m.0, i as u32)).collect());
        }
        if let Some(&i) = self.msg_idx.as_ref().unwrap().get(&h) {
            let m = &mut self.msgs[i as usize];
            m.2 = m.2.max(u[0]);
            m.3 = m.3.max(u[1]);
            m.4 = m.4.max(u[2]);
            m.5 = m.5.max(u[3]);
            return;
        }
        let b = match self.buckets.iter().position(|x| x == bucket) {
            Some(b) => b,
            None => {
                self.buckets.push(bucket.clone());
                self.buckets.len() - 1
            }
        };
        self.msg_idx.as_mut().unwrap().insert(h, self.msgs.len() as u32);
        self.msgs.push((h, b as u32, u[0], u[1], u[2], u[3]));
    }

    pub fn add_io(&mut self, input: i64, cache_read: i64, cache_write: i64, output: i64) {
        let v = self.io.get_or_insert_with(Io::default);
        v.inp += input.max(0);
        v.cr += cache_read.max(0);
        v.cw += cache_write.max(0);
        v.out += output.max(0);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct State {
    pub v: u32,
    /// The IANA zone the day keys were computed in.
    pub tz: String,
    /// When the T3 Code usage cache was imported (it is imported once).
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "js_num_opt")]
    pub t3: Option<f64>,
    #[serde(default)]
    pub days: BTreeMap<String, Totals>,
    #[serde(default)]
    pub repos: BTreeMap<String, i64>,
    /// Before several hubs: the one hub's unacknowledged keys. Moved into `hubs` at startup, never written again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub dirty: BTreeSet<String>,
    #[serde(default, rename = "dirtyRepos", skip_serializing_if = "BTreeSet::is_empty")]
    pub dirty_repos: BTreeSet<String>,
    /// Per hub: keys changed since that hub last acknowledged a report; kept until it does. Keyed by
    /// [`crate::config::Hub::state_key`]: `id:<hubId>` for hubs with an id, the name for id-less (legacy) hubs.
    /// State written before ids were keys has names only; [`State::sync_hubs`] moves those to the id keys.
    #[serde(default)]
    pub hubs: BTreeMap<String, Pending>,
    /// Repo name → its normalized git remote, for the repos that have one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remotes: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
}

impl State {
    pub fn new(tz: &str) -> Self {
        State {
            v: V,
            tz: tz.to_owned(),
            t3: None,
            days: BTreeMap::new(),
            repos: BTreeMap::new(),
            dirty: BTreeSet::new(),
            dirty_repos: BTreeSet::new(),
            hubs: BTreeMap::new(),
            remotes: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    /// Adds a file's (or the T3 import's) new tokens to the totals and marks the keys for every hub's next report.
    pub fn apply(&mut self, delta: &Contrib) {
        for (k, t) in &delta.days {
            if t.tokens == 0 && t.output == 0 {
                continue;
            }
            let v = self.days.entry(k.clone()).or_default();
            v.tokens += t.tokens;
            v.output += t.output;
            for p in self.hubs.values_mut() {
                if !p.dirty.contains(k) {
                    p.dirty.insert(k.clone());
                }
            }
        }
        for (k, t) in &delta.repos {
            if *t == 0 {
                continue;
            }
            *self.repos.entry(k.clone()).or_default() += t;
            for p in self.hubs.values_mut() {
                if !p.dirty_repos.contains(k) {
                    p.dirty_repos.insert(k.clone());
                }
            }
        }
    }

    /// Makes `hubs` match the configured hubs, each given as `(state key, name)`. A hub new to this state gets
    /// every key, so its first report carries the whole history; a hub no longer configured is forgotten.
    ///
    /// Older states: entries keyed by a hub's name (before ids were keys) move to that hub's id key. The first time
    /// hubs are tracked at all (state written before multi-hub, or migrated from the JS reporter), the old single
    /// dirty set goes to `inherit` only, the implicit legacy hub that was receiving reports then; every other hub
    /// starts with every key.
    pub fn sync_hubs(&mut self, hubs: &[(&str, &str)], inherit: Option<&str>) -> bool {
        let mut changed = false;
        let fresh = self.hubs.is_empty();
        let legacy = (std::mem::take(&mut self.dirty), std::mem::take(&mut self.dirty_repos));
        if !legacy.0.is_empty() || !legacy.1.is_empty() {
            changed = true;
        }
        let mut legacy = Some(legacy);
        for &(key, name) in hubs {
            if self.hubs.contains_key(key) {
                continue;
            }
            changed = true;
            // Name-keyed state of this hub (written before ids were keys), unless that name is now another
            // configured hub's key.
            if key != name && !hubs.iter().any(|(k, _)| *k == name) {
                if let Some(p) = self.hubs.remove(name) {
                    self.hubs.insert(key.to_owned(), p);
                    continue;
                }
            }
            let p = match legacy.take_if(|_| fresh && inherit == Some(key)) {
                Some((dirty, dirty_repos)) => Pending { dirty, dirty_repos },
                None => Pending {
                    dirty: self.days.keys().cloned().collect(),
                    dirty_repos: self.repos.keys().cloned().collect(),
                },
            };
            self.hubs.insert(key.to_owned(), p);
        }
        let before = self.hubs.len();
        self.hubs.retain(|k, _| hubs.iter().any(|(key, _)| key == k));
        changed || before != self.hubs.len()
    }
}

/// Keys one hub has not acknowledged yet.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Pending {
    #[serde(default)]
    pub dirty: BTreeSet<String>,
    #[serde(default, rename = "dirtyRepos")]
    pub dirty_repos: BTreeSet<String>,
}

impl Pending {
    pub fn len(&self) -> usize {
        self.dirty.len() + self.dirty_repos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub enum Loaded {
    State(State),
    /// The file exists but cannot be used; carries the reason.
    Unusable(String),
    Missing,
}

pub fn load(path: &Path) -> Loaded {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => return Loaded::Unusable(e.to_string()),
    };
    match serde_json::from_slice::<State>(&data) {
        Ok(s) if s.v == V => Loaded::State(s),
        Ok(s) => Loaded::Unusable(format!("state version {} (this reporter writes {V})", s.v)),
        Err(e) => Loaded::Unusable(e.to_string()),
    }
}

/// Writes the state atomically: a temporary file, flushed to disk, renamed over the old one. The temporary file
/// is per process, a second line of defence behind [`lock`].
pub fn save(state: &State, path: &Path) -> io::Result<usize> {
    let data = serde_json::to_vec(state).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let res = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.map(|_| data.len())
}

/// The lock file of every run that writes the state (`report`, `report --once`), in the state directory.
pub const LOCK_FILE: &str = "reporter.lock";

/// Takes the writer's advisory lock (`flock`) on `dir/reporter.lock` without waiting. `Ok(None)`: another
/// process holds it. The lock lasts as long as the returned file is open (it is released if the process dies).
pub fn lock(dir: &Path) -> io::Result<Option<fs::File>> {
    fs::create_dir_all(dir)?;
    let f = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join(LOCK_FILE))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
        }
    }
    Ok(Some(f))
}

// ---- migration from the JS reporter's state.json (v3) ----------------------------------------------------------

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}
fn int(v: &Value) -> i64 {
    v.as_f64().map(|f| f.round() as i64).unwrap_or(0)
}
fn string(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Builds Rust state from the JS reporter's state file, or `None` when it is missing or not version 3.
pub fn migrate_js(path: &Path, tz: &str) -> Result<Option<State>, String> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let js: Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    if js.get("v").and_then(Value::as_f64) != Some(3.0) {
        return Ok(None);
    }
    let mut s = State::new(tz);
    if let Some(t) = js.get("t3").and_then(Value::as_f64).filter(|t| *t != 0.0) {
        s.t3 = Some(t);
    }
    if let Some(days) = js.get("days").and_then(Value::as_object) {
        for (k, v) in days {
            s.days.insert(k.clone(), Totals { tokens: int(&v["tokens"]), output: int(&v["output"]) });
        }
    }
    if let Some(repos) = js.get("repos").and_then(Value::as_object) {
        for (k, v) in repos {
            s.repos.insert(k.clone(), int(v));
        }
    }
    if let Some(files) = js.get("files").and_then(Value::as_object) {
        for (path, f) in files {
            let Some(agent) = f.get("agent").and_then(Value::as_str).and_then(Agent::parse) else { continue };
            let mut fs = FileState::new(agent, string(&f["cwd"]));
            fs.offset = num(&f["offset"]).max(0.0) as u64;
            fs.mtime = num(&f["mtime"]);
            fs.total = int(&f["total"]);
            fs.tokens = int(&f["tokens"]);
            fs.model = string(&f["model"]);
            fs.state = string(&f["state"]);
            fs.state_at = f.get("stateAt").and_then(Value::as_f64).filter(|t| *t != 0.0);
            if let Some(io) = f.get("io").filter(|v| v.is_object()) {
                fs.io = Some(Io { inp: int(&io["in"]), cr: int(&io["cr"]), cw: int(&io["cw"]), out: int(&io["out"]) });
            }
            fs.parent = string(&f["parent"]);
            fs.sub = f.get("sub").map(|v| v.as_bool().unwrap_or(!v.is_null())).unwrap_or(false);
            if let Some(ids) = f.get("ids").and_then(Value::as_array) {
                for id in ids {
                    let h = match id {
                        Value::String(s) => Some(fnv64(&[b"s:", s.as_bytes()])),
                        Value::Number(n) => crate::js::id_hash(&JsVal::Num(n.as_f64().unwrap_or(f64::NAN))),
                        Value::Bool(b) => crate::js::id_hash(&JsVal::Bool(*b)),
                        Value::Null => crate::js::id_hash(&JsVal::Null),
                        _ => None,
                    };
                    if let Some(h) = h {
                        fs.ids.push(h);
                    }
                }
            }
            fs.baseline = true;
            s.files.insert(path.clone(), fs);
        }
    }
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_uses_a_temporary_file_of_its_own() {
        let d = crate::ingest::tests::tmpdir("statetmp");
        let path = d.join("reporter-state.json");
        // Another process's temporary file under the old fixed name is left alone.
        fs::write(d.join("reporter-state.json.tmp"), "other writer").unwrap();
        save(&State::new("UTC"), &path).unwrap();
        assert_eq!(fs::read_to_string(d.join("reporter-state.json.tmp")).unwrap(), "other writer");
        assert!(matches!(load(&path), Loaded::State(_)));
        assert!(!d.join(format!("reporter-state.json.tmp{}", std::process::id())).exists());
        // The lock is exclusive between open files, even within one process.
        let held = lock(&d).unwrap();
        assert!(held.is_some());
        assert!(lock(&d).unwrap().is_none());
        drop(held);
        assert!(lock(&d).unwrap().is_some());
        let _ = fs::remove_dir_all(&d);
    }
}
