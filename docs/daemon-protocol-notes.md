# Driving the daemon by hand

Notes for debugging the daemon's protocol directly, without the IDE. Everything
below runs inside the `bondsymphonic` WSL distro.

## 1. Start the daemon

Build it once, then run it against a scratch data directory and a scratch repo:

```bash
cd /mnt/c/git/BondSymphonic
CARGO_TARGET_DIR=~/.bondsymphonic/target cargo build -p bondsymphonic-daemon
mkdir -p ~/bs-manual/repo ~/bs-manual/data && cd ~/bs-manual/repo
git init -q -b main && git commit -q --allow-empty -m init

~/.bondsymphonic/target/debug/bondsymphonic-daemon \
  --data-dir ~/bs-manual/data --log-level debug
```

The daemon prints exactly one line to stdout before doing anything else — the
port and auth token, as JSON:

```
{"port":40331,"token":"150d3dc7c9b7..."}
```

Everything after that is `tracing` output on stderr. Leave this running in one
terminal and read the port/token from that first line for the next step. (Pass
`--no-sandbox` instead of relying on the platform default if you want to compare
behavior against the noop backend; on Linux the default is `linux_bwrap`.)

## 2. Drive the protocol

The wire format is newline-delimited JSON, one message per line, in both
directions:

- Client to daemon: `{"type":"request","id":<n>,"method":"<name>","params":{...}}`
- Daemon to client: either
  `{"type":"response","id":<n>,"result":{...}}` /
  `{"type":"response","id":<n>,"error":{"code":"...","message":"..."}}`, or
  `{"type":"event","workspace_id":"...","event":{"kind":"...",...}}` — events can
  arrive at any time, interleaved with responses.

`hello` must be the first request on a new connection (with the token from step
1) or everything else comes back `Unauthorized`.

A small `python3` script is the easiest way to poke at this without writing a
throwaway Rust binary. Save it and run it with the port and token from step 1:

```python
# drive.py — usage: python3 drive.py <port> <token> <repo_path>
import json, socket, sys, threading

port, token, repo = int(sys.argv[1]), sys.argv[2], sys.argv[3]
sock = socket.create_connection(("127.0.0.1", port))
rfile = sock.makefile("r")
next_id = 1

def send(method, **params):
    global next_id
    msg = {"type": "request", "id": next_id, "method": method, "params": params}
    next_id += 1
    sock.sendall((json.dumps(msg) + "\n").encode())
    return msg["id"]

def reader():
    for line in rfile:
        msg = json.loads(line)
        if msg["type"] == "event":
            print("EVENT ", msg["workspace_id"], msg["event"])
        else:
            print("REPLY ", msg["id"], msg.get("result", msg.get("error")))

threading.Thread(target=reader, daemon=True).start()

send("hello", token=token, client_version="drive.py")
send("workspace.create", repo_path=repo, base_branch="main", name="scratch")
send("pty.open", workspace_id="PLACEHOLDER", cols=80, rows=24, command=None)

input("press enter to exit\n")
```

`workspace.create`'s reply carries the new `workspace_id`; substitute it into
the `pty.open` call (or just read the printed `REPLY` line and issue `pty.open`
from a second `send(...)` call typed into a REPL — `python3 -i drive.py ...`
drops into one after the script body runs). `pty.write` takes base64: `echo -n
'ls\n' | base64` gives a `data_b64` value.

```
$ python3 -i drive.py 40331 150d3dc7c9b7... /home/bs/bs-manual/repo
REPLY  1 {'daemon_version': '0.1.0', 'capabilities': {...}}
EVENT  ws_48c102af {'kind': 'workspace.state', 'info': {'state': 'creating', ...}}
EVENT  ws_48c102af {'kind': 'workspace.state', 'info': {'state': 'ready', ...}}
REPLY  2 {'id': 'ws_48c102af', 'state': 'ready', 'branch': 'bs/scratch/work', ...}
>>> send("workspace.destroy", workspace_id="ws_48c102af", force=True)
```

## 3. Or just `nc`

For a single request without needing to track ids, `nc` plus a heredoc also
works, since the daemon answers each request as soon as it can regardless of
what else is queued on the connection:

```bash
printf '%s\n%s\n' \
  '{"type":"request","id":1,"method":"hello","params":{"token":"150d3dc7c9b7...","client_version":"nc"}}' \
  '{"type":"request","id":2,"method":"workspace.list","params":{}}' \
  | nc -q1 127.0.0.1 40331
```

`-q1` closes the connection a second after the last byte is read, which is
plenty of time for two quick replies.

## Reference

- Message shapes: `crates/proto/src/message.rs` (`ClientMessage`, `ServerMessage`).
- Every request method and its params: `crates/proto/src/request.rs` (the
  `#[serde(rename = "...")]` on each `Request` variant is the wire method name).
- Every event: `crates/proto/src/event.rs`.
- `system.check_prereqs` is unauthenticated-safe to call anytime after `hello`
  and is a quick way to see what the daemon thinks of its host, including the
  sandbox backend it started with (item named `sandbox`, appended after the
  seven host checks).
