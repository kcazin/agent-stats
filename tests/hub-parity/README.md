# Hub parity harness

Runs the legacy JS hub (`legacy/hub/hub.mjs`, unmodified) and the Rust hub side by side on identical input with a frozen,
controllable clock, and byte-compares what they serve and publish. Node >= 22.5, no npm deps.

```
node tests/hub-parity/parity.mjs --rust target/release/agent-stats [--keep] [--lenient-agents]
node tests/hub-parity/parity.mjs --js-vs-js        # harness self-test: two JS hubs against each other
```

Exit code 0 only on PASS. `--keep` keeps the scratch dir (`/var/tmp/agents/agent-stats/scratch-hub/parity-<pid>/`, with each
hub's `hub.log`, its sqlite db, and the last published body per step); without it the dir is deleted on success.

Safety: every child gets `HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, `AGENT_STATS_CONFIG_DIR` under the scratch dir; hubs bind
127.0.0.1 on random ports; `fakebin/gh` is first on `PATH`; the relay is a local capture server.

## What it does

1. Starts a capture server (`/publish/a`, `/publish/b`), hub A (JS, via `js-clock.mjs`) and hub B (Rust `hub serve`, with a
   `hub.toml` of the three projects in `AGENT_STATS_CONFIG_DIR`). Both are configured through `HOST PORT HUB_TOKEN PUBLISH_URL
   PUBLISH_TOKEN GITHUB_USER`.
2. `POST /__test/github` on both, so the github table is identical.
3. Replays ~80 steps over ~4 simulated days from 2026-10-10T08:00Z plus jumps to +6.9 and +7.5 days. Each step: set clock,
   POST the step's reports (same bytes, `Bearer parity-token`, both must answer 200), compare `/api/public`, tick, compare
   `/api/public`, compare a subset of `/api/state` (working subagents open byAgent week allTime firstDay daily today history),
   and compare the body each hub published to the relay (which also carries `projects.last/lastMs` and `agents[].flow`;
   `Authorization` must be `Bearer pub-token`).
4. Comparison is `JSON.stringify` equality (key order included). On mismatch: first 20 differing paths with both values, and
   whether the two are deep-equal ignoring key order.

Scenario: machines `alpha` (golden `expected.json` report, 28 sessions, lastActivity cycling across the working boundary,
growing io), `beta` (protocol 2, remotes, sub/parent delegate, all states, null state/stateAt, missing model), `gamma`
(protocol 2, one session sharing an id with beta's, zero rows, a row without `output`). Models cover
Claude/GPT/Grok families, `<synthetic>`, `unknown`, missing (top-5 + "Other" folding). Repos include `NEON-Tools`, `chum-tools`,
`my-content-hall` (must not match `contenthall`), session repo null with project fallback. Smaller re-sends (max upsert),
`full:false` partial reports, a machine silent > 45 s (offline), one at exactly the 45 s boundary, a machine silent > 7 days
(drops out of `machinesTotal`), three Tokyo midnights (15:00Z) with ticks at -900 s .. +3600 s, pile reset each day.
Also: 401 on a bad token, 404 on an unknown path (bodies compared).

Rust only (after step 3): reports hub.mjs would coerce or choke on (fractional, negative, numeric-string and 1e16 token
counts, a bad day, an empty machine, a session without `lastActivity`, `repos` not an array, non-JSON) must get 422, and
`protocol: 3` must get 426 `{supported:[1,2]}`; the Rust hub's `/api/public` must be unchanged afterwards. They are not sent to
the JS hub, which would store them.

Takeover and rollback (after the scenario): the JS hub is stopped and a Rust hub started on the JS hub's state dir (its
`hub.db` in place) must serve the same `/api/public` at the same instant; after one more report, a JS hub started on that
database (now with the Rust hub's extra tables) must serve the same `/api/public` as the Rust hub did.

## Control contract

JS hub (`js-clock.mjs`, `node --import`; control server on `$PARITY_CONTROL_PORT`; start time from `$PARITY_NOW` ms):
`Date` becomes a subclass whose `new Date()` / `Date.now()` return the controlled time; `setInterval(fn, 15000)` and
`setInterval(fn, 3600000)` are captured, not scheduled; `child_process.execFile` is wrapped so `/__test/github` can wait for the
hub's callback.

Rust hub (env `AGENT_STATS_TEST_HOOKS=1 AGENT_STATS_TICK_MS=0 AGENT_STATS_NOW=<ms>`): the same routes on its main port.

| route | body | effect |
| --- | --- | --- |
| `POST /__test/clock` | `{"now": ms}` | set the clock |
| `POST /__test/tick` | none | run one tick (incl. publish) and answer `{"ok":true}` when done |
| `POST /__test/github` | none | run the GitHub fetch, answer `{"ok":true}` when the table is filled |

## Normalizations

- Rust `/api/public` and published body: `projectList` is removed after checking it equals the three configured
  `{key,name,color}` in order (JS has no such field).
- `--lenient-agents` (off by default): `live.agents` is re-sorted by (agent, kind, id) on both sides. hub.mjs sorts agents with
  a comparator that returns -1 for any two `main` (or two `subagent`) entries of the same agent, so their relative order
  depends on V8's sort algorithm. A port should reproduce it exactly (the default); use the flag to see whether that is the
  only difference.
- Nothing else is normalized: with the clock frozen, `updated` and all time-derived fields must match exactly.
