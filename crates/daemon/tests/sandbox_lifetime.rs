#![cfg(target_os = "linux")]
//! A sandbox has to outlive the thread that happened to fork it.
//!
//! `bwrap --die-with-parent` arms `PR_SET_PDEATHSIG(SIGKILL)`, and the kernel
//! keys that signal to the *thread* that created the process, not to the parent
//! process. A fork that lands on a tokio thread which later retires — the
//! blocking pool drops an idle thread ten seconds after it goes idle — takes
//! the whole sandbox with it, about ten seconds after `workspace.create`
//! returns.
//!
//! Three things about this test are load-bearing, and each of them alone is
//! enough to make a broken daemon pass:
//!
//! * `flavor = "multi_thread"`. The current-thread runtime `#[tokio::test]`
//!   builds by default has no worker pool and no thread to retire.
//! * The create runs **through the server**, as a real client's request does,
//!   so it is polled on a worker rather than on the thread that called
//!   `block_on`. A future driven by `block_on` holds no worker core, and
//!   `block_in_place` on such a thread is a no-op that orphans nothing.
//! * The workspace is left strictly alone for the fifteen seconds afterwards.
//!   A live PTY hides the bug: every read on a pty master is a
//!   `spawn_blocking`, which hands the idle thread new work and restarts its
//!   keep-alive timer.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::dispatch::SystemHandler;
use bondsymphonic_daemon::server::handlers::WorkspaceHandler;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::DataDirs;
use bondsymphonic_proto::{Request, WorkspaceCreateParams, WorkspaceInfo, WorkspaceState};
use common::Client;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-all",
            "--die-with-parent",
            "true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The pids whose command line carries `pattern`. A sandbox's `bwrap` argv
/// names its own run directory, so this identifies exactly one workspace's
/// sandbox even while the rest of the suite runs its own.
fn pgrep_pids(pattern: &str) -> Vec<i32> {
    let out = match std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// A server on an ephemeral port over the real bubblewrap backend — the same
/// wiring `common::start_daemon` builds, with the sandbox that actually forks
/// `bwrap` instead of the no-op one.
async fn start_bwrap_daemon(
    root: &std::path::Path,
) -> (u16, String, Arc<Daemon>, CancellationToken) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(root),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let handler = Arc::new(WorkspaceHandler {
        system: SystemHandler {
            token: server.token().to_string(),
            capabilities: ServerConfig::default().capabilities,
        },
        daemon: daemon.clone(),
    });
    let (port, token) = (server.port(), server.token().to_string());
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.with_handler(handler).run(c2).await.unwrap() });
    (port, token, daemon, cancel)
}

/// Comfortably past tokio's blocking-pool `KEEP_ALIVE` of ten seconds, so a
/// thread that went idle at the end of the create has certainly retired.
const PAST_THE_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// A workspace nobody touches after creating it must still have its sandbox a
/// quarter of a minute later.
///
/// This is what a user who opens a tab and reads the diff before typing does:
/// the connection stays open, the workspace stays `Ready`, and nothing is asked
/// of the sandbox at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_sandbox_outlives_the_blocking_pool_keep_alive() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_bwrap_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "idle".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

    let run_dir = daemon.dirs.run(&ws.id).to_string_lossy().into_owned();
    let before = pgrep_pids(&run_dir);
    assert!(!before.is_empty(), "no bwrap process for {run_dir}");

    tokio::time::sleep(PAST_THE_KEEP_ALIVE).await;

    let after = pgrep_pids(&run_dir);
    assert_eq!(
        after, before,
        "the sandbox was killed while the workspace sat idle: {before:?} became {after:?}"
    );
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Ready,
        "an idle workspace must not fall to SandboxDown"
    );
    assert!(
        daemon.sandbox(&ws.id).is_ok(),
        "the sandbox handle must still be registered"
    );

    c.call(Request::WorkspaceDestroy(
        bondsymphonic_proto::WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: true,
        },
    ))
    .await
    .unwrap();
    cancel.cancel();
}
