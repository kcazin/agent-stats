# agent-stats architecture

One binary, `agent-stats`, three roles:

```
 every machine                one place you choose              optional, public
┌──────────────┐  push      ┌──────────────────────────┐  push  ┌──────────────┐   GET /stats   ┌─────────┐
│ report       │ ─────────▶ │ hub                      │ ─────▶ │ relay        │ ◀───────────── │ website │
│ (reporter)   │  15 s      │ SQLite, dashboard,       │  15 s  │ one snapshot │                └─────────┘
└──────────────┘            │ projects, public payload │        └──────────────┘
      … × N                 └──────────────────────────┘
```

Everything is a push except the website's read. A reporter only knows its hubs; a hub only knows its relays;
nothing ever connects *in* to a reporter, and a hub that publishes to a relay needs no public address.

Where the hub lives is the one real choice, and both are the same binary:

| Setup | Hub | Reporters reach it via | Public stats |
|---|---|---|---|
| **Solo** | same machine as the reporter, `127.0.0.1` | loopback | optional relay |
| **Home** | an always-on machine (the Mac mini) | Tailscale / LAN (`http://mac-mini:7777`) | relay (the hub has no public address) |
| **Cloud** | a container (Railway, Fly, a VPS) with a volume | HTTPS | the hub serves `/stats` itself, no relay |

Or no main machine at all: a **fleet**, where every Mac is a replica of the hub and they sync with each other
(Linux boxes stay reporters). See [FLEET.md](FLEET.md) for the model, the sync and the add-machine commands.

Private data (paths, cwd, repo names, git remotes, session ids) never leaves the hub (in a fleet: the replicas). The relay and `/stats` only
get the public payload: project buckets, opaque session ids, totals.

## Design decisions (and where they come from)

Borrowed from how T3 Code runs its server and connects environments (`~/.t3/source/*/docs/internals/`):

1. **Stable identities, created once.** Each hub has a random `hubId` (`state_dir()/hub-id`), each machine a
   `machineId`. Addresses can change; ids don't. Existing installs keep their hostname as the machine id so the hub's
   history (keyed by machine) continues; new installs also default to the hostname (readable on the dashboard), with
   `machine` in config.toml to override.
2. **Hubs are a catalog, not a URL.** A reporter can report to several hubs (home + a team hub, or old + new during a
   migration). Each hub entry has an ordered list of routes (`http://mac-mini:7777`, `http://192.168.1.20:7777`,
   `https://stats.example.com`); the reporter uses the first that answers `/.well-known/agent-stats` with the expected
   `hubId`, backs off 1 s → 5 min on failure, and re-probes for a better route every 5 min.
3. **Joining is a pairing code, not a shared secret.** On the hub: `agent-stats hub pair --label "MacBook"` prints a
   one-line command valid for 10 minutes. On the new machine: `agent-stats join http://mac-mini:7777 --code K7Q2-9XMD`.
   The hub swaps the code for a per-machine token (stored hashed on the hub, 0600 file on the machine) and returns
   its `hubId` and known routes. Tokens are scoped to reporting for that machine id only and revocable
   (`agent-stats hub revoke <machine>`). The old shared `hub_token` keeps working if the hub config allows it
   (`legacy_token_file`), so existing reporters keep reporting during the migration, but never for a machine that has
   its own token (enrolled or revoked): that gets 403.
4. **Every server answers a well-known descriptor.** `GET /.well-known/agent-stats` →
   `{"role":"hub","hubId":"…","name":"home","version":"0.5.0","protocol":2,"routes":["http://mac-mini:7777"],"timezone":"Europe/Paris"}`. Reporters use it to pick routes and
   check compatibility; `doctor` uses it to explain what's wrong.
5. **Explicit protocol numbers.** Reports carry `protocol` (absent = 1, the JS reporter). A hub that can't read a
   protocol answers 426 with the versions it speaks.
6. **Secrets are 0600 files in a 0700 directory** (`config_dir()/secrets/`), written atomically. Never in argv, never
   in the service unit.
