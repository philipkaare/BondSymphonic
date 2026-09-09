//! Drives a connected IDE with nobody at the keyboard. **Test-only**: with
//! `BS_SMOKE_SCRIPT` unset, which is every ordinary run, [`script`] returns
//! `None`, no task is spawned, and nothing in this module executes.
//!
//! The variable holds a comma-separated step list, run in order once the daemon
//! connection is up. `tests/smoke.rs` runs the real IDE binary offscreen against
//! an in-process fake daemon with `create,open,tree,quit`. The steps are:
//!
//! * `create` — create a workspace over the repository in `BS_SMOKE_REPO` and
//!   emit `workspace_created`, so the window builds its tab, its terminal pane
//!   and its file tree from the same signal a user's New Agent would produce.
//! * `open` — open a PTY in the workspace the last `create` made.
//! * `tree` — list that workspace's root directory.
//! * `quit` — let the window settle, then end the process with status 0.
//!
//! A failing step logs and stops the script *without* quitting, so a broken run
//! is a process that never exits rather than a green exit code.

use crate::client::DaemonClient;
use crate::qobjects::app_controller::QtHandle;
use bondsymphonic_proto::*;
use cxx_qt_lib::QString;
use std::time::Duration;

/// The comma-separated step list. Unset in every ordinary run.
const SCRIPT_ENV: &str = "BS_SMOKE_SCRIPT";
/// The repository `create` builds its workspace from. A fake daemon ignores
/// it, so the default only has to be a path shape.
const REPO_ENV: &str = "BS_SMOKE_REPO";
const DEFAULT_REPO: &str = "/smoke/repo";
const BASE_BRANCH: &str = "main";
/// The group the created tabs are filed under.
const GROUP: &str = "Default";
/// How long `quit` leaves the window running before ending the process.
const QUIT_DELAY: Duration = Duration::from_secs(2);
const PTY_COLS: u16 = 80;
const PTY_ROWS: u16 = 24;

/// The steps in `BS_SMOKE_SCRIPT`, or `None` when it is unset or empty.
pub(crate) fn script() -> Option<Vec<String>> {
    let raw = std::env::var(SCRIPT_ENV).ok()?;
    let steps: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    (!steps.is_empty()).then_some(steps)
}

/// Runs `steps` in order against the connected daemon.
pub(crate) async fn run(steps: Vec<String>, client: DaemonClient, qt: QtHandle) {
    let repo = std::env::var(REPO_ENV).unwrap_or_else(|_| DEFAULT_REPO.to_owned());
    let mut workspace: Option<WorkspaceId> = None;
    let mut created = 0usize;
    for step in steps {
        tracing::info!(target: "smoke", "step: {step}");
        let outcome = match step.as_str() {
            "create" => {
                created += 1;
                create(&client, &qt, &repo, created)
                    .await
                    .map(|id| workspace = Some(id))
            }
            "open" => open(&client, workspace.as_ref()).await,
            "tree" => tree(&client, workspace.as_ref()).await,
            "quit" => quit(&qt).await,
            other => Err(format!("unknown step {other:?}")),
        };
        if let Err(e) = outcome {
            tracing::error!(target: "smoke", "step {step:?} failed: {e}");
            return;
        }
    }
}

/// Creates a workspace and announces it with the signal the
/// `create_workspace` invokable emits, which is what the window listens for.
async fn create(
    client: &DaemonClient,
    qt: &QtHandle,
    repo: &str,
    nth: usize,
) -> Result<WorkspaceId, String> {
    let name = if nth == 1 {
        "smoke".to_owned()
    } else {
        format!("smoke{nth}")
    };
    let info = client
        .request::<WorkspaceInfo>(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_owned(),
            base_branch: BASE_BRANCH.to_owned(),
            name,
        }))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(target: "smoke", "created {:?}", info.id);
    let json = serde_json::to_string(&info).unwrap_or_default();
    let _ = qt.queue(move |q| {
        q.workspace_created(
            QString::from(&json),
            QString::from(GROUP),
            QString::from("terminal"),
            QString::from(""),
        )
    });
    Ok(info.id)
}

async fn open(client: &DaemonClient, workspace: Option<&WorkspaceId>) -> Result<(), String> {
    let workspace_id = need(workspace, "open")?;
    let res = client
        .request::<PtyOpenResult>(Request::PtyOpen(PtyOpenParams {
            workspace_id,
            cols: PTY_COLS,
            rows: PTY_ROWS,
            command: None,
        }))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(target: "smoke", "opened {:?}", res.pty_id);
    Ok(())
}

async fn tree(client: &DaemonClient, workspace: Option<&WorkspaceId>) -> Result<(), String> {
    let workspace_id = need(workspace, "tree")?;
    let res = client
        .request::<ListDirResult>(Request::FsListDir(FsPathParams {
            workspace_id,
            path: String::new(),
        }))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(target: "smoke", "root listed: {} entries", res.entries.len());
    Ok(())
}

/// Ends the run from the Qt thread, so an event loop that has wedged never
/// reaches it and the test times out rather than passing.
///
/// This is `std::process::exit`, not `QApplication::quit()`. `quit` is a
/// static member, which cxx cannot bind, and cxx-qt-lib exposes neither it
/// nor `QCoreApplication::instance()`, so leaving the event loop properly
/// would need a C++ shim in `cpp/app.*`. The cost of exiting instead is that
/// destructors do not run, so a crash on the way out would go unseen.
async fn quit(qt: &QtHandle) -> Result<(), String> {
    tokio::time::sleep(QUIT_DELAY).await;
    qt.queue(|_| {
        tracing::info!(target: "smoke", "quit");
        std::process::exit(0)
    })
    .map_err(|_| "the Qt thread is gone".to_owned())
}

/// The workspace a step operates on, or an error naming what is missing.
fn need(workspace: Option<&WorkspaceId>, step: &str) -> Result<WorkspaceId, String> {
    workspace
        .cloned()
        .ok_or_else(|| format!("`{step}` needs a `create` before it"))
}
