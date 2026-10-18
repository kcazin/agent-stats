# Installing agent-stats

One binary, `agent-stats`, installed per user (no root). Binaries live in versioned directories so an update can be
tried and undone:

```
~/.local/share/agent-stats/versions/<version>/agent-stats   (+ .install-complete)
~/.local/share/agent-stats/current -> versions/<version>
~/.local/share/agent-stats/install-state.json               active, previous, last update
~/.local/bin/agent-stats -> ~/.local/share/agent-stats/current/agent-stats
```

Services run `current/agent-stats`, so switching `current` switches what runs. The last 3 versions are kept (never
the current or the previous one). If `~/.local/bin` is not on your `PATH`, the installer says so; add it to your shell
profile.

`update`, `rollback` and `service install` hold an exclusive lock (`install.lock` in the data directory), so two of
them never interleave. A second one prints "another agent-stats install/update is running, waiting…" and waits up to
5 minutes, then fails. A complete version directory is never replaced or removed while `current` points at it.

## A new machine

```sh
curl -fsSL https://raw.githubusercontent.com/kcazin/agent-stats/main/install.sh | sh -s -- "MacBook Air"
```

The argument is the label dashboards show (default: the hostname). The script works in a temporary `.install-<pid>` directory under the data directory (so a `noexec` `/tmp` does not
matter; `$TMPDIR` is only a fallback) and cleans it up when interrupted. It downloads the release for this
OS/architecture, verifies it against `SHA256SUMS`, then runs `agent-stats service install report --label …`, which
installs the binary into the layout above and starts the reporter service (systemd `--user` unit
`agent-stats-reporter.service` on Linux, LaunchAgent `dev.agent-stats.report` on macOS; restart on crash after 10 s).
If the binary you run `service install` from is not inside `versions/` (an old checkout build, an extracted
tarball), it is installed as a version and `current` moves to it, printing `current: A -> B`; it refuses to move
`current` to a lower version unless you pass `--force`. On Linux it also runs `loginctl enable-linger` so the reporter survives logout (it warns if that fails).

From a checkout, `./install.sh "label"` does the same, and builds with cargo when there is no release (or with
`--from-source`). Options: `--hub` also installs the hub service, `--no-service` installs the binary only,
`--version V` installs that release instead of the latest, `--archive FILE` installs a release archive already on
the machine (checked against the `SHA256SUMS` beside it), and `--join URL --code CODE` joins a hub or fleet once
the reporter runs. When no release can be downloaded the script exits with status 3.

