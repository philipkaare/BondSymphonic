//! Drives a connected IDE with nobody at the keyboard. **Test-only**: with
//! `BS_SMOKE_SCRIPT` unset, which is every ordinary run, [`script`] returns
//! `None`, no task is spawned, and nothing in this module executes.
//!
//! The variable holds a comma-separated step list, run in order once the daemon
//! connection is up. `tests/smoke.rs` runs the real IDE binary offscreen against
//! an in-process fake daemon with `create,open,tree,close,destroy,quit`. The
//! steps are:
//!
//! * `create` — create a workspace over the repository in `BS_SMOKE_REPO` and
//!   emit `workspace_created`, so the window builds its tab, its terminal pane
//!   and its file tree from the same signal a user's New Agent would produce.
//! * `open` — open a PTY in the workspace the last `create` made.
//! * `tree` — list that workspace's root directory.
//! * `close` — close the PTY the last `open` made. The fake daemon answers by
//!   ending every PTY it has handed out, including the ones the window opened
//!   for its own panes, so the steps after this one run against terminals whose
//!   process has exited.
//! * `destroy` — destroy the workspace and emit `workspace_destroyed`, which is
//!   what makes the window tear its panes down.
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
/// How long `close` waits for the `pty.exit` events it triggers to reach the
/// window's terminals, so the steps after it act on exited sessions.
const EXIT_SETTLE: Duration = Duration::from_millis(750);
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
    let mut pty: Option<PtyId> = None;
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
            "open" => open(&client, workspace.as_ref())
                .await
                .map(|id| pty = Some(id)),
            "tree" => tree(&client, workspace.as_ref()).await,
            "close" => close(&client, pty.take()).await,
            "destroy" => destroy(&client, &qt, workspace.take()).await,
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

async fn open(client: &DaemonClient, workspace: Option<&WorkspaceId>) -> Result<PtyId, String> {
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
    Ok(res.pty_id)
}

/// Closes the script's own PTY. The fake daemon takes that as the cue to end
/// every PTY it has open, which is the only way a script that never sees the
/// window's pane ids can make their processes exit.
async fn close(client: &DaemonClient, pty: Option<PtyId>) -> Result<(), String> {
    let pty_id = pty.ok_or_else(|| "`close` needs an `open` before it".to_owned())?;
    client
        .request_raw(Request::PtyClose(PtyIdParams { pty_id }))
        .await
        .map_err(|e| e.to_string())?;
    // The exits travel as events, so give them time to reach the terminals
    // before the next step tears their panes down.
    tokio::time::sleep(EXIT_SETTLE).await;
    Ok(())
}

/// Destroys the workspace and announces it with the signal the
/// `destroy_workspace` invokable emits, which is what makes the window drop the
/// tab and close its terminals.
async fn destroy(
    client: &DaemonClient,
    qt: &QtHandle,
    workspace: Option<WorkspaceId>,
) -> Result<(), String> {
    let workspace_id = need(workspace.as_ref(), "destroy")?;
    client
        .request_raw(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: workspace_id.clone(),
            force: true,
        }))
        .await
        .map_err(|e| e.to_string())?;
    let id = workspace_id.to_string();
    qt.queue(move |q| q.workspace_destroyed(QString::from(&id)))
        .map_err(|_| "the Qt thread is gone".to_owned())?;
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
