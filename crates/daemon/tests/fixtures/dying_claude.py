#!/usr/bin/env python3
"""A stand-in for the Claude Code CLI that says its piece and dies.

`fake_claude.py` is the one that holds a conversation open; this one exists for
the adapter's *exit* paths, which need a process that really goes away while the
daemon is still reading its pipes:

* `FAKE_CLAUDE_FIXTURE` (optional): an NDJSON stream to replay on stdout first,
  one line at a time, flushed. An `is_error` result in it is how a turn that
  failed is followed by an exit.
* `DYING_CLAUDE_STDERR` (optional): the text to write to stderr.
* `DYING_CLAUDE_STDERR_FROM_CHILD` (optional): when set, that text is written by
  a *child* process that inherits this one's stderr, and this process exits
  without waiting for it. That is the ordering the adapter's exit detail has to
  survive, and the only one that is not a coin toss: the daemon has the exit
  code in hand while the agent's last words are still on their way. It is not a
  contrivance either -- the CLI hands its stderr to every tool it runs, so a
  shell command outliving the CLI by a few milliseconds is the ordinary case.
  Without it the text is written by this process just before it exits, which the
  daemon usually, but only usually, reads in time.

  This process waits for the child to say it is up before exiting, so the gap
  the daemon has to cover is the delay below and nothing else. Starting a Python
  interpreter is not part of it: from a Windows drive that takes anything up to
  a second, and the adapter's wait is deliberately bounded.
* `DYING_CLAUDE_STDERR_DELAY` (seconds, default 0.1): how long to wait after
  that before writing the stderr, so the daemon is reliably already waiting on
  the exit.
* `DYING_CLAUDE_EXIT` (default 0): the exit status.

`--version` is answered and nothing else happens, because the daemon probes the
program it is about to run before it runs it.

The exit is `os._exit`, not `sys.exit`: the point is a process that stops
without flushing anything else or running an interpreter shutdown, which is what
a CLI dying on a bad flag or a missing login looks like.
"""

import os
import subprocess
import sys
import time

LINE_DELAY = 0.02

#: argv marker for the child half of `DYING_CLAUDE_STDERR_FROM_CHILD`.
WRITER = "--stderr-writer"


def replay(path):
    with open(path, "r", encoding="utf-8") as f:
        lines = [ln.strip() for ln in f if ln.strip()]
    for line in lines:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()
        time.sleep(LINE_DELAY)


def float_env(name, default):
    try:
        return float(os.environ.get(name, default))
    except ValueError:
        return float(default)


def stderr_text():
    text = os.environ.get("DYING_CLAUDE_STDERR", "")
    if not text:
        return ""
    return text if text.endswith("\n") else text + "\n"


#: what the child says on its stdout once it is up, so the parent can stop
#: waiting. The parent's exit is the event under test, and it must not be
#: separated from the stderr by an interpreter start.
READY = "ready"


def write_stderr_after_the_delay():
    """The child half: says it is up, sleeps, writes on the inherited stderr."""
    sys.stdout.write(READY + "\n")
    sys.stdout.flush()
    time.sleep(float_env("DYING_CLAUDE_STDERR_DELAY", "0.1"))
    text = stderr_text()
    if text:
        sys.stderr.write(text)
        sys.stderr.flush()
    return 0


def main():
    if WRITER in sys.argv[1:]:
        return write_stderr_after_the_delay()
    if "--version" in sys.argv[1:]:
        print("0.0.0-dying (dying_claude.py)")
        return 0
    fixture = os.environ.get("FAKE_CLAUDE_FIXTURE")
    if fixture:
        replay(fixture)
    text = stderr_text()
    if text and os.environ.get("DYING_CLAUDE_STDERR_FROM_CHILD"):
        # Started and abandoned: stderr is fd 2, which the child inherits, and
        # this process is gone before the child writes to it.
        child = subprocess.Popen(
            [sys.executable, os.path.abspath(__file__), WRITER],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=2,
        )
        # Blocks until the child is running, so what stands between this exit
        # and the agent's last words is the delay and nothing else. An empty
        # read means the child could not start at all, and then there is
        # nothing to wait for.
        child.stdout.readline()
        sys.stdout.close()
    else:
        sys.stdout.close()
        if text:
            time.sleep(float_env("DYING_CLAUDE_STDERR_DELAY", "0.1"))
            sys.stderr.write(text)
            sys.stderr.flush()
    os._exit(int(os.environ.get("DYING_CLAUDE_EXIT", "0")))


if __name__ == "__main__":
    sys.exit(main())
