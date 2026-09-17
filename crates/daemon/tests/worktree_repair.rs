//! A workspace whose worktree registration was pruned from under it.
//!
//! A git on Windows cannot see a worktree under the WSL user's home, so its
//! `git worktree prune` deletes `<repo>/.git/worktrees/<id>` and leaves the
//! directory and the branch behind. The daemon puts the registration back when
//! a workspace is restored or restarted, and says why when it cannot.

mod common;

use async_trait::async_trait;
use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::git::worktree::{self, Layout};
use bondsymphonic_daemon::sandbox::{backend_for, SandboxBackend, SandboxHandle, SandboxSpec};
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{create_ws, start_daemon, Client};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a Windows `git worktree prune` does to a registration.
fn prune_like_windows_git(layout: &Layout) {
    std::fs::remove_dir_all(layout.worktree_gitdir()).unwrap();
}

/// A workspace on a throwaway repository with an uncommitted file in it, whose
/// registration has then been pruned. Returns the data dir, the workspace and
/// its layout; the daemon that made it is shut down.
async fn pruned_workspace(dir: &Path, name: &str) -> (PathBuf, WorkspaceInfo, Layout) {
    let repo = common::init_repo(dir);
    let data = dir.join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, name).await;
    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    std::fs::write(w.worktree_path.join("wip.txt"), "uncommitted\n").unwrap();
    shut_down_sandboxes(&daemon).await;
    cancel.cancel();
    prune_like_windows_git(&layout);
    (data, ws, layout)
}

async fn shut_down_sandboxes(d: &Daemon) {
    let handles: Vec<Arc<dyn SandboxHandle>> = d.sandboxes.lock().drain().map(|(_, h)| h).collect();
    for h in handles {
        let _ = h.shutdown().await;
    }
}

fn daemon_over(data: &Path, backend: Arc<dyn SandboxBackend>) -> (Arc<Daemon>, EventBus) {
    let events = EventBus::new(256);
    let d = Daemon::new(DataDirs::new(data), backend, events.clone()).unwrap();
    (d, events)
}

/// The `daemon.log` messages published for `id` so far.
fn logs_for(
    rx: &mut tokio::sync::broadcast::Receiver<ServerMessage>,
    id: &WorkspaceId,
) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let ServerMessage::Event {
            workspace_id: Some(w),
            event: Event::DaemonLog { message, .. },
        } = msg
        {
            if &w == id {
                out.push(message);
            }
        }
    }
    out
}

/// The registration is back, locked, and git in the worktree works and still
/// sees the uncommitted file.
fn assert_repaired(layout: &Layout) {
    let gitdir = layout.worktree_gitdir();
    assert!(
        gitdir.join("HEAD").is_file(),
        "the registration is not back"
    );
    assert_eq!(
        std::fs::read_to_string(gitdir.join("locked"))
            .unwrap()
            .trim_end(),
        worktree::LOCK_REASON
    );
    let status = common::git_out(&layout.worktree_path, &["status", "--porcelain"]);
    assert!(status.contains("?? wip.txt"), "{status}");
    // A prune, with the directory present, keeps it; the lock is what keeps it
    // when the directory is invisible, which `git_worktree.rs` shows.
    common::git_ok(&layout.repo, &["worktree", "prune"]);
    let listed = common::git_out(&layout.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains(&format!("branch refs/heads/{}", layout.branch)),
        "{listed}"
    );
    assert!(gitdir.join("HEAD").is_file());
}

#[tokio::test]
async fn restore_re_registers_a_pruned_worktree_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (data, ws, layout) = pruned_workspace(dir.path(), "restored").await;

    let (daemon, events) = daemon_over(&data, backend_for("noop"));
    let mut rx = events.subscribe();
    daemon.restore().await;

    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Ready
    );
    assert!(daemon.sandbox(&ws.id).is_ok());
    assert_repaired(&layout);
    let logs = logs_for(&mut rx, &ws.id);
    assert!(
        logs.iter()
            .any(|m| m.starts_with("Re-registered this workspace's worktree")),
        "{logs:?}"
    );
    shut_down_sandboxes(&daemon).await;
}

#[tokio::test]
async fn restart_re_registers_a_pruned_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "restarted").await;
    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    std::fs::write(w.worktree_path.join("wip.txt"), "uncommitted\n").unwrap();
    prune_like_windows_git(&layout);
    daemon
        .set_state(&ws.id, WorkspaceState::SandboxDown)
        .await
        .unwrap();
    c.drain_events();

    let info: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceRestart(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    assert_eq!(info.state, WorkspaceState::Ready);
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Ready
    );
    assert_repaired(&layout);
    let events = c.drain_events();
    assert!(
        events.iter().any(|(w, e)| w.as_ref() == Some(&ws.id)
            && matches!(e, Event::DaemonLog { message, .. }
                if message.starts_with("Re-registered this workspace's worktree"))),
        "{events:?}"
    );
    assert!(
        events.iter().any(|(_, e)| matches!(e,
            Event::WorkspaceStateChanged { info } if info.id == ws.id && info.state == WorkspaceState::Ready)),
        "{events:?}"
    );
    cancel.cancel();
    shut_down_sandboxes(&daemon).await;
}

