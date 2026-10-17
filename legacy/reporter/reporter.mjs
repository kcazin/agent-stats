// Agent stats reporter. Runs on every machine, reads Claude Code, Codex and Grok transcripts incrementally,
// and every TICK reports daily token totals and live sessions to the hub on the Mac mini.
//
//   HUB_URL=http://100.x.y.z:7777 node reporter.mjs      # run forever
//   node reporter.mjs --once --dry                        # one scan, print, don't send
//
// The hub token is read from ~/.config/agent-stats/hub_token unless HUB_TOKEN is set.
// Daily totals are absolute (not deltas), so a lost or repeated report changes nothing.

import { readFile, writeFile, mkdir, readdir, stat, open, rename } from 'node:fs/promises';
import { homedir, hostname } from 'node:os';
import { join, dirname, basename, sep } from 'node:path';

const HOME = homedir();
const HUB_URL = process.env.HUB_URL || 'http://mac-mini:7777';
const HUB_TOKEN = process.env.HUB_TOKEN || await readFile(join(HOME, '.config', 'agent-stats', 'hub_token'), 'utf8').then(t => t.trim(), () => '');
const MACHINE = process.env.STATS_MACHINE || hostname().replace(/\.local$/, '');
const LABEL = process.env.STATS_LABEL || MACHINE;
const TICK = 15_000;
const FULL_SYNC = 10 * 60_000;       // resend every day's totals this often, in case the hub lost them
const OPEN_WINDOW = 30 * 60_000;     // sessions written in this window are reported as open
const STATE_FILE = join(process.env.XDG_STATE_HOME || join(HOME, '.local', 'state'), 'agent-stats', 'state.json');
const ONCE = process.argv.includes('--once');
const DRY = process.argv.includes('--dry');

const ROOTS = [
  { agent: 'claude', dir: join(HOME, '.claude', 'projects') },
  { agent: 'codex', dir: join(HOME, '.codex', 'sessions') },
  { agent: 'codex', dir: join(HOME, '.codex', 'archived_sessions') },
  { agent: 'grok', dir: join(HOME, '.grok', 'sessions') },
];

const dayFmt = new Intl.DateTimeFormat('en-CA', { timeZone: 'Asia/Tokyo', year: 'numeric', month: '2-digit', day: '2-digit' });
const day = ts => dayFmt.format(new Date(ts));

// files: path -> { agent, offset, mtime, ids, total, model, cwd, tokens }
// days:  "day|agent|model" -> { tokens, output }
// repos: "day|repo" -> tokens, for the tokens whose working directory is known
// Bump V when counting changes: older state is dropped and every machine rebuilds from its transcripts.
const V = 3;
let state = { v: V, files: {}, days: {}, repos: {} };
const dirty = new Set(), dirtyRepos = new Set();

async function load() {
  try { const s = JSON.parse(await readFile(STATE_FILE, 'utf8')); if (s.v === V) state = s; } catch {}
}
async function save() {
  await mkdir(dirname(STATE_FILE), { recursive: true });
  await writeFile(STATE_FILE + '.tmp', JSON.stringify(state));
  await rename(STATE_FILE + '.tmp', STATE_FILE);
}

function count(d, agent, model, tokens, output, cwd) {
  if (!tokens) return;
  const k = `${d}|${agent}|${model || 'unknown'}`;
  const v = state.days[k] ??= { tokens: 0, output: 0 };
  v.tokens += tokens; v.output += output;
  dirty.add(k);
  if (!cwd) return;
  const rk = `${d}|${repoOf(cwd)}`;
  state.repos[rk] = (state.repos[rk] || 0) + tokens;
  dirtyRepos.add(rk);
}

