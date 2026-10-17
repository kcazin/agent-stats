#!/usr/bin/env node
// Parity harness: legacy JS hub (hub A) vs Rust hub (hub B), same inputs, frozen clock, byte-compared outputs.
//   node tests/hub-parity/parity.mjs --rust <agent-stats binary> [--keep] [--lenient-agents]
//   node tests/hub-parity/parity.mjs --js-vs-js        (harness self-test: two JS hubs)
// See README.md. Never touches the real ~/.config|state|share/agent-stats: every child gets scratch HOME/XDG dirs.
import { spawn } from 'node:child_process';
import { createServer, request as httpRequest } from 'node:http';
import { createServer as netServer } from 'node:net';
import { mkdirSync, openSync, closeSync, readFileSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const args = process.argv.slice(2);
const flag = n => args.includes(n);
const opt = n => { const i = args.indexOf(n); return i >= 0 ? args[i + 1] : undefined; };
const JS_VS_JS = flag('--js-vs-js'), KEEP = flag('--keep'), LENIENT_AGENTS = flag('--lenient-agents');
const RUST = opt('--rust') && resolve(opt('--rust'));
if (!JS_VS_JS && !RUST) { console.error('usage: parity.mjs --rust <binary> [--keep] | --js-vs-js'); process.exit(2); }

const SCRATCH = `/var/tmp/agents/agent-stats/scratch-hub/parity-${process.pid}`;
const S0 = Date.parse('2026-10-10T08:00:00Z');
// hub.mjs inserts ~385 GitHub rows at startup as separate autocommit transactions on its event loop and answers
// nothing until they are done; on a loaded disk that takes well over 10 s, so the startup wait is generous.
const STARTUP_MS = 60_000
const AUTH = 'Bearer parity-token', PUB_AUTH = 'Bearer pub-token';
const HUB_TOML = `timezone = "Asia/Tokyo"
[[project]]
key = "neon"
name = "Neon Overrun"
color = "#5b8cff"
match = ["*space-rock*", "*neon*", "*poe-mobile*"]
[[project]]
key = "chum"
name = "Chum"
color = "#2fbf71"
match = ["chum*"]
[[project]]
key = "contenthall"
name = "Content Hall"
color = "#f2a93b"
match = ["content-hall*"]
[unattributed]
neon = 0.9
other = 0.1
`;
const PROJECT_LIST = [{ key: 'neon', name: 'Neon Overrun', color: '#5b8cff' }, { key: 'chum', name: 'Chum', color: '#2fbf71' },
  { key: 'contenthall', name: 'Content Hall', color: '#f2a93b' }];

// ---- process / io plumbing --------------------------------------------------------------------------------------
const children = [], servers = [], logs = [];
let cleaned = false;
function cleanup() {
  if (cleaned) return; cleaned = true;
  for (const c of children) try { c.kill('SIGKILL'); } catch {}
  for (const s of servers) try { s.close(); s.closeAllConnections?.(); } catch {}
}
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => { cleanup(); process.exit(130); });
process.on('exit', cleanup);

const freePort = () => new Promise((res, rej) => {
  const s = netServer(); s.once('error', rej);
  s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); });
});
// node:http (not fetch) so keep-alive pools never carry state between the two hubs.
const http = (base, method, path, body, headers = {}, timeout = 60_000) => new Promise((res, rej) => {
  const u = new URL(base + path);
  const r = httpRequest({ host: u.hostname, port: u.port, path: u.pathname, method, headers: { connection: 'close', ...headers }, timeout }, resp => {
    let t = ''; resp.setEncoding('utf8'); resp.on('data', c => t += c); resp.on('end', () => res({ status: resp.statusCode, text: t }));
  });
  r.on('timeout', () => r.destroy(new Error(`timeout ${method} ${base}${path}`))); r.on('error', rej);
  r.end(body);
});