7. **Versioned installs with trial-then-commit updates.** Binaries live in `data_dir()/versions/<version>/agent-stats`,
   `data_dir()/current` points at the active one, and the service runs `current/agent-stats`. `agent-stats update`
   downloads and verifies (SHA256SUMS) the release, runs `--version` on it, switches `current`, restarts the services,
   and waits for each role's status heartbeat; if it doesn't come within 90 s it switches back and restarts
   (`agent-stats rollback` does the same by hand). The last 3 versions are kept.
8. **A status file per role** (`state_dir()/<role>-status.json`, see `agent_stats_common::Status`): pid, version,
   heartbeat at least every 60 s, and role details (reporter: per-hub lastOkAt/lastError). `status`, `doctor` and the
   update health check read it.
9. **Logs are size-capped.** 10 MiB × 3 files in `state_dir()/logs/`, on top of journald/launchd.
10. **Git remote is the repo identity.** The reporter resolves a cwd's repository root (`.git` dir, or a worktree's
    `.git` file → `gitdir` → `commondir`), reads `remote.origin.url` from its config once, and normalizes it
    (`git@github.com:kcazin/neon.git` → `github.com/kcazin/neon`). Reports carry `repo` = the repository name
    (`neon`, same as before for most repos, so old hubs keep working) plus `remote` = the normalized remote, which
    new hubs use as the identity. Repos without a remote fall back to the folder name.
11. **New repos are noticed, not published.** The hub lists repos it hasn't mapped to a project ("unassigned") on its
    dashboard and at `/api/repos`, with tokens today and all time, and can POST a webhook the first time it sees one.
    Unassigned repos count as `other` in the public payload, so a new private repo never shows up by name.
12. **Projects are hub config.** `hub.toml` maps repos to public projects (`key`, `name`, `color`, glob patterns on
    repo name or remote). The public payload carries the project list, so a website doesn't hard-code it.
13. **Adding a machine is one command on a machine with the hub** (`crates/fleet`, `agent-stats machines`).
    `discover` merges `tailscale status --json` with Bonjour (`_agent-stats._tcp`; the hub advertises itself with
    `dns-sd -R` or `avahi-publish-service` when it listens beyond loopback, TXT `hubId`, `name`, `node`, `v`, `os`;
    `AGENT_STATS_NO_BONJOUR=1` turns that off) and asks each online candidate for its well-known descriptor; it only
    suggests. `add --ssh` does the install and join over the system `ssh` (batch mode, `accept-new` host keys, one
    POSIX script on stdin per round trip: probe, install.sh at this version, join), with the pairing code minted in
    hub.db here and carried only inside the script. `invite` prints the paste-able line. See docs/FLEET.md.

## Files

| Path | What |
|---|---|
| `config_dir()/config.toml` | reporter: machine, label, timezone, `[[hub]]` entries |
| `config_dir()/hub.toml` | hub: listen, name, timezone, projects, publish targets, public, CORS, legacy token |
| `config_dir()/secrets/` | `hub-<hubId>.token` (reporter side), `publish-<name>.token`, `legacy-hub.token`, `hub-admin.token` (hub dashboard) |
| `state_dir()/hub-id` | the hub's id, created once |
| `state_dir()/reporter-state.json` | reporter counts and per-hub unacknowledged keys |
| `state_dir()/hub.db` | hub SQLite (same schema as the JS hub, plus new tables); the directory is 0700, `hub.db`, `-wal`, `-shm` 0600 |
| `state_dir()/<role>-status.json` | status / heartbeat |
| `state_dir()/logs/` | rotated logs |
| `data_dir()/versions/<v>/agent-stats`, `data_dir()/current` | installed binaries |

`config_dir()` = `$AGENT_STATS_CONFIG_DIR` or `~/.config/agent-stats`; `state_dir()` = `$XDG_STATE_HOME/agent-stats`
or `~/.local/state/agent-stats`; `data_dir()` = `~/.local/share/agent-stats`. The legacy env vars (`HUB_URL`,
`HUB_TOKEN`, `STATS_MACHINE`, `STATS_LABEL`, `STATS_TZ`, `~/.config/agent-stats/hub_token`) still work: they define
one implicit hub, which is how existing installs and the golden tests run.

### config.toml (reporter)

