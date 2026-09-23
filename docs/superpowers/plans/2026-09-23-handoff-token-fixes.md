# Handoff — 2026-09-23, end of day

State of the tree and what is left, written as the user left. The daemon-side
work was run as parallel implementer subagents; two were still finishing when
this was written and their uncommitted edits may be in the working tree (see
"In flight"). `main` is pushed at `d809170`.

## Shipped today (all on `main`, each verified with a break-it on its tests)

| Commit | What |
| --- | --- |
| `ef9aba1` | daemon: `SetupAction::ClaudeLogout` / `GhLogout`; credentials seeding mirrors removals |
| `6ca042a` | IDE: Log out button on a passing Setup row; label fallthrough removed |
| `3b77eca` | IDE: full-width branch busy strip, space retained |
| `e29a8b2` | IDE: prerequisite re-check on agent exit / window activation; 500 ms auto-restart on tab selection with loop guard |
| `09b37c4` | IDE: the tab the window opened on also auto-starts, within three bounds |
| `afa260e` | daemon: unrestored workspaces read `Creating` (in memory); phase timing logs for restore/bring-up/`agent.start` |
| `ed8730d` | daemon: flaky permission-mode test fixed |
| `e4ca6d1` | IDE: `workspace.changes` / `repo.detect_run_configs` get 120 s; Changes list shows "Loading…" after 800 ms |
| `5cce54f` | IDE: opening-tab start waits for the daemon's `workspace.state` event; failed automatic start clears its booking |
| `66300bc` | daemon: prerequisite probes run concurrently |
| `84fdeb6` | IDE: log file `%LOCALAPPDATA%\BondSymphonic\logs\ide.log` (+ `ide.1.log`, `BS_LOG_FILE=0`) |
| `4fd9a14` | IDE: "Checking…" in the gate and Setup until the first prerequisite answer |
| `8b21eee` | IDE: splash screen until connected + prerequisites answered + list arrived |
| `d5cde45` | daemon: `fs.watch` registers off the runtime, in-flight guard, no walk on a v9fs root |
| `d809170` | daemon: `platform.claude.com` allowed in the sandbox (the CLI's OAuth token endpoint) |
| (next) | daemon: a login a workspace refreshed is written back to the host; emptied copies never; host with no login gets none |

## Root causes found today, for the record

1. **Cold `/mnt/c` git status**: 38–120 s+ over 9P; timeouts raised (`e4ca6d1`).
2. **Stale `ready` during restore**: the daemon listed unrestored workspaces as
   `ready`; a launch-time `agent.start` against a missing sandbox (`afa260e`,
   `5cce54f`).
3. **`fs.watch` froze the daemon**: recursive inotify registration of a 77k-dir
   `/mnt/c` checkout on the runtime worker; retries stacked until every worker
   was blocked and *every* request timed out (`d5cde45`). inotify over 9P never
   delivers events.
4. **OAuth**: each sandbox gets a copy of `~/.claude/.credentials.json`; a refresh
   inside a sandbox rotates the refresh token and invalidates the host's copy;
   later starts seed the dead token → "OAuth session expired and could not be
   refreshed"; the CLI then empties that sandbox's copy. Proven by a direct
   refresh answering `400 invalid_grant`. `claude auth status` only reads the
   file and still says logged in. The token endpoint (`platform.claude.com`) was
   also outside the sandbox allowlist (`d809170`).

**Immediate remedy for the user:** Settings → Setup → Log out of Claude Code →
Log in to Claude Code, then start agents. Recurs until the two in-flight fixes
below ship.

## In flight (uncommitted when written — check `git status`)

- **Token write-back** — SHIPPED (see table). Optional follow-ups: an end-to-end test of the
  `ended()`/`start` call sites (`agents/mod.rs` ~502 and ~853); the per-start warn for a stale
  emptied copy in a dead workspace, if it proves noisy.
- **CLI auth sentence → not logged in** (`ide-auth-failure`):
  `crates/ide/src/qobjects/app_controller.rs` (`auth_failure_in(detail)` rule,
  `claude_auth_override`), `MainWindow.cpp`, `TranscriptView.*`, tests in
  `qobject_smoke.rs` / `smoke.rs`, `docs/user-guide.md`. Override set on an
  agent exit whose detail says "OAuth session expired" / "could not be
  refreshed" / "Failed to authenticate" / "not logged in" / "invalid_grant";
  gate shows the CLI's sentence; `claude_auth` row rewritten to a cross; cleared
  only by a setup terminal (login) exiting or connection loss — **not** by a
  plain re-check, which is the case that lied today.

If either agent left a non-compiling tree: `git stash` or move the files to a
`wip/` branch before the next `launch.ps1`, which builds from the working tree.

## Still open, not started

- **`smoke.rs:96`** carries its own 12-entry allowlist fixture; add
  `platform.claude.com` for fidelity (comments say "the twelve defaults").
- **`in_place_sandbox::a_snapshot_notices_replaced_removed_and_rewritten_entries`**
  fails deterministically on Windows at the pushed tip (pre-existing; CI gates the
  daemon on Ubuntu only). Skip with `-- --skip a_snapshot_notices` locally.
- **Restore stall** seen once at 12:36 (2.5 min for `ws_be2db101`): did not
  reproduce; the phase lines (`grep 'restore:' ide.log`) will name the step if it
  recurs.
- `IGNORED` in `fs_watch.rs` skips `.git`, `node_modules`, `target` only; `bin`/`obj`
  build trees are still walked on ext4 (harmless there, but a cap was declined).
- The daemon's `claude_auth` cannot see a dead refresh token; the IDE-side
  override above is the chosen answer. A daemon-side validation was not designed.
- Verify inside the distro, not only on Windows, for daemon changes:
  `cargo test -p bondsymphonic-daemon --lib` and the relevant `--test` with
  `CARGO_TARGET_DIR=~/.bondsymphonic/target` (see `scripts/build-daemon.ps1`).

## How to verify the whole thing

```
# daemon (Windows; one known pre-existing failure, see above)
cargo test -p bondsymphonic-daemon --lib
# IDE
. .\scripts\env.ps1
cargo test -p bondsymphonic-ide --lib
cargo test -p bondsymphonic-ide --test client_tests
cargo test -p bondsymphonic-ide --test qobject_smoke     # 63 widget checks as of 8b21eee
cargo test -p bondsymphonic-ide --test smoke             # 22 as of 5cce54f
```
