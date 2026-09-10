# BondSymphonic Daemon — Design

**Date:** 2026-09-08
**Status:** Approved for planning
**Parent:** `2026-09-08-bondsymphonic-overview-design.md`

## 1. Role

`bondsymphonic-daemon` is a headless Rust binary that runs inside the WSL2 distro
(later: natively on Linux and macOS). It owns every operation with side effects on
a repository or a sandbox: worktrees, git, sandbox lifecycle, agent processes,
PTYs, web-app runs, network policy, and file access for the editor. The IDE is a
pure client of it over the protocol defined in the overview spec.

## 2. Crate layout

```
crates/daemon/
  src/
    main.rs            CLI args, logging, start server, print port+token line
    server/
      mod.rs           TCP listener, per-connection NDJSON codec, auth
      dispatch.rs      Request -> handler routing, error mapping
      broadcast.rs     Event fan-out to all connections
    workspace/
      mod.rs           Workspace struct, WorkspaceInfo mapping
      registry.rs      Persistent registry (~/.bondsymphonic/workspaces.json)
      lifecycle.rs     create / destroy / status transitions
    git/
      mod.rs           Thin async wrapper around the git CLI
      worktree.rs      worktree add/remove, ref protection layout
      merge.rs         merge / rebase / squash, conflict parsing
      pr.rs            push + gh pr create
    sandbox/
      mod.rs           SandboxBackend trait, SandboxSpec, SandboxHandle
      linux_bwrap.rs   bubblewrap implementation
      noop.rs          runs processes unsandboxed (tests, --no-sandbox flag)
    net/
      proxy.rs         HTTP CONNECT proxy over Unix socket, allowlist
      allowlist.rs     Host pattern matching (exact, *.suffix)
      bridge.rs        Host TCP port <-> Unix socket <-> sandbox localhost port
      forwarder.rs     Tiny in-sandbox forwarder (same binary, subcommand)
    agent/
      mod.rs           AgentAdapter trait, AgentHandle, transcript store
      claude.rs        Claude Code stream-json adapter
      terminal.rs      PTY-based adapter for any CLI
    pty.rs             PTY sessions inside a sandbox
    run/
      config.rs        bondsymphonic.toml parsing
      detect.rs        auto-detection (package.json, compose, Cargo, manage.py)
      manager.rs       run processes, port allocation, state
    fs.rs              list/read/write with path containment, watcher
    prereqs.rs         check_prereqs implementation
  tests/
    workspace_integration.rs
    sandbox_integration.rs
    network_integration.rs
    fixtures/claude-stream/*.ndjson
```

Async runtime: tokio. Each workspace owns a `JoinSet` of tasks; a workspace's
tasks are cancelled together on destroy. Panics in a workspace task are caught by
the JoinSet and reported as `workspace.state {error}`.

## 3. Startup and CLI

```
bondsymphonic-daemon [--data-dir DIR] [--no-sandbox] [--log-level L]
bondsymphonic-daemon forward --socket PATH --port N     # in-sandbox forwarder
bondsymphonic-daemon proxy-shim --socket PATH           # (see 7.2)
```

On start: load registry, bind `127.0.0.1:0`, generate a 32-byte random token,
print `{"port":N,"token":"hex"}` as a single stdout line, then serve. Logs go to
stderr and `~/.bondsymphonic/daemon.log`. The daemon exits when it receives
`system.shutdown` or when stdin closes (the IDE holds stdin open; if the IDE dies,
the daemon stops all sandboxes and exits).

Data directory default: `~/.bondsymphonic/` containing `workspaces.json`,
`worktrees/<ws_id>/`, `homes/<ws_id>/`, `caches/<ws_id>/`, `transcripts/<agent_id>.ndjson`,
`daemon.log`.

## 4. Workspaces

A workspace is the unit of isolation: one worktree, one branch, one sandbox, any
number of agents/PTYs/runs inside it.

