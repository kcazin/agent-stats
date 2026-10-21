//! The per-agent counting rules, ported from reporter.mjs (claudeLine / codexLine / grokLine), as typed views over
//! one transcript line. Only the fields the rules read are materialised; everything else is skipped by serde.

use crate::git::GitIds;
use crate::js::{act_time, date_value, tool_kind_of, DayCache, Entries, JsVal, Lax};
use crate::state::{Contrib, FileState, Io, RepoTotals, Usage4};
use jiff::tz::TimeZone;
use serde::de::{Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;
use std::marker::PhantomData;

/// The state of a session whose turn has ended: it waits for the user's next prompt.
pub const IDLE: &str = "idle";

/// Where counted tokens go: the file's own contribution record, and (unless a rewritten file is being rebuilt)
/// the delta to add to the global totals.
pub struct Sink<'s> {
    pub contrib: &'s mut Contrib,
    pub delta: Option<&'s mut Contrib>,
    pub home: &'s str,
    /// Repository identity from git; `None` keeps the folder rules only.
    pub git: Option<&'s GitIds>,
    pub local: &'s TimeZone,
    pub now: f64,
    days: DayCache,
    repo_cache: Option<(String, String)>,
    key: String,
}

impl<'s> Sink<'s> {
    pub fn new(
        contrib: &'s mut Contrib,
        delta: Option<&'s mut Contrib>,
        home: &'s str,
        tz: &TimeZone,
        local: &'s TimeZone,
        now: f64,
    ) -> Self {
        Sink {
            contrib,
            delta,
            home,
            git: None,
            local,
            now,
            days: DayCache::new(tz.clone()),
            repo_cache: None,
            key: String::new(),
        }
    }

    pub fn with_git(mut self, git: Option<&'s GitIds>) -> Self {
        self.git = git;
        self
    }

    /// `count(day(ts), agent, model, tokens, output, cwd)`, with the tokens' split by kind (`io`, its `out` the
    /// output) as far as the agent said. `day_ms` is `None` for an invalid date, where the JS reporter throws and
    /// stops reading the file; here the line just isn't counted.
    pub fn count(&mut self, day_ms: Option<f64>, agent: &str, model: &str, tokens: i64, io: Io, cwd: Option<&str>) {
        let Some(ms) = day_ms else { return };
        let Some(day) = self.days.day(ms) else { return };
        self.key.clear();
        self.key.push_str(day);
        self.count_keyed(agent, model, tokens, io, cwd);
    }

    /// `count` for a day that is already a key such as "2026-10-12".
    pub fn count_day(&mut self, day: &str, agent: &str, model: &str, tokens: i64, io: Io, cwd: Option<&str>) {
        self.key.clear();
        self.key.push_str(day);
        self.count_keyed(agent, model, tokens, io, cwd);
    }

    /// The day key of a timestamp in ms, `None` when invalid.
    pub fn day_key(&mut self, day_ms: Option<f64>) -> Option<String> {
        self.days.day(day_ms?).map(str::to_owned)
    }

    /// `self.key` holds the day.
    fn count_keyed(&mut self, agent: &str, model: &str, tokens: i64, io: Io, cwd: Option<&str>) {
        if tokens == 0 {
            return;
        }
        let output = io.out;
        let model = if model.is_empty() { "unknown" } else { model };
        let day_len = self.key.len();
        self.key.push('|');
        self.key.push_str(agent);
        self.key.push('|');
        self.key.push_str(model);
        self.contrib.add_day(&self.key, tokens, output);
        if let Some(d) = self.delta.as_deref_mut() {
            d.add_day(&self.key, tokens, output);
        }
        let Some(cwd) = cwd else { return };
        if self.repo_cache.as_ref().map(|(c, _)| c.as_str()) != Some(cwd) {
            self.repo_cache = Some((cwd.to_owned(), crate::report::repo_name(cwd, self.home, self.git)));
        }
        let repo = &self.repo_cache.as_ref().unwrap().1;
        self.key.truncate(day_len);
        self.key.push('|');
        self.key.push_str(repo);
        let v = RepoTotals { tokens, io };
        self.contrib.add_repo(&self.key, v);
        if let Some(d) = self.delta.as_deref_mut() {
            d.add_repo(&self.key, v);
        }
    }

    fn date(&self, v: &JsVal) -> Option<f64> {
        date_value(v, self.local)
    }

    fn act(&self, f: &mut FileState, state: &str, ts: &JsVal) {
        if f.state.as_deref() != Some(state) {
            f.state = Some(state.to_owned());
        }
        f.state_at = Some(act_time(ts, self.local, self.now));
    }
}

