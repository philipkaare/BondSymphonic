# BondSymphonic — Overview Design

**Date:** 2026-09-08
**Status:** Approved for planning
**Companion specs:** `2026-09-08-bondsymphonic-daemon-design.md`, `2026-09-08-bondsymphonic-ide-design.md`

## 1. Purpose

BondSymphonic is a desktop IDE for orchestrating coding agents. Each agent works in
its own git worktree inside its own OS-level sandbox, and the IDE makes it easy to
run and test the web application each agent is building. It looks like a classic
IDE: agent groups across the top, a folder view on the left, and a split view with
the code editor on one side and the running agent on the other.

It is written in Rust for a small memory footprint and uses Qt for portability
across Windows, Linux, and macOS.

## 2. Goals for v1

1. Open a git repository and create several agent workspaces from it, each a git
   worktree on its own branch with its own sandbox.
2. Run Claude Code inside the sandbox with a structured (non-terminal) UI: chat
   transcript, tool-call cards, permission prompts, cost.
3. Run any other CLI agent or a plain shell inside the sandbox in a terminal pane.
4. Start a workspace's web app inside the sandbox from a per-repo run config (with
   auto-detection fallback), forward its port to the host, and open it in the
   system browser.
5. Edit files in the worktree with syntax highlighting; see a diff of every file the
   agent changed against the base branch.
6. Merge or rebase a workspace back into its base branch, or push and open a PR, or
   discard it. Clean up worktree and sandbox afterwards.
7. Survive restarts: workspaces, agent sessions, groups, and layout persist.

**v1 acceptance scenario:** open one repo, spawn two Claude Code agents in separate
sandboxed worktrees, give each a task, start each one's web app and open both in
the browser at the same time, review both diffs, merge one, discard the other.

## 3. Non-goals for v1 (explicitly deferred)

- VS Code integration (a future extension would speak the daemon protocol).
- macOS sandbox backend (`sandbox-exec`) and native Win32 sandbox backend.
- Language server support, autocomplete, or refactoring in the editor.
- An embedded browser; web apps open in the system browser.
- Agent-to-agent messaging or a "lead agent" that dispatches workers.
- Structured adapters for agents other than Claude Code (they use the terminal
  adapter).
- Docker/container-based sandboxes.

## 4. Platform decisions

| Decision | Choice | Reason |
|---|---|---|
| Primary platform | Windows 11 | User's machine and first target |
| Sandbox technology | OS-native namespaces inside WSL2 (bubblewrap) | Lightweight, no Docker, same backend serves native Linux later |
| Isolation level | Filesystem scoped to the worktree + outbound network allowlist | Protects the host and limits exfiltration without VM overhead |
| UI toolkit | Qt 6 Widgets | Classic IDE look, smallest Qt runtime footprint |
| Language split | Rust for all logic; C++ only for the Qt Widgets shell | Rust footprint and safety; no maintained pure-Rust Widgets binding exists |
| Rust/Qt bridge | cxx-qt | Maintained by KDAB, cargo-driven build, exposes Rust structs as QObjects |
| Agent protocol | Claude Code stream-json (input and output) | Structured events without scraping a terminal |
| Web preview | System browser | Keeps the binary and memory small |
| Merge flow | Local merge/rebase and PR via `gh`, both supported | User choice per workspace |

## 5. System architecture

```
+-----------------------------------------------------------+
| Windows                                                   |
|  bondsymphonic-ide.exe  (Rust + C++ Qt Widgets, cxx-qt)   |
|    - window, panels, editor, diff, terminal rendering     |
|    - Rust model layer (groups, tabs, buffers, highlight)  |
|    - daemon client (tokio, TCP localhost, NDJSON)         |
|          |                                                |
|          | TCP 127.0.0.1:<port>  (WSL2 localhost forward) |
+----------|------------------------------------------------+
           v
+-----------------------------------------------------------+
| WSL2 distro                                               |
|  bondsymphonic-daemon (Rust, headless)                    |
|    - workspace registry (worktree + sandbox pairs)        |
|    - git operations (git CLI)                             |
|    - SandboxBackend: linux_bwrap  (macos, win32 later)    |
|    - network: CONNECT proxy + allowlist, port bridge      |
|    - agent adapters: claude (stream-json), terminal (pty) |
|    - run manager (bondsymphonic.toml / auto-detect)       |
|    - fs service for the editor                            |
|                                                           |
|   per workspace:  bwrap sandbox                           |
|     [ claude | shell | web app | port forwarder ]         |
+-----------------------------------------------------------+
```

