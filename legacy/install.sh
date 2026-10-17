#!/bin/sh
# Installs the agent stats reporter or hub as a background service (launchd on macOS, systemd --user on Linux).
#
#   ./install.sh reporter "MacBook Air"       # on every machine; the label is what the dashboard and site show
#   ./install.sh reporter-node "MacBook Air"  # rollback: the old Node reporter, same service name
#   ./install.sh hub                          # on the Mac mini
#
# `reporter` installs the Rust binary (stats/reporter-rs). Source, in order:
#   1. cargo present: cargo build --release, then copy the result
#   2. REPORTER_BIN=/path/to/agent-stats-reporter (a prebuilt binary)
#   3. otherwise warn and fall back to the Node reporter
# Re-running swaps the running service in place (same launchd label / systemd unit).
#
# Secrets live in ~/.config/agent-stats/: hub_token on every machine, publish_token on the hub.
set -eu

ROLE=${1:?usage: install.sh reporter <label> | reporter-node <label> | hub}
LABEL=${2:-$(hostname -s)}
HUB_URL=${HUB_URL:-http://mac-mini:7777}   # the hub; reporter.mjs has the same default

SRC=$(cd "$(dirname "$0")" && pwd)
DEST="$HOME/.local/share/agent-stats"
LOGS="$HOME/.local/state/agent-stats"
BIN="$DEST/agent-stats-reporter"
USER_NAME=${USER:-$(id -un)}
mkdir -p "$DEST" "$LOGS"
[ -s "$HOME/.config/agent-stats/hub_token" ] || { echo "missing ~/.config/agent-stats/hub_token" >&2; exit 1; }

warn() { echo "warning: $*" >&2; }

# Escape for XML text nodes (plist).
xml_esc() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g'; }
# Escape for a double-quoted systemd word: \ " and % (specifier). exec_esc also escapes $ (variable expansion).
unit_esc() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/%/%%/g'; }
exec_esc() { unit_esc "$1" | sed -e 's/\$/$$/g'; }

# Picks node for the Node roles and says which one.
need_node() {
  NODE=$(command -v node || true)
  [ -n "$NODE" ] || { echo "node not found on PATH" >&2; exit 1; }
  echo "using node: $NODE ($("$NODE" --version))" >&2
  echo "  (pinned into the service; re-run install.sh after switching node versions)" >&2
}

# Installs the Rust binary into $BIN, atomically (the old one may be running). Returns 1 if no source is available.
install_binary() {
  if command -v cargo >/dev/null 2>&1 && [ -f "$SRC/reporter-rs/Cargo.toml" ]; then
    echo "building reporter with cargo" >&2
    cargo build --release --manifest-path "$SRC/reporter-rs/Cargo.toml"
    FROM="$SRC/reporter-rs/target/release/agent-stats-reporter"
  elif [ -n "${REPORTER_BIN:-}" ]; then
    [ -x "$REPORTER_BIN" ] || { echo "REPORTER_BIN is not an executable file: $REPORTER_BIN" >&2; exit 1; }
    FROM=$REPORTER_BIN
  else
    return 1
  fi
  cp "$FROM" "$BIN.new"
  chmod 755 "$BIN.new"
  [ "$(uname)" = Darwin ] && xattr -d com.apple.quarantine "$BIN.new" 2>/dev/null || true
  mv -f "$BIN.new" "$BIN"
  echo "installed binary: $BIN" >&2
}

KIND=node   # node | binary
case $ROLE in
  reporter|reporter-node)
    if [ "$ROLE" = reporter ]; then
      if install_binary; then
        KIND=binary
      else
        warn "no cargo and no REPORTER_BIN; falling back to the Node reporter"
      fi
    fi
    if [ "$KIND" = node ]; then
      need_node
      cp "$SRC/reporter/reporter.mjs" "$DEST/"
      SCRIPT="$DEST/reporter.mjs"
    fi
    ROLE=reporter
    ENV=$(printf 'HUB_URL=%s\nSTATS_LABEL=%s' "$HUB_URL" "$LABEL") ;;
  hub)
    need_node
    [ -s "$HOME/.config/agent-stats/publish_token" ] || { echo "missing ~/.config/agent-stats/publish_token" >&2; exit 1; }
    cp "$SRC/hub/hub.mjs" "$SRC/hub/dashboard.html" "$DEST/"
    SCRIPT="$DEST/hub.mjs"
    : "${PUBLISH_URL:?set PUBLISH_URL to the relay's /publish URL}"
    HOST=$(tailscale ip -4 2>/dev/null || /Applications/Tailscale.app/Contents/MacOS/Tailscale ip -4)
    ENV=$(printf 'HOST=%s\nPUBLISH_URL=%s' "$HOST" "$PUBLISH_URL") ;;
  *) echo "unknown role: $ROLE" >&2; exit 1 ;;
esac
NAME="com.kilcazn.agent-stats.$ROLE"
if [ "$KIND" = node ]; then
  PATH_ENV="$(dirname "$NODE"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
  set -- "$NODE" "$SCRIPT"
else
  PATH_ENV="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
  set -- "$BIN"
fi

if [ "$(uname)" = Darwin ]; then
  PLIST="$HOME/Library/LaunchAgents/$NAME.plist"
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo "  <key>Label</key><string>$(xml_esc "$NAME")</string>"
    printf '  <key>ProgramArguments</key><array>'
    for a in "$@"; do printf '<string>%s</string>' "$(xml_esc "$a")"; done
    echo '</array>'
    echo "  <key>EnvironmentVariables</key><dict><key>PATH</key><string>$(xml_esc "$PATH_ENV")</string>"
    printf '%s\n' "$ENV" | while IFS= read -r kv; do echo "    <key>$(xml_esc "${kv%%=*}")</key><string>$(xml_esc "${kv#*=}")</string>"; done
    echo "  </dict>"
    echo "  <key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer>"
    echo "  <key>ProcessType</key><string>Background</string>"
    echo "  <key>StandardOutPath</key><string>$(xml_esc "$LOGS/$ROLE.log")</string><key>StandardErrorPath</key><string>$(xml_esc "$LOGS/$ROLE.log")</string>"
    echo '</dict></plist>'
  } > "$PLIST"
  launchctl bootout "gui/$(id -u)/$NAME" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
else
  UNIT="$HOME/.config/systemd/user/agent-stats-$ROLE.service"
  mkdir -p "$(dirname "$UNIT")"
  EXEC=""
  for a in "$@"; do EXEC="$EXEC \"$(exec_esc "$a")\""; done
  systemctl --user stop "agent-stats-$ROLE" 2>/dev/null || true
  {
    echo "[Unit]"; echo "Description=Agent stats $ROLE"; echo "After=network-online.target"
    echo "[Service]"; echo "ExecStart=${EXEC# }"; echo "Environment=\"PATH=$(unit_esc "$PATH_ENV")\""
    printf '%s\n' "$ENV" | while IFS= read -r kv; do echo "Environment=\"$(unit_esc "$kv")\""; done
    echo "Restart=always"; echo "RestartSec=10"; echo "Nice=10"
    echo "[Install]"; echo "WantedBy=default.target"
  } > "$UNIT"
  systemctl --user daemon-reload
  systemctl --user enable "agent-stats-$ROLE"
  systemctl --user restart "agent-stats-$ROLE"
  # Keep the user service running after logout / across reboots without a login.
  loginctl enable-linger "$USER_NAME" 2>/dev/null || warn "loginctl enable-linger failed; the service will stop when you log out (try: sudo loginctl enable-linger $USER_NAME)"
fi
echo "installed $ROLE ($LABEL, $KIND)"
