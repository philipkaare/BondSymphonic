//! Drives a connected IDE with nobody at the keyboard. **Test-only**: with
//! `BS_SMOKE_SCRIPT` unset, which is every ordinary run, [`script`] returns
//! `None`, no task is spawned, and nothing in this module executes.
//!
//! The variable holds a comma-separated step list, run in order once the daemon
//! connection is up. `tests/smoke.rs` runs the real IDE binary offscreen against
//! an in-process fake daemon with
//! `create_claude,open_agent,send,allow,tree,open_file,open_diff,stop,create,open,close,destroy,quit`.
//! The steps are:
//!
//! * `create` — create a workspace over the repository in `BS_SMOKE_REPO` and
//!   emit `workspace_created`, so the window builds its tab, its terminal pane
//!   and its file tree from the same signal a user's New Agent would produce.
//! * `create_claude` — the same, with adapter `claude`, so the window builds a
//!   transcript pane instead of a terminal.
//! * `open_agent` — start a Claude agent in the workspace the last `create*`
//!   made and emit `agent_started`. The window reacts by recording the id on
//!   the tab and attaching the pane's `TranscriptModel`, which is what issues
//!   `agent.history` and subscribes to the agent's events.
//! * `send` — send a prompt to that agent, then wait for the permission request
//!   the fake daemon answers with to reach the transcript.
//! * `allow` — allow the pending tool call through
//!   `AppController::requestPermissionReply`, the same production path
//!   Milestone 6's notifications will use: the window finds the transcript that
//!   is showing the request and calls `TranscriptModel::reply` on it.
//! * `stop` — stop the agent. A destroy would reap it daemon-side, so this is
//!   the only step that puts `agent.stop` on the wire.
//! * `open` — open a PTY in the workspace the last `create*` made.
//! * `tree` — list that workspace's root directory.
//! * `open_file` — ask the window to open [`OPEN_PATH`] in an editor tab, by
//!   emitting the same `open_file_requested` signal the Explorer's double-click
//!   produces. The requests that follow (`fs.read_file`, `fs.watch`) come from
//!   the `EditorDocument` the window builds, not from this module.
//! * `open_diff` — the same for `open_diff_requested`, which builds a
//!   `DiffWidget` and makes it call `workspace.diff`.
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
/// The file `open_file` and `open_diff` ask for. It is one of the two entries
/// the fake daemon's `fs.list_dir` reports, so the Explorer lists it too.
const OPEN_PATH: &str = "README.md";
/// How long `open_file` and `open_diff` wait after emitting their signal. The
/// widget is built, and its document's request issued, on the Qt thread; a step
/// that returned at once would let the next one race it, and the order the test
/// asserts on would depend on scheduling.
const OPEN_SETTLE: Duration = Duration::from_millis(750);
const PTY_COLS: u16 = 80;
const PTY_ROWS: u16 = 24;
/// How long a `create*` step waits after announcing its workspace. The window
/// builds the tab, the pane and the file tree on the Qt thread when it sees the
/// signal, and a terminal pane opens its PTY only once Qt has laid it out and
/// shown it; a step that returned at once would race all of that.
const CREATE_SETTLE: Duration = Duration::from_millis(750);
/// The prompt `send` puts to the agent.
const PROMPT: &str = "hi";
/// The permission request `allow` answers. The script cannot read the
/// transcript, so the id is agreed with the fake daemon rather than discovered;
/// the window refuses to route an answer to a pane that is not showing exactly
/// this request, so a wrong id fails the run instead of passing it quietly.
const PERMISSION_REQUEST_ID: &str = "req-1";
/// How long `send` waits for the permission request to travel from the daemon
/// through the router and the transcript onto the Qt thread. Everything after
/// the reply reaches the window through the same queue, so `allow` gets the
/// same grace before the next step reads the result.
const AGENT_SETTLE: Duration = Duration::from_millis(1_500);

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
    let mut agent: Option<AgentId> = None;
    let mut created = 0usize;
    for step in steps {
        tracing::info!(target: "smoke", "step: {step}");
        let outcome = match step.as_str() {
            "create" => {
                created += 1;
                create(&client, &qt, &repo, created, TERMINAL_ADAPTER)
                    .await
                    .map(|id| workspace = Some(id))
            }
            "create_claude" => {
                created += 1;
                create(&client, &qt, &repo, created, CLAUDE_ADAPTER)
                    .await
                    .map(|id| workspace = Some(id))
            }
            "open_agent" => open_agent(&client, &qt, workspace.as_ref())
                .await
                .map(|id| agent = Some(id)),
            "send" => send(&qt, agent.as_ref()).await,
            "allow" => allow(&qt, agent.as_ref()).await,
            "stop" => stop(&qt, agent.take()).await,
            "open" => open(&client, workspace.as_ref())
                .await
                .map(|id| pty = Some(id)),
            "tree" => tree(&client, workspace.as_ref()).await,
            "open_file" => open_editor(&qt, workspace.as_ref(), Pane::File).await,
            "open_diff" => open_editor(&qt, workspace.as_ref(), Pane::Diff).await,
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

/// The two adapters a `create*` step can file its tab under. The window builds
/// a terminal pane for one and a transcript pane for the other.
const TERMINAL_ADAPTER: &str = "terminal";
const CLAUDE_ADAPTER: &str = "claude";

/// Creates a workspace and announces it with the signal the
/// `create_workspace` invokable emits, which is what the window listens for.
async fn create(
    client: &DaemonClient,
    qt: &QtHandle,
    repo: &str,
    nth: usize,
    adapter: &'static str,
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
            QString::from(adapter),
            QString::from(""),
            QString::from(""),
        )
    });
    tokio::time::sleep(CREATE_SETTLE).await;
    Ok(info.id)
}

