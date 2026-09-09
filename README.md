# BondSymphonic

An IDE for orchestrating coding agents: each agent gets its own git worktree inside
its own OS-level sandbox, and the IDE makes running and testing each agent's web app
one click. Rust + Qt Widgets on Windows (Linux/macOS later); a Rust daemon inside
WSL2 owns worktrees, sandboxes, and processes.

Design: `docs/superpowers/specs/`. Plans: `docs/superpowers/plans/`.

## What works now

Milestone 4: a New Agent tab can be a Claude Code agent running inside the workspace's
sandbox, with its transcript, its tool permissions and its cost in the IDE — and logging
in to Claude Code and GitHub happens on a setup page inside the IDE.

- **Claude Code tabs.** New Agent → Claude Code creates the workspace, then starts
  `claude` inside its sandbox in stream-json mode with a pinned flag set. The pane shows
  the conversation as it arrives: your prompts, the assistant's answers rendered as
  Markdown, one card per tool call with its input and its result, and a line per turn with
  the cost and how long it took. Model and permission mode are fields on the dialog, and
  an initial prompt is sent as soon as the agent is up. The tab's glyph follows the
  agent's state (idle, working, waiting, done) and the status bar shows the cost of the
  tab you are looking at. Interrupt abandons the current turn and leaves the agent alive;
  Stop ends the process. A pane replays the whole transcript from the daemon when it
  attaches, so an IDE restarted while the daemon kept running finds its tabs where it left
  them.
- **Tool permissions.** When the agent asks to use a tool, a bar appears above the prompt
  box with the tool's name, what it wants to run, Allow, Deny, and "always allow this tool
  for this session". The answer travels back to `claude` as a `control_response`. Which
  tool is allowed is decided in the IDE, not in the pane: a ticked box means later
  requests for that same tool are answered without asking again.
- **Signing in, inside the IDE.** Help > Setup… lists every prerequisite with a tick or a
  cross. A failing one that the IDE can fix carries a button — "Log in to Claude Code",
  "Log in to GitHub", "Install Claude Code", "Install GitHub CLI" — and pressing it runs
  that command in a terminal pane on the page, on the host rather than in a sandbox, since
  a login has to write to your home directory. A login URL that appears in that output is
  opened in your browser for you. When the command exits, the prerequisites are re-checked;
  when they all pass, the page steps aside. The page also appears by itself at startup
  when something is missing that would stop a workspace being created.
- **API key.** File > Settings… stores an Anthropic API key in the Windows credential
  store (never in a config file), and it is passed to the daemon with each agent start.
  `settings.json` records only whether a key is set.

Milestone 3: the IDE drives the daemon end to end on real worktrees inside real
sandboxes, and edits, saves and diffs the files in them.

- **Workspaces.** New Agent creates a workspace on a chosen repository and files its tab
  under a group; the tab's status word follows the daemon's `workspace.state` events, and
  its context menu destroys the workspace, with a force option for a dirty worktree. The
  tab layout is rebuilt from the daemon's workspace list on each connect rather than saved
  to disk.
- **Terminals.** The agent pane and the bottom Terminal tab each run their own PTY inside
  that workspace's bubblewrap sandbox, with colours, text attributes, resize and 10,000
  lines of scrollback. Two workspaces keep independent terminals, and destroying one
  leaves the other running. When the daemon has to drop events, the affected screens show
  an `[output dropped]` marker instead of quietly losing bytes.
- **Explorer.** The Files tab lists the workspace's worktree one directory at a time, as
  they are expanded, and the toolbar's Refresh reloads it in place.
- **Editing.** Double-clicking a file in the Files tab opens it in a tab of the centre
  pane, with tree-sitter syntax highlighting for Rust, JavaScript, TypeScript, TSX,
  Python, JSON, TOML, YAML, HTML, CSS, Markdown, Bash, C, C++ and Go. Other extensions
  open as plain text. Typing marks the tab with a dot; File > Save (Ctrl+S) writes the
  file back through the daemon into the sandboxed worktree, and Save All writes every
  dirty tab. A binary file, or one over 4 MiB, opens as a read-only notice instead.
  Closing a tab with unsaved edits asks first, and so does closing the window: save all,
  discard, or stay.
- **External changes.** While a file is open the IDE watches it. A change made on disk by
  an agent or a shell reloads an unmodified tab silently, keeping the caret and the scroll
  position; on a tab with unsaved edits a bar offers reload or keep.
- **Changes and diffs.** The Explorer's Changes tab lists the files that differ from the
  workspace's base branch with their status and +/- counts, and follows the worktree
  without pressing Refresh. Double-clicking a row opens a side-by-side diff: aligned rows,
  green and red tints, two line-number columns, synchronised scrolling in both axes and
  the same syntax highlighting as the editor.

