"""Deterministic ACP peer. All wire envelopes and approval choices are checked.

Frame shapes follow OpenCode 1.14.48 as recorded in opencode/probe-1.14.48:
text is `agent_message_chunk`, a tool call's input arrives after its first
`tool_call`, results are `{output, metadata}` or `{error}`, permission options
are `once`/`always`/`reject`, and a cancelled prompt ends with `end_turn`.
`replay:<scenario>` plays a recorded scenario instead."""
import json
import sys
import time
import os
from pathlib import Path

mode = sys.argv[1]
session = "new-session"
prompt = None
answered = []
selected_mode = "build"


def emit(value):
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)


def answer(request, result):
    emit({"id": request["id"], "result": result})


def update(value):
    emit({"method": "session/update", "params": {"sessionId": session, "update": value}})


def chunk(text):
    update({"sessionUpdate": "agent_message_chunk", "messageId": "msg_fake", "content": {"type": "text", "text": text}})


def tool(ident, title, raw_input):
    """A tool call as OpenCode starts one: no input at first, then the input."""
    update({"sessionUpdate": "tool_call", "toolCallId": ident, "title": title, "kind": "execute", "status": "pending", "locations": [], "rawInput": {}})
    update({"sessionUpdate": "tool_call_update", "toolCallId": ident, "status": "in_progress", "kind": "execute", "title": title, "locations": [], "rawInput": raw_input})


def complete():
    global prompt
    chunk("finished")
    answer(prompt, {"stopReason": "end_turn", "usage": {"totalTokens": 8, "inputTokens": 5, "outputTokens": 3}, "_meta": {}})
    prompt = None


def replay(scenario):
    """Answers each request with what OpenCode answered the same request in a
    recorded scenario. Where the recorded client sent a cancel or answered a
    permission, waits for ours; a prompt recorded without an answer (a provider
    retrying forever) hangs until cancelled. The scenario's WARN/ERROR log lines
    are written to stderr before its first prompt is answered."""
    base = Path(__file__).parent / "opencode" / "probe-1.14.48" / "frames"
    records = [json.loads(line) for line in (base / (scenario + ".jsonl")).read_text(encoding="utf-8").splitlines()]
    frames = [(r["dir"], r["frame"]) for r in records if r["dir"] in ("c->a", "a->c")]
    log = base / (scenario + ".stderr.txt")
    warnings, keep = [], False
    for line in log.read_text(encoding="utf-8").splitlines() if log.exists() else []:
        if line[:5] in ("ERROR", "WARN ", "INFO ", "DEBUG"):
            keep = line.startswith(("ERROR", "WARN"))
        if keep:
            warnings.append(line)
    used, backlog, hanging = set(), [], None

    def read():
        if backlog:
            return backlog.pop(0)
        line = sys.stdin.readline()
        if not line:
            sys.exit(0)
        return json.loads(line)

    def await_client(expected):
        while True:
            got = read()
            if expected.get("method") and got.get("method") == expected["method"]:
                return
            if "method" not in expected and "method" not in got and got.get("id") == expected.get("id"):
                return
            backlog.append(got)

    while True:
        request = read()
        method = request.get("method")
        if method == "session/cancel":
            if hanging is not None:
                answer({"id": hanging}, {"stopReason": "end_turn", "usage": {"totalTokens": 0, "inputTokens": 0, "outputTokens": 0}, "_meta": {}})
                hanging = None
            continue
        if method is None or "id" not in request:
            continue
        start = next((i for i, (d, f) in enumerate(frames) if i not in used and d == "c->a" and f.get("method") == method and "id" in f), None)
        if start is None:
            answer(request, {})
            continue
        used.add(start)
        if method == "session/prompt" and warnings:
            print("\n".join(warnings), file=sys.stderr, flush=True)
            warnings.clear()
            time.sleep(0.2)
        recorded, answered = frames[start][1]["id"], False
        for direction, frame in frames[start + 1:]:
            if direction == "a->c":
                if "method" in frame:
                    print(json.dumps(frame), flush=True)
                elif frame.get("id") == recorded:
                    print(json.dumps(dict(frame, id=request["id"])), flush=True)
                    answered = True
                    break
            elif "method" not in frame or "id" not in frame:
                await_client(frame)
        if not answered:
            hanging = request["id"]


if mode.startswith("replay:"):
    replay(mode.split(":", 1)[1])


