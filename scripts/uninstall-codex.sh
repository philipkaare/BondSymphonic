#!/usr/bin/env bash
# Removes the Codex install scripts/install-codex.sh made: the version
# directories under ~/.local/share/bondsymphonic/codex and the two links in
# ~/.local/bin that point into them. Invoked by the fixed UninstallCodex setup
# action. The sign-in, which lives in the daemon's own Codex home, is kept.
set -euo pipefail
share="$HOME/.local/share/bondsymphonic/codex"
bin="$HOME/.local/bin"
removed=0
for name in codex codex-code-mode-host; do
    target=$(readlink -- "$bin/$name" 2>/dev/null || true)
    case "$target" in
        "$share"/*)
            rm -f -- "$bin/$name"
            echo "Removed $bin/$name"
            removed=1
            ;;
        *)
            if [ -e "$bin/$name" ] || [ -L "$bin/$name" ]; then
                echo "Left $bin/$name alone: BondSymphonic's installer did not make it."
            fi
            ;;
    esac
done
if [ -d "$share" ]; then
    rm -rf -- "$share"
    echo "Removed $share"
    removed=1
fi
if [ "$removed" = 0 ]; then
    echo "No Codex install made by BondSymphonic was found; nothing was removed."
fi
echo "Your Codex sign-in was kept."