```toml
machine = "cachyos-x8664"        # id the hubs key data by; default: hostname
label = "Linux workstation"      # shown on dashboards and the site
timezone = "Europe/Paris"        # day boundaries; must match the hub's (see Timezones)

[[hub]]
name = "home"
id = "6f1c…"                     # hubId, learned at join
routes = ["http://mac-mini:7777", "http://100.101.102.103:7777"]
token_file = "secrets/hub-6f1c….token"
view_token_file = "secrets/hub-6f1c….view.token"   # read-only, for a widget on this machine (see View tokens)
```

**Timezones.** Reporter and hub must cut days in the same zone. The hub's is `timezone` in hub.toml, else its
machine's zone (Asia/Tokyo without a hub.toml, as the JS hub hard-coded). It says which in `/.well-known/agent-stats`
and the `/pair` answer. A reporter's is `STATS_TZ`, else `timezone` in config.toml, else the zone its existing state
was counted in (so an unset timezone keeps meaning what it meant; Asia/Tokyo when only the JS reporter's state is
there), else the machine's. `join` writes the hub's zone into config.toml when the machine has none of its own, and
warns when they differ; `doctor` warns too.

**Managed mode.** When an app ships agent-stats (a binary under `X.app/Contents/`, or `AGENT_STATS_MANAGED_BY=X`), the
app installs, runs and updates it: `update` and `rollback` refuse and point at the app, `service install`/`uninstall`
refuse (`service status` works). `update --auto` installs nothing; on a hub machine (a `hub.toml` in the config dir) it
mirrors the running version for the hub's reporters, which still update themselves from the hub. The app's launchd
jobs can't name a log file under the home directory, so a managed `report`, `hub serve`, `relay serve` or
`update --auto` whose output isn't a terminal appends it to `state_dir()/logs/<role>.log` itself (`updater.log` for
`update --auto`), the files `service install` would use. `status` and `doctor` name the app.

Reporter details (crates/reporter):

- **Precedence.** Environment over config.toml over defaults: `STATS_MACHINE`, `STATS_LABEL`, `STATS_TZ` win over
  `machine`, `label`, `timezone`, so an existing service unit keeps meaning what it meant. Transcript roots follow the
  agents' own variables: `CLAUDE_CONFIG_DIR` (projects under it), `CODEX_HOME`, `GROK_HOME`.
- **The implicit hub.** `HUB_URL`, `HUB_TOKEN` or `config_dir()/hub_token` define a hub named `default` (no `id`, so
  no descriptor check: the JS hub has none), listed before the `[[hub]]` entries. It disappears once a `[[hub]]` entry
  has the same route (after `join` to that hub, its own token takes over).
- **Every hub receives everything.** Each configured hub gets the same full report: every repo name and normalized
  git remote, every live session (id, cwd-derived repo, model, state), and the whole token history, not a subset.
  There is no per-hub filtering, so only add a hub you would trust with all of that.