for line in sys.stdin:
    request = json.loads(line)
    assert request["jsonrpc"] == "2.0"
    method = request.get("method")
    p = request.get("params", {})
    if method == "initialize":
        assert p["protocolVersion"] == 1
        assert p["clientCapabilities"]["fs"] == {"readTextFile": False, "writeTextFile": False}
        assert p["clientCapabilities"]["terminal"] is False
        if mode == "hang":
            time.sleep(60)
        if mode == "die":
            sys.exit(2)
        answer(request, {"protocolVersion": 2 if mode == "version" else 1,
                         "agentCapabilities": {"loadSession": mode != "no-resume", "sessionCapabilities": {"resume": {}} if mode == "resume-cap" else {}}})
    elif method in ("session/new", "session/load", "session/resume"):
        assert p["cwd"]
        if mode == "sandbox":
            assert p["mcpServers"] == [{"name": "bondsymphonic", "command": "/opt/bs/daemon", "args": ["mcp-bridge", "--socket", "/run/bs/mcp.sock"], "env": []}]
        else:
            assert p["mcpServers"] == []
        session = p.get("sessionId", "new-session")
        if method == "session/load":
            update({"sessionUpdate": "user_message_chunk", "messageId": "msg_old_user", "content": {"type": "text", "text": "old prompt"}})
            update({"sessionUpdate": "agent_message_chunk", "messageId": "msg_old", "content": {"type": "text", "text": "replayed"}})
        if mode == "commands":
            update({"sessionUpdate":"available_commands_update", "availableCommands":[{"name":"replay", "description":"During loading"}]})
        result = {"configOptions": [{"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "provider/default", "options": []}]} if mode != "legacy" else {"models": {"currentModelId": "provider/default"}}
        if method == "session/new":
            result["sessionId"] = session
        if mode == "resume-cap":
            assert method == "session/resume"
        answer(request, result)
    elif method == "session/set_config_option":
        assert p["configId"] == "model" and p["value"] == "provider/model"
        if mode == "commands":
            emit({"method":"session/update", "params":{"sessionId":"unrelated", "update":{"sessionUpdate":"available_commands_update", "availableCommands":[{"name":"wrong"}]}}})
            update({"sessionUpdate":"available_commands_update", "availableCommands":[{"name":"idle", "description":"While idle"}]})
        answer(request, {"configOptions": []})
    elif method == "session/set_model":
        assert mode == "legacy" and p["modelId"] == "provider/model"
        answer(request, {})
    elif method == "session/set_mode":
        selected_mode = p["modeId"]
        answer(request, {})
    elif method == "session/prompt":
        assert prompt is None, "client sent overlapping prompts"
        assert p["sessionId"] == session
        assert p["prompt"][0]["type"] == "text"
        prompt = request
        if mode.startswith("config-secret-"):
            config = Path(os.environ["XDG_CONFIG_HOME"]) / "opencode"
            old = config.joinpath("opencode.jsonc").read_bytes()
            assert b"https://example.test/provider//api" in old and b"/* value comment */" in old
            config.joinpath("opencode.jsonc").write_text('{/* refresh */ "provider":{"local":{"options":{"apiKey":"rotated-jsonc-key","clientSecret":"rotated-jsonc-client-secret",}}}}')
            secrets = ["legacy-config-token", "regular-config-password", "original-jsonc-key", "original-jsonc-client-secret", "rotated-jsonc-key", "rotated-jsonc-client-secret"]
            assert all(secret not in os.environ.values() for secret in secrets), "config credentials must not become environment values"
            text = "provider diagnostics " + " ".join(secrets)
            if mode == "config-secret-tool":
                update({"sessionUpdate":"tool_call_update","toolCallId":"provider-diagnostic","status":"completed","rawOutput":{"output":text,"metadata":{"output":text}}})
                complete()
            elif mode == "config-secret-error":
                emit({"id":request["id"],"error":{"code":-32000,"message":text}})
                prompt = None
            else:
                print(text,file=sys.stderr,flush=True)
                sys.exit(2)
        elif mode.startswith("native-"):
            native = Path(os.environ["XDG_DATA_HOME"]) / "opencode" / "auth.json"
            old = json.loads(native.read_text())
            old_values = [old["api"]["key"], old["oauth"]["access"], old["oauth"]["refresh"]]
            rotated = {"api": {"type": "api", "key": "rotated-native-key"}, "oauth": {"type": "oauth", "access": "rotated-native-access", "refresh": "rotated-native-refresh"}}
            native.write_text(json.dumps(rotated))
            values = old_values + [rotated["api"]["key"], rotated["oauth"]["access"], rotated["oauth"]["refresh"]]
            assert all(value not in os.environ.values() for value in values), "native credentials must not become process env"
            if mode == "native-tool":
                tool("auth-file", "bash", {"command": "cat auth.json"})
                update({"sessionUpdate": "tool_call_update", "toolCallId": "auth-file", "status": "completed", "rawOutput": {"output": json.dumps(rotated) + " old=" + " ".join(old_values), "metadata": {}}})
                # Tokens seen in nested JSON may precede the native auth-file write.
                nested = {"nested": {"key": "not-yet-written-key", "access": "not-yet-written-access", "refresh": "not-yet-written-refresh"}}
                update({"sessionUpdate": "tool_call_update", "toolCallId": "nested-auth", "status": "completed", "rawOutput": {"output": json.dumps(nested), "metadata": nested}})
                complete()
            elif mode == "native-error":
                emit({"id": request["id"], "error": {"code": -32000, "message": "auth_required " + " ".join(values)}})
                prompt = None
            else:
                print("native stderr " + " ".join(values), file=sys.stderr, flush=True)
                sys.exit(2)
        elif mode.startswith("permission-"):
            config = json.loads(os.environ["OPENCODE_CONFIG_CONTENT"])
            native = json.loads(Path("opencode.json").read_text())
            internal = config.get("default_agent")
            assert internal and internal != "build" and internal.startswith("bondsymphonic_ws_")
            assert selected_mode == internal, "resumed native build mode must be replaced"
            assert native["agent"]["build"]["permission"]["bash"] == "allow"
            assert native["model"] == "local/existing" and native["mcp"]["existing"]["enabled"]
            agent = config["agent"][internal]
            expected = "ask" if mode == "permission-ask" else "allow"
            assert agent["mode"] == "primary" and agent["permission"] == {"*": expected}
            chunk(expected + "-checked")
            complete()
        elif mode == "error":
            emit({"id": request["id"], "error": {"code": -32000, "message": "auth_required secret-value"}})
            prompt = None
        elif mode in ("approval", "queue") and not answered:
            options = [{"optionId": "once", "kind": "allow_once", "name": "Allow once"},
                       {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
                       {"optionId": "reject", "kind": "reject_once", "name": "Reject"}]
            # A peer request may reuse the prompt's own ID; it must not answer it.
            for ident in ("first", request["id"]):
                tool("call_" + str(ident), "bash", {"command": "echo " + str(ident)})
                emit({"id": ident, "method": "session/request_permission", "params": {
                    "sessionId": session, "toolCall": {"toolCallId": "call_" + str(ident), "status": "pending", "title": "bash", "rawInput": {}, "kind": "execute", "locations": []},
                    "options": options}})
        elif mode == "foreign":
            emit({"id": "foreign-permission", "method": "session/request_permission", "params": {"sessionId": "other-session", "toolCall": {}, "options": []}})
        elif mode == "unsupported":
            emit({"id": "host-fs", "method": "fs/write_text_file", "params": {"sessionId": session, "path": "HOST-PWNED", "content": "bad"}})
            emit({"id": "host-terminal", "method": "terminal/create", "params": {"sessionId": session, "command": "echo bad"}})
        elif mode == "bad-stderr":
            # More than a pipe holds, after bytes that are not UTF-8: a reader
            # that stops at them leaves this write blocked for ever.
            sys.stderr.buffer.write(b"\xff\xfe not utf-8\n" + b"x" * 300000 + b"\n")
            sys.stderr.flush()
            chunk("still here")
            complete()
        elif mode == "hold":
            chunk("waiting")
        elif mode == "usage":
            # OpenCode 1.14.48 reports cost 0 for a model it has no price for.
            priced = p["prompt"][0]["text"] == "priced"
            update({"sessionUpdate": "usage_update", "used": 1200 if priced else 1300, "size": 200000,
                    "cost": {"amount": 0.0123 if priced else 0, "currency": "USD"}})
            complete()
        else:
            tool("tool", "bash", {"command": "echo hi"})
            update({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed", "kind": "execute", "title": "Print hi", "rawInput": {"command": "echo hi"}, "rawOutput": {"output": "hi", "metadata": {"output": "hi", "exit": 0}}})
            chunk(p["prompt"][0]["text"])
            complete()
    elif method == "session/cancel":
        assert "id" not in request
        if prompt:
            # OpenCode keeps streaming briefly after a cancel, then reports an
            # ordinary end of turn.
            chunk(" late")
            answer(prompt, {"stopReason": "end_turn", "usage": {"totalTokens": 0, "inputTokens": 0, "outputTokens": 0}, "_meta": {}})
            prompt = None
    elif method is None:
        if mode == "foreign":
            assert request["id"] == "foreign-permission" and request["error"]["code"] == -32602
            complete()
        elif mode == "unsupported":
            assert request["id"] in ("host-fs", "host-terminal") and request["error"]["code"] == -32601
            answered.append(request["id"])
            if len(answered) == 2:
                complete()
        else:
            outcome = request["result"]["outcome"]
            assert outcome["outcome"] == "selected"
            expected = "always" if request["id"] == "first" else "reject"
            assert outcome["optionId"] == expected
            answered.append(request["id"])
            if len(answered) == 2:
                complete()
    else:
        raise AssertionError(method)

if mode == "stubborn":
    time.sleep(60)
