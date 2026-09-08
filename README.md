# BondSymphonic

An IDE for orchestrating coding agents: each agent gets its own git worktree inside
its own OS-level sandbox, and the IDE makes running and testing each agent's web app
one click. Rust + Qt Widgets on Windows (Linux/macOS later); a Rust daemon inside
WSL2 owns worktrees, sandboxes, and processes.

Design: `docs/superpowers/specs/`. Plans: `docs/superpowers/plans/`.

## One-time setup (Windows 11)

1. Elevated PowerShell: `powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1`
   (Rust, VS 2022 Build Tools, CMake, Ninja, Python, Qt 6.9.2 msvc2022_64)
2. `powershell -ExecutionPolicy Bypass -File scripts\setup-wsl.ps1`
   (creates the `bondsymphonic` Ubuntu 24.04 distro, default user `bs`, with git, bubblewrap, rustup, Claude Code, gh)
3. Log in inside the distro: `wsl -d bondsymphonic -- bash -lc "claude"` (follow the prompts, then `/exit`)
   and `wsl -d bondsymphonic -- gh auth login`.

## Build and run

```powershell
. .\scripts\env.ps1            # QMAKE + PATH for this shell
.\scripts\build-daemon.ps1     # builds the Linux daemon inside WSL -> target\daemon\ (add -Debug for a debug build)
.\scripts\run-ide.ps1          # builds and starts the IDE; it installs and launches the daemon
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
