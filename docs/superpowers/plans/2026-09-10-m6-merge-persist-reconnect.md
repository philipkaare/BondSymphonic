# Milestone 6: Merge Flow, Persistence and Reconnection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace can be merged, rebased or squashed back into its base branch, pushed as a GitHub PR, or discarded, from the Changes tab and from a group's close menu; groups, layout, open editors, recent repos and port overrides survive an IDE restart; agents survive a daemon restart as resumable history; and when the daemon dies the IDE relaunches it with backoff and re-syncs instead of going dead.

**Architecture:** The daemon gains `git/merge.rs` (merge/rebase/squash run by the daemon in the main repo or a temporary base worktree, never in a sandbox), `git/pr.rs` (`git push -u` + `gh pr create`), an `agents.json` record so `agent.history` and `WorkspaceInfo.agents` survive restarts, an optional per-start `port` override on `run.start`, and honouring `[claude] settings`. The IDE gains `model/persistence.rs` (`state.json`: groups, active tab, open editors, splitter, recent repos, window state/geometry, port overrides) saved debounced through `AppController`, a flattened settings path with migration, a reconnect loop in `AppController` (relaunch with 1/2/4/8…30 s backoff, `workspace.list` re-sync, connection generation bump so every subscriber re-attaches), and C++ for the Changes toolbar, the group close dialog, the PR dialog, conflict banners and a port field in the Run panel.

**Tech Stack:** Rust 1.98, tokio, git CLI (`merge --no-ff`, `rebase`, `merge --squash`, `worktree add`), `gh` 2.45, cxx-qt 0.10, Qt 6.9.2 Widgets (`QMainWindow::saveState/restoreState`, `saveGeometry`), `directories` 5, serde_json.

**Spec:** daemon spec §5.4 (merge, rebase, squash), §5.5 (PR), §8.3 (`[claude] settings` override); IDE spec §2 (group close keep/merge/discard; Changes toolbar Merge/Rebase/Squash/Create PR/Discard; splitter ratio persists), §5 step 5 (reconnecting with backoff, re-sync `workspace.list` then `agent.history`), §10 (recent repos), §11 (persistence: `settings.json`, `state.json`, debounced writes, reconciliation), §12 (per-workspace errors: red glyph + dismissible banner, `GitError` stderr expandable); overview §6.3 (`workspace.merge`, `workspace.create_pr`), §7 (reconnection, agent sessions resume via `--resume`), §10 milestone 6.

## Global Constraints