function startHub(name, kind, bin, dirName = name) {
  return (async () => {
    const dir = join(SCRATCH, dirName);
    const env = { HOME: join(dir, 'home'), XDG_STATE_HOME: join(dir, 'state'), XDG_DATA_HOME: join(dir, 'data'), AGENT_STATS_CONFIG_DIR: join(dir, 'config') };
    for (const d of Object.values(env)) mkdirSync(d, { recursive: true });
    const port = await freePort(), control = kind === 'js' ? await freePort() : port;
    const pub = `http://127.0.0.1:${capture.port}/publish/${name === 'a' ? 'a' : 'b'}`;
    Object.assign(env, { HOST: '127.0.0.1', PORT: String(port), HUB_TOKEN: 'parity-token', PUBLISH_URL: pub, PUBLISH_TOKEN: 'pub-token',
      GITHUB_USER: 'parity-user', PATH: `${join(HERE, 'fakebin')}:${process.env.PATH}`, TZ: 'UTC' });
    let cmd, cargs;
    if (kind === 'js') {
      Object.assign(env, { PARITY_CONTROL_PORT: String(control), PARITY_NOW: String(S0) });
      cmd = process.execPath; cargs = ['--import', join(HERE, 'js-clock.mjs'), 'legacy/hub/hub.mjs'];
    } else {
      Object.assign(env, { AGENT_STATS_TEST_HOOKS: '1', AGENT_STATS_TICK_MS: '0', AGENT_STATS_NOW: String(S0) });
      writeFileSync(join(env.AGENT_STATS_CONFIG_DIR, 'hub.toml'), HUB_TOML);
      cmd = bin; cargs = ['hub', 'serve'];
    }
    const logFile = join(dir, `hub-${name}.log`), fd = openSync(logFile, 'w');
    const child = spawn(cmd, cargs, { cwd: ROOT, env, stdio: ['ignore', fd, fd] }); closeSync(fd);
    children.push(child); logs.push([name, logFile]);
    const stop = () => new Promise(r => { if (exited) return r(); child.once('exit', () => r()); child.kill('SIGTERM'); });
    let exited = null; child.on('exit', (c, s) => { exited = `exit ${c ?? s}`; });
    const base = `http://127.0.0.1:${port}`, ctl = `http://127.0.0.1:${control}`;
    const t0 = Date.now();
    for (;;) {
      if (exited) throw new Error(`hub ${name} died at startup (${exited})`);
      try { if ((await http(base, 'GET', '/api/public', undefined, {}, 5000)).status === 200) break; } catch {}
      if (Date.now() - t0 > STARTUP_MS) throw new Error(`hub ${name} did not answer /api/public within ${STARTUP_MS / 1000} s`);
      await new Promise(r => setTimeout(r, 100));
    }
    return { name, kind, base, ctl, stop };
  })();
}

// ---- capture server (stands in for the relay) --------------------------------------------------------------------
const capture = { port: 0, posts: { '/publish/a': [], '/publish/b': [] } };
async function startCapture() {
  const s = createServer((req, res) => {
    let body = ''; req.on('data', c => body += c);
    req.on('end', () => { (capture.posts[req.url] ||= []).push({ body, auth: req.headers.authorization }); res.writeHead(200); res.end('ok'); });
  });
  servers.push(s);
  await new Promise(r => s.listen(0, '127.0.0.1', r)); capture.port = s.address().port;
}

// ---- comparison helpers -------------------------------------------------------------------------------------------
const isObj = v => v && typeof v === 'object';
function leaves(v) { return isObj(v) ? Object.values(v).reduce((n, x) => n + leaves(x), 0) : 1; }
function deepEqualUnordered(a, b) {
  if (!isObj(a) || !isObj(b)) return Object.is(a, b);
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a), kb = Object.keys(b);
  return ka.length === kb.length && ka.every(k => k in b && deepEqualUnordered(a[k], b[k]));
}
function diffPaths(a, b, path = '$', out = []) {
  if (out.length >= 20) return out;
  if (isObj(a) && isObj(b) && Array.isArray(a) === Array.isArray(b)) {
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) diffPaths(a[k], b[k], `${path}${Array.isArray(a) ? `[${k}]` : '.' + k}`, out);
  } else if (!Object.is(a, b)) out.push(`${path}: A=${JSON.stringify(a)} B=${JSON.stringify(b)}`);
  return out;
}
// Normalization (opt-in via --lenient-agents): hub.mjs sorts live.agents with a comparator that is inconsistent for two
// "main" (or two "subagent") entries of the same agent, so their relative order depends on V8's sort internals. With the
// flag both sides are re-sorted by (agent, kind, id) before comparing. Off by default: a port should match JS exactly.
function normalizeAgents(p) {
  if (LENIENT_AGENTS && p?.live?.agents) p.live.agents.sort((x, y) => (x.agent + x.kind + x.id).localeCompare(y.agent + y.kind + y.id));
  return p;
}
const stats = { steps: 0, comparisons: 0, fields: 0, failures: 0 };
const failureLines = [];
function compare(label, a, b) {
  stats.comparisons++; stats.fields += leaves(a);
  const sa = JSON.stringify(a), sb = JSON.stringify(b);
  if (sa === sb) return true;
  stats.failures++;
  failureLines.push(`MISMATCH ${label}: ${deepEqualUnordered(a, b) ? 'equal ignoring key order (key order differs)' : 'values differ'}`);
  for (const d of diffPaths(a, b)) failureLines.push('    ' + d);
  return false;
}
const check = (label, ok, detail = '') => { stats.comparisons++; stats.fields++; if (!ok) { stats.failures++; failureLines.push(`CHECK FAILED ${label} ${detail}`); } };
const STATE_KEYS = ['working', 'subagents', 'open', 'byAgent', 'week', 'allTime', 'firstDay', 'daily', 'today', 'history'];
const pick = (o, ks) => Object.fromEntries(ks.map(k => [k, o[k]]));

