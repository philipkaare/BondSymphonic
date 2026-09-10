//! Drives a connected IDE with nobody at the keyboard. **Test-only**: with
//! `BS_SMOKE_SCRIPT` unset, which is every ordinary run, [`script`] returns
//! `None`, no task is spawned, and nothing in this module executes.
//!
//! The variable holds a comma-separated step list, run in order once the daemon
//! connection is up. `tests/smoke.rs` runs the real IDE binary offscreen against
//! an in-process fake daemon with
//! `create_claude,open_agent,send,allow,tree,open_file,open_diff,stop,create,detect,run_start,allow_host,run_stop,merge,merge,pr,open,close,reconnect,destroy,quit`.
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
//! * `detect` — ask the daemon what the repository in `BS_SMOKE_REPO` can run,
//!   through `AppController::detectRunConfigs` — the invokable the New Agent
//!   dialog calls while the user is still typing the path. The Run panel makes
//!   its own `repo.detect_run_configs` for the *worktree* when the tab appears;
//!   this is the dialog's half of that pair.
//! * `run_start` — start the run configuration [`RUN_CONFIG`] in the workspace
//!   the last `create*` made. The daemon answers with a bridged host port and
//!   then reports `starting` → output → `ready`, and the fake follows those with
//!   a network denial for [`DENIED_HOST`], so the window's toast path runs on a
//!   workspace that is on screen.
//! * `allow_host` — answer that toast through
//!   `AppController::requestAllowHost`, the production path Milestone 6's
//!   desktop notifications will use: the window checks that the Run panel is
//!   showing that workspace and calls `RunPanelModel::allowHost`, which reads
//!   the daemon's own allowlist and sends it back with the host added.
//! * `run_stop` — stop the run the last `run_start` made.
//! * `close` — close the PTY the last `open` made. The fake daemon answers by
//!   ending every PTY it has handed out, including the ones the window opened
//!   for its own panes, so the steps after this one run against terminals whose
//!   process has exited.
//! * `merge` — merge the workspace the last `create*` made into its base
//!   branch, through `AppController::mergeWorkspace` — the invokable the
//!   Changes toolbar calls once its confirmation has been answered. The first
//!   `merge` of a run sends mode `merge` with no summary and the second
//!   `squash` with one, so a run covers both the mode crossing and the optional
//!   message. Nothing here reads the answer: it arrives as `mergeFinished` or
//!   `workspaceOperationFailed`, which the toolbar turns into a status message
//!   or a banner.
//! * `pr` — open a pull request for that workspace through
//!   `AppController::createPr`, the invokable behind the PR dialog's OK. The
//!   URL comes back as `prCreated` and reaches the status bar as a link.
//! * `destroy` — destroy the workspace and emit `workspace_destroyed`, which is
//!   what makes the window tear its panes down.
//! * `reconnect` — ask the daemon to drop the connection with
//!   [`TEST_DROP_METHOD`], then wait for the controller's reconnect loop to
//!   publish a new one and re-sync. **Only a fake daemon acts on this**: the
//!   method is not in `Request`, so to the real daemon's decoder it is an
//!   unknown enum variant — it answers `invalid_params` and stays connected,
//!   and the step then fails on its own timeout rather than dropping anything.
//!   Every step after it runs on the new connection, because this is the one
//!   step that hands the script a fresh client.
//! * `quit` — let the window settle, then end the process with status 0.
//!
//! A failing step logs and stops the script *without* quitting, so a broken run
//! is a process that never exits rather than a green exit code.