```rust
struct Workspace {
    id: WorkspaceId,           // "ws_" + 8 hex
    name: String,              // user-facing, unique per repo
    repo_path: PathBuf,        // main repo (may be under /mnt/c)
    base_branch: String,
    branch: String,            // "bs/<name>/work"  (see 5.2)
    worktree_path: PathBuf,    // ~/.bondsymphonic/worktrees/<id>
    created_at: DateTime,
    allowlist: Vec<HostPattern>,
    state: WorkspaceState,     // Creating | Ready | SandboxDown | Error(String) | Destroying
    agents: Vec<AgentId>,
    runs: Vec<RunId>,
}
```

**Create** (`workspace.create`):
1. Validate repo (`git rev-parse --git-common-dir`), base branch exists.
2. Compute branch `bs/<name>/work`; fail with `Conflict` if it exists.
3. Pre-create the writable ref directories (5.2).
4. `git worktree add -b bs/<name>/work <worktree_path> <base_branch>`.
5. Create `homes/<id>` seeded with Claude credentials (8.3) and `caches/<id>`.
6. Build the `SandboxSpec` and start the sandbox supervisor (6).
7. Persist to registry, emit `workspace.state`.

**Destroy**: stop agents, runs, PTYs; tear down sandbox; `git worktree remove
--force`; `git branch -D bs/<name>/work`; delete `homes/`, `caches/`, transcripts;
remove from registry. With `force=false`, refuse if the worktree has uncommitted
changes or unmerged commits and return `Conflict` with details.

**Registry** is rewritten atomically (write temp, rename) after every change.
On startup, each registered workspace is validated: if the worktree directory or
the branch is gone, the workspace is marked `Error` rather than deleted, so the
user can decide.

## 5. Git

All git access goes through the `git` CLI via `tokio::process::Command`, with
`GIT_TERMINAL_PROMPT=0`, structured stderr capture, and a 60-second timeout.
Every failure maps to `GitError {command, exit_code, stderr}`.

### 5.1 Why the CLI and not libgit2
Worktrees, rebase, and `gh` interplay are far better covered by the CLI; it
avoids a C dependency; and the sandboxed agent already needs git installed.

### 5.2 Ref protection layout

The sandbox must let the agent commit on its own branch without being able to
move the base branch or any other workspace's branch. Git updates refs by
creating `<ref>.lock` in the ref's directory and renaming, so protection has to be
per directory, not per file. Hence every workspace branch lives in its own
directory: `refs/heads/bs/<name>/work`.

Inside the sandbox, the main repo's `.git` is mounted read-only except these
paths, which are bind-mounted read-write:

| Path under main `.git` | Why writable |
|---|---|
| `worktrees/<ws_id>/` | HEAD, index, ORIG_HEAD, logs for this worktree (git names it after the worktree directory) |
| `refs/heads/bs/<name>/` | the workspace branch and its lock file |
| `logs/refs/heads/bs/<name>/` | reflog for the branch |

New objects are kept out of the shared store: the sandbox environment sets
`GIT_OBJECT_DIRECTORY=<worktree_path>/../objects-<id>` and
`GIT_ALTERNATE_OBJECT_DIRECTORIES=<repo>/.git/objects`, so the agent's commits are
written to a private object directory while it can still read every existing
object. `objects/` in the main repo therefore stays read-only.

Consequences the daemon handles:
- Every daemon-side git command touching the main repo (changes, diff, merge,
  rebase, squash, push) runs outside the sandbox with the main store as primary
  (`GIT_OBJECT_DIRECTORY` unset) and `GIT_ALTERNATE_OBJECT_DIRECTORIES=<private
  objects>`, so it can read workspace commits and any commits it creates land in
  the shared store. After a successful merge, rebase, squash, or push the daemon
  runs `git repack -a -d` in the main repo so every referenced object is copied
  into the shared store before the private directory is deleted with the
  workspace.
- `git fetch`/`git pull` inside the sandbox cannot update `refs/remotes/*` (read-
  only). This is intended: fetches are a daemon operation (`repo.inspect` refreshes
  remotes on request).
- `git gc` / `pack-refs` inside the sandbox fail harmlessly.

A `--no-git-protect` daemon flag mounts `.git` read-write for troubleshooting;
`check_prereqs` reports whether protection is active.

### 5.3 Changes and diff
- `workspace.changes`: `git diff --numstat --name-status <merge-base>...HEAD` plus
  uncommitted changes (`git status --porcelain=v2`), merged into one list with
  status `added|modified|deleted|renamed|untracked`.
