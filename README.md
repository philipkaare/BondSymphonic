# BondSymphonic

An IDE for orchestrating coding agents: each agent gets its own git worktree inside
its own OS-level sandbox, and the IDE makes running and testing each agent's web app
one click. Rust + Qt Widgets on Windows (Linux/macOS later); a Rust daemon inside
WSL2 owns worktrees, sandboxes, and processes.

Design: `docs/superpowers/specs/`. Plans: `docs/superpowers/plans/`.

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

## Tests

`scripts\env.ps1` must be dot-sourced first so the `bondsymphonic-ide` build and its tests can
find the Qt DLLs:

```powershell
. .\scripts\env.ps1
cargo test -p bondsymphonic-proto -p bondsymphonic-ide   # Windows; ide crate is 6 lib + 5 client tests
.\scripts\test-daemon.ps1                                 # daemon tests inside WSL (7 integration + 3/4 unit, platform-dependent)
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

## Layout

- `crates/proto` — protocol types shared by IDE and daemon
- `crates/daemon` — Linux daemon (worktrees, sandboxes, agents, runs)
- `crates/ide` — Qt Widgets IDE (Rust model + cxx-qt + thin C++ shell in `cpp/`)
- `scripts/` — setup, build, test helpers
