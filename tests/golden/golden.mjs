#!/usr/bin/env node
// Golden-payload harness for the agent-stats reporter. Runs any reporter implementation against a fixed fake $HOME
// and compares the report it POSTs with test/expected.json (and expected2.json, ... for later ticks).
//
//   node tests/golden/golden.mjs --update                 # (re)write expected*.json from the command under test
//   node tests/golden/golden.mjs                          # check the JS reporter
//   node tests/golden/golden.mjs --cmd "target/release/agent-stats-reporter"
//   node tests/golden/golden.mjs --ticks 2 [--update]     # run twice against the same state dir
//
// Options: --expected <name> (read <name>.json and <name>2.json, default `expected`), --cmd "<shell command>" (run from the repo root, `--once` is appended), --ticks N, --update, --keep (keep temp dir).
// See README.md for what the fixtures cover.

import { mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync, appendFileSync, utimesSync, rmSync, existsSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, '..', '..');
const FIXTURES = join(HERE, 'fixtures', 'home');
const SCRATCH = '/var/tmp/agents/agent-stats-golden';

const argv = process.argv.slice(2);
const flag = n => argv.includes(n);
const opt = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const UPDATE = flag('--update');
const KEEP = flag('--keep');
const TICKS = Number(opt('--ticks', 1));
const EXPECTED = opt('--expected', 'expected');
const CMD = opt('--cmd', 'node legacy/reporter/reporter.mjs');

// Files whose mtime is 2 hours ago (outside the 30 minute "open session" window); everything else is "now".
const OLD = new Set([
  '.claude/projects/-t3-wt/33333333-aaaa-4bbb-8ccc-000000000003.jsonl',
  '.claude/projects/-srv/99999999-aaaa-4bbb-8ccc-000000000009.jsonl',
  '.codex/archived_sessions/rollout-2026-09-01T09-00-00-019a0000-0000-7000-8000-00000000000d.jsonl',
  '.grok/sessions/%2FUsers%2Fme%2Fmy%20proj/g3000000-aaaa-4bbb-8ccc-000000000003/updates.jsonl',
]);

// Line appended to this transcript before every tick after the first (it ends with a newline, so the line is whole).
const APPEND_TO = '.claude/projects/-game-wt/44444444-aaaa-4bbb-8ccc-000000000004.jsonl';
const appendLine = n => JSON.stringify({ type: 'assistant', timestamp: '2026-10-12T03:00:00.000Z', cwd: '{{HOME}}/Projects/game/.claude/worktrees/agent-1',
  message: { id: `msg_TICK${n}`, model: 'claude-sonnet-4-5', role: 'assistant', content: [{ type: 'text', text: 'tick' }],
    usage: { input_tokens: 1, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: 9 } } }) + '\n';

const subst = (s, HOME) => s.replaceAll('{{HOME}}', HOME).replaceAll('%7B%7BHOME%7D%7D', encodeURIComponent(HOME));

function copyTree(src, dst, HOME, rel = '') {
  mkdirSync(dst, { recursive: true });
  for (const e of readdirSync(src, { withFileTypes: true })) {
    const r = rel ? `${rel}/${e.name}` : e.name;
    const to = join(dst, subst(e.name, HOME));
    if (e.isDirectory()) copyTree(join(src, e.name), to, HOME, r);
    else {
      writeFileSync(to, subst(readFileSync(join(src, e.name), 'utf8'), HOME));
      const t = (Date.now() - (OLD.has(r) ? 2 * 3600_000 : 60_000)) / 1000;
      utimesSync(to, t, t);
    }
  }
}

function sortKeys(v) {
  if (Array.isArray(v)) return v.map(sortKeys);
  if (v && typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map(k => [k, sortKeys(v[k])]));
  return v;
}
const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

// Drop volatile fields, bucket timestamps relative to now, and sort every list by a stable key.
function normalize(body, HOME) {
  const now = Date.now();
  const b = JSON.parse(JSON.stringify(body).replaceAll(HOME, '{{HOME}}'));
  delete b.at; delete b.platform; delete b.version; // version changes every release
  const near = t => Math.abs(now - t) < 10 * 60_000;
  b.days = (b.days || []).sort((x, y) => cmp(`${x.day}|${x.agent}|${x.model}`, `${y.day}|${y.agent}|${y.model}`));
  b.repos = (b.repos || []).sort((x, y) => cmp(`${x.day}|${x.repo}`, `${y.day}|${y.repo}`));
  b.sessions = (b.sessions || []).map(s => ({
    ...s,
    lastActivity: s.lastActivity == null ? null : near(s.lastActivity) ? '~now' : `${Math.round((now - s.lastActivity) / 60000)}min-ago`,
    stateAt: s.stateAt == null ? null : near(s.stateAt) ? '~now' : s.stateAt,
  })).sort((x, y) => cmp(`${x.agent}|${x.id}|${x.cwd}`, `${y.agent}|${y.id}|${y.cwd}`));
  return sortKeys(b);
}

