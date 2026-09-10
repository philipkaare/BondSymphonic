# Milestone 7: Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the Milestone 1–6 build into something a person can install and trust: a version-gated IDE/daemon pair, the deferred correctness items closed, the daemon integration suite matching its spec, an IDE job in CI, a packaging script that produces a runnable `dist\BondSymphonic\`, a comparable footprint row, and a user guide.

**Architecture:** No new subsystems. Every task either closes a deferred finding recorded in the M2–M6 reviews, pins a spec claim with a test, or wraps the existing build in scripts and docs. The one protocol change (Task 1) is additive and lands first so every later task builds on it.

**Tech Stack:** Rust 1.98 (MSVC on Windows, GNU in the `bondsymphonic` WSL2 distro), cxx-qt 0.10 + Qt 6.9.2 msvc2022_64, `windeployqt`, PowerShell 5.1 scripts, GitHub Actions (`ubuntu-24.04`, `windows-latest`, `jurplel/install-qt-action`).

**Spec:** `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` (§8 build workflow, §9 testing, §10 milestone 7), `-daemon-design.md` (§3 startup, §13 testing), `-ide-design.md` (§5 launcher, §13 footprint, §14 testing).

## Global Constraints

- Rust edition 2021, `cargo fmt` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, zero MSVC warnings in the C++ shell.
- Every daemon test skips with a printed reason (never fails) when `bwrap` is unavailable; every IDE test that needs the Qt runtime skips with a printed reason when `QMAKE` is unset — **a skip is reported as `SKIP` in the test output and counted in a `skipped` line, never as a silent pass**.
- Tests never touch the user's real `%APPDATA%\BondSymphonic` (`BS_SETTINGS_PATH` / `BS_STATE_PATH`), never the daemon's real data dir (each test its own `--data-dir`), never a real Claude/GitHub login, never a real remote, never the user's live workspace `ws_eebdd832`.
- No desktop input injection by any agent, ever. GUI verification is by env-gated self-test hooks that create their own throwaway workspace, assert its id before every action and resolve every widget through that workspace's own page, own-window `QWidget::grab()`, or the offscreen smoke test.
- Additive protocol changes only (`#[serde(default)]`); the IDE and daemon are shipped as a pair and Task 1 makes a mismatched pair fail with a clear message rather than a decode error.
- Commit trailers: `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01DcYV4WREYNvdE2MsWATepg`.
- Commands: PowerShell 5.1 (no `&&`); `. .\scripts\env.ps1` before cargo on Windows; `.\scripts\test-daemon.ps1` for the WSL suite; `.\scripts\build-daemon.ps1` then `.\launch.ps1` for the real pair.

---

### Task 1: Protocol version gate in `hello` (proto + daemon + IDE)

