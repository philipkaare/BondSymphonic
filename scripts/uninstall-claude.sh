#!/usr/bin/env bash
# Removes the Claude Code CLI the native installer put in this home -- the
# installer the InstallClaude setup action runs: the `claude` link in
# ~/.local/bin and the versions it points into under ~/.local/share/claude.
# Invoked by the fixed UninstallClaude setup action. The login, settings and
# sessions (~/.claude, ~/.claude.json) are never touched.
set -euo pipefail
share="$HOME/.local/share/claude"
bin="$HOME/.local/bin/claude"
removed=0
target=$(readlink -- "$bin" 2>/dev/null || true)
case "$target" in
    "$share"/*)
        rm -f -- "$bin"
        echo "Removed $bin"
        removed=1
        ;;
    *)
        # Not a link into the native install: somebody else's claude.
        if [ -e "$bin" ] || [ -L "$bin" ]; then
            echo "Left $bin alone: it is not the link the Claude Code installer made."
        fi
        ;;
esac
if [ -d "$share" ]; then
    rm -rf -- "$share"
    echo "Removed $share"
    removed=1
fi
if [ "$removed" = 0 ]; then
    echo "No Claude Code install made by BondSymphonic was found; nothing was removed."
fi
echo "Your Claude login and settings in ~/.claude were kept."