- `workspace.diff {path}`: returns base text (`git show <merge-base>:<path>`, empty
  if absent) and working-tree text. The IDE computes and renders the diff.

### 5.4 Merge, rebase, squash
Run in the main repo by the daemon, never inside a sandbox:
- Guard: main repo working tree must be clean on the base branch, else
  `Conflict {reason: "base_dirty"}`. If the main repo is currently checked out on a
  different branch, the daemon uses a temporary worktree of the base branch under
  `~/.bondsymphonic/merge-<id>` so it never disturbs the user's checkout.
- `merge`: `git merge --no-ff bs/<name>/work`.
- `rebase`: `git rebase <base> bs/<name>/work` in the workspace worktree, then
  fast-forward the base.
- `squash`: `git merge --squash` + `git commit -m "<name>: <summary>"` where the
  summary is the first line of the last workspace commit unless the request
  supplies a message.
- On conflict: abort (`--abort`), return `{ok:false, conflicts:[paths]}`. The
  workspace is untouched and the user can ask the agent to rebase.

### 5.5 PR
`git push -u origin bs/<name>/work` then `gh pr create --title --body [--draft]
--head bs/<name>/work --base <base>`; parse the URL from stdout. `gh` must be
authenticated in the distro (reported by `check_prereqs`).

## 6. Sandbox

### 6.1 Trait

```rust
#[async_trait]
trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Vec<PrereqStatus>;
    /// Start the long-lived sandbox for a workspace (one per workspace).
    async fn start(&self, spec: &SandboxSpec) -> Result<Box<dyn SandboxHandle>>;
}

#[async_trait]
trait SandboxHandle: Send + Sync {
    /// Spawn a process inside the running sandbox.
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild>;
    async fn shutdown(&self) -> Result<()>;   // kills every process inside
}

struct SandboxSpec {
    id: WorkspaceId,
    rw_binds: Vec<(PathBuf, PathBuf)>,   // host -> sandbox
    ro_binds: Vec<(PathBuf, PathBuf)>,
    home: PathBuf,                        // mounted at /home/<user>
    run_dir: PathBuf,                     // host ~/.bondsymphonic/run/<id>, mounted rw at /run/bs
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}

struct SandboxCommand { argv: Vec<String>, pty: Option<PtySize>, env: BTreeMap<String,String>, cwd: Option<PathBuf> }

struct SandboxChild {
    pid: u32,                       // pid as seen by the sandbox init
    stdin: Option<OwnedFd>, stdout: Option<OwnedFd>, stderr: Option<OwnedFd>,  // pipes, or
    pty_master: Option<OwnedFd>,    // when cmd.pty was set
    exit: oneshot::Receiver<i32>,
}
```

Every process (agent, shell, web app, forwarder) is spawned through the
workspace's `SandboxHandle`, so all of them share one set of namespaces: one
network namespace (so the forwarder can reach the dev server), one PID namespace
(so shutdown kills everything), one mount namespace.

### 6.2 Linux/bwrap filesystem rules
- `--ro-bind / /` as the base, then `--tmpfs /tmp`, `--proc /proc`, `--dev /dev`.
- `--tmpfs /home`, then `--bind homes/<id> /home/<user>`.
- `--bind worktree_path worktree_path` (same path inside so git paths match).
- `--bind objects-<id> objects-<id>`.
- The three writable `.git` subpaths from 5.2 (`--ro-bind <repo>/.git` first,
  then the rw binds on top).
- `--bind caches/<id> /home/<user>/.cache`.
- `--bind ~/.bondsymphonic/run/<id> /run/bs` (exec, proxy, and forward sockets).
- `--tmpfs /opt`, then `--ro-bind <resolved claude> /opt/bs/claude`: the mount
  point cannot be created under the read-only root, and an empty `/opt` also
  keeps host-installed third-party software out of a workspace. See 8.2 for how
  the host path is resolved.
- `--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup`
  and `--unshare-net` (6.3). `--die-with-parent`, `--new-session`.
- No `/mnt/c` visibility unless the repo lives there, in which case only the
  repo's `.git` and the worktree binds are visible.

