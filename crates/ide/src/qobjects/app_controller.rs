// Two of the create invokables take seven arguments. A Qt invokable carries no
// structs, so every field the New Agent dialog collected crosses the boundary
// one by one; folding them into a JSON blob would only move the same list
// behind a string C++ has to build and this file has to parse. The allow is
// file-level because cxx-qt refuses any attribute but `doc` on the bridge
// module, and the generated declarations trip the lint too.
#![allow(clippy::too_many_arguments)]

use crate::client::router::EventRouter;
use crate::client::DaemonClient;
use crate::launcher::{self, LaunchSpec};
use crate::model::app_state::{agent_state_word, compose_status, ConnectionState};
use crate::qobjects::settings::Settings;
use crate::qobjects::smoke;
use bondsymphonic_proto::*;
use std::sync::OnceLock;
use std::time::Duration;

/// One process-wide multi-threaded runtime drives the daemon connection. It is
/// intentionally never dropped: the Qt event loop owns the main thread, and the
/// daemon process handle parked on the controller outlives every task here.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio")
    })
}

/// The connection, plus the router fanning its events out. Published process-wide
/// once `start` has connected so that QObjects created later (terminal panes, the
/// file tree) can issue requests and subscribe to events without holding a
/// pointer to the controller.
#[derive(Clone)]
pub struct Shared {
    pub client: DaemonClient,
    pub router: EventRouter,
}

static SHARED: OnceLock<std::sync::Mutex<Option<Shared>>> = OnceLock::new();

/// Set once the connection has ended, so an operation attempted afterwards can
/// say the connection was lost rather than that it never started.
static CONNECTION_WAS_LOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Bumped by every [`publish_shared`], so a QObject holding a subscription can
/// tell whether it was made on the connection that is live now.
static CONNECTION_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Which connection is live, counting from 1. Zero means none has been
/// published yet, so a recorded generation is always non-zero and can never be
/// mistaken for "never connected".
///
/// Every connect builds a fresh [`EventRouter`], which makes every subscription
/// taken on the previous one dead: its sender lives in a router nothing
/// dispatches into any more, so the receiver never closes and the task behind
/// it parks forever. Anything that keeps a subscription across calls records
/// this number with it and re-subscribes when it no longer matches.
pub fn connection_generation() -> u64 {
    CONNECTION_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

fn shared_slot() -> &'static std::sync::Mutex<Option<Shared>> {
    SHARED.get_or_init(|| std::sync::Mutex::new(None))
}

/// The live daemon connection and event router, or `None` before `start` has
/// connected and again once the connection has been lost. The returned clone
/// keeps working only until the slot is replaced, so callers fetch it per
/// operation rather than caching it.
pub fn shared() -> Option<Shared> {
    shared_slot()
        .lock()
        .expect("shared handle mutex poisoned")
        .clone()
}

/// Publishes a fresh connection for every QObject to use.
pub fn publish_shared(shared: Shared) {
    CONNECTION_WAS_LOST.store(false, std::sync::atomic::Ordering::SeqCst);
    CONNECTION_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *shared_slot().lock().expect("shared handle mutex poisoned") = Some(shared);
}

/// Drops the process-wide connection handle because the daemon connection has
/// ended. Nothing reconnects in this milestone, so leaving the dead client in
/// the slot would only mean every later operation issues a request on it and
/// fails with a protocol error instead of a sentence the user can act on.
pub fn on_connection_lost() {
    CONNECTION_WAS_LOST.store(true, std::sync::atomic::Ordering::SeqCst);
    *shared_slot().lock().expect("shared handle mutex poisoned") = None;
}