// ---- scenario data ----------------------------------------------------------------------------------------------------
const h32 = s => { let h = 2166136261; for (const c of s) h = Math.imul(h ^ c.charCodeAt(0), 16777619) >>> 0; return h; };
const tokyoDay = ms => new Date(ms + 9 * 3_600_000).toISOString().slice(0, 10);
const dayStart = day => Date.parse(day + 'T00:00:00+09:00');
const growth = (day, now) => 1 + 1.5 * Math.min(1, Math.max(0, (now - dayStart(day)) / 86_400_000));
const STATES = ['thinking', 'writing', 'run', 'edit', 'read', 'delegate', 'waiting', 'tool'];
const EXPECTED = readFileSync(join(ROOT, 'tests/golden/expected.json'), 'utf8');

const COMBOS = {
  alpha: [['claude', 'claude-opus-4-5'], ['claude', 'claude-sonnet-4-5'], ['claude', 'claude-haiku-4-5'], ['claude', '<synthetic>'],
    ['codex', 'gpt-5.5'], ['codex', 'gpt-5.4-mini'], ['grok', 'grok-4']],
  beta: [['claude', 'claude-opus-4-5'], ['claude', 'claude-sonnet-4-5'], ['claude', null], ['codex', 'gpt-5.3-codex'], ['codex', 'gpt-5.5'],
    ['grok', 'grok-code-fast'], ['grok', 'unknown']],
  gamma: [],
};
const REPOS = { alpha: ['neon', 'space-rock-2', 'chum', 'site'], beta: ['poe-mobile-app', 'NEON-Tools', 'chum-tools', 'content-hall', 'my-content-hall', 'blog'], gamma: ['chum'] };
const PLATFORM = { alpha: 'linux', beta: 'darwin', gamma: 'linux' };
const LABEL = { alpha: 'Alpha Linux', beta: 'Beta Mac', gamma: 'Gamma' };

function rows(m, now, scale, onlyToday, agentFilter) {
  const days = [], repos = [], today = tokyoDay(now);
  for (const day of onlyToday ? [today] : [tokyoDay(now - 86_400_000), today]) {
    const g = growth(day, now) * scale;
    for (const [agent, model] of COMBOS[m]) {
      if (agentFilter && agent !== agentFilter) continue;
      const tokens = Math.round((50e6 + h32(`${m}|${agent}|${model}`) % 300e6) * g);
      const row = { day, agent, tokens, output: Math.round(tokens / 40) };
      if (model !== null) row.model = model;
      days.push(row);
    }
    for (const repo of REPOS[m].slice(0, onlyToday ? 2 : 99))
      repos.push({ day, repo, tokens: Math.round((2e6 + h32(`${m}|${repo}`) % 20e6) * g), ...(m !== 'alpha' ? { remote: `github.com/kcazin/${repo}` } : {}) });
  }
  if (m === 'gamma') {   // small and zero rows, a missing output; repos exceed the day total. (Fractional, negative and
    // numeric-string counts, which hub.mjs coerces, are refused by the Rust hub with 422: see INVALID below.)
    days.push({ day: tokyoDay(now), agent: 'grok', model: 'grok-4', tokens: 1000000, output: 10 },
      { day: tokyoDay(now), agent: 'codex', model: 'gpt-5.5', tokens: 0, output: 0 },
      { day: tokyoDay(now), agent: 'claude', model: 'claude-haiku-4-5', tokens: 7e6 });
    repos.push({ day: tokyoDay(now), repo: 'chum', tokens: 3e6, remote: 'github.com/kcazin/chum' });
  }
  return { days, repos };
}

