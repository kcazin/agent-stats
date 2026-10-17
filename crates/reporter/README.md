# agent-stats reporter (Rust)

A port of `legacy/reporter/reporter.mjs`. It reads Claude Code, Codex and Grok transcripts incrementally and every
15 s POSTs daily token totals and live sessions to its hubs. The report body has the JS reporter's shape and meaning
(`machine, label, platform, at, full, days[], repos[], sessions[]`), with the same counting rules per agent, the
same session rules, and the same T3 Code usage-cache import on first run, plus the protocol 2 fields (`protocol`,
`version`, `machineId`, `repos[].remote`). It is a library behind `agent-stats report|join|hubs|status|doctor`; the
`agent-stats-reporter` binary is the same as `agent-stats report`.

```sh
agent-stats report                   # run forever (what the service runs)
agent-stats report --once            # one scan and report; exits 1 if any hub didn't get it
agent-stats report --once --dry      # one scan, print the JS dry-mode summary, don't send
agent-stats report --once --print    # one scan, print the exact body that would be POSTed, don't send
agent-stats join http://mac-mini:7777 --code K7Q2-9XMD [--name home]   # pair with a hub
agent-stats hubs [list | remove <name>]
agent-stats status                   # the running reporter, per hub: route, last ok, unacknowledged keys, error
agent-stats doctor                   # config, transcript roots, each hub's routes and token, the heartbeat
```

Hubs come from `config_dir()/config.toml` (`[[hub]]` entries, written by `join`) and from the legacy variables below,
which define one hub named `default`. See docs/ARCHITECTURE.md ("config.toml (reporter)" and "Reporter details") for
precedence, routes, per-hub acknowledgement and the git repo identity.

