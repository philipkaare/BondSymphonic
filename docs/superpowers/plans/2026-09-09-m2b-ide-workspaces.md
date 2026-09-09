# Milestone 2b: IDE Groups, Agent Tabs, File Tree, Terminal — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From the IDE you can create a workspace (repo, base branch, name, group), see it as an agent tab under a group tab, browse its worktree in the Explorer, and use a real terminal running inside its sandbox (both the bottom "Terminal" tab and, for the terminal adapter, the agent pane), then destroy it. Workspace and PTY events from the daemon drive the UI.

**Architecture:** Pure-Rust model (`model/`): groups and tabs, file-tree cache, terminal grid via `alacritty_terminal`. An `EventRouter` on the tokio thread fans daemon events out by workspace/PTY id. cxx-qt QObjects (`qobjects/`) expose properties, invokables, and signals; lists cross the bridge as JSON strings. C++ (`cpp/`) is layout and painting only: `GroupBar`, `NewAgentDialog`, `ExplorerDock`, `TerminalWidget`, `AgentArea`. Two small daemon changes make the terminal safe: a bounded retry for Windows renames in `fs.write_file`, and an explicit "output dropped" signal when a slow client lags the event bus.

**Tech Stack:** Rust stable, cxx-qt 0.10, Qt 6.9 Widgets, `alacritty_terminal` 0.26 (+ `vte` 0.15), `base64` 0.22, tokio, `bondsymphonic-proto`.

**Spec:** `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` §2, §3, §4, §9, §10, §11 (in-memory part), §12; `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` §6.3 (workspace, pty, fs, repo), §6.4, §10 milestone 2. Deferred-by-design here: Claude adapter and transcript view (Milestone 4), run panel (Milestone 5), editor and diff (Milestone 3), persistence of groups to disk (Milestone 6).

## Global Constraints

- Qt Widgets only (no QtQuick/QtQml/QtWebEngine/QtNetwork). `model/`, `client/`, `launcher.rs` never import Qt. C++ contains layout and painting; conditional logic beyond "which widget to show" lives in Rust.
- The IDE never runs git, sandbox, or process logic; it only calls the daemon and spawns `wsl.exe`/the browser.
- No blocking calls on the Qt thread; daemon I/O runs on the tokio runtime (`app_controller::runtime()`); results reach QObjects via `qt_thread().queue`.
- Protocol types come from `bondsymphonic-proto`; no wire-format changes. Daemon changes in Task 1 are additive.
- Qt-side names are camelCase (`#[auto_cxx_name]`); Rust property names snake_case. Generated headers: `bondsymphonic-ide/src/qobjects/<file>.cxxqt.h`.
- Build/test on Windows needs `scripts\env.ps1` dot-sourced (Qt on PATH). `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all`.
- Terminal: `pty.output` may arrive before the `pty.open` reply (bounded window); the IDE buffers output for unknown PTY ids for 5 s. A `daemon.log` warn whose message starts with `events dropped:` renders as an inline `[output dropped]` marker in every open terminal of that connection.
- Commit after every task (conventional commits) on branch `m2b-ide-workspaces`. Scratch files in the session scratchpad, never `/tmp`.

---

## File structure for this milestone

```
crates/daemon/
  src/fs.rs                          write_file: bounded rename retry on Windows sharing violations
  src/server/connection.rs           Lagged(n) -> daemon.log warn "events dropped: n" to that client; capacity 4096
  src/server/broadcast.rs            capacity constant
  tests/fs_service.rs                retry test (windows-gated)
  tests/server_integration.rs        lagged-client test
crates/ide/
  Cargo.toml                         + alacritty_terminal, vte, base64
  build.rs                           + new bridge files, new cpp files
  src/model/app_state.rs             + Group, AgentTab, TabStatus, Workspaces (reconcile with daemon list)
  src/model/file_tree.rs             FileTree cache: dirs loaded lazily, entries sorted per daemon
  src/model/terminal_grid.rs         TerminalGrid over alacritty_terminal: feed, rows, key mapping, resize
  src/client/router.rs               EventRouter: subscribe by workspace_id / pty_id / all
  src/qobjects/app_controller.rs     + client accessor, router, workspace ops, event routing
  src/qobjects/group_model.rs        GroupModel QObject: groups+tabs JSON, invokables, signals
  src/qobjects/file_tree.rs          FileTreeModel QObject: load_dir(path) -> entriesLoaded(path, json)
  src/qobjects/terminal_session.rs   TerminalSession QObject: open/write/resize/close; rows JSON; signals
  cpp/GroupBar.{h,cpp}               two QTabBars + status glyphs + "+" button + context menu
  cpp/NewAgentDialog.{h,cpp}         repo picker, branch combo, name, adapter, command, group
  cpp/ExplorerDock.{h,cpp}           Files tree (QTreeView + QStandardItemModel, lazy expand)
  cpp/TerminalWidget.{h,cpp}         paints TerminalSession rows; keys; wheel; resize
  cpp/AgentArea.{h,cpp}              QStackedWidget: one TerminalWidget per workspace (terminal adapter)
  cpp/MainWindow.{h,cpp}             wires the above; bottom Terminal tab per workspace; status bar
  tests/model_tests.rs               app_state, file_tree, terminal_grid tests
  tests/router_tests.rs              EventRouter tests with a fake daemon
```

---

### Task 1: Daemon hardening for the terminal (rename retry, dropped-output signal)

**Files:**
- Modify: `crates/daemon/src/fs.rs`, `crates/daemon/src/server/connection.rs`, `crates/daemon/src/server/broadcast.rs`
- Test: `crates/daemon/tests/fs_service.rs`, `crates/daemon/tests/server_integration.rs`

**Interfaces:**
- `fs::write_file` unchanged signature; on Windows, `rename` is retried up to 20 times with 5 ms sleeps when the error is `PermissionDenied` or raw OS error 32/33 (sharing violation / lock violation).
- `broadcast::EventBus::new` capacity constant `EVENT_BUS_CAPACITY = 4096`.
- Connection loop: on `RecvError::Lagged(n)` the connection writes `ServerMessage::event(None, Event::DaemonLog { level: Warn, message: format!("events dropped: {n}"), host: None })` to that client only, then continues.

- [ ] **Step 1: Failing tests**

Append to `crates/daemon/tests/fs_service.rs`:

```rust
#[cfg(windows)]
#[test]
fn write_file_retries_when_destination_is_briefly_locked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(&root).unwrap();
    fs::write_file(&root, "locked.txt", "v1").unwrap();
    // Hold the destination open with share-deny-none is not enough to block rename on Windows;
    // a mapping does: open + hold a std File and MapViewOfFile-like effect is not available in std,
    // so emulate contention with concurrent renames: many writers, plus a reader loop that keeps
    // reopening the file. Success criterion: no writer errors.
    let path = root.join("locked.txt");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = { let p = path.clone(); let s = stop.clone(); std::thread::spawn(move || {
        while !s.load(std::sync::atomic::Ordering::Relaxed) { let _ = std::fs::read(&p); }
    }) };
    let writers: Vec<_> = (0..8).map(|i| { let r = root.clone(); std::thread::spawn(move || {
        for _ in 0..50 { fs::write_file(&r, "locked.txt", &format!("writer {i}")).expect("write_file must not fail under contention"); }
    }) }).collect();
    for w in writers { w.join().unwrap(); }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().starts_with("writer "));
    assert!(std::fs::read_dir(&root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".bs-tmp")));
}
```