/// The live connection, or the message an operation should fail with. Every
/// invokable goes through this, so once the connection is gone each of them
/// fails immediately, with a reason, instead of talking to a dead client.
pub fn require_connection() -> Result<Shared, &'static str> {
    match shared() {
        Some(shared) => Ok(shared),
        None if CONNECTION_WAS_LOST.load(std::sync::atomic::Ordering::SeqCst) => {
            Err(CONNECTION_LOST)
        }
        None => Err(NOT_CONNECTED),
    }
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `status_message` is exposed as `getStatusMessage`/`statusMessageChanged`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qproperty(i32, connection_state)]
        #[qproperty(QString, status_message)]
        #[qproperty(QString, daemon_version)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn prereq_warning(self: Pin<&mut AppController>, message: QString);

        /// Every prerequisite the daemon reported, as a JSON array of
        /// `PrereqStatus`. Emitted after every check: the one at start-up and
        /// every `recheck_prereqs`. This is what the setup page draws its rows
        /// from, and what the window decides to show the page at all from.
        #[qsignal]
        fn prereqs_checked(self: Pin<&mut AppController>, json: QString);

        /// A setup terminal is open. `action` is echoed back from the call, so
        /// a page that started two of them can tell which answered; `pty_id`
        /// is an ordinary PTY id, taking `pty.write`, `pty.resize` and
        /// `pty.close` with no special casing.
        #[qsignal]
        fn setup_pty_opened(self: Pin<&mut AppController>, action: QString, pty_id: QString);

        /// The daemon's full workspace list, as a JSON array of `WorkspaceInfo`,
        /// emitted once per successful connect.
        #[qsignal]
        fn workspaces_listed(self: Pin<&mut AppController>, json: QString);

        /// One workspace changed state: a JSON `WorkspaceInfo`.
        #[qsignal]
        fn workspace_changed(self: Pin<&mut AppController>, info_json: QString);

        /// A `create_workspace` call succeeded. `group`, `adapter`, `command`,
        /// `options_json` and `run_config` are echoed back from the call so the
        /// UI can place the new tab without tracking the in-flight request
        /// itself. `options_json` is the agent's `AgentStartOptions` for a
        /// Claude workspace and empty otherwise; it never carries the API key,
        /// which the controller merges in when it builds the request.
        /// `run_config` is the run configuration the user picked in the New
        /// Agent dialog, or empty; the window stores it on the tab so the Run
        /// panel opens on it.
        #[qsignal]
        fn workspace_created(
            self: Pin<&mut AppController>,
            info_json: QString,
            group: QString,
            adapter: QString,
            command: QString,
            options_json: QString,
            run_config: QString,
        );

        /// A `destroy_workspace` call succeeded, for the workspace with this id.
        #[qsignal]
        fn workspace_destroyed(self: Pin<&mut AppController>, id: QString);

        /// `agent.start` succeeded: `workspace_id` is now running `agent_id`.
        /// Emitted before any initial prompt is sent, so the transcript is
        /// attached and subscribed before the first answer arrives.
        #[qsignal]
        fn agent_started(self: Pin<&mut AppController>, workspace_id: QString, agent_id: QString);

        /// An agent changed state. `state` is the daemon's snake_case word
        /// ("idle", "working", "waiting_permission", "error", "exited") and
        /// `detail` its explanation, or empty.
        #[qsignal]
        fn agent_state_changed(
            self: Pin<&mut AppController>,
            agent_id: QString,
            state: QString,
            detail: QString,
        );

        /// An `inspect_repo` call succeeded: a JSON `RepoInfo` for `path`.
        #[qsignal]
        fn repo_inspected(self: Pin<&mut AppController>, path: QString, info_json: QString);

        /// A `detect_run_configs` call succeeded: a JSON array of `RunConfig`
        /// for `path`. `path` is echoed back because the New Agent dialog can
        /// have asked about a repository the user has since moved off.
        #[qsignal]
        fn run_configs_detected(self: Pin<&mut AppController>, path: QString, json: QString);

        /// The workspace's proxy refused a connection: `host` is not on its
        /// allowlist. The window routes this to the Run panel, which queues the
        /// hosts and shows one toast at a time.
        #[qsignal]
        fn network_denied(self: Pin<&mut AppController>, workspace_id: QString, host: QString);

        /// The daemon dropped `count` events because a consumer fell behind.
        #[qsignal]
        fn output_dropped(self: Pin<&mut AppController>, count: i64);

        /// An asynchronous operation failed. `op` is the daemon method name.
        #[qsignal]
        fn operation_failed(self: Pin<&mut AppController>, op: QString, message: QString);

        /// Launch the daemon inside WSL and connect to it.
        #[qinvokable]
        fn start(self: Pin<&mut AppController>);

        /// Record the daemon version and recompose the status text. Callers use
        /// this rather than `set_daemon_version` so the status bar stays in sync.
        #[qinvokable]
        fn apply_daemon_version(self: Pin<&mut AppController>, version: QString);

        /// Create a workspace. Answers with `workspace_created` or
        /// `operation_failed`; `group`, `adapter` and `command` are not sent to
        /// the daemon, only echoed back to the caller.
        #[qinvokable]
        fn create_workspace(
            self: Pin<&mut AppController>,
            repo_path: QString,
            base_branch: QString,
            name: QString,
            group: QString,
            adapter: QString,
            command: QString,
        );

        /// Create a workspace, remembering the run configuration the user
        /// picked. Identical to `createWorkspace` otherwise; the name is not
        /// sent to the daemon, only echoed back in `workspaceCreated` so the
        /// window can store it on the tab.
        #[qinvokable]
        fn create_workspace_with_run(
            self: Pin<&mut AppController>,
            repo_path: QString,
            base_branch: QString,
            name: QString,
            group: QString,
            adapter: QString,
            command: QString,
            run_config: QString,
        );

        /// `createWorkspaceWithAgent` plus the run configuration the user
        /// picked, echoed back in `workspaceCreated`.
        #[qinvokable]
        fn create_workspace_with_agent_and_run(
            self: Pin<&mut AppController>,
            repo_path: QString,
            base_branch: QString,
            name: QString,
            group: QString,
            options_json: QString,
            initial_prompt: QString,
            run_config: QString,
        );

        /// Create a workspace and start a Claude agent in it. Answers with
        /// `workspace_created` (adapter "claude"), then `agent_started`, then
        /// sends `initial_prompt` if it is not empty; any step can answer with
        /// `operation_failed` instead. `options_json` is an `AgentStartOptions`
        /// object without the API key, which the controller merges in.
        #[qinvokable]
        fn create_workspace_with_agent(
            self: Pin<&mut AppController>,
            repo_path: QString,
            base_branch: QString,
            name: QString,
            group: QString,
            options_json: QString,
            initial_prompt: QString,
        );

        /// Start a Claude agent in an existing workspace. Answers with
        /// `agent_started` or `operation_failed("agent.start", ...)`.
        #[qinvokable]
        fn start_agent(self: Pin<&mut AppController>, workspace_id: QString, options_json: QString);

        /// Destroy a workspace. Answers with `workspace_destroyed` or
        /// `operation_failed`.
        #[qinvokable]
        fn destroy_workspace(self: Pin<&mut AppController>, id: QString, force: bool);

        /// Inspect a git repository. Answers with `repo_inspected` or
        /// `operation_failed`.
        #[qinvokable]
        fn inspect_repo(self: Pin<&mut AppController>, path: QString);

        /// Ask the daemon which run configurations a repository (or a
        /// workspace's worktree) offers. Answers with `run_configs_detected`
        /// or `operation_failed("repo.detect_run_configs", ...)`.
        #[qinvokable]
        fn detect_run_configs(self: Pin<&mut AppController>, path: QString);

        /// Replace a workspace's network allowlist. `hosts_json` is a JSON
        /// array of host patterns. Success is silent -- the daemon answers with
        /// a `workspace.state` event carrying the new list -- and a failure
        /// answers with `operation_failed("workspace.set_allowlist", ...)`.
        #[qinvokable]
        fn set_allowlist(self: Pin<&mut AppController>, workspace_id: QString, hosts_json: QString);

        /// Translates a Windows path to the WSL path the daemon expects, or
        /// returns an empty string if it is not a translatable path.
        #[qinvokable]
        fn wsl_path(self: &AppController, windows_path: QString) -> QString;

        /// Asks the daemon for the prerequisites again and answers with
        /// `prereqs_checked`. Called after a setup terminal exits, and by the
        /// setup page's "Re-check" button.
        #[qinvokable]
        fn recheck_prereqs(self: Pin<&mut AppController>);

        /// Opens a host terminal running one of the four fixed setup commands.
        /// `action` is `claude_login`, `gh_login`, `install_claude` or
        /// `install_gh`; anything else is refused without reaching the daemon.
        /// Answers with `setup_pty_opened` or `operation_failed("setup", ...)`.
        #[qinvokable]
        fn open_setup_pty(self: Pin<&mut AppController>, action: QString, cols: i32, rows: i32);

        /// Whether any prerequisite in `json` is one the IDE cannot work
        /// without. The window asks before deciding between the setup page and
        /// a status-bar warning.
        #[qinvokable]
        fn prereqs_block(self: &AppController, json: QString) -> bool;

        /// What the daemon said it could do, as a JSON `Capabilities`, or an
        /// empty string before `hello` has answered. The New Agent dialog reads
        /// the adapter list out of it.
        #[qinvokable]
        fn capabilities_json(self: &AppController) -> QString;

        /// Stores the Anthropic API key in the Windows credential store,
        /// replacing any previous one. Returns whether it was stored.
        ///
        /// The key goes no further than the credential store and, later, the
        /// `agent.start` request: it is never held on this object, never
        /// logged, and never sent back out through a signal or a property.
        #[qinvokable]
        fn set_api_key(self: &AppController, key: QString) -> bool;

        /// Removes the stored API key. Removing one that is not there
        /// succeeds: the caller asked for there to be none, and there is none.
        #[qinvokable]
        fn clear_api_key(self: &AppController) -> bool;

        /// Whether a key is in the credential store. Asks the store, not the
        /// settings file, so a key deleted in Credential Manager is not still
        /// advertised by the dialog.
        #[qinvokable]
        fn api_key_set(self: &AppController) -> bool;

        /// The permission mode a new Claude agent should start on, from
        /// `settings.json`. The settings dialog writes it; the New Agent dialog
        /// opens on it.
        #[qinvokable]
        fn default_permission_mode(self: &AppController) -> QString;

        /// Records the permission mode new agents start on.
        #[qinvokable]
        fn set_default_permission_mode(self: &AppController, mode: QString);

        /// Someone in Rust asked the window to open a file in an editor tab.
        #[qsignal]
        fn open_file_requested(self: Pin<&mut AppController>, workspace_id: QString, path: QString);

        /// Someone in Rust asked the window to open a file's diff.
        #[qsignal]
        fn open_diff_requested(self: Pin<&mut AppController>, workspace_id: QString, path: QString);

        /// Someone in Rust asked the window to answer the tool-permission
        /// request `request_id` on `agent_id`'s transcript.
        #[qsignal]
        fn permission_reply_requested(
            self: Pin<&mut AppController>,
            agent_id: QString,
            request_id: QString,
            allow: bool,
        );

        /// Someone in Rust asked the window to send `text` as a prompt to
        /// `agent_id`'s transcript.
        #[qsignal]
        fn agent_send_requested(self: Pin<&mut AppController>, agent_id: QString, text: QString);

        /// Someone in Rust asked the window to stop `agent_id`.
        #[qsignal]
        fn agent_stop_requested(self: Pin<&mut AppController>, agent_id: QString);

        /// Someone in Rust asked the window to save every dirty editor.
        #[qsignal]
        fn save_all_requested(self: Pin<&mut AppController>);

        /// Someone in Rust asked the window to add `host` to `workspace_id`'s
        /// allowlist, as if the toast's "Allow host" had been pressed.
        #[qsignal]
        fn allow_host_requested(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            host: QString,
        );

        /// Asks the window to open `path` of `workspace_id` in an editor tab.
        ///
        /// These seven `request_*` invokables are the one path by which
        /// anything on the Rust side reaches the window: the controller has no
        /// pointer to it, and the window is the only thing that knows which
        /// tabs exist.
        /// The smoke script drives them today; the transcript's tool cards will
        /// drive them next.
        #[qinvokable]
        fn request_open_file(self: Pin<&mut AppController>, workspace_id: QString, path: QString);

        /// Asks the window to open the diff for `path` of `workspace_id`.
        #[qinvokable]
        fn request_open_diff(self: Pin<&mut AppController>, workspace_id: QString, path: QString);

        /// Asks the window to answer `request_id` on `agent_id`'s transcript
        /// with allow or deny, as if the user had pressed the button on the
        /// permission bar.
        ///
        /// The controller owns no transcript: only the window knows which pane
        /// is attached to which agent, so this goes out as a signal like the
        /// three above. It is production API, not a test hook — Milestone 6's
        /// desktop notifications answer a permission the same way — and the
        /// smoke script is its first caller.
        #[qinvokable]
        fn request_permission_reply(
            self: Pin<&mut AppController>,
            agent_id: QString,
            request_id: QString,
            allow: bool,
        );

        /// Asks the window to send `text` to `agent_id`, as if the user had
        /// typed it into that pane's prompt box.
        ///
        /// Like `request_permission_reply` this goes through the window because
        /// only the window knows which pane is attached to which agent, and it
        /// travels the production path: `TranscriptModel::send`, which is what
        /// turns a failed `agent.send` into a warning and an error banner. A
        /// request built here instead would reach the daemon without any of
        /// that.
        #[qinvokable]
        fn request_agent_send(self: Pin<&mut AppController>, agent_id: QString, text: QString);

        /// Asks the window to stop `agent_id`, as if the user had pressed Stop
        /// on that pane. Routed for the same reason as `request_agent_send`.
        #[qinvokable]
        fn request_agent_stop(self: Pin<&mut AppController>, agent_id: QString);

        /// Asks the window to save every dirty editor.
        #[qinvokable]
        fn request_save_all(self: Pin<&mut AppController>);

        /// Asks the window to allow `host` for `workspace_id`, as if the user
        /// had pressed "Allow host" on the network-denial toast.
        ///
        /// Routed through the window for the same reason the five above are:
        /// the controller owns no `RunPanelModel`, and only the window knows
        /// which workspace the panel is showing. The window refuses a request
        /// for any other workspace, so an allow can never be applied to a
        /// workspace the user is not looking at. Production API — Milestone 6's
        /// notifications answer a denial the same way — with the smoke script
        /// as its first caller.
        #[qinvokable]
        fn request_allow_host(self: Pin<&mut AppController>, workspace_id: QString, host: QString);
    }

    impl cxx_qt::Threading for AppController {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

pub(crate) type QtHandle = cxx_qt::CxxQtThread<qobject::AppController>;

/// Shared handle to the running daemon so a later "Exit" action can shut it down.
type ProcessHandle = std::sync::Arc<tokio::sync::Mutex<Option<launcher::DaemonProcess>>>;

/// Reported when an invokable runs before `start` has connected.
pub const NOT_CONNECTED: &str = "not connected to the daemon";

/// Reported once the daemon connection has ended. Distinct from
/// [`NOT_CONNECTED`] because the two need different answers from the user:
/// one is "wait", the other is "restart the IDE".
pub const CONNECTION_LOST: &str = "daemon connection lost";

/// The prerequisites the IDE cannot work around.
///
/// Without git there is no worktree, without bubblewrap and unprivileged user
/// namespaces there is no sandbox, and without a sandbox there is nowhere to
/// put an agent: a workbench on top of any of those would only be able to
/// report the same failure once per click. Everything else on the list -- the
/// two CLIs and their two logins -- costs the user Claude Code and leaves the
/// rest of the IDE working, so it is a warning rather than a wall.
pub const BLOCKING_PREREQS: [&str; 4] = ["git", "bwrap", "userns", "sandbox"];

/// Whether any of `items` is a failing [`BLOCKING_PREREQS`] entry.
///
/// A name this build has never heard of is not blocking. A daemon that grows a
/// new check should not be able to lock an older IDE out of its own workbench.
pub fn prereqs_blocking(items: &[PrereqStatus]) -> bool {
    items
        .iter()
        .any(|item| !item.ok && BLOCKING_PREREQS.contains(&item.name.as_str()))
}

/// The workspace and host of a network denial, for an event that is one.
///
/// The proxy announces a refused connection as a warn-level `daemon.log`
/// carrying the host, built by `Event::network_denied`; this recognises it
/// through the matching proto helper, so neither end parses the message text.
///
/// An event with no workspace is the daemon talking about itself: there is no
/// allowlist to offer to extend, so there is nothing for a toast to do and this
/// answers `None`.
pub fn network_denial(ws: &Option<WorkspaceId>, ev: &Event) -> Option<(String, String)> {
    let host = ev.denied_host()?;
    let workspace = ws.as_ref()?;
    Some((workspace.0.clone(), host.to_owned()))
}

/// The four setup terminals, by the names the UI and the daemon both use.
/// Anything else is refused here rather than sent on: the enum is the whole
/// point of `system.setup_pty`, and a typo should fail loudly and locally.
fn parse_setup_action(action: &str) -> Option<SetupAction> {
    match action {
        "claude_login" => Some(SetupAction::ClaudeLogin),
        "gh_login" => Some(SetupAction::GhLogin),
        "install_claude" => Some(SetupAction::InstallClaude),
        "install_gh" => Some(SetupAction::InstallGh),
        _ => None,
    }
}

pub struct AppControllerRust {
    connection_state: i32,
    status_message: QString,
    daemon_version: QString,
    /// What `hello` said the daemon supports, as JSON. Empty until connected.
    capabilities: QString,
    client: Option<DaemonClient>,
    process: Option<ProcessHandle>,
}

impl Default for AppControllerRust {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected.as_i32(),
            status_message: QString::from(ConnectionState::Disconnected.label()),
            daemon_version: QString::from(""),
            capabilities: QString::from(""),
            client: None,
            process: None,
        }
    }
}

