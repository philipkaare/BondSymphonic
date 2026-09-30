# BondSymphonic

An IDE for orchestrating coding agents: each agent gets its own git worktree inside
its own OS-level sandbox, and the IDE makes running and testing each agent's web app
one click. Rust + Qt Widgets on Windows (Linux/macOS later); a Rust daemon inside
WSL2 owns worktrees, sandboxes, and processes.

**[docs/user-guide.md](docs/user-guide.md) is the guide** — installing, the setup page,
creating workspaces, the Claude tab, runs, merging, troubleshooting, and the test hooks.

Design: `docs/superpowers/specs/`. Plans: `docs/superpowers/plans/`.

## What works now

- **Milestone 1 — Foundation.** Toolchain setup, the proto crate, a daemon that starts
  and answers `hello`, an IDE that launches it and shows a status bar.
- **Milestone 2 — Workspaces and sandbox.** Real git worktrees on `bs/<name>/work`
  inside real bubblewrap sandboxes, with PTYs and file access running inside them.
- **Milestone 3 — Editor and changes.** Tree-sitter highlighting, save through the
  daemon, a live Changes list against the base branch, and a side-by-side diff.
- **Milestone 4 — Claude Code adapter.** Structured transcripts, tool cards, permission
  prompts, cost in the status bar, and in-IDE login for Claude Code and GitHub.
- **Milestone 5 — Network and run.** An allowlisting CONNECT proxy per workspace, a
  port bridge to the Windows browser, `bondsymphonic.toml` run configs with detection.
- **Milestone 6 — Merge flow and persistence.** Merge, rebase, squash, Create PR and
  Discard; layout and groups persisted; reconnection when the daemon dies.
- **Milestone 7 — Hardening.** A version-gated IDE/daemon pair, atomic state writes and
  a data-dir instance lock, the daemon suite matched to its spec, an IDE job in CI,
  `package.ps1`, and this guide.

Agent backends: Claude Code and Codex, plus a plain terminal. Enable Codex under
Settings > Agents, install it there, then sign in to ChatGPT or store an OpenAI
API key. Each backend has its own sandbox, proxy, credentials and defaults.
See [agent backend setup](docs/user-guide.md#agent-backends).
Known limits are listed per area in the [user guide](docs/user-guide.md).

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

Signing in to Claude Code and GitHub happens inside the IDE, on its setup page — never
by typing a login command into a terminal yourself.

## Install from a package

If somebody handed you a `BondSymphonic-<version>-win64.zip` rather than the source:

```powershell
Expand-Archive BondSymphonic-0.1.0-win64.zip -DestinationPath C:\Tools
Get-ChildItem C:\Tools\BondSymphonic -Recurse | Unblock-File
powershell -ExecutionPolicy Bypass -File C:\Tools\BondSymphonic\install.ps1
C:\Tools\BondSymphonic\bondsymphonic-ide.exe
```

The middle two lines are what a clean Windows 11 machine needs. Everything extracted from
a downloaded zip carries the mark of the web, which blocks the scripts and can stop the
DLLs loading, and `Unblock-File` clears it. The default execution policy is `Restricted`,
which refuses to run `install.ps1` at all, so start it with `-ExecutionPolicy Bypass`
rather than changing the machine's policy.

`install.ps1` creates the `bondsymphonic` WSL2 distro if it is not already there —
Ubuntu 24.04 with git, bubblewrap, python3, Claude Code and gh, and **no Rust toolchain**,
because the daemon is already in the folder as a binary — then runs the runtime
provisioning over it and starts the IDE. It runs that provisioning every time, which is
what repairs a distro whose first setup was interrupted; it is idempotent, so a machine
that is already set up ends up unchanged.

**Close the IDE before re-running it.** Provisioning ends with
`wsl --terminate bondsymphonic`, so a re-run shuts the whole distro down: the daemon, every
sandbox, and with them your terminals, runs and agents. The IDE will reconnect and relaunch
the daemon, but the processes inside are gone.

**A second run is not instant either.** It still works through the apt packages and checks
whether Claude Code is installed, so expect minutes rather than seconds even when there is
nothing to do. Run it again to repair a distro, not as a quick way to start the IDE — for
that, run `bondsymphonic-ide.exe` directly. `-WhatIf` prints what it would do without
touching anything, and `-NoStart` provisions without launching.

WSL2 itself is the one prerequisite the package cannot install for you. If `wsl --version`
does not answer, run `wsl --install` in an elevated PowerShell and reboot first.

`bondsymphonic-ide.exe --version` prints the build and the daemon protocol it speaks, for
example `bondsymphonic-ide 0.1.0 (protocol 1)`. It answers before any window or daemon is
started, so it is the quickest check that a package unzipped correctly — it still needs the
Qt and CRT DLLs beside it, which the exe imports at load time, but it needs no display, no
platform plugin and no WSL.

To build a package from a source checkout: `. .\scripts\env.ps1` then
`.\scripts\package.ps1`. It builds the release IDE, builds the daemon in WSL, runs
`windeployqt` over the executable, and writes `dist\BondSymphonic\` plus the zip beside it.

## Developer setup

`launch.ps1` is these five in order; run them yourself to work on one part at a time.

1. `scripts\setup-windows.ps1` (elevated): Rust, VS 2022 Build Tools, CMake, Ninja, Python, Qt 6.9.2 msvc2022_64.
2. `scripts\setup-wsl.ps1`: creates the `bondsymphonic` Ubuntu 24.04 distro, default user `bs`, with git, bubblewrap, rustup, Claude Code, gh. `-Runtime` skips rustup, for a machine that only runs a packaged daemon.
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
`crates/ide/src/launcher.rs`. The daemon keeps its state under `~/.bondsymphonic` inside
the distro; `docs/daemon-protocol-notes.md` drives its protocol by hand, without the IDE.

### Tests

```powershell
. .\scripts\env.ps1                                   # required before cargo on Windows
cargo test --workspace
cargo test -p bondsymphonic-ide --features require-qt # Qt-less skips become failures
.\scripts\test-daemon.ps1                             # the daemon suite inside WSL
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) runs two jobs. **linux** (`ubuntu-24.04`) installs
bubblewrap, insists user namespaces work so the sandbox suites cannot silently skip, then
runs `cargo fmt --all --check`, the proto and daemon tests, and clippy over both.
**windows-ide** (`windows-latest`) installs Qt 6.9.2 msvc2022_64, points `QMAKE` at it,
and runs fmt, the proto and IDE tests with `--features bondsymphonic-ide/require-qt`, and
clippy over the IDE.

The environment-gated test hooks — `BS_DAEMON_ADDR`, `BS_SMOKE_SCRIPT`, `BS_SETTINGS_PATH`,
`BS_CLAUDE_BIN` and the rest — are documented in the
[user guide](docs/user-guide.md#for-developers).

## Layout

- `crates/proto` — protocol types shared by IDE and daemon
- `crates/daemon` — Linux daemon (worktrees, sandboxes, agents, runs)
- `crates/ide` — Qt Widgets IDE (Rust model + cxx-qt + thin C++ shell in `cpp/`)
- `scripts/` — setup, build, test helpers
- `docs/user-guide.md` — the user guide