function diffText(a, b) {
  const d = join(SCRATCH, `diff-${process.pid}`);
  mkdirSync(d, { recursive: true });
  writeFileSync(join(d, 'expected'), a); writeFileSync(join(d, 'actual'), b);
  const r = spawnSync('diff', ['-u', join(d, 'expected'), join(d, 'actual')], { encoding: 'utf8' });
  rmSync(d, { recursive: true, force: true });
  return r.error ? `(diff unavailable)\n--- expected\n${a}\n--- actual\n${b}` : r.stdout.replace(/^(---|\+\+\+) .*$/gm, m => m.slice(0, 3) + (m.startsWith('---') ? ' expected' : ' actual'));
}

mkdirSync(SCRATCH, { recursive: true });
const ROOT = mkdtempSync(join(SCRATCH, 'run-'));
const HOME = join(ROOT, 'home');
const STATE = join(ROOT, 'state');
copyTree(FIXTURES, HOME, HOME);

const bodies = [];
let authFailures = 0;
const server = createServer((req, res) => {
  let data = '';
  req.on('data', c => data += c);
  req.on('end', () => {
    if (req.method !== 'POST' || req.url !== '/report') { res.writeHead(404).end(); return; }
    if (req.headers.authorization !== 'Bearer test') { authFailures++; res.writeHead(401).end(); return; }
    try { bodies.push(JSON.parse(data)); } catch { res.writeHead(400).end(); return; }
    res.writeHead(200, { 'content-type': 'application/json' }).end('{"ok":true}');
  });
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

function run() {
  return new Promise(resolve => {
    const p = spawn('sh', ['-c', `${CMD} --once`], { cwd: REPO, stdio: ['ignore', 'pipe', 'pipe'], env: {
      ...process.env, HOME, HUB_URL: `http://127.0.0.1:${port}`, HUB_TOKEN: 'test', STATS_MACHINE: 'fixture', STATS_LABEL: 'Fixture', XDG_STATE_HOME: STATE,
      // The JS reporter always counts in Asia/Tokyo; the Rust one defaults to this machine's zone.
      STATS_TZ: 'Asia/Tokyo',
      // Never read the real config: the fake HOME's config dir is empty, so only the env hub above exists.
      AGENT_STATS_CONFIG_DIR: join(HOME, '.config', 'agent-stats'), XDG_CONFIG_HOME: join(HOME, '.config'), XDG_DATA_HOME: join(HOME, '.local', 'share') } });
    let out = '';
    p.stdout.on('data', c => out += c); p.stderr.on('data', c => out += c);
    const timer = setTimeout(() => p.kill('SIGKILL'), 60_000);
    p.on('close', code => { clearTimeout(timer); resolve({ code, out }); });
  });
}

let failed = false;
try {
  for (let n = 1; n <= TICKS; n++) {
    if (n > 1) {
      const f = join(HOME, APPEND_TO);
      appendFileSync(f, subst(appendLine(n), HOME));
      const t = (Date.now() - 60_000) / 1000;
      utimesSync(f, t, t);
    }
    const before = bodies.length;
    const { code, out } = await run();
    if (code !== 0 || bodies.length !== before + 1) {
      console.error(`tick ${n}: command exited ${code}, ${bodies.length - before} report(s) received (want 1), ${authFailures} auth failure(s)\n${out}`);
      failed = true; break;
    }
    const actual = JSON.stringify(normalize(bodies[bodies.length - 1], HOME), null, 2) + '\n';
    const file = join(HERE, n === 1 ? `${EXPECTED}.json` : `${EXPECTED}${n}.json`);
    if (UPDATE) { writeFileSync(file, actual); console.log(`tick ${n}: wrote ${relative(REPO, file)}`); continue; }
    if (!existsSync(file)) { console.error(`tick ${n}: ${relative(REPO, file)} missing (run with --update)`); failed = true; continue; }
    const expected = readFileSync(file, 'utf8');
    if (expected === actual) console.log(`tick ${n}: ok (${relative(REPO, file)})`);
    else { console.error(`tick ${n}: MISMATCH against ${relative(REPO, file)}\n${diffText(expected, actual)}`); failed = true; }
  }
} finally {
  server.close();
  if (KEEP) console.log(`kept ${ROOT}`); else rmSync(ROOT, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
