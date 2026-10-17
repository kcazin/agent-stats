// Agent stats hub. Runs on the Mac mini, on the tailnet only.
// Receives reports from every machine's reporter, keeps history in SQLite, serves the internal dashboard,
// and publishes the public subset to the relay on Railway.
//
//   HOST=100.x.y.z PUBLISH_URL=https://…/publish node hub.mjs
// Tokens are read from ~/.config/agent-stats/{hub_token,publish_token} unless set in the environment.

import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { readFile, mkdir } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

const HOST = process.env.HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || 7777);
const secret = name => readFile(join(homedir(), '.config', 'agent-stats', name), 'utf8').then(t => t.trim(), () => '');
const HUB_TOKEN = process.env.HUB_TOKEN || await secret('hub_token');
const PUBLISH_URL = process.env.PUBLISH_URL || '';
const PUBLISH_TOKEN = process.env.PUBLISH_TOKEN || await secret('publish_token');
const GITHUB_USER = process.env.GITHUB_USER || 'kcazin';
const DATA = join(process.env.XDG_STATE_HOME || join(homedir(), '.local', 'state'), 'agent-stats');
const TICK = 15_000;
const ONLINE = 45_000;     // a machine is online if it reported this recently
const WORKING = 60_000;    // a session is working if its transcript was written this recently

await mkdir(DATA, { recursive: true });
const db = new DatabaseSync(join(DATA, 'hub.db'));
db.exec(`
  PRAGMA journal_mode = WAL;
  CREATE TABLE IF NOT EXISTS usage (machine TEXT, day TEXT, agent TEXT, model TEXT, tokens INTEGER, output INTEGER,
    PRIMARY KEY (machine, day, agent, model));
  CREATE TABLE IF NOT EXISTS machines (machine TEXT PRIMARY KEY, label TEXT, platform TEXT, last_seen INTEGER, sessions TEXT);
  CREATE TABLE IF NOT EXISTS samples (at INTEGER PRIMARY KEY, working INTEGER, open INTEGER, subagents INTEGER, machines INTEGER);
  CREATE TABLE IF NOT EXISTS github (day TEXT PRIMARY KEY, count INTEGER);
  CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT);
  CREATE TABLE IF NOT EXISTS repos (machine TEXT, day TEXT, repo TEXT, tokens INTEGER, PRIMARY KEY (machine, day, repo));
`);
// Working agents per agent type, added after the first samples were taken.
try { db.exec('ALTER TABLE samples ADD COLUMN by_agent TEXT'); } catch {}

const upsertUsage = db.prepare(`INSERT INTO usage VALUES (?, ?, ?, ?, ?, ?)
  ON CONFLICT DO UPDATE SET tokens = max(tokens, excluded.tokens), output = max(output, excluded.output)`);
const upsertRepo = db.prepare(`INSERT INTO repos VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET tokens = max(tokens, excluded.tokens)`);
const upsertMachine = db.prepare(`INSERT INTO machines VALUES (?, ?, ?, ?, ?)
  ON CONFLICT DO UPDATE SET label = excluded.label, platform = excluded.platform, last_seen = excluded.last_seen, sessions = excluded.sessions`);

const dayFmt = new Intl.DateTimeFormat('en-CA', { timeZone: 'Asia/Tokyo', year: 'numeric', month: '2-digit', day: '2-digit' });
const today = () => dayFmt.format(new Date());
const daysAgo = n => dayFmt.format(new Date(Date.now() - n * 86_400_000));

// claude-opus-5-5 → Opus 5.5, gpt-6-astra → GPT-6 Astra, grok-4.6-build → Grok 4.6 Build
function modelName(id) {
  if (!id || id === 'unknown') return 'Other';
  const c = id.match(/(opus|sonnet|haiku|fable)-(\d+)(?:-(\d{1,2}))?(?!\d)/i);
  if (c) return `${c[1][0].toUpperCase()}${c[1].slice(1)} ${c[2]}${c[3] ? '.' + c[3] : ''}`;
  const g = id.match(/^gpt-?([\d.]+)(.*)$/i);
  if (g) return `GPT-${g[1]}` + g[2].split('-').filter(Boolean).map(w => ' ' + w[0].toUpperCase() + w.slice(1)).join('');
  if (/^grok-/i.test(id)) return id.split('-').map(w => w[0].toUpperCase() + w.slice(1)).join(' ');
  return id;
}