- **Per-hub acknowledgement.** `reporter-state.json` keeps `hubs: {<key>: {dirty, dirtyRepos}}`, keyed by the hub's
  id (`id:<hubId>`) when it has one and by name for id-less (legacy) hubs, so a name reused for another hub does not
  inherit the first one's acknowledgements. Entries written by older versions under a hub's name move to its id key.
  On the first run of this version the old single `dirty`/`dirtyRepos` go to the implicit legacy hub only (the one
  the old reporter was sending to; after migrating the JS reporter's state, that set is empty because the JS
  reporter's full syncs sent every day); any other hub, and any hub new to the state, starts with every key
  unacknowledged, so its first report carries the whole history; a removed hub is forgotten. Each report builds one
  body with the union of what the hubs due now still need (plus the last 400 days on a full sync, due per hub every
  10 min) and sends it to them in parallel; each hub's keys are cleared only by its own acknowledgement: a `200`
  whose body is JSON with `"ok": true`. Redirects are never followed (reports, `/pair`, the well-known probe): a 3xx,
  an HTML page or any other answer is an error and the keys stay unacknowledged. A hub in backoff is skipped, so a
  dead hub costs the others nothing.
- **join.** Replaces the entry of the same hub (same id, or an id-less entry sharing a route) and keeps its name.
  A different hub that has the requested name is kept: the new one gets the next free name (`hub-2`, …) and `join`
  says so; an explicit `--name` that is taken is refused.
- **Routes.** Hubs with an `id`: the first route whose descriptor has that `hubId` (4 s probe timeout). The route in
  use, route 0 included, is verified again at least every 5 min by probing the routes in order (an earlier one that
  answers takes over; one whose descriptor has another id is skipped, so a bare hostname that resolves to another
  machine on another network never gets the token or the report). A failed report (connection error or any other)
  drops the route, so it is verified again before the next send. No per-request probe. Reports time out after 10 s.
  Backoff 1 s doubling to 5 min. Hubs without an `id`: routes are used in turn without a probe.
- **Cadence.** Two loops in one: a fast tick every 1 s (`STATS_TICK_MS` overrides it) and a slow tick every 15 s. The
  fast tick stats only the transcripts modified in the last 30 min (reading what was appended) and does not list
  directories; the slow tick walks the directories (so a new transcript is found within ~15 s), reloads config.toml,
  and checks every file on its tier (idle under a day: every slow tick; under a week: every 2nd; older: every 4th;
  tiers count slow ticks). A hub gets a report as soon as it has something new: unacknowledged keys, or a change in the
  live sessions (set of ids, state, stateAt, model, tokens, io). Otherwise it gets one at least every 13 s (a heartbeat;
  hubs call a machine online for 45 s). Full sync every 10 min and the backoff are unchanged, and a hub in backoff is
  skipped. State saves (at most every 5 min unless urgent) and the status file (on change, at least every 60 s) do not
  follow the fast tick. An idle fast tick costs well under 1 ms.
- **config.toml is watched.** The reporter re-reads it when its mtime changes (checked on the slow tick), so `join` and
  `hubs remove` apply within ~15 s. Changing machine, label or timezone takes a restart.
- **Git identity.** The search for a repository root stops at `$HOME` (a dotfiles repo in `$HOME` doesn't claim every
  cwd below it). Local-path remotes (`/srv/git/r.git`, `file://…`) are not identities and fall back to the folder name.
  Repo buckets stay keyed by name, so two remotes with the same last segment (`github.com/a/x`, `github.com/b/x`) share
  a bucket and its rows carry the remote seen last. `remote.origin.url` is read with git's config rules (`;`/`#`
  outside double quotes start a comment), and query strings and fragments (`?token=…`, `#…`) are cut off. `sessions[].repo` uses the same name as the repo rows.
- **One writer.** `report` and `report --once` hold an advisory `flock` on `state_dir()/reporter.lock` while they
  run. A second `report` waits for it (logging once); `report --once` beside the service exits 2 with "the reporter
  service is running; use --dry or --print". `--dry`/`--print` never write state and take no lock. The temporary
  file of a state save is per process (`reporter-state.json.tmp<pid>`).
- **Status.** `state_dir()/report-status.json` is written by `report` only (not `--dry`/`--print`, which may run
  beside the service). detail: `{machine, label, hubs: [{name, id, route, lastOkAt, lastError, unacked}]}`.

### hub.toml

```toml
name = "home"
listen = "100.101.102.103:7777"  # or 127.0.0.1:7777 (solo), 0.0.0.0:$PORT (cloud)
timezone = "Europe/Paris"        # default: this machine's zone
public = false                   # true: serve GET /stats itself (cloud hub)
cors_origins = ["https://example.com"]   # /stats only; exact match, Vary: Origin
legacy_token_file = "hub_token"  # accept the old shared token (paths are relative to config_dir())
github_user = "me"               # optional: GitHub contributions in the payload (`gh api graphql`, hourly)
new_repo_webhook = ""            # optional: POSTed once per report that adds repos: {machine, firstSeen, count, more, repos: [{repo, remote,
                                 # tokensToday, project}] (first 50)}; a single repo also has those fields at top level
advertise = ["http://100.101.102.103:7777", "http://mac-mini:7777"]   # optional: routes given to reporters (/pair, well-known); default http://<listen>
trust_loopback = true            # 127.0.0.1/::1 clients count as admin (dashboard, /api/state, /api/repos)
trusted_networks = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]   # CIDRs treated like loopback for those read-only routes
                                 # (default none; TCP peer address only, never X-Forwarded-For; not /report or /pair)
max_connections_per_ip = 8       # open connections one peer IP may hold (0 = unlimited; raise behind a proxy that connects from one address)

[[publish]]                      # any number; each has its own token, timeout and last ok/error
name = "relay"
url = "https://relay.example.com/publish"
token_file = "publish_token"
timeout_secs = 10

[[project]]                      # first match wins; globs are case-insensitive, whole-string; no "/" = repo name, with "/" = normalized remote (github.com/me/*)
key = "neon"
name = "Neon Overrun"
color = "#5b8cff"
match = ["*space-rock*", "*neon*", "*poe-mobile*"]

[unattributed]                   # tokens with no repo (old history imported from T3 Code's cache); default other = 1
neon = 0.9
other = 0.1

[pile]                           # the site's token pile (hub.mjs constants)
per_hour = 1e9                   # trucks clear this many tokens an hour
seed_drain = 1e5                 # first run only: today's tokens less seed_drain per seed_every_ms since midnight
seed_every_ms = 15000
```