use crate::client::DaemonClient;
use crate::qobjects::app_controller::{connection_generation, shared, QtHandle};
use bondsymphonic_proto::*;
use cxx_qt_lib::QString;
use std::time::{Duration, Instant};

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
/// The run configuration `run_start` starts. The fake daemon reports exactly
/// this one from `repo.detect_run_configs`, so it is also what the Run panel
/// preselects.
const RUN_CONFIG: &str = "web";
/// The host the fake daemon's proxy refuses just after the run comes up. It is
/// not in the default allowlist, which is what makes `allow_host` a change.
const DENIED_HOST: &str = "example.com";
/// How long `run_start` waits for `run.state`, `run.output` and the denial
/// behind them to travel from the daemon through the router and onto the Qt
/// thread, where the toast is raised.
const RUN_SETTLE: Duration = Duration::from_millis(1_500);
/// How long `allow_host` waits. The answer is two requests deep — the window
/// asks the daemon for the workspace's current allowlist and only then sends it
/// back with the host added — so this has to cover both round trips.
const ALLOW_SETTLE: Duration = Duration::from_millis(1_500);
/// How long `detect` waits for its reply, so the request it makes is in the
/// journal before the next step's.
const DETECT_SETTLE: Duration = Duration::from_millis(750);
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
/// The control method `reconnect` sends. **Fake daemons only.** It is not a
/// `Request` variant, so it can only be built by hand and only a daemon written
/// to recognise it does anything with it; to the real daemon it is an
/// undecodable request, answered `invalid_params` with the connection left up.
pub const TEST_DROP_METHOD: &str = "system.test_drop";
/// How long `reconnect` waits for the connection generation to move. The first
/// backoff is one second, so this covers several attempts of a fake daemon that
/// is slow to accept again, and fails the step rather than the whole run if it
/// never does.
const RECONNECT_LIMIT: Duration = Duration::from_secs(45);
/// How often the generation is checked while waiting.
const RECONNECT_POLL: Duration = Duration::from_millis(100);
/// The modes the `merge` steps use, in that order: the first lands the branch,
/// the second squashes it. Two rather than one because the word is translated
/// into the daemon's `MergeMode` on the way, and a run that only ever sent
/// `merge` would not show that translation happening. A third `merge` step
/// repeats the last.
const MERGE_MODES: [&str; 2] = ["merge", "squash"];
/// The summary the squashing merge carries. The plain `merge` sends none, the
/// way the toolbar does it -- Merge and Rebase have no box to type in, and
/// Squash's may be left empty -- so the pair covers both halves of the
/// daemon's optional message.
const SQUASH_SUMMARY: &str = "smoke: squashed";
/// What `pr` puts in the three fields of the PR dialog. `draft` is true so the
/// flag travels as something other than its default.
const PR_TITLE: &str = "Smoke PR";
const PR_BODY: &str = "Opened by the smoke run.";
const PR_DRAFT: bool = true;
/// How long `merge` and `pr` wait. Each is one request, made on the Qt thread
/// and answered back onto it, and the toolbar makes two more of its own behind
/// a merge that landed; a step that returned at once would let the next one
/// race them into the journal.
const OPERATION_SETTLE: Duration = Duration::from_millis(750);
/// How long `reconnect` waits after the new connection is published, so the
/// re-sync it triggers -- `system.check_prereqs` and `workspace.list` -- and
/// every pane's own re-attach have landed before the next step acts.
const RECONNECT_SETTLE: Duration = Duration::from_millis(1_500);

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
///
/// `client` is the connection the script started on. The `reconnect` step
/// replaces it, so every step after one runs on the connection that is live
/// then rather than on a socket that has been closed.
pub(crate) async fn run(steps: Vec<String>, mut client: DaemonClient, qt: QtHandle) {
    let repo = std::env::var(REPO_ENV).unwrap_or_else(|_| DEFAULT_REPO.to_owned());
    let mut workspace: Option<WorkspaceId> = None;
    let mut pty: Option<PtyId> = None;
    let mut agent: Option<AgentId> = None;
    let mut run: Option<RunId> = None;
    let mut created = 0usize;
    // How many `merge` steps have run, which is what picks the mode.
    let mut merged = 0usize;
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
            "detect" => detect(&qt, &repo).await,
            "run_start" => run_start(&client, workspace.as_ref())
                .await
                .map(|id| run = Some(id)),
            "allow_host" => allow_host(&qt, workspace.as_ref()).await,
            "run_stop" => run_stop(&client, run.take()).await,
            "close" => close(&client, pty.take()).await,
            "merge" => {
                merged += 1;
                merge(&qt, workspace.as_ref(), merged).await
            }
            "pr" => pr(&qt, workspace.as_ref()).await,
            "destroy" => destroy(&client, &qt, workspace.take()).await,
            "reconnect" => reconnect(&client).await.map(|fresh| client = fresh),
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
            // The script's repository is a repository already; the script is
            // not the place to exercise initialising one.
            init_if_missing: false,
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

/// Merges the workspace the last `create*` made into its base branch.
///
/// Through the window, not on this module's own client: `workspace.merge` has
/// to leave `AppController::merge_workspace`, which is where the toolbar's word
/// becomes a `MergeMode` and where an empty summary becomes `None`. Sending the
/// request by hand would prove only that the daemon answers one, and would step
/// around both of those.
///
/// `nth` is which merge of the run this is, and picks the mode out of
/// [`MERGE_MODES`]. Nothing here reads the answer -- it arrives as
/// `mergeFinished` or `workspaceOperationFailed`, both of which the toolbar
/// handles -- so a conflict is not a failed step; a failed *request* is, and
/// shows up as the `workspace.merge failed` warning the suite asserts against.
async fn merge(qt: &QtHandle, workspace: Option<&WorkspaceId>, nth: usize) -> Result<(), String> {
    let workspace_id = need(workspace, "merge")?.to_string();
    let mode = MERGE_MODES[nth.saturating_sub(1).min(MERGE_MODES.len() - 1)];
    let summary = if mode == "squash" { SQUASH_SUMMARY } else { "" };
    qt.queue(move |q| {
        q.merge_workspace(
            QString::from(&workspace_id),
            QString::from(mode),
            QString::from(summary),
        )
    })
    .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(OPERATION_SETTLE).await;
    tracing::info!(target: "smoke", "merge requested with mode {mode}");
    Ok(())
}

/// Opens a pull request for that workspace, the way the PR dialog's OK does.
///
/// Through the window for the same reason [`merge`] is: `workspace.create_pr`
/// has to leave `AppController::create_pr` carrying the dialog's three fields.
async fn pr(qt: &QtHandle, workspace: Option<&WorkspaceId>) -> Result<(), String> {
    let workspace_id = need(workspace, "pr")?.to_string();
    qt.queue(move |q| {
        q.create_pr(
            QString::from(&workspace_id),
            QString::from(PR_TITLE),
            QString::from(PR_BODY),
            PR_DRAFT,
        )
    })
    .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(OPERATION_SETTLE).await;
    tracing::info!(target: "smoke", "pull request requested");
    Ok(())
}

/// Makes the daemon drop the connection, then waits for the IDE to build a new
/// one and hands the script the client for it.
///
/// Nothing here reconnects anything: the request goes out, the socket closes,
/// and everything after that is `AppController`'s reconnect loop acting on its
/// own. What this step waits for is the connection generation moving, which
/// only [`crate::qobjects::app_controller::publish_shared`] does, so a run that
/// gets past this step is a run in which the IDE really did relaunch and
/// re-handshake without being told to.
///
/// The drop request is expected to fail as often as it succeeds -- the daemon
/// answers it by closing the socket, and the writer may notice that before the
/// line is even flushed -- so a send error is logged rather than failed on.
async fn reconnect(client: &DaemonClient) -> Result<DaemonClient, String> {
    let before = connection_generation();
    if let Err(e) = client.send_untyped(TEST_DROP_METHOD).await {
        tracing::info!(
            target: "smoke",
            "{TEST_DROP_METHOD} could not be sent ({e}); the connection was already going"
        );
    }
    let deadline = Instant::now() + RECONNECT_LIMIT;
    while connection_generation() <= before {
        if Instant::now() >= deadline {
            return Err(format!(
                "the IDE did not reconnect within {RECONNECT_LIMIT:?}"
            ));
        }
        tokio::time::sleep(RECONNECT_POLL).await;
    }
    // The connection is published before the re-sync runs, so the settle is
    // what makes the next step act on a window that has caught up.
    tokio::time::sleep(RECONNECT_SETTLE).await;
    let shared = shared().ok_or_else(|| "the reconnect published no connection".to_owned())?;
    tracing::info!(target: "smoke", "reconnected on generation {}", connection_generation());
    Ok(shared.client)
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

/// Asks the daemon what `repo` can run, through the controller invokable the
/// New Agent dialog calls.
///
/// Nothing here reads the answer: it arrives as
/// `AppController::runConfigsDetected`, which the dialog listens for. The step
/// exists so `repo.detect_run_configs` reaches the wire from the *dialog's*
/// path as well as from the Run panel's, and so a reply the IDE could not read
/// would be logged as `repo.detect_run_configs failed`.
async fn detect(qt: &QtHandle, repo: &str) -> Result<(), String> {
    let repo = repo.to_owned();
    qt.queue(move |q| q.detect_run_configs(QString::from(&repo)))
        .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(DETECT_SETTLE).await;
    Ok(())
}

/// Starts [`RUN_CONFIG`] in the workspace the last `create*` made.
///
/// On the script's own client, the way `create` and `open` make theirs: the Run
/// panel's Start button is a widget, and this suite never touches the desktop.
/// What the run is here for is everything that follows it — the `run.state` and
/// `run.output` events, and the network denial the fake daemon sends behind
/// them, which the window has to turn into a toast for the workspace on screen.
async fn run_start(
    client: &DaemonClient,
    workspace: Option<&WorkspaceId>,
) -> Result<RunId, String> {
    let workspace_id = need(workspace, "run_start")?;
    let res = client
        .request::<RunStartResult>(Request::RunStart(RunStartParams {
            workspace_id,
            config_name: RUN_CONFIG.to_owned(),
            // The port override is the Run panel's business; this step starts
            // the configuration as it is written.
            port: None,
        }))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(target: "smoke", "run started {:?} on {}", res.run_id, res.url);
    tokio::time::sleep(RUN_SETTLE).await;
    Ok(res.run_id)
}

/// Answers the denial toast the way a click on "Allow host" does.
///
/// Through the window, not on this module's own client: `workspace.get` and
/// `workspace.set_allowlist` have to leave `RunPanelModel::allowHost`, or the
/// run proves only that the fake daemon answers a request the IDE never made.
/// The window refuses the request unless the Run panel is showing exactly this
/// workspace, and logs `allow host not routed` when it does — so a
/// `workspace.set_allowlist` reaching the daemon at all is the proof the toast
/// was up on the right tab.
async fn allow_host(qt: &QtHandle, workspace: Option<&WorkspaceId>) -> Result<(), String> {
    let workspace_id = need(workspace, "allow_host")?.to_string();
    qt.queue(move |q| {
        q.request_allow_host(QString::from(&workspace_id), QString::from(DENIED_HOST))
    })
    .map_err(|_| "the Qt thread is gone".to_owned())?;
    tokio::time::sleep(ALLOW_SETTLE).await;
    tracing::info!(target: "smoke", "allowed {DENIED_HOST}");
    Ok(())
}

/// Stops the run the last `run_start` made.
async fn run_stop(client: &DaemonClient, run: Option<RunId>) -> Result<(), String> {
    let run_id = run.ok_or_else(|| "`run_stop` needs a `run_start` before it".to_owned())?;
    client
        .request_raw(Request::RunStop(RunIdParams { run_id }))
        .await
        .map_err(|e| e.to_string())?;
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