// The projects on the site, by repository; every other repository counts as "other". Tokens whose working
// directory is unknown (deleted transcripts, imported from T3 Code's usage cache) were mostly Neon Overrun.
const PROJECTS = [['neon', /space-rock|neon|poe-mobile/i], ['chum', /^chum/i], ['contenthall', /^content-hall/i]];
const UNKNOWN = { neon: 0.9, other: 0.1 };
const projectOf = repo => PROJECTS.find(([, re]) => re.test(repo || ''))?.[0] || 'other';
const noProjects = () => Object.fromEntries([...PROJECTS.map(([k]) => k), 'other'].map(k => [k, 0]));
// Tokens per project, all time and today. Each machine's day total that its repositories don't account for is
// split as UNKNOWN says.
function projects() {
  const all = noProjects(), now = noProjects(), d = today(), known = new Map();
  for (const r of db.prepare('SELECT machine, day, repo, tokens FROM repos').all()) {
    const p = projectOf(r.repo);
    all[p] += r.tokens; if (r.day === d) now[p] += r.tokens;
    known.set(r.machine + '|' + r.day, (known.get(r.machine + '|' + r.day) || 0) + r.tokens);
  }
  for (const r of db.prepare('SELECT machine, day, sum(tokens) AS t FROM usage GROUP BY machine, day').all()) {
    const rest = Math.max(0, r.t - (known.get(r.machine + '|' + r.day) || 0));
    for (const [p, f] of Object.entries(UNKNOWN)) { all[p] += rest * f; if (r.day === d) now[p] += rest * f; }
  }
  const round = o => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, Math.round(v)]));
  return { all: round(all), today: round(now) };
}

// Every repository with its tokens today and all time, and its project (null when none matches), as the Rust hub's
// /api/repos gives them. Private: the desktop overlay reads it over the tailnet to show each repository by name.
function repos() {
  return db.prepare(`SELECT repo, sum(tokens) AS tokensAll, sum(CASE WHEN day = ? THEN tokens ELSE 0 END) AS tokensToday
    FROM repos WHERE repo IS NOT NULL GROUP BY repo ORDER BY tokensAll DESC`).all(today()).map(r => ({
    repo: r.repo, project: PROJECTS.find(([, re]) => re.test(r.repo))?.[0] ?? null, tokensToday: r.tokensToday, tokensAll: r.tokensAll,
  }));
}

// Sessions are public only under an opaque id, stable for a session.
const publicId = (machine, id) => createHash('sha256').update(machine + '|' + id).digest('hex').slice(0, 10);

// Day totals pass 2^31 on busy days, so no `| 0` here.
const tokenCount = n => Math.max(0, Math.round(Number(n) || 0));

function report(r) {
  db.exec('BEGIN');
  try {
    for (const d of r.days || []) upsertUsage.run(r.machine, d.day, d.agent, d.model || 'unknown', tokenCount(d.tokens), tokenCount(d.output));
    for (const d of r.repos || []) upsertRepo.run(r.machine, d.day, d.repo, tokenCount(d.tokens));
    upsertMachine.run(r.machine, r.label || r.machine, r.platform || '', Date.now(), JSON.stringify(r.sessions || []));
    db.exec('COMMIT');
  } catch (e) { db.exec('ROLLBACK'); throw e; }
}

function live(now = Date.now()) {
  const machines = db.prepare('SELECT * FROM machines ORDER BY machine').all().map(m => ({
    machine: m.machine, label: m.label, platform: m.platform, lastSeen: m.last_seen,
    online: now - m.last_seen < ONLINE, sessions: JSON.parse(m.sessions || '[]'),
  }));
  const sessions = [];
  for (const m of machines) {
    if (!m.online) continue;
    for (const s of m.sessions) sessions.push({ ...s, machine: m.label, working: now - s.lastActivity < WORKING });
  }
  sessions.sort((a, b) => b.lastActivity - a.lastActivity);
  const working = sessions.filter(s => s.working);
  return {
    machines, sessions,
    working: working.filter(s => s.kind === 'main').length,
    subagents: working.filter(s => s.kind === 'subagent').length,
    open: sessions.filter(s => s.kind === 'main').length,
    byAgent: Object.fromEntries(['claude', 'codex', 'grok'].map(a => [a, working.filter(s => s.agent === a).length])),
  };
}

