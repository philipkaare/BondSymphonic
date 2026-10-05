#!/usr/bin/env bash
# Installs or updates Codex and its matching Code Mode helper to the latest
# release. Invoked by the fixed InstallCodex setup action and by the daemon's
# startup update (`agent_updates.rs`); does nothing when the latest release is
# already the one installed.
set -euo pipefail
if [ "$(uname -m)" != x86_64 ]; then
    echo 'This Codex package currently supports x86_64 WSL only.' >&2
    exit 1
fi

# GitHub redirects the latest-release page to the release's tag; prereleases
# are never "latest". The binary and its helper come from that one tag, so
# they always match.
latest=$(curl --fail --silent --show-error --location --head --proto '=https' --tlsv1.2 \
    --output /dev/null --write-out '%{url_effective}' \
    https://github.com/openai/codex/releases/latest)
version=${latest##*/rust-v}
if [[ ! "$version" =~ ^[0-9][0-9A-Za-z.+-]*$ ]]; then
    echo "Could not determine the latest Codex release from $latest" >&2
    exit 1
fi

share="$HOME/.local/share/bondsymphonic/codex"
destination="$share/$version"
bin="$HOME/.local/bin"
if [ -x "$destination/codex" ] && [ -x "$destination/codex-code-mode-host" ] &&
    [ "$(readlink "$bin/codex" || true)" = "$destination/codex" ] &&
    [ "$(readlink "$bin/codex-code-mode-host" || true)" = "$destination/codex-code-mode-host" ]; then
    echo "Codex $version is up to date."
    exit 0
fi

# The version the links point at now: sandboxes started from it bind its
# files, so it stays until the next update replaces it.
previous=$(dirname -- "$(readlink -f "$bin/codex" 2>/dev/null || echo /nonexistent/x)")

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

# Staged beside the destination and renamed into place, so an install that is
# interrupted never leaves a half-written version for the links to point at.
mkdir -p "$share" "$bin"
staging=$(mktemp -d "$share/.staging.XXXXXX")
chmod 755 "$staging"
for name in codex codex-code-mode-host; do
    install -m 755 "$scratch/$name-x86_64-unknown-linux-musl" "$staging/$name"
done
rm -rf -- "$destination"
mv -- "$staging" "$destination"
for name in codex codex-code-mode-host; do
    ln -sfn "$destination/$name" "$bin/$name"
done

# Keep only the new version and the one it replaced.
for old in "$share"/*/ "$share"/.staging.*/; do
    [ -d "$old" ] || continue
    old=${old%/}
    [ "$old" = "$destination" ] || [ "$old" = "$previous" ] || rm -rf -- "$old"
done
echo "Installed Codex $version and its matching Code Mode helper."