/// Queues an `operation_failed` for `op` back onto the Qt thread.
fn report_failure(qt: &QtHandle, op: &'static str, message: String) {
    tracing::warn!("{op} failed: {message}");
    let _ = qt.queue(move |q| q.operation_failed(QString::from(op), QString::from(&message)));
}

/// Drains the connection's event stream for the life of the connection. Runs on
/// its own task, started before any request is issued, so nothing the controller
/// asks the daemon for can be starved by events the daemon is already sending.
async fn drain_events(mut events: crate::client::EventStream, router: EventRouter, qt: QtHandle) {
    while let Some((ws, ev)) = events.recv().await {
        // Every consumer sees the event before the controller acts on it, so a
        // terminal's output is never delayed behind UI work.
        router.dispatch(ws.clone(), ev.clone());
        // The daemon discarded events for this connection: every terminal has
        // a hole in it and says so. Recognised through the proto helper the
        // daemon builds the notice with, so the wording lives in one place.
        if let Some(count) = ev.dropped_event_count() {
            let count = i64::try_from(count).unwrap_or(i64::MAX);
            let _ = qt.queue(move |q| q.output_dropped(count));
            continue;
        }
        // A workspace's proxy refused a connection. Recognised through the
        // proto helper the daemon builds the notice with, like the drop notice
        // above; the line is still logged by the `daemon.log` arm below.
        if let Some((workspace, host)) = network_denial(&ws, &ev) {
            let _ = qt
                .queue(move |q| q.network_denied(QString::from(&workspace), QString::from(&host)));
        }
        match ev {
            Event::WorkspaceStateChanged { info } => {
                let json = serde_json::to_string(&info).unwrap_or_default();
                let _ = qt.queue(move |q| q.workspace_changed(QString::from(&json)));
            }
            // Routed to the transcript model by id above; this arm is what
            // moves the tab's own indicator, which no transcript owns.
            Event::AgentStateChanged {
                agent_id,
                state,
                detail,
            } => {
                let id = agent_id.to_string();
                let word = agent_state_word(state);
                let detail = detail.unwrap_or_default();
                let _ = qt.queue(move |q| {
                    q.agent_state_changed(
                        QString::from(&id),
                        QString::from(word),
                        QString::from(&detail),
                    )
                });
            }
            Event::DaemonLog { level, message, .. } => tracing::info!(?level, "{message}"),
            _ => {}
        }
    }
    // The stream only ends when the connection does. Drop the shared handle
    // first: the state change below is what the status bar shows, but it is the
    // empty slot that makes the next operation fail at once instead of issuing
    // a request nobody will answer.
    on_connection_lost();
    let _ = qt.queue(|mut q| {
        q.as_mut().rust_mut().client = None;
        q.set_state(ConnectionState::Lost);
    });
}

