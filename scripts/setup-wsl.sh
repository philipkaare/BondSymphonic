#!/usr/bin/env bash
# Provisions the bondsymphonic WSL distro. Idempotent.
set -euo pipefail
USER_NAME="${1:-bs}"

export DEBIAN_FRONTEND=noninteractive
# This host's WSL NAT only has broken/blackholed IPv6 routes to some Ubuntu
# mirrors (e.g. security.ubuntu.com resolves AAAA-only over some DNS paths
# and IPv6 connect attempts hang for minutes); force apt to IPv4 so it fails
# fast instead of hanging.
echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4
apt-get update -y
apt-get install -y git bubblewrap build-essential curl ca-certificates pkg-config \
    python3 socat unzip gh

# Non-root user
if ! id "$USER_NAME" >/dev/null 2>&1; then
  useradd -m -s /bin/bash "$USER_NAME"
  echo "$USER_NAME ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/$USER_NAME
fi

# Make the user the default for wsl.exe -d bondsymphonic
cat > /etc/wsl.conf <<EOF
[user]
default=$USER_NAME
[boot]
systemd=false
EOF

# Ubuntu 24.04 restricts unprivileged user namespaces via AppArmor; bwrap needs them.
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = "1" ]; then
  echo "kernel.apparmor_restrict_unprivileged_userns=0" > /etc/sysctl.d/60-bondsymphonic.conf
  sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi

# Per-user tooling
sudo -u "$USER_NAME" -H bash <<'EOS'
set -euo pipefail
cd "$HOME"
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile default
fi
source "$HOME/.cargo/env"
rustup default stable
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
echo "setup-wsl: OK"