Three Cargo crates in one workspace, plus a C++ directory:

| Path | Crate / dir | Role |
|---|---|---|
| `crates/proto` | `bondsymphonic-proto` | Protocol types (requests, responses, events), shared by both binaries |
| `crates/daemon` | `bondsymphonic-daemon` | Linux daemon; see daemon spec |
| `crates/ide` | `bondsymphonic-ide` | Windows/Linux/macOS IDE; see IDE spec |
| `crates/ide/cpp` | C++ | Qt Widgets shell compiled by cxx-qt-build |
| `scripts/` | | Setup, build-daemon-in-WSL, package |
| `docs/` | | Specs, plans, user docs |

The IDE contains no sandbox, git, or process logic. Everything with side effects
on a repository or a sandbox is a daemon request. This boundary is what makes the
future VS Code extension and the future Linux/macOS builds cheap.

## 6. Protocol (`bondsymphonic-proto`)

### 6.1 Transport

- TCP on `127.0.0.1`. The daemon binds an ephemeral port and prints one JSON line
  to stdout on startup: `{"port": 41234, "token": "<random 32 bytes hex>"}`. The
  IDE reads it from the `wsl.exe` child process's stdout.
- Newline-delimited JSON (NDJSON). One message per line, UTF-8.
- The first client message must be `hello` carrying the token; anything else
  closes the connection. This stops other local processes from driving the daemon.
- Multiple authenticated connections are permitted; events are broadcast to all.

### 6.2 Message envelope

```jsonc
// client -> daemon
{"type": "request", "id": 17, "method": "workspace.create", "params": { ... }}

// daemon -> client, exactly one per request
{"type": "response", "id": 17, "result": { ... }}
{"type": "response", "id": 17, "error": {"code": "GitError", "message": "...", "data": { ... }}}

// daemon -> client, unsolicited
{"type": "event", "workspace_id": "ws_a1b2", "event": {"kind": "agent.message", ...}}
```

`id` is a client-chosen u64. `workspace_id` on events is optional (daemon-level
events omit it). Binary payloads (PTY bytes) are base64 strings.

All types are Rust enums/structs in `bondsymphonic-proto` with `serde` derives and
`#[serde(tag = "method", content = "params")]` for requests, so the IDE and daemon
cannot drift. The crate has round-trip tests for every variant.

### 6.3 Methods

Grouped by namespace. Params and results are summarised; exact fields live in the
crate.

**system**
- `hello {token, client_version}` → `{daemon_version, capabilities}`
- `system.check_prereqs` → list of `{name, ok, detail, fix_hint}` for git, bwrap,
  user-namespace support, claude, gh
- `system.shutdown`

**repo**
- `repo.inspect {path}` → `{default_branch, branches[], is_dirty, remotes[]}`
- `repo.detect_run_configs {path}` → `RunConfig[]` (from `bondsymphonic.toml` or
  auto-detection, with `source` field)

**workspace**
- `workspace.create {repo_path, base_branch, name}` → `WorkspaceInfo`
- `workspace.list` → `WorkspaceInfo[]`
- `workspace.get {workspace_id}` → `WorkspaceInfo`
- `workspace.destroy {workspace_id, force}` — stops all processes, removes sandbox
  and worktree, deletes branch
- `workspace.status {workspace_id}` → git status entries
- `workspace.changes {workspace_id}` → files changed vs base
  `{path, status, additions, deletions}`
- `workspace.diff {workspace_id, path}` → `{base_text, work_text}` (the IDE
  computes the diff)
- `workspace.merge {workspace_id, mode: "merge"|"rebase"|"squash"}` →
  `{ok, conflicts[]}`