const ioOf = (j, k, scale = 1000) => ({ in: 100 + j * 3 + k * scale, cr: 5000 + j * 11 + k * scale * 4, cw: 50 + j + k * 10, out: 20 + j * 2 + k * Math.round(scale / 4) });
function alphaSessions(now, k) {
  const base = JSON.parse(EXPECTED.replaceAll('{{HOME}}', '/home/alpha')).sessions;
  const table = [7000, 20000, 59999, 61000, 300000, 2_000_000];
  return base.map((s, j) => {
    const phase = (k + j) % 6;
    let off = table[phase] + j * 3;
    if (j === 3 && k % 5 === 0) off = 60000;   // exactly at the WORKING boundary: not working
    const o = { ...s, lastActivity: now - off, tokens: s.tokens + k * 1000 };
    if (o.stateAt === '~now') o.stateAt = now - 5000;
    if (o.io) { o.io = ioOf(j, j === 1 && k % 7 === 0 ? 0 : k); }   // session 1's io drops back every 7th step: flow clamps at 0
    return o;
  });
}
function betaSessions(now, k) {
  const st = i => STATES[(k + i) % 8], io = (j, kk = k) => ioOf(j + 40, kk, 700);
  const s = [
    { id: 'beta-main-work', agent: 'claude', kind: 'main', parent: null, model: 'claude-sonnet-4-5', cwd: '/Users/beta/Dev/neon', repo: 'neon', project: 'neon',
      state: st(0), stateAt: now - 3000, lastActivity: now - 10_000, tokens: 1000 * k, io: io(0) },
    { id: 'beta-main-idle', agent: 'claude', kind: 'main', parent: null, model: 'claude-opus-4-5', cwd: '/Users/beta/Dev/chum', repo: 'chum', project: 'chum',
      state: st(1), stateAt: now - 400_000, lastActivity: k % 4 === 3 ? now - 20_000 : now - 300_000, tokens: 5, io: io(1) },
    { id: 'beta-sub-1', agent: 'claude', kind: 'subagent', parent: 'beta-main-idle', model: 'claude-haiku-4-5', cwd: '/Users/beta/Dev/chum', repo: 'chum', project: 'chum',
      state: st(2), stateAt: now - 1000, lastActivity: k % 5 !== 4 ? now - 5000 : now - 200_000, tokens: 7, io: io(2) },
    { id: 'beta-sub-orphan', agent: 'claude', kind: 'subagent', parent: '11111111-aaaa-4bbb-8ccc-000000000001', model: 'claude-fable-5', cwd: '/x', repo: 'blog',
      project: 'blog', state: st(3), stateAt: null, lastActivity: now - 8000, tokens: 1, io: io(3) },
    { id: 'rollout-beta-1', agent: 'codex', kind: 'main', parent: null, model: 'gpt-5.4-mini', cwd: '/Users/beta/Dev/NEON-Tools', repo: 'NEON-Tools', project: 'NEON-Tools',
      state: 'edit', stateAt: null, lastActivity: now - 30_000, tokens: 2, io: io(4) },
    { id: 'rollout-beta-2', agent: 'codex', kind: 'main', parent: null, model: 'gpt-6-astra', cwd: '/Users/beta/Dev/site', repo: 'site', project: 'site',
      state: st(5), stateAt: now - 9000, lastActivity: now - 55_000, tokens: 2, io: io(5) },
    { id: 'grok-beta-1', agent: 'grok', kind: 'main', parent: null, model: 'grok-4.6-build', cwd: '/Users/beta/Dev/poe-mobile-app', repo: null, project: 'poe-mobile-app',
      ...(k % 6 === 0 ? {} : { state: 'waiting' }), stateAt: now - 2000, lastActivity: now - 45_000, tokens: 3, io: null },
    { id: 'dup-id', agent: 'claude', kind: 'main', parent: null, model: null, cwd: '/Users/beta/Dev/content-hall', repo: 'content-hall', project: 'content-hall',
      state: st(6), stateAt: now - 100, lastActivity: now - 12_000, tokens: 4 },
    { id: 'beta-mch', agent: 'claude', kind: 'main', parent: null, model: 'claude-sonnet-4', cwd: '/Users/beta/Dev/my-content-hall', repo: 'my-content-hall', project: 'my-content-hall',
      state: st(7), stateAt: now - 700, lastActivity: now - 25_000, tokens: 4, io: io(7) },
  ];
  return s;
}
const gammaSessions = (now, k) => [{ id: 'dup-id', agent: 'codex', kind: 'main', parent: null, model: 'gpt-5.5', cwd: '/g/chum', repo: 'chum', project: 'chum',
  state: 'tool', stateAt: now - 4000, lastActivity: now - 20_000, tokens: 1, io: ioOf(70, k) }];