const group = (rows, key) => {
  const out = {};
  for (const r of rows) out[key(r)] = (out[key(r)] || 0) + r.tokens;
  return Object.entries(out).map(([name, tokens]) => ({ name, tokens })).sort((a, b) => b.tokens - a.tokens);
};
const topN = (list, n) => list.length <= n ? list
  : [...list.slice(0, n - 1), { name: 'Other', tokens: list.slice(n - 1).reduce((a, b) => a + b.tokens, 0) }];

function labels() {
  return Object.fromEntries(db.prepare('SELECT machine, label FROM machines').all().map(m => [m.machine, m.label]));
}

function todayRows() {
  return db.prepare('SELECT machine, agent, model, tokens, output FROM usage WHERE day = ?').all(today());
}

function calendar(from) {
  const tokens = {}, github = {};
  for (const r of db.prepare('SELECT day, sum(tokens) AS t FROM usage WHERE day >= ? GROUP BY day').all(from)) tokens[r.day] = r.t;
  for (const r of db.prepare('SELECT day, count FROM github WHERE day >= ?').all(from)) github[r.day] = r.count;
  return { tokens, github };
}

// Peak agents working per 10-minute bucket, last 24 hours.
function history(now = Date.now()) {
  const bucket = 600_000, start = Math.floor((now - 86_400_000) / bucket) * bucket;
  const out = Array.from({ length: Math.ceil((now - start) / bucket) }, (_, i) => ({ at: start + i * bucket, working: 0 }));
  for (const s of db.prepare('SELECT at, working FROM samples WHERE at >= ?').all(start)) {
    const b = out[Math.floor((s.at - start) / bucket)];
    if (b) b.working = Math.max(b.working, s.working);
  }
  return out;
}

// Agents working at once (sessions and subagents), by agent type: the busiest sample in each 10-minute bucket
// over the last 24 hours. null where nothing was recorded, so the site can tell "no data" from "zero".
function activity(now = Date.now()) {
  const bucket = 600_000, start = Math.floor((now - 86_400_000) / bucket) * bucket;
  const out = Array.from({ length: Math.ceil((now - start) / bucket) }, () => null);
  for (const s of db.prepare('SELECT at, working, subagents, by_agent FROM samples WHERE at >= ?').all(start)) {
    const i = Math.floor((s.at - start) / bucket), total = s.working + s.subagents;
    if (out[i] && out[i].total >= total) continue;
    let by = {};
    try { by = JSON.parse(s.by_agent) || {}; } catch {}
    if (!s.by_agent && total) by = { claude: total };   // samples from before by_agent existed
    out[i] = { total, ...by };
  }
  return { start, bucket, points: out };
}

// Headline totals count from the first recorded day; GitHub over the same days, so the two compare.
function since() {
  const all = db.prepare('SELECT sum(tokens) AS t, min(day) AS first FROM usage').get();
  const github = db.prepare('SELECT sum(count) AS n FROM github WHERE day >= ?').get(all.first || today()).n || 0;
  return { day: all.first, tokens: all.t || 0, github };
}

