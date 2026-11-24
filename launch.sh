#!/usr/bin/env bash
# One-command launch for BondSymphonic on macOS. The counterpart of launch.ps1:
# the daemon needs Linux (bubblewrap, user namespaces), so it runs in a Lima VM
# named `bondsymphonic` where Windows uses the WSL distro of that name.
#
#   ./launch.sh               build the daemon (release) and start the IDE
#   ./launch.sh --debug       debug daemon build
#   ./launch.sh --release     release IDE build
#   ./launch.sh --skip-daemon reuse the last daemon binary in target/daemon/
#   ./launch.sh --no-setup    fail instead of installing missing prerequisites
#
# Missing prerequisites come from Homebrew (rustup, Qt 6, CMake, Ninja, Lima);
# the VM is created and provisioned by scripts/setup-lima.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
VM="bondsymphonic"

debug=0 release=0 skip_daemon=0 no_setup=0
for arg in "$@"; do
  case "$arg" in
    --debug) debug=1 ;;
    --release) release=1 ;;
    --skip-daemon) skip_daemon=1 ;;
    --no-setup) no_setup=1 ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "launch.sh: unknown option '$arg' (see --help)" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "launch.sh is for macOS; use launch.ps1 on Windows" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# ---- 1. Environment -------------------------------------------------------------
[ -x /opt/homebrew/bin/brew ] && eval "$(/opt/homebrew/bin/brew shellenv)"
[ -x /usr/local/bin/brew ] && eval "$(/usr/local/bin/brew shellenv)"
export PATH="$HOME/.cargo/bin:$PATH"
if have brew; then
  # rustup is keg-only in Homebrew so it does not shadow a system cargo.
  export PATH="$(brew --prefix)/opt/rustup/bin:$PATH"
  export QMAKE="${QMAKE:-$(brew --prefix)/opt/qt/bin/qmake}"
fi

# ---- 2. macOS toolchain ---------------------------------------------------------
missing=()
have brew || { echo "Homebrew is required: https://brew.sh" >&2; exit 1; }
have cargo || missing+=(rustup)
[ -x "${QMAKE:-}" ] || missing+=(qt)
have cmake || missing+=(cmake)
have ninja || missing+=(ninja)
have limactl || missing+=(lima)
if [ ${#missing[@]} -gt 0 ]; then
  echo "macOS toolchain incomplete: ${missing[*]}"
  [ "$no_setup" = 1 ] && { echo "Run: brew install ${missing[*]}" >&2; exit 1; }
  brew install "${missing[@]}"
fi
if have rustup && ! rustup default >/dev/null 2>&1; then
  rustup default stable
fi
xcode-select -p >/dev/null 2>&1 || {
  echo "The Xcode command line tools are required: run 'xcode-select --install' and retry" >&2
  exit 1
}

# ---- 3. Lima VM -----------------------------------------------------------------
vm_ready() {
  [ "$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)" = "Running" ] &&
    limactl shell --workdir / "$VM" bash -lc \
      "command -v cargo >/dev/null && command -v bwrap >/dev/null && command -v git >/dev/null" >/dev/null 2>&1
}
if ! vm_ready; then
  status="$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null || true)"
  if [ "$status" = "Stopped" ]; then
    echo "Starting VM '$VM' ..."
    limactl start --tty=false "$VM"
  fi
  if ! vm_ready; then
    echo "Lima VM '$VM' missing or incomplete."
    [ "$no_setup" = 1 ] && { echo "Run scripts/setup-lima.sh, then retry." >&2; exit 1; }
    echo "Running scripts/setup-lima.sh (this takes several minutes the first time) ..."
    "$ROOT/scripts/setup-lima.sh" dev
    vm_ready || { echo "VM '$VM' still unhealthy after setup." >&2; exit 1; }
  fi
fi

# ---- 4. Daemon ------------------------------------------------------------------
if [ "$skip_daemon" = 0 ]; then
  if [ "$debug" = 1 ]; then "$ROOT/scripts/build-daemon.sh" --debug; else "$ROOT/scripts/build-daemon.sh"; fi
elif [ ! -f "$ROOT/target/daemon/bondsymphonic-daemon" ]; then
  echo "--skip-daemon given but target/daemon/bondsymphonic-daemon does not exist. Run once without it." >&2
  exit 1
fi

# ---- 5. IDE ---------------------------------------------------------------------
cd "$ROOT"
cargo_args=(run -p bondsymphonic-ide)
[ "$release" = 1 ] && cargo_args+=(--release)
exec cargo "${cargo_args[@]}"
