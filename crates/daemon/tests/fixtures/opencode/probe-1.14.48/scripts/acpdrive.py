"""Minimal ACP (JSON-RPC 2.0 NDJSON over stdio) driver that records every frame.

usage: acpdrive.py <frames.jsonl> <scenario.json> -- <argv...>
scenario: list of steps:
  {"call": method, "params": {...}, "timeout": s, "save": "name"}   request, wait for response
  {"send": method, "params": {...}, "after": s}                     request, do not wait (id saved)
  {"notify": method, "params": {...}, "delay": s}                   notification
  {"wait": s}                                                       idle and collect
  {"await": "name", "timeout": s}                                   wait for a previously sent request
  {"close_stdin": true}
Strings "$name.path.to.field" in params are substituted from saved results.
Peer requests: session/request_permission -> reply per env PERM_REPLY (cancel|allow_once|...);
other methods -> -32601.
"""
import json, os, subprocess, sys, threading, time, queue, re

out_path, scen_path = sys.argv[1], sys.argv[2]
argv = sys.argv[sys.argv.index("--") + 1:]
steps = json.load(open(scen_path))
t0 = time.time()
log = open(out_path, "w")
lock = threading.Lock()
SECRET_RE = re.compile(r'(sk-[A-Za-z0-9_-]{8,}|"(apiKey|key|access|refresh|token)"\s*:\s*"[^"]+")')

def rec(direction, obj):
    with lock:
        line = json.dumps({"t": round(time.time() - t0, 3), "dir": direction, "frame": obj})
        line = SECRET_RE.sub("<REDACTED>", line)
        log.write(line + "\n"); log.flush()

proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
responses = {}
cond = threading.Condition()
perm_reply = os.environ.get("PERM_REPLY", "cancel")

def write(obj):
    rec("c->a", obj)
    proc.stdin.write((json.dumps(obj) + "\n").encode()); proc.stdin.flush()

def reader():
    for raw in proc.stdout:
        s = raw.decode(errors="replace").rstrip("\n")
        if not s:
            continue
        try:
            obj = json.loads(s)
        except Exception:
            rec("a->c NONJSON", s); continue
        rec("a->c", obj)
        if "method" in obj and "id" in obj:
            m = obj["method"]
            if m == "session/request_permission":
                if perm_reply == "cancel":
                    res = {"outcome": {"outcome": "cancelled"}}
                else:
                    opt = next((o["optionId"] for o in obj["params"].get("options", []) if o.get("kind") == perm_reply), None)
                    res = {"outcome": {"outcome": "selected", "optionId": opt}}
                write({"jsonrpc": "2.0", "id": obj["id"], "result": res})
            else:
                write({"jsonrpc": "2.0", "id": obj["id"], "error": {"code": -32601, "message": "unsupported by probe"}})
        elif "id" in obj:
            with cond:
                responses[obj["id"]] = obj; cond.notify_all()
    rec("eof", None)
    with cond:
        cond.notify_all()

def errreader():
    with open(out_path.replace(".jsonl", ".stderr.txt"), "w") as f:
        for raw in proc.stderr:
            f.write(raw.decode(errors="replace")); f.flush()

threading.Thread(target=reader, daemon=True).start()
threading.Thread(target=errreader, daemon=True).start()
saved = {}
nid = [0]

def subst(v):
    if isinstance(v, str) and v.startswith("$"):
        name, *path = v[1:].split(".")
        cur = saved[name]
        for p in path:
            cur = cur[int(p)] if isinstance(cur, list) else cur[p]
        return cur
    if isinstance(v, dict):
        return {k: subst(x) for k, x in v.items()}
    if isinstance(v, list):
        return [subst(x) for x in v]
    return v

def wait_for(i, timeout):
    end = time.time() + timeout
    with cond:
        while i not in responses and time.time() < end and proc.poll() is None:
            cond.wait(0.2)
    return responses.get(i)

pending = {}
for st in steps:
    try:
        if "call" in st or "send" in st:
            nid[0] += 1
            i = nid[0]
            write({"jsonrpc": "2.0", "id": i, "method": st.get("call") or st.get("send"), "params": subst(st.get("params", {}))})
            if "call" in st:
                r = wait_for(i, st.get("timeout", 30))
                if r is None:
                    rec("note", f"timeout waiting for id {i}")
                elif "result" in r and st.get("save"):
                    saved[st["save"]] = r["result"]
            else:
                pending[st.get("save", str(i))] = i
                time.sleep(st.get("after", 0))
        elif "notify" in st:
            time.sleep(st.get("delay", 0))
            write({"jsonrpc": "2.0", "method": st["notify"], "params": subst(st.get("params", {}))})
        elif "wait" in st:
            time.sleep(st["wait"])
        elif "await" in st:
            r = wait_for(pending[st["await"]], st.get("timeout", 30))
            if r is None:
                rec("note", f"timeout awaiting {st['await']}")
            elif "result" in r:
                saved[st["await"]] = r["result"]
        elif "close_stdin" in st:
            proc.stdin.close()
    except Exception as e:
        rec("note", f"step error {st}: {e!r}")
try:
    proc.stdin.close()
except Exception:
    pass
try:
    proc.wait(10)
except subprocess.TimeoutExpired:
    rec("note", "process did not exit 10s after stdin close; killing")
    proc.kill(); proc.wait()
rec("exit", proc.returncode)
json.dump(saved, open(out_path.replace(".jsonl", ".saved.json"), "w"), indent=1)