### 6.3 Sandbox init (one bwrap per workspace)
bubblewrap cannot join an existing network namespace, so the daemon runs exactly
one `bwrap` per workspace and does all process management through a small init
inside it:

```
bwrap <mount and unshare flags> --die-with-parent --new-session \
      -- bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock
```

`sandbox-init` is PID 1 inside the sandbox. It:
- listens on the Unix socket `/run/bs/exec.sock` (host path
  `~/.bondsymphonic/run/<id>/exec.sock`, rw-bound as `/run/bs`);
- accepts spawn requests (argv, env, cwd, optional pty size) from the daemon;
  for each it forks the child inside the sandbox, and passes the child's stdio
  pipes or the PTY master back to the daemon over the socket with `SCM_RIGHTS`,
  so the daemon reads and writes those fds directly with no proxying;
- reports exits (pid, code) on the socket and reaps zombies;
- on `shutdown` or socket close sends SIGTERM to every child, SIGKILL after 5 s,
  then exits, which tears the sandbox down.

The `linux_bwrap` backend's `start` spawns bwrap, waits for `exec.sock` to
appear (5 s timeout), and returns a handle wrapping the socket client. `spawn`
is one request/response on that socket. This mirrors how container runtimes
implement `exec` and keeps every bwrap-specific flag in one place.

The daemon itself lives outside the sandbox, so the proxy (7.2) and port bridge
(7.3) sockets are simply files in `/run/bs` that both sides can open.

### 6.4 Noop backend
Runs commands directly with the same env and cwd. Used by unit tests, on hosts
without namespace support (with a loud warning in the UI), and via
`--no-sandbox`.

## 7. Network

### 7.1 Allowlist
`HostPattern` is either an exact host (`api.anthropic.com`) or a suffix wildcard
(`*.npmjs.org`). Ports are unrestricted. Default list: `api.anthropic.com`,
`*.anthropic.com`, `registry.npmjs.org`, `*.npmjs.org`, `pypi.org`,
`files.pythonhosted.org`, `crates.io`, `static.crates.io`, `index.crates.io`,
`github.com`, `*.github.com`, `*.githubusercontent.com`. The repo's
`bondsymphonic.toml` `[network] allow = [...]` extends it; `workspace.set_allowlist`
overrides it at runtime.

### 7.2 Proxy
Per workspace, the daemon listens on a Unix socket
`~/.bondsymphonic/run/<id>/proxy.sock`, bound into the sandbox at
`/run/bs/proxy.sock`. Because most tools only speak proxy over TCP, a shim inside
the sandbox (`bondsymphonic-daemon proxy-shim`) listens on `127.0.0.1:3128` and
pipes to the Unix socket. The sandbox env sets `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, `NO_PROXY=localhost,127.0.0.1`, and git/npm/pip/cargo honour those.

The proxy implements HTTP `CONNECT` (for TLS) and plain absolute-URI `GET/POST`
forwarding. It checks the target host against the allowlist before connecting and
answers `403` with a body naming the host and the config key to add it. Every
denial is emitted as `daemon.log {level: warn}` so the IDE can surface it.

### 7.3 Port bridge (web apps)
When a run declares port `P`, the daemon:
1. Allocates a free host port `H`.
2. Creates `~/.bondsymphonic/run/<id>/fwd-P.sock`, bound into the sandbox at
   `/run/bs/fwd-P.sock`.
3. Spawns inside the sandbox `bondsymphonic-daemon forward --socket
   /run/bs/fwd-P.sock --port P`, which accepts on the Unix socket and connects to
   `127.0.0.1:P`.
4. Listens on `127.0.0.1:H` and bridges each accepted TCP connection to the Unix
   socket.

**The status byte.** Before any of the application's own bytes, the forwarder
writes exactly one byte on each accepted Unix connection: `1` once it holds the
in-sandbox TCP connection, `0` when it could not get one (the connect attempt is
on a 2 s clock). The host side reads that byte before it starts copying, and on
`0`, on a read error, or after 5 s of silence it shuts the TCP connection down
rather than leaving it open — so a browser is told "connection refused" instead
of spinning on a port whose server is not there.

The byte is also what makes readiness a fact rather than a guess. A TCP port on
the host that accepts proves only that the bridge's own listener is up; a `1`
from the far side of the namespace proves the application answered. The
readiness poll (10.3) is therefore one round trip on the socket — connect, read
the byte, close — with a 400 ms budget over both steps so a probe cannot spill
into the next 500 ms tick. Nothing is written to the application, so probing a
server that logs its requests does not fill the run's output with them.

WSL2 forwards Windows `localhost:H` to the distro, so the IDE presents
`http://localhost:H`. Once the byte says `1`, bridging is bidirectional byte
copying with per-connection tasks and no lifetime cap: no protocol awareness, so
WebSockets and HMR work.

