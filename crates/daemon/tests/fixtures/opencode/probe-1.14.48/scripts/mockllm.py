"""Tiny OpenAI-compatible chat/completions mock (SSE streaming). Logs every request.
Behaviour: if request offers tools and no tool result yet -> one tool call (TOOL env, default bash);
otherwise -> text reply. A request whose prompt contains SLOWSTREAM streams slowly (for cancel)."""
import json, os, sys, time, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

port = int(sys.argv[1]); logp = sys.argv[2]
lk = threading.Lock()

def log(o):
    with lk, open(logp, "a") as f:
        f.write(json.dumps(o) + "\n")

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        log({"t": time.time(), "method": "GET", "path": self.path, "headers": dict(self.headers)})
        body = json.dumps({"object": "list", "data": [{"id": "m1", "object": "model"}]}).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); raw = self.rfile.read(n)
        try: req = json.loads(raw)
        except Exception: req = {}
        msgs = req.get("messages", [])
        hdrs = {k: (v if k.lower() not in ("authorization",) else "<REDACTED>") for k, v in self.headers.items()}
        log({"t": time.time(), "method": "POST", "path": self.path, "headers": hdrs, "requestline": self.requestline,
             "n_messages": len(msgs), "tools": [t.get("function", {}).get("name") for t in req.get("tools", [])],
             "last": msgs[-1] if msgs else None, "stream": req.get("stream")})
        if os.environ.get("MOCK_STATUS"):
            st = int(os.environ["MOCK_STATUS"]); body = json.dumps({"error": {"message": "Incorrect API key provided", "type": "invalid_request_error", "code": "invalid_api_key"}}).encode()
            self.send_response(st); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        lu = max([i for i, m in enumerate(msgs) if m.get("role") == "user"] or [0])
        all_text = json.dumps(msgs[lu:])
        has_tool_result = any(m.get("role") == "tool" for m in msgs[lu:])
        tool = os.environ.get("TOOL", "bash")
        if req.get("tools") and not has_tool_result and "NOTOOL" not in all_text:
            if tool == "bash":
                args = {"command": "ls -la", "description": "List files"}
            else:
                args = {"filePath": "/tmp/ocprobe/work/probe.txt", "content": "hello\n"}
            deltas = [{"role": "assistant", "content": None, "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": tool, "arguments": json.dumps(args)}}]}]
            finish = "tool_calls"
        else:
            words = ["DONE"] if "SLOWSTREAM" not in all_text else [f"{i} " for i in range(1, 60)]
            deltas = [{"role": "assistant", "content": w} for w in words]
            finish = "stop"
        if not req.get("stream"):
            d = deltas[0]
            body = json.dumps({"id": "x", "object": "chat.completion", "created": 0, "model": "m1",
                               "choices": [{"index": 0, "message": {"role": "assistant", "content": "".join(x.get("content") or "" for x in deltas), "tool_calls": d.get("tool_calls")}, "finish_reason": finish}],
                               "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}}).encode()
            self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        self.send_response(200); self.send_header("content-type", "text/event-stream"); self.send_header("connection", "close"); self.end_headers()
        try:
            for d in deltas:
                ch = {"id": "x", "object": "chat.completion.chunk", "created": 0, "model": "m1", "choices": [{"index": 0, "delta": d, "finish_reason": None}]}
                self.wfile.write(f"data: {json.dumps(ch)}\n\n".encode()); self.wfile.flush()
                if "SLOWSTREAM" in all_text: time.sleep(0.5)
            ch = {"id": "x", "object": "chat.completion.chunk", "created": 0, "model": "m1", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}}
            self.wfile.write(f"data: {json.dumps(ch)}\n\ndata: [DONE]\n\n".encode()); self.wfile.flush()
        except Exception as e:
            log({"note": f"client went away: {e!r}"})
        self.close_connection = True

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
