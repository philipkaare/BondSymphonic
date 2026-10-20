#!/usr/bin/env bash
set -euo pipefail
# Must match PINNED_OPENCODE_VERSION in crates/daemon/src/agents/opencode_backend.rs.
pinned=1.14.48
# Keep the managed install on the same PATH as the other agent CLIs.
install_dir="$HOME/.local/bin"
# The daemon runs this at every start to hold the install at the pin, so an
# install already at the pin is left alone rather than downloaded again.
if [ -x "$install_dir/opencode" ] && [ "$("$install_dir/opencode" --version 2>/dev/null || true)" = "$pinned" ]; then
  echo "OpenCode $pinned is already installed"
  exit 0
fi
mkdir -p "$install_dir"
curl -fsSL https://opencode.ai/install | bash -s -- --version "$pinned" --no-modify-path
install -m 0755 "$HOME/.opencode/bin/opencode" "$install_dir/opencode"
"$install_dir/opencode" --version
