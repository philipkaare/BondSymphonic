#!/usr/bin/env bash
set -euo pipefail
# Keep the managed install on the same PATH as the other agent CLIs.
install_dir="$HOME/.local/bin"
mkdir -p "$install_dir"
curl -fsSL https://opencode.ai/install | bash -s -- --version 1.14.48 --no-modify-path
install -m 0755 "$HOME/.opencode/bin/opencode" "$install_dir/opencode"
"$install_dir/opencode" --version