/// The Anthropic API key to put in `agent.start`'s options.
///
/// Read from the OS credential store at the moment the request is built, never
/// cached. It lives here, and is merged in [`start_options`], because the key
/// must never cross into C++, never reach a signal, and never be logged: the
/// dialog builds the options without it and the controller is the only thing
/// that adds it.
pub fn api_key_for_start() -> Option<String> {
    crate::qobjects::settings::api_key()
}

/// Runs `system.check_prereqs` and reports the answer twice: the whole list as
/// `prereqs_checked`, which is what the setup page draws, and the failures as
/// one sentence in `prereq_warning`, which is what the status bar shows.
///
/// Both are queued from the same closure, `prereqs_checked` first, so the
/// window has already decided which of the two views it is in by the time the
/// warning text reaches it.
async fn check_prereqs(client: DaemonClient, qt: QtHandle) {
    let items = match client
        .request::<CheckPrereqsResult>(Request::SystemCheckPrereqs {})
        .await
    {
        Ok(res) => res.items,
        Err(e) => {
            report_failure(&qt, "system.check_prereqs", e.to_string());
            return;
        }
    };
    let failures: Vec<String> = items
        .iter()
        .filter(|i| !i.ok)
        .map(|i| {
            let fix = i
                .fix_hint
                .as_ref()
                .map(|f| format!(" (fix: {f})"))
                .unwrap_or_default();
            format!("{}: {}{}", i.name, i.detail, fix)
        })
        .collect();
    let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".into());
    tracing::info!(
        total = items.len(),
        failed = failures.len(),
        "prerequisites checked"
    );
    let _ = qt.queue(move |mut q| {
        q.as_mut().prereqs_checked(QString::from(&json));
        if !failures.is_empty() {
            q.prereq_warning(QString::from(&failures.join("\n")));
        }
    });
}