Append to `crates/daemon/tests/server_integration.rs` (uses the existing `start`/`connect`/`send`/`recv` helpers in that file):

```rust
#[tokio::test]
async fn a_lagging_client_is_told_how_many_events_it_missed() {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let (port, token, bus) = (server.port(), server.token().to_string(), server.event_bus());
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });
    let (mut r, mut w) = connect(port).await;
    send(&mut w, 1, Request::Hello(HelloParams { token, client_version: "0.1.0".into() })).await;
    recv(&mut r).await.unwrap();
    // Publish far more than the bus capacity while the client reads nothing.
    for i in 0..(bondsymphonic_daemon::server::broadcast::EVENT_BUS_CAPACITY * 3) {
        bus.publish(None, Event::DaemonLog { level: LogLevel::Info, message: format!("m{i}"), host: None });
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Drain: somewhere in the stream there must be exactly one "events dropped: N" with N > 0,
    // and the messages after it must be in order.
    let mut dropped: Option<u64> = None;
    let mut last_seen: Option<u64> = None;
    for _ in 0..(bondsymphonic_daemon::server::broadcast::EVENT_BUS_CAPACITY * 3 + 5) {
        let Some(msg) = tokio::time::timeout(std::time::Duration::from_millis(500), recv(&mut r)).await.ok().flatten() else { break };
        if let ServerMessage::Event { event: Event::DaemonLog { message, level, .. }, .. } = msg {
            if let Some(n) = message.strip_prefix("events dropped: ") {
                assert_eq!(level, LogLevel::Warn);
                assert!(dropped.replace(n.parse().unwrap()).is_none(), "only one drop notice expected");
            } else if let Some(i) = message.strip_prefix('m') {
                let i: u64 = i.parse().unwrap();
                if let Some(prev) = last_seen { assert!(i > prev, "events after a drop must stay ordered"); }
                last_seen = Some(i);
            }
        }
    }
    assert!(dropped.unwrap_or(0) > 0, "expected a drop notice, got {dropped:?}");
    cancel.cancel();
}
```

- [ ] **Step 2: Run to see failures**

Run: `cargo test -p bondsymphonic-daemon --test fs_service --test server_integration`
Expected: the lag test fails (no drop notice, or unresolved `EVENT_BUS_CAPACITY`); the Windows retry test may already pass (contention is timing-dependent) — that is acceptable, it is a regression guard.

- [ ] **Step 3: Implement**

`broadcast.rs`: add `pub const EVENT_BUS_CAPACITY: usize = 4096;` and use it where `Server::bind` constructs the bus (`EventBus::new(EVENT_BUS_CAPACITY)`).

`connection.rs`: in the event branch of the `select!`, replace the silent `Lagged` handling with:

```rust
Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
    tracing::warn!(dropped = n, "client lagged; events dropped");
    if ctx.is_authenticated() {
        let notice = ServerMessage::event(None, Event::DaemonLog { level: LogLevel::Warn, message: format!("events dropped: {n}"), host: None });
        if out_tx.send(codec::encode(&notice)).await.is_err() { break; }
    }
}
Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
```

(keep whatever structure the loop already has; the point is that `Lagged` produces exactly one notice per lag event and the loop continues).

`fs.rs` `write_file`: replace the single `std::fs::rename(&tmp, &path)` with:

```rust
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if cfg!(windows) && attempts < 20 && is_sharing_violation(&e) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied) || matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5))
}
```

and on final failure remove the temp file as the existing error path does.

- [ ] **Step 4: Tests, clippy, fmt; commit**

Run on Windows: `cargo test -p bondsymphonic-daemon` and in WSL: `scripts\test-daemon.ps1`; clippy both; fmt.

```bash
git add crates/daemon
git commit -m "fix(daemon): retry Windows renames in write_file; tell lagging clients how many events were dropped"
```

---

### Task 2: IDE model — groups, agent tabs, reconciliation, file-tree cache

**Files:**
- Modify: `crates/ide/src/model/app_state.rs`, `crates/ide/src/model/mod.rs`
- Create: `crates/ide/src/model/file_tree.rs`
- Test: `crates/ide/tests/model_tests.rs`

**Interfaces:**

```rust
// model/app_state.rs (additions)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabStatus { Idle, Working, WaitingPermission, Error, Done, Creating, SandboxDown }
impl TabStatus { pub fn glyph(self) -> &'static str /* "○" "●" "!" "✕" "✓" "…" "⏸" */; pub fn from_workspace_state(s: &WorkspaceState) -> TabStatus; pub fn as_i32(self) -> i32 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTab { pub workspace_id: WorkspaceId, pub name: String, pub repo_path: String, pub branch: String, pub status: TabStatus, pub detail: String, pub adapter: AgentAdapterKind, pub command: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group { pub id: String, pub name: String, pub tabs: Vec<AgentTab> }

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspaces { pub groups: Vec<Group>, pub active_group: usize, pub active_tab: usize }

impl Workspaces {
    pub fn new_default() -> Self;                                   // one group "Default"
    pub fn add_group(&mut self, name: &str) -> usize;
    pub fn rename_group(&mut self, idx: usize, name: &str) -> bool;
    pub fn add_tab(&mut self, group_idx: usize, tab: AgentTab) -> (usize, usize);   // makes it active
    pub fn remove_workspace(&mut self, id: &WorkspaceId) -> bool;   // from whichever group; fixes active indexes
    pub fn find(&self, id: &WorkspaceId) -> Option<(usize, usize)>;
    pub fn apply_workspace_info(&mut self, info: &WorkspaceInfo) -> Option<(usize, usize)>; // updates status/branch/detail; None if unknown
    pub fn reconcile(&mut self, daemon_list: &[WorkspaceInfo]);     // drop tabs the daemon no longer has; put unknown ones in "Unsorted"
    pub fn active(&self) -> Option<&AgentTab>;
    pub fn set_active(&mut self, group_idx: usize, tab_idx: usize) -> bool;
    pub fn to_json(&self) -> String; pub fn from_json(s: &str) -> Option<Self>;
}

// model/file_tree.rs
pub struct FileTree { root_loaded: bool, dirs: HashMap<String, Vec<FileEntry>> }   // key: relative dir path, "" = root
impl FileTree {
    pub fn new() -> Self;
    pub fn set_dir(&mut self, path: &str, entries: Vec<FileEntry>);
    pub fn get_dir(&self, path: &str) -> Option<&[FileEntry]>;
    pub fn is_loaded(&self, path: &str) -> bool;
    pub fn invalidate(&mut self, path: &str);       // drops that dir and every descendant
    pub fn child_path(dir: &str, name: &str) -> String;   // "" + "src" = "src"; "src" + "a.rs" = "src/a.rs"
    pub fn entries_json(entries: &[FileEntry]) -> String;
}
```