`examples/hub.example.toml` is a complete home hub. Without a hub.toml, the hub runs from the JS hub's environment:
`HOST`/`PORT` (listen), `HUB_TOKEN` or `config_dir()/hub_token` (legacy token), `PUBLISH_URL` + `PUBLISH_TOKEN` or
`config_dir()/publish_token` (one publish target), `GITHUB_USER` (no default: hub.mjs defaulted to `kcazin`). With a
hub.toml, `HUB_TOKEN`, `PUBLISH_URL`/`PUBLISH_TOKEN` (only if there is no `[[publish]]`) and `GITHUB_USER` still apply when
the file doesn't set them.

### The hub (`crates/hub`)

- **Same database.** `state_dir()/hub.db` keeps hub.mjs's six tables untouched, so the Rust hub can take over the JS hub's
  file in place and the JS hub can take it back (the parity harness checks both directions). New tables, all
  `CREATE TABLE IF NOT EXISTS`: `machine_tokens` (sha256 of each machine token, label, created, last_seen, revoked),
  `pair_codes` (sha256 of each code, expiry, used), `machine_meta` (reporter version, protocol, auth kind), `repo_remotes`
  (machine, repo → remote), `repo_meta` (first seen; repos already in `repos` are backfilled without notifying).
  `kv.schema_version` = 2.