fn model_key<'a>(v: &'a JsVal) -> std::borrow::Cow<'a, str> {
    if v.truthy() {
        v.js_string()
    } else {
        "unknown".into()
    }
}

// ---- Claude Code ------------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CLine<'a> {
    #[serde(rename = "type", borrow)]
    ty: JsVal<'a>,
    #[serde(borrow)]
    cwd: JsVal<'a>,
    #[serde(borrow)]
    timestamp: JsVal<'a>,
    #[serde(borrow)]
    message: Lax<CMsg<'a>>,
    /// Lines Claude Code adds for the model (caveats, a command's output, a compaction's summary), not the user's.
    #[serde(rename = "isMeta", borrow)]
    is_meta: JsVal<'a>,
    #[serde(rename = "isCompactSummary", borrow)]
    is_compact_summary: JsVal<'a>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CMsg<'a> {
    #[serde(borrow)]
    id: JsVal<'a>,
    #[serde(borrow)]
    model: JsVal<'a>,
    #[serde(borrow)]
    usage: Lax<CUsage<'a>>,
    #[serde(borrow)]
    content: Content<'a>,
    #[serde(borrow)]
    stop_reason: JsVal<'a>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CUsage<'a> {
    #[serde(borrow)]
    input_tokens: JsVal<'a>,
    #[serde(borrow)]
    cache_creation_input_tokens: JsVal<'a>,
    #[serde(borrow)]
    cache_read_input_tokens: JsVal<'a>,
    #[serde(borrow)]
    output_tokens: JsVal<'a>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block<'a> {
    #[serde(rename = "type", borrow)]
    ty: JsVal<'a>,
    #[serde(borrow)]
    name: JsVal<'a>,
    #[serde(borrow)]
    text: JsVal<'a>,
}

/// What a user line's text says: a prompt (a turn starts), a command or other markup Claude Code wrote (`<...>`),
/// or that the user interrupted the turn.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Said {
    Prompt,
    Markup,
    Interrupt,
}

impl Said {
    fn of(text: &str) -> Said {
        if text.contains("[Request interrupted by user") {
            Said::Interrupt
        } else if text.trim_start().starts_with('<') {
            Said::Markup
        } else {
            Said::Prompt
        }
    }
    /// Text blocks in order: an interruption anywhere wins, else the first block says what the line is.
    fn add(prev: Option<Said>, text: &str) -> Option<Said> {
        let s = Said::of(text);
        match prev {
            Some(Said::Interrupt) => prev,
            Some(_) if s != Said::Interrupt => prev,
            _ => Some(s),
        }
    }
}

/// `message.content` summarised: how many blocks, the last one, whether any is a tool result, and what its text
/// says. That is all the state rules need (each block overwrites the state, so only the last one shows).
#[derive(Default)]
struct Content<'a> {
    len: usize,
    last: Block<'a>,
    tool_result: bool,
    said: Option<Said>,
}

struct ContentVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for ContentVisitor<'a> {
    type Value = Content<'a>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut c = Content::default();
        while let Some(b) = seq.next_element::<Lax<Block<'a>>>()? {
            let b = b.0.unwrap_or_default();
            c.len += 1;
            if b.ty.is("tool_result") {
                c.tool_result = true;
            }
            if b.ty.is("text") {
                if let Some(t) = b.text.as_str() {
                    c.said = Said::add(c.said, t);
                }
            }
            c.last = b;
        }
        Ok(c)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Content::default())
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    /// A prompt given as a plain string.
    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
        Ok(Content { len: 1, said: Some(Said::of(s)), ..Content::default() })
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Content::default())
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Content<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ContentVisitor(PhantomData))
    }
}