- [ ] **Step 1: Failing tests** `crates/ide/tests/model_tests.rs`

```rust
use bondsymphonic_ide::model::app_state::*;
use bondsymphonic_ide::model::file_tree::*;
use bondsymphonic_proto::*;

fn info(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo { id: id.into(), name: name.into(), repo_path: "/r".into(), base_branch: "main".into(), branch: format!("bs/{name}/work"),
        worktree_path: "/w".into(), created_at: "t".into(), allowlist: vec![], state, agents: vec![], runs: vec![] }
}
fn tab(id: &str, name: &str) -> AgentTab {
    AgentTab { workspace_id: id.into(), name: name.into(), repo_path: "/r".into(), branch: format!("bs/{name}/work"), status: TabStatus::Creating, detail: String::new(), adapter: AgentAdapterKind::Terminal, command: None }
}

#[test]
fn groups_tabs_and_active_selection() {
    let mut w = Workspaces::new_default();
    assert_eq!(w.groups[0].name, "Default");
    let g = w.add_group("Frontend");
    assert_eq!(w.add_tab(g, tab("ws_1", "a")), (1, 0));
    assert_eq!(w.add_tab(0, tab("ws_2", "b")), (0, 0));
    assert_eq!(w.active().unwrap().name, "b");
    assert!(w.set_active(1, 0));
    assert_eq!(w.active().unwrap().workspace_id, WorkspaceId::from("ws_1"));
    assert!(w.remove_workspace(&"ws_1".into()));
    assert!(w.active().is_some(), "active must fall back to an existing tab");
    assert!(!w.remove_workspace(&"ws_1".into()));
    let json = w.to_json();
    assert_eq!(Workspaces::from_json(&json).unwrap(), w);
}

#[test]
fn workspace_info_updates_status_and_reconcile_drops_and_adds() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "a"));
    assert_eq!(w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Ready)), Some((0, 0)));
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Idle);
    assert_eq!(w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Error("boom".into()))), Some((0, 0)));
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(w.groups[0].tabs[0].detail, "boom");
    assert_eq!(w.apply_workspace_info(&info("ws_9", "z", WorkspaceState::Ready)), None);

    w.reconcile(&[info("ws_9", "z", WorkspaceState::SandboxDown)]);
    assert!(w.find(&"ws_1".into()).is_none(), "ws_1 gone from daemon -> dropped");
    let (g, t) = w.find(&"ws_9".into()).unwrap();
    assert_eq!(w.groups[g].name, "Unsorted");
    assert_eq!(w.groups[g].tabs[t].status, TabStatus::SandboxDown);
}

#[test]
fn tab_status_glyphs_and_mapping() {
    assert_eq!(TabStatus::from_workspace_state(&WorkspaceState::Creating), TabStatus::Creating);
    assert_eq!(TabStatus::from_workspace_state(&WorkspaceState::Destroying), TabStatus::Done);
    let all = [TabStatus::Idle, TabStatus::Working, TabStatus::WaitingPermission, TabStatus::Error, TabStatus::Done, TabStatus::Creating, TabStatus::SandboxDown];
    let glyphs: std::collections::HashSet<&str> = all.iter().map(|s| s.glyph()).collect();
    assert_eq!(glyphs.len(), all.len());
}

#[test]
fn file_tree_cache_paths_and_invalidation() {
    let mut t = FileTree::new();
    assert!(!t.is_loaded(""));
    let e = |n: &str, d: bool| FileEntry { name: n.into(), is_dir: d, size: 0, status: FileStatus::Unchanged };
    t.set_dir("", vec![e("src", true), e("README.md", false)]);
    t.set_dir("src", vec![e("main.rs", false)]);
    assert_eq!(FileTree::child_path("", "src"), "src");
    assert_eq!(FileTree::child_path("src", "main.rs"), "src/main.rs");
    assert!(t.is_loaded("src"));
    t.invalidate("");
    assert!(!t.is_loaded("") && !t.is_loaded("src"));
    let json = FileTree::entries_json(&[e("a", true)]);
    assert!(json.contains("\"is_dir\":true"));
}
```

- [ ] **Step 2: Run to see failures** — `cargo test -p bondsymphonic-ide --test model_tests` (dot-source `scripts\env.ps1` first) → compile errors.

- [ ] **Step 3: Implement** the additions in `app_state.rs` (keep the existing `ConnectionState`, `compose_status`) and the new `file_tree.rs`. Mapping: `Creating→Creating`, `Ready→Idle`, `SandboxDown→SandboxDown`, `Error(d)→Error` with `detail = d`, `Destroying→Done`. `reconcile`: for each existing tab not in the daemon list, remove it; for each daemon workspace not present, add a tab named after the workspace to a group named `Unsorted` (create it at the end if missing) with `status` mapped from its state, `adapter: Terminal`, `command: None`. Glyphs: Idle `○`, Working `●`, WaitingPermission `!`, Error `✕`, Done `✓`, Creating `…`, SandboxDown `⏸`. `as_i32` in that order 0..6.

- [ ] **Step 4: Tests, clippy, fmt; commit** — `cargo test -p bondsymphonic-ide --test model_tests`, clippy, fmt.

```bash
git add crates/ide
git commit -m "feat(ide): workspace/group/tab model with reconciliation and file-tree cache"
```

---

### Task 3: Event router and controller workspace operations

**Files:**
- Create: `crates/ide/src/client/router.rs`
- Modify: `crates/ide/src/client/mod.rs` (`pub mod router;`), `crates/ide/src/qobjects/app_controller.rs`
- Test: `crates/ide/tests/router_tests.rs`

**Interfaces:**

```rust
// client/router.rs
#[derive(Clone)]
pub struct EventRouter { inner: Arc<Mutex<Inner>> }
pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<(Option<WorkspaceId>, Event)>;
impl EventRouter {
    pub fn new() -> Self;
    /// Every event, in order (used by AppController for workspace.state and daemon.log).
    pub fn subscribe_all(&self) -> EventRx;
    /// Only events whose `pty_id` matches (PtyOutput/PtyExit). Events that arrived up to 5 s before
    /// the subscription for that id are replayed first (early-output buffer).
    pub fn subscribe_pty(&self, id: &PtyId) -> EventRx;
    pub fn unsubscribe_pty(&self, id: &PtyId);
    /// Called by the reader loop for every event.
    pub fn dispatch(&self, workspace_id: Option<WorkspaceId>, event: Event);
}
```