- `workspace.create_pr {workspace_id, title, body, draft}` → `{url}`
- `workspace.set_allowlist {workspace_id, hosts[]}`

**fs** (paths are always relative to the worktree root; `..` is rejected)
- `fs.list_dir {workspace_id, path}` → entries with type, size, git status
- `fs.read_file {workspace_id, path}` → `{content, encoding, truncated}`
- `fs.write_file {workspace_id, path, content}`
- `fs.watch {workspace_id, enable}` — emits `fs.changed` events

**agent**
- `agent.start {workspace_id, adapter: "claude"|"terminal", options}` → `{agent_id}`
- `agent.send {agent_id, text}` — user message (claude) or raw input (terminal)
- `agent.permission_reply {agent_id, request_id, decision: allow|deny,
  updated_input?, message?}`
- `agent.interrupt {agent_id}`
- `agent.stop {agent_id}`
- `agent.history {agent_id}` → `{messages, state, detail?}`: the stored
  transcript events for replay, plus the agent's state as of the read. The
  state travels with the history because state changes are events, not
  transcript entries, so a client attaching to an agent that is already
  running has missed every one of them. Both extra fields default, so an
  older daemon's reply still deserialises.

**pty**
- `pty.open {workspace_id, cols, rows, command?}` → `{pty_id}`
- `pty.write {pty_id, data_b64}`
- `pty.resize {pty_id, cols, rows}`
- `pty.close {pty_id}`

**run**
- `run.start {workspace_id, config_name}` → `{run_id, host_port, url}`
- `run.stop {run_id}`
- `run.list {workspace_id}` → active runs

### 6.4 Events

- `daemon.log {level, message}`
- `workspace.state {info}` — created, sandbox up, sandbox down, destroyed
- `agent.state {agent_id, state: idle|working|waiting_permission|error|exited,
  detail}`
- `agent.message {agent_id, message}` — a typed transcript item: `user_text`,
  `assistant_text`, `assistant_delta`, `tool_use {id, name, input}`,
  `tool_result {id, output, is_error}`, `permission_request {request_id,
  tool_name, input, suggestions}`, `result {cost_usd, duration_ms, num_turns,
  session_id}`, `system {subtype, data}`
- `pty.output {pty_id, data_b64}`, `pty.exit {pty_id, code}`
- `run.output {run_id, line}`, `run.state {run_id, state, url}`
- `fs.changed {paths[]}`

### 6.5 Errors

`error.code` is a closed enum: `Unauthorized`, `InvalidParams`, `NotFound`,
`GitError`, `SandboxError`, `PrereqMissing`, `AgentError`, `IoError`, `Conflict`,
`Internal`. `data` carries structured detail (for `GitError` the command, exit
code, stderr; for `PrereqMissing` the same shape as `check_prereqs`).

## 7. Cross-cutting rules

- **Isolation of failures.** Every workspace is its own async task tree in the
  daemon. A panic or error inside one workspace is reported as an event for that
  workspace; the daemon and other workspaces keep running.
- **Reconnection.** If the IDE loses the daemon, it enters a read-only
  "reconnecting" state, relaunches the daemon if the process died, and re-syncs
  from `workspace.list` and `agent.history`. Agent sessions resume via Claude
  Code's `--resume`: after a restart an agent comes back as an `exited` record
  whose history is still readable, and the transcript's Restart starts a new
  agent with `resume_session` set to the last session id in that history. Runs
  and PTYs do not survive — a terminal shows `[daemon restarted]` and offers to
  reopen.
- **No hidden shell-outs from the IDE.** The IDE only ever spawns `wsl.exe` to
  start the daemon and the system browser to open a URL.
- **Prerequisite messages are specific.** Each missing prerequisite names the
  thing missing and the exact command to install it.
- **Footprint targets.** IDE idle RSS under 150 MB on Windows with two workspaces
  open and files in the editor; daemon under 30 MB excluding sandboxed processes.
  Measured, not promised: the plan includes a task that records both numbers.

## 8. Toolchain and repository setup

None of this is installed on the development machine today (Docker, git, WSL2 and
Claude Code on Windows are present; Rust, Qt, CMake and the WSL-side tools are
not). The first implementation task is a scripted, verified setup:

**Windows**
- Rust stable via rustup (MSVC toolchain).
- Visual Studio 2022 Build Tools with the C++ workload.
- Qt 6.x (6.8 LTS or newer) for MSVC 2022 64-bit via the Qt online installer or
  `aqtinstall`. `QMAKE` env var or `qmake` on PATH for cxx-qt-build.
- CMake and Ninja.

**WSL2**
- A dedicated distro (Ubuntu 24.04 recommended; the existing NVIDIA JetPack distro
  is not assumed). Configured name stored in IDE settings.
- Inside it: git, bubblewrap, Rust via rustup, Claude Code (native installer),
  GitHub CLI (`gh`), and user-namespace support verified by running
  `bwrap --ro-bind / / --unshare-all true`. If Ubuntu's AppArmor restriction on
  unprivileged user namespaces is active, the setup script sets
  `kernel.apparmor_restrict_unprivileged_userns=0` in `/etc/sysctl.d/`.

**Build workflow**
- `cargo build -p bondsymphonic-ide` on Windows builds the IDE (cxx-qt-build
  compiles the C++ shell as part of the cargo build).
- `scripts/build-daemon.ps1` runs `wsl.exe -d <distro> -- cargo build -p
  bondsymphonic-daemon --release` and copies the binary to
  `target/daemon/bondsymphonic-daemon`. The IDE looks for it there in dev builds
  and next to the executable in packaged builds.
- `cargo test --workspace --exclude bondsymphonic-daemon` runs proto and IDE
  tests on Windows; `scripts/test-daemon.ps1` runs daemon tests inside WSL.

## 9. Testing strategy

| Level | Where it runs | What it covers |
|---|---|---|
| Unit | Windows and WSL, `cargo test` | Protocol round-trips; stream-json parsing against recorded Claude Code fixtures; run-config parsing and auto-detection; allowlist matching; diff and highlight span computation; IDE model state transitions |
| Daemon integration | WSL (and Linux CI) | Create a real workspace in a temp repo; run a sandboxed process; assert it cannot write outside the worktree, cannot connect to a non-allowlisted host, and that a TCP listener inside is reachable on the forwarded host port; merge, rebase, conflict, destroy |
| IDE smoke | Windows | Launch offscreen (`QT_QPA_PLATFORM=offscreen`), build the main window against a mock daemon, open a file, render a transcript |
| Manual acceptance | Windows + WSL | The v1 scenario in section 2 |

Tests that need the real Claude Code binary are marked `#[ignore]` and run
manually; everything else uses fixtures or a fake `claude` script that replays a
recorded stream.

## 10. Milestones (build order)

Each milestone is independently demonstrable. Plans are written per milestone.

1. **Foundation.** Toolchain setup script; workspace skeleton; proto crate with
   all types and round-trip tests; daemon that starts, prints its port, answers
   `hello` and `check_prereqs`; IDE that launches the daemon, connects, and shows a
   status bar. Empty main window with the four dock areas.
2. **Workspaces and sandbox.** `workspace.create/list/destroy`; git worktree
   creation with protected refs; bwrap sandbox with filesystem rules; `pty.*`;
   IDE shows groups, agent tabs, file tree, and a shell terminal inside the
   sandbox.
3. **Editor and changes.** `fs.*`; editor with tree-sitter highlighting and
   save; `workspace.changes/diff`; side-by-side diff view.
4. **Claude Code adapter.** stream-json process management; transcript events;
   permission bar; resume; history replay; cost in status bar; in-IDE login flow
   on the setup page for Claude Code and GitHub (terminal pane running the login
   command, login URL auto-opened in the browser, prerequisites re-checked on
   exit).
5. **Network and run.** CONNECT proxy with allowlist; Unix-socket port bridge;
   `bondsymphonic.toml` and auto-detection; run panel with URL and open-in-browser.
6. **Merge flow and persistence.** merge/rebase/squash with conflict reporting;
   `gh pr create`; discard; IDE layout and group persistence; reconnection.
7. **Hardening.** Footprint measurement; integration test suite complete;
   packaging script (windeployqt + daemon binary); user docs.
