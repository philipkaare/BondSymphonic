#!/usr/bin/env python3
"""A stand-in for the Claude Code CLI that speaks its `stream-json` protocol.

It replays a recorded stream on stdout and reads the same line protocol on
stdin, so the daemon's adapter can be tested without a login, a network, or a
real model. It is deliberately dumb: it never parses the daemon's argv beyond
`--resume`, and it never validates what it is sent.

Which stream it replays comes from `FAKE_CLAUDE_FIXTURE`, or from
`fixture.ndjson` beside this script when that variable is not set -- which is
how it works inside a real sandbox, where the daemon's own environment does not
reach the process.

Behaviour:

* Fixture lines go out one every 20 ms, flushed, so the daemon sees a stream
  rather than one burst.
* A `control_request` line in the fixture is printed and then the replay stops
  until a `control_response` arrives on stdin. That is where the real CLI waits
  for a permission answer.
* After the fixture, every `user` line on stdin is answered with an assistant
  message echoing its text and a `result` line, so a turn can be driven from
  the test. `FAKE_CLAUDE_ECHO_DELAY` (seconds, default 0) holds that turn open
  for a while, which is how a test gets an agent that is genuinely busy.
* The one message it reads rather than echoes is `print-claude-json`, which is
  answered with the contents of `$HOME/.claude.json` -- the file the real CLI
  reads its per-project trust from. It is how a test sees the sandbox home as
  the agent sees it, rather than as the daemon meant to write it.
* An `interrupt` control request is acknowledged and ends the turn cleanly.
* SIGINT exits 130; end of input on stdin exits 0.
"""

import json
import os
import signal
import sys
import time

LINE_DELAY = 0.02


def echo_delay():
    """How long an echoed turn takes before it answers."""
    try:
        return float(os.environ.get("FAKE_CLAUDE_ECHO_DELAY", "0"))
    except ValueError:
        return 0.0


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def emit_raw(line):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def read_line():
    """One line from stdin, or None at end of input."""
    line = sys.stdin.readline()
    if line == "":
        return None
    return line.strip()


def wait_for_control_response():
    """Blocks until the host answers a permission request."""
    while True:
        line = read_line()
        if line is None:
            sys.exit(0)
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("type") == "control_response":
            return


def fixture_path():
    from_env = os.environ.get("FAKE_CLAUDE_FIXTURE")
    if from_env:
        return from_env
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixture.ndjson")


def resume_session(argv):
    for i, arg in enumerate(argv):
        if arg in ("--resume", "-r") and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith("--resume="):
            return arg.split("=", 1)[1]
    return None


def replay(path, resumed):
    """Streams the fixture, pausing where the real CLI would wait for us.

    Returns the session id the stream used, for the lines produced afterwards.
    """
    session = "sess-fake"
    with open(path, "r", encoding="utf-8") as f:
        lines = [ln.strip() for ln in f if ln.strip()]
    for line in lines:
        try:
            msg = json.loads(line)
        except ValueError:
            emit_raw(line)
            time.sleep(LINE_DELAY)
            continue
        # `--resume <id>` is echoed back as the session, the way the real CLI
        # continues a named session.
        if msg.get("type") == "system" and msg.get("subtype") == "init" and resumed:
            msg["session_id"] = resumed
        if msg.get("session_id"):
            session = msg["session_id"]
        emit(msg)
        if msg.get("type") == "control_request":
            wait_for_control_response()
        time.sleep(LINE_DELAY)
    return session


def echo_turn(session, text):
    time.sleep(echo_delay())
    emit(
        {
            "type": "assistant",
            "message": {
                "id": "m-echo",
                "role": "assistant",
                "content": [{"type": "text", "text": "echo: " + text}],
            },
            "session_id": session,
        }
    )
    time.sleep(LINE_DELAY)
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "duration_ms": 1,
            "num_turns": 1,
            "result": "echo: " + text,
            "session_id": session,
            "total_cost_usd": 0.0,
        }
    )


def interrupted_turn(session, request_id):
    """The turn ends, but not in error: the host asked for it."""
    emit(
        {
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id},
        }
    )
    time.sleep(LINE_DELAY)
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "duration_ms": 1,
            "num_turns": 0,
            "result": "",
            "session_id": session,
            "total_cost_usd": 0.0,
        }
    )


PRINT_CLAUDE_JSON = "print-claude-json"


def home_claude_json():
    """`$HOME/.claude.json` as the agent sees it, or why it cannot be read."""
    home = os.environ.get("HOME") or os.path.expanduser("~")
    path = os.path.join(home, ".claude.json")
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except OSError as e:
        return "cannot read %s: %s" % (path, e)


def user_text(msg):
    content = msg.get("message", {}).get("content", [])
    if isinstance(content, str):
        return content
    return "".join(
        block.get("text", "") for block in content if isinstance(block, dict)
    )


def main():
    if "--version" in sys.argv[1:]:
        print("0.0.0-fake (fake_claude.py)")
        return 0
    signal.signal(signal.SIGINT, lambda *_: sys.exit(130))
    session = replay(fixture_path(), resume_session(sys.argv[1:]))
    while True:
        line = read_line()
        if line is None:
            return 0
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        kind = msg.get("type")
        if kind == "user":
            text = user_text(msg)
            if text.strip() == PRINT_CLAUDE_JSON:
                text = home_claude_json()
            echo_turn(session, text)
        elif kind == "control_request":
            request = msg.get("request", {})
            if request.get("subtype") == "interrupt":
                interrupted_turn(session, msg.get("request_id", ""))
        # A `control_response` outside a pause is an answer to a request that
        # is no longer open; the real CLI ignores it too.


if __name__ == "__main__":
    sys.exit(main())
