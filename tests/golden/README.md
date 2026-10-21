# Reporter golden tests

Behavioural spec for `legacy/reporter/reporter.mjs`, as a fake `$HOME` plus the report the JS reporter sends for it.
Any reimplementation (the Rust port) is correct when `golden.mjs` passes against it.

```
node tests/golden/golden.mjs                      # check the JS reporter against expected*.json
node tests/golden/golden.mjs --update             # rewrite expected.json from the command under test
node tests/golden/golden.mjs --cmd "target/release/agent-stats-reporter"
node tests/golden/golden.mjs --ticks 2            # two runs on the same state, also checks expected2.json
node tests/golden/golden.mjs --cmd target/release/agent-stats-reporter --expected expected-rs --ticks 2
                                                         # the Rust reporter against expected-rs.json / expected-rs2.json
```

`--expected <name>` (default `expected`) picks the files `<name>.json` and `<name>2.json`. The JS reporter is checked against
`expected*.json`, the Rust one against `expected-rs*.json` (see "Rust differences").

`--cmd` runs from the repo root through `sh -c "<cmd> --once"`. `--keep` keeps the temp dir (under `/var/tmp/agents/agent-stats-golden/`).

How it works: `fixtures/home/` is copied to a temp HOME. `{{HOME}}` is replaced in every file's contents and in file/dir names
(`%7B%7BHOME%7D%7D` in a name becomes `encodeURIComponent(HOME)`, used for Grok's URL-encoded cwd directories). File mtimes are
set to now-60s, except the four files in `OLD` in `golden.mjs` (now-2h, outside the 30 minute open window). The reporter runs with
`HOME=<temp>/home`, `XDG_STATE_HOME=<temp>/state`, `HUB_URL`, `HUB_TOKEN=test`, `STATS_MACHINE=fixture`, `STATS_LABEL=Fixture`.
The captured body is normalized: `at` and `platform` dropped; `lastActivity` becomes `~now`; `stateAt` becomes `~now` when it is
within 10 minutes of the run (a timestamp-less event falls back to `Date.now()`), otherwise it is kept exactly; HOME becomes
`{{HOME}}`; `days`, `repos`, `sessions` are sorted (by day|agent|model, day|repo, agent|id|cwd); object keys are sorted.
Nothing is rounded.

Ticks: `--once` is a fresh process each time, so `lastFull` is 0 and every report has `full: true` with all day rows; the
dirty-only (non-full) path is not exercised here. With `--ticks 2`, one line (`msg_TICK2`, 10 tokens, 2026-10-12T03:00Z) is
appended to the `-game-wt` transcript between runs. `expected2.json` differs from `expected.json` only by: new day row
`2026-10-12|claude|claude-sonnet-4-5` (10 tokens, output 9), repo row `2026-10-12|game` 10, and that session's tokens 325 -> 335,
io in 3 -> 4, out 12 -> 21, stateAt moved. The T3 import must not run again (totals otherwise unchanged).

## Totals in expected.json (tokens)

| agent | tokens | of which T3 import |
| --- | --- | --- |
| claude | 4544 | 1512 |
| codex | 11110 | 1000 |
| grok | 2125 | 100 |

## Checklist: fixture case -> expected numbers

Claude (`.claude/projects/...`), session `11111111-...0001` (cwd `~/Dev/site`) = 1497 tokens, io in 43 / cr 1300 / cw 100 / out 54
- `msg_A1` streamed on 3 lines (thinking, text, tool_use Bash) with growing output: counted once, from the first line: 1115 tokens, output 5 (2026-10-10, opus-4-5).
- malformed JSON line (complete line, bad JSON): skipped, no effect.
- `msg_A2` 14:59:59.999Z, usage with only input/output (7+3 = 10): day 2026-10-10 (sonnet-4-5). `msg_A3` 15:00:00.000Z (360): day 2026-10-11.
- `msg_SYN` model `<synthetic>`: counted, 10 tokens as a day row with model `<synthetic>` on 2026-10-11, but does not change the session model (stays sonnet-4-5).
- `msg_NOU` no usage: nothing counted (state -> writing). `msg_NOM` no model: counted under model `unknown` (2 tokens, 2026-10-11), state thinking (redacted_thinking), final stateAt 1791676860000.
- `msg_TAIL` is the last line with no trailing newline: never counted (999 tokens would be obvious).
- subagent `agent-ab12cd` (haiku-4-5): 283 tokens, `kind: subagent`, `parent` = `11111111-...0001`, state writing.
- `88888888-...0008` (haiku): 264 tokens. Ring buffer test, see quirks.
- `22222222-...0002` cwd `/tmp/t3-namer-xyz`: 120 tokens counted (2026-10-10 haiku) and repo `t3-namer-xyz`, but absent from `sessions`.
- `33333333-...0003` cwd `~/.t3/worktrees/blog/feature-x`, 2h old: 515 tokens to repo `blog` (2026-10-09), not in `sessions`.
- `44444444-...0004` cwd `~/Projects/game/.claude/worktrees/agent-1`: 325 tokens, repo `game`, session project `agent-1`.
- `55555555-...0005` cwd `~/notes/misc`: repo `misc` (basename fallback). `99999999-...0009` cwd `/srv/app/web`, 2h old: repo `web`.
- `66666666-...0006`: first assistant line has no cwd yet (2 tokens, day only, no repo); after a user line sets cwd `~/.t3/scratch/idea1` the next 4 tokens go to repo `scratch`. 2026-10-12 repo `scratch` = 4.
- `77777777-...0007` no cwd at all: tokens counted, no repo, session `cwd: null`, `repo: null`, project `-nocwd` (project dir name). Total 10, see quirks. A line with usage but no `timestamp` is ignored for counting.
- `kind-*` sessions (no usage, cwd `~/Dev/kinds`): state per tool name. Bash/BashOutput -> run; Edit/NotebookEdit/TodoWrite -> edit; Read/WebFetch -> read; Task/Agent -> delegate; AskUserQuestion -> waiting; `mcp__srv__do`/ExitPlanMode -> tool; `mcp__srv__list_tools` -> read (quirk); thinking/writing blocks; a tool_result user line -> thinking; assistant line without timestamp -> stateAt `~now`.
- `readme.txt` in a project dir is not a `.jsonl`: ignored.

Codex (`.codex/sessions/2026/...`, `.codex/archived_sessions/`)
- `...0a` parent (4300 tokens; cwd from session_meta `~/.codex/worktrees/ab12/webapp` -> repo `webapp`): `info: null` skipped; first event 1100 (io in 200/cr 800/out 100); identical repeat skipped; event with `last_token_usage` lacking `total_tokens` -> 1550 (input+output); event with no `last_token_usage` -> delta from total 3700-2650 = 1050 (output 0, no io), at 15:00:30Z -> 2026-10-11; timestamp-less event ignored; event with `cache_write_input_tokens` -> 500 (io cw 20); then turn_context switches model to `gpt-5.4-mini` and cwd to `~/Dev/site`: last 100 counted to model gpt-5.4-mini and repo `site`. Session cwd = last seen (site), model gpt-5.4-mini, state `read` (web_search_call) at 1791644530000. io in 1090 / cr 1900 / cw 20 / out 260. Day rows: 2026-10-10 gpt-5.5 2650 (out 150); 2026-10-11 gpt-5.5 1550 (out 100) + gpt-5.4-mini 100 (out 10). Repo rows: webapp 2026-10-10 2650 and 2026-10-11 1550.
- `...0b` subagent (parent_thread_id = `...0a`): total starts at 4300 but `last_token_usage` exists, so only 200 + 300 = 500 counted. `kind: subagent`, `parent` = full basename of the `...0a` file. Same basename in `archived_sessions/` (777777 tokens, model `gpt-archived-dup`) is ignored.
- `...0c` subagent with unknown parent: `parent: null`. First event has no `last_token_usage`, so the whole 4300 is counted (quirk), then 50 -> 4350.
- `...0d` archived-only, 2h old: 900 tokens on 2026-09-01 (gpt-5.3-codex), not in `sessions`.
- `...0e` cwd `/private/var/folders/...`, no turn_context: 60 tokens under model `unknown`, repo `helper`, not in `sessions`.
- Session ids are the rollout basename without `.jsonl`.

Grok (`.grok/sessions/<url-encoded cwd>/<id>/updates.jsonl`; `events.jsonl` with 888888 tokens is ignored)
- `g1000000-...0001` cwd (decoded from dir name) `~/Dev/site`, 1885 tokens: p1 -> grok-4 1100 (uses `totalTokens`; io in 400/cr 600/cw 50/out 100) and grok-code-fast 550 (input+output); repeated prompt_id p1 skipped; p2 at 15:00:00Z has no `modelUsage`, so it is attributed to the session's last model (grok-code-fast) on 2026-10-11 (15); turn without prompt_id counted (grok-4 220, 2026-10-11); turn with no timestamp ignored. Final model grok-4, state writing at agentTimestampMs 1791676810500. Grok events with no `_meta.agentTimestampMs` fall back to `timestamp * 1000` (`Date.parse(number)` is NaN, so `Number()` is used).
- `g2000000-...0002` cwd `/tmp/grokhelper`: 40 tokens, model `unknown`, repo `grokhelper`, not in `sessions`.
- `g3000000-...0003` cwd `/Users/me/my proj` (outside HOME, `%20` decoded), 2h old: 100 tokens on 2026-10-08 (grok-3), repo `my proj`.

T3 usage cache (`.t3/userdata/usage-scan-cache.json` legacy + `usage-scan-cache-v5.json`; imported only on the first run, no repo attribution)
- `-gone/old-session.jsonl` (missing): k1 100+1000+50+25 = 1175 on 2026-09-20 (opus, 14:59:59.999Z); k2 15 on 2026-09-21 (sonnet, index 3, 15:00:00Z); second k1 skipped; `<synthetic>` (k3) skipped; two key-less identical records both counted (5 each, 2026-09-21); all-zero k4 adds nothing.
- `-gone/old-session2.jsonl`: its k1 (cross-file duplicate) skipped, k5 250 counted. 2026-09-21 opus total = 250 + 10 = 260, output 54.
- `-gone/override.jsonl`: present in both files; v5 wins (12 tokens, 2026-09-22), legacy's 99999-token record is dropped.
- `-gone/legacy.jsonl`: legacy file with its own `models` order, `[gpt-5.5, claude-opus-4-5]`: 50 tokens on 2026-07-15 (opus).
- codex path (missing): 1000 on 2026-08-01 (gpt-5.5). grok path (missing): 100 on 2026-09-10 (model index 2 = gpt-5.5, agent grok).
- Ignored: path that exists in state (the Claude `...0001.jsonl`), path that exists on disk but is not a scanned transcript (`readme.txt`), unknown agent `foo`, `r` not an array.

## Known JS quirks (pinned in expected.json; the Rust port fixes 1, 2, 3, 5 and 9, see "Rust differences")

1. Streamed Claude responses: the first line of a message id wins, and its `output_tokens` is usually a partial count, so output is under-counted (msg_A1: 5 counted, 60 on the last line).
2. Assistant lines with no `message.id` all share the id `undefined`: only the first is counted (`77777777`: 2 counted, the 100-token second one skipped).
3. The id memory is a 64-entry ring: a message id replayed after 64 other ids is counted again (`...0008`: `msg_R0` counted twice, 264 total).
4. `<synthetic>` Claude responses are counted in day rows under model `<synthetic>` (live scan), but skipped by the T3 import. A missing `message.model` is counted under `unknown` and does not change the session model.
5. Codex fork/subagent whose first `token_count` has no `last_token_usage` counts the parent's full running total (`...0c`: 4300). Delta events (no `last_token_usage`) also report output 0 and add nothing to io.
6. A Codex `token_count` event is skipped whenever its total equals the previous total, even if `last_token_usage` differs.
7. Grok: a `turn_completed` without `modelUsage` is attributed to the last model of the previous turn (iteration order of `modelUsage`), and a turn without `prompt_id` is never deduped.
8. Tokens are attributed to the cwd seen so far, while the session reports the final cwd/model (`...0a`: webapp then site).
9. `toolKind` matches unanchored substrings in order run, edit, read, delegate, waiting: `mcp__srv__list_tools` is `read` because of `ls`; `ExitPlanMode` falls through to `tool`.
10. T3 import order follows object insertion order of the merged `files` map (legacy cache paths first, v5 overrides values but keeps position), and the dedupe `keys` set is global across paths, so which duplicate wins depends on that order.
11. Imported tokens never get a repo. A Claude transcript basename that is a UUID is also registered as a Codex thread id for parent lookup.
12. A Grok session directory name with an invalid percent escape makes `decodeURIComponent` throw out of `scan()` (not covered by fixtures).
13. `at` / stateAt fall back to `Date.now()` when an event has no usable timestamp, so such a session's `stateAt` is nondeterministic (normalized to `~now`).

## Rust differences (expected-rs*.json vs expected*.json)

The Rust reporter fixes some quirks above; the JS reporter stays as it is (the rollback path). `expected-rs.json` /
`expected-rs2.json` differ from `expected.json` / `expected2.json` only here (identical in both ticks):

| Quirk | Fix in Rust | Numbers |
| --- | --- | --- |
| 1 streamed Claude responses | each message id counts at its largest usage per field; later lines add only the positive growth, to the day/model/repo of the first line. Remembered per file as `id -> counted usage` (no ring), persisted in the state | `msg_A1`: output 5 -> 60, tokens 1115 -> 1170 (opus-4-5 2026-10-10 output 5 -> 60); session `...0001` tokens 1497 -> 1552, io out 54 -> 109 |
| 2 assistant lines without `message.id` | each such line counts once | `...0007`: tokens 10 -> 110, io in 6 -> 56, out 4 -> 54; opus-4-5 2026-10-12 tokens 28 -> 128, output 14 -> 64 |
| 3 id ring of 64 | removed by the fix for quirk 1 (all ids of a file are remembered) | `...0008`: `msg_R0` counted once, tokens 264 -> 164; haiku-4-5 2026-10-10 667 -> 567; repo `site` 2026-10-10 3322 -> 3277 (-100 from R0, +55 from A1) |
| 5 Codex fork, first `token_count` without `last_token_usage` | that total is the baseline (counts 0), deltas count after | `...0c`: tokens 4350 -> 50; gpt-5.5 2026-10-11 5900 -> 1600; repo `site` 2026-10-11 5057 -> 757 |
| 9 `toolKind` substrings | whole-word matching; MCP tools (`mcp__server__tool`) by the tool part's leading verb: list/get/read/search/fetch/find -> read, create/update/write/delete/edit/set -> edit, else tool | no number changes: `mcp__srv__list_tools` is still `read`, now because of the verb `list` (and `mcp__srv__do` is `tool`) |

Additions, not in JS (0.7): every repo row carries `io` (`{in, cr, cw, out}`, the row's tokens by kind), its sum the row's
`tokens` except where the agent didn't split them (Codex delta events, quirk 5: repo `webapp` 2026-10-11 has 1550 tokens,
`io` 500; the T3 import gives the output only). Codex's and Grok's input counts their cache reads and writes, so fresh
input (`io.in`, in repo rows and sessions) is input less both: session `...0a` io in 1090 -> 1070, Grok `g1000000-...0001`
io in 1106 -> 1056.

Additions, not in JS (0.8): a Claude session written in the last 10 minutes says `alive`, whether a Claude Code process
that may write it runs. No process owns the fixture's folders, so every such session has `"alive": false`. The fixtures end
no turn, so no session is `idle`.

Left as in JS: quirks 4, 6, 7, 8, 10, 11, 12, 13. Totals (tokens) in `expected-rs.json`: claude 4544 -> 4599 (+55 for A1, +100 for the id-less line, -100 for R0), codex 11110 -> 6810 (-4300), grok 2125 unchanged.