## 8. Agents

### 8.1 Trait

```rust
#[async_trait]
trait AgentAdapter: Send {
    async fn start(&mut self, ctx: AgentContext) -> Result<()>;
    async fn send(&mut self, text: String) -> Result<()>;
    async fn permission_reply(&mut self, reply: PermissionReply) -> Result<()>;
    async fn interrupt(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}
```

`AgentContext` gives the adapter the sandbox backend + spec, an event sink
(`mpsc::Sender<AgentEvent>`), and the transcript store. Every event the adapter
emits is appended to `transcripts/<agent_id>.ndjson` before broadcast, so
`agent.history` is a file read.

### 8.2 Claude Code adapter
Spawns inside the sandbox, as built by `claude_argv` in
`crates/daemon/src/agents/claude.rs`:

```
<claude> -p --input-format stream-json --output-format stream-json --verbose \
       --include-partial-messages --permission-prompts host \
       [--resume <session_id>] [--model M] [--permission-mode MODE]
```

`<claude>` is always an absolute path, never the bare name. Under `linux_bwrap`
it is `/opt/bs/claude`, where `workspace::lifecycle::spec_for` binds the daemon
user's own install read-only; under a backend with no mounts it is that
install's own path. Claude Code is a single native executable, so one bind of
one file is the whole of it, and nothing is created on the host. The binary is
resolved from `~/.local/bin/claude`, canonicalised because it is a symlink into
`~/.local/share/claude/versions/<v>`, falling back to a `PATH` search that
refuses anything under `/mnt/`. A host with no install makes `agent.start` fail
with `PrereqMissing` naming the path it looked at, rather than an exec failure a
second later.

`--permission-prompts host` is what routes a tool prompt back over stdout as a
`control_request` for the IDE to answer, instead of the CLI asking at a terminal
it does not have. The three optional flags are appended only when
`agent.start.options` carries a non-empty value for them, and `permission_mode`
is validated against the CLI's own list (`default`, `acceptEdits`, `plan`,
`dontAsk`, `bypassPermissions`, `auto`, `manual`) so a bad value is an
`InvalidParams` on `agent.start` rather than a usage error a second later.

The flag set is pinned per Claude Code version: the adapter records
`TESTED_CLAUDE_VERSION = "2.1.263"` and warns once per daemon lifetime when the
installed `claude --version` differs. Every flag above was verified accepted by
2.1.263 — a wrong flag makes `claude` exit with "unknown option" before any login
check, so this is testable without being logged in. Two traps found in practice:

- Under WSL a *non-login* shell inherits the Windows PATH, so a bare `claude`
  can resolve to a Windows npm install of a different version. 2.1.177 rejects
  `--permission-prompts` outright. This is why the daemon never spawns the bare
  name: the version check, the prerequisite check and the agent spawn all go
  through the one resolver above, which prefers `~/.local/bin/claude` and refuses
  `/mnt/...`. `scripts/record-claude-stream.sh` refuses a `/mnt/...` binary for
  the same reason. The setup terminals still put `$HOME/.local/bin` first on
  their `PATH`, because those run `claude auth login` by name on the host.
- `BS_CLAUDE_BIN` replaces the program (split with `shell_words`, so an
  interpreter plus a script works) and suppresses the version warning, because a
  stand-in's version says nothing about the protocol. It wins on every backend,
  so under `linux_bwrap` whatever it names has to be reachable *inside* the
  sandbox -- under the worktree, or another bound path -- because the sandbox has
  its own `/home` and `/tmp`.