impl<'a> CLine<'a> {
    /// The state this line leaves the session in, if it sets one. A response that stopped for any reason but a tool
    /// call ends the turn (`end_turn`, or the `stop_sequence` of an API error Claude Code writes), and so does the
    /// user interrupting it: the session is `idle`. A prompt starts a turn.
    fn act(&self) -> Option<&'static str> {
        let m = self.message.get()?;
        let c = &m.content;
        if c.len == 0 {
            return None;
        }
        if self.ty.is("assistant") {
            let b = &c.last;
            Some(if m.stop_reason.as_str().is_some_and(|r| r != "tool_use" && r != "pause_turn") {
                IDLE
            } else if b.ty.is("thinking") || b.ty.is("redacted_thinking") {
                "thinking"
            } else if b.ty.is("tool_use") {
                tool_kind_of(&b.name)
            } else {
                "writing"
            })
        } else if c.tool_result {
            Some("thinking")
        } else if !self.ty.is("user") || self.is_meta.truthy() || self.is_compact_summary.truthy() {
            None
        } else {
            match c.said {
                Some(Said::Interrupt) => Some(IDLE),
                Some(Said::Prompt) => Some("thinking"),
                _ => None,
            }
        }
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.nonempty_str()
    }

    pub fn apply_act(&self, f: &mut FileState, sink: &Sink) -> bool {
        match self.act() {
            Some(s) => {
                sink.act(f, s, &self.timestamp);
                true
            }
            None => false,
        }
    }

    /// Whether this line counts tokens (`r.type === 'assistant' && m.usage && r.timestamp`).
    pub fn counts(&self) -> bool {
        self.ty.is("assistant")
            && self.timestamp.truthy()
            && self.message.get().is_some_and(|m| m.usage.get().is_some())
    }

    /// The token-counting half of `claudeLine`; cwd and state are applied by the caller.
    pub fn apply_usage(&self, f: &mut FileState, sink: &mut Sink) {
        if !self.counts() {
            return;
        }
        let m = self.message.get().unwrap();
        let u = m.usage.get().unwrap();
        if m.model.truthy() && !m.model.is("<synthetic>") {
            let model = m.model.js_string();
            if f.model.as_deref() != Some(&*model) {
                f.model = Some(model.into_owned());
            }
        }
        let cur: Usage4 = [
            u.input_tokens.num_or0(),
            u.cache_creation_input_tokens.num_or0(),
            u.cache_read_input_tokens.num_or0(),
            u.output_tokens.num_or0(),
        ];
        // A response streamed over several lines repeats its id with a growing usage: each id counts at its
        // largest usage, the growth going to the day, model and repo of its first line. A line without an id
        // is its own response.
        let id = if m.id.nullish() { None } else { crate::js::id_hash(&m.id) };
        let Some(h) = id else {
            let day = sink.date(&self.timestamp);
            add_claude(f, sink, day, &model_key(&m.model), cur, f.cwd.clone().as_deref());
            return;
        };
        if let Some((prev, (day, model, cwd))) = f.msg(h) {
            let d: Usage4 = std::array::from_fn(|k| (cur[k] - prev[k]).max(0));
            f.set_msg(h, cur, &(day.clone(), model.clone(), cwd.clone()));
            if d != [0; 4] {
                add_claude_day(f, sink, &day, &model, d, cwd.as_deref());
            }
            return;
        }
        if f.migrated_id(h) {
            return; // counted before the state was migrated, with no usage recorded
        }
        let Some(day) = sink.day_key(sink.date(&self.timestamp)) else { return };
        let model = model_key(&m.model).into_owned();
        let bucket = (day, model, f.cwd.clone());
        f.set_msg(h, cur, &bucket);
        add_claude_day(f, sink, &bucket.0, &bucket.1, cur, bucket.2.as_deref());
    }
}

/// A Claude usage (input, cache write, cache read, output) as a split.
fn claude_io(u: Usage4) -> Io {
    Io { inp: u[0], cr: u[2], cw: u[1], out: u[3] }
}

fn add_claude(f: &mut FileState, sink: &mut Sink, day: Option<f64>, model: &str, u: Usage4, cwd: Option<&str>) {
    let tokens: i64 = u.iter().sum();
    f.tokens += tokens;
    f.add_io(u[0], u[2], u[1], u[3]);
    sink.count(day, "claude", model, tokens, claude_io(u), cwd);
}

fn add_claude_day(f: &mut FileState, sink: &mut Sink, day: &str, model: &str, u: Usage4, cwd: Option<&str>) {
    let tokens: i64 = u.iter().sum();
    f.tokens += tokens;
    f.add_io(u[0], u[2], u[1], u[3]);
    sink.count_day(day, "claude", model, tokens, claude_io(u), cwd);
}

// ---- Codex ------------------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct XLine<'a> {
    #[serde(rename = "type", borrow)]
    ty: JsVal<'a>,
    #[serde(borrow)]
    timestamp: JsVal<'a>,
    #[serde(borrow)]
    payload: Lax<XPayload<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct XPayload<'a> {
    #[serde(rename = "type", borrow)]
    ty: JsVal<'a>,
    #[serde(borrow)]
    cwd: JsVal<'a>,
    #[serde(borrow)]
    parent_thread_id: JsVal<'a>,
    #[serde(borrow)]
    model: JsVal<'a>,
    #[serde(borrow)]
    name: JsVal<'a>,
    #[serde(borrow)]
    role: JsVal<'a>,
    #[serde(borrow)]
    info: Lax<XInfo<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct XInfo<'a> {
    #[serde(borrow)]
    total_token_usage: Lax<XUsage<'a>>,
    #[serde(borrow)]
    last_token_usage: Lax<XUsage<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct XUsage<'a> {
    #[serde(borrow)]
    total_tokens: JsVal<'a>,
    #[serde(borrow)]
    input_tokens: JsVal<'a>,
    #[serde(borrow)]
    output_tokens: JsVal<'a>,
    #[serde(borrow)]
    cached_input_tokens: JsVal<'a>,
    #[serde(borrow)]
    cache_write_input_tokens: JsVal<'a>,
}

