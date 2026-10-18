# Fleets: no main machine

A fleet is the 2–5 machines one person uses. Every Mac (anything running Tokiscope, or any machine you choose)
is a **replica**: it runs the hub, keeps every machine's history and can show the whole dashboard on its own.
Other machines (headless Linux boxes) are **leaves**: they only run the reporter and send to whichever replica
answers. Nothing depends on one machine being awake: a replica that was asleep or offline catches up from the
others, and a leaf keeps what it counted until a replica takes it.

```
 leaf (reporter only) ──report──▶ any replica ◀──sync──▶ every other replica ──(one of them)──▶ relay
```

## Identity

- **Fleet id** = the hub id the first hub created (`state_dir()/hub-id`). Every replica of the fleet answers its
  well-known descriptor with that `hubId`, so a reporter's `[[hub]]` entry (verified by id) works with any of them.
  Existing installs keep their hub id: the old hub becomes the first replica and nobody re-pairs.
- **Node** = a replica's name, the same as its machine name (`[fleet] node` in hub.toml, default: the hostname
  without `.local`, as the reporter's `machine`).
- Each replica has its own **replica token** (`config_dir()/secrets/replica.token`, created on first start). Its
  sha256 travels in the replica's `fleet_members` row, so replicas authenticate to each other without the
  reporter's token.

## hub.toml in a fleet

```toml
listen = "0.0.0.0:7777"
advertise = ["http://macbook-air.tail1234.ts.net:7777", "http://macbook-air.local:7777"]

[fleet]
node = "macbook-air"          # default: hostname without .local
always_on = false              # preferred for publishing when true (desktops, servers); laptops false
peers = ["http://mac-mini.tail1234.ts.net:7777"]   # where to start syncing; fleet_members adds the rest
```

Fleet-wide settings (name, timezone, projects, unattributed, pile, publish targets, github_user, public, cors,
new_repo_webhook) live in `config_dir()/fleet.toml`, same keys as hub.toml. It is synced: edit it on any replica
and the others get it (newest edit wins) and restart their hub to apply it. Publish tokens travel with it and are
written to `secrets/publish-<name>.token`. A hub.toml without `[fleet]` is a plain hub, as before.

