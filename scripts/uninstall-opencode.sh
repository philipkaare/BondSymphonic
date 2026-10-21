#!/usr/bin/env bash
# Removes the OpenCode install scripts/install-opencode.sh made: its copy in
# ~/.local/bin, and the upstream installer's ~/.opencode/bin/opencode that the
# same run left behind -- but only while that one is still the identical file,
# since the daemon would otherwise go on finding it there. Invoked by the
# fixed UninstallOpencode setup action. Sign-in and configuration are kept.
set -euo pipefail
bin="$HOME/.local/bin/opencode"
upstream="$HOME/.opencode/bin/opencode"
removed=0
if [ -f "$bin" ] && [ ! -L "$bin" ]; then
    if [ -f "$upstream" ] && cmp -s -- "$bin" "$upstream"; then
        rm -f -- "$upstream"
        rmdir -- "$HOME/.opencode/bin" "$HOME/.opencode" 2>/dev/null || true
        echo "Removed $upstream"
    fi
    rm -f -- "$bin"
    echo "Removed $bin"
    removed=1
elif [ -e "$bin" ] || [ -L "$bin" ]; then
    echo "Left $bin alone: BondSymphonic's installer did not make it."
fi
if [ "$removed" = 0 ]; then
    echo "No OpenCode install made by BondSymphonic was found; nothing was removed."
fi
if [ -e "$upstream" ]; then
    echo "OpenCode's own install at $upstream was left alone, and is still used."
fi
echo "Your OpenCode sign-in and configuration were kept."
