// Public relay for the agent stats. The hub on the Mac mini POSTs the public payload here every 15s;
// kilcazn.com reads it from GET /stats. Holds only the latest payload, persisted to the volume.
// GET /presence?id=<per-tab id> counts the site's visitors: ids pinged in the last 45s, in memory only.

const TOKEN = Bun.env.PUBLISH_TOKEN || '';
const FILE = (Bun.env.DATA_DIR || '/data') + '/stats.json';
const ORIGINS = (Bun.env.ALLOWED_ORIGINS || 'https://kilcazn.com,https://www.kilcazn.com').split(',');

let latest: string | null = null;
try { latest = await Bun.file(FILE).text(); } catch {}

const PRESENCE_TTL = 45_000, PRESENCE_MAX = 10_000;
const seen = new Map<string, number>();   // id -> last ping (performance.now())
const online = (id: string | null) => {
  const now = performance.now();
  for (const [k, t] of seen) if (now - t >= PRESENCE_TTL) seen.delete(k);
  if (id && (seen.has(id) || seen.size < PRESENCE_MAX)) seen.set(id, now);
  return seen.size;
};

const cors = (req: Request) => {
  const origin = req.headers.get('origin') || '';
  const ok = ORIGINS.includes(origin) || /^http:\/\/localhost(:\d+)?$/.test(origin);
  return ok ? { 'access-control-allow-origin': origin, vary: 'origin' } : {};
};

Bun.serve({
  port: Number(Bun.env.PORT || 3000),
  async fetch(req) {
    const url = new URL(req.url), { pathname } = url;
    if (req.method === 'POST' && pathname === '/publish') {
      if (!TOKEN || req.headers.get('authorization') !== `Bearer ${TOKEN}`) return new Response('unauthorized', { status: 401 });
      const body = await req.text();
      if (body.length > 512 * 1024) return new Response('too large', { status: 413 });
      JSON.parse(body);
      latest = body;
      await Bun.write(FILE, body).catch(err => console.warn('persist failed:', err.message));
      return new Response('ok');
    }
    if (req.method === 'GET' && pathname === '/stats') {
      if (!latest) return new Response('no data yet', { status: 503, headers: cors(req) });
      return new Response(latest, {
        headers: { 'content-type': 'application/json', 'cache-control': 'public, max-age=5', ...cors(req) },
      });
    }
    if (pathname === '/presence') {
      if (req.method === 'OPTIONS') return new Response(null, { status: 204, headers: { ...cors(req), 'access-control-allow-methods': 'GET, OPTIONS', 'access-control-max-age': '600' } });
      if (req.method !== 'GET' && req.method !== 'HEAD') return new Response('method not allowed', { status: 405 });
      const id = url.searchParams.get('id');
      if (id !== null && !/^[A-Za-z0-9_-]{8,64}$/.test(id)) return new Response('bad id', { status: 400, headers: { 'cache-control': 'no-store', ...cors(req) } });
      return new Response(JSON.stringify({ online: online(id) }), {
        headers: { 'content-type': 'application/json', 'cache-control': 'no-store', ...cors(req) },
      });
    }
    if (pathname === '/') return new Response('ok');
    return new Response('not found', { status: 404 });
  },
});