`agent-stats fleet replica` makes a joined machine a replica (writes `[fleet]`, `fleet.toml`, the fleet id, and
puts `http://127.0.0.1:<port>` first in the reporter's routes, so the reporter reports to its own replica). On the
old hub it moves the fleet-wide keys from hub.toml to fleet.toml (keeping `hub.toml.bak`).

## Sync

Every row a hub stores is owned by one machine and only grows (token counts are per-day totals: `max` merges
them), so replicas merge by union with fixed rules and never conflict:

| table | key | merge |
|---|---|---|
| usage | machine, day, agent, model | tokens, output = max |
| repos | machine, day, repo | tokens = max |
| machines | machine | newest `last_seen` wins (label, platform, sessions) |
| machine_meta | machine | newest `updated` wins |
| repo_remotes | machine, repo | newest `updated` wins |
| repo_meta | repo | earliest `first_seen` wins |
| samples | at | insert if absent (charts take the max per bucket) |
| github | day | count = max |
| machine_tokens, view_tokens | machine | newer `created` replaces; same token: revoked/last_seen = max |
| fleet_members | node | newest `updated` wins |
| join_requests | id | approved/denied beat pending |
| purges | machine, from, to | insert; deletes the matching usage/repos rows where applied |
| kv `pile`, `fleet_config` | key | newest `at` / `stamp` wins |

Triggers write each changed key into `sync_log(seq, tbl, k)` (one entry per key, its seq bumped on change; an
update that changes nothing logs nothing, so merges never ping-pong). Existing rows are logged once when the
schema is upgraded.

- `GET /sync/changes?since=SEQ&limit=N` (replica or machine token) →
  `{"node","db","seq","more","rows":[{"t":"usage","v":[...]}]}`. `db` is a random id of that database: when it
  changes, the puller starts again from 0.
- `POST /sync/hello` (replica or machine token): `{"node","routes","version","alwaysOn","tokenHash"}` → upserts
  the sender's `fleet_members` row. Sent at start and every 5 min to every peer.
- Each replica pulls from every other non-revoked member with routes, every 2 s while it answers (backoff 2 s → 1
  min when it doesn't), and stores a cursor per peer in `sync_cursors`. Whatever it pulls it serves onward.
- After rows arrive, the dashboard streams update as after a report.
- `/sync/*` takes a replica token or any machine token of the fleet (a machine introduces its replica with its
  reporting token). So a leaf *could* pull everything if someone made it ask: leaves are a default that keeps other
  machines' repo names off boxes nobody looks at, not a wall between machines you own.
- A replica on an older agent-stats (no `/sync`) is skipped with "update it"; the newer one keeps saying hello until
  it answers, then the older one, once updated, pulls everything it missed.

## Publishing

Only one replica publishes to the relay: among members heard from in the last 10 minutes (and itself), the first
by `always_on` (true first), then node name. A replica that can't reach the others publishes alone; the relay
keeps the newest payload, so an overlap is harmless. The pile is synced, so the next publisher continues it.

## Joining

Three ways, all ending with a machine token in `machine_tokens` (synced to every replica):

1. **Over SSH** (`agent-stats machines add --ssh user@host`): run on a replica. It probes the machine, mints a
   pairing code locally, finds a route back to this replica from the target, installs the release there (same
   version as here) with `install.sh --join <route> --code <code>`, and waits for the first report. SSH access is
   the proof of trust; no code is shown.
2. **A line to paste** (`agent-stats machines invite`): `curl … install.sh | sh -s -- --join <route> --code <code>`.
3. **Join request** (a Mac with Tokiscope, no SSH): the new machine runs `agent-stats fleet request <url>`, which
   generates its own token and view token and sends their hashes to `POST /join-request`. Both sides show 4 digits
   (from the request id and the token hash); `agent-stats fleet approve <id>` on any replica enrolls it. The
   requester polls `GET /join-request/<id>?key=<sha256 of token>` until approved. Requests expire after 10 min.

Discovery (`agent-stats machines discover`: `tailscale status --json`, Bonjour `_agent-stats._tcp`) only suggests
machines; it never enrolls one. A Mac that runs Tokiscope joins from its own first launch (way 3), so `add --ssh`
refuses one ("open it there and choose Join").

## CLI contracts (JSON, for Tokiscope)

All `--json` output is UTF-8 JSON; streams are one JSON object per line.

`agent-stats machines discover [--timeout 3s] --json` → one array:
```json
[{"name":"cachyos","host":"cachyos.tail1234.ts.net","addresses":["100.64.0.5"],"os":"linux","sources":["tailscale"],
  "online":true,"self":false,"member":false,
  "agentStats":{"url":"http://cachyos.tail1234.ts.net:7777","role":"hub","fleetId":"…","name":"home","node":"cachyos","version":"0.6.0"}}]
```
`os`: `linux`, `macos`, `windows` or null. `agentStats` is null when nothing answers the well-known route on
port 7777. `member`: the machine's name is a machine of this fleet.

`agent-stats machines add --ssh <target> [--label L] [--replace] [--json]` → lines:
```json
{"step":"connect","state":"running","detail":"ssh kcazin@cachyos"}
{"step":"connect","state":"ok","detail":"Linux x86_64 · systemd · no agent-stats yet"}
{"step":"route","state":"ok","detail":"http://mac-mini.tail1234.ts.net:7777 answered from cachyos"}
{"step":"install","state":"ok","detail":"agent-stats 0.6.0, SHA256 verified"}
{"step":"join","state":"ok","detail":"systemd user service, runs without login"}
{"step":"report","state":"ok","detail":"first report received"}
{"step":"done","state":"ok","machine":"cachyos","label":"cachyos"}
```
Steps in order: `connect`, `route`, `install`, `join`, `report`, `done`. A failure is
`{"step":…,"state":"error","error":KIND,"message":…,"hint":…}` then exit 1. KIND is one of `needs-key`,
`host-key-changed`, `unreachable`, `unsupported`, `no-route-back`, `taken` (the name is enrolled: retry with
`--replace`), `other`.

`agent-stats machines invite [--label L] [--json]` → `{"line","url","code","expires"}`.

`agent-stats machines list --json` → `[{"machine","label","role":"replica"|"leaf","online","lastReport","version",
"platform","revoked","routes"}]`.

`agent-stats machines remove <machine> [--ssh <target>] [--json]` → revokes it (its token, and its member row if
it is a replica; both sync); with `--ssh` also uninstalls there. Lines: `revoke`, `uninstall` (with `--ssh`), then
`{"step":"done","state":"ok","machine"}`.

`agent-stats fleet status --json` → `{"fleetId","name","node","replica":bool,"publisher":node,"members":[{"node",
"routes","alwaysOn","version","revoked","self","lastSync","lastError","reachable"}]}`. Run on a machine with a hub.

`agent-stats fleet replica [--port P] [--json]` → makes this machine a replica (see above); `{"fleetId","node",
"listen","startHub","alwaysOn"}` (`restartHub` too when it turned an existing hub into a replica, `already` when it
was one). `startHub`/`restartHub` are true in managed mode, where it only writes configuration and the app (re)starts
the hub; otherwise it installs or restarts the hub service itself.

`agent-stats fleet create [--name N] [--port P] [--json]` → a new fleet with this machine as its first replica
(hub.toml, fleet.toml, and the reporter enrolled straight in the local database and joined over loopback; no running
hub needed). Same answer as `replica`. On a machine that already has a hub, it does what `replica` does.

`agent-stats fleet request <url> [--label L] [--leaf] [--json]` → lines `{"state":"pending","id","digits","expires",
"fleetId","name"}`, then `{"state":"approved","fleetId","name","replica","startHub"}` (joined, and made a replica
unless `--leaf`) or `{"state":"denied"}` / `{"state":"expired"}` (exit 1).

`agent-stats fleet requests --json` → `[{"id","machine","label","platform","digits","at"}]` (pending).
`agent-stats fleet approve <id>` / `agent-stats fleet deny <id>`.

`/api/stream` also sends `event: requests` with the pending join requests when they change.
