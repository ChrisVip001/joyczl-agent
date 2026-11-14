#!/usr/bin/env bash
# Shared preamble for the smoke scripts: where the repo is, where the binary is, a temp dir that
# always gets cleaned up, and a `fail` that prints one line and exits.
#
#   source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
#   need_bin                       # fail early with the build hint
#   track_pid "$SOME_PID"          # register a background process for cleanup
#
# Why a library at all: the same ten lines were copied into six scripts, and the copies had
# already drifted — one cleaned up `$FAKE_PID` while its variables were named `$CB_PID`, another
# reset `trap` mid-run and dropped the cleanup entirely. A copy that drifts is worse than a
# copy that is merely repeated: it fails at the one moment you need it (leaving a server running
# on a port, so the *next* run fails somewhere unrelated).
#
# PIDs are **registered**, not named in `cleanup`: a script that grows a second background
# process then cannot forget to kill it.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/joy-rs/target/debug/joy"
WORK="$(mktemp -d)"

_PIDS=()

cleanup() {
  for pid in ${_PIDS[@]+"${_PIDS[@]}"}; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
  [[ -n "${WORK:-}" ]] && rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "✗ $*" >&2
  exit 1
}

track_pid() {
  _PIDS+=("$1")
}

need_bin() {
  [[ -x "$BIN" ]] || fail "cannot find $BIN; run: cd joy-rs && cargo build"
}
