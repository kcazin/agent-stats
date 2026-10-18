# agent-stats

Live token and agent-activity stats from Claude Code, Codex and Grok, across all your machines.

One binary, three roles. A **reporter** on every machine reads agent transcripts and pushes daily token totals and
live sessions to a **hub**. The hub keeps history, serves a private dashboard, maps repositories to projects and,
optionally, publishes a public snapshot to a **relay** a website can read.

```
 reporter ─┐
 reporter ─┼─▶ hub (SQLite, dashboard, projects) ─▶ relay (optional, public) ─▶ your website
 reporter ─┘
```

Everything pushes; nothing connects in to your machines. Paths, repo names and git remotes stay on the hub; the
relay only gets project totals and opaque session ids. Design and contracts: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Pick where the hub lives

| Setup | When | Hub | Reporters reach it via |
|---|---|---|---|
| **Fleet** | 2–5 machines you use, no main one | every Mac is a replica ([docs/FLEET.md](docs/FLEET.md)) | whichever replica answers |
| **Solo** | one machine | same machine | `http://127.0.0.1:7777` |
| **Home** | an always-on machine and a private network (Tailscale, LAN) | e.g. a Mac mini | `http://mac-mini:7777` |
| **Cloud** | no always-on machine, or reporters on different networks | a container with a volume ([docs/deploy.md](docs/deploy.md)) | `https://stats.example.com` |

## Set up a fleet

Every replica keeps every machine's history and shows the whole dashboard on its own, so nothing stops when one
machine sleeps, restarts or goes offline. With Tokiscope this is the default; by hand:

```sh
agent-stats fleet create --name home            # on the first machine
agent-stats machines add --ssh me@linux-box     # from any replica: installs, joins, waits for the first report
agent-stats machines invite                     # or a line to paste on a machine you can't SSH to
agent-stats fleet request http://mac-mini:7777  # on another Mac: then approve it on any replica (same 4 digits)
agent-stats fleet status                        # replicas, who publishes, when each was last reached
```

## Set up a single hub

**1. The hub** (once). On the hub machine:

```sh
curl -fsSL https://raw.githubusercontent.com/kcazin/agent-stats/main/install.sh | sh -s -- "Mac mini" --hub
$EDITOR ~/.config/agent-stats/hub.toml        # name, listen address, projects (see examples/hub.example.toml)
agent-stats service install hub
```

**2. Each machine.** On the hub, make a one-time code; on the new machine, install and join:

```sh
agent-stats hub pair --label "MacBook Air"      # on the hub: prints `agent-stats join <url> --code XXXX-XXXX`
```

```sh
curl -fsSL https://raw.githubusercontent.com/kcazin/agent-stats/main/install.sh | sh -s -- "MacBook Air"
agent-stats join http://mac-mini:7777 --code XXXX-XXXX
agent-stats status
```

A machine can report to several hubs (run `join` again with another hub's code); each gets its own token and keeps
its own place in the stream. `agent-stats hubs` lists them, `agent-stats hub revoke <machine>` cuts one off. Each hub
sees everything this machine reports (every repo, remote, session and the whole history); only add hubs you'd trust
with that.

**3. Public stats (optional).** Either run a relay ([docs/deploy.md](docs/deploy.md), one Docker image) and add a
`[[publish]]` target to `hub.toml`, or, for a cloud hub, set `public = true` and read `GET /stats` from the hub.

## Day to day

```sh
agent-stats status            # is the reporter running, when did each hub last accept a report
agent-stats doctor            # checks config, transcripts, hubs, tokens, the service; prints fixes
agent-stats hub repos         # on the hub: every repo, its git remote, project, tokens; new ones show as unassigned
agent-stats update            # install the latest release, restart, roll back automatically if it doesn't come up
                              # (the updater installed by install.sh does this every 15 min: hub from GitHub, others from the hub)
agent-stats rollback
```

New repositories are picked up automatically (identified by their git remote, across machines and worktrees). They
count as `other` publicly until you add them to a `[[project]]` in `hub.toml`; set `new_repo_webhook` to be told
when one appears.

More: [docs/install.md](docs/install.md) (install, update, uninstall, migrating from the JS version),
[docs/deploy.md](docs/deploy.md) (relay and cloud hub).

## Development

```sh
cargo test --workspace
node tests/golden/golden.mjs --cmd "target/release/agent-stats report" --expected expected-rs --ticks 2   # reporter payloads
node tests/hub-parity/parity.mjs --rust target/release/agent-stats                                         # hub vs the JS hub
```

`legacy/` holds the original JS reporter, hub, relay and installer: the reference the Rust versions are checked
against, and the rollback path until every machine runs the Rust version.

## License

GPL-3.0-or-later: see [LICENSE](LICENSE). You may use, change and share agent-stats; whatever you distribute that
builds on it must be under the GPL too, with its source.