/// A restart of a healthy workspace is a sandbox restart: the old sandbox goes,
/// a new one takes its place, and nothing is repaired.
#[tokio::test]
async fn restart_of_a_ready_workspace_replaces_its_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "healthy").await;
    let before = daemon.sandbox(&ws.id).unwrap();
    let mut rx = daemon.events.subscribe();

    let info = lifecycle::restart(&daemon, &ws.id).await.unwrap();

    assert_eq!(info.state, WorkspaceState::Ready);
    let after = daemon.sandbox(&ws.id).unwrap();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "the sandbox was not replaced"
    );
    let logs = logs_for(&mut rx, &ws.id);
    assert!(
        !logs.iter().any(|m| m.starts_with("Re-registered")),
        "{logs:?}"
    );
    cancel.cancel();
    shut_down_sandboxes(&daemon).await;
}

#[tokio::test]
async fn an_unrepairable_worktree_is_an_error_with_a_reason_on_restore_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (data, ws, layout) = pruned_workspace(dir.path(), "branchless").await;
    // The branch tip is in the private objects, so `-D` is what deletes it.
    common::git_ok(&layout.repo, &["branch", "-D", &layout.branch]);

    let (daemon, _events) = daemon_over(&data, backend_for("noop"));
    daemon.restore().await;

    let reason = match daemon.registry.get(&ws.id).unwrap().state {
        WorkspaceState::Error(m) => m,
        other => panic!("expected Error, got {other:?}"),
    };
    assert!(
        reason.contains("no longer lists this workspace's worktree"),
        "{reason}"
    );
    assert!(reason.contains(&layout.branch), "{reason}");
    assert!(daemon.sandbox(&ws.id).is_err());

    let err = lifecycle::restart(&daemon, &ws.id).await.unwrap_err();
    assert_eq!(err.message, reason);
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Error(reason)
    );
    assert!(daemon.sandbox(&ws.id).is_err());
}

#[tokio::test]
async fn restart_names_a_missing_worktree_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "dirless").await;
    shut_down_sandboxes(&daemon).await;
    std::fs::remove_dir_all(&ws.worktree_path).unwrap();

    let err = c
        .call(Request::WorkspaceRestart(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap_err();

    assert!(err.message.contains(&ws.worktree_path), "{}", err.message);
    assert!(err.message.contains("missing"), "{}", err.message);
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Error(err.message.clone())
    );
    cancel.cancel();
}

#[tokio::test]
async fn restart_is_refused_while_a_workspace_is_being_created_or_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "busy").await;

    for state in [WorkspaceState::Creating, WorkspaceState::Destroying] {
        daemon.set_state(&ws.id, state.clone()).await.unwrap();
        let err = lifecycle::restart(&daemon, &ws.id).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams, "{state:?}: {err:?}");
        assert_eq!(daemon.registry.get(&ws.id).unwrap().state, state);
    }
    cancel.cancel();
    shut_down_sandboxes(&daemon).await;
}

/// The no-sandbox backend with a `start` that always fails, the way bwrap does
/// on a host where it cannot create namespaces.
struct FailingBackend;

#[async_trait]
impl SandboxBackend for FailingBackend {
    fn name(&self) -> &'static str {
        "failing"
    }
    async fn check(&self) -> Vec<PrereqStatus> {
        Vec::new()
    }
    async fn start(&self, _spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        Err(RpcError::new(
            ErrorCode::SandboxError,
            "bwrap: setting up uid map: Permission denied",
        ))
    }
}

#[tokio::test]
async fn a_sandbox_that_will_not_start_is_an_error_that_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, first, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "unstartable").await;
    shut_down_sandboxes(&first).await;
    cancel.cancel();

    let (daemon, _events) = daemon_over(&data, Arc::new(FailingBackend));
    daemon.restore().await;

    let reason = match daemon.registry.get(&ws.id).unwrap().state {
        WorkspaceState::Error(m) => m,
        other => panic!("expected Error, got {other:?}"),
    };
    assert!(reason.contains("sandbox could not be started"), "{reason}");
    assert!(reason.contains("Permission denied"), "{reason}");

    let err = lifecycle::restart(&daemon, &ws.id).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::SandboxError);
    assert_eq!(err.message, reason);
}

