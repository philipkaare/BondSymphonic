#!/usr/bin/env bash
# Provisions the bondsymphonic WSL distro. Idempotent.
#
#   setup-wsl.sh [user] [dev|runtime]
#
# `dev` (the default) also installs a Rust toolchain and the C toolchain cargo
# needs, because a developer builds the daemon inside this distro. `runtime`
# installs only what running the daemon needs: an end user gets the daemon as a
# binary in the package and never compiles anything.
set -euo pipefail
USER_NAME="${1:-bs}"
MODE="${2:-dev}"
case "$MODE" in
  dev|runtime) ;;
  *) echo "setup-wsl: unknown mode '$MODE' (expected dev or runtime)" >&2; exit 2 ;;
esac
echo "setup-wsl: user '$USER_NAME', mode '$MODE'"

export DEBIAN_FRONTEND=noninteractive
# This host's WSL NAT only has broken/blackholed IPv6 routes to some Ubuntu
# mirrors (e.g. security.ubuntu.com resolves AAAA-only over some DNS paths
# and IPv6 connect attempts hang for minutes); force apt to IPv4 so it fails
# fast instead of hanging.
echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4
apt-get update -y
# What the daemon needs at run time: git for worktrees, bubblewrap for the
# sandbox, python3 for the run configurations the IDE detects, gh for pull
# requests, and curl/ca-certificates/unzip for the Claude Code installer.
apt-get install -y git bubblewrap curl ca-certificates python3 socat unzip gh
if [ "$MODE" = "dev" ]; then
  # Only a build needs a C toolchain and pkg-config; cargo links with them.
  apt-get install -y build-essential pkg-config
fi

# Non-root user
if ! id "$USER_NAME" >/dev/null 2>&1; then
  useradd -m -s /bin/bash "$USER_NAME"
  echo "$USER_NAME ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/$USER_NAME
fi

# Make the user the default for wsl.exe -d bondsymphonic.
#
# The boot command lowers eth0's MTU from 1500. Behind a VPN on the Windows
# side, full-size packets into the WSL NAT are dropped without an ICMP reply,
# so connections open and then stall at the first large reply -- GitHub's TLS
# handshake, so every agent's `git fetch` timed out while Windows was fine.
# 1400 leaves room for the VPN's encapsulation and costs nothing without one.
# `|| true`: in mirrored networking the interface may have another name.
cat > /etc/wsl.conf <<EOF
[user]
default=$USER_NAME
[boot]
systemd=false
command=ip link set dev eth0 mtu 1400 || true
EOF

# Ubuntu 24.04 restricts unprivileged user namespaces via AppArmor; bwrap needs them.
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = "1" ]; then
  echo "kernel.apparmor_restrict_unprivileged_userns=0" > /etc/sysctl.d/60-bondsymphonic.conf
  sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi

# Per-user tooling. The mode arrives as a positional argument rather than
# interpolated: the heredoc is quoted so the outer shell expands nothing inside
# it, which is what keeps `$HOME` and `$PATH` the inner user's.
sudo -u "$USER_NAME" -H bash -s -- "$MODE" <<'EOS'
set -euo pipefail
MODE="$1"
cd "$HOME"
if [ "$MODE" = "dev" ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile default
  fi
  source "$HOME/.cargo/env"
  rustup default stable
fi
if ! command -v claude >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/claude" ]; then
  curl -fsSL https://claude.ai/install.sh | bash
fi
mkdir -p "$HOME/.bondsymphonic/bin"
grep -q '.local/bin' "$HOME/.bashrc" || echo 'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"' >> "$HOME/.bashrc"
# Verify sandboxing works
if bwrap --ro-bind / / --unshare-all --die-with-parent true; then
  echo "bwrap: OK"
else
  echo "bwrap: FAILED (user namespaces unavailable)"; exit 1
fi
EOS
echo "setup-wsl: OK ($MODE)"
