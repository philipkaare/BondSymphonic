#!/usr/bin/env bash
# Explicitly runs a live, authenticated model against a disposable sandbox.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
: "${BS_CODEX_BIN:?Set the pinned Linux Codex executable}"
: "${BS_CODEX_CODE_MODE_HOST:?Set the matching Code Mode helper}"
: "${BS_CODEX_HOME:?Set the shared authenticated Codex home}"
: "${BS_CODEX_CAPTURE_DIR:?Set a disposable Linux directory for the capture}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.bondsymphonic/target}"
export BS_DAEMON_EXE="$CARGO_TARGET_DIR/debug/bondsymphonic-daemon"
export BS_CODEX_PROBE="$repo/scripts/probe-codex.py"
cd "$repo"
cargo build -p bondsymphonic-daemon
cargo test -p bondsymphonic-daemon --test codex_live_preflight -- --ignored --nocapture
