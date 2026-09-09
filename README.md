# BondSymphonic

An IDE for orchestrating coding agents: each agent gets its own git worktree inside
its own OS-level sandbox, and the IDE makes running and testing each agent's web app
one click. Rust + Qt Widgets on Windows (Linux/macOS later); a Rust daemon inside
WSL2 owns worktrees, sandboxes, and processes.

Design: `docs/superpowers/specs/`. Plans: `docs/superpowers/plans/`.

## What works now

Milestone 2b: the IDE drives the daemon end to end, on real worktrees inside real
sandboxes.

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

Still placeholders: the editor and the diff/Changes views, run configurations, and every
agent adapter other than a plain terminal.

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

Signing in to Claude Code and GitHub happens inside the IDE from its setup page (planned
for Milestone 4). Until then the setup page lists what is missing.

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
  transcripts/        # agent transcripts (from Milestone 3)
  bin/                # daemon binaries installed into the distro
```

See `docs/daemon-protocol-notes.md` for driving the protocol by hand, outside the IDE.

## Tests

`scripts\env.ps1` must be dot-sourced first so the `bondsymphonic-ide` build and its tests can
find the Qt DLLs:

```powershell
. .\scripts\env.ps1
cargo test -p bondsymphonic-proto -p bondsymphonic-ide   # Windows; ide suites: lib, client, model, qobject_smoke, router, smoke
.\scripts\test-daemon.ps1                                 # daemon tests inside WSL (10 integration test files; unit tests: 15 on Windows, 18 on Linux)
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

### Test hooks

The IDE carries two environment-gated hooks for `crates/ide/tests/smoke.rs`, which runs
the real binary with `QT_QPA_PLATFORM=offscreen` against an in-process fake daemon. Both
are read once at startup and do nothing at all when unset, which is every ordinary run.

- `BS_DAEMON_ADDR` (a `host:port`) and `BS_DAEMON_TOKEN`: connect straight to that address
  with that handshake token instead of starting a daemon through `wsl.exe`. An address
  that is not a `host:port` is reported and ignored rather than guessed at.
- `BS_SMOKE_SCRIPT`: a comma-separated list of steps the controller performs once the
  connection is up — `create` (a workspace over `BS_SMOKE_REPO`, announced with the same
  signal New Agent produces), `open` (a PTY in it), `tree` (its root listing), `quit` (end
  the process with status 0 after letting the window settle).

The smoke test skips itself with a message when `QMAKE` is unset, since the IDE cannot
start without the Qt runtime on PATH.

## Layout

- `crates/proto` — protocol types shared by IDE and daemon
- `crates/daemon` — Linux daemon (worktrees, sandboxes, agents, runs)
- `crates/ide` — Qt Widgets IDE (Rust model + cxx-qt + thin C++ shell in `cpp/`)
- `scripts/` — setup, build, test helpers