`AppController` additions (Rust side, callable from other QObjects through a process-wide handle):

```rust
pub struct Shared { pub client: DaemonClient, pub router: EventRouter }
pub fn shared() -> Option<Shared>;             // None until connected; stored in a static OnceLock<Mutex<Option<Shared>>>
```

and `start()` now: after connect, stores `Shared`, subscribes `subscribe_all()`, and in the reader loop calls `router.dispatch(ws, ev)` for every event **and** handles `Event::WorkspaceStateChanged` by queueing `q.workspace_changed(info_json)` (a new `#[qsignal] fn workspace_changed(self: Pin<&mut AppController>, info_json: QString)`) and `Event::DaemonLog` with a message starting `events dropped:` by queueing `q.output_dropped(count)` (`#[qsignal] fn output_dropped(self: Pin<&mut AppController>, count: i64)`). On connect it also requests `workspace.list` and queues `q.workspaces_listed(json)` (`#[qsignal]`) with the JSON array of `WorkspaceInfo`.

Invokables on `AppController` (all async on the runtime; results come back via signals):
- `create_workspace(repo_path: QString, base_branch: QString, name: QString, group: QString, adapter: QString, command: QString)` → on success `workspace_created(info_json, group)`; on error `operation_failed(op: QString, message: QString)`.
- `destroy_workspace(id: QString, force: bool)` → `workspace_destroyed(id)` or `operation_failed`.
- `inspect_repo(path: QString)` → `repo_inspected(path, info_json)` or `operation_failed`.
- `wsl_path(windows_path: QString) -> QString` (uses `launcher::windows_path_to_wsl`, synchronous).

- [ ] **Step 1: Failing router tests** `crates/ide/tests/router_tests.rs`

```rust
use bondsymphonic_ide::client::router::EventRouter;
use bondsymphonic_proto::*;

fn out(pty: &str, s: &str) -> Event { Event::PtyOutput { pty_id: pty.into(), data_b64: s.into() } }

#[tokio::test]
async fn routes_by_pty_and_replays_early_output() {
    let r = EventRouter::new();
    let mut all = r.subscribe_all();
    r.dispatch(Some("ws_1".into()), out("pty_a", "early1"));
    r.dispatch(Some("ws_1".into()), out("pty_b", "other"));
    let mut a = r.subscribe_pty(&"pty_a".into());
    r.dispatch(Some("ws_1".into()), out("pty_a", "live2"));
    r.dispatch(Some("ws_1".into()), Event::PtyExit { pty_id: "pty_a".into(), code: 0 });
    let got: Vec<String> = [a.recv().await, a.recv().await, a.recv().await].into_iter().flatten()
        .map(|(_, e)| match e { Event::PtyOutput { data_b64, .. } => data_b64, Event::PtyExit { code, .. } => format!("exit{code}"), _ => "?".into() }).collect();
    assert_eq!(got, vec!["early1", "live2", "exit0"]);
    assert!(a.try_recv().is_err(), "pty_b output must not reach pty_a");
    // subscribe_all saw everything in order
    let mut n = 0; while all.try_recv().is_ok() { n += 1; }
    assert_eq!(n, 4);
}

#[tokio::test]
async fn early_output_buffer_expires() {
    let r = EventRouter::new();
    r.dispatch(None, out("pty_x", "stale"));
    r.expire_early_buffers_older_than(std::time::Duration::from_secs(0)); // test hook: everything is "old"
    let mut x = r.subscribe_pty(&"pty_x".into());
    assert!(x.try_recv().is_err());
    r.unsubscribe_pty(&"pty_x".into());
    r.dispatch(None, out("pty_x", "after"));
    assert!(x.recv().await.is_none(), "channel closed after unsubscribe");
}
```

Add `pub fn expire_early_buffers_older_than(&self, age: Duration)` as the pruning hook (also called from `dispatch` with 5 s).

- [ ] **Step 2: Implement `router.rs`**: `Inner { all: Vec<UnboundedSender>, pty: HashMap<PtyId, UnboundedSender>, early: HashMap<PtyId, Vec<(Instant, Option<WorkspaceId>, Event)>> }`. `dispatch`: send to all subscribers (drop closed ones); if the event has a `pty_id` and a subscriber exists, send; else push into `early` (cap 256 events per id); prune early entries older than 5 s. `subscribe_pty`: create channel, replay any buffered events for that id, register. `unsubscribe_pty`: remove sender (dropping it closes the receiver).

- [ ] **Step 3: Extend `AppController`** as specified (signals, invokables, `Shared`, reader loop). Keep `set_state`/`apply_daemon_version` unchanged. For `create_workspace`: call `workspace.create`; on success the daemon's `workspace.state` events already flow through `workspace_changed`, so `workspace_created(info_json, group)` is the signal the UI uses to place the tab in the chosen group.

- [ ] **Step 4: Tests, build, clippy, fmt; commit** — `cargo test -p bondsymphonic-ide` (router tests + earlier), `cargo build -p bondsymphonic-ide`, clippy, fmt.

```bash
git add crates/ide
git commit -m "feat(ide): event router with early-output buffer; controller workspace operations and signals"
```

---

### Task 4: Terminal grid model (alacritty_terminal) and key mapping

**Files:**
- Create: `crates/ide/src/model/terminal_grid.rs`
- Modify: `crates/ide/src/model/mod.rs`, `crates/ide/Cargo.toml` (+ `alacritty_terminal = "0.26"`, `vte = "0.15"`, `base64 = "0.22"`)
- Test: `crates/ide/tests/model_tests.rs` (append)

**Interfaces:**