| Variable | Default |
| --- | --- |
| `AGENT_STATS_CONFIG_DIR` | `$XDG_CONFIG_HOME/agent-stats` or `~/.config/agent-stats`: config.toml, secrets/, hub_token |
| `HUB_URL` | `http://mac-mini:7777` when only a token is set (https works too) |
| `HUB_TOKEN` | contents of `config_dir()/hub_token`. Without any hub with a token, `report` exits with an error, unless `--dry`/`--print` is set |
| `STATS_MACHINE` | `machine` in config.toml, else the hostname without `.local` |
| `STATS_LABEL` | `label` in config.toml, else the machine name |
| `STATS_TZ` | `timezone` in config.toml, else the zone existing counts are in, else this machine's: the IANA zone that sets day boundaries (`join` sets the hub's when there is none). The tz database is built in |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME` | `~/.claude` (transcripts in `projects/`), `~/.codex`, `~/.grok` |
| `XDG_STATE_HOME` | `~/.local/state`; state goes in `agent-stats/reporter-state.json`, status in `agent-stats/report-status.json` |
| `STATS_VERBOSE` | set to `1` to log what each tick did and how long it took |

The home directory is `$HOME`. Logs go to stderr, so stdout carries only the `--dry`/`--print` output.

## Why the rewrite: what changed

**State is saved only when it changed, and not often.** The JS reporter rewrites a 3 MB `state.json` every 15 s
(about 18 GB a day). This one saves only when something changed. While transcripts keep changing it saves at most
once every 5 minutes. It saves at once after expensive work (the first full scan, the T3 import, a rewritten
transcript) and on SIGTERM/SIGINT. Losing up to 5 minutes of state in a crash is harmless. The totals, read offsets
and unsent keys are saved together, so after a restart the same lines are read again and come to the same totals.
Writes are atomic: a temporary file, fsync, then rename. If a save fails (ENOSPC, read-only disk), the error is
logged once and the state is kept in memory and saved again later. Message ids are stored as 64-bit hashes.

**No line-length limit.** The JS reporter reads 32 MB at a time and stops reading a file for good once a single
line is longer than that. This one streams lines through a fixed 1 MB buffer. Each line is classified by substring
before any JSON is parsed:

- Lines that can count tokens are parsed at once: `"usage"` for Claude; `token_count`, `turn_context` and
  `session_meta` for Codex; `turn_completed` for Grok.
- Lines that can only change what a session shows (its state, and for Claude its cwd) are remembered by offset,
  not parsed. Only the last one that sets the state matters. So at the end of a read the remembered lines are
  parsed newest first until one sets the state, which is usually the first one tried. A counted line that sets the
  state makes the earlier ones moot, and if the list passes 1024 lines it is settled early the same way. Tool
  output and file reads are therefore never parsed.
- A line longer than the buffer is scanned for the same substrings in a streaming pass. If it has to be parsed, it
  is parsed straight from the file through a reader, so memory stays bounded.

**A rewritten or truncated transcript is not counted twice.** The JS reporter re-reads a shorter file from offset 0
and relies on its 64-id window and Codex running total to skip what it already counted. Both forget, so the file's
history is counted again. This reporter records what each transcript has added to the totals, per `day|agent|model`
and per `day|repo`. A file counts as rewritten when it got shorter, its inode changed, the byte before the read
offset is not a newline, or its first 4 KiB changed. A rewritten file is read again from the start into a fresh
parse state. Each key then counts the larger of what the file held before and what it holds now:

- Content that was already counted adds nothing.
- Content that was cut stays counted, because those tokens were spent. Subtracting it would also not reach the hub,
  which keeps the largest total it has seen for each day.
- New content adds the difference.

There is one edge case: if a rewrite drops old content and adds new content under the same day and model before
the reporter sees the file, the new part is undercounted by the dropped amount.

**Migrated files.** Transcripts migrated from the JS state have no contribution record (`baseline: true`). If one
is rewritten later, it is read again with a fresh parse state, nothing is added to the totals, and its record is
rebuilt from what it now holds. The JS reporter already counted that content. The only possible loss is content
appended and then cut between two scans of the same file. From then on the file is tracked like any other.

**Cheap ticks; one parallel full scan.** With no state, every transcript is read in parallel (rayon). After that a
tick:

- re-lists a directory only when its mtime changed, and reuses the cached listing otherwise. A listing taken within
  2 s of the directory's mtime is not trusted, because some file systems have coarse timestamps.
- stats transcripts modified in the last day on every tick, files idle for 1–7 days every other tick (30 s), and
  older ones every 4th tick (1 min). An active session's appends are picked up on the next tick. A long-idle
  session that is resumed (Codex appends to its old rollout file) shows up within a minute.
- reads only the bytes appended since the last read.

**Bounded full syncs.** The hub ignores `full` and keeps the larger value for each day. So a full sync (every 10
minutes) sends the last 400 days plus any keys not yet acknowledged, instead of every day ever recorded. `full: true`
is still set and otherwise means the same thing.

**Failures.**

- No hub with a token is a startup error; a hub without one is skipped with a warning.
- A failed report (no answer within 10 s, or a non-2xx status) is logged once per distinct error and hub, with a
  "reachable again" line when it recovers. That hub's changed keys stay pending and go out with its next successful
  report; it backs off (1 s doubling to 5 min) while the other hubs carry on.
- The pending keys are saved in the state, per hub. If a hub is unreachable for days, nothing is lost.
- A transcript that fails to read is logged once and retried on every tick.

**Memory.** Steady state is about 15 MB RSS. The first full scan peaks at about 65 MB with 16 threads (28 MB with
`RAYON_NUM_THREADS=1`). The T3 usage caches are stream-parsed: only the records of deleted, untracked transcripts
are kept, at about 40 bytes each.

## Switching over from the JS reporter

On the first run without `reporter-state.json`, if `agent-stats/state.json` from the JS reporter exists with
`"v": 3`, it is migrated instead of rescanning. This matters because Claude Code deletes transcripts after 30 days,
so the JS state holds history that can no longer be rebuilt. The migration carries over:

- `days` and `repos`
- every file's offset, mtime, id window, Codex total, tokens, model, cwd, state, stateAt, io, parent and sub
- the `t3` marker, so the usage-cache import is not repeated

The migrated files continue from their offsets. The JS `state.json` is left untouched, so rolling back to the JS
reporter works: it resumes from its own last state. A `reporter-state.json` that cannot be read is moved aside
(`.unusable-<ms>`), and the reporter migrates or rebuilds.

## Deliberate differences from reporter.mjs

- Rewritten transcripts are not double counted (see above).
- A Codex session moved from `sessions/` to `archived_sessions/` keeps its state (it is matched by basename, the same
  identity the JS dedupe uses). The JS reporter treats the new path as a new file and counts it again.
- A line with an unparseable timestamp is skipped. In JS, `Intl.DateTimeFormat.format` throws on it, which aborts
  the read so the file is retried, and partly re-counted, on every tick.
- A transcript line that is not valid UTF-8 is decoded lossily, as Node does. A Grok directory name that is not
  valid percent-encoding is used as is; in JS, `decodeURIComponent` throws and fails the whole scan.
- A T3 usage record that is not an array is skipped. In JS it throws and the import is retried on every tick.
- `Date.parse` is implemented for the ISO 8601 forms that V8 accepts, which are what these logs use. Other formats
  (RFC 2822 and so on) count as invalid dates.
- Rows in `days`, `repos` and `sessions` are sorted by key or path. The JS order is object-insertion order, which the
  hub doesn't depend on.
- `--dry` and `--print` never write state (they can run beside the live service, where a second writer would corrupt
  it): no state file or directory is created, and an unusable state file is left in place instead of moved aside.
  They still read the existing state, or migrate the JS one, in memory.
- Counting quirks of the JS reporter that are fixed here (the golden file `expected-rs.json` pins them; see
  `tests/golden/README.md`, "Known JS quirks" and "Rust differences"):
  - 1: a Claude response streamed over several lines sharing `message.id` counts at its largest usage. A repeat id adds
    only the positive growth per field (input, cache write, cache read, output) to the day, model and repo of its
    first line, so a message never moves between days. The JS reporter counts only the first line, which has a partial
    `output_tokens` (measured on real data: output under-counted by 42.5%). The state keeps, per transcript, every id
    with its counted usage and bucket (`msgs`, `buckets`); on the 2,340 transcripts measured here the state file grew from 2.9 MB to 6.9 MB.
  - 2: assistant lines without `message.id` each count once (the JS reporter counts one per file).
  - 3: no 64-entry id ring for Claude: every id of a file is remembered. Files migrated from the JS state (or from an
    earlier version of this reporter) only have the last 64 ids and no usage: a repeat of one of those is treated as
    already counted, and only ids that come later use the max rule.
  - 5: a Codex fork or subagent (`parent_thread_id` in `session_meta`) whose first `token_count` has no
    `last_token_usage` takes that total as its baseline (counts 0) instead of the parent's whole running total.
  - 9: `toolKind` matches whole words (`mcp__srv__list_tools` is not `read` because of "ls"). MCP tools
    (`mcp__server__tool`) are classified by the tool part's leading verb: list/get/read/search/fetch/find -> read,
    create/update/write/delete/edit/set -> edit, otherwise tool.
  - Not changed: quirks 4, 6, 7, 8, 10, 11, 12, 13 (as in the JS reporter).
- `--once` exits 1 when the report fails, as `node reporter.mjs --once` does.
- The state is saved when it changed, at most every 5 minutes, and pending keys survive a restart. The JS reporter
  keeps them only in memory.

## Measurements

Linux, 16 cores, about 9.3 GB of transcripts (2,340 files), T3 cache 22 MB, release build (lto, codegen-units=1,
strip, panic=abort):

| | JS reporter | this |
| --- | --- | --- |
| binary | — | 3.4 MB (rustls included) |
| full rebuild, warm page cache | 42–61 s, 480–490 MB peak | 0.62 s, 65 MB peak (single thread 4.2 s, 28 MB) |
| full rebuild, cold cache | — | 2.9 s |
| steady tick (2,340 files, live sessions appending) | — | 8–12 ms |
| bytes written in 60 s with live sessions | 12.8 MB (state rewrite every tick) | 0 state bytes (at most one 6.9 MB save every 5 min while changing; none when idle) |
| steady RSS | 105–131 MB | 15 MB |

For parity, the JS reporter and this one each did a full rebuild of the same data with their own state directory,
POSTing to a capture server. All 191 day rows and 333 repo rows were identical, except today's
`claude-opus-5-5` row and one repo row. Those came from the two transcripts that were being written while the runs
were going.