impl XUsage<'_> {
    fn sum(&self) -> i64 {
        if self.total_tokens.nullish() {
            self.input_tokens.num_or0() + self.output_tokens.num_or0()
        } else {
            self.total_tokens.num_or0()
        }
    }
}

impl<'a> XLine<'a> {
    /// A turn runs from `task_started` to `task_complete`, or `turn_aborted` when the user stops it.
    pub fn act(&self) -> Option<&'static str> {
        let p = self.payload.get()?;
        if !self.timestamp.truthy() {
            return None;
        }
        if self.ty.is("event_msg") {
            return if p.ty.is("task_started") {
                Some("thinking")
            } else if p.ty.is("task_complete") || p.ty.is("turn_aborted") {
                Some(IDLE)
            } else {
                None
            };
        }
        if !self.ty.is("response_item") {
            return None;
        }
        let pt = p.ty.js_string();
        if p.ty.is("reasoning") || pt.ends_with("call_output") {
            Some("thinking")
        } else if pt.ends_with("call") {
            Some(tool_kind_of(if p.name.truthy() { &p.name } else { &p.ty }))
        } else if (p.ty.is("message") && p.role.is("assistant")) || p.ty.is("agent_message") {
            Some("writing")
        } else {
            None
        }
    }

    pub fn apply_act(&self, f: &mut FileState, sink: &Sink) -> bool {
        match self.act() {
            Some(s) => {
                sink.act(f, s, &self.timestamp);
                true
            }
            None => false,
        }
    }

    /// `codexLine`.
    pub fn apply(&self, f: &mut FileState, sink: &mut Sink) {
        let Some(p) = self.payload.get() else { return };
        let meta = self.ty.is("session_meta");
        let turn = self.ty.is("turn_context");
        if meta || turn {
            if let Some(cwd) = p.cwd.nonempty_str() {
                if f.cwd.as_deref() != Some(cwd) {
                    f.cwd = Some(cwd.to_owned());
                }
            }
        }
        // A Codex subagent names the thread that started it.
        if meta && p.parent_thread_id.truthy() {
            f.parent = p.parent_thread_id.as_str().map(str::to_owned);
            f.sub = true;
        }
        if turn && p.model.truthy() {
            f.model = Some(p.model.js_string().into_owned());
        }
        self.apply_act(f, sink);
        if !p.ty.is("token_count") || !self.timestamp.truthy() {
            return;
        }
        let Some(i) = p.info.get() else { return };
        let Some(t) = i.total_token_usage.get() else { return };
        let total = t.sum();
        if total == f.total {
            return; // the same event, repeated
        }
        let last = i.last_token_usage.get();
        let tokens = match last {
            Some(l) => l.sum(),
            // A fork starts from its parent's running total: that first total is the baseline, not usage.
            None if f.sub && f.total == 0 => 0,
            None => (total - f.total).max(0),
        };
        f.total = total;
        f.tokens += tokens;
        // Without the last turn's usage only the total is known. The input counts the cache reads and writes too.
        let io = last.map_or_else(Io::default, |l| {
            let (cr, cw) = (l.cached_input_tokens.num_or0(), l.cache_write_input_tokens.num_or0());
            Io { inp: (l.input_tokens.num_or0() - cr - cw).max(0), cr, cw, out: l.output_tokens.num_or0() }
        });
        if last.is_some() {
            f.add_io(io.inp, io.cr, io.cw, io.out);
        }
        let day = sink.date(&self.timestamp);
        let model = f.model.clone().unwrap_or_default();
        sink.count(day, "codex", &model, tokens, io, f.cwd.as_deref());
    }
}