```rust
pub struct TerminalGrid { /* Term<Listener>, Processor, cols, rows, scrollback */ }
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span { pub start: usize, pub len: usize, pub fg: String, pub bg: String, pub bold: bool, pub italic: bool, pub underline: bool, pub inverse: bool }
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row { pub text: String, pub spans: Vec<Span> }
impl TerminalGrid {
    pub fn new(cols: u16, rows: u16) -> Self;              // scrollback 10_000 lines
    pub fn feed(&mut self, bytes: &[u8]);
    pub fn resize(&mut self, cols: u16, rows: u16);
    pub fn size(&self) -> (u16, u16);
    pub fn rows(&self) -> Vec<Row>;                        // visible rows at the current display offset
    pub fn rows_json(&self) -> String;
    pub fn cursor(&self) -> (u16, u16, bool);              // col, row, visible (row relative to the visible area; visible=false when scrolled up)
    pub fn scroll(&mut self, delta_lines: i32);            // positive = towards history
    pub fn scroll_to_bottom(&mut self);
    pub fn title(&self) -> Option<String>;
    pub fn insert_marker(&mut self, text: &str);           // writes "\r\n<text>\r\n" through the parser (used for [output dropped])
}
/// Qt::Key value + Qt::KeyboardModifiers bits + the event's text → bytes for the PTY. Empty when the key produces nothing.
pub fn key_to_bytes(qt_key: i32, modifiers: u32, text: &str, app_cursor_keys: bool) -> Vec<u8>;
pub mod qt { pub const KEY_ESCAPE: i32 = 0x0100_0000; pub const KEY_TAB: i32 = 0x0100_0001; pub const KEY_BACKTAB: i32 = 0x0100_0002; pub const KEY_BACKSPACE: i32 = 0x0100_0003; pub const KEY_RETURN: i32 = 0x0100_0004; pub const KEY_ENTER: i32 = 0x0100_0005; pub const KEY_INSERT: i32 = 0x0100_0006; pub const KEY_DELETE: i32 = 0x0100_0007; pub const KEY_HOME: i32 = 0x0100_0010; pub const KEY_END: i32 = 0x0100_0011; pub const KEY_LEFT: i32 = 0x0100_0012; pub const KEY_UP: i32 = 0x0100_0013; pub const KEY_RIGHT: i32 = 0x0100_0014; pub const KEY_DOWN: i32 = 0x0100_0015; pub const KEY_PAGEUP: i32 = 0x0100_0016; pub const KEY_PAGEDOWN: i32 = 0x0100_0017; pub const KEY_F1: i32 = 0x0100_0030; pub const MOD_SHIFT: u32 = 0x0200_0000; pub const MOD_CTRL: u32 = 0x0400_0000; pub const MOD_ALT: u32 = 0x0800_0000; }
```

- [ ] **Step 1: Failing tests** (append to `model_tests.rs`)

```rust
use bondsymphonic_ide::model::terminal_grid::{key_to_bytes, qt, TerminalGrid};

#[test]
fn grid_renders_text_and_tracks_cursor() {
    let mut g = TerminalGrid::new(20, 5);
    g.feed(b"hello\r\nworld");
    let rows = g.rows();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].text.trim_end(), "hello");
    assert_eq!(rows[1].text.trim_end(), "world");
    assert_eq!(g.cursor(), (5, 1, true));
    g.resize(10, 3);
    assert_eq!(g.size(), (10, 3));
}

#[test]
fn grid_spans_carry_colors_and_bold() {
    let mut g = TerminalGrid::new(20, 3);
    g.feed(b"\x1b[1;31mred\x1b[0m plain");
    let row = &g.rows()[0];
    let red = row.spans.iter().find(|s| s.start == 0).unwrap();
    assert!(red.bold);
    assert!(!red.fg.is_empty(), "fg colour must be set for the red span: {red:?}");
    let plain = row.spans.iter().find(|s| s.start >= 3 && s.len > 0 && !s.bold).unwrap();
    assert!(plain.fg.is_empty() || plain.fg != red.fg);
    let json = g.rows_json();
    assert!(json.contains("\"spans\""));
}

#[test]
fn grid_scrollback_and_marker() {
    let mut g = TerminalGrid::new(10, 3);
    for i in 0..10 { g.feed(format!("line{i}\r\n").as_bytes()); }
    assert!(g.rows()[0].text.starts_with("line8") || g.rows()[0].text.starts_with("line7"));
    g.scroll(5);
    assert!(g.rows()[0].text.starts_with("line"));
    assert!(!g.cursor().2, "cursor hidden while scrolled into history");
    g.scroll_to_bottom();
    assert!(g.cursor().2);
    g.insert_marker("[output dropped]");
    assert!(g.rows().iter().any(|r| r.text.contains("[output dropped]")));
}

#[test]
fn key_mapping() {
    assert_eq!(key_to_bytes(qt::KEY_RETURN, 0, "\r", false), b"\r");
    assert_eq!(key_to_bytes(qt::KEY_BACKSPACE, 0, "\x08", false), b"\x7f");
    assert_eq!(key_to_bytes(qt::KEY_UP, 0, "", false), b"\x1b[A");
    assert_eq!(key_to_bytes(qt::KEY_UP, 0, "", true), b"\x1bOA");
    assert_eq!(key_to_bytes(qt::KEY_F1, 0, "", false), b"\x1bOP");
    assert_eq!(key_to_bytes(qt::KEY_F1 + 4, 0, "", false), b"\x1b[15~");
    assert_eq!(key_to_bytes('C' as i32, qt::MOD_CTRL, "", false), b"\x03");
    assert_eq!(key_to_bytes(qt::KEY_TAB, 0, "\t", false), b"\t");
    assert_eq!(key_to_bytes(qt::KEY_ESCAPE, 0, "", false), b"\x1b");
    assert_eq!(key_to_bytes('a' as i32, 0, "a", false), b"a");
    assert_eq!(key_to_bytes('a' as i32, qt::MOD_ALT, "a", false), b"\x1ba");
    assert_eq!(key_to_bytes(qt::KEY_PAGEUP, 0, "", false), b"\x1b[5~");
    assert!(key_to_bytes(0x0100_0020 /* Shift key alone */, qt::MOD_SHIFT, "", false).is_empty());
}
```

- [ ] **Step 2: Run to see failures** — compile errors.

- [ ] **Step 3: Implement `terminal_grid.rs`**

Use `alacritty_terminal` 0.26. Verify these names against the crate docs (`cargo doc -p alacritty_terminal --open` or the vendored source) before writing; adapt names, keep the public interface above:

```rust
use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{cell::Flags, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

#[derive(Clone)]
struct Listener(std::sync::Arc<std::sync::Mutex<Option<String>>>); // captures title
impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        if let TermEvent::Title(t) = event { *self.0.lock().unwrap() = Some(t); }
    }
}

struct Size { cols: u16, rows: u16 }
impl Dimensions for Size {
    fn total_lines(&self) -> usize { self.rows as usize }
    fn screen_lines(&self) -> usize { self.rows as usize }
    fn columns(&self) -> usize { self.cols as usize }
}

pub struct TerminalGrid { term: Term<Listener>, parser: Processor, cols: u16, rows: u16, title: Listener }

impl TerminalGrid {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cfg = Config { scrolling_history: 10_000, ..Config::default() };
        let listener = Listener(Default::default());
        let term = Term::new(cfg, &Size { cols, rows }, listener.clone());
        Self { term, parser: Processor::new(), cols, rows, title: listener }
    }
    pub fn feed(&mut self, bytes: &[u8]) { self.parser.advance(&mut self.term, bytes); }
    pub fn resize(&mut self, cols: u16, rows: u16) { self.cols = cols.max(2); self.rows = rows.max(1); self.term.resize(Size { cols: self.cols, rows: self.rows }); }
    pub fn size(&self) -> (u16, u16) { (self.cols, self.rows) }
    pub fn scroll(&mut self, delta_lines: i32) { self.term.scroll_display(Scroll::Delta(delta_lines)); }
    pub fn scroll_to_bottom(&mut self) { self.term.scroll_display(Scroll::Bottom); }
    pub fn title(&self) -> Option<String> { self.title.0.lock().unwrap().clone() }
    pub fn insert_marker(&mut self, text: &str) { self.feed(format!("\r\n\x1b[7m{text}\x1b[0m\r\n").as_bytes()); }

    pub fn cursor(&self) -> (u16, u16, bool) {
        let offset = self.term.grid().display_offset();
        let p = self.term.grid().cursor.point;
        let visible = offset == 0 && self.term.mode().contains(TermMode::SHOW_CURSOR);
        (p.column.0 as u16, p.line.0 as u16, visible)
    }

    pub fn rows(&self) -> Vec<Row> {
        let grid = self.term.grid();
        let offset = grid.display_offset();
        let colors = self.term.colors();
        (0..self.rows as usize).map(|screen_line| {
            let line = Line(screen_line as i32 - offset as i32);
            let mut text = String::with_capacity(self.cols as usize);
            let mut spans: Vec<Span> = Vec::new();
            for col in 0..self.cols as usize {
                let cell = &grid[line][Column(col)];
                let ch = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { cell.c };
                text.push(ch);
                let fg = color_hex(cell.fg, colors); let bg = color_hex(cell.bg, colors);
                let (bold, italic, underline, inverse) = (cell.flags.contains(Flags::BOLD), cell.flags.contains(Flags::ITALIC), cell.flags.intersects(Flags::ALL_UNDERLINES), cell.flags.contains(Flags::INVERSE));
                match spans.last_mut() {
                    Some(s) if s.fg == fg && s.bg == bg && s.bold == bold && s.italic == italic && s.underline == underline && s.inverse == inverse => s.len += 1,
                    _ => spans.push(Span { start: col, len: 1, fg, bg, bold, italic, underline, inverse }),
                }
            }
            Row { text, spans }
        }).collect()
    }
    pub fn rows_json(&self) -> String { serde_json::to_string(&self.rows()).unwrap_or_default() }
}

fn color_hex(c: Color, colors: &alacritty_terminal::term::color::Colors) -> String {
    match c {
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => String::new(), // default → widget theme decides
        Color::Spec(Rgb { r, g, b }) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Named(n) => colors[n].map(|Rgb { r, g, b }| format!("#{r:02x}{g:02x}{b:02x}")).unwrap_or_else(|| ansi_default(n as usize)),
        Color::Indexed(i) => colors[i as usize].map(|Rgb { r, g, b }| format!("#{r:02x}{g:02x}{b:02x}")).unwrap_or_else(|| ansi_default(i as usize)),
    }
}

/// Standard 16-colour palette (xterm defaults) plus the 6x6x6 cube and greys for 16..=255.
fn ansi_default(i: usize) -> String { /* table for 0..16 (e.g. 1 => "#cd0000", 9 => "#ff0000"), cube for 16..232, greys for 232..256 */ }
```

`key_to_bytes`: match on the constants in `qt`; F-keys: F1–F4 → `\x1bOP`..`\x1bOS`, F5 `\x1b[15~`, F6 `[17~`, F7 `[18~`, F8 `[19~`, F9 `[20~`, F10 `[21~`, F11 `[23~`, F12 `[24~`; arrows use `\x1bO?` when `app_cursor_keys`; Ctrl+letter (A–Z, case-insensitive) → `(c as u8 & 0x1f)`; Alt+printable → `\x1b` + text; Shift+Tab → `\x1b[Z`; otherwise `text.as_bytes()` (empty for pure modifier keys). `app_cursor_keys` comes from `TermMode::APP_CURSOR` (expose `pub fn app_cursor_keys(&self) -> bool`).

- [ ] **Step 4: Tests, clippy, fmt; commit**

```bash
git add crates/ide
git commit -m "feat(ide): terminal grid over alacritty_terminal with spans, scrollback, and Qt key mapping"
```

---

### Task 5: QObjects — GroupModel, FileTreeModel, TerminalSession

**Files:**
- Create: `crates/ide/src/qobjects/group_model.rs`, `crates/ide/src/qobjects/file_tree.rs`, `crates/ide/src/qobjects/terminal_session.rs`
- Modify: `crates/ide/src/qobjects/mod.rs`, `crates/ide/build.rs` (add the three bridge files)

**Interfaces (Qt-side names camelCase):**

`GroupModel` — property `state_json: QString` (serialised `Workspaces`), signal `changed()`. Invokables: `add_group(name) -> i32`, `rename_group(idx, name) -> bool`, `add_tab(info_json, group_name, adapter, command) -> bool` (creates the group if missing; `info_json` is a `WorkspaceInfo`), `apply_workspace_info(info_json) -> bool`, `reconcile(list_json)`, `remove_workspace(id) -> bool`, `set_active(group_idx, tab_idx) -> bool`, `active_workspace_id() -> QString` (empty when none), `active_tab_json() -> QString`, `group_count() -> i32`, `tab_count(group_idx) -> i32`, `group_name(idx) -> QString`, `tab_label(group_idx, tab_idx) -> QString` (`"<glyph> <name>"`), `tab_status(group_idx, tab_idx) -> i32`, `tab_tooltip(group_idx, tab_idx) -> QString` (repo, branch, adapter, detail). Every mutation re-serialises `state_json` and emits `changed`.

`FileTreeModel` — invokable `load_dir(workspace_id, path)` (async via `shared().client`, `fs.list_dir`; result cached in `FileTree`), signals `entries_loaded(path, entries_json)` and `load_failed(path, message)`; invokables `invalidate(path)`, `is_loaded(path) -> bool`, `cached_entries(path) -> QString`, `set_workspace(workspace_id)` (clears the cache when it changes).

`TerminalSession` — properties: `pty_id: QString`, `workspace_id: QString`, `rows_json: QString`, `cursor_col: i32`, `cursor_row: i32`, `cursor_visible: bool`, `cols: i32`, `rows: i32`, `exited: bool`, `exit_code: i32`, `title: QString`, `error: QString`. Invokables: `open(workspace_id, cols, rows, command)` (empty command = daemon default), `write_text(text)`, `write_key(qt_key, modifiers, text)`, `resize(cols, rows)`, `scroll(delta)`, `scroll_to_bottom()`, `close()`, `note_output_dropped()` (inserts the marker). Signal `frame()` after each batch of output is applied (coalesced: the tokio-side task collects bytes for up to 16 ms before queueing one update), `opened()`, `exited_signal()`.