Known limits in Milestone 4:

- The stream-json fixtures the parser is tested against are **synthetic**. Every flag the
  adapter passes was verified accepted by Claude Code 2.1.263, but no line in the fixtures
  came out of a real `claude`. `scripts/record-claude-stream.sh` records a real turn beside
  them; it needs someone logged in, and it will not log you in itself.
- An agent does not survive a daemon restart. The session id `claude --resume` would need
  is kept in memory only, and the IDE never sends one, so a restarted daemon starts a fresh
  conversation. Resume lands in Milestone 6.
- "Always allow this tool for this session" lives in the tab's transcript in the IDE. It is
  never sent to the daemon and never written to disk, and it is forgotten when that pane
  re-attaches — reopening the workspace, or pointing the tab at another agent.
- The status bar shows the cost of the tab you are looking at, not of every agent running.
- The permission bar answers with the tool input `claude` proposed. It never edits it, so
  the "allow with a changed input" path of the protocol is unused and untested.

Known limits in Milestone 3:

- A file over 512 KiB opens without highlighting, because highlighting re-runs over the
  whole buffer after every edit rather than incrementally.
- Highlighting is that same full re-pass per edit, computed lazily on the next span query.
- An open diff does not reload when the file changes on disk. Close and re-open it.
- A diff of a file over 4 MiB compares the first 4 MiB of each side; the pane's header
  says so, separately from the note it shows when the alignment runs out of time.
- Languages embedded in another (`<script>` in HTML, fenced code in Markdown) are not
  highlighted: tree-sitter injections are not wired up.

Still placeholders: run configurations, and every agent adapter other than Claude Code and
a plain terminal.

## Quick start (Windows 11)

```powershell
.\launch.ps1
```

That single script installs anything missing (the Windows toolchain via an elevated
window, then the `bondsymphonic` WSL distro), builds the Linux daemon inside WSL, and
starts the IDE, which installs the daemon into the distro and connects to it. The first
run downloads several gigabytes and takes a while; later runs are incremental. Switches:
`-Debug` (debug daemon build), `-Release` (release IDE build), `-SkipDaemon` (reuse the
last daemon binary), `-NoSetup` (fail instead of installing prerequisites).

Signing in to Claude Code and GitHub happens inside the IDE, on its setup page: the page
appears by itself when something needed is missing, and Help > Setup… opens it any time.
Each failing item the IDE can fix has a button that runs the command in a terminal on the
page, opens any login URL in your browser, and re-checks when it finishes.

## The pieces behind `launch.ps1`

1. `scripts\setup-windows.ps1` (elevated): Rust, VS 2022 Build Tools, CMake, Ninja, Python, Qt 6.9.2 msvc2022_64.
2. `scripts\setup-wsl.ps1`: creates the `bondsymphonic` Ubuntu 24.04 distro, default user `bs`, with git, bubblewrap, rustup, Claude Code, gh.
3. `scripts\env.ps1`: dot-source for `QMAKE` and PATH in a dev shell.
4. `scripts\build-daemon.ps1`: builds the Linux daemon inside WSL into `target\daemon\` (`-Debug` for a debug build).
5. `scripts\run-ide.ps1`: `cargo run -p bondsymphonic-ide` with the env set.

```powershell
. .\scripts\env.ps1
.\scripts\build-daemon.ps1
.\scripts\run-ide.ps1
```

The IDE side (`crates/ide`) uses [cxx-qt](https://github.com/KDAB/cxx-qt) 0.10 to bind Rust to
Qt Widgets; the code that installs, starts, and connects to the daemon over WSL lives in
`crates/ide/src/launcher.rs`.

## Daemon

Milestone 2a: the daemon manages real git worktrees and bubblewrap sandboxes, not just
protocol scaffolding. `workspace.create` adds a worktree on branch `bs/<name>/work` and starts
a sandbox in front of it: the sandbox's root filesystem is read-only, only the worktree, the
workspace's own object directory and its cache are writable, the shared `refs/heads` and object
store stay read-only, the sandbox gets its own PID namespace, and it has no network access.
Commits made inside the sandbox land in that private object directory and reach the daemon
through a git alternate, so the agent's own branch is isolated without the daemon losing sight
of it. `workspace.list/get/status/destroy` manage that lifecycle (destroying a dirty workspace
needs `force`), and `pty.open/write/resize/close` and `fs.list_dir/read_file/write_file` run
inside the sandbox once it exists. Pass `--no-sandbox` to `bondsymphonic-daemon` to run every
workspace's processes directly on the host instead of in bubblewrap (development only). The
backend choice itself is a compile-time OS check, not a probe: on any non-Linux host the daemon
always uses the unsandboxed noop backend, and on Linux it always uses bubblewrap unless
`--no-sandbox` is given. A Linux host whose bubblewrap doesn't actually work (missing binary,
unprivileged user namespaces disabled, ...) fails at sandbox start rather than silently falling
back; the `sandbox` item of `system.check_prereqs` explains why.

The daemon keeps its state under its data directory (default `~/.bondsymphonic`, override with
`--data-dir`):

```
~/.bondsymphonic/
  workspaces.json   # the workspace registry
  worktrees/<id>/    # one git worktree per workspace
  objects/<id>/      # that workspace's private git object directory
  homes/<id>/        # $HOME inside the sandbox
  caches/<id>/       # writable cache, mounted at $HOME/.cache inside the sandbox
  run/<id>/          # the sandbox's exec socket and other runtime files
  transcripts/        # one NDJSON transcript per agent, replayed by agent.history
  bin/                # daemon binaries installed into the distro
