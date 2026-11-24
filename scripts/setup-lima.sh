#!/usr/bin/env bash
# Creates and provisions the `bondsymphonic` Lima VM, the macOS stand-in for the
# WSL distro. Idempotent.
#
#   scripts/setup-lima.sh [dev|runtime]
#
# The VM is Ubuntu 24.04 with the host home directory mounted, writable, at the
# same path, so a macOS path is also a path inside the VM and the daemon built
# there can be copied straight out into target/daemon/. Provisioning is
# scripts/setup-wsl.sh, run as root for the VM's own user (Lima names it after
# the macOS user), which installs git, bubblewrap, gh, Claude Code and, in `dev`
# mode, rustup.
set -euo pipefail
MODE="${1:-dev}"
VM="bondsymphonic"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

command -v limactl >/dev/null || { echo "setup-lima: limactl not found (brew install lima)" >&2; exit 1; }

case "$ROOT" in
  "$HOME"/*) ;;
  *) echo "setup-lima: the checkout must live under $HOME, the only folder the VM mounts" >&2; exit 1 ;;
esac

status="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null || true)"
if [ -z "$status" ]; then
  echo "setup-lima: creating VM '$VM' (Ubuntu 24.04) ..."
  limactl create --name="$VM" --tty=false --mount-writable template://ubuntu-24.04
  status="Stopped"
fi
if [ "$status" != "Running" ]; then
  echo "setup-lima: starting VM '$VM' ..."
  limactl start --tty=false "$VM"
fi

vm_user="$(limactl shell --workdir / "$VM" whoami)"
limactl shell --workdir / "$VM" sudo bash "$ROOT/scripts/setup-wsl.sh" "$vm_user" "$MODE"
echo "setup-lima: OK ($MODE)"