- **Boundary rule (IDE spec §3):** `model/` and `client/` never import Qt; QObjects are thin adapters; C++ lays out, paints and forwards. What is persisted, how reconciliation goes, when to reconnect and what "dirty" means are decided in Rust.
- **Merge runs by the daemon, never inside a sandbox** (daemon §5.4): guard = main repo working tree clean and on the base branch, else `Conflict { reason: "base_dirty" }`; if the main repo is on another branch, use a temporary worktree of the base under `~/.bondsymphonic/merge-<id>` and remove it afterwards; `merge` = `git merge --no-ff bs/<name>/work`; `rebase` = `git rebase <base> bs/<name>/work` in the workspace worktree (through the pinned `worktree_git`), then fast-forward the base; `squash` = `git merge --squash` + `git commit -m "<name>: <summary>"` (summary = request message, else the first line of the workspace's last commit); conflict → `--abort`, `{ok: false, conflicts: [paths]}`, workspace untouched. All git through `Layout::daemon_git()` for the main repo and `worktree_git()` for the worktree; `core.hooksPath` pinned as today.
- **PR (daemon §5.5):** `git push -u origin bs/<name>/work` then `gh pr create --title T --body B [--draft] --head bs/<name>/work --base <base>`; the URL parsed from stdout; `gh` must be authenticated (`check_prereqs` `gh_auth`); a failure is `GitError` with the command, exit code and stderr in `data`. `gh` runs on the host with the daemon user's config, never in a sandbox; `BS_GH_BIN` (test hook, documented) overrides the binary.
- **Discard** = `workspace.destroy { force: true }` after an explicit confirmation naming the workspace and its unmerged commit count.
- **Agent persistence:** `agents.json` under the data dir records `{agent_id, workspace_id, adapter, session_id, options, started_at, ended_at}`; on daemon start, entries load as `exited` agents whose `agent.history` still reads the transcript; `WorkspaceInfo.agents` lists them; destroying a workspace removes its records and transcripts.
- **Run port override:** `RunStartParams.port: Option<u16>` (serde default) replaces the config's port for that start (also `PORT` env and the forwarder); the IDE persists overrides per `(workspace, config)` in `state.json`. Guessed ports are editable in the Run panel; configured ones are not.
- **`[claude] settings`** (daemon §8.3): a repo-relative path from `bondsymphonic.toml` copied into the workspace home as `.claude/settings.json` at agent start, overriding the daemon user's copy; a path escaping the worktree is `InvalidParams`.
- **Persistence (IDE §11):** `%APPDATA%\BondSymphonic\settings.json` and `state.json` (flattened from the current `ProjectDirs` path; migrate an existing file once); `state.json` written on every structural change debounced 500 ms and on exit; on start the IDE reconciles: workspaces the daemon no longer knows are dropped, daemon workspaces not in any group land in "Unsorted"; `BS_STATE_PATH` (test hook) overrides the path like `BS_SETTINGS_PATH`.
- **Reconnection (IDE §5, overview §7):** on process exit or connection loss the IDE enters `Reconnecting` (status bar: "daemon: reconnecting (attempt N)"), relaunches the daemon with backoff 1, 2, 4, 8, 16, 30, 30… s (unless the loss was the IDE's own shutdown), reconnects, re-runs `check_prereqs` and `workspace.list`, bumps the connection generation so `TranscriptModel`/`RunPanelModel`/`ChangesModel`/`FileTreeModel` re-attach; PTYs do not survive (terminals show "[daemon restarted]" and offer reopen); agents come back as `exited` with history, and the transcript pane's Restart button starts a new agent with `resume_session` = the last known session id.
- **Errors (IDE §12):** merge/PR failures show a dismissible banner at the top of the workspace's agent area with the message and, for `GitError`, the stderr in an expandable section; the tab gets the red glyph until dismissed.
- **Quality gates:** `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, zero MSVC `warning C`; daemon suite green on Windows (noop) and WSL; `gh` never invoked for real in tests (a stub on PATH / `BS_GH_BIN`).
- **No desktop input injection, ever.** Env-gated in-process hooks (removed before commit), own-window `grab()`, offscreen smoke test. Every hook creates its own throwaway workspaces, asserts their ids before every action, logs them, and never touches the user's live workspace `ws_eebdd832` (repo `/mnt/c/git/fredsholm-toollib`) — never merge, destroy, restart agents in it, or push its branch.
- **Never push to a real remote in tests or hooks:** throwaway repos get a local bare "origin" (`git init --bare`) so `git push -u origin` is real but local.
- **Environment:** PowerShell 5.1 (no `&&`); `. .\scripts\env.ps1` before cargo on Windows; WSL distro `bondsymphonic`; `.\launch.ps1`; python3 in the distro.
- **Commit trailers:** every commit ends with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01DcYV4WREYNvdE2MsWATepg`.

## File structure

Daemon: `crates/daemon/src/git/merge.rs`, `git/pr.rs` (new), `git/mod.rs`; `agents/persist.rs` (new), `agents/mod.rs`, `agents/claude.rs` (`[claude] settings`), `agents/credentials.rs`; `runs/manager.rs` (port override); `server/handlers.rs`; `workspace/{mod.rs,lifecycle.rs}` (merge temp dir, agent records on destroy); tests `tests/merge_integration.rs`, `tests/pr_integration.rs`, `tests/agent_persistence.rs`, `tests/fixtures/gh_stub.py` (new).

Proto: `crates/proto/src/request.rs` (`RunStartParams.port`, `MergeResult.reason`), roundtrip tests.

IDE Rust: `model/persistence.rs` (new), `model/app_state.rs` (groups from state), `qobjects/settings.rs` (path flatten + migration), `qobjects/app_controller.rs` (state load/save, reconnect loop, merge/pr/discard invokables + signals, generation), `qobjects/{transcript_model,run_panel,changes_model,file_tree,terminal_session}.rs` (reconnect handling), `qobjects/group_model.rs` (load/store groups), `launcher.rs` (relaunch), `client/mod.rs` (disconnect detection). Tests: `tests/persistence_tests.rs` (new), `tests/reconnect_tests.rs` (new, fake daemon restart), `tests/qobject_smoke.rs`, `tests/smoke.rs`.

IDE C++: `cpp/ChangesToolbar.{h,cpp}` (new, inside `ExplorerDock`), `cpp/PrDialog.{h,cpp}` (new), `cpp/CloseGroupDialog.{h,cpp}` (new), `cpp/WorkspaceBanner.{h,cpp}` (new), `cpp/{ExplorerDock,GroupBar,AgentArea,RunPanel,NewAgentDialog,TerminalWidget,TranscriptView,MainWindow,app}.*`, `build.rs`.

---

### Task 1: Daemon merge, rebase, squash, PR

**Files:**
- Create: `crates/daemon/src/git/merge.rs`, `crates/daemon/src/git/pr.rs`, `crates/daemon/tests/merge_integration.rs`, `crates/daemon/tests/pr_integration.rs`, `crates/daemon/tests/fixtures/gh_stub.py`
- Modify: `crates/daemon/src/git/mod.rs` (`pub mod merge; pub mod pr;`), `crates/daemon/src/workspace/mod.rs` (`DataDirs::merge_worktree(id) -> root/merge-<id>`), `crates/daemon/src/server/handlers.rs` (`WorkspaceMerge`, `WorkspaceCreatePr`), `crates/proto/src/request.rs` (`MergeResult { ok, conflicts, #[serde(default)] reason: Option<String> }`; roundtrip), `crates/daemon/tests/common/mod.rs` (`init_repo_with_origin`: a bare local origin and the repo tracking it)

**Interfaces:**
- Produces (`git/merge.rs`): `pub async fn merge(d: &Daemon, id: &WorkspaceId, mode: MergeMode, message: Option<String>) -> Result<MergeResult, RpcError>`. Steps: workspace must be `Ready`; `layout_for`; guard: `git status --porcelain` in the main repo must be empty (else `Conflict` with `data.reason = "base_dirty"`, `MergeResult` not returned); determine the main repo's current branch (`git symbolic-ref --short HEAD`); if it is the base → operate in the main repo; else → `git worktree add <merge-<id>> <base>` (removed with `git worktree remove --force` in every exit path); `merge`: `git merge --no-ff <branch>` (message `Merge <branch>`); `rebase`: in the workspace worktree via `worktree_git`: `git rebase <base> <branch>`; on conflict `git rebase --abort`; then in the base checkout `git merge --ff-only <branch>`; `squash`: `git merge --squash <branch>` then `git commit -m "<name>: <summary>"` where summary = `message` or `git log -1 --format=%s <branch>`; conflicts detected from a non-zero exit whose stderr/`git diff --name-only --diff-filter=U` lists paths → `--abort` (`merge --abort` / `rebase --abort` / `reset --merge` for squash) and `Ok(MergeResult { ok: false, conflicts, reason: Some("conflict") })`; after success the workspace's branch still exists (the user destroys explicitly); `emit_state` unchanged.
- Produces (`git/pr.rs`): `pub async fn create_pr(d: &Daemon, id: &WorkspaceId, title: &str, body: &str, draft: bool) -> Result<CreatePrResult, RpcError>`: `git push -u origin <branch>` in the main repo (daemon_git), then `gh pr create --title --body --head <branch> --base <base> [--draft]` with cwd = main repo, binary from `BS_GH_BIN` or `gh`; parse the first `https://` line of stdout as the URL; failures → `GitError` with `{command, exit_code, stderr}`.
- Consumes: `Layout::daemon_git/worktree_git`, `repo::branch_exists`, `Git::run`, `Workspace { branch, base_branch, repo_path, name }`.

**Fixture** `tests/fixtures/gh_stub.py`: prints its argv to `$GH_STUB_LOG` (one line), and for `pr create` prints `https://github.com/example/repo/pull/42`; exits 1 with `stderr: not logged in` when `GH_STUB_FAIL=1`.

- [ ] **Step 1: Write the failing integration tests.** `merge_integration.rs` (Windows + WSL, noop): repo via `common::init_repo`; create workspace `alpha`; commit a file in the worktree (`common::commit_all` with the worktree env) → `workspace.merge merge` → `ok: true`, main repo `git log --oneline main -1` shows the merge commit and the file exists in the main repo; second workspace `beta` on the same base with a conflicting change to the same file → `merge` → `ok: false`, `conflicts == ["README.md"]`, main repo `git status --porcelain` empty afterwards and `MERGE_HEAD` absent; `rebase` on a third workspace after `alpha` merged → base fast-forwarded (`rev-list base..branch` empty); `squash` with `message: Some("squashed feature")` → one new commit with that subject, and without a message → subject `"<name>: <last commit subject>"`; dirty main repo (write an untracked-then-modified tracked file) → `Conflict` with `data.reason == "base_dirty"`; main repo checked out on another branch (`git checkout -b elsewhere` in the repo) → merge still succeeds and `merge-<id>` is gone afterwards, `elsewhere` still checked out. `pr_integration.rs`: repo with a bare origin; `BS_GH_BIN` pointing at the python stub; `create_pr` → `{url}` equals the stub's URL, the origin has `refs/heads/bs/alpha/work`, the stub log shows `pr create --title T --body B --head bs/alpha/work --base main --draft`; `GH_STUB_FAIL=1` → `GitError` with the stderr.

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --test merge_integration --test pr_integration`.

- [ ] **Step 3: Implement** as specified; register handlers; `DataDirs::merge_worktree`.

- [ ] **Step 4: Run** Windows and WSL suites → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon crates/proto
git commit -m "feat(daemon): workspace.merge (merge/rebase/squash with conflict abort) and workspace.create_pr"
```

---

### Task 2: Agent records across restarts, run port override, `[claude] settings`

**Files:**
- Create: `crates/daemon/src/agents/persist.rs`, `crates/daemon/tests/agent_persistence.rs`
- Modify: `crates/daemon/src/agents/mod.rs` (`AgentManager::restore(&self, dirs)`, records written on start/exit, `history` for restored agents, `agents_of` includes them, `stop_all_in` removes records), `crates/daemon/src/agents/claude.rs` (`[claude] settings` copy at start via `runs::config::load_repo_config`), `crates/daemon/src/daemon.rs` (`restore` calls `agents.restore`), `crates/daemon/src/workspace/lifecycle.rs` (`destroy` removes the workspace's records + transcript files), `crates/daemon/src/workspace/mod.rs` (`DataDirs::agents_file()`), `crates/daemon/src/runs/manager.rs` (`RunStartParams.port` override), `crates/proto/src/request.rs` (`RunStartParams { .., #[serde(default)] port: Option<u16> }`; roundtrip), `crates/daemon/tests/run_integration.rs` (override case)

**Interfaces:**
- Produces (`agents/persist.rs`): `#[derive(Serialize, Deserialize, Clone, PartialEq)] pub struct AgentRecord { pub agent_id: AgentId, pub workspace_id: WorkspaceId, pub adapter: AgentAdapterKind, pub session_id: Option<String>, pub options: AgentStartOptions /* api_key never stored: set to None before writing */, pub started_at: String, pub ended_at: Option<String> }`; `pub struct AgentRecords { path }` with `load() -> Vec<AgentRecord>` (missing → empty; corrupt → warn + empty + the file renamed `.corrupt`), `upsert(record)`, `remove_workspace(ws)` (atomic temp+rename writes).
- `AgentManager`: on `start` → `upsert` (session id updated when the init line arrives, `ended_at` on exit); `restore(&self, records)` creates `LiveAgent`-less "archived" entries so `history`, `agents_of` and `WorkspaceInfo.agents` include them (state `Exited`, detail "daemon restarted"); `send`/`permission_reply`/`interrupt` on an archived agent → `NotFound` with message "agent ended; start a new one with resume"; `stop` → `Ok`. `agent.start` with `options.resume_session` passes `--resume` (already supported) — the IDE takes the id from history's init/result messages.
- `runs/manager.rs`: `let port = p.port.unwrap_or(config.port)` used for `PORT`, the forwarder and the probe; `Conflict` still keyed by config name.
- `claude.rs`: at `start`, if the worktree's `bondsymphonic.toml` has `[claude] settings = "<rel path>"`, resolve inside the worktree (`fs::resolve`) and copy it to `<home>/.claude/settings.json` (overwrite), else the daemon user's copy as today; a path outside the worktree → `InvalidParams`.

- [ ] **Step 1: Write the failing tests.** `agent_persistence.rs`: start the fake claude agent (`BS_CLAUDE_BIN` as in `agent_integration.rs`), send a message, stop it → `agents.json` holds one record with `ended_at` set and no `api_key`; start a second daemon over the same data dir (`start_daemon` again with the same root after cancelling the first) → `workspace.get` lists the agent id, `agent.history` returns the same messages, `agent.history`'s `state` field (added in M4) reports `exited` with a detail containing "restarted"; `agent.send` → `NotFound`; `workspace.destroy` removes the record and the transcript file. `run_integration.rs`: `run.start web` with `port: Some(other)` → `host_port == other` on noop, `PORT` env observed by the command (print it). Unit test for the `[claude] settings` copy with a temp worktree + toml (and the escaping path rejected).

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run** Windows and WSL suites → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon crates/proto
git commit -m "feat(daemon): agent records survive restarts; run.start port override; [claude] settings honoured"
```

---

### Task 3: IDE persistence — `state.json`, flattened settings, group reconciliation, recent repos, port overrides

**Files:**
- Create: `crates/ide/src/model/persistence.rs`, `crates/ide/tests/persistence_tests.rs`
- Modify: `crates/ide/src/model/mod.rs`, `crates/ide/src/model/app_state.rs` (`Workspaces::from_persisted(groups, daemon_list)`), `crates/ide/src/qobjects/settings.rs` (path flatten + migration; `state_path()`), `crates/ide/src/qobjects/app_controller.rs` (`loadState() -> QString`, `saveState(json)` debounced 500 ms, `flushState()`, `recentRepos()`/`noteRecentRepo(path)`, `portOverride(ws, config) -> i32`/`setPortOverride(ws, config, port)`, `stateLoaded(json)` after the first `workspace.list`), `crates/ide/src/qobjects/group_model.rs` (`loadGroups(json)` before the first reconcile; `groupsJson()` for saving), `crates/ide/src/qobjects/run_panel.rs` (`start()` sends the override), `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Produces (`model/persistence.rs`): `#[derive(Serialize, Deserialize, Default, PartialEq, Debug, Clone)] pub struct StateFile { pub version: u32 /* 1 */, pub groups: Vec<PersistedGroup { name, workspace_ids: Vec<String> }>, pub active_workspace: Option<String>, pub open_editors: BTreeMap<String /* ws */, Vec<String /* rel path */>>, pub active_editor: BTreeMap<String, String>, pub splitter_sizes: Vec<i32>, pub swapped: bool, pub recent_repos: Vec<String> /* most recent first, ≤ 10 */, pub window_state_b64: String, pub geometry_b64: String, pub port_overrides: BTreeMap<String /* "ws\nconfig" */, u16> }`; `pub fn load(path) -> StateFile` (missing/corrupt → default + `warn!`, corrupt file renamed `.corrupt`), `pub fn save(path, &StateFile) -> io::Result<()>` (atomic temp + rename), `pub fn note_recent(&mut self, repo: &str)` (dedup, cap 10), `pub fn prune(&mut self, live_workspace_ids: &[String])` (drops editors/overrides/groups' ids for workspaces the daemon no longer has).
- Produces (`app_state.rs`): `Workspaces::from_persisted(groups: &[PersistedGroup], list: &[WorkspaceInfo], active: Option<&str>) -> Workspaces` — groups in persisted order with the tabs the daemon still knows (built from `WorkspaceInfo` like `reconcile` does), unknown daemon workspaces → "Unsorted", empty persisted groups kept (a user may keep an empty group), active tab restored when present; `Workspaces::persisted_groups(&self) -> Vec<PersistedGroup>`.
- `AppController`: keeps a `StateFile` in Rust; `loadState()` at start (before connecting), emits `stateLoaded(json)` so `MainWindow` restores geometry/window state/splitter/swap; after the first `workspace.list` the controller builds `Workspaces::from_persisted` and hands it to `GroupModel::loadWorkspaces(json)` (replacing the current reconcile-only start); `saveState()` is called by `GroupModel` on every mutation (via the `changed` signal handled in Rust) and by `MainWindow` for editor/splitter/window changes through `noteEditors(ws, json)`, `noteSplitter(sizesJson, swapped)`, `noteWindow(stateB64, geometryB64)` — each marks dirty and the debounced writer flushes 500 ms later; `flushState()` on exit (MainWindow `closeEvent`).
- `Settings::path()`: `%APPDATA%\BondSymphonic\settings.json`; if absent and the old `ProjectDirs` path exists, copy it once; `Settings::state_path()` beside it, `BS_STATE_PATH` override.

- [ ] **Step 1: Write the failing tests** `persistence_tests.rs`: `StateFile` round trip; corrupt file → default and `.corrupt` rename; `note_recent` dedup/cap; `prune`; `Workspaces::from_persisted` (order kept, unknown → Unsorted, dropped ids, active restored, empty group kept); settings migration from the old path (temp dirs via `BS_SETTINGS_PATH`); debounce helper test if factored (`fn should_write(last_write, now, dirty) -> bool`).

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** No Qt in `persistence.rs`.

- [ ] **Step 4: Build and test**: `cargo test -p bondsymphonic-ide` → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide
git commit -m "feat(ide): state.json persistence for groups, editors, layout, recent repos and port overrides; flattened settings path"
```

---

### Task 4: IDE reconnection with backoff and re-sync

**Files:**
- Create: `crates/ide/tests/reconnect_tests.rs`
- Modify: `crates/ide/src/qobjects/app_controller.rs` (reconnect loop, `Reconnecting` state with attempt count, `reconnected()` signal, `connectionGeneration` already exists), `crates/ide/src/model/app_state.rs` (`ConnectionState::Reconnecting`, `compose_status`), `crates/ide/src/launcher.rs` (`DaemonProcess::wait_exit()` future so a dead process is noticed; relaunch), `crates/ide/src/client/mod.rs` (no change unless needed), `crates/ide/src/qobjects/{transcript_model.rs,run_panel.rs,changes_model.rs,file_tree.rs,terminal_session.rs}` (react to `reconnected()`/generation: transcripts re-attach and replay, run panel refreshes and re-subscribes, changes/tree reload, terminals mark "[daemon restarted]" and expose `reopen()`), `crates/ide/tests/qobject_smoke.rs`, `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` and `-overview-design.md` (remove the "not yet implemented" reconnect notes)

**Interfaces:**
- `AppController`: on `drain_events` end or `DaemonProcess` exit → if `m_quitting` is false: `set_state(Reconnecting)`, spawn the reconnect task: attempt N with delay `min(30, 2^(N-1))` s: relaunch (unless `BS_DAEMON_ADDR` test endpoint is set — then only reconnect), connect, `publish_shared` (bumps the generation), `drain_events` again, `check_prereqs`, `workspace.list` → `workspacesListed` (reconcile), then emit `reconnected(generation)`; status text "daemon: reconnecting (attempt N)" then "daemon: connected v…"; `stopReconnecting()` invokable (File > Exit sets `m_quitting`). A pure helper `pub fn backoff_delay(attempt: u32) -> Duration` with a test.
- `TranscriptModel`: on `reconnected` (connected in Rust through a shared `tokio::sync::watch<u64>` generation channel exposed by `app_controller::generation_watch()`), if attached: re-`attach` (subscribe on the new router, replay history; the restored agent's `state` from history is `exited` → the view shows Restart; `restartOptionsJson()` returns the tab's options with `resume_session` = the last session id seen in history).
- `RunPanelModel`, `ChangesModel`, `FileTreeModel`: on generation change → `refresh()`/reload for the shown workspace (their subscriptions already re-subscribe via generation checks).
- `TerminalSession`: on generation change while a PTY is open → mark `exited` with the exit line "[daemon restarted]" and emit `frame`; `reopen()` invokable re-runs `open` with the last workspace/command/size; `TerminalWidget` (C++, Task 5) shows a "Reopen" button on that state.

- [ ] **Step 1: Write the failing tests.** `reconnect_tests.rs` (in-process fake daemon like `client_tests.rs`, IDE launched offscreen with `BS_DAEMON_ADDR`): the fake daemon closes the connection after the first `workspace.list`; the test restarts the fake on the same port; assert the IDE's stdout shows `reconnecting (attempt 1)` then `connected`, a second `hello` + `workspace.list` in the journal, and the smoke script step `quit` still exits 0 (extend `smoke.rs` with a `reconnect` step that asks the fake, via a control request the fake understands — `system.test_drop` — to drop the connection; document it as a test-only fake-daemon method that the real daemon rejects). `backoff_delay` unit test. `compose_status` for `Reconnecting`.

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Build and test** → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide docs
git commit -m "feat(ide): reconnect to the daemon with backoff and re-sync; panes re-attach after a restart"
```

---

### Task 5: C++ — Changes toolbar (merge/rebase/squash/PR/discard), group close dialog, workspace banner, persistence hooks, port field, reopen/restart

**Files:**
- Create: `crates/ide/cpp/ChangesToolbar.{h,cpp}`, `crates/ide/cpp/PrDialog.{h,cpp}`, `crates/ide/cpp/CloseGroupDialog.{h,cpp}`, `crates/ide/cpp/WorkspaceBanner.{h,cpp}`
- Modify: `crates/ide/cpp/{ExplorerDock,GroupBar,AgentArea,RunPanel,NewAgentDialog,TerminalWidget,TranscriptView,MainWindow,app}.*`, `crates/ide/build.rs`, plus the Rust invokables/signals this needs on `AppController`: `mergeWorkspace(ws, mode, message)` → `mergeFinished(ws, okBool, conflictsJson, reason)` / `operationFailed`; `createPr(ws, title, body, draft)` → `prCreated(ws, url)`; `discardWorkspace(ws)` (= destroy force) reusing `workspaceDestroyed`; `workspaceSummary(ws)` → `workspaceSummarized(ws, json {dirty, changed_files})` built in Rust from `workspace.status` and `workspace.changes` (no proto change; the squash summary line defaults on the daemon side, which derives it from the last commit) (Task 5 owns these Rust additions in `app_controller.rs`; keep them thin)

**Interfaces:**
- `ChangesToolbar : QToolBar` (top of the Changes tab): Merge, Rebase, Squash, Create PR…, Discard… — enabled only with an active workspace; Merge/Rebase run immediately with a confirmation naming the workspace and base; Squash asks for the summary line (optional; empty lets the daemon use the last commit subject); Create PR… opens `PrDialog` (title prefilled with the workspace name, body multi-line, draft checkbox) → `createPr`; Discard… confirms with "Discard <name>? Its N changed files and any unmerged commits will be lost" → `discardWorkspace`. Results: success → status-bar message ("Merged bs/x/work into main", "PR: <url>" with a clickable link that opens the browser); conflict → `WorkspaceBanner` with "Merge stopped: conflicts in a, b, c — the workspace is untouched; ask the agent to rebase" ; `operationFailed` → banner with the message and, when the data has `stderr`, an expandable section.
- `WorkspaceBanner : QFrame` — shown at the top of the workspace's agent area page (`AgentArea::showBanner(ws, title, detail, stderrOrEmpty)`), dismiss button; the tab's red glyph via `GroupModel::setWorkspaceError(ws, detail)`/`clearWorkspaceError(ws)` (errors are per workspace, not per agent) (Rust additions in `group_model.rs`, mapping to `TabStatus::Error` with detail until cleared, restored to the agent/workspace status on clear).
- `CloseGroupDialog : QDialog` — for "Close group" in the group tab context menu: one row per workspace with a combo {Keep (move to Unsorted), Merge, Discard}; OK runs the choices in order (merge → on conflict stop and report, keep the rest untouched; discard → destroy force), then removes the group (`GroupModel::removeGroup(name)` — Rust addition moving kept tabs to Unsorted).
- `RunPanel`: a port `QSpinBox` next to the config combo, editable only when the selected config is `port_guessed`, initialised from `AppController::portOverride(ws, config)` (0 = none) and written back with `setPortOverride` on change; `start()` sends the override (Task 3's model change).
- `NewAgentDialog`: the repo path field gets a "Recent" dropdown from `recentRepos()`; a successful create calls `noteRecentRepo`.
- `MainWindow`: restore geometry/window state/splitter sizes/swap from `stateLoaded`; save on `closeEvent` (`flushState`) and on splitter/dock changes (`noteSplitter`/`noteWindow`); open editors per workspace restored when a workspace tab is first shown after start (`EditorArea::openFile` for each persisted path; failures ignored) and noted on open/close (`noteEditors`); `reconnected` → status bar refresh.
- `TerminalWidget`: on the "[daemon restarted]" exited state show a "Reopen" button calling `session->reopen()`. `TranscriptView`: after reconnect the model reports `exited`; the existing Restart button uses `restartOptionsJson()` so the new agent resumes.

- [ ] **Step 1: Implement** (Rust additions first, then C++).

- [ ] **Step 2: Verification with a temporary env-gated hook** (`BS_T5_SELFTEST=1`; own-window `grab()` to `<scratchpad>\m6-merge.png`; removed before commit): throwaway repo with a bare local origin; two workspaces; commit a change in A through its shell PTY (`git commit` inside the sandbox), click Merge (in-process) → log the status message and that the main repo has the commit; make B conflict, click Merge → log the banner text and the conflict list; click Create PR with `BS_GH_BIN` pointing at the stub inside the distro → log the URL message; open Close group on a group with both → choose Keep for A, Discard for B → log that B is destroyed and A is in Unsorted; set a port override on a guessed config and log `run.start`'s `port` in the daemon log; quit and relaunch the IDE (the hook runs twice, keyed by a marker file) → log that groups, the active tab, the open editor tab and the port override came back. Never touch `ws_eebdd832`.

- [ ] **Step 3: Build, clippy, fmt, `cargo test -p bondsymphonic-ide`, zero MSVC warnings; remove the hook; commit**

```bash
git add crates/ide
git commit -m "feat(ide): merge/rebase/squash/PR/discard from the Changes tab; close-group dialog; persisted layout and editors; port override; reopen after restart"
```

---

### Task 6: Smoke, docs, footprint, end-to-end (incl. a real daemon restart)

**Files:**
- Modify: `crates/ide/src/qobjects/smoke.rs` (steps `merge`, `pr`, `reconnect`), `crates/ide/tests/smoke.rs` (fake daemon: `workspace.merge` → `{ok:true,conflicts:[]}` then a second call → `{ok:false, conflicts:["a.txt"]}`; `workspace.create_pr` → `{url}`; `system.test_drop`; the fake restarts its listener on the same port after the drop), `README.md` ("What works now": merge/rebase/squash, PR, discard, close group, persistence, reconnect, port override, `[claude] settings`; "Test hooks": `BS_GH_BIN`, `BS_STATE_PATH`, `system.test_drop`, new smoke steps; limitations: PTYs do not survive a daemon restart; runs do not either; merge requires a clean base), `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§13 footprint row; §14 smoke), `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§8 agent records), `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` (§7 reconnection note removed)

- [ ] **Step 1: Smoke** as above; assertions: order contains `workspace.merge`, `workspace.create_pr`, a second `hello` after the drop, a second `workspace.list`; the merge journal entries carry `mode`; no "failed" warnings for `merge`/`create_pr`/`reconnect`; `state.json` written under `BS_STATE_PATH` with the smoke workspace in a group and the open editor path.

- [ ] **Step 2: Footprint** per §13 (two workspaces, one Claude tab with history, one run, editors) after a reconnect cycle: IDE working set and daemon RSS.

- [ ] **Step 3: Docs** as listed.

- [ ] **Step 4: End-to-end** with `.\launch.ps1` and a temporary hook: create a throwaway workspace with a terminal tab; from PowerShell send `wsl -d bondsymphonic -- pkill -f bondsymphonic-daemon` (only the daemon process; the user's workspace registry is on disk and restored by the relaunch — its sandbox is restarted by `restore`, which is expected and does not modify it); log the status bar going through reconnecting → connected, the workspace list coming back with both the throwaway and the user's workspace listed (do not act on the latter), the terminal showing "[daemon restarted]" and Reopen working; then merge the throwaway workspace into its own repo and destroy it; screenshot `<scratchpad>\m6-final.png`; registry before/after for `ws_eebdd832` byte-identical apart from `state` timestamps if any.

- [ ] **Step 5: Commit**

```bash
git add crates/ide README.md docs
git commit -m "test(ide): smoke covers merge, PR and a daemon reconnect; docs and footprint for milestone 6"
```

---

## Milestone 6 exit criteria

- From the Changes tab: Merge, Rebase and Squash land the workspace's commits on the base branch (temporary base worktree when the repo is checked out elsewhere); conflicts abort cleanly and are listed in a banner; Create PR pushes and opens a PR (verified with a stub `gh` and a local bare origin); Discard destroys with a confirmation naming what is lost.
- Close group offers keep/merge/discard per workspace and acts accordingly.
- Restarting the IDE restores groups, the active tab, open editors, splitter/window layout, recent repos and port overrides; workspaces the daemon lost are dropped and new ones land in Unsorted.
- Killing the daemon makes the IDE reconnect with backoff, re-sync, re-attach transcripts (agents resumable with `--resume`), refresh panels, and offer to reopen terminals; agents' history survives a daemon restart.
- `run.start` honours a port override; `[claude] settings` from the repo is applied; daemon suites green on Windows and WSL; smoke covers merge, PR and reconnect; clippy/fmt clean; footprint recorded.
