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

### The handshake carries a protocol version

`hello` takes `protocol_version` and answers with one, and the daemon refuses a
client on a version it does not speak:

```json
{"type":"request","id":1,"method":"hello",
 "params":{"token":"…","client_version":"drive.py","protocol_version":1}}
{"type":"response","id":1,
 "result":{"daemon_version":"0.1.0","capabilities":{…},"protocol_version":1}}
```

The number is `bondsymphonic_proto::PROTOCOL_VERSION`, currently **1**. Send a
different one and the reply is an `invalid_params` error, after which the daemon
**closes the connection**:

```json
{"type":"response","id":1,"error":{
  "code":"invalid_params",
  "message":"protocol version 99 is not supported (daemon speaks 1)",
  "data":{"reason":"protocol_mismatch","daemon":1,"client":99}}}
```

Branch on `data.reason`, never on the message text. Leaving the field out
entirely is still accepted: an absent `protocol_version` means a pre-M7 peer and
is read as version 1, in both directions. That is why the `nc` snippet in
section 3 still works without it.

The version is checked **after** the token, so an unauthenticated peer learns
nothing about the build.

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

send("hello", token=token, client_version="drive.py", protocol_version=1)
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

## 4. Landing the work: `workspace.merge` and `workspace.create_pr`

Both run on the host, never inside a sandbox, and both take minutes rather than
milliseconds on a large repository — a hand-written client needs a longer
timeout than the rest of the protocol.

```python
send("workspace.merge", workspace_id=ws, mode="merge", message=None)
send("workspace.merge", workspace_id=ws, mode="squash", message="one line")
send("workspace.create_pr", workspace_id=ws, title="t", body="b", draft=False)
```

`mode` is `merge`, `rebase` or `squash`; `message` is the squash subject and is
ignored by the other two. The reply is
`{"ok": bool, "conflicts": [path, …], "reason": "conflict" | null}`. A conflict
is **not** an error — it is `ok: false` with the paths git left marked, and the
workspace is already back exactly as it was. Branch on `reason`, which is a
stable tag; the prose in `message` is not.

A merge the daemon refuses outright — a base checkout with uncommitted work — is
an `RpcError` instead, carrying its own `data.reason`, because no merge was
attempted. So is a merge that landed but whose objects could not be copied into
the shared store: `data.reason == "objects_stranded"` with `merged` or `pushed`
true means the base really moved but the objects are readable only while the
workspace exists. Do not destroy it.

`workspace.create_pr` answers `{"url": "…"}` and needs a real `origin` and an
authenticated `gh`; it uses the daemon user's own credentials.

## 5. `workspace.list` carries agent records

`workspace.list` and `workspace.get` answer with each workspace's agents twice
over. `agents` is the bare id list it has always been. `agent_records` is the
same agents, in the same order, with what a client needs to rebuild a tab:

```json
{"id":"ag_1","adapter":"claude","state":"exited",
 "session_id":"…","model":"…","permission_mode":"default"}
```

`state` is `exited` for every agent the daemon restored from its records after a
restart, which is what puts a pane on **Restart** rather than on a prompt box,
and `session_id` is what `agent.start`'s `resume_session` takes. The unset
fields are omitted rather than written as `null`, so a record for an agent
started with nothing is `{id, adapter, state}`.

Two lists rather than one changed list, deliberately: an older client reads
`agents` off a newer daemon exactly as before, and a newer client reading an
older daemon finds `agent_records` defaulted to empty. **Empty means "nothing is
known about them", not "there are none"** — `agents` is the list.

There is no field an API key can travel in. A client starting another agent like
one of these supplies the key itself.

`agent.history` replays a restored agent's transcript. Sending to one,
answering a permission for one or interrupting one is a `NotFound` telling you
to start a new agent with `resume_session`.

## Reference

- Message shapes: `crates/proto/src/message.rs` (`ClientMessage`, `ServerMessage`).
- Every request method and its params: `crates/proto/src/request.rs` (the
  `#[serde(rename = "...")]` on each `Request` variant is the wire method name).
- Every event: `crates/proto/src/event.rs`.
- `system.check_prereqs` is unauthenticated-safe to call anytime after `hello`
  and is a quick way to see what the daemon thinks of its host, including the
  sandbox backend it started with (item named `sandbox`, appended after the
  seven host checks).
- The version constant and the mismatch helpers: `crates/proto/src/lib.rs`
  (`PROTOCOL_VERSION`, `peer_protocol_version`) and `crates/proto/src/error.rs`
  (`RpcError::protocol_mismatch`, `protocol_mismatch_versions`,
  `PROTOCOL_MISMATCH_REASON`).
- `AgentSummary` and `WorkspaceInfo::agent_records`: `crates/proto/src/types.rs`.
- Only one daemon may own a data directory. A second on the same `--data-dir`
  prints `another bondsymphonic-daemon owns <dir>` and exits **2**, so a manual
  daemon needs its own scratch directory as in step 1.