Input: each `agent.send` is first recorded as a `user_text` transcript event
(so history replay shows the user's side), then becomes a
`{"type":"user","message":{"role":"user","content":[{"type":"text","text":...}]}}`
line on stdin. Output lines are parsed into `AgentEvent`:

| stream-json | AgentEvent |
|---|---|
| `system` (init) | `system {subtype:"init", session_id, model, tools}`; session_id saved for resume |
| `stream_event` with text delta | `assistant_delta` |
| `assistant` message blocks | `assistant_text`, `tool_use` |
| `user` message with tool_result | `tool_result` |
| `control_request` (`can_use_tool`) | `permission_request`; state → `waiting_permission` |
| `result` | `result {cost, duration, turns}`; state → `idle` |

`permission_reply` writes a `control_response` line carrying allow (with optional
updated input) or deny (with message), and then records the answer in the
transcript as `system {subtype:"permission_reply", data:{request_id, decision}}`.
That record is what makes a replayed transcript agree with a live one: the
request is a message and comes back from disk, so without the answer beside it a
client that re-attaches raises its permission bar over a settled question and the
reply it then sends is a `NotFound`. `interrupt` writes a `control_request`
`interrupt` line if supported by the pinned version, otherwise sends SIGINT.
`stop` closes stdin, waits 5 s, then kills the process group.

Unknown message types are stored verbatim as `system {subtype:"raw"}` so nothing
is lost when Claude Code adds message kinds.

Fixtures in `tests/fixtures/claude-stream/` drive the parser tests, and
`tests/fixtures/fake_claude.py` replays one of them in place of the real CLI for
the integration tests, so no suite needs a network or a login. As of Milestone 4
those fixtures are **synthetic**: written from the documented shapes, with every
flag verified against the real binary, but no line in them came out of a real
`claude`. `scripts/record-claude-stream.sh` records one real turn into
`recorded-<version>.ndjson` beside them; run it once from a login shell on a
logged-in machine and correct any synthetic shape that differs.

### 8.3 Credentials and home seeding
On workspace creation, `homes/<id>/` receives:
- `.claude/settings.json` copied from the daemon user's `~/.claude/settings.json`
  if present.
- `.claude/.credentials.json` and `~/.claude.json` copied if present
  (OAuth login), else `ANTHROPIC_API_KEY` is passed through from the daemon's
  environment or from `agent.start.options.api_key`.
- `.gitconfig` with `user.name`/`user.email` copied from the daemon user's config.

The spec accepts that the agent can read these credentials; the sandbox protects
the host, not the credentials. A `.claude/settings.json` supplied via
`bondsymphonic.toml [claude] settings = "path"` overrides the copy, which is how a
repo can pin allowed tools.

### 8.4 Terminal adapter
A terminal agent is a PTY, not an entry in the agent registry: the IDE calls
`pty.open` directly with the configured command (default `$SHELL -l`, or e.g.
`codex`) inside the sandbox, and never `agent.start`. Events are raw
`pty.output`; input is `pty.write`; the process dying is `pty.exit`. There is no
`AgentAdapter` implementation behind it, no transcript file and no `agent.state`,
which is why a terminal tab's status follows its workspace rather than an agent.
`AgentAdapterKind::Terminal` exists in the protocol and in `capabilities.adapters`
only to name the choice in the New Agent dialog; `agent.start` refuses it with
`InvalidParams("terminal agents use pty.open")`.

## 9. PTY
`portable-pty` (Rust) opens a pty pair; the slave is handed to the sandboxed
child. Output is read in 4 KiB chunks and emitted as `pty.output` base64.
Resize forwards `TIOCSWINSZ`. Idle PTYs cost one task each.

## 10. Runs

### 10.1 `bondsymphonic.toml`

```toml
[[run]]
name = "web"
command = "npm run dev -- --port 3000"
port = 3000
cwd = "."               # optional, relative to worktree
env = { NODE_ENV = "development" }
ready_regex = "Local:.*http"   # optional, marks state "ready"

[[run]]
name = "api"
command = "cargo run -p api"
port = 8080

[network]
allow = ["*.mycompany.com"]

[claude]
settings = ".claude/settings.json"   # optional
```

### 10.2 Auto-detection (when no file, or `repo.detect_run_configs` asks)
Ordered heuristics, each yielding `RunConfig {name, command, port, source:
"detected"}`:
- `package.json` scripts `dev`, `start`, `serve` → `npm run <script>` (or `pnpm`/
  `yarn` if the lockfile says so); port guessed from `vite.config.*`, `next` (3000),
  `angular.json` (4200), or 3000.
- `docker-compose.yml` → not runnable in v1 (no Docker in sandbox); listed with
  `disabled_reason`.
- `Cargo.toml` with a `[[bin]]` or `axum`/`actix`/`rocket` dependency →
  `cargo run`, port 8080 guess.
- `manage.py` → `python manage.py runserver 0.0.0.0:8000`, port 8000.
- `pyproject.toml` with `fastapi`/`flask` → `uvicorn`/`flask run`, port 8000/5000.

Guessed ports are flagged `port_guessed: true` so the IDE lets the user edit them.

### 10.3 Manager
`run.start` spawns the command through the sandbox with the run's env, plus
`PORT=<port>` and `HOST=0.0.0.0`, sets up the bridge (7.3), and streams stdout/
stderr lines as `run.output`. `PORT` and `HOST` are pushed after the config's own
env, so a repository cannot quietly redefine the two variables every run is
promised. State: `starting` → `ready` (regex match, else the forwarder's status
byte, polled every 500 ms) → `stopped`/`failed`. A configuration that sets
`ready_regex` is saying the port alone is not good enough, so it is never made
ready by a probe. `run.stop` sends SIGTERM to the process group, SIGKILL after
5 s, and tears down the bridge; `workspace.destroy` stops every run first.

**The noop backend has no bridge.** Without a network namespace the run is a
plain child of the daemon and its port already is the host's, so `host_port` is
the configuration's own port, the URL is `http://localhost:<port>`, no
`fwd-<P>.sock` and no in-sandbox forwarder exist, and readiness is a direct TCP
connect to `127.0.0.1:<port>` on the same 500 ms tick. This is the path Windows
development takes, and the one the daemon's `run_integration` suite exercises on
both hosts; the bridge path is covered by `sandbox_integration` under bwrap.
A client must therefore take the URL from `run.start`'s reply or the `ready`
event and never rebuild it from the configuration's port: under bwrap the two
differ, and the host port changes on every start.

## 11. File service
- Paths are joined to the worktree root and canonicalised; anything escaping the
  root (including via symlink) returns `InvalidParams`.
- `read_file` returns UTF-8 text, or `encoding: "binary"` with no content for
  non-UTF-8 files, truncated above 4 MiB with `truncated: true`.
- `write_file` writes atomically (temp + rename) and preserves mode.
- `fs.watch` uses `notify` with 200 ms debouncing; emits relative paths; ignores
  `.git/`, `node_modules/`, `target/`.

## 12. Prerequisite checks
`check_prereqs` returns, in order: `git ≥ 2.40`, `bwrap` present, user namespaces
usable (spawn `bwrap --ro-bind / / --unshare-all true`), `--userns` support,
`claude` present + version, `claude` authenticated (`~/.claude/.credentials.json`
or API key present), `gh` present, `gh auth status` ok. Each has `fix_hint` with a
shell command.

## 13. Testing (daemon-specific)
- Unit: allowlist matching; run config parsing and detection on fixture trees;
  stream-json parsing on recorded fixtures; registry load/save; path containment.
- Integration (`tests/`, Linux only, `#[cfg(target_os = "linux")]`):
  - `workspace_integration`: temp repo → create → commit inside worktree via
    sandboxed `git` → `changes` lists it → `merge` succeeds and base contains it →
    `repack` made objects visible → destroy cleans everything.
  - `sandbox_integration`: sandboxed `touch /etc/x` fails; `touch $HOME/x`
    succeeds; `git update-ref refs/heads/main` inside fails; write to
    `refs/heads/bs/<name>/work` succeeds.
  - `network_integration`: a local TCP server on the host is unreachable directly
    from the sandbox; reachable via proxy only when allowlisted; a `python -m
    http.server` inside the sandbox is reachable on the bridged host port.
- Tests skip with a clear message (not fail) when `bwrap` is unavailable, so
  `cargo test` on a bare CI box still passes the non-sandbox tests.