// ---------------------------------------------------------------------------
// Restart, restore and destroy of one workspace, interleaved.
//
// A fake backend makes the interleavings deterministic: its `start` can be
// slow, and its handle's `shutdown` announces the death first and returns
// only after a pause, which is the window in which a death watcher runs.
// ---------------------------------------------------------------------------

struct FakeHandle {
    inner: Arc<dyn SandboxHandle>,
    died: tokio::sync::watch::Sender<bool>,
    shut: std::sync::atomic::AtomicBool,
    shutdown_pause: std::time::Duration,
}

#[async_trait]
impl SandboxHandle for FakeHandle {
    async fn spawn(
        &self,
        cmd: bondsymphonic_daemon::sandbox::SandboxCommand,
    ) -> Result<bondsymphonic_daemon::sandbox::SandboxChild, RpcError> {
        self.inner.spawn(cmd).await
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        self.shut.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.died.send(true);
        tokio::time::sleep(self.shutdown_pause).await;
        self.inner.shutdown().await
    }
    fn helper_exe(&self) -> PathBuf {
        self.inner.helper_exe()
    }
    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        Some(self.died.subscribe())
    }
}

struct FakeBackend {
    start_delay: std::time::Duration,
    shutdown_pause: std::time::Duration,
    handles: parking_lot::Mutex<Vec<Arc<FakeHandle>>>,
}

impl FakeBackend {
    fn new(start_delay_ms: u64, shutdown_pause_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            start_delay: std::time::Duration::from_millis(start_delay_ms),
            shutdown_pause: std::time::Duration::from_millis(shutdown_pause_ms),
            handles: parking_lot::Mutex::new(Vec::new()),
        })
    }
    fn started(&self) -> usize {
        self.handles.lock().len()
    }
    /// Handles that were started and never shut down.
    fn alive(&self) -> usize {
        self.handles
            .lock()
            .iter()
            .filter(|h| !h.shut.load(std::sync::atomic::Ordering::SeqCst))
            .count()
    }
    fn last(&self) -> Arc<FakeHandle> {
        self.handles.lock().last().unwrap().clone()
    }
}

#[async_trait]
impl SandboxBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn check(&self) -> Vec<PrereqStatus> {
        Vec::new()
    }
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        tokio::time::sleep(self.start_delay).await;
        let inner = backend_for("noop").start(spec).await?;
        let handle = Arc::new(FakeHandle {
            inner,
            died: tokio::sync::watch::channel(false).0,
            shut: std::sync::atomic::AtomicBool::new(false),
            shutdown_pause: self.shutdown_pause,
        });
        self.handles.lock().push(handle.clone());
        Ok(handle)
    }
}

async fn create_on(d: &Arc<Daemon>, repo: &Path, name: &str) -> WorkspaceInfo {
    let ws = lifecycle::create(
        d,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: name.into(),
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    ws
}

fn saw_sandbox_down(
    rx: &mut tokio::sync::broadcast::Receiver<ServerMessage>,
    id: &WorkspaceId,
) -> bool {
    let mut seen = false;
    while let Ok(msg) = rx.try_recv() {
        if let ServerMessage::Event {
            event: Event::WorkspaceStateChanged { info },
            ..
        } = msg
        {
            seen |= &info.id == id && info.state == WorkspaceState::SandboxDown;
        }
    }
    seen
}

/// The old sandbox's death watcher must not report the death a restart causes
/// on purpose. The fake announces the death at the start of `shutdown` and
/// returns 200 ms later, so a restart that shut the handle down while it was
/// still the registered one would have its watcher publish `SandboxDown`.
#[tokio::test]
async fn a_restart_does_not_report_its_own_sandbox_dying() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let backend = FakeBackend::new(0, 200);
    let (daemon, events) = daemon_over(&dir.path().join("data"), backend.clone());
    let ws = create_on(&daemon, &repo, "quiet").await;
    let mut rx = events.subscribe();

    lifecycle::restart(&daemon, &ws.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(!saw_sandbox_down(&mut rx, &ws.id));
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Ready
    );
    let current: Arc<dyn SandboxHandle> = backend.last();
    assert!(Arc::ptr_eq(&daemon.sandbox(&ws.id).unwrap(), &current));
    assert_eq!((backend.started(), backend.alive()), (2, 1));
    shut_down_sandboxes(&daemon).await;
}