Rules: all daemon calls go through `app_controller::shared()`; if `None`, set `error` and emit `frame`. The grid lives in the Rust struct; `rows_json` is regenerated only when `frame` is emitted (not on every property read). PTY output bytes are base64-decoded on the tokio side. The PTY id is only known from the `pty.open` reply, so `open` subscribes to the router immediately after the reply arrives; the router replays any output that landed in the meantime (Task 3's early-output buffer).

- [ ] **Step 1: Write the three bridge files** following `app_controller.rs`'s pattern (`#[cxx_qt::bridge]`, `#[auto_cxx_name]`, `impl cxx_qt::Threading`). Keep each Rust struct minimal: `GroupModelRust { state_json, workspaces: Workspaces }`, `FileTreeModelRust { workspace_id: String, tree: FileTree }`, `TerminalSessionRust { grid: Option<TerminalGrid>, pty_id, ..., unsubscribe: Option<Box<dyn FnOnce() + Send>> }`.

- [ ] **Step 2: Unit-test the non-Qt logic** where it lives in `model/` (already covered by Task 2 and 4); for the QObjects add a `tests/qobject_smoke.rs` that constructs `Workspaces` JSON round trips via the same functions the invokables call (`Workspaces::from_json` etc.) — the QObject wrappers themselves are exercised by the app in Tasks 6–9.

- [ ] **Step 3: Build, clippy, fmt; commit** — `cargo build -p bondsymphonic-ide` must succeed with the new bridges (moc/cxx-qt codegen). Confirm the generated headers exist under `target/.../cxxqtbuild/include/bondsymphonic-ide/src/qobjects/`.

```bash
git add crates/ide
git commit -m "feat(ide): GroupModel, FileTreeModel, and TerminalSession QObjects"
```

---

### Task 6: Group bar, New Agent dialog, workspace lifecycle wiring

**Files:**
- Create: `crates/ide/cpp/GroupBar.h`, `crates/ide/cpp/GroupBar.cpp`, `crates/ide/cpp/NewAgentDialog.h`, `crates/ide/cpp/NewAgentDialog.cpp`
- Modify: `crates/ide/cpp/MainWindow.h`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/cpp/app.cpp` (construct `GroupModel`, `FileTreeModel` and pass to `MainWindow`), `crates/ide/build.rs` (cpp files)

**Interfaces:**
- `GroupBar(GroupModel*)`: two `QTabBar`s; rebuilds tabs from the model on `changed` (group names; agent labels from `tabLabel`, tooltips, a coloured status via `setTabTextColor` per `tabStatus`: Idle grey, Working blue, WaitingPermission amber, Error red, Done green, Creating grey italic-ish (use `…`), SandboxDown orange); `+` tool button at the right emits `newAgentRequested()`; group tab change → `model->setActive(g, 0)`; agent tab change → `setActive(currentGroup, idx)`; right-click on an agent tab → context menu "Destroy workspace…" → `destroyRequested(workspaceId)`; on a group tab → "New group…" / "Rename group…".
- `NewAgentDialog(AppController*, GroupModel*, QWidget*)`: fields: repo path (`QLineEdit` + Browse via `QFileDialog::getExistingDirectory`), base branch `QComboBox` (filled from `repoInspected`; editable), name `QLineEdit` (default `agent-N`), adapter `QComboBox` {Terminal} (Claude Code is added in Milestone 4), command `QLineEdit` (placeholder "default shell"), group `QComboBox` (existing group names + "New group…" → inline `QLineEdit`). On repo path edit finished → `controller->inspectRepo(controller->wslPath(path))`. `accept()` is disabled until repo and branch are set. Exposes `repoPath()` (WSL form), `baseBranch()`, `name()`, `adapter()`, `command()`, `group()`.
- `MainWindow` wiring: `newAgentRequested` → dialog → `controller->createWorkspace(...)`; `workspaceCreated(info, group)` → `groupModel->addTab(info, group, adapter, command)` (remember the dialog's adapter/command in a pending map keyed by name until the info arrives); `workspaceChanged(info)` → `groupModel->applyWorkspaceInfo(info)`; `workspacesListed(json)` → `groupModel->reconcile(json)`; `destroyRequested(id)` → confirm dialog with a "Force (discard changes)" checkbox → `controller->destroyWorkspace(id, force)`; `workspaceDestroyed(id)` → `groupModel->removeWorkspace(id)`; `operationFailed(op, msg)` → `QMessageBox::warning`. Status bar: on `groupModel changed` → branch label = active tab's branch, sandbox label = status word.

- [ ] **Step 1: Implement** the two widgets and the wiring. C++ contains no decisions beyond mapping model values to widget calls; label text and status words come from the Rust model (`tabLabel`, `tabTooltip`, and a new invokable `statusWord(g, t)` if needed — add it to `GroupModel` rather than switching in C++).

- [ ] **Step 2: Manual verification (record in the report):** `scripts\run-ide.ps1`; File → New Agent (also the `+` button); pick a repo that lives on Windows (e.g. `C:\git\BondSymphonic` itself — the dialog converts it to `/mnt/c/git/BondSymphonic`) and one that lives in the distro if available; base branch populates; Create → a tab appears with `…` then `○`, and the daemon log shows the sandbox starting; right-click → Destroy → tab disappears. Take a screenshot with the tab visible (`scratchpad\m2b-tabs.png`) and read it.

- [ ] **Step 3: Build, clippy, fmt; commit**

```bash
git add crates/ide
git commit -m "feat(ide): group/agent tab bar, New Agent dialog, workspace create/destroy wiring"
```

---

### Task 7: Terminal widget, agent area, bottom terminal tab

**Files:**
- Create: `crates/ide/cpp/TerminalWidget.h`, `crates/ide/cpp/TerminalWidget.cpp`, `crates/ide/cpp/AgentArea.h`, `crates/ide/cpp/AgentArea.cpp`
- Modify: `crates/ide/cpp/MainWindow.h`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/build.rs`

**Interfaces:**
- `TerminalWidget(TerminalSession* session, QWidget* parent)`: owns nothing but the pointer; on `session->frame()` calls `update()`; `paintEvent` parses `session->getRowsJson()` (a `QJsonDocument`), draws each row's spans with the monospace font from `QFontDatabase::systemFont(FixedFont)` (size 10), background rectangles per span (`bg` empty → widget palette base), foreground per span (`fg` empty → palette text), bold/italic/underline via `QFont` flags, inverse swaps the two; draws a block cursor at (`cursorCol`, `cursorRow`) when `cursorVisible`; `keyPressEvent` → `session->writeKey(e->key(), e->modifiers(), e->text())` and accepts the event (Tab included: override `focusNextPrevChild` to return false); `wheelEvent` → `session->scroll(-angleDelta().y()/40)` (3 lines per notch); `resizeEvent` → computes `cols = width / charWidth`, `rows = height / lineHeight` and calls `session->resize(cols, rows)` when they changed; `focusInEvent` starts a 500 ms blink timer, `focusOutEvent` stops it; `sizeHint` = 80×24 cells. Shows `session->getError()` centred in red when non-empty and `getExited()` → draws a dim "[process exited with code N]" line under the last row. No other logic.
- `AgentArea(GroupModel*, QWidget*)`: `QStackedWidget`; `showWorkspace(workspaceId, adapter, command)` creates (once) a `TerminalSession` + `TerminalWidget` for terminal-adapter tabs, opening the PTY with `command` (empty = daemon default) and `cols/rows` from the widget; `removeWorkspace(id)` closes the session and deletes the widget; a placeholder `QLabel("No agent selected")` page when nothing is active.
- `MainWindow`: the bottom dock's "Terminal" tab becomes a `QStackedWidget` of one shell `TerminalWidget` per workspace (created lazily on first activation, default command); active-tab changes (from `GroupModel::changed`) switch both the agent area and the bottom terminal; `workspaceDestroyed` removes both; `AppController::outputDropped` → every live `TerminalSession::noteOutputDropped()`.

- [ ] **Step 1: Implement** the widgets and wiring. Painting must be per-span, not per-cell (one `drawText` per span). Sizing: cache `QFontMetrics` `horizontalAdvance("M")` and `lineSpacing()`.

- [ ] **Step 2: Manual verification (record in the report with a screenshot `scratchpad\m2b-terminal.png`):** create a workspace on a repo; the bottom Terminal tab shows a `bash -l` prompt from inside the sandbox; type `id; pwd; ls; touch /etc/x; echo $?` → uid `bs`-mapped, cwd is the worktree, `touch` fails; resize the dock and confirm `stty size` reports the new size; run `ls --color` and confirm colours; `top` then `q` to prove full-screen apps redraw; close with `exit` → the widget shows the exit line. Switch to a second workspace and back: each keeps its own scrollback.

- [ ] **Step 3: Build, clippy, fmt; commit**

```bash
git add crates/ide
git commit -m "feat(ide): terminal widget over TerminalSession; per-workspace agent pane and shell tab"
```

---

### Task 8: Explorer file tree

**Files:**
- Create: `crates/ide/cpp/ExplorerDock.h`, `crates/ide/cpp/ExplorerDock.cpp`
- Modify: `crates/ide/cpp/MainWindow.h`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/build.rs`

**Interfaces:**
- `ExplorerDock(FileTreeModel*, QWidget*)`: `QDockWidget` "Explorer" with a `QTabWidget` {Files, Changes}; Files is a `QTreeView` over a `QStandardItemModel` with one column; each directory item carries a placeholder child until loaded; `expanded(index)` → `model->loadDir(workspaceId, path)` if not loaded; `entriesLoaded(path, json)` → replaces that directory's children with items (folder icon `QStyle::SP_DirIcon`, file icon `SP_FileIcon`, `Qt::UserRole` = relative path, size in the tooltip); `loadFailed(path, msg)` → the placeholder child becomes an italic error item; `setWorkspace(id)` clears the tree, calls `model->setWorkspace(id)` and loads `""`; `Changes` stays an empty `QTreeView` (Milestone 3). Signal `fileActivated(QString path)` on double-click of a file (unused until Milestone 3).
- `MainWindow`: replaces the placeholder Explorer dock from Milestone 1 with `ExplorerDock`; on active-tab change → `explorer->setWorkspace(activeId)`; a "Refresh" toolbar action → `model->invalidate("")` + reload.

- [ ] **Step 1: Implement.** Sorting and `is_dir` come from the daemon/model; C++ only maps entries to items.

- [ ] **Step 2: Manual verification (screenshot `scratchpad\m2b-tree.png`):** the tree shows the worktree root, expands `crates/` lazily, refresh after `touch newfile` in the terminal shows the new file.

- [ ] **Step 3: Build, clippy, fmt; commit**

```bash
git add crates/ide
git commit -m "feat(ide): lazy worktree file tree in the Explorer dock"
```

---

### Task 9: End-to-end verification, smoke test, footprint, docs

**Files:**
- Create: `crates/ide/tests/smoke.rs`
- Modify: `README.md`, `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§13 footprint numbers measured), `.github/workflows/ci.yml` (no change unless needed)

