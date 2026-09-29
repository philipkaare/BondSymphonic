"""Deterministic app-server peer; asserts handshake and raw approval IDs."""
import json
import sys
import time

if "--version" in sys.argv:
    print("codex-cli 0.158.0")
    sys.exit(0)

mode = sys.argv[1] if len(sys.argv) > 1 else "normal"
initialized = False
thread = "new-thread"
turn = 0


def emit(value):
    print(json.dumps(value), flush=True)


def event(method, params):
    emit({"method": method, "params": params})


for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    p = m.get("params", {})
    if method == "initialize":
        if mode == "hang":
            time.sleep(60)
        if mode == "die":
            sys.exit(2)
        emit({"id": m["id"], "result": {"userAgent": "fake"}})
    elif method == "initialized":
        initialized = True
    elif method == "model/list":
        assert initialized
        if mode == "models-error":
            emit({"id":m["id"],"error":{"message":"model discovery failed"}})
        else:
            ident = "other-account" if mode == "models-other" else "second" if p.get("cursor") else "first"
            emit({"id":m["id"],"result":{"data":[] if mode == "models-empty" else [{"id":ident,"displayName":ident}],"nextCursor":"page2" if mode == "models" and not p.get("cursor") else None}})
    elif method in ("thread/start", "thread/resume"):
        assert initialized
        thread = p.get("threadId", "new-thread")
        emit({"id": m["id"], "result": {"thread": {"id": thread}}})
    elif method == "turn/start":
        turn += 1
        emit({"id": m["id"], "result": {"turn": {"id": str(turn)}}})
        event("turn/started", {"threadId": thread, "turn": {"id": str(turn)}})
        if mode in ("approval", "approvaldie"):
            emit({"id": "server-approval", "method": "item/commandExecution/requestApproval",
                  "params": {"threadId": thread, "turnId": str(turn), "itemId": "cmd", "command": "echo test"}})
            if mode == "approvaldie":
                sys.exit(2)
    elif method == "turn/steer":
        assert p["expectedTurnId"] == str(turn)
        emit({"id": m["id"], "result": {"turnId": str(turn)}})
        event("item/completed", {"item": {"type": "agentMessage", "id": "text", "text": "steered"}})
    elif method == "turn/interrupt":
        assert p["turnId"] == str(turn)
        emit({"id": m["id"], "result": {}})
        event("turn/completed", {"threadId": thread, "turn": {"id": str(turn), "status": "interrupted"}})
    elif method is None:
        assert m["id"] == "server-approval"
        assert m["result"]["decision"] in ("accept", "decline", "acceptForSession")
        event("turn/completed", {"threadId": thread, "turn": {"id": str(turn), "status": "completed"}})
    else:
        raise AssertionError(method)

if mode == "stubborn":
    time.sleep(60)