/// Starts a Claude agent and announces it with the signal
/// `AppController::start_agent` emits.
///
/// The request goes out on the script's own client, the way `create` makes its
/// workspace, so the script learns the agent id; everything the *window* does
/// with it — recording it on the tab, building the transcript pane, replaying
/// `agent.history` and subscribing to the live stream — happens because it
/// reacted to `agent_started`, not because this module asked for it.
async fn open_agent(
    client: &DaemonClient,
    qt: &QtHandle,
    workspace: Option<&WorkspaceId>,
) -> Result<AgentId, String> {
    let workspace_id = need(workspace, "open_agent")?;
    let res = client
        .request::<AgentStartResult>(Request::AgentStart(AgentStartParams {
            workspace_id: workspace_id.clone(),
            adapter: AgentAdapterKind::Claude,
            options: AgentStartOptions {
                command: None,
                resume_session: None,
                model: None,
                permission_mode: None,
                api_key: None,
            },
        }))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(target: "smoke", "agent started {:?}", res.agent_id);
    let (ws, id) = (workspace_id.to_string(), res.agent_id.to_string());
    qt.queue(move |q| q.agent_started(QString::from(&ws), QString::from(&id)))
        .map_err(|_| "the Qt thread is gone".to_owned())?;
    // The pane attaches on the Qt thread, and `agent.history` goes out from
    // there; a `send` that raced it would answer into a transcript that is
    // still replaying.
    tokio::time::sleep(AGENT_SETTLE).await;
    Ok(res.agent_id)
}

/// Sends a prompt through the window, then waits for the permission request
/// the fake daemon answers with to reach the transcript.
///
/// Through the window, not on this module's own client: `agent.send` has to
/// leave `TranscriptModel::send`, or the run proves only that the fake daemon
/// answers a request the IDE never made and the suite's "no `agent.send`
/// failed" guard can never fire.
async fn send(qt: &QtHandle, agent: Option<&AgentId>) -> Result<(), String> {
    let agent_id = need_agent(agent, "send")?.to_string();
    qt.queue(move |q| q.request_agent_send(QString::from(&agent_id), QString::from(PROMPT)))
        .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(AGENT_SETTLE).await;
    Ok(())
}

/// Allows the pending tool call the way a desktop notification will: through
/// the window, which is the only thing that knows which pane is showing it.
///
/// Nothing here talks to the daemon. `agent.permission_reply` appears on the
/// wire only if the window found a transcript attached to `agent_id` with
/// [`PERMISSION_REQUEST_ID`] on its permission bar and called
/// `TranscriptModel::reply` on it.
async fn allow(qt: &QtHandle, agent: Option<&AgentId>) -> Result<(), String> {
    let agent_id = need_agent(agent, "allow")?.to_string();
    qt.queue(move |q| {
        q.request_permission_reply(
            QString::from(&agent_id),
            QString::from(PERMISSION_REQUEST_ID),
            true,
        )
    })
    .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(AGENT_SETTLE).await;
    tracing::info!(target: "smoke", "allowed {PERMISSION_REQUEST_ID}");
    Ok(())
}

/// Stops the agent through the window. `destroy` would reap it daemon-side
/// without the IDE saying anything, so this is the step that exercises
/// `agent.stop` -- and it goes through `TranscriptModel::stop` for the same
/// reason [`send`] does.
async fn stop(qt: &QtHandle, agent: Option<AgentId>) -> Result<(), String> {
    let agent_id = need_agent(agent.as_ref(), "stop")?.to_string();
    qt.queue(move |q| q.request_agent_stop(QString::from(&agent_id)))
        .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(EXIT_SETTLE).await;
    Ok(())
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

/// Which of the two open requests [`open_editor`] makes.
#[derive(Clone, Copy)]
enum Pane {
    File,
    Diff,
}

impl Pane {
    fn step(self) -> &'static str {
        match self {
            Pane::File => "open_file",
            Pane::Diff => "open_diff",
        }
    }
}

/// Asks the window to open [`OPEN_PATH`] as an editor tab or as a diff tab.
///
/// This goes through `AppController::request_open_file`/`request_open_diff`,
/// which is the production path: the Explorer's double-click calls the same
/// invokable, `MainWindow` is connected to the same signal, and everything
/// after the signal — building the widget, creating the `EditorDocument` or
/// `DiffDocument`, and the `fs.read_file`, `fs.watch` and `workspace.diff`
/// requests those make — happens because the window reacted, not because this
/// module asked the daemon for anything itself.
async fn open_editor(
    qt: &QtHandle,
    workspace: Option<&WorkspaceId>,
    pane: Pane,
) -> Result<(), String> {
    let workspace_id = need(workspace, pane.step())?.to_string();
    qt.queue(move |q| {
        let workspace_id = QString::from(&workspace_id);
        let path = QString::from(OPEN_PATH);
        match pane {
            Pane::File => q.request_open_file(workspace_id, path),
            Pane::Diff => q.request_open_diff(workspace_id, path),
        }
    })
    .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(OPEN_SETTLE).await;
    tracing::info!(target: "smoke", "{} requested for {OPEN_PATH}", pane.step());
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

/// The agent a step operates on, or an error naming what is missing.
fn need_agent(agent: Option<&AgentId>, step: &str) -> Result<AgentId, String> {
    agent
        .cloned()
        .ok_or_else(|| format!("`{step}` needs an `open_agent` before it"))
}