function buildReport(m, mode, now, k) {
  const r = rows(m, now, mode === 'smaller' ? 0.5 : 1, mode === 'partial', mode === 'partial' ? 'claude' : undefined);
  const sessions = { alpha: alphaSessions, beta: betaSessions, gamma: gammaSessions }[m](now, k);
  const rep = { machine: m, label: LABEL[m], platform: PLATFORM[m], at: now, full: mode !== 'partial', days: r.days, repos: r.repos, sessions };
  if (m === 'alpha') {
    if (mode === 'full') {   // the golden report's historical rows (constant across steps)
      const g = JSON.parse(EXPECTED.replaceAll('{{HOME}}', '/home/alpha'));
      // The Rust hub refuses days after the hub's tomorrow, which a real reporter never sends: keep the golden rows
      // that exist by then (the same body goes to both hubs).
      const known = x => x.day <= tokyoDay(now + 86_400_000);
      rep.days = [...g.days.filter(known), ...r.days]; rep.repos = [...g.repos.filter(known), ...r.repos];
    }
    return rep;
  }
  return { protocol: 2, version: '0.2.0', machineId: m, ...rep };
}

// Steps: { now, reports: [{m, mode}] }. Tick-only steps have no reports.
function scenario() {
  const steps = [], at = (sec, reports = []) => steps.push({ now: S0 + sec * 1000, reports });
  const R = (m, mode = 'full') => ({ m, mode });
  const ALL = [R('alpha'), R('beta'), R('gamma')], AB = [R('alpha'), R('beta')];
  at(0, ALL); at(15, ALL); at(30, AB); at(45, AB);
  at(60, AB);                                // gamma last seen 45 s ago: offline (strictly less than 45 s is online)
  at(75, AB); at(90, [R('alpha', 'smaller'), R('beta')]); at(105, [R('alpha', 'partial'), R('beta', 'partial')]);
  at(120, [R('alpha')]); at(135); at(150, AB); at(165, [R('beta')]);   // beta silent for 135 s then back
  for (const s of [300, 600, 1800, 3600, 7200, 14400, 21600]) at(s, AB);
  const mids = [25200, 111600, 198000];      // 15:00Z = Tokyo midnight, three times
  mids.forEach((M, i) => {
    for (const d of [-900, -30, -15, -1, 0, 1, 15, 30, 45, 60, 75, 900, 3600]) at(M + d, d % 2 ? AB : [R('alpha'), R('beta', d % 3 ? 'full' : 'partial')]);
    if (i < 2) for (let j = 1; j <= 6; j++) at(M + 3600 + 10800 * j, [R('alpha', j % 3 === 1 ? 'smaller' : 'full'), ...(j % 2 ? [R('beta')] : [])]);
  });
  for (let j = 1; j <= 5; j++) at(198000 + 3600 + 15000 * j, j % 2 ? AB : [R('alpha')]);
  const E = 300000; at(E, ALL);              // everyone reports once more (gamma's last report; gamma's rows persist)
  const D = 86_400;
  at(6.9 * D, [R('beta')]);                  // gamma last seen < 7 days ago: still counted in machinesTotal
  at(7.5 * D, [R('beta')]); at(7.5 * D + 15);   // gamma > 7 days: dropped
  at(E + 7.5 * D, [R('beta')]); at(E + 7.5 * D + 15, [R('beta')]);   // alpha > 7 days: dropped
  return steps;
}