// The repository a working directory belongs to, seeing through worktrees: ~/.t3/worktrees/<repo>/…,
// ~/.codex/worktrees/<id>/<repo>/…, <repo>/.claude/worktrees/…, and anything under ~/Dev/<repo>/.
// Only the hub sees these names; the site gets the project they map to.
function repoOf(cwd) {
  const p = cwd.startsWith(HOME) ? '~' + cwd.slice(HOME.length) : cwd;
  const m = p.match(/\/\.t3\/worktrees\/([^/]+)/) || p.match(/\/\.codex\/worktrees\/[^/]+\/([^/]+)/) || p.match(/\/([^/]+)\/\.claude\/worktrees\//)
    || p.match(/^~\/(?:Dev|dev|Developer|Projects|projects|src|code|Documents)\/([^/]+)/);
  if (m) return m[1];
  if (/^~\/\.t3\/scratch\//.test(p)) return 'scratch';
  return basename(p) || p;
}

// What a session is doing, from its latest event. Tool names are folded into a few coarse kinds so nothing
// specific (MCP servers, commands) leaves the machine.
function toolKind(name = '') {
  name = String(name).toLowerCase();
  if (/^(bash|shell|exec|exec_command|local_shell|execute|run)/.test(name)) return 'run';
  if (/(edit|write|patch|notebook|delete|move)/.test(name)) return 'edit';
  if (/(read|grep|glob|search|fetch|ls|find|view)/.test(name)) return 'read';
  if (/^(task|agent|spawn)|subagent|delegate/.test(name)) return 'delegate';
  if (/askuser|ask_user|question/.test(name)) return 'waiting';
  return 'tool';
}
function act(f, state, ts) { f.state = state; f.stateAt = Date.parse(ts) || Number(ts) || Date.now(); }
// Running split of a session's tokens: fresh input, cache reads, cache writes, output.
function io(f, input, cacheRead, cacheWrite, output) {
  const v = f.io ??= { in: 0, cr: 0, cw: 0, out: 0 };
  v.in += Math.max(0, input); v.cr += Math.max(0, cacheRead); v.cw += Math.max(0, cacheWrite); v.out += Math.max(0, output);
}

// Claude Code: one usage block per API response, repeated across streamed lines, so dedupe on message id.
function claudeLine(f, r) {
  if (r.cwd) f.cwd = r.cwd;
  const m = r.message;
  for (const b of Array.isArray(m?.content) ? m.content : []) {
    if (r.type === 'assistant') act(f, b.type === 'thinking' || b.type === 'redacted_thinking' ? 'thinking' : b.type === 'tool_use' ? toolKind(b.name) : 'writing', r.timestamp);
    else if (b.type === 'tool_result') act(f, 'thinking', r.timestamp);
  }
  if (r.type !== 'assistant' || !m?.usage || !r.timestamp) return;
  if (m.model && m.model !== '<synthetic>') f.model = m.model;
  if (f.ids.includes(m.id)) return;
  f.ids.push(m.id);
  if (f.ids.length > 64) f.ids.shift();
  const u = m.usage;
  const tokens = (u.input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.output_tokens ?? 0);
  f.tokens += tokens;
  io(f, u.input_tokens ?? 0, u.cache_read_input_tokens ?? 0, u.cache_creation_input_tokens ?? 0, u.output_tokens ?? 0);
  count(day(r.timestamp), 'claude', m.model, tokens, u.output_tokens ?? 0, f.cwd);
}

// Codex: token_count events carry the last request's usage and a running session total. Count the last
// request: a forked or subagent rollout starts from its parent's running total, which the parent already counted.
function codexLine(f, r) {
  const p = r.payload;
  if (!p) return;
  if ((r.type === 'session_meta' || r.type === 'turn_context') && p.cwd) f.cwd = p.cwd;
  // A Codex subagent names the thread that started it.
  if (r.type === 'session_meta' && p.parent_thread_id) { f.parent = p.parent_thread_id; f.sub = true; }
  if (r.type === 'turn_context' && p.model) f.model = p.model;
  if (r.type === 'response_item' && r.timestamp) {
    if (p.type === 'reasoning') act(f, 'thinking', r.timestamp);
    else if (/call_output$/.test(p.type)) act(f, 'thinking', r.timestamp);
    else if (/call$/.test(p.type)) act(f, toolKind(p.name || p.type), r.timestamp);
    else if (p.type === 'message' && p.role === 'assistant' || p.type === 'agent_message') act(f, 'writing', r.timestamp);
  }
  const i = p.type === 'token_count' && p.info;
  if (!i?.total_token_usage || !r.timestamp) return;
  const sum = u => u.total_tokens ?? (u.input_tokens ?? 0) + (u.output_tokens ?? 0);
  const total = sum(i.total_token_usage);
  if (total === f.total) return;   // the same event, repeated
  const last = i.last_token_usage;
  const tokens = last ? sum(last) : Math.max(0, total - f.total);
  f.total = total;
  f.tokens += tokens;
  if (last) io(f, (last.input_tokens ?? 0) - (last.cached_input_tokens ?? 0), last.cached_input_tokens ?? 0, last.cache_write_input_tokens ?? 0, last.output_tokens ?? 0);
  count(day(r.timestamp), 'codex', f.model, tokens, last?.output_tokens ?? 0, f.cwd);
}

// Grok: each finished turn reports its usage per model.
function grokLine(f, r) {
  const u = r.params?.update;
  const at = (u?._meta?.agentTimestampMs ?? r.timestamp * 1000);
  if (u?.sessionUpdate === 'agent_thought_chunk') act(f, 'thinking', at);
  else if (u?.sessionUpdate === 'agent_message_chunk') act(f, 'writing', at);
  else if (u?.sessionUpdate === 'tool_call') act(f, toolKind(u.kind || u.title), at);
  else if (u?.sessionUpdate === 'tool_call_update' && u.status === 'completed') act(f, 'thinking', at);
  if (u?.sessionUpdate !== 'turn_completed' || !u.usage || !r.timestamp) return;
  if (u.prompt_id) {
    if (f.ids.includes(u.prompt_id)) return;
    f.ids.push(u.prompt_id);
    if (f.ids.length > 64) f.ids.shift();
  }
  for (const [model, m] of Object.entries(u.usage.modelUsage || { [f.model || 'unknown']: u.usage })) {
    const tokens = m.totalTokens ?? (m.inputTokens ?? 0) + (m.outputTokens ?? 0);
    f.model = model;
    f.tokens += tokens;
    io(f, (m.inputTokens ?? 0) - (m.cachedReadTokens ?? 0), m.cachedReadTokens ?? 0, m.cacheCreationTokens ?? 0, m.outputTokens ?? 0);
    count(day(r.timestamp * 1000), 'grok', model, tokens, m.outputTokens ?? 0, f.cwd);
  }
}
const LINE = { claude: claudeLine, codex: codexLine, grok: grokLine };

// Read whatever was appended to a transcript since the last scan, whole lines only.
async function readNew(path, f, size) {
  if (size < f.offset) f.offset = 0;   // rewritten; ids and running totals keep it from double counting
  const fh = await open(path, 'r');
  const handle = LINE[f.agent];
  try {
    while (f.offset < size) {
      const buf = Buffer.alloc(Math.min(size - f.offset, 32 << 20));
      await fh.read(buf, 0, buf.length, f.offset);
      const end = buf.lastIndexOf(10);
      if (end < 0) return;
      for (const line of buf.subarray(0, end).toString('utf8').split('\n')) {
        if (!line) continue;
        let r;
        try { r = JSON.parse(line); } catch { continue; }
        handle(f, r);
      }
      f.offset += end + 1;
    }
  } finally { await fh.close(); }
}

async function* walk(dir) {
  let entries;
  try { entries = await readdir(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    const p = join(dir, e.name);
    if (e.isDirectory()) yield* walk(p);
    else if (e.isFile() && e.name.endsWith('.jsonl')) yield p;
  }
}

async function scan() {
  const seen = new Set();
  const codexNames = new Set();
  for (const { agent, dir } of ROOTS) {
    for await (const path of walk(dir)) {
      // A Codex session can sit in both sessions/ and archived_sessions/; count it once.
      if (agent === 'codex') {
        if (codexNames.has(basename(path))) continue;
        codexNames.add(basename(path));
      }
      // Grok keeps several logs per session (sessions/<cwd, URL-encoded>/<id>/); usage is in updates.jsonl.
      if (agent === 'grok' && basename(path) !== 'updates.jsonl') continue;
      seen.add(path);
      let s;
      try { s = await stat(path); } catch { continue; }
      const f = state.files[path] ??= { agent, offset: 0, mtime: 0, ids: [], total: 0, tokens: 0,
        ...(agent === 'grok' && { cwd: decodeURIComponent(basename(dirname(dirname(path)))) }) };
      if (s.mtimeMs === f.mtime && s.size === f.offset) continue;
      try { await readNew(path, f, s.size); } catch (err) { console.warn('read failed', path, err.message); continue; }
      f.mtime = s.mtimeMs;
    }
  }
  // Pruned transcripts leave the file list; their tokens stay in the daily totals.
  for (const path of Object.keys(state.files)) if (!seen.has(path)) delete state.files[path];
}

// Claude Code deletes transcripts after 30 days by default. T3 Code's usage cache keeps one record per API
// response, [time, model, session, input, cache read, cache write, output, …, dedupe key], including for
// transcripts that are gone, so the first scan takes the history of deleted transcripts from there.
async function importT3() {
  const files = {};
  for (const name of ['usage-scan-cache.json', 'usage-scan-cache-v5.json']) {
    let c;
    try { c = JSON.parse(await readFile(join(HOME, '.t3', 'userdata', name), 'utf8')); } catch { continue; }
    for (const [path, v] of Object.entries(c.files || {})) files[path] = { ...v, models: c.models || [] };
  }
  const keys = new Set();
  let tokens = 0;
  for (const [path, v] of Object.entries(files)) {
    if (!LINE[v.p] || state.files[path] || !Array.isArray(v.r)) continue;
    try { await stat(path); continue; } catch {}
    for (const [ts, m, , input, cacheRead, cacheWrite, output, , key] of v.r) {
      if (key) { if (keys.has(key)) continue; keys.add(key); }
      const model = v.models[m];
      if (model === '<synthetic>') continue;
      const n = input + cacheRead + cacheWrite + output;
      count(day(ts), v.p, model, n, output);
      tokens += n;
    }
  }
  if (tokens) console.log(`imported ${(tokens / 1e9).toFixed(2)}B tokens of deleted transcripts from T3 Code's usage cache`);
}

function sessions(now) {
  const out = [];
  // Codex threads by id, to find a subagent's parent session.
  const codexById = new Map();
  for (const path of Object.keys(state.files)) { const m = basename(path).match(/([0-9a-f-]{36})\.jsonl$/); if (m) codexById.set(m[1], basename(path, '.jsonl')); }
  for (const [path, f] of Object.entries(state.files)) {
    if (now - f.mtime > OPEN_WINDOW) continue;
    // One-shot helper calls (e.g. T3 naming a thread) run in temp dirs; they count as tokens, not as agents.
    if (/^(\/private)?\/(tmp|var\/folders)\//.test(f.cwd || '')) continue;
    const sub = path.includes(`${sep}subagents${sep}`) || !!f.sub;
    // Claude Code keeps a subagent's transcript in <session>/subagents/, next to <session>.jsonl.
    const parent = !sub ? null : f.agent === 'claude' ? basename(dirname(dirname(path))) : codexById.get(f.parent) || null;
    out.push({
      id: f.agent === 'grok' ? basename(dirname(path)) : basename(path, '.jsonl'),
      agent: f.agent,
      kind: sub ? 'subagent' : 'main',
      parent,
      project: f.cwd ? basename(f.cwd) : basename(dirname(sub ? dirname(dirname(path)) : path)),
      cwd: f.cwd || null,
      repo: f.cwd ? repoOf(f.cwd) : null,
      model: f.model || null,
      lastActivity: Math.round(f.mtime),
      tokens: f.tokens,
      state: f.state || null,
      stateAt: f.stateAt || null,
      io: f.io || null,
    });
  }
  return out;
}

let lastFull = 0;
async function tick() {
  const started = Date.now();
  await scan();
  if (!state.t3) { await importT3(); state.t3 = Date.now(); }
  const now = Date.now();
  const full = now - lastFull > FULL_SYNC;
  const keys = full ? Object.keys(state.days) : [...dirty];
  const body = {
    machine: MACHINE, label: LABEL, platform: process.platform, at: now, full,
    days: keys.map(k => { const [d, agent, model] = k.split('|'); return { day: d, agent, model, ...state.days[k] }; }),
    repos: (full ? Object.keys(state.repos) : [...dirtyRepos]).map(k => { const [d, repo] = k.split('|'); return { day: d, repo, tokens: state.repos[k] }; }),
    sessions: sessions(now),
  };
  if (DRY) {
    const total = Object.values(state.days).reduce((a, v) => a + v.tokens, 0);
    console.log(JSON.stringify({ machine: MACHINE, scanMs: now - started, files: Object.keys(state.files).length,
      tokens: total, dayRows: body.days.length, repos: Object.fromEntries(Object.entries(body.repos.reduce((a, r) => (a[r.repo] = (a[r.repo] || 0) + r.tokens, a), {}))
        .sort((a, b) => b[1] - a[1]).slice(0, 15)), sessions: body.sessions.length }, null, 2));
  } else {
    const res = await fetch(HUB_URL + '/report', {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${HUB_TOKEN}` },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) throw new Error(`hub answered ${res.status}`);
    dirty.clear(); dirtyRepos.clear();
    if (full) lastFull = now;
  }
  await save();
}

await load();
if (ONCE) { await tick(); process.exit(0); }
for (;;) {
  try { await tick(); } catch (err) { console.warn(new Date().toISOString(), 'tick failed:', err.message); }
  await new Promise(r => setTimeout(r, TICK));
}
