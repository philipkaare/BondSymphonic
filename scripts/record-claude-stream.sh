#!/usr/bin/env bash
# Record one real Claude Code stream-json turn into a parser fixture.
#
# The fixtures under crates/daemon/tests/fixtures/claude-stream/ are synthetic:
# they were written from the documented shapes, and every pinned flag was
# verified against the installed binary, but no line in them came out of a real
# `claude`. This script closes that gap. Run it once, on a machine that is
# logged in to Claude Code, and compare what it records against the synthetic
# fixtures; where a shape differs, the recording is right and the fixture is
# wrong.
#
#   bash scripts/record-claude-stream.sh ["a one-line prompt"]
#
# It runs inside the WSL distro (or any Linux host with `claude` on PATH), never
# on Windows. It creates a throwaway git repository in a temporary directory,
# runs `claude` there with exactly the argv the daemon's adapter uses, and saves
# stdout verbatim to
#
#   crates/daemon/tests/fixtures/claude-stream/recorded-<version>.ndjson
#
# What it does NOT do: log you in. `claude auth login` is interactive and writes
# credentials to your home directory; if you are not logged in this script says
# so and stops, rather than starting a login on your behalf.
#
# Run it from a *login* shell (`wsl -d bondsymphonic -- bash -lc '...'`). WSL
# appends the Windows PATH to a non-login shell, so a plain `bash -c` can
# resolve `claude` to a Windows npm install of another version, which rejects
# flags the Linux one accepts. This script prefers ~/.local/bin/claude, the way
# the daemon does, and refuses a Windows path outright.
#
# What to check in the recording, because these are the shapes the parser
# (crates/daemon/src/agents/claude_stream.rs) is built around:
#
#   * the `system`/`init` line and the name of its session field;
#   * `stream_event` partial-message deltas, if the account has them;
#   * an `assistant` message whose content holds a `tool_use` block;
#   * a `user` message whose content holds a `tool_result` block;
#   * the `result` line's cost, duration and turn-count field names;
#   * for a permission turn, the `control_request`/`can_use_tool` line and the
#     `control_response` shape it expects back.
#
# A prompt that only answers a question will not produce the last three. Ask for
# something that reads a file to get a tool call, and run with a permission mode
# that actually prompts to get a `control_request`.

set -euo pipefail

PROMPT="${1:-List the files in this directory, then stop.}"

# The version crates/daemon/src/agents/claude.rs pins its argv to.
TESTED_VERSION="2.1.263"

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
FIXTURES="$ROOT/crates/daemon/tests/fixtures/claude-stream"

# The daemon puts $HOME/.local/bin first, so do the same rather than trusting
# whatever PATH this shell happens to carry.
if [ -x "$HOME/.local/bin/claude" ]; then
    CLAUDE="$HOME/.local/bin/claude"
elif command -v claude >/dev/null 2>&1; then
    CLAUDE="$(command -v claude)"
else
    echo "record-claude-stream: no 'claude' on PATH." >&2
    echo "  The daemon reports this as the 'claude' item of system.check_prereqs." >&2
    exit 1
fi

case "$CLAUDE" in
/mnt/*)
    echo "record-claude-stream: '$CLAUDE' is a Windows install seen through WSL." >&2
    echo "  Install Claude Code inside the distro, or run this from a login shell." >&2
    exit 1
    ;;
esac

VERSION="$("$CLAUDE" --version 2>/dev/null | tr -cd '0-9.' || true)"
if [ -z "$VERSION" ]; then
    echo "record-claude-stream: '$CLAUDE --version' printed no version." >&2
    exit 1
fi
if [ "$VERSION" != "$TESTED_VERSION" ]; then
    echo "record-claude-stream: note - $CLAUDE is $VERSION, the adapter pins" >&2
    echo "  $TESTED_VERSION. Recording anyway; the file is named for $VERSION." >&2
fi

# Not logged in is the common case on a fresh machine, and it produces a
# useless recording (one error line), so stop here instead.
if ! "$CLAUDE" auth status >/dev/null 2>&1; then
    echo "record-claude-stream: not logged in to Claude Code." >&2
    echo "  Log in from the IDE's setup page (Help > Setup...), then run this again." >&2
    echo "  This script will not start a login itself." >&2
    exit 1
fi

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# A throwaway repository, so the recording never carries anything out of a real
# working tree and the agent has something small to look at.
git -C "$WORK" init -q
printf 'hello\n' >"$WORK/README.md"
printf 'fn main() {}\n' >"$WORK/main.rs"
git -C "$WORK" add -A
git -C "$WORK" -c user.name=recorder -c user.email=recorder@example.invalid \
    commit -qm "fixture repository"

mkdir -p "$FIXTURES"
OUT="$FIXTURES/recorded-$VERSION.ndjson"

# `--input-format stream-json` means stdin is NDJSON, not prose: one line in
# exactly the shape `claude_stream::user_line` builds. python3 does the quoting,
# so a prompt with a quote or a backslash in it still produces valid JSON.
USER_LINE="$(PROMPT="$PROMPT" python3 -c '
import json, os
print(json.dumps({"type": "user", "message": {"role": "user",
                  "content": [{"type": "text", "text": os.environ["PROMPT"]}]}}))
')"

# In the throwaway repository, not in this one.
cd "$WORK"

# Exactly the base argv of `claude_argv` in crates/daemon/src/agents/claude.rs.
# Keep the two in step: a flag that is here and not there, or the other way
# round, makes the recording describe a stream the daemon never sees.
STATUS=0
printf '%s\n' "$USER_LINE" | "$CLAUDE" \
    -p \
    --input-format stream-json \
    --output-format stream-json \
    --verbose \
    --include-partial-messages \
    --permission-prompts host \
    >"$OUT" 2>"$WORK/stderr.txt" || STATUS=$?

# A rejected flag or a failed turn writes nothing useful, and an empty fixture
# beside the real ones is worse than none at all.
if [ "$STATUS" -ne 0 ] || [ ! -s "$OUT" ]; then
    rm -f "$OUT"
    echo "record-claude-stream: '$CLAUDE' exited $STATUS and recorded nothing." >&2
    echo "  Its stderr:" >&2
    sed 's/^/    /' "$WORK/stderr.txt" >&2 || true
    echo "  If a flag was rejected, the adapter's argv needs updating for $VERSION." >&2
    exit 1
fi

LINES="$(wc -l <"$OUT" | tr -d ' ')"
echo "record-claude-stream: wrote $LINES lines to $OUT"
echo "record-claude-stream: message types seen:"
grep -o '"type":"[a-z_]*"' "$OUT" | sort | uniq -c || true
echo
echo "Next: compare these against the synthetic fixtures beside it and correct"
echo "any shape that differs, then re-run the daemon's claude_stream tests."