**Files:**
- Modify: `crates/proto/src/lib.rs` (`PROTOCOL_VERSION` stays `1`, now read), `crates/proto/src/request.rs` (`HelloParams { token, #[serde(default)] protocol_version: Option<u32> }`, `HelloResult { daemon_version, #[serde(default)] protocol_version: Option<u32> }`; also, so that Task 2 needs no proto edit of its own: `DetectRunConfigsResult { .., #[serde(default)] warnings: Vec<String> }`, produced empty by the daemon until Task 2 fills it), `crates/proto/tests/roundtrip.rs`
- Modify: `crates/daemon/src/server/dispatch.rs` (answer with `PROTOCOL_VERSION`; a client that sends a different version gets `RpcError { code: InvalidParams, message: "protocol version N is not supported (daemon speaks M)", data: {"reason":"protocol_mismatch","daemon":M,"client":N} }` and the connection is closed after the reply), `crates/daemon/tests/server_integration.rs`
- Modify: `crates/ide/src/client/mod.rs` (send `PROTOCOL_VERSION`; compare the answer; `ClientError::ProtocolMismatch { daemon, client }`), `crates/ide/src/qobjects/app_controller.rs` (`connect_once`: on `ProtocolMismatch` set `ConnectionState::Error` with text "daemon speaks protocol M, this IDE needs N — reinstalling the daemon" and, when the launcher owns the daemon, run the launcher's install step once and retry immediately; on a second mismatch stay in `Error` and do not enter the backoff loop), `crates/ide/src/launcher.rs` (`install_daemon(spec) -> Result<()>` made public and idempotent — it already hashes the binary), `crates/ide/tests/client_tests.rs`, `crates/ide/tests/reconnect_tests.rs`
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` §6 (hello carries `protocol_version`; mismatch is a typed error; an absent field means "a pre-M7 peer" and is treated as version 1)

**Interfaces:**
- Produces: `ClientError::ProtocolMismatch { daemon: u32, client: u32 }`; `AppController` status text `"daemon: protocol mismatch (daemon M, IDE N)"`; `ConnectionState::Error` code 5 unchanged.
- Consumes: `launcher::install_daemon` (exists as a private step of `launch`; expose it).

- [ ] **Step 1: Write the failing tests.** `roundtrip.rs`: `HelloParams`/`HelloResult` decode with and without the new field. `server_integration.rs`: a hello with `protocol_version: 99` gets the mismatch error with `data.reason == "protocol_mismatch"` and the socket closes; a hello without the field is accepted. `client_tests.rs`: the fake daemon answering `protocol_version: 2` makes `DaemonClient::connect` return `ProtocolMismatch { daemon: 2, client: 1 }`. `reconnect_tests.rs`: with `BS_DAEMON_ADDR` set (no launcher), the IDE's status text reaches `daemon: protocol mismatch (daemon 2, IDE 1)` and the log shows no `reconnecting (attempt` line.
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** proto → daemon → IDE as described.
- [ ] **Step 4: Build and test**: Windows `cargo test --workspace`; WSL `.\scripts\test-daemon.ps1`.
- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates docs
git commit -m "feat(proto,daemon,ide): hello carries the protocol version; a mismatched pair fails with a reason and the IDE reinstalls the daemon once"
```

---

### Task 2: Daemon hardening (deferred findings + integration suite per spec §13)

**Files:**
- Create: `crates/daemon/src/util/atomic.rs` (`pub fn write_atomic(path, bytes) -> io::Result<()>`: sibling temp file with a unique suffix (`.<pid>.<counter>.tmp`), `sync_all`, rename, best-effort directory fsync on Unix)
- Modify: `crates/daemon/src/workspace/registry.rs`, `crates/daemon/src/agents/persist.rs` (use `write_atomic`); `crates/daemon/src/git/changes.rs` and `crates/daemon/src/git/merge.rs` (every path-listing git call gets `-c core.quotePath=false`; a test with a UTF-8 file name asserts the raw name comes back); `crates/daemon/src/git/worktree.rs` (`create`/`remove` use `daemon_git()` so `post-checkout`/`reference-transaction` hooks of the user's repo do not run; test with a hook that records its invocation); `crates/daemon/src/main.rs` + `crates/daemon/src/daemon.rs` (single-instance lock: `<data_dir>/daemon.lock` taken with an advisory exclusive lock via `fs2`/`rustix` flock at startup; a second daemon on the same data dir exits 2 with "another bondsymphonic-daemon owns <data_dir>"; the IDE's launcher already restarts on exit — document in §3); `crates/daemon/src/runs/config.rs` (a `[[run]]` entry without `port` is a **per-entry** parse error reported in `repo.detect_run_configs`'s `warnings` list, and the remaining entries still load — today the whole file is discarded); `crates/daemon/tests/run_integration.rs` (the ready-probe tests wait on the run's own `ready` event with a 60 s ceiling instead of a fixed 5 s sleep-and-check, which is the flake M6 saw under load; production code unchanged), `crates/daemon/tests/common/mod.rs` (`PortGuard` kills by the PID it started, never by port)
- Modify (suite completeness, spec §13): `crates/daemon/tests/sandbox_integration.rs` (assert present or add: `touch /etc/x` fails; `touch $HOME/x` succeeds; `git update-ref refs/heads/main <sha>` inside the sandbox fails; `git update-ref refs/heads/bs/<name>/work <sha>` inside succeeds), `crates/daemon/tests/network_integration.rs` (assert present or add: a host TCP listener is unreachable directly from the sandbox; reachable through the proxy only once allowlisted; `python3 -m http.server` inside the sandbox is reachable on the bridged host port), `crates/daemon/tests/workspace_integration.rs` (the §13 chain: create → commit inside via sandboxed git → `changes` lists it → `merge` → base contains it → objects readable with no alternates → destroy cleans worktree, objects, home, cache, run dir, records and transcripts — one test that walks the whole chain and asserts each directory is gone)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§3 lock file; §5.3 quotePath; §5.2 hooks for create/remove; §10.1 per-entry errors; §13 wording "objects absorbed" instead of "repack")

**Interfaces:**
- Consumes: `DetectRunConfigsResult.warnings` (added by Task 1, empty until here).
- Produces: the daemon fills `warnings` with one line per ignored `[[run]]` entry (the IDE shows them in the run panel's tooltip in Task 3); daemon exit code 2 for the instance lock.

- [ ] **Step 1: Write the failing tests** for each bullet above (one test per bullet; the spec-§13 tests are added only where `grep` shows the assertion is missing — record which were already present in the report).
- [ ] **Step 2: Run to verify failure** (WSL for the sandbox ones).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Build and test** on both hosts; run the WSL suite three times in a row to check the run_integration flake is gone.
- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon crates/proto docs
git commit -m "fix(daemon): atomic writers fsync, quotePath off, no repo hooks on worktree create/remove, data-dir instance lock, per-entry run config errors; integration suite matches the spec"
```

---

### Task 3: IDE hardening (deferred findings)

**Files:**
- Modify: `crates/ide/tests/smoke.rs`, `crates/ide/tests/reconnect_tests.rs`, `crates/ide/tests/restore_tests.rs`, `crates/ide/tests/qobject_smoke.rs` (when `QMAKE` is unset: print `SKIP: <reason>` and return **only** under `cfg!(not(feature = "require-qt"))`; add a cargo feature `require-qt` that turns the skip into a panic — CI's Windows job enables it in Task 5 so a missing Qt can never pass as green)
- Modify: `crates/ide/src/model/app_state.rs` (`Workspaces::from_persisted`: group ids derived from a stable per-name counter kept in `Workspaces`, not the positional index, so renames and drags after a restore keep ids the C++ side already holds; test), `crates/ide/src/qobjects/transcript_model.rs` (`restart_options` prefers the transcript's last session id, then falls back to `AgentSummary.session_id` carried on the tab — the daemon holds the resumable id even when the transcript is damaged; test), `crates/ide/cpp/ExplorerDock.cpp` (refresh the workspace summary on tab change and right before the Discard box opens, not on every `changesLoaded`), `crates/ide/src/client/mod.rs` (timeout comments say "heuristic, above the daemon's typical worst case" rather than "derived"), `crates/ide/cpp/RunPanel.cpp` + `crates/ide/src/qobjects/run_panel.rs` (show Task 2's `warnings` as the config combo's tooltip and a status line "1 run config ignored: …")
- Modify (M4 deferrals that are cheap now): `crates/ide/src/qobjects/app_controller.rs` + `crates/ide/cpp/MainWindow.cpp` (a permission prompt raised by an agent in a **background** tab shows a dot on that tab and a status-bar hint "agent-2 is waiting for permission" — `GroupModel::setWorkspaceAttention(ws, text)`/`clearWorkspaceAttention`, cleared when the reply leaves; test in `qobject_smoke.rs`), `crates/ide/src/model/transcript.rs` (items above 2,000 per agent are collapsed into one "load earlier (N)" block per IDE spec §13; the block expands on click; `model_tests.rs` asserts the count of live items after 2,500 fixture messages is ≤ 2,001)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§13 the 2,000-item claim now true; §14 `require-qt`)

- [ ] **Step 1: Write the failing tests** (one per bullet).
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Build and test**: `cargo test -p bondsymphonic-ide` and `cargo test -p bondsymphonic-ide --features require-qt`; zero MSVC warnings.
- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide docs
git commit -m "fix(ide): Qt-less tests skip loudly, stable group ids, resume from the daemon's session id, summary on demand, background-tab permission attention, transcript collapse above 2,000 items"
```

---

### Task 4: Packaging script (`windeployqt` + daemon binary) and packaged-mode launcher

**Files:**
- Create: `scripts/package.ps1` (param `-Version` defaults to the workspace version from `Cargo.toml`; steps: `cargo build -p bondsymphonic-ide --release`; `.\scripts\build-daemon.ps1` (release) unless `-SkipDaemon`; `New-Item dist\BondSymphonic`; copy `target\release\bondsymphonic-ide.exe`; run `<Qt>\bin\windeployqt.exe --release --no-translations --no-system-d3d-compiler --no-opengl-sw --no-quick-import dist\BondSymphonic\bondsymphonic-ide.exe`; copy `target\daemon\bondsymphonic-daemon` next to the exe; copy `scripts\setup-wsl.ps1`, `scripts\setup-wsl.sh` and a generated `install.ps1` (creates the distro if missing, then starts the exe); write `dist\BondSymphonic\VERSION`; `Compress-Archive` to `dist\BondSymphonic-<version>-win64.zip`; print the folder size), `scripts/setup-wsl.ps1` (new switch `-Runtime`: install git, bubblewrap, python3, Claude Code and gh **without** rustup — end users do not build the daemon)
- Modify: `crates/ide/src/launcher.rs` (packaged lookup: `local_daemon_binary` = `<exe dir>\bondsymphonic-daemon` when it exists, else `target\daemon\…` in dev builds; `BS_DAEMON_BINARY` env override for tests; unit test of the resolution order with temp dirs), `crates/ide/src/main.rs` (`--version` prints `bondsymphonic-ide <version> (protocol N)` and exits 0)
- Create: `crates/ide/tests/packaged_smoke.rs` (skipped unless `BS_PACKAGED_EXE` points at a built `dist\BondSymphonic\bondsymphonic-ide.exe`: runs it with `--version` and asserts the string; then runs it offscreen with `BS_SMOKE_SCRIPT=quit` against a minimal fake daemon written in this file (answers `hello`, `system.check_prereqs` and `workspace.list` only — do not touch `tests/smoke.rs`, Task 3 edits it) and asserts `hello` + `workspace.list` + exit 0 — this proves the deployed Qt DLLs load)
- Modify: `.gitignore` (`/dist/`), `README.md` ("Install from a package" section: unzip, run `install.ps1` once, then `bondsymphonic-ide.exe`)

- [ ] **Step 1: Write the failing tests** (launcher resolution order; `--version`; `packaged_smoke.rs` skips with a reason when the env var is unset).
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** the script and the launcher change.
- [ ] **Step 4: Build the package** with `.\scripts\package.ps1`, run `cargo test -p bondsymphonic-ide --test packaged_smoke` with `BS_PACKAGED_EXE` set, and record the folder size and DLL list in the report. Then start the packaged exe once against the real daemon with `BS_SMOKE_SCRIPT=quit` (it connects, lists the user's workspace without touching it, quits) and record the log line `daemon: connected`.
- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add scripts crates/ide README.md .gitignore
git commit -m "feat: package.ps1 builds dist\\BondSymphonic with windeployqt and the daemon binary; the launcher finds the daemon next to the exe"
```

---

### Task 5: CI for the IDE crate and workspace-wide gates

**Files:**
- Modify: `.github/workflows/ci.yml`:
  - `linux` job: add `cargo fmt --all -- --check`, `cargo clippy -p bondsymphonic-proto -p bondsymphonic-daemon --all-targets -- -D warnings`, `cargo test -p bondsymphonic-proto -p bondsymphonic-daemon` (bwrap present, so the sandbox suites run; the `bwrap` user-namespace check must pass on `ubuntu-24.04` — if AppArmor blocks it, `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` first).
  - New `windows-ide` job on `windows-latest`: `jurplel/install-qt-action@v4` with `version: 6.9.2`, `arch: win64_msvc2022_64`, `cache: true`; `Swatinem/rust-cache@v2`; set `QMAKE` to `<Qt>\bin\qmake.exe`; `cargo test -p bondsymphonic-proto -p bondsymphonic-ide --features bondsymphonic-ide/require-qt`; `cargo clippy -p bondsymphonic-ide --all-targets -- -D warnings`; `cargo fmt --all -- --check`. Set `QT_QPA_PLATFORM=offscreen` for the job.
  - Keep `windows-proto` folded into `windows-ide`.
- Modify: `README.md` ("Tests" section: what CI runs where).

- [ ] **Step 1: Write the workflow.**
- [ ] **Step 2: Push a branch and watch the run** (`gh run watch`); fix until both jobs are green. Record the wall time of the Windows job; if above 25 minutes, split the IDE tests into a matrix by test binary.
- [ ] **Step 3: Commit**

```bash
git add .github README.md
git commit -m "ci: Windows job builds and tests the IDE against Qt 6.9.2; fmt and clippy gates on both hosts"
```

---

### Task 6: Footprint, user guide, end-to-end on the packaged build

**Files:**
- Create: `docs/user-guide.md` (Install; First run and the setup page (Claude Code and GitHub logins happen there — never in the terminal); Creating an agent workspace (repo, name, group, adapter); The Claude tab (transcript, tool cards, permissions, cost, Restart with resume); Terminal tabs; Files and Changes (editor, diff, Refresh in the Explorer); Running the web app (run configs, `bondsymphonic.toml`, port override, allowlist toast); Finishing a workspace (Merge/Rebase/Squash, Create PR, Discard, Close group); What persists across restarts; What happens when the daemon restarts (Reopen, Restart agent); Troubleshooting (protocol mismatch, `daemon.log`, `--no-sandbox`, WSL distro health, "another daemon owns the data dir"); Test hooks moved here from README under "For developers")
- Modify: `README.md` (trim to: what it is, quick start, install from a package, developer setup, link to the guide; keep "What works now" as a milestone list of one line each)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` §13 (a Milestone 7 row measured the M2b–M5 way: **release** build of the packaged IDE, two workspaces, one Claude tab with history, one run, three editors, one diff, idle 60 s — and a second row for the debug build so it is comparable with M6's; both against the real daemon), `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` §9 (CI table updated), §10 (milestone 7 marked done with the date)
- Modify: `docs/daemon-protocol-notes.md` (hello `protocol_version`, `agent_records`, the M6 methods)

- [ ] **Step 1: Footprint** as above, from the packaged build (`dist\BondSymphonic\bondsymphonic-ide.exe` launched with `BS_DAEMON_BINARY` unset so it installs its own daemon copy) and from the debug build; measure with `Get-Process bondsymphonic-ide | select WS,PM` and `wsl -d bondsymphonic -- ps -o rss= -C bondsymphonic-daemon` after 60 s idle. The workspaces are throwaways created by an env-gated hook that follows the Global Constraints (own workspace, id asserted, page-scoped widgets); the user's workspace is listed and not touched; the hook is removed before the commit.
- [ ] **Step 2: Docs** as listed; every claim in the guide checked against the code (the reviewer will spot-check ten).
- [ ] **Step 3: End-to-end on the packaged build**: with the hook from Step 1, in one IDE session: create a workspace, open a terminal tab, kill the daemon (`wsl -d bondsymphonic -- pkill -f bondsymphonic-daemon`), see reconnect + Reopen, merge the workspace into its throwaway repo, destroy it, quit through `closeEvent`; assert no daemon was relaunched after the quit (`wsl -d bondsymphonic -- pgrep -f bondsymphonic-daemon` empty 5 s after exit). Screenshot `<scratchpad>\m7-final.png`.
- [ ] **Step 4: Commit**

```bash
git add docs README.md
git commit -m "docs: user guide, README trimmed, milestone 7 footprint on the packaged build"
```

---

## Milestone 7 exit criteria

- A mismatched IDE/daemon pair fails with a clear status message and the IDE reinstalls its daemon once; `hello` carries the protocol version both ways.
- Registry, agent records and `state.json` are written atomically with fsync; conflict and change paths are never octal-escaped; the daemon refuses to start twice on one data dir; a bad `[[run]]` entry is reported, not silently dropped; worktree create/remove run no repository hooks.
- The daemon integration suite contains every scenario daemon spec §13 lists; three consecutive WSL runs pass.
- Qt-less IDE tests report `SKIP` and fail under `--features require-qt`; CI runs the IDE tests on Windows with Qt 6.9.2 and fmt/clippy on both hosts, green.
- `.\scripts\package.ps1` produces `dist\BondSymphonic\` whose exe runs `--version`, passes the packaged smoke against the fake daemon, and connects to the real daemon; `install.ps1` creates the distro without a Rust toolchain.
- Transcripts collapse above 2,000 items; background-tab permissions are signalled; Restart resumes from the daemon's session id when the transcript cannot supply one.
- `docs/user-guide.md` exists and README points at it; §13 has a packaged-build footprint row measured after 60 s idle; the end-to-end on the packaged build passed with no daemon left running after quit.
- The v1 acceptance scenario (overview §2) is written up as a checklist at the end of the user guide for the user to run after logging in — it needs a real Claude Code login, which agents never perform.