- **Same payload.** The public payload is computed exactly as hub.mjs computes it (same queries, float arithmetic,
  `Math.round`, key order, V8's sort for the agents list, whose comparator is inconsistent) and printed like
  `JSON.stringify`; `tests/hub-parity` byte-compares the two. Additions: `projectList`, and `projects.*` keys come from
  hub.toml instead of being hard-coded.
- **Auth.** The hub refuses to start unless reporters can authenticate (a legacy token, an enrolled machine or a pending
  pairing code), or `--insecure-no-auth`. Tokens are compared in constant time. `/report` takes the legacy token (any
  machine) or a machine token (its own machine only, 403 otherwise). The dashboard, `/api/state` and `/api/repos` show
  paths, so they need a loopback client, a peer in `trusted_networks`, or the admin token (`config_dir()/secrets/hub-admin.token`, created by `hub serve`)
  as `Authorization: Bearer`, or as a cookie set by opening `/?token=<admin token>` once, or a machine's view token as
  `Authorization: Bearer`. `/api/public` is open when `public = true`, needs the same otherwise. Token-less access (loopback or `trusted_networks`) also needs a `Host` header that
  names this hub (anti DNS rebinding): `localhost`, `127.0.0.1`, `[::1]`, the listen host, an `advertise` host, the machine's
  hostname or `<hostname>.local`, `*.ts.net`, or any IP literal (an address can't be rebound). Any other `Host` needs the admin token. A valid token is unaffected, as are
  `/report`, `/pair`, the well-known route and `/stats`.
- **Pairing.** `hub pair` writes the hashed code straight into hub.db (SQLite's write lock is the lock), so it works whether
  or not the hub is running and the plain code never crosses a socket; it prints `agent-stats join <route> --code XXXX-XXXX`
  (Crockford base32, 40 bits, 10 min by default, one use). `POST /pair` attempts are limited to 5 per client and 200 overall
  per 10 minutes (429); an attempt is counted before the code is checked and given back when it works. A code never takes
  over a name that already has a token (enrolled or revoked): that is a 409 telling the admin to run
  `hub pair --replace <machine>`, whose code is bound to that machine and swaps its token (and un-revokes it). A machine
  that so far used the legacy token has no token, so its first pairing needs no flag. `hub revoke` sets `revoked` in the
  same table (history stays; `hub purge <machine> [--from DAY] [--to DAY]` deletes that machine's usage and repo rows for
  the days, e.g. after a stolen token); tokens are checked against the database on every report.
- **Validation.** Reports are validated before anything is written: `machine`, days (`YYYY-MM-DD`), repo names, session ids
  and token counts (integers, 0 ≤ n < 1e12 per row) must be well-formed or the report gets 422 with the reason. Days must
  be real dates from the hub's today + 1 back to 401 days ago (hub time zone); a later or impossible date is a 422, rows
  older than the window are skipped. Descriptive fields (label, model, cwd…) lose control, bidi and line-separator
  characters, are trimmed and length-capped (identifiers containing them are refused); `agent`/`kind`/`state` outside the
  known sets become null. Sums over tokens use SQLite `total()`, so no pile of rows can make a query fail. Per report at
  most 200 repositories the hub has never seen are accepted (rows of further new ones are dropped, and a repeat report
  lets the next 200 in), 2,000 sessions and 100,000 rows; the new-repo webhook has one worker thread fed by a bounded queue
  (256 events, dropped when full), and nothing is queued without a webhook. `hub machines` and `hub repos` show control
  characters escaped.
- **Limits.** Decided from the request head, before any body is read: bodies are capped per route (`/pair` 4 KiB, `/report` 8 MiB,
  anything else 64 KiB; 413), and `/report` needs a known bearer token first (401, body unread). At most 4 `/report` bodies are
  read and parsed at once; the rest wait up to 2 s, then get 503. The request head must arrive within 5 s (408), the body within
  30 s of the connection, every read bounded by what is left of those budgets. At most 64 connections, 8 per peer IP, of which 8
  are kept for loopback and `trusted_networks` peers; over the cap the accept thread answers 503 at once. If the accept loop
  ends the hub exits with status 1. Event streams (`/api/stream`, `/stream`) don't count against those: once the response
  head is written the socket leaves its connection thread (which ends, freeing its slot) for one broadcaster thread
  that writes every stream from a per-client buffer over non-blocking sockets (`poll(2)`), so a client that stops
  reading never blocks the others. Streams have their own cap: 32 in all, 8 of them kept for the admin, loopback and
  `trusted_networks` peers, and 4 per IP for anyone else (none when `max_connections_per_ip = 0`, a hub behind a
  proxy); over it, 503. A `: ping` goes to every stream every 15 s; a client more than 256 KiB behind, or one nothing
  could be written to for 5 s, is dropped (its EventSource reconnects).
- **Routes.** `GET /sync/changes`, `POST /sync/hello`, `POST /join-request`, `GET /join-request/<id>` and
  `GET /api/join-requests` for fleets (docs/FLEET.md). `POST /report`, `POST /pair`, `GET /.well-known/agent-stats`, `GET /` (dashboard), `GET /api/state`,
  `GET /api/repos`, `GET /api/stream` (same auth as `/api/state`), `GET /api/public`, and `GET /stats` and `GET /stream`
  when `public = true`. The dashboard follows `/api/stream` and polls `/api/state` every 15 s for the rest (every 5 s,
  with `/api/repos`, on a hub without the stream). Test-only `/__test/{clock,tick,github}` exist when
  `AGENT_STATS_TEST_HOOKS=1`, for loopback clients (see tests/hub-parity/README.md).

## Wire contracts

- `POST /report` (reporter → hub), `Authorization: Bearer <machine token or legacy token>`. Body: the JS reporter's
  shape (`machine, label, platform, at, full, days[], repos[], sessions[]`) plus, from protocol 2: `protocol`,
  `version`, `machineId`, and `repos[].remote` when known. A machine token only accepts its own `machine`.
  Answers `{"ok":true}`; 401 bad token, 403 machine mismatch, 413 too large, 422 invalid, 426 protocol.
- `POST /pair` (reporter → hub): `{code, machine, label, platform, version}` → `{hubId, name, token, viewToken, routes,
  timezone}` (`viewToken` and `timezone` from 0.5).
- **View tokens.** Each enrolled machine gets a read-only token at pairing, for a widget on that machine: it opens the
  dashboard, `/api/state`, `/api/repos`, `/api/stream` and `/api/public` as `Authorization: Bearer`, and nothing else
  (no `/report`, no `/release`). Stored hashed in `view_tokens`; revoking the machine revokes it. `POST /view-token`
  with the machine's own token issues a new one (replacing the old), for machines paired before 0.5:
  `agent-stats hubs view-token` does that and saves it beside the reporting token. Answers `{"viewToken"}`; 401 for
  anything but a live machine token.
- `GET /release` (reporter → hub), same auth as `POST /report` (machine token, legacy token) or the admin token:
  `{"version":"<v>","targets":["<target>", ...]}`, or `{"version":null,"targets":[]}` when there is nothing to
  offer. The hub only ever advertises its own running version, so a reporter never gets ahead of the hub's protocol.
  It reads the mirror the updater keeps at `<data dir>/releases/<version>/` (`SHA256SUMS`, one
  `agent-stats-<version>-<target>.tar.gz` per target); a version is offered only once the file `.complete` exists
  in its directory.
- `GET /release/<version>/<file>` (same auth): the bytes of `SHA256SUMS` (`text/plain`) or of
  `agent-stats-<version>-<target>.tar.gz` (`application/gzip`), `cache-control: no-store`. 404 unless `<version>` is
  the hub's own, its mirror is complete and `<file>` is exactly one of those two shapes that exists; no other path
  is ever built from the request. Files over 200 MiB answer 500.
- `POST /publish` (hub → relay), `Authorization: Bearer <publish token>`: the public payload. The relay keeps the
  newest by `updated` and ignores older ones. Sent on every tick (15 s) and, publish on change, within a second of a
  report the hub accepts: debounced, at most one a second per target and never two in flight to the same target (one
  worker per target; payloads waiting behind a slow one collapse into the newest). A payload between ticks is computed
  from the current data but takes no sample, doesn't move the pile, and repeats the last tick's `projects.last`,
  `projects.lastMs` and `agents[].flow`. `AGENT_STATS_TICK_MS=0` turns publishing on change off. Each publish also
  refreshes what the hub's own `/stats` and `/stream` serve.
- `GET /stats` (relay, or a public hub): the latest public payload, plus `stale: true` once it's older than 2 minutes.
- `GET /api/stream` (hub, admin auth as `/api/state`): Server-Sent Events, chunked. First `retry: 3000`, then
  `event: live` (`{"now", "sessions"}`, exactly those fields of `/api/state`) and `event: repos` (the `/api/repos` body).
  `live` again within a second of an accepted report and at least every 15 s; `repos` whenever it changes.
- `GET /presence?id=<tab id>` (relay): `{"online":N}`, tabs seen in the last 45 s; in memory only. No `id`: count only.
- `GET /stream` (relay, or a public hub): Server-Sent Events of the `/stats` document, same CORS allowlist,
  `content-type: text/event-stream`, `cache-control: no-store`, `x-accel-buffering: no`, chunked (no content-length).
  First `retry: 3000`, then `event: stats` (the bytes the relay's `/stats` would return, `stale` included; a public
  hub's stream carries `"stale": true` only while stale) when there is a document. On each accepted publish and each `stale` flip (timer, checked every second): `event: patch`, an RFC 7386
  merge patch from the last document this stream was sent (objects recursive, arrays and scalars whole, removed key →
  `null`, so un-staling is `"stale":null`); a change a merge patch cannot express (a member that becomes `null`) is
  sent as a full `stats` instead. `: ping` every 15 s. Streams belong to one broadcaster thread after the response
  head (no request slot), at most 1000 (503 over it); a stream with more than 256 KiB unsent when a new event comes,
  or no write progress for 5 s, is dropped and reconnects to a fresh `stats`. Clients fall back to polling `/stats`
  when `/stream` 404s.
- `GET /.well-known/agent-stats` (hub, relay): descriptor, no auth. The hub's adds `routes` (its advertised URLs);
  a fleet replica's also `node` (its name) and `fleetRoutes` (every replica's routes, always-on first), which
  reporters append to their hub entry.

The public payload keeps the exact shape the kilcazn.com site reads today (`updated, live, today, history, activity,
since, pile, projects, calendar`), and adds `projectList: [{key, name, color}]`.