/// A destroy that arrives while a restart is starting the sandbox waits for
/// it and takes the new sandbox down with the workspace.
#[tokio::test]
async fn a_destroy_during_a_restart_leaves_no_sandbox_behind() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let backend = FakeBackend::new(300, 0);
    let (daemon, _events) = daemon_over(&dir.path().join("data"), backend.clone());
    let ws = create_on(&daemon, &repo, "raced").await;

    let restarting = {
        let d = daemon.clone();
        let id = ws.id.clone();
        tokio::spawn(async move { lifecycle::restart(&d, &id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    let _ = restarting.await.unwrap();

    assert!(daemon.registry.get(&ws.id).is_none());
    assert!(
        daemon.sandboxes.lock().is_empty(),
        "a sandbox outlived its workspace"
    );
    assert_eq!(backend.alive(), 0, "a started sandbox was never shut down");
}

/// A second daemon over the data directory of a first one that made the
/// workspace and whose sandboxes are down: what a daemon start looks like.
async fn restarted_daemon(
    dir: &Path,
    name: &str,
    backend: Arc<FakeBackend>,
) -> (Arc<Daemon>, WorkspaceId) {
    let repo = common::init_repo(dir);
    let data = dir.join("data");
    let (first, _) = daemon_over(&data, backend_for("noop"));
    let ws = create_on(&first, &repo, name).await;
    shut_down_sandboxes(&first).await;
    let (daemon, _events) = daemon_over(&data, backend);
    (daemon, ws.id)
}

/// A Retry that reaches the daemon while its startup restore is still going
/// must not end with two sandboxes for one workspace.
#[tokio::test]
async fn a_restart_during_the_startup_restore_starts_one_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend::new(300, 0);
    let (daemon, id) = restarted_daemon(dir.path(), "retried", backend.clone()).await;
    let snapshot = lifecycle::restore_snapshot(&daemon);

    let restarting = {
        let d = daemon.clone();
        let id = id.clone();
        tokio::spawn(async move { lifecycle::restart(&d, &id).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    daemon.restore_workspaces_from(snapshot).await;
    restarting.await.unwrap().unwrap();

    assert_eq!(backend.started(), 1, "the restore started a second sandbox");
    assert_eq!(backend.alive(), 1);
    assert_eq!(
        daemon.registry.get(&id).unwrap().state,
        WorkspaceState::Ready
    );
    shut_down_sandboxes(&daemon).await;
}

/// A workspace removed while the startup restore had not reached it yet is
/// not brought back up.
#[tokio::test]
async fn the_startup_restore_skips_a_workspace_destroyed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend::new(0, 0);
    let (daemon, id) = restarted_daemon(dir.path(), "removed", backend.clone()).await;
    let snapshot = lifecycle::restore_snapshot(&daemon);

    lifecycle::destroy(&daemon, &id, true).await.unwrap();
    daemon.restore_workspaces_from(snapshot).await;

    assert_eq!(backend.started(), 0);
    assert!(daemon.registry.get(&id).is_none());
    assert!(daemon.sandboxes.lock().is_empty());
}

/// A non-forced destroy of a workspace whose registration is gone cannot ask
/// git whether the worktree is dirty. It answers the way a dirty one does, a
/// `Conflict` the IDE turns into "discard anyway?", rather than a raw
/// "not a git repository".
#[tokio::test]
async fn a_destroy_without_force_of_an_unregistered_worktree_is_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let (data, ws, layout) = pruned_workspace(dir.path(), "unlisted").await;
    common::git_ok(&layout.repo, &["branch", "-D", &layout.branch]);
    let (daemon, _events) = daemon_over(&data, backend_for("noop"));

    let err = lifecycle::destroy(&daemon, &ws.id, false)
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert_eq!(err.data.unwrap()["dirty"], serde_json::json!(true));
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

/// A workspace the last daemon left `Creating` or `Destroying` was in the
/// middle of something nobody is going to finish. It is not brought up (a
/// half-removed worktree must not come back `Ready`) but put in `Error` with
/// what to do.
#[tokio::test]
async fn the_startup_restore_reports_an_interrupted_create_or_removal() {
    for state in [WorkspaceState::Creating, WorkspaceState::Destroying] {
        let dir = tempfile::tempdir().unwrap();
        let backend = FakeBackend::new(0, 0);
        let (daemon, id) = restarted_daemon(dir.path(), "interrupted", backend.clone()).await;
        daemon.set_state(&id, state.clone()).await.unwrap();
        let snapshot = lifecycle::restore_snapshot(&daemon);

        daemon.restore_workspaces_from(snapshot).await;

        assert_eq!(backend.started(), 0, "{state:?}");
        match daemon.registry.get(&id).unwrap().state {
            WorkspaceState::Error(m) => {
                assert!(m.contains("interrupted"), "{state:?}: {m}");
                assert!(m.contains("Remove the workspace"), "{state:?}: {m}");
            }
            other => panic!("{state:?}: expected Error, got {other:?}"),
        }
    }
}
