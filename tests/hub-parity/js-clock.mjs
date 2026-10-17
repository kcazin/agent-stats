// Preload for the legacy JS hub (`node --import js-clock.mjs legacy/hub/hub.mjs`): makes it controllable without
// editing hub.mjs. Frozen Date, captured timers, observable `gh` calls, and a control HTTP server.
import cp from 'node:child_process';
import http from 'node:http';
import { syncBuiltinESMExports } from 'node:module';

// ---- frozen clock -----------------------------------------------------------------------------------------------
const RealDate = Date;
let nowMs = Number(process.env.PARITY_NOW) || RealDate.now();
class FakeDate extends RealDate {
  constructor(...args) { if (args.length === 0) super(nowMs); else super(...args); }
  static now() { return nowMs; }
}
globalThis.Date = FakeDate;

// ---- timers: capture the hub's 15 s tick and hourly GitHub fetch instead of scheduling them ---------------------
let tickHook = null, githubHook = null;
const realSetInterval = globalThis.setInterval;
globalThis.setInterval = function (fn, delay, ...rest) {
  if (delay === 15_000) { tickHook = fn; return { ref() { return this; }, unref() { return this; }, hasRef: () => false }; }
  if (delay === 3_600_000) { githubHook = fn; return { ref() { return this; }, unref() { return this; }, hasRef: () => false }; }
  return realSetInterval(fn, delay, ...rest);
};

// ---- observable GitHub fetch: resolve a promise after the hub's execFile callback has run -------------------------
let pendingGithub = [];
const realExecFile = cp.execFile;
cp.execFile = function (file, args, opts, cb) {
  if (typeof opts === 'function') { cb = opts; opts = undefined; }
  let done;
  pendingGithub.push(new Promise(r => { done = r; }));
  const wrapped = (...a) => { try { cb && cb(...a); } finally { done(); } };
  return opts === undefined ? realExecFile(file, args, wrapped) : realExecFile(file, args, opts, wrapped);
};
syncBuiltinESMExports();

// ---- control server ---------------------------------------------------------------------------------------------
const port = Number(process.env.PARITY_CONTROL_PORT || 0);
if (port) {
  const reply = (res, code, body) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
  http.createServer(async (req, res) => {
    try {
      let body = ''; for await (const c of req) body += c;
      if (req.method !== 'POST') return reply(res, 404, { error: 'not found' });
      if (req.url === '/__test/clock') { nowMs = Number(JSON.parse(body).now); return reply(res, 200, { ok: true }); }
      if (req.url === '/__test/tick') { await tickHook(); return reply(res, 200, { ok: true }); }
      if (req.url === '/__test/github') {
        githubHook();
        const waits = pendingGithub; pendingGithub = [];
        await Promise.all(waits);
        return reply(res, 200, { ok: true });
      }
      reply(res, 404, { error: 'not found' });
    } catch (err) { reply(res, 500, { error: String(err && err.message || err) }); }
  }).listen(port, '127.0.0.1');
}