// ---- main ---------------------------------------------------------------------------------------------------------------
async function main() {
  const t0 = Date.now();
  mkdirSync(SCRATCH, { recursive: true });
  await startCapture();
  const [A, B] = await Promise.all([startHub('a', 'js'), startHub('b', JS_VS_JS ? 'js' : 'rust', RUST)]);
  const both = (fn) => Promise.all([fn(A), fn(B)]);
  const post = (h, path, body, headers) => http(h.ctl, 'POST', path, body, { 'content-type': 'application/json', ...headers });
  const control = async (path, body = '{}') => {
    const [ra, rb] = await both(h => post(h, path, body));
    check(`${path} -> 200 on both`, ra.status === 200 && rb.status === 200, `A=${ra.status} ${ra.text} B=${rb.status} ${rb.text}`);
  };
  const getJson = async (h, path) => JSON.parse((await http(h.base, 'GET', path)).text);
  const publicBodies = async () => {
    const [a, b] = await both(h => getJson(h, '/api/public'));
    if (!JS_VS_JS) {   // the Rust hub also lists the configured projects; JS has no projectList
      compare('/api/public projectList (B vs config)', PROJECT_LIST, b.projectList);
      delete b.projectList;
    }
    return [normalizeAgents(a), normalizeAgents(b)];
  };

  await control('/__test/clock', JSON.stringify({ now: S0 }));
  await control('/__test/github');    // fills the github table on both (hub.mjs also fetched once at startup)
  { const [a, b] = await publicBodies(); compare('empty /api/public', a, b); }

  // Error paths
  { const [ra, rb] = await both(h => http(h.base, 'POST', '/report', '{}', { authorization: 'Bearer wrong' }));
    check('POST /report with bad token -> 401', ra.status === 401 && rb.status === 401, `A=${ra.status} B=${rb.status}`);
    const [na, nb] = await both(h => http(h.base, 'GET', '/nope'));
    check('GET /nope -> 404', na.status === 404 && nb.status === 404, `A=${na.status} B=${nb.status}`);
    compare('401 body', JSON.parse(ra.text), JSON.parse(rb.text)); compare('404 body', JSON.parse(na.text), JSON.parse(nb.text)); }

  // Reports hub.mjs would coerce or choke on, which the Rust hub refuses without touching its database. Sent to the
  // Rust hub only (sending them to hub.mjs would change its data), after real data exists (step 3), checking that B's
  // /api/public is unchanged.
  const INVALID = [
    [422, { machine: 'gamma', days: [{ day: '2026-10-10', agent: 'grok', model: 'grok-4', tokens: 1000000.5, output: 1 }] }],
    [422, { machine: 'gamma', days: [{ day: '2026-10-10', agent: 'codex', model: 'gpt-5.5', tokens: -5, output: 0 }] }],
    [422, { machine: 'gamma', days: [{ day: '2026-10-10', agent: 'claude', model: 'x', tokens: '7e6', output: '12' }] }],
    [422, { machine: 'gamma', days: [{ day: '10/10/2026', agent: 'claude', model: 'x', tokens: 1, output: 1 }] }],
    [422, { machine: 'gamma', days: [{ day: '2026-10-10', agent: 'claude', model: 'x', tokens: 1e16, output: 1 }] }],
    [422, { machine: '', days: [] }],
    [422, { machine: 'gamma', sessions: [{ id: 'x', agent: 'claude', kind: 'main' }] }],
    [422, { machine: 'gamma', repos: 'nope' }],
    [426, { machine: 'gamma', protocol: 3, days: [] }],
  ];
  const invalidChecks = async () => {
    if (JS_VS_JS) return;
    const before = await getJson(B, '/api/public');
    for (const [want, body] of INVALID) {
      const r = await http(B.base, 'POST', '/report', JSON.stringify(body), { authorization: AUTH, 'content-type': 'application/json' });
      check(`invalid report -> ${want}: ${JSON.stringify(body).slice(0, 80)}`, r.status === want, `B=${r.status} ${r.text}`);
      if (want === 426) check('426 lists supported protocols', JSON.stringify(JSON.parse(r.text).supported) === '[1,2]', r.text);
    }
    const nj = await http(B.base, 'POST', '/report', 'not json', { authorization: AUTH });
    check('non-JSON report -> 422', nj.status === 422, `B=${nj.status}`);
    compare('Rust /api/public unchanged by refused reports', before, await getJson(B, '/api/public'));
  };

  const steps = scenario(), published = { a: 0, b: 0 };
  for (const [k, s] of steps.entries()) {
    const tag = `step ${k} @${new Date(s.now).toISOString()}`;
    await control('/__test/clock', JSON.stringify({ now: s.now }));
    for (const { m, mode } of s.reports) {
      const body = JSON.stringify(buildReport(m, mode, s.now, k));
      const [ra, rb] = await both(h => http(h.base, 'POST', '/report', body, { authorization: AUTH, 'content-type': 'application/json' }));
      check(`${tag} report ${m}/${mode} -> 200`, ra.status === 200 && rb.status === 200, `A=${ra.status} ${ra.text} B=${rb.status} ${rb.text}`);
    }
    { const [a, b] = await publicBodies(); compare(`${tag} /api/public before tick`, a, b); }
    await control('/__test/tick');
    { const [a, b] = await publicBodies(); compare(`${tag} /api/public after tick`, a, b); }
    { const [a, b] = await both(h => getJson(h, '/api/state'));
      compare(`${tag} /api/state subset`, pick(a, STATE_KEYS), pick(b, STATE_KEYS)); }
    // What each hub published to the relay on this tick (includes projects.last/lastMs and agents[].flow)
    const pa = capture.posts['/publish/a'], pb = capture.posts['/publish/b'];
    check(`${tag} one publish per hub`, pa.length === ++published.a && pb.length === ++published.b, `A=${pa.length} B=${pb.length}`);
    if (pa.length && pb.length) {
      check(`${tag} publish Authorization`, pa.at(-1).auth === PUB_AUTH && pb.at(-1).auth === PUB_AUTH, `A=${pa.at(-1).auth} B=${pb.at(-1).auth}`);
      const a = normalizeAgents(JSON.parse(pa.at(-1).body)), b = normalizeAgents(JSON.parse(pb.at(-1).body));
      if (!JS_VS_JS) delete b.projectList;
      compare(`${tag} published body`, a, b);
    }
    if (KEEP) writeFileSync(join(SCRATCH, `published-a-step${k}.json`), pa.at(-1)?.body ?? '');   // for eyeballing with --keep
    stats.steps++;
    if (k === 3) await invalidChecks();
    if (failureLines.length > 60) break;
  }

  // Takeover: the Rust hub opens the JS hub's hub.db in place (same state dir) and must serve the same /api/public
  // at the same instant. Then the JS hub reopens the database the Rust hub has written to (rollback) and must too.
  if (!JS_VS_JS && failureLines.length === 0) {
    const last = steps.at(-1).now + 15_000;
    const atLast = async h => {
      const r = await post(h, '/__test/clock', JSON.stringify({ now: last }));
      check(`${h.name} clock`, r.status === 200, r.text);
      const p = await getJson(h, '/api/public'); delete p.projectList; return p;
    };
    const jsBefore = await atLast(A);
    await A.stop();
    const C = await startHub('c', 'rust', RUST, 'a');
    compare('takeover: Rust on the JS hub.db, /api/public', jsBefore, await atLast(C));
    const body = JSON.stringify(buildReport('beta', 'full', last, steps.length));
    const rc = await http(C.base, 'POST', '/report', body, { authorization: AUTH, 'content-type': 'application/json' });
    check('takeover: report to the Rust hub -> 200', rc.status === 200, rc.text);
    const rustAfter = await atLast(C);
    await C.stop();
    const D = await startHub('d', 'js', undefined, 'a');
    compare('rollback: JS on the Rust-written hub.db, /api/public', rustAfter, await atLast(D));
    await D.stop();
  }
  return Date.now() - t0;
}

let ms = 0, fatal = null;
try { ms = await main(); } catch (e) { fatal = e; }
cleanup();
const ok = !fatal && stats.failures === 0;
if (fatal) console.error('FATAL:', fatal.stack || fatal);
for (const l of failureLines.slice(0, 120)) console.error(l);
if (!ok) for (const [n, f] of logs) if (existsSync(f)) console.error(`--- hub ${n} log (${f}) ---\n${readFileSync(f, 'utf8').slice(-4000)}`);
console.log(`${JS_VS_JS ? 'js-vs-js' : 'js-vs-rust'}: ${stats.steps} steps, ${stats.comparisons} comparisons, ${stats.fields} fields compared, ${stats.failures} failures, ${(ms / 1000).toFixed(1)} s: ${ok ? 'PASS' : 'FAIL'}`);
if (ok && !KEEP) rmSync(SCRATCH, { recursive: true, force: true });
else console.log(`scratch kept: ${SCRATCH}`);
process.exit(ok ? 0 : 1);