// ---- Grok -------------------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct GLine<'a> {
    #[serde(borrow)]
    timestamp: JsVal<'a>,
    #[serde(borrow)]
    params: Lax<GParams<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GParams<'a> {
    #[serde(borrow)]
    update: Lax<GUpdate<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GUpdate<'a> {
    #[serde(rename = "sessionUpdate", borrow)]
    su: JsVal<'a>,
    #[serde(rename = "_meta", borrow)]
    meta: Lax<GMeta<'a>>,
    #[serde(borrow)]
    kind: JsVal<'a>,
    #[serde(borrow)]
    title: JsVal<'a>,
    #[serde(borrow)]
    status: JsVal<'a>,
    #[serde(borrow)]
    prompt_id: JsVal<'a>,
    #[serde(borrow)]
    usage: Lax<GUsage<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GMeta<'a> {
    #[serde(rename = "agentTimestampMs", borrow)]
    agent_ts: JsVal<'a>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct GNums<'a> {
    #[serde(rename = "inputTokens", borrow)]
    input: JsVal<'a>,
    #[serde(rename = "outputTokens", borrow)]
    output: JsVal<'a>,
    #[serde(rename = "totalTokens", borrow)]
    total: JsVal<'a>,
    #[serde(rename = "cachedReadTokens", borrow)]
    cached_read: JsVal<'a>,
    #[serde(rename = "cacheCreationTokens", borrow)]
    cache_creation: JsVal<'a>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GUsage<'a> {
    #[serde(rename = "inputTokens", borrow)]
    input: JsVal<'a>,
    #[serde(rename = "outputTokens", borrow)]
    output: JsVal<'a>,
    #[serde(rename = "totalTokens", borrow)]
    total: JsVal<'a>,
    #[serde(rename = "cachedReadTokens", borrow)]
    cached_read: JsVal<'a>,
    #[serde(rename = "cacheCreationTokens", borrow)]
    cache_creation: JsVal<'a>,
    #[serde(rename = "modelUsage", borrow)]
    model_usage: Entries<'a, Lax<GNums<'a>>>,
}

impl<'a> GLine<'a> {
    fn update(&self) -> Option<&GUpdate<'a>> {
        self.params.get()?.update.get()
    }

    /// A turn runs from the user's message to `turn_completed`.
    pub fn act(&self) -> Option<&'static str> {
        let u = self.update()?;
        if u.su.is("turn_completed") {
            Some(IDLE)
        } else if u.su.is("user_message_chunk") || u.su.is("agent_thought_chunk") {
            Some("thinking")
        } else if u.su.is("agent_message_chunk") {
            Some("writing")
        } else if u.su.is("tool_call") {
            Some(tool_kind_of(if u.kind.truthy() { &u.kind } else { &u.title }))
        } else if u.su.is("tool_call_update") && u.status.is("completed") {
            Some("thinking")
        } else {
            None
        }
    }

    pub fn apply_act(&self, f: &mut FileState, sink: &Sink) -> bool {
        let Some(s) = self.act() else { return false };
        let u = self.update().unwrap();
        // u._meta.agentTimestampMs ?? r.timestamp * 1000
        let at = match u.meta.get().map(|m| &m.agent_ts) {
            Some(v) if !v.nullish() => v.clone(),
            _ => JsVal::Num(self.timestamp.to_number() * 1000.0),
        };
        sink.act(f, s, &at);
        true
    }

    /// `grokLine`: each finished turn reports its usage per model.
    pub fn apply(&self, f: &mut FileState, sink: &mut Sink) {
        self.apply_act(f, sink);
        let Some(u) = self.update() else { return };
        if !u.su.is("turn_completed") || !self.timestamp.truthy() {
            return;
        }
        let Some(usage) = u.usage.get() else { return };
        if u.prompt_id.truthy() && f.seen_id(&u.prompt_id) {
            return;
        }
        let own = GNums {
            input: usage.input.clone(),
            output: usage.output.clone(),
            total: usage.total.clone(),
            cached_read: usage.cached_read.clone(),
            cache_creation: usage.cache_creation.clone(),
        };
        let entries: Vec<(String, GNums)> = match &usage.model_usage.0 {
            Some(e) => e.iter().map(|(k, v)| (k.to_string(), v.0.clone().unwrap_or_default())).collect(),
            None => vec![(f.model.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into()), own)],
        };
        let day = sink.date(&JsVal::Num(self.timestamp.to_number() * 1000.0));
        for (model, m) in entries {
            let tokens = if m.total.nullish() { m.input.num_or0() + m.output.num_or0() } else { m.total.num_or0() };
            f.model = Some(model.clone());
            f.tokens += tokens;
            // The input counts the cache reads and writes too (the total is input and output).
            let (cr, cw) = (m.cached_read.num_or0(), m.cache_creation.num_or0());
            let io = Io { inp: (m.input.num_or0() - cr - cw).max(0), cr, cw, out: m.output.num_or0() };
            f.add_io(io.inp, io.cr, io.cw, io.out);
            sink.count(day, "grok", &model, tokens, io, f.cwd.as_deref());
        }
    }
}