```

See `docs/daemon-protocol-notes.md` for driving the protocol by hand, outside the IDE.

## Tests

`scripts\env.ps1` must be dot-sourced first so the `bondsymphonic-ide` build and its tests can
find the Qt DLLs:

```powershell
. .\scripts\env.ps1
cargo test --workspace                                    # Windows; ide suites: lib, client, connection, diff, editor, model, qobject_smoke, router, smoke, transcript
.\scripts\test-daemon.ps1                                 # daemon tests inside WSL (14 integration test files; unit tests: 45 on Windows, 48 on Linux)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Test hooks

The IDE carries two environment-gated hooks for `crates/ide/tests/smoke.rs`, which runs
the real binary with `QT_QPA_PLATFORM=offscreen` against an in-process fake daemon. Both
are read once at startup and do nothing at all when unset, which is every ordinary run.

- `BS_DAEMON_ADDR` (a loopback `host:port`) and `BS_DAEMON_TOKEN`: connect straight to that
  address with that handshake token instead of starting a daemon through `wsl.exe`. Only
  loopback addresses are accepted, since the protocol carries file contents and PTY
  traffic; anything else, and anything that is not a `host:port` at all, is reported and
  ignored rather than guessed at.
- `BS_SMOKE_SCRIPT`: a comma-separated list of steps the controller performs once the
  connection is up. `create` and `create_claude` make a workspace over `BS_SMOKE_REPO`,
  announced with the same signal New Agent produces, as a terminal tab or a Claude tab;
  `open_agent` starts a Claude agent in it and announces that; `send` puts a prompt to the
  agent; `allow` allows the pending tool call through the window, the same way a
  notification will; `stop` stops the agent; `open` opens a PTY; `tree` lists the
  workspace root; `open_file` and `open_diff` ask the window to open `README.md` as an
  editor tab and as a diff tab, through the same controller signals the Explorer's
  double-click emits; `close` closes the script's PTY; `destroy` destroys the workspace;
  `quit` ends the process with status 0 after letting the window settle.

The smoke test skips itself with a message when `QMAKE` is unset, since the IDE cannot
start without the Qt runtime on PATH.

The daemon carries three, all read from its own environment, all inert when unset. No test
needs a Claude login or a network.

- `BS_CLAUDE_BIN`: the command to run instead of `claude`, split the way a shell would, so
  a stand-in can be an interpreter plus a script
  (`python3 /opt/fake/fake_claude.py`). It also suppresses the "untested Claude Code
  version" warning, since a stand-in's version says nothing about the protocol.
- `FAKE_CLAUDE_FIXTURE`: which NDJSON stream `crates/daemon/tests/fixtures/fake_claude.py`
  replays. Inside a bubblewrap sandbox this never arrives — the sandbox builds the agent's
  environment from the spec alone — so the fake also falls back to `fixture.ndjson` beside
  itself, which is how you point it at a stream in a real sandbox.
- `FAKE_CLAUDE_ECHO_DELAY`: seconds the fake holds an echoed turn open before answering,
  for testing interrupt against something that is actually still working.

The daemon reads these where it spawns an agent, so they have to be in the *daemon's*
environment. When the IDE launches it through `wsl.exe`, name them in `WSLENV`
(`WSLENV=BS_CLAUDE_BIN/u:FAKE_CLAUDE_FIXTURE/u`) before starting the IDE, and put the fake
somewhere the sandbox can see — `/opt/...`, not under `/home`, which the workspace's own
home is mounted over.

## Layout

- `crates/proto` — protocol types shared by IDE and daemon
- `crates/daemon` — Linux daemon (worktrees, sandboxes, agents, runs)
- `crates/ide` — Qt Widgets IDE (Rust model + cxx-qt + thin C++ shell in `cpp/`)
- `scripts/` — setup, build, test helpers