/// Parses the options a dialog built and merges the stored API key in. An
/// unparseable string is not fatal: the daemon's defaults are a working agent,
/// which is a better answer than refusing to start one.
fn start_options(options_json: &str) -> AgentStartOptions {
    // Written out rather than parsed from `{}`: `AgentStartOptions` derives no
    // `Default`, and a literal turns a future proto change into a compile
    // error here instead of a panic in the running IDE.
    let unset = || AgentStartOptions {
        command: None,
        resume_session: None,
        model: None,
        permission_mode: None,
        api_key: None,
    };
    let mut options = if options_json.trim().is_empty() {
        unset()
    } else {
        serde_json::from_str(options_json).unwrap_or_else(|e| {
            tracing::warn!("agent.start: unparseable options ({e}); using defaults");
            unset()
        })
    };
    // Unconditional, not a fallback: the credential store is the only source
    // of this field. C++ has no business setting it, and a value that arrived
    // in `options_json` must be dropped rather than passed through.
    options.api_key = api_key_for_start();
    options
}

/// Starts a Claude agent, announces it, and sends the initial prompt if there
/// is one. Announcing first is what lets the transcript attach and subscribe
/// before the agent's first message arrives; anything said in between is
/// replayed out of the router's early buffer.
async fn start_agent_and_prompt(
    shared: Shared,
    qt: QtHandle,
    workspace: WorkspaceId,
    options: AgentStartOptions,
    initial_prompt: String,
) {
    let params = AgentStartParams {
        workspace_id: workspace.clone(),
        adapter: AgentAdapterKind::Claude,
        options,
    };
    let agent_id = match shared
        .client
        .request::<AgentStartResult>(Request::AgentStart(params))
        .await
    {
        Ok(res) => res.agent_id,
        Err(e) => {
            report_failure(&qt, "agent.start", e.to_string());
            return;
        }
    };
    let (ws, id) = (workspace.to_string(), agent_id.to_string());
    let _ = qt.queue(move |q| q.agent_started(QString::from(&ws), QString::from(&id)));

    if initial_prompt.is_empty() {
        return;
    }
    let params = AgentSendParams {
        agent_id,
        text: initial_prompt,
    };
    if let Err(e) = shared.client.request_raw(Request::AgentSend(params)).await {
        report_failure(&qt, "agent.send", e.to_string());
    }
}