function publicPayload() {
  const now = Date.now(), l = live(now), rows = todayRows();
  const recent = l.machines.filter(m => now - m.lastSeen < 7 * 86_400_000);
  const workingBy = label => l.sessions.filter(s => s.working && s.machine === label).length;
  return {
    updated: now,
    live: { working: l.working, subagents: l.subagents, open: l.open, byAgent: l.byAgent,
      machinesOnline: recent.filter(m => m.online).length, machinesTotal: recent.length,
      // Per agent: what it is doing and its running token split, under an opaque id so the site can follow it.
      // A session waiting on a working subagent counts too, so the subagent has its parent on the site.
      agents: l.sessions.filter(s => s.working || l.sessions.some(c => c.working && c.parent === s.id && c.machine === s.machine)).map(s => ({
        id: publicId(s.machine, s.id), parent: s.parent ? publicId(s.machine, s.parent) : null,
        agent: s.agent, kind: s.kind, model: modelName(s.model), project: projectOf(s.repo || s.project),
        state: s.working ? s.state || null : 'delegate', stateAt: s.stateAt || null, io: s.io || null }))
        .sort((a, b) => a.agent.localeCompare(b.agent) || (a.kind === 'main' ? -1 : 1)) },
    today: {
      day: today(),
      tokens: rows.reduce((a, r) => a + r.tokens, 0),
      output: rows.reduce((a, r) => a + r.output, 0),
      byModel: topN(group(rows, r => modelName(r.model)), 5),
      byMachine: recent.map(m => ({ name: m.label, online: m.online, working: workingBy(m.label),
        tokens: rows.filter(r => r.machine === m.machine).reduce((a, r) => a + r.tokens, 0) }))
        .sort((a, b) => b.tokens - a.tokens),
    },
    history: history(now).map(b => b.working),
    activity: activity(now),
    since: since(),
    pile: pile && { tokens: Math.round(pile.tokens), perHour: 1e9 },
    projects: projects(),
    calendar: calendar(daysAgo(380)),
  };
}

function internalState() {
  const now = Date.now(), l = live(now), rows = todayRows(), names = labels();
  const from = daysAgo(89);
  const daily = {};
  for (const r of db.prepare('SELECT day, machine, sum(tokens) AS t FROM usage WHERE day >= ? GROUP BY day, machine').all(from))
    (daily[r.day] ??= {})[names[r.machine] || r.machine] = r.t;
  const week = db.prepare('SELECT sum(tokens) AS t FROM usage WHERE day >= ?').get(daysAgo(6)).t || 0;
  const all = db.prepare('SELECT sum(tokens) AS t, min(day) AS first FROM usage').get();
  return {
    now, ...l,
    machines: l.machines.map(m => ({ ...m, sessions: undefined,
      tokensToday: rows.filter(r => r.machine === m.machine).reduce((a, r) => a + r.tokens, 0),
      working: l.sessions.filter(s => s.working && s.machine === m.label).length })),
    today: { tokens: rows.reduce((a, r) => a + r.tokens, 0), output: rows.reduce((a, r) => a + r.output, 0),
      byModel: group(rows, r => modelName(r.model)), byAgent: group(rows, r => r.agent) },
    week, allTime: all.t || 0, firstDay: all.first,
    daily, history: history(now), publish,
  };
}

// GitHub contributions, hourly, through the gh CLI's login.
function fetchGithub() {
  const query = `query($u: String!) { user(login: $u) { contributionsCollection {
    contributionCalendar { weeks { contributionDays { date contributionCount } } } } } }`;
  execFile('gh', ['api', 'graphql', '-f', `query=${query}`, '-f', `u=${GITHUB_USER}`], { timeout: 30_000 }, (err, out) => {
    if (err) return console.warn('github fetch failed:', err.message);
    const ins = db.prepare('INSERT INTO github VALUES (?, ?) ON CONFLICT DO UPDATE SET count = excluded.count');
    for (const w of JSON.parse(out).data.user.contributionsCollection.contributionCalendar.weeks)
      for (const d of w.contributionDays) ins.run(d.date, d.contributionCount);
  });
}

