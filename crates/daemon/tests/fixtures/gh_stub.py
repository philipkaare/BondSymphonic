#!/usr/bin/env python3
"""A stand-in for the `gh` CLI, pointed at by `BS_GH_BIN` in the daemon tests.

The real `gh` must never run in a test: it would talk to github.com with the
developer's own credentials. This records what it was asked to do in the file
named by `$GH_STUB_LOG` (one line, argv joined by spaces) and answers the way
`gh pr create` does. `GH_STUB_FAIL=1` turns it into the authentication failure
the daemon has to report back as a `GitError`.
"""

import os
import sys

args = sys.argv[1:]

log = os.environ.get("GH_STUB_LOG")
if log:
    with open(log, "a", encoding="utf-8") as f:
        f.write(" ".join(args) + "\n")

if os.environ.get("GH_STUB_FAIL") == "1":
    sys.stderr.write("not logged in\n")
    sys.exit(1)

if args[:2] == ["pr", "create"]:
    sys.stdout.write("https://github.com/example/repo/pull/42\n")

sys.exit(0)
