#!/usr/bin/env bash
# Builds the Linux daemon inside the `bondsymphonic` Lima VM and copies it to
# target/daemon/, where the IDE's launcher picks it up and installs it into the
# VM. The macOS counterpart of build-daemon.ps1.
#
#   scripts/build-daemon.sh [--debug]
set -euo pipefail
VM="bondsymphonic"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
profile_flag="--release"
profile_dir="release"
if [ "${1:-}" = "--debug" ]; then
  profile_flag=""
  profile_dir="debug"
fi

# Built in the VM's own disk rather than in target/ on the shared mount: a
# Linux target dir on virtiofs is slow, and it would collide with the host's.
limactl shell --workdir / "$VM" bash -lc \
  "cd '$ROOT' && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo build -p bondsymphonic-daemon $profile_flag"

mkdir -p "$ROOT/target/daemon"
limactl shell --workdir / "$VM" bash -lc \
  "cp ~/.bondsymphonic/target/$profile_dir/bondsymphonic-daemon '$ROOT/target/daemon/bondsymphonic-daemon'"

echo "daemon built: target/daemon/bondsymphonic-daemon"