/// How long to wait before the single `workspace.list` retry.
const LIST_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Fetches the daemon's workspace list and queues `workspaces_listed`. The
/// daemon is authoritative about which workspaces exist and the UI reconciles
/// its restored tabs against this list, so a single lost reply would leave the
/// session with no list at all: one retry, and `operation_failed` only if that
/// also fails.
async fn load_workspace_list(client: DaemonClient, qt: QtHandle) {
    let mut last_error = String::new();
    for attempt in 1..=2 {
        match client
            .request::<WorkspaceListResult>(Request::WorkspaceList {})
            .await
        {
            Ok(res) => {
                let json = serde_json::to_string(&res.workspaces).unwrap_or_else(|_| "[]".into());
                tracing::info!(count = res.workspaces.len(), attempt, "workspaces_listed");
                let _ = qt.queue(move |q| q.workspaces_listed(QString::from(&json)));
                return;
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == 1 {
                    tracing::warn!("workspace.list failed ({last_error}); retrying");
                    tokio::time::sleep(LIST_RETRY_DELAY).await;
                }
            }
        }
    }
    report_failure(&qt, "workspace.list", last_error);
}

impl qobject::AppController {
    pub fn start(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        let settings = Settings::load();
        let spec = LaunchSpec {
            distro: settings.distro.clone(),
            daemon_path_in_wsl: settings.daemon_path.clone(),
            local_daemon_binary: Settings::local_daemon_binary(),
            log_level: settings.log_level.clone(),
        };
        let _ = qt.queue(|q| q.set_state(ConnectionState::Launching));
        runtime().spawn(async move {
            // `test_endpoint` is the `BS_DAEMON_ADDR` test hook and is `None` in
            // every ordinary run, which then launches the daemon inside WSL.
            let (addr, token, proc) = match launcher::test_endpoint() {
                Some((addr, token)) => {
                    tracing::warn!(
                        "{} is set: connecting to {addr} instead of launching a daemon",
                        launcher::TEST_ADDR_ENV
                    );
                    (addr, token, None)
                }
                None => match launcher::launch(&spec).await {
                    Ok(p) => (
                        std::net::SocketAddr::from(([127, 0, 0, 1], p.port)),
                        p.token.clone(),
                        Some(p),
                    ),
                    Err(e) => {
                        let msg = format!("daemon: launch failed: {e:#}");
                        tracing::error!("{msg}");
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().set_state(ConnectionState::Error);
                            q.set_status_message(QString::from(msg.as_str()));
                        });
                        return;
                    }
                },
            };
            let _ = qt.queue(|q| q.set_state(ConnectionState::Connecting));

            let (client, hello, events) =
                match DaemonClient::connect(addr, &token, env!("CARGO_PKG_VERSION")).await {
                    Ok(x) => x,
                    Err(e) => {
                        let msg = format!("daemon: connect failed: {e}");
                        tracing::error!("{msg}");
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().set_state(ConnectionState::Error);
                            q.set_status_message(QString::from(msg.as_str()));
                        });
                        return;
                    }
                };

            // Published before any event is dispatched, so a QObject woken by the
            // first `workspaces_listed` can already reach the daemon.
            let router = EventRouter::new();
            publish_shared(Shared {
                client: client.clone(),
                router: router.clone(),
            });

            // Draining starts before any request goes out. The client's event
            // channel is bounded, and its socket reader pushes into it with
            // backpressure, so a daemon that is already streaming (a reconnect to
            // one with live PTYs) would otherwise fill the channel, stall the
            // reader, and starve every reply below until the request timeout.
            runtime().spawn(drain_events(events, router.clone(), qt.clone()));

            let version = hello.daemon_version.clone();
            let capabilities = serde_json::to_string(&hello.capabilities).unwrap_or_default();
            // `None` under the test hook: there is no daemon process to own.
            let handle: Option<ProcessHandle> =
                proc.map(|p| std::sync::Arc::new(tokio::sync::Mutex::new(Some(p))));
            let c2 = client.clone();
            let _ = qt.queue(move |mut q| {
                q.as_mut().rust_mut().client = Some(c2);
                q.as_mut().rust_mut().process = handle;
                q.as_mut().rust_mut().capabilities = QString::from(&capabilities);
                // State first, then the version, so `apply_daemon_version`
                // recomposes the text as "daemon: connected v<version>".
                q.as_mut().set_state(ConnectionState::Connected);
                q.apply_daemon_version(QString::from(version.as_str()));
            });

            // Its own task, so the list does not queue behind the prereq check.
            let list_client = client.clone();
            let list_qt = qt.clone();
            runtime().spawn(async move {
                load_workspace_list(list_client.clone(), list_qt.clone()).await;
                // The `BS_SMOKE_SCRIPT` test hook; `None` in every ordinary run.
                // It starts only once the list has been fetched and queued:
                // `reconcile` keeps only the workspaces the daemon listed, so a
                // list applied after the script's `workspace_created` would drop
                // the tab it just made, silently costing the run its coverage of
                // the window's own PTY and directory requests. Queuing is enough
                // to order them, because the Qt thread runs queued closures in
                // the order they were posted.
                if let Some(steps) = smoke::script() {
                    smoke::run(steps, list_client, list_qt).await;
                }
            });

            check_prereqs(client, qt).await;
        });
    }

    pub fn create_workspace(
        self: Pin<&mut Self>,
        repo_path: QString,
        base_branch: QString,
        name: QString,
        group: QString,
        adapter: QString,
        command: QString,
    ) {
        self.create_workspace_with_run(
            repo_path,
            base_branch,
            name,
            group,
            adapter,
            command,
            QString::from(""),
        );
    }

    pub fn create_workspace_with_run(
        self: Pin<&mut Self>,
        repo_path: QString,
        base_branch: QString,
        name: QString,
        group: QString,
        adapter: QString,
        command: QString,
        run_config: QString,
    ) {
        let qt = self.qt_thread();
        let params = WorkspaceCreateParams {
            repo_path: repo_path.to_string(),
            base_branch: base_branch.to_string(),
            name: name.to_string(),
        };
        // Echoed straight back on success: the controller keeps no tab state.
        let (group, adapter, command, run_config) = (
            group.to_string(),
            adapter.to_string(),
            command.to_string(),
            run_config.to_string(),
        );
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "workspace.create", message.to_owned());
                return;
            }
        };
        runtime().spawn(async move {
            match shared
                .client
                .request::<WorkspaceInfo>(Request::WorkspaceCreate(params))
                .await
            {
                Ok(info) => {
                    let json = serde_json::to_string(&info).unwrap_or_default();
                    let _ = qt.queue(move |q| {
                        q.workspace_created(
                            QString::from(&json),
                            QString::from(&group),
                            QString::from(&adapter),
                            QString::from(&command),
                            QString::from(""),
                            QString::from(&run_config),
                        )
                    });
                }
                Err(e) => report_failure(&qt, "workspace.create", e.to_string()),
            }
        });
    }

    pub fn create_workspace_with_agent(
        self: Pin<&mut Self>,
        repo_path: QString,
        base_branch: QString,
        name: QString,
        group: QString,
        options_json: QString,
        initial_prompt: QString,
    ) {
        self.create_workspace_with_agent_and_run(
            repo_path,
            base_branch,
            name,
            group,
            options_json,
            initial_prompt,
            QString::from(""),
        );
    }

    pub fn create_workspace_with_agent_and_run(
        self: Pin<&mut Self>,
        repo_path: QString,
        base_branch: QString,
        name: QString,
        group: QString,
        options_json: QString,
        initial_prompt: QString,
        run_config: QString,
    ) {
        let qt = self.qt_thread();
        let params = WorkspaceCreateParams {
            repo_path: repo_path.to_string(),
            base_branch: base_branch.to_string(),
            name: name.to_string(),
        };
        let group = group.to_string();
        let options = start_options(&options_json.to_string());
        // Echoed back to the window verbatim, so the tab keeps what the user
        // asked for and can start the agent again with it. The API key is not
        // in it: `start_options` merges the key into the request it builds and
        // never into this string.
        let echoed_options = options_json.to_string();
        let run_config = run_config.to_string();
        let prompt = initial_prompt.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "workspace.create", message.to_owned());
                return;
            }
        };
        runtime().spawn(async move {
            let info = match shared
                .client
                .request::<WorkspaceInfo>(Request::WorkspaceCreate(params))
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    report_failure(&qt, "workspace.create", e.to_string());
                    return;
                }
            };
            let json = serde_json::to_string(&info).unwrap_or_default();
            // The tab appears as soon as the workspace exists, so a slow
            // `agent.start` happens in front of the user rather than behind a
            // dialog that has not closed yet.
            let _ = qt.queue(move |q| {
                q.workspace_created(
                    QString::from(&json),
                    QString::from(&group),
                    QString::from("claude"),
                    QString::from(""),
                    QString::from(&echoed_options),
                    QString::from(&run_config),
                )
            });
            start_agent_and_prompt(shared, qt, info.id, options, prompt).await;
        });
    }

    pub fn start_agent(self: Pin<&mut Self>, workspace_id: QString, options_json: QString) {
        let qt = self.qt_thread();
        let workspace = WorkspaceId(workspace_id.to_string());
        let options = start_options(&options_json.to_string());
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "agent.start", message.to_owned());
                return;
            }
        };
        runtime().spawn(start_agent_and_prompt(
            shared,
            qt,
            workspace,
            options,
            String::new(),
        ));
    }

    pub fn destroy_workspace(self: Pin<&mut Self>, id: QString, force: bool) {
        let qt = self.qt_thread();
        let id = id.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "workspace.destroy", message.to_owned());
                return;
            }
        };
        runtime().spawn(async move {
            let params = WorkspaceDestroyParams {
                workspace_id: WorkspaceId(id.clone()),
                force,
            };
            match shared
                .client
                .request_raw(Request::WorkspaceDestroy(params))
                .await
            {
                Ok(_) => {
                    let _ = qt.queue(move |q| q.workspace_destroyed(QString::from(&id)));
                }
                Err(e) => report_failure(&qt, "workspace.destroy", e.to_string()),
            }
        });
    }

    pub fn inspect_repo(self: Pin<&mut Self>, path: QString) {
        let qt = self.qt_thread();
        let path = path.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "repo.inspect", message.to_owned());
                return;
            }
        };
        runtime().spawn(async move {
            let params = RepoPathParams { path: path.clone() };
            match shared
                .client
                .request::<RepoInfo>(Request::RepoInspect(params))
                .await
            {
                Ok(info) => {
                    let json = serde_json::to_string(&info).unwrap_or_default();
                    let _ = qt.queue(move |q| {
                        q.repo_inspected(QString::from(&path), QString::from(&json))
                    });
                }
                Err(e) => report_failure(&qt, "repo.inspect", e.to_string()),
            }
        });
    }

    pub fn detect_run_configs(self: Pin<&mut Self>, path: QString) {
        let qt = self.qt_thread();
        let path = path.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "repo.detect_run_configs", message.to_owned());
                return;
            }
        };
        runtime().spawn(async move {
            let params = RepoPathParams { path: path.clone() };
            match shared
                .client
                .request::<DetectRunConfigsResult>(Request::RepoDetectRunConfigs(params))
                .await
            {
                Ok(res) => {
                    let json = serde_json::to_string(&res.configs).unwrap_or_else(|_| "[]".into());
                    let _ = qt.queue(move |q| {
                        q.run_configs_detected(QString::from(&path), QString::from(&json))
                    });
                }
                Err(e) => report_failure(&qt, "repo.detect_run_configs", e.to_string()),
            }
        });
    }

    pub fn set_allowlist(self: Pin<&mut Self>, workspace_id: QString, hosts_json: QString) {
        let qt = self.qt_thread();
        let hosts: Vec<String> = match serde_json::from_str(&hosts_json.to_string()) {
            Ok(hosts) => hosts,
            Err(e) => {
                // Refused here rather than sent on: an unparseable list would
                // otherwise reach the daemon as an empty one and lock the
                // workspace out of the network entirely.
                report_failure(
                    &qt,
                    "workspace.set_allowlist",
                    format!("host list is not a JSON array of strings: {e}"),
                );
                return;
            }
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "workspace.set_allowlist", message.to_owned());
                return;
            }
        };
        let params = WorkspaceSetAllowlistParams {
            workspace_id: WorkspaceId(workspace_id.to_string()),
            hosts,
        };
        runtime().spawn(async move {
            // Success is silent: the daemon answers with a `workspace.state`
            // event carrying the new list, which is what refreshes the UI.
            if let Err(e) = shared
                .client
                .request_raw(Request::WorkspaceSetAllowlist(params))
                .await
            {
                report_failure(&qt, "workspace.set_allowlist", e.to_string());
            }
        });
    }

    pub fn request_open_file(self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        self.open_file_requested(workspace_id, path);
    }

    pub fn request_open_diff(self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        self.open_diff_requested(workspace_id, path);
    }

    pub fn request_permission_reply(
        self: Pin<&mut Self>,
        agent_id: QString,
        request_id: QString,
        allow: bool,
    ) {
        self.permission_reply_requested(agent_id, request_id, allow);
    }

    pub fn request_agent_send(self: Pin<&mut Self>, agent_id: QString, text: QString) {
        self.agent_send_requested(agent_id, text);
    }

    pub fn request_agent_stop(self: Pin<&mut Self>, agent_id: QString) {
        self.agent_stop_requested(agent_id);
    }

    pub fn request_save_all(self: Pin<&mut Self>) {
        self.save_all_requested();
    }

    pub fn request_allow_host(self: Pin<&mut Self>, workspace_id: QString, host: QString) {
        self.allow_host_requested(workspace_id, host);
    }

    pub fn recheck_prereqs(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "system.check_prereqs", message.to_owned());
                return;
            }
        };
        runtime().spawn(check_prereqs(shared.client, qt));
    }

    pub fn open_setup_pty(self: Pin<&mut Self>, action: QString, cols: i32, rows: i32) {
        let qt = self.qt_thread();
        let name = action.to_string();
        let Some(action) = parse_setup_action(&name) else {
            report_failure(&qt, "setup", format!("unknown setup action: {name}"));
            return;
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_failure(&qt, "setup", message.to_owned());
                return;
            }
        };
        // The daemon clamps these itself; clamping here too keeps a widget that
        // has not been laid out yet from asking for a zero-column terminal.
        let params = SetupPtyParams {
            action,
            cols: cols.clamp(2, u16::MAX as i32) as u16,
            rows: rows.clamp(1, u16::MAX as i32) as u16,
        };
        runtime().spawn(async move {
            match shared
                .client
                .request::<PtyOpenResult>(Request::SystemSetupPty(params))
                .await
            {
                Ok(res) => {
                    let id = res.pty_id.to_string();
                    tracing::info!("setup terminal for {name} opened as {id}");
                    let _ = qt.queue(move |q| {
                        q.setup_pty_opened(QString::from(&name), QString::from(&id))
                    });
                }
                Err(e) => report_failure(&qt, "setup", e.to_string()),
            }
        });
    }

    pub fn prereqs_block(&self, json: QString) -> bool {
        match serde_json::from_str::<Vec<PrereqStatus>>(&json.to_string()) {
            Ok(items) => prereqs_blocking(&items),
            // Unreadable is not blocking: the list is the daemon's own output,
            // so failing to parse it is this build's problem, and hiding the
            // workbench over it would leave the user nothing at all.
            Err(e) => {
                tracing::warn!("prereqs_block: unparseable prerequisite list: {e}");
                false
            }
        }
    }

    pub fn capabilities_json(&self) -> QString {
        self.rust().capabilities.clone()
    }

    pub fn set_api_key(&self, key: QString) -> bool {
        // `key` is moved straight into the credential store and dropped. It is
        // deliberately not logged, not stored on this object, and not echoed
        // back through any signal.
        crate::qobjects::settings::set_api_key(&key.to_string())
    }

    pub fn clear_api_key(&self) -> bool {
        crate::qobjects::settings::clear_api_key()
    }

    pub fn api_key_set(&self) -> bool {
        crate::qobjects::settings::api_key_set()
    }

    pub fn default_permission_mode(&self) -> QString {
        QString::from(&Settings::load().default_permission_mode)
    }

    pub fn set_default_permission_mode(&self, mode: QString) {
        let mode = mode.to_string();
        let mut settings = Settings::load();
        if settings.default_permission_mode == mode {
            return;
        }
        settings.default_permission_mode = mode;
        if let Err(e) = settings.save() {
            tracing::warn!("settings.json could not be written: {e}");
        }
    }

    pub fn wsl_path(&self, windows_path: QString) -> QString {
        let path = windows_path.to_string();
        match launcher::windows_path_to_wsl(std::path::Path::new(&path)) {
            Some(p) => QString::from(&p),
            None => QString::from(""),
        }
    }

    pub fn set_state(mut self: Pin<&mut Self>, state: ConnectionState) {
        self.as_mut().set_connection_state(state.as_i32());
        self.refresh_status(state);
    }

    pub fn apply_daemon_version(mut self: Pin<&mut Self>, version: QString) {
        self.as_mut().set_daemon_version(version);
        let state = ConnectionState::from_i32(*self.connection_state());
        self.refresh_status(state);
    }

    /// Recomposes `status_message` from `state` and the current `daemon_version`.
    fn refresh_status(mut self: Pin<&mut Self>, state: ConnectionState) {
        let version = self.daemon_version().to_string();
        let text = compose_status(state.label(), &version);
        self.as_mut().set_status_message(QString::from(&text));
    }
}
