#!/usr/bin/env python3
"""A stand-in for the `gh` CLI, pointed at by `BS_GH_BIN` in the daemon tests.

The real `gh` must never run in a test: it would talk to github.com with the
developer's own credentials. This records what it was asked to do in the file
named by `$GH_STUB_LOG` (one line, argv joined by spaces) and answers the way
the `gh` subcommands the daemon uses do (`GH_STUB_PR_EXISTS`, `GH_STUB_RUNS`
and `GH_STUB_LOG_TEXT` shape those answers). `GH_STUB_FAIL=1` turns it into the authentication failure
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

# Canned answers by subcommand, so the agent tools have something to parse.
if args[:2] == ["pr", "create"]:
    if os.environ.get("GH_STUB_PR_EXISTS") == "1":
        sys.stderr.write('a pull request for branch "x" into branch "main" already exists:\nhttps://github.com/example/repo/pull/7\n')
        sys.exit(1)
    sys.stdout.write("https://github.com/example/repo/pull/42\n")
elif args[:2] == ["pr", "view"]:
    if "--jq" in args:
        sys.stdout.write("https://github.com/example/repo/pull/7\n")
    else:
        sys.stdout.write('{"number":42,"state":"OPEN","title":"T"}\n')
elif args[:2] == ["run", "list"]:
    sys.stdout.write(os.environ.get("GH_STUB_RUNS", '[{"databaseId":99,"status":"completed","conclusion":"failure","name":"ci","url":"u"}]') + "\n")
elif args[:2] == ["run", "view"]:
    if "--log-failed" in args:
        sys.stdout.write(os.environ.get("GH_STUB_LOG_TEXT", "step failed: boom\n"))
    else:
        sys.stdout.write('{"status":"completed","conclusion":"failure","name":"ci"}\n')
elif args[:2] == ["issue", "view"]:
    sys.stdout.write('{"number":5,"title":"Bug"}\n')

sys.exit(0)
