#!/bin/sh
# Installs agent-stats on this machine and starts the reporter service (systemd --user on Linux, launchd on macOS).
#
#   gh api repos/kcazin/agent-stats/contents/install.sh -H 'Accept: application/vnd.github.raw' | sh -s -- "MacBook Air"
#   ./install.sh "MacBook Air"            # from a checkout; builds from source if there is no release
#
# usage: install.sh [label] [--hub] [--no-service] [--no-auto-update] [--from-source]
#   label          what dashboards show for this machine (default: the hostname)
#   --hub          also install the hub service on this machine
#   --no-service   install the binary only, no service
#   --no-auto-update  skip the scheduled updater (it runs `update --auto` every 15 minutes)
#   --from-source  build with cargo from the checkout instead of downloading a release
#
# The repo is private: releases are downloaded with `gh` (run `gh auth login` first).
# Env: AGENT_STATS_REPO overrides the repository (default kcazin/agent-stats).
set -eu

REPO=${AGENT_STATS_REPO:-kcazin/agent-stats}
LABEL=""
WITH_HUB=0
WITH_SERVICE=1
AUTO_UPDATE=1
FROM_SOURCE=0

usage() { sed -n '2,14p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' >&2 || true; }
die() { echo "install.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case $1 in
    --hub) WITH_HUB=1 ;;
    --no-service) WITH_SERVICE=0 ;;
    --no-auto-update) AUTO_UPDATE=0 ;;
    --from-source) FROM_SOURCE=1 ;;
    -h|--help) usage; exit 0 ;;
    -*) usage; die "unknown option $1" ;;
    *) [ -z "$LABEL" ] || die "more than one label given"; LABEL=$1 ;;
  esac
  shift
done
[ -n "$LABEL" ] || LABEL=$(hostname -s 2>/dev/null || hostname)

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  *) die "unsupported platform $(uname -s)-$(uname -m)" ;;
esac

# Inside a checkout? ($0 is the script when run as a file; piped into sh it is just "sh".)
CHECKOUT=""
if [ -f "$0" ]; then
  dir=$(cd "$(dirname "$0")" && pwd)
  if [ -f "$dir/Cargo.toml" ] && [ -d "$dir/crates/cli" ]; then CHECKOUT=$dir; fi
fi

# Work under the data dir, not /tmp: the downloaded binary is run from here and /tmp may be mounted noexec.
# ($TMPDIR is only a fallback.) `agent-stats` prunes leftover .install-<pid> dirs of dead processes.
if [ -n "${XDG_DATA_HOME:-}" ]; then DATA_HOME=$XDG_DATA_HOME/agent-stats
else DATA_HOME=${HOME:?HOME is not set}/.local/share/agent-stats; fi
TMP="$DATA_HOME/.install-$$"
if mkdir -p "$DATA_HOME" 2>/dev/null && rm -rf "$TMP" && mkdir -m 700 "$TMP" 2>/dev/null; then :
else TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-stats-install.XXXXXX") || die "cannot create a temp dir"; fi
cleanup() { rm -rf "$TMP"; }
# Clean up and stop on a signal (a bare cleanup trap would carry on into the fallback path).
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust (https://rustup.rs) or use a release"
  echo "building agent-stats with cargo (this takes a few minutes)" >&2
  (cd "$CHECKOUT" && cargo build --release -p agent-stats) >&2
  BIN="${CARGO_TARGET_DIR:-$CHECKOUT/target}/release/agent-stats"
  [ -x "$BIN" ] || die "build finished but $BIN is missing"
}

download_release() {
  command -v gh >/dev/null 2>&1 || return 1
  # `gh auth token`, not `gh auth status`: status fails when any stored account is broken, even an inactive one.
  gh auth token >/dev/null 2>&1 || { echo "gh is not logged in (gh auth login)" >&2; return 1; }
  echo "downloading the latest release for $TARGET from $REPO" >&2
  gh release download --repo "$REPO" --pattern "agent-stats-*-$TARGET.tar.gz" --pattern SHA256SUMS --dir "$TMP" >&2 || return 1
  asset=$(cd "$TMP" && ls agent-stats-*-"$TARGET".tar.gz 2>/dev/null | head -n 1)
  [ -n "$asset" ] || return 1
  want=$(grep "  $asset\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
  [ -n "$want" ] || die "SHA256SUMS has no entry for $asset"
  [ "$(sha256_of "$TMP/$asset")" = "$want" ] || die "checksum mismatch for $asset"
  echo "checksum ok: $asset" >&2
  mkdir -p "$TMP/x"
  tar -xzf "$TMP/$asset" -C "$TMP/x"
  BIN="$TMP/x/agent-stats"
  [ -x "$BIN" ] || die "$asset has no agent-stats at its root"
}

BIN=""
if [ "$FROM_SOURCE" = 1 ]; then
  [ -n "$CHECKOUT" ] || die "--from-source needs to run from a checkout (./install.sh)"
  build_from_source
elif ! download_release; then
  if [ -n "$CHECKOUT" ] && command -v cargo >/dev/null 2>&1; then
    echo "no release available; building from source" >&2
    build_from_source
  else
    die "could not download a release (is gh installed and logged in, and is there a release for $TARGET?). In a checkout with cargo, use --from-source."
  fi
fi

if [ "$WITH_SERVICE" = 1 ]; then
  "$BIN" service install report --label "$LABEL"
  if [ "$WITH_HUB" = 1 ]; then "$BIN" service install hub; fi
  if [ "$AUTO_UPDATE" = 1 ]; then "$BIN" service install updater; fi
else
  "$BIN" update --from "$BIN" --no-restart
fi

cat <<NEXT

agent-stats is installed ($("$BIN" version)).

Next steps
  1. On the machine that runs the hub:   agent-stats hub pair --label "$LABEL"
  2. Here:                               agent-stats join <hub-url> --code <CODE>
  3. Check everything:                   agent-stats status      (and: agent-stats doctor)
  Update later with:                     agent-stats update      (agent-stats rollback to go back)
NEXT
