#!/usr/bin/env bash
# Invoked by the fixed InstallCodex setup action. Install the verified pair.
set -euo pipefail
if [ "$(uname -m)" != x86_64 ]; then
    echo 'This Codex package currently supports x86_64 WSL only.' >&2
    exit 1
fi
version=0.158.0
destination="$HOME/.local/share/bondsymphonic/codex/$version"
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
base="https://github.com/openai/codex/releases/download/rust-v$version"
for name in codex codex-code-mode-host; do
    asset="$name-x86_64-unknown-linux-musl"
    curl --fail --location --proto '=https' --tlsv1.2 "$base/$asset.tar.gz" -o "$scratch/$name.tar.gz"
    tar -xzf "$scratch/$name.tar.gz" -C "$scratch" "$asset"
    chmod 755 "$scratch/$asset"
done
"$scratch/codex-x86_64-unknown-linux-musl" --version
mkdir -p "$destination" "$HOME/.local/bin"
for name in codex codex-code-mode-host; do
    install -m 755 "$scratch/$name-x86_64-unknown-linux-musl" "$destination/$name"
    ln -sfn "$destination/$name" "$HOME/.local/bin/$name"
done
echo "Installed Codex $version and its matching Code Mode helper."
