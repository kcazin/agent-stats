# Deploying the relay (and a cloud hub)

The `agent-stats` image is one binary with every role. The default command is `relay serve`; override the command to
run `hub serve` instead. The image is built from the repo root `Dockerfile` (multi-stage, Debian slim, non-root user
`agent-stats` uid 10001, `ENTRYPOINT ["agent-stats"]`, `CMD ["relay","serve"]`, state under `/data`).

## Relay configuration

Flags with environment fallbacks (use the environment in containers):

| Flag | Env | Default |
|---|---|---|
| `--listen ADDR` | `LISTEN`, else `PORT` | `0.0.0.0:$PORT`, PORT defaults to 8080 (the image sets `PORT=8080`) |
| `--data DIR` | `DATA_DIR` | `/data` if it exists, else `state_dir()` (the image sets `DATA_DIR=/data`) |
| `--token-file FILE` | `PUBLISH_TOKEN` | none: with no token every publish is refused (401) |
| `--cors ORIGINS` | `CORS_ORIGINS`, else `ALLOWED_ORIGINS` | empty (plus `http://localhost[:port]` for local dev); a warning is logged at start |
| `--stale-after SECS` | | 120 |
| `--no-status` | | status file `state_dir()/relay-status.json` is written |

Behaviour: `POST /publish` (Bearer token, body must be a JSON object, at most 512 KiB, `Content-Length` required)
stores it in memory and atomically in `<data>/stats.json`; a document whose numeric `updated` is older than the
stored one is acknowledged with `{"ok":true,"ignored":"older"}` and dropped; a failed disk write answers 500 and the
served snapshot does not advance. `GET /stats` serves the stored JSON, adding `"stale":true` once `updated` is older
than `--stale-after`. `GET /stream` is the same document as Server-Sent Events: `retry: 3000`, a full `event: stats`
on connect, then `event: patch` (RFC 7386 merge patches) on each accepted publish and each `stale` flip, and a
`: ping` comment every 15 s (contract in [ARCHITECTURE.md](ARCHITECTURE.md#wire-contracts)). Open streams don't use
connection slots (128 concurrent requests); they are capped at 1000 (503), and one that falls behind (256 KiB unsent,
or no progress for 5 s) is dropped and reconnects. There is no per-IP cap: behind Railway's proxy every client has
the proxy's address. `GET /presence?id=<tab id>` records the id (`[A-Za-z0-9_-]{8,64}`, else 400) in memory and answers
`{"online":N}`, the ids seen in the last 45 s (cap 10,000, never persisted); without `id` it only counts. `GET /healthz` answers `ok`. `GET /.well-known/agent-stats` answers
`{"role":"relay","version","protocol","updated"}`.

The file format is unchanged from the Bun relay (the plain JSON body), so an existing volume loads on first boot.

### Compared with the Bun relay (`legacy/relay/server.ts`)

| Bun reads | Rust relay | Same? |
|---|---|---|
| `PUBLISH_TOKEN` | `PUBLISH_TOKEN` (surrounding whitespace trimmed) | yes |
| `DATA_DIR` (default `/data`), file `$DATA_DIR/stats.json` | same variable and file; the image sets `DATA_DIR=/data` | yes |
| `ALLOWED_ORIGINS` (default `https://kilcazn.com,https://www.kilcazn.com`) | `CORS_ORIGINS`, else `ALLOWED_ORIGINS`; **default empty** | name yes, default no |
| `PORT` (default 3000) | `PORT` (image default 8080) | name yes, default no |

The origin default is deliberately not carried over: the binary is not kilcazn-specific, and an allowlist baked into
it would silently open `/stats` and `/stream` to a site nobody configured. The relay logs a warning when the list is
empty, and the cutover below sets the variable explicitly. The port default never matters on Railway, which injects
`PORT`.

## a. Cut over from the Bun relay on Railway, in place

An existing Railway service running the Bun relay (`legacy/relay`), with a volume at `/data`; its public URL is `$R`
below. Same service, same volume, same variables, so the hub's publish target and the site's URL don't change.


Checklist:

1. **Rollback target.** Note the current (Bun) deployment id (dashboard, or `railway deployment list`).
2. **Backup.** `railway ssh` into the service and `cp /data/stats.json /data/stats.json.bak` (optional; both relays
   read and write the same plain JSON file).
3. **Variables to keep / set** on the `relay` service:
   - `PUBLISH_TOKEN`: keep as is (same token, so the hub keeps publishing).
   - `ALLOWED_ORIGINS`: keep if it is set. If it is **not** set, the Bun relay was using its built-in default; set
     `ALLOWED_ORIGINS` to the origins it used to allow before deploying, or your site loses CORS.
     (`CORS_ORIGINS` works too and wins if both are set.)
   - `DATA_DIR`: keep, or leave unset (the image sets `/data`). Must be the volume's mount path.
   - `PORT`: leave to Railway. If the public domain's target port is pinned to 3000 (Settings, Networking), either
     set `PORT=3000` or change the target port to Railway's injected one.
   - `RAILWAY_RUN_UID=0`: Railway mounts volumes root-owned and the image runs as uid 10001, which could not write
     `/data` otherwise. (The Bun image ran as root.)
4. **Volume.** Keep it mounted at `/data`. Nothing to migrate: the relay loads `/data/stats.json` on boot.
5. **Source and build.** Point the service at this repo (Settings, Source) with builder
   Dockerfile, Dockerfile path `Dockerfile`, root directory `/`, custom start command empty (the image's `CMD` runs
   `relay serve`). Remove any root directory that pointed at `legacy/relay`. `railway.toml.example` has the same
   settings as config-as-code.
6. **Healthcheck** path `/healthz` (the Bun relay had none; `/` still answers `ok`).
7. **Deploy** and watch the logs for `relay: listening on 0.0.0.0:<port>, data /data, cors [...]`: the origins
   must be listed, and there must be no `no CORS origins` or `no publish token` warning.
8. **Verify**, before touching anything else:
   - `curl -s $R/healthz` gives `ok`.
   - `curl -s $R/stats | head -c 200` returns the previous snapshot (loaded from the volume), maybe with
     `"stale":true` until the hub's next publish.
   - After one hub publish, `curl -si $R/stats -H 'Origin: https://example.com'` shows fresh data without `stale`,
     `access-control-allow-origin: https://example.com`, `vary: Origin`.
   - `curl -s "$R/presence?id=cutovercheck1"` gives `{"online":N}` with N at least 1.
   - `curl -N -si $R/stream -H 'Origin: https://example.com'` prints `content-type: text/event-stream`, the
     allow-origin header, `retry: 3000`, an `event: stats` line with the document, then an `event: patch` within a
     few seconds of each hub publish and `: ping` every 15 s. If events only show up when the stream closes, something
     in front is buffering.
   - `curl -s $R/.well-known/agent-stats` shows `"role":"relay"`.
   - The site loads its stats.
9. **Rollback:** Deployments, open the previous (Bun) deployment, Redeploy; it uses its own image. The volume file is
   compatible both ways, the variables are the ones it reads, and the site falls back to polling `/stats` when
   `/stream` 404s. Revert the source setting afterwards so the next push does not rebuild the wrong thing.

## b. A relay from scratch (Railway, Fly, any Docker host)

Build and run anywhere with a persistent directory:

```sh
docker build -t agent-stats .
docker run -d --restart=always -p 8080:8080 -v relay-data:/data \
  -e PUBLISH_TOKEN="$(openssl rand -hex 32)" -e CORS_ORIGINS=https://your.site agent-stats
```

(When the volume is root-owned, run with `--user 0` or chown it to uid 10001.) Put HTTPS in front (the platform's
edge, Caddy, a tunnel); the relay speaks plain HTTP.

- **Railway:** new project, deploy from the repo with the Dockerfile builder, add a volume at `/data`, variables
  `PUBLISH_TOKEN`, `CORS_ORIGINS`, `RAILWAY_RUN_UID=0`, generate a domain. Healthcheck `/healthz`.
- **Fly:** `fly launch --no-deploy`, `fly volumes create data --size 1`, mount it at `/data`
  (`[mounts] source="data" destination="/data"`), `internal_port = 8080`, `fly secrets set PUBLISH_TOKEN=...`,
  `fly deploy`. Fly volumes are root-owned: add `USER root` or run a chown at start.
- Then on the hub, add the publish target (`hub.toml`):

```toml
[[publish]]
name = "relay"
url = "https://your-relay.example.com/publish"
token_file = "publish_token"      # same value as PUBLISH_TOKEN
```

## c. A cloud hub with the same image

Override the command: `agent-stats hub serve`. Give it a volume at `/data` (the image sets
`XDG_STATE_HOME=/data/state` and `AGENT_STATS_CONFIG_DIR=/data/config`, so `hub.db`, `hub-id`, status files, and
`hub.toml` with its `secrets/` all live on the volume), set `listen = "0.0.0.0:$PORT"` and `public = true` in
`hub.toml` so the hub serves `GET /stats` itself, `cors_origins` for the website, and terminate HTTPS at the platform.
Reporters join with a pairing code over HTTPS (`agent-stats join https://stats.example.com --code ...`).

Pick the cloud hub when no machine of yours is always on, when several people or machines report from different
networks and a Tailscale/LAN address is awkward, or when you want one fewer moving part (no relay). Pick a home hub
plus relay when you already have an always-on machine, want raw history and private data (repo names, paths) to stay
on hardware you control, or want the public endpoint to hold nothing but the public payload. The relay exists
precisely so the hub needs no public address.

## d. The website side

Poll `GET <relay>/stats` (every 15 to 30 s is plenty; the response is cacheable for 5 s). It is plain CORS, no
credentials. Handle three cases:

- `503` with body `no data yet`: nothing has ever been published; show an empty or loading state.
- `200` with `"stale": true`: the hub has not published for longer than `--stale-after` (default 2 minutes). Keep
  rendering the numbers but mark them as not live (grey out the live indicator, show "last updated" from `updated`).
- `200` without `stale`: live.

```js
const res = await fetch(`${RELAY}/stats`);
if (res.status === 503) return showEmpty();
const stats = await res.json();
render(stats, { live: !stats.stale });
```

For local development, `http://localhost:<port>` origins are always allowed; production origins must be listed in
`CORS_ORIGINS`.

## Automatic relay deploys

Pushing a `v*` tag runs `.github/workflows/release.yml`; its last job, `relay`, runs after the GitHub release is
published, checks out the tagged commit, copies `railway.toml.example` to `railway.toml`, and runs
`railway up --ci --service <name>` (Railway CLI pinned in the workflow). It then polls `https://$RELAY_URL/healthz` for
up to 5 minutes and requires `GET /.well-known/agent-stats` to report the tag's version.

Set these in the GitHub repo (Settings, Secrets and variables, Actions):

| Kind | Name | Value |
|---|---|---|
| Secret | `RAILWAY_TOKEN` | a Railway project token (below). Unset: the job prints a notice and succeeds without deploying |
| Variable | `RAILWAY_RELAY_SERVICE` | the Railway service name; default `relay` |
| Variable | `RELAY_URL` | the relay's public host, e.g. `relay-production-1234.up.railway.app`; unset: the post-deploy check is skipped |

Make a project token in the Railway dashboard: open the project, Settings, Tokens, pick the environment (the token is
scoped to that one environment, which is why the workflow passes no `--environment`), create it, and paste it into the
`RAILWAY_TOKEN` secret. It must be a project token: an account or workspace token is read from `RAILWAY_API_TOKEN`
instead and is not what this workflow uses. Service variables (`PUBLISH_TOKEN` and the rest) stay in Railway; the
workflow never touches them.