**Interfaces:** none new.

- [ ] **Step 1: Offscreen smoke test** `crates/ide/tests/smoke.rs`: starts a fake daemon in-process (reuse the pattern from `tests/client_tests.rs`: answers `hello`, `workspace.list` (empty), `workspace.create` (returns a `WorkspaceInfo` and emits `workspace.state` Creating/Ready), `pty.open` (returns an id and emits one `pty.output` "prompt$ " then nothing), `fs.list_dir` (two entries)); sets `QT_QPA_PLATFORM=offscreen`; launches the IDE binary via `std::process::Command` with an env var `BS_DAEMON_ADDR=127.0.0.1:<port>` and `BS_DAEMON_TOKEN=<token>` that the launcher honours **as a test hook** (add to `launcher.rs`: if `BS_DAEMON_ADDR` is set, skip `wsl.exe` and connect directly — document it as test-only) and `BS_SMOKE_SCRIPT=create,open,tree,quit` that `AppController::start` reads and performs after connecting (create a workspace named `smoke` in group `Default`, open a PTY, load the tree root, then `QApplication::quit()` after 2 s); the test asserts the process exits 0 within 30 s and that the fake daemon saw those requests in order. Mark `#[ignore]` if Qt is not on PATH (check `QMAKE`), printing the reason.
- [ ] **Step 2: Footprint measurement:** run the IDE with two workspaces open (each with the agent terminal and the shell tab) for 60 s and record idle RSS of `bondsymphonic-ide.exe` (`Get-Process bondsymphonic-ide | Select WorkingSet64`) and of the daemon (`wsl -d bondsymphonic -- ps -o rss= -C bondsymphonic-daemon`); write both numbers into the IDE spec §13 and the report. Targets: IDE < 150 MB, daemon < 30 MB (excluding sandboxed processes). If the IDE exceeds it, profile once (`rows_json` frequency, font cache) and note the finding; do not optimise beyond one obvious fix.
- [ ] **Step 3: Docs:** README "What works now" section (create workspace, terminal inside the sandbox, file tree); protocol notes unchanged.
- [ ] **Step 4: Full verification:** Windows `cargo test --workspace` (env sourced), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`; WSL `scripts\test-daemon.ps1`; `.\launch.ps1` end-to-end with the acceptance mini-scenario: two workspaces on one repo, both terminals usable at once, destroy one, the other keeps working. Screenshot `scratchpad\m2b-final.png`.
- [ ] **Step 5: Commit**

```bash
git add crates/ide README.md docs
git commit -m "test(ide): offscreen smoke test against a fake daemon; footprint numbers; docs"
```

---

## Milestone 2b exit criteria

- From the IDE: New Agent → workspace created in a chosen group; the agent tab shows live status from `workspace.state` events; destroy from the tab's context menu works (with a force option) and the tab disappears.
- The bottom Terminal tab and the agent pane each run a real PTY inside the workspace's bubblewrap sandbox with correct colours, resize, and scrollback; two workspaces keep independent terminals; a lagging client sees an `[output dropped]` marker instead of silent corruption.
- The Explorer shows the worktree lazily and refreshes.
- Offscreen smoke test passes on Windows; clippy and fmt clean; daemon suite still green in WSL; footprint numbers recorded.
