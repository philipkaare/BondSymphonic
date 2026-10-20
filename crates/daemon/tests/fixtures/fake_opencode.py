"""Deterministic ACP peer. All wire envelopes and approval choices are checked."""
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


def complete():
    global prompt
    update({"sessionUpdate": "assistant_message_chunk", "content": {"type": "text", "text": "finished"}})
    answer(prompt, {"stopReason": "end_turn", "usage": {"inputTokens": 5, "outputTokens": 3}})
    prompt = None


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
            update({"sessionUpdate": "assistant_message_chunk", "content": {"type": "text", "text": "replayed"}})
        result = {"configOptions": [{"id": "model", "category": "model"}]} if mode != "legacy" else {"models": {"currentModelId": "provider/default"}}
        if method == "session/new":
            result["sessionId"] = session
        if mode == "resume-cap":
            assert method == "session/resume"
        answer(request, result)
    elif method == "session/set_config_option":
        assert p["configId"] == "model" and p["value"] == "provider/model"
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
                update({"sessionUpdate":"tool_call_update","toolCallId":"provider-diagnostic","status":"completed","rawOutput":text})
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
                update({"sessionUpdate": "tool_call", "toolCallId": "auth-file", "title": "cat auth.json", "rawInput": {}})
                update({"sessionUpdate": "tool_call_update", "toolCallId": "auth-file", "status": "completed", "rawOutput": json.dumps(rotated) + " old=" + " ".join(old_values)})
                # Tokens seen in nested JSON may precede the native auth-file write.
                update({"sessionUpdate": "tool_call_update", "toolCallId": "nested-auth", "status": "completed", "rawOutput": {"nested": {"key": "not-yet-written-key", "access": "not-yet-written-access", "refresh": "not-yet-written-refresh"}}})
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
            assert agent["mode"] == "primary" and agent["permission"] == expected
            update({"sessionUpdate": "assistant_message_chunk", "content": {"type": "text", "text": expected + "-checked"}})
            complete()
        elif mode == "error":
            emit({"id": request["id"], "error": {"code": -32000, "message": "auth_required secret-value"}})
            prompt = None
        elif mode in ("approval", "queue") and not answered:
            options = [{"optionId": "once-choice", "kind": "allow_once"},
                       {"optionId": "session-choice", "kind": "allow_always"},
                       {"optionId": "deny-choice", "kind": "reject_once"}]
            for ident in ("first", request["id"]):
                emit({"id": ident, "method": "session/request_permission", "params": {
                    "sessionId": session, "toolCall": {"toolCallId": str(ident), "title": "shell", "rawInput": {"command": "echo hi"}},
                    "options": options}})
        elif mode == "foreign":
            emit({"id": "foreign-permission", "method": "session/request_permission", "params": {"sessionId": "other-session", "toolCall": {}, "options": []}})
        elif mode == "unsupported":
            emit({"id": "host-fs", "method": "fs/write_text_file", "params": {"sessionId": session, "path": "HOST-PWNED", "content": "bad"}})
            emit({"id": "host-terminal", "method": "terminal/create", "params": {"sessionId": session, "command": "echo bad"}})
        elif mode == "hold":
            update({"sessionUpdate": "assistant_message_chunk", "content": {"type": "text", "text": "waiting"}})
        else:
            update({"sessionUpdate": "tool_call", "toolCallId": "tool", "title": "shell", "kind": "execute", "status": "in_progress", "rawInput": {"command": "echo hi"}})
            update({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "completed", "rawOutput": "hi"})
            update({"sessionUpdate": "assistant_message_chunk", "content": {"type": "text", "text": p["prompt"][0]["text"]}})
            complete()
    elif method == "session/cancel":
        assert "id" not in request
        if prompt:
            answer(prompt, {"stopReason": "cancelled"})
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
            expected = "session-choice" if request["id"] == "first" else "deny-choice"
            assert outcome["optionId"] == expected
            answered.append(request["id"])
            if len(answered) == 2:
                complete()
    else:
        raise AssertionError(method)

if mode == "stubborn":
    time.sleep(60)