Supported targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`,
`x86_64-apple-darwin`.

## Adding a machine from one that runs the hub

On a machine with the hub (a Mac with Tokiscope, or `agent-stats service install hub`):

```sh
agent-stats machines discover                    # Tailscale peers and Bonjour adverts, and which run agent-stats
agent-stats machines add --ssh me@cachy          # installs agent-stats there and joins it: nothing to type there
agent-stats machines invite                      # or: a one-time line to paste on the new machine
agent-stats machines list                        # the machines of this fleet
agent-stats machines remove cachy --ssh me@cachy # revoke, and stop its reporter
```

`add --ssh` uses the system `ssh` (your `~/.ssh/config`, agent and Tailscale SSH work) in batch mode, so it never
waits on a password: a machine that wants one is reported with the `ssh-copy-id` line to run. A first connection
accepts the host key; a changed key stops it. It probes the machine, finds an address of this machine that answers
from there (with this hub's id), installs this machine's version of agent-stats with install.sh (uploading the
release over SSH if the machine can't download it), joins it with a pairing code made here and passed only over
SSH, and waits for its first report. `--replace` re-adds a machine whose name is already enrolled (reinstalled).
Every command takes `--json` (docs/FLEET.md has the shapes).

`invite` prints `curl … install.sh | sh -s -- --join <url> --code <code>`, valid 10 minutes, one use.

## Adding a hub

On the machine that runs the hub (`agent-stats service install hub`, or `install.sh --hub`):

```sh
agent-stats hub pair --label "MacBook Air"      # prints a one-line join command, valid for 10 minutes
```

On the new machine:

```sh
agent-stats join http://mac-mini:7777 --code K7Q2-9XMD
agent-stats status
```

`agent-stats service install relay` sets up the optional public relay the same way.

## Updating

```sh
agent-stats update                  # latest GitHub release
agent-stats update --version 0.3.1
agent-stats update --from ./agent-stats-0.3.1-x86_64-unknown-linux-gnu.tar.gz   # or a bare binary
agent-stats update --no-restart     # install and switch, leave the running services alone
```

Releases are fetched with `gh release download` when `gh` is installed and logged in, otherwise over HTTPS
(with `GITHUB_TOKEN` or `GH_TOKEN` when set, which a private fork needs). `AGENT_STATS_REPO=owner/name` points it at another repository. The archive is checked
against `SHA256SUMS` (a local `.tar.gz` is checked against a `SHA256SUMS` next to it, if there is one), and the binary
must print the expected version before it is moved into place.

Then it switches `current`, restarts every installed agent-stats service, and runs a health trial: each restarted
role must write its status file (`state_dir/<role>-status.json`) with the new version, a live pid and a heartbeat
newer than the restart, within 90 seconds. If any role fails, `current` is switched back, the services are restarted
again, the reason is printed and the command exits non-zero. A role must also stay up, with the same pid, for 8
seconds after its first good status; a binary that dies right after starting fails the trial. The outcome is recorded
in `install-state.json`.

Before `current` moves, `install-state.json` gets a `trial` record (`from`, `to`, `pid`, `started`). SIGHUP (a dropped
ssh session), SIGINT and SIGTERM during the trial roll back to the old version and exit non-zero. If the updater is
killed outright (SIGKILL, power loss) the record stays: `agent-stats service status` warns about it, and the next
`agent-stats update` or `agent-stats rollback` first rolls back to the old version (restart and health check) before
doing anything else. Running `update` again for the interrupted version therefore verifies it properly instead of
reporting "already the active version". Binaries, the install marker and the directories are fsynced before `current`
switches. Leftover `.update-<pid>` and temp files of dead processes are removed by the next prune.

On Linux the unit is written only after the whole definition validated; a data directory path containing `'`, `"`,
`\` or control characters is refused up front (systemd cannot execute such a path; set `XDG_DATA_HOME` elsewhere). If
the new unit fails to start, the previous unit is put back and restarted.

## Automatic updates

`install.sh` also installs the **updater**: a systemd timer (`agent-stats-updater.timer`) or a LaunchAgent
(`dev.agent-stats.updater`) that runs `agent-stats update --auto` every 15 minutes. It is its own unit, so the update
it runs can restart the report and hub services without killing itself. Add it to an existing machine with:

```sh
agent-stats service install updater              # --every 1h for another interval (minimum 5m)
agent-stats service status                       # ...updater: every 15m, last run 3 min ago: up-to-date (0.4.0)
agent-stats service uninstall updater
```

Releases flow hub first:

- **The hub machine** (the hub service is installed) follows GitHub. It takes a release once it has been published
  for `soak_hours`, updates like `agent-stats update` (health trial, automatic rollback), then mirrors every target's
  tarball and `SHA256SUMS` into `data_dir/releases/<version>/`. It re-checks the mirror on every run, so a failed
  mirror heals on the next one.
- **Every other machine** follows the hub it joined first: `GET /release` with its machine token, then the tarball
  for its target from the hub, checked against `SHA256SUMS`. The hub only offers the version it is running itself,
  so a reporter is never ahead of its hub. No GitHub credentials are needed on these machines.

A version that fails its health trial and is rolled back goes on a failed list and is skipped by later runs until a
newer one is out (`agent-stats update --version V` still installs it by hand). The last run's outcome is in
`data_dir/auto-update.json`; `--auto` never downgrades.

Settings, all optional, in `~/.config/agent-stats/update.toml`:

```toml
auto = true          # false: the timer still runs but does nothing
source = "github"    # "github" or "hub"; default github when the hub service is installed here, else hub
hub = "home"         # source = hub: which joined hub to follow (default: the first in config.toml)
soak_hours = 6       # source = github: how long a release must have been out before it is taken
```

On a Mac hub, `gh` keeps its token in the login keychain, which a LaunchAgent may not reach. If the updater log
(`~/.local/state/agent-stats/logs/updater.log`) shows GitHub auth errors, put a read-only fine-grained token for the
repository (Contents: read) in `~/.config/agent-stats/secrets/github.token` (mode 0600).

## Rolling back

```sh
agent-stats rollback        # back to the previous version, restart, health check
agent-stats service status  # versions, and per role: installed, running, version, heartbeat age
```

## Uninstalling

```sh
agent-stats service uninstall report     # and hub / relay if installed
rm -rf ~/.local/share/agent-stats ~/.local/bin/agent-stats
# optional, deletes your config, hub tokens and reporter state:
rm -rf ~/.config/agent-stats ~/.local/state/agent-stats
```

## Migrating a machine that runs the old JS reporter

Run the installer; nothing needs to be stopped first.

- **Linux.** The new unit has the same name as the old one, `agent-stats-reporter.service`, so it is replaced in
  place: `ExecStart` now runs `current/agent-stats report`. The unit is read before it is overwritten and `HUB_URL`,
  `STATS_LABEL`, `STATS_MACHINE`, `STATS_TZ`, `HOST`, `PORT`, `PUBLISH_URL` and `GITHUB_USER` are copied into the new
  `Environment=` lines, so the machine keeps reporting to the same hub under the same name before any `config.toml`
  exists. `--label` overrides `STATS_LABEL`. Other variables (including the old `PATH`) are not carried over.
- **macOS.** The old LaunchAgents `com.kilcazn.agent-stats.reporter` and `.hub` get the same treatment: their
  environment is read and the new plist is checked with `plutil -lint`. Then the old agent is `launchctl bootout`'d
  (the hub has to free its port) and its plist renamed to `*.plist.bak`, and `dev.agent-stats.<role>` is bootstrapped
  in its place (KeepAlive, 10 s throttle, background, logs in `state_dir/logs/<role>.log`). The installer waits up to
  10 seconds for it to run; if it does not (or `bootstrap` fails), it is booted out, the old plist is restored and
  bootstrapped again, and the command fails. The `.bak` is deleted only after success. Over SSH without a logged-in
  desktop session `bootstrap gui/<uid>` fails with "Domain does not support specified action"; the error says so: log
  in to the Mac (or run it from Terminal there) and re-run.
- The old `~/.config/agent-stats/hub_token` keeps working through the legacy-token path; move to a per-machine token
  with `agent-stats join` when convenient.
- The old files in `~/.local/share/agent-stats/` (`reporter.mjs`, `agent-stats-reporter`) are left alone and can be
  deleted. To go back to the JS reporter, run `legacy/install.sh reporter-node "<label>"`; it uses the same unit name.