const publish = { url: PUBLISH_URL, lastOk: null, lastError: null };
// The site's pile of unprocessed tokens: every new token lands on it, and trucks clear it at 1B an hour. It starts
// empty each Tokyo day (yesterday's pile joins the mountain). Kept here, and saved, so every visitor sees the same pile.
const PILE_RATE = 1e9 / 3_600_000;   // tokens per ms
let pile = JSON.parse(db.prepare("SELECT v FROM kv WHERE k = 'pile'").get()?.v || 'null');
function updatePile(now) {
  const s = since(), day = today(), todayTokens = todayRows().reduce((a, r) => a + r.tokens, 0);
  if (!pile) {
    // First run: start from the pile the site showed before this existed (today's tokens less 100K per 15 s).
    const midnight = Date.parse(day + 'T00:00:00+09:00');
    pile = { day, tokens: Math.max(0, todayTokens - Math.floor((now - midnight) / 15000) * 1e5), total: s.tokens, at: now };
  } else if (pile.day !== day) pile = { day, tokens: 0, total: s.tokens, at: now };
  else {
    const added = Math.max(0, s.tokens - pile.total), cleared = (now - pile.at) * PILE_RATE;
    pile = { day, tokens: Math.max(0, Math.min(todayTokens, pile.tokens + added - cleared)), total: s.tokens, at: now };
  }
  db.prepare("INSERT OR REPLACE INTO kv VALUES ('pile', ?)").run(JSON.stringify(pile));
}

// What happened lately: tokens per project over the last minute, and each agent's token split since the previous
// publish. A page that has just loaded replays the minute, so it does not wait for a second publish to see anything
// move.
const recent = [];
function withLast(d) {
  const prev = recent.at(-1), old = recent[0];
  recent.push({ at: d.updated, all: d.projects.all, io: Object.fromEntries(d.live.agents.filter(a => a.io).map(a => [a.id, a.io])) });
  while (recent.length > 1 && d.updated - recent[0].at > 60_000) recent.shift();
  if (!prev) return d;
  d.projects.last = Object.fromEntries(Object.entries(d.projects.all).map(([k, v]) => [k, Math.max(0, v - (old.all[k] || 0))]));
  d.projects.lastMs = d.updated - old.at;
  for (const a of d.live.agents) {
    const p = prev.io[a.id];
    if (a.io && p) a.flow = Object.fromEntries(['in', 'cr', 'cw', 'out'].map(k => [k, Math.max(0, (a.io[k] || 0) - (p[k] || 0))]));
  }
  return d;
}

async function tick() {
  const now = Date.now(), l = live(now);
  updatePile(now);
  db.prepare('INSERT OR REPLACE INTO samples (at, working, open, subagents, machines, by_agent) VALUES (?, ?, ?, ?, ?, ?)')
    .run(now, l.working, l.open, l.subagents, l.machines.filter(m => m.online).length, JSON.stringify(l.byAgent));
  db.prepare('DELETE FROM samples WHERE at < ?').run(now - 30 * 86_400_000);
  if (!PUBLISH_URL) return;
  try {
    const res = await fetch(PUBLISH_URL, {
      method: 'POST', body: JSON.stringify(withLast(publicPayload())), signal: AbortSignal.timeout(10_000),
      headers: { 'content-type': 'application/json', authorization: `Bearer ${PUBLISH_TOKEN}` },
    });
    if (!res.ok) throw new Error(`relay answered ${res.status}`);
    publish.lastOk = now; publish.lastError = null;
  } catch (err) { publish.lastError = `${new Date(now).toISOString()} ${err.message}`; }
}

const json = (res, code, body) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
const dashboard = new URL('./dashboard.html', import.meta.url);

createServer(async (req, res) => {
  try {
    if (req.method === 'POST' && req.url === '/report') {
      if (HUB_TOKEN && req.headers.authorization !== `Bearer ${HUB_TOKEN}`) return json(res, 401, { error: 'unauthorized' });
      let body = '';
      for await (const chunk of req) { body += chunk; if (body.length > 8 << 20) return json(res, 413, { error: 'too large' }); }
      report(JSON.parse(body));
      return json(res, 200, { ok: true });
    }
    if (req.url === '/api/state') return json(res, 200, internalState());
    if (req.url === '/api/public') return json(res, 200, publicPayload());
    if (req.url === '/api/repos') return json(res, 200, repos());
    if (req.url === '/' || req.url === '/index.html') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      return res.end(await readFile(dashboard));
    }
    json(res, 404, { error: 'not found' });
  } catch (err) {
    console.warn(req.method, req.url, err.message);
    json(res, 500, { error: err.message });
  }
}).listen(PORT, HOST, () => console.log(`hub on http://${HOST}:${PORT}`));

fetchGithub();
setInterval(fetchGithub, 3_600_000);
setInterval(tick, TICK);
