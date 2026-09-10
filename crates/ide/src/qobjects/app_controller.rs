// Two of the create invokables take seven arguments. A Qt invokable carries no
// structs, so every field the New Agent dialog collected crosses the boundary
// one by one; folding them into a JSON blob would only move the same list
// behind a string C++ has to build and this file has to parse. The allow is
// file-level because cxx-qt refuses any attribute but `doc` on the bridge
// module, and the generated declarations trip the lint too.
#![allow(clippy::too_many_arguments)]

use crate::client::router::EventRouter;
use crate::client::{ClientError, DaemonClient};
use crate::launcher::{self, LaunchSpec};
use crate::model::app_state::{
    agent_state_word, compose_status, ConnectionState, Workspaces, UNSORTED_GROUP,
};
use crate::model::persistence::{self, StateFile, StateStore};
use crate::qobjects::settings::Settings;
use crate::qobjects::smoke;
use bondsymphonic_proto::*;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

/// Bumped by every [`publish_shared`]. A watch channel rather than a counter,
/// so a QObject can both *read* which connection is live and *wait* for it to
/// change without polling; see [`connection_generation`] and [`on_reconnect`].
static GENERATION: OnceLock<tokio::sync::watch::Sender<u64>> = OnceLock::new();

fn generation_channel() -> &'static tokio::sync::watch::Sender<u64> {
    // The receiver end is dropped at once; every reader subscribes its own.
    // `send_modify` works with no receivers at all, so the count is kept
    // whether or not anything is currently listening.
    GENERATION.get_or_init(|| tokio::sync::watch::channel(0).0)
}

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
    *generation_channel().borrow()
}

/// A receiver that wakes whenever [`connection_generation`] moves.
///
/// This is the whole mechanism by which panes survive a daemon restart: the
/// reconnect loop publishes a new connection, the number changes, and every
/// subscriber re-attaches to the router that is live now. Prefer
/// [`on_reconnect`], which wraps the waiting; this is for a caller that needs
/// the receiver itself.
pub fn generation_watch() -> tokio::sync::watch::Receiver<u64> {
    generation_channel().subscribe()
}

/// Calls `apply` on `qt`'s Qt thread the next time the connection generation
/// moves away from the one that is live now, then ends.
///
/// One shot, deliberately: `apply` re-attaches the object, and re-attaching is
/// what arms the next watch. A loop here would need the object to abort its own
/// task from inside the closure it queued.
///
/// The generation is captured when this is *called*, not when the task first
/// runs, so a connection published in between still fires it. Without that, a
/// pane attached microseconds before a reconnect would wait for a change that
/// had already happened and stay dead for the session.
pub fn on_reconnect<T>(
    qt: cxx_qt::CxxQtThread<T>,
    apply: fn(core::pin::Pin<&mut T>),
) -> tokio::task::JoinHandle<()>
where
    T: cxx_qt::Threading + 'static,
{
    let armed_at = connection_generation();
    runtime().spawn(async move {
        let mut generations = generation_watch();
        loop {
            if *generations.borrow_and_update() != armed_at {
                break;
            }
            // The sender is a process-wide `OnceLock` that is never dropped, so
            // this only errors if that ever changes; ending the task is then
            // the right answer either way.
            if generations.changed().await.is_err() {
                return;
            }
        }
        let _ = qt.queue(apply);
    })
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
///
/// The slot is filled *before* the generation is bumped: a subscriber woken by
/// the change immediately reaches for [`shared`], and finding `None` there
/// would cost it its re-attach.
pub fn publish_shared(shared: Shared) {
    CONNECTION_WAS_LOST.store(false, std::sync::atomic::Ordering::SeqCst);
    *shared_slot().lock().expect("shared handle mutex poisoned") = Some(shared);
    generation_channel().send_modify(|generation| *generation += 1);
}

/// Drops the process-wide connection handle because the daemon connection has
/// ended. Leaving the dead client in the slot would only mean every operation
/// made before the reconnect lands issues a request on it and fails with a
/// protocol error instead of a sentence the user can act on.
pub fn on_connection_lost() {
    CONNECTION_WAS_LOST.store(true, std::sync::atomic::Ordering::SeqCst);
    *shared_slot().lock().expect("shared handle mutex poisoned") = None;
}

/// Set by `prepareQuit`, so a connection ending because the IDE is shutting
/// down is not mistaken for one that should be reconnected.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the reconnect loop is already running. There is one daemon
/// connection per IDE, so a second `start()` -- a stray call from C++, a
/// window rebuilt -- must not put a second loop behind it: two loops would
/// relaunch two daemons over one data directory and take turns replacing each
/// other's connection.
static SUPERVISING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the IDE is on its way out. The reconnect loop checks this at every
/// step it could act on, so a daemon shut down by an exiting IDE is never
/// relaunched behind it.
pub fn quitting() -> bool {
    QUITTING.load(std::sync::atomic::Ordering::SeqCst)
}

/// How long to wait before reconnect attempt `attempt`, counting from 1:
/// 1, 2, 4, 8, 16, then 30 seconds for every attempt after that.
///
/// Doubling gives a daemon that is merely slow to come back a fast reconnect,
/// and the cap keeps a daemon that is down for the afternoon from being poked
/// once an hour. Attempts are unbounded, so the exponent is clamped before it
/// is shifted rather than after: `1 << 64` is not a long wait, it is undefined.
pub fn backoff_delay(attempt: u32) -> Duration {
    const MAX_SECS: u64 = 30;
    // Attempt 0 is not a thing the loop produces, but a caller that passes one
    // must still get a wait rather than a spin.
    let steps = attempt.saturating_sub(1).min(5);
    Duration::from_secs((1u64 << steps).min(MAX_SECS))
}

/// How long a connection has to last before it counts as having worked.
///
/// A daemon that starts, answers `hello`, and dies a second later has not
/// recovered; it is crash-looping. Below this, a handshake is not evidence of
/// anything and the backoff keeps climbing.
pub const HELD_LONG_ENOUGH: Duration = Duration::from_secs(5);

/// The attempt number to use for the next reconnect, given the one that was
/// just made and how long the connection it produced lasted.
///
/// A connection that held resets the schedule, so an ordinary daemon restart
/// costs the user one second rather than the thirty a long-running IDE would
/// otherwise have climbed to. A connection that did not reset nothing: a daemon
/// crash-looping at start-up would otherwise be relaunched every couple of
/// seconds for as long as the IDE is open, with the status bar frozen on
/// "attempt 1" and no sign that anything is wrong.
pub fn next_attempt(prev: u32, held: Duration) -> u32 {
    if held >= HELD_LONG_ENOUGH {
        1
    } else {
        prev.saturating_add(1).max(1)
    }
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

// ---------------------------------------------------------------------------
// Persistence (`state.json`).
//
// The store is process-wide for the same reason [`SHARED`] is: the Run panel
// has to read a port override and the window has to report its layout, and
// neither holds a pointer to the controller. Everything the file knows lives in
// `model/persistence.rs`; what is here is only when to write.
// ---------------------------------------------------------------------------

static STATE: OnceLock<StateStore> = OnceLock::new();

/// The IDE's persisted state, read from disk on first use.
///
/// The path is decided once, at that first use, from `Settings::state_path()`,
/// so anything overriding `BS_STATE_PATH` (or `BS_SETTINGS_PATH`) has to do it
/// before the IDE starts -- which is what the launcher and the smoke test do.
pub fn state_store() -> &'static StateStore {
    STATE.get_or_init(|| StateStore::load(Settings::state_path()))
}

/// Applies a change and schedules the debounced write.
///
/// Every change hands out a token and starts a timer; a change made while a
/// timer is running issues a newer token, so the earlier timer finds itself
/// stale and does nothing. A burst -- a splitter being dragged, five tabs
/// closing -- is therefore one write, [`persistence::DEBOUNCE`] after it stops.
pub fn note_state(f: impl FnOnce(&mut StateFile)) {
    schedule_flush(state_store().update(f));
}

/// Starts the timer that writes `state.json` unless a later change supersedes
/// it. Separate from [`note_state`] so a caller that already updated the store
/// (as [`restore_workspaces`] does) can schedule its write without a second
/// mutation.
pub fn schedule_flush(token: u64) {
    runtime().spawn(async move {
        tokio::time::sleep(persistence::DEBOUNCE).await;
        state_store().flush_if_current(token);
    });
}

/// Rebuilds the tab model from the persisted arrangement and the daemon's
/// first workspace list, and records what it settled on.
///
/// Both halves matter. The model is what the sidebar is rebuilt from, so the
/// user's groups come back rather than one "Unsorted" pile. Writing the result
/// straight back is what keeps the file from growing forever: workspaces the
/// daemon has lost take their editors and port overrides with them, and the
/// ones it gained are recorded in the group they landed in.
///
/// Returns the model and the token whose timer may write the file.
pub fn restore_workspaces(store: &StateStore, list: &[WorkspaceInfo]) -> (Workspaces, u64) {
    let live: Vec<String> = list.iter().map(|info| info.id.0.clone()).collect();
    let (groups, active) = store.with(|s| (s.groups.clone(), s.active_workspace.clone()));
    let model = Workspaces::from_persisted(&groups, list, active.as_deref());
    let restored_groups = model.persisted_groups();
    let restored_active = model.active().map(|tab| tab.workspace_id.0.clone());
    let token = store.update(|s| {
        s.prune(&live);
        s.set_groups(restored_groups, restored_active);
    });
    (model, token)
}

/// Records a workspace the daemon has just created: the group the user filed
/// it into, and its repository as the most recently used one.
///
/// The window keeps the authoritative arrangement in `GroupModel` and reports
/// it through `noteGroups`, but a create is the one mutation the controller can
/// see for itself, so groups survive a restart even before the C++ side wires
/// that signal up.
fn note_workspace_created(group: &str, workspace: &str, repo_path: &str) {
    let group = if group.is_empty() {
        UNSORTED_GROUP
    } else {
        group
    };
    note_state(|s| {
        s.add_to_group(group, workspace);
        s.note_recent(repo_path);
    });
}

/// What `noteEditors` accepts: the object the window builds,
/// `{"open": [...], "active": "..."}`, or a bare array of open paths from
/// anything that does not track which tab is in front. `None` for a string
/// that is neither, which leaves the recorded list alone rather than emptying
/// it over a bug in the caller.
///
/// An "active" path that is not in the open list is dropped: the window cannot
/// be showing a tab it does not have, so recording one would only restore a
/// selection that fails next time.
pub fn parse_editors(json: &str) -> Option<(Vec<String>, Option<String>)> {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Editors {
        open: Vec<String>,
        active: Option<String>,
    }
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let editors: Editors = match value {
        serde_json::Value::Array(_) => Editors {
            open: serde_json::from_value(value).ok()?,
            active: None,
        },
        serde_json::Value::Object(_) => serde_json::from_value(value).ok()?,
        _ => return None,
    };
    let active = editors.active.filter(|a| editors.open.contains(a));
    Some((editors.open, active))
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

        /// A `detect_run_configs` call succeeded: the `DetectRunConfigsResult`
        /// for `path` as JSON, an object with `configs` (an array of
        /// `RunConfig`) and `network_allow` (the hosts the repository's own
        /// `bondsymphonic.toml` would add to the workspace's allowlist).
        /// `path` is echoed back because the New Agent dialog can have asked
        /// about a repository the user has since moved off.
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

        /// The IDE is talking to a daemon again after a connection loss, and
        /// has already *attempted* `system.check_prereqs` and `workspace.list`:
        /// this is emitted after both have answered, whether they succeeded or
        /// failed. A success arrives as `prereqsChecked` / `workspacesListed`
        /// before this signal and a failure as `operationFailed`, so the window
        /// has already seen whichever it was; this only says the re-sync is no
        /// longer in flight. `generation` is the connection number every
        /// subscription is now taken on.
        ///
        /// The Rust-side panes do not need it -- they re-attach through the
        /// generation channel themselves -- so this is for the window: refresh
        /// the status bar, take a "connection lost" banner down, re-enable
        /// anything a loss disabled.
        #[qsignal]
        fn reconnected(self: Pin<&mut AppController>, generation: i64);

        /// Launch the daemon inside WSL and connect to it.
        #[qinvokable]
        fn start(self: Pin<&mut AppController>);

        /// Records that the IDE is shutting down, so the connection about to
        /// end is not treated as a loss to reconnect from.
        ///
        /// Call it from File > Exit and from `MainWindow::closeEvent`, before
        /// anything else: without it, closing the window races the reconnect
        /// loop, which relaunches a daemon inside WSL for an IDE that is on
        /// its way out. Idempotent, and there is no way back -- an IDE that
        /// has said it is quitting does not un-quit.
        #[qinvokable]
        fn prepare_quit(self: &AppController);

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

        // -------------------------------------------------------------------
        // Persistence (`state.json`). Everything below reads or writes the
        // process-wide state store; none of it touches the connection.
        // -------------------------------------------------------------------

        /// The persisted state, as a JSON `StateFile`. Emitted by `loadState`,
        /// so the window restores its geometry, dock layout, splitter sizes
        /// and swap from one signal before anything is connected.
        #[qsignal]
        fn state_loaded(self: Pin<&mut AppController>, json: QString);

        /// The tab model rebuilt from `state.json` and the daemon's *first*
        /// workspace list, as a serialised `Workspaces`. The window hands this
        /// to `GroupModel::loadWorkspaces`; every later list arrives as
        /// `workspacesListed` and goes through `reconcile` as before.
        #[qsignal]
        fn workspaces_restored(self: Pin<&mut AppController>, json: QString);

        /// Reads `state.json` and emits `stateLoaded` with it, returning the
        /// same JSON for a caller that would rather have it directly. Called
        /// before `start`, so the window is laid out before the daemon answers.
        #[qinvokable]
        fn load_state(self: Pin<&mut AppController>) -> QString;

        /// Writes any pending change now rather than in half a second. The
        /// window calls this from `closeEvent`, where there is no half second
        /// left.
        #[qinvokable]
        fn flush_state(self: &AppController);

        /// Records the whole arrangement after a `GroupModel` mutation:
        /// `json` is what `GroupModel::groupsJson()` returns, an object with
        /// `groups` and `active_workspace`.
        #[qinvokable]
        fn note_groups(self: &AppController, json: QString);

        /// Records `workspace_id`'s open editor tabs. `json` is either
        /// `{"open": ["rel/path", ...], "active": "rel/path"}` or a bare array
        /// of the open paths; anything else is refused and the recorded list
        /// left alone.
        #[qinvokable]
        fn note_editors(self: &AppController, workspace_id: QString, json: QString);

        /// Records the agent-area splitter: `sizes_json` is a JSON array of
        /// `QSplitter::sizes()`, `swapped` whether the halves are swapped.
        #[qinvokable]
        fn note_splitter(self: &AppController, sizes_json: QString, swapped: bool);

        /// Records `QMainWindow::saveState()` and `saveGeometry()`, both
        /// base64.
        #[qinvokable]
        fn note_window(self: &AppController, state_b64: QString, geometry_b64: QString);

        /// The repositories the New Agent dialog offers, most recent first, as
        /// a JSON array of paths.
        #[qinvokable]
        fn recent_repos(self: &AppController) -> QString;

        /// Records a repository as the most recently used one.
        #[qinvokable]
        fn note_recent_repo(self: &AppController, path: QString);

        /// The port a run of `config` in `workspace_id` should use instead of
        /// the configured one, or 0 when the user set none.
        #[qinvokable]
        fn port_override(self: &AppController, workspace_id: QString, config: QString) -> i32;

        /// Records that override. 0 (or anything outside 1..=65535) clears it.
        #[qinvokable]
        fn set_port_override(
            self: &AppController,
            workspace_id: QString,
            config: QString,
            port: i32,
        );

        // -------------------------------------------------------------------
        // Merge, pull request and discard, for the Changes toolbar.
        //
        // The three of them and `workspaceSummary` all name a workspace in
        // their answer. `operationFailed` does not, and a banner belongs to one
        // workspace's pane, so these report their own failures on
        // `workspaceOperationFailed` instead -- which also carries the error's
        // `data`, where `GitError`'s stderr is.
        // -------------------------------------------------------------------

        /// A `workspace.merge` answered. `ok` is the daemon's: true means the
        /// base branch moved. False means the merge was attempted and stopped,
        /// with `conflicts_json` a JSON array of repo-relative paths and
        /// `reason` the daemon's tag for why (`"conflict"` today). A merge the
        /// daemon *refused* never gets here; that is
        /// `workspaceOperationFailed`.
        #[qsignal]
        fn merge_finished(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            ok: bool,
            conflicts_json: QString,
            reason: QString,
        );

        /// `workspace.create_pr` succeeded: `url` is the pull request.
        #[qsignal]
        fn pr_created(self: Pin<&mut AppController>, workspace_id: QString, url: QString);

        /// What `workspaceSummary` found, as
        /// `{"dirty": bool, "changed_files": n}`. `changed_files` is -1 when
        /// the daemon could not be asked, so a confirmation can say "its
        /// changed files" rather than claiming a count it does not have.
        #[qsignal]
        fn workspace_summarized(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            json: QString,
        );

        /// A merge, pull request or discard failed. `op` is the daemon method
        /// name, `message` the error's own sentence, and `data_json` the
        /// error's `data` object verbatim (empty when it had none) so the
        /// banner can read `reason` and put a `GitError`'s `stderr` in its
        /// expandable section.
        #[qsignal]
        fn workspace_operation_failed(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            op: QString,
            message: QString,
            data_json: QString,
        );

        /// Merges the workspace's branch into its base. `mode` is "merge",
        /// "rebase" or "squash"; `message` is the squash summary line and is
        /// ignored by the other two, and empty lets the daemon derive it from
        /// the workspace's last commit. Answers with `mergeFinished` or
        /// `workspaceOperationFailed`.
        #[qinvokable]
        fn merge_workspace(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            mode: QString,
            message: QString,
        );

        /// Pushes the workspace's branch and opens a pull request. Answers with
        /// `prCreated` or `workspaceOperationFailed`.
        #[qinvokable]
        fn create_pr(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            title: QString,
            body: QString,
            draft: bool,
        );

        /// Destroys the workspace and everything unmerged in it: a
        /// `workspace.destroy` with `force`. Answers with the same
        /// `workspaceDestroyed` an ordinary destroy does, or with
        /// `workspaceOperationFailed` so the reason lands in the workspace's
        /// own banner rather than in a box over the window.
        #[qinvokable]
        fn discard_workspace(self: Pin<&mut AppController>, workspace_id: QString);

        /// Asks what would be lost with the workspace, for the Discard
        /// confirmation. Answers with `workspaceSummarized`.
        #[qinvokable]
        fn workspace_summary(self: Pin<&mut AppController>, workspace_id: QString);

        /// Whether `workspace_id` has a merge, pull request, discard or destroy
        /// out. Anything that offers one of those to the user asks this before
        /// enabling it.
        ///
        /// The interlock is here rather than in the Changes toolbar because the
        /// toolbar is not the only caller: the tab context menu destroys and
        /// `CloseGroupRunner` merges and discards, and a toolbar-local set
        /// leaves both of those walking around it. A refusal is enforced here
        /// too -- this is what a view *asks*, not what protects the daemon.
        #[qinvokable]
        fn is_workspace_busy(self: &AppController, workspace_id: QString) -> bool;

        /// `workspace_id` started or finished one. Emitted on every change, so
        /// a view that greys buttons out has something to re-run its enabling
        /// on rather than polling.
        #[qsignal]
        fn workspace_busy_changed(self: Pin<&mut AppController>, workspace_id: QString, busy: bool);
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
    /// Whether a `workspace.list` has already been answered on this run. The
    /// first one restores the persisted arrangement; every later one is an
    /// ordinary reconcile.
    first_list_done: bool,
    /// Which reconnect attempt the status text is counting. Kept here because
    /// the `connection_state` property is one integer and cannot carry it, so
    /// anything recomposing the status from that property alone would
    /// otherwise lose the number.
    reconnect_attempt: u32,
    /// The workspaces with a merge, pull request, discard or destroy out.
    ///
    /// Here rather than in the Changes toolbar because the toolbar is not the
    /// only way to start one: the tab context menu destroys, and
    /// `CloseGroupRunner` merges and discards without going near it. Every one
    /// of those calls goes through this object, so this is the one place a
    /// second operation on a workspace that is already busy can be refused --
    /// and a discard racing a merge is how the user's base branch ends up
    /// pointing at objects the discard deleted.
    busy: crate::model::app_state::BusyWorkspaces,
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
            first_list_done: false,
            reconnect_attempt: 0,
            busy: crate::model::app_state::BusyWorkspaces::default(),
        }
    }
}

/// What a merge, pull request, discard or destroy is refused with while the
/// same workspace already has one out.
pub const WORKSPACE_BUSY: &str =
    "another merge, pull request or discard is still running on this workspace";

/// The three merge modes, by the words the toolbar and the daemon both use.
/// Anything else is refused here rather than sent on: an unrecognised mode is
/// a bug in the caller, and guessing one would move a branch nobody asked to
/// move.
pub fn parse_merge_mode(word: &str) -> Option<MergeMode> {
    match word {
        "merge" => Some(MergeMode::Merge),
        "rebase" => Some(MergeMode::Rebase),
        "squash" => Some(MergeMode::Squash),
        _ => None,
    }
}

/// A failed request as the pair a workspace banner needs: the sentence to
/// show, and the error's `data` verbatim.
///
/// The `data` is passed through rather than picked apart because the daemon
/// keeps growing keys in it -- `reason` for a refusal, `command`/`exit_code`/
/// `stderr` for a `GitError`, `merged`/`pushed` for work that landed and could
/// not be copied out -- and the banner is the thing that decides which of them
/// it can render. An error that is not an `RpcError` at all (a timeout, a
/// closed socket) has no data, only its own text.
pub fn failure_parts(e: &crate::client::ClientError) -> (String, String) {
    match e {
        crate::client::ClientError::Rpc(rpc) => {
            let data = rpc.data.as_ref().map(|d| d.to_string()).unwrap_or_default();
            (rpc.message.clone(), data)
        }
        other => (other.to_string(), String::new()),
    }
}

/// Queues an `operation_failed` for `op` back onto the Qt thread.
fn report_failure(qt: &QtHandle, op: &'static str, message: String) {
    tracing::warn!("{op} failed: {message}");
    let _ = qt.queue(move |q| q.operation_failed(QString::from(op), QString::from(&message)));
}

/// Queues a `workspace_operation_failed` back onto the Qt thread. Used instead
/// of [`report_failure`] wherever the failure belongs to one workspace's pane,
/// so the window can raise a banner there rather than a box over everything.
fn report_workspace_failure(
    qt: &QtHandle,
    workspace: String,
    op: &'static str,
    message: String,
    data: String,
) {
    tracing::warn!("{op} failed for {workspace}: {message} {data}");
    let _ = qt.queue(move |q| {
        q.workspace_operation_failed(
            QString::from(&workspace),
            QString::from(op),
            QString::from(&message),
            QString::from(&data),
        )
    });
}

/// The same, for a failure that ends an operation booked in by
/// [`AppController::begin_workspace_op`]: the slot is given back in the same
/// queued step that raises the banner, so nothing can report the failure and
/// leave the workspace looking busy forever.
///
/// Separate from [`report_workspace_failure`] rather than folded into it,
/// because the one failure that must *not* free the slot is the refusal of a
/// second operation while the first is still running -- freeing it there would
/// hand the running operation's booking to the request that was just refused.
fn end_workspace_op_with_failure(
    qt: &QtHandle,
    workspace: String,
    op: &'static str,
    message: String,
    data: String,
) {
    tracing::warn!("{op} failed for {workspace}: {message} {data}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().end_workspace_op(&workspace);
        q.workspace_operation_failed(
            QString::from(&workspace),
            QString::from(op),
            QString::from(&message),
            QString::from(&data),
        )
    });
}

/// A `destroy_workspace` that failed: the booking goes back and the failure is
/// reported on `operationFailed`, which is where a plain destroy's errors have
/// always gone (a discard's go to the workspace's own banner instead).
fn end_destroy(qt: &QtHandle, workspace: String, message: String) {
    tracing::warn!("workspace.destroy failed for {workspace}: {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().end_workspace_op(&workspace);
        q.operation_failed(QString::from("workspace.destroy"), QString::from(&message))
    });
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
    // The stream only ends when the connection does. What happens next is
    // [`supervise`]'s decision, not this task's: it is awaiting this very task
    // and will drop the shared handle, reap the daemon and start the reconnect
    // clock. Doing any of that here as well would race it.
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
                let list = res.workspaces;
                let _ = qt.queue(move |mut q| {
                    // The first list is the one the persisted arrangement is
                    // reconciled against: the groups come back as the user left
                    // them, and the workspaces the daemon has lost are dropped
                    // from the file. Every later list is an ordinary reconcile.
                    if !q.as_ref().rust().first_list_done {
                        q.as_mut().rust_mut().first_list_done = true;
                        let (model, token) = restore_workspaces(state_store(), &list);
                        schedule_flush(token);
                        q.as_mut()
                            .workspaces_restored(QString::from(&model.to_json()));
                    }
                    q.workspaces_listed(QString::from(&json));
                });
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

/// The daemon connection for the whole life of the IDE: the first launch, and
/// then a relaunch after every loss until the window says it is quitting.
///
/// One task, one loop, so there can only ever be one reconnect in flight: the
/// loop is not connecting while a connection is up, and it is not connected
/// while it is waiting out a backoff. Everything that decides *whether* to
/// reconnect is here rather than spread across the event drain and the process
/// handle, which is what makes that guarantee readable.
async fn supervise(spec: LaunchSpec, qt: QtHandle, process: ProcessHandle) {
    let mut attempt: u32 = 0;
    loop {
        if attempt > 0 {
            let delay = backoff_delay(attempt);
            tracing::warn!("reconnecting to the daemon in {delay:?} (attempt {attempt})");
            let _ = qt.queue(move |q| q.set_state(ConnectionState::Reconnecting { attempt }));
            tokio::time::sleep(delay).await;
            if quitting() {
                return;
            }
        }
        let mut drain = match connect_once(&spec, &qt, &process, attempt).await {
            Ok(drain) => drain,
            Err(failure) => {
                let fatal = matches!(failure, ConnectFailure::Fatal(_));
                let message = failure.into_message();
                // A quit that arrived while this attempt was in flight. Nothing
                // to report and nothing to retry: the window is going, and the
                // error state below would put a sentence about quitting in a
                // status bar the user is closing.
                if quitting() {
                    tracing::info!("{message}");
                    return;
                }
                if fatal || attempt == 0 {
                    // Either the very first launch never came up -- a machine
                    // that cannot run the daemon at all: a missing distro, a
                    // bad path, a binary that will not start -- or the failure
                    // is one no retry can change, which is a daemon speaking
                    // another protocol version. Retrying either every second
                    // would only repeat the same sentence behind a status bar
                    // that says "reconnecting". The error text and the setup
                    // page are the answer to both instead.
                    tracing::error!("{message}");
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().set_state(ConnectionState::Error);
                        q.set_status_message(QString::from(message.as_str()));
                    });
                    return;
                }
                tracing::warn!("reconnect attempt {attempt} failed: {message}");
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let connected_at = Instant::now();

        // Connected. Two things end a connection and the loop acts on
        // whichever comes first: the event stream ending (the socket died) or
        // the daemon process exiting (the relay was killed, the daemon
        // crashed). `select!` drops the losing future, which is what lets
        // `wait_for_process` hold the process lock across its await.
        let reason = tokio::select! {
            _ = &mut drain => "the daemon's event stream ended",
            reason = wait_for_process(&process) => {
                // The stream has not ended yet, but nothing will answer on it.
                drain.abort();
                reason
            }
        };

        // The dead client goes first: an operation attempted in the gap then
        // fails at once, with a reason, instead of issuing a request nobody
        // will answer.
        on_connection_lost();
        let _ = qt.queue(|mut q| {
            q.as_mut().rust_mut().client = None;
        });
        // The old child is reaped rather than left behind: `shutdown` closes
        // its stdin, which is how the daemon is asked to exit, and then kills
        // the relay -- so a relaunch cannot end up with two daemons over one
        // data directory.
        let old = process.lock().await.take();
        if let Some(old) = old {
            old.shutdown().await;
        }
        if quitting() {
            tracing::info!("{reason}; the IDE is quitting, so nothing is reconnected");
            let _ = qt.queue(|q| q.set_state(ConnectionState::Lost));
            return;
        }
        // How long the connection lasted is what decides whether the schedule
        // starts over. A daemon that answers `hello` and dies a second later is
        // crash-looping, not recovering, and must not be able to hold the
        // backoff at one second for the rest of the session.
        let held = connected_at.elapsed();
        tracing::warn!("{reason} after {held:?}");
        attempt = next_attempt(attempt, held);
    }
}

/// Resolves when the daemon process ends, and never when there is none.
///
/// Holding the lock across the await is deliberate. The only other thing that
/// takes it is [`supervise`], and only after this future has been dropped by
/// the `select!` that owned it, so the wait can never block the reaping that
/// follows it.
async fn wait_for_process(process: &ProcessHandle) -> &'static str {
    let mut guard = process.lock().await;
    match guard.as_mut() {
        Some(daemon) => {
            let status = daemon.wait_exit().await;
            tracing::warn!("the daemon process ended: {status:?}");
            "the daemon process exited"
        }
        // The `BS_DAEMON_ADDR` test hook: there is no process of ours to
        // watch, so the event stream is the only signal and this parks.
        None => std::future::pending().await,
    }
}

/// Why one connection attempt did not produce a connection.
///
/// The distinction is what the backoff loop reads: a daemon that is not up yet
/// is worth trying again, and a daemon speaking another version of the protocol
/// is not -- every attempt would be refused in exactly the same way, behind a
/// status bar claiming something was being tried.
enum ConnectFailure {
    /// Worth another attempt on the backoff schedule.
    Retryable(String),
    /// Nothing left to try: the status bar says this and the loop ends.
    Fatal(String),
}

impl ConnectFailure {
    fn into_message(self) -> String {
        match self {
            Self::Retryable(m) | Self::Fatal(m) => m,
        }
    }
}

/// What the status bar says when the IDE and the daemon do not speak the same
/// protocol. Both numbers are in it: which half is out of date is the user's
/// next question, and only the pair of versions answers it.
pub fn mismatch_status(daemon: u32, client: u32) -> String {
    format!("daemon: protocol mismatch (daemon {daemon}, IDE {client})")
}

/// One launch-connect-resync cycle. `attempt` is 0 for the first connection of
/// the run and counts the reconnects after that.
///
/// Returns the handle of the task draining this connection's events, which is
/// what [`supervise`] waits on to learn that the connection has ended.
async fn connect_once(
    spec: &LaunchSpec,
    qt: &QtHandle,
    process: &ProcessHandle,
    attempt: u32,
) -> Result<tokio::task::JoinHandle<()>, ConnectFailure> {
    let first = attempt == 0;
    // A protocol mismatch is answered by reinstalling the daemon this IDE
    // ships with, once. The flag is what makes it once: the reinstall is a
    // no-op when the copy in the distro already matches the local one, so a
    // second refusal is a pair that really cannot talk, and nothing here can
    // change that.
    let mut reinstalled = false;
    let (client, hello, events) = loop {
        // Checked here as well as in `supervise`'s backoff sleep, because that check
        // is followed by a `launch` that takes seconds: a quit announced inside that
        // window would otherwise start one more daemon for an IDE on its way out.
        // Checked before the launch rather than after it, so nothing is started that
        // then has to be shut down.
        if quitting() {
            return Err(ConnectFailure::Retryable(
                "the IDE is quitting; no daemon was launched".to_owned(),
            ));
        }
        // `test_endpoint` is the `BS_DAEMON_ADDR` test hook and is `None` in every
        // ordinary run, which then launches the daemon inside WSL. Under the hook
        // a reconnect only reconnects: there is no daemon of ours to relaunch, and
        // the fake one is expected to be listening on the same port again.
        let (addr, token, launcher_owns_the_daemon) = match launcher::test_endpoint() {
            Some((addr, token)) => {
                if first && !reinstalled {
                    tracing::warn!(
                        "{} is set: connecting to {addr} instead of launching a daemon",
                        launcher::TEST_ADDR_ENV
                    );
                }
                (addr, token, false)
            }
            None => {
                if first {
                    let _ = qt.queue(|q| q.set_state(ConnectionState::Launching));
                }
                let daemon = launcher::launch(spec).await.map_err(|e| {
                    ConnectFailure::Retryable(format!("daemon: launch failed: {e:#}"))
                })?;
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], daemon.port));
                let token = daemon.token.clone();
                *process.lock().await = Some(daemon);
                (addr, token, true)
            }
        };
        // Only on the first connection: a reconnect's status text is
        // "daemon: reconnecting (attempt N)", and overwriting it with "connecting"
        // would take away the count the user is watching.
        if first {
            let _ = qt.queue(|q| q.set_state(ConnectionState::Connecting));
        }

        match DaemonClient::connect(addr, &token, env!("CARGO_PKG_VERSION")).await {
            Ok(connected) => break connected,
            Err(e) => {
                // A daemon we just launched but cannot talk to. Reaped here
                // rather than left as an orphan behind every failed attempt.
                let orphan = process.lock().await.take();
                if let Some(orphan) = orphan {
                    orphan.shutdown().await;
                }
                let ClientError::ProtocolMismatch { daemon, client } = e else {
                    return Err(ConnectFailure::Retryable(format!(
                        "daemon: connect failed: {e}"
                    )));
                };
                let text = mismatch_status(daemon, client);
                // Nothing of ours to replace under the test hook, and nothing
                // to replace twice. Either way the pair cannot talk, so the
                // loop stops here rather than backing off against a refusal
                // that would be the same every time.
                if !launcher_owns_the_daemon || reinstalled {
                    return Err(ConnectFailure::Fatal(text));
                }
                reinstalled = true;
                tracing::warn!("{text}; reinstalling the daemon this IDE ships with");
                if let Err(e) = launcher::install_daemon(spec).await {
                    return Err(ConnectFailure::Fatal(format!(
                        "{text}; reinstalling the daemon failed: {e:#}"
                    )));
                }
            }
        }
    };

    // Published before any event is dispatched, so a QObject woken by the
    // first `workspaces_listed` -- or by the generation change this publish
    // *is* -- can already reach the daemon.
    let router = EventRouter::new();
    publish_shared(Shared {
        client: client.clone(),
        router: router.clone(),
    });
    let generation = connection_generation();

    // Draining starts before any request goes out. The client's event
    // channel is bounded, and its socket reader pushes into it with
    // backpressure, so a daemon that is already streaming (a reconnect to
    // one with live PTYs) would otherwise fill the channel, stall the
    // reader, and starve every reply below until the request timeout.
    let drain = runtime().spawn(drain_events(events, router, qt.clone()));

    let version = hello.daemon_version.clone();
    let capabilities = serde_json::to_string(&hello.capabilities).unwrap_or_default();
    let c2 = client.clone();
    let _ = qt.queue(move |mut q| {
        q.as_mut().rust_mut().client = Some(c2);
        q.as_mut().rust_mut().capabilities = QString::from(&capabilities);
        // State first, then the version, so `apply_daemon_version`
        // recomposes the text as "daemon: connected v<version>".
        q.as_mut().set_state(ConnectionState::Connected);
        q.apply_daemon_version(QString::from(version.as_str()));
    });

    if first {
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
        check_prereqs(client, qt.clone()).await;
    } else {
        // A reconnect re-syncs in order and announces itself only once that is
        // done, so anything listening for `reconnected` can take it that the
        // prerequisites and the workspace list have already been reported.
        // Awaited rather than spawned for exactly that reason.
        check_prereqs(client.clone(), qt.clone()).await;
        load_workspace_list(client, qt.clone()).await;
        let announced = i64::try_from(generation).unwrap_or(i64::MAX);
        tracing::info!("reconnected on connection generation {generation}");
        let _ = qt.queue(move |q| q.reconnected(announced));
    }
    Ok(drain)
}

impl qobject::AppController {
    pub fn start(mut self: Pin<&mut Self>) {
        if SUPERVISING.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!("start() called again; the daemon connection is already supervised");
            return;
        }
        let qt = self.as_ref().qt_thread();
        let settings = Settings::load();
        let spec = LaunchSpec {
            distro: settings.distro.clone(),
            daemon_path_in_wsl: settings.daemon_path.clone(),
            local_daemon_binary: Settings::local_daemon_binary(),
            log_level: settings.log_level.clone(),
        };
        // The slot the reconnect loop empties and refills. Kept on the
        // controller as well, so a later Exit action reaches the daemon that
        // is running now rather than the one that was running at start-up.
        let process: ProcessHandle = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        self.as_mut().rust_mut().process = Some(process.clone());
        runtime().spawn(supervise(spec, qt, process));
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
        // Kept for `state.json`, which the echo above does not reach.
        let (group_for_state, repo_for_state) = (group.clone(), params.repo_path.clone());
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
                    note_workspace_created(&group_for_state, &info.id.0, &repo_for_state);
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
        // Kept for `state.json`, which the echo below does not reach.
        let (group_for_state, repo_for_state) = (group.clone(), params.repo_path.clone());
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
            note_workspace_created(&group_for_state, &info.id.0, &repo_for_state);
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

    pub fn destroy_workspace(mut self: Pin<&mut Self>, id: QString, force: bool) {
        let qt = self.qt_thread();
        let id = id.to_string();
        // The same interlock the Changes toolbar's Discard goes through, and
        // for the same reason: this is reached from the tab context menu, which
        // consults nothing, and destroying a workspace whose merge is still
        // absorbing objects out of it leaves the base branch pointing at
        // commits whose parents have been deleted.
        if !self.as_mut().begin_workspace_op(&id) {
            report_failure(&qt, "workspace.destroy", WORKSPACE_BUSY.to_owned());
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                end_destroy(&qt, id, message.to_owned());
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
                    // The workspace is gone: its group entry, editors and port
                    // overrides go with it rather than waiting for the next
                    // start to prune them.
                    note_state(|s| s.forget_workspace(&id));
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_workspace_op(&id);
                        q.workspace_destroyed(QString::from(&id))
                    });
                }
                Err(e) => end_destroy(&qt, id, e.to_string()),
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
                    // The whole result, not just the array: the dialog also has
                    // to name the hosts this repository would add to the
                    // workspace's network allowlist. Readers that only want the
                    // configurations take `configs` out of the object.
                    let json = serde_json::to_string(&res)
                        .unwrap_or_else(|_| r#"{"configs":[],"network_allow":[]}"#.into());
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
        if let ConnectionState::Reconnecting { attempt } = state {
            self.as_mut().rust_mut().reconnect_attempt = attempt;
        }
        self.as_mut().set_connection_state(state.as_i32());
        self.refresh_status(state);
    }

    pub fn apply_daemon_version(mut self: Pin<&mut Self>, version: QString) {
        self.as_mut().set_daemon_version(version);
        let state = self.as_ref().current_state();
        self.refresh_status(state);
    }

    pub fn prepare_quit(&self) {
        // No un-setting: an IDE that has announced it is quitting does not
        // change its mind, and a way back would only be a way for a stray
        // caller to re-arm the reconnect loop during teardown.
        QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("the IDE is quitting; the daemon will not be relaunched");
    }

    /// The connection state the property holds, with the reconnect attempt put
    /// back. The property cannot carry the attempt, so anything that needs the
    /// whole state reads it through here rather than through `from_i32` alone.
    fn current_state(&self) -> ConnectionState {
        match ConnectionState::from_i32(*self.connection_state()) {
            ConnectionState::Reconnecting { .. } => ConnectionState::Reconnecting {
                attempt: self.rust().reconnect_attempt,
            },
            other => other,
        }
    }

    /// Recomposes `status_message` from `state` and the current `daemon_version`.
    ///
    /// The text is logged as well as published. The status bar is the one place
    /// a user watches a reconnect happen, and an offscreen run has no status
    /// bar to read: without this, "reconnecting (attempt 1)" would be a claim
    /// no test could check. Only a change is logged, so recomposing the same
    /// text (which `apply_daemon_version` does) says nothing twice.
    fn refresh_status(mut self: Pin<&mut Self>, state: ConnectionState) {
        let version = self.daemon_version().to_string();
        let text = compose_status(state, &version);
        if self.as_ref().status_message().to_string() != text {
            tracing::info!(target: "connection", "{text}");
        }
        self.as_mut().set_status_message(QString::from(&text));
    }

    // -----------------------------------------------------------------------
    // Persistence (`state.json`). Thin adapters over the process-wide store;
    // the rules live in `model/persistence.rs`.
    // -----------------------------------------------------------------------

    pub fn load_state(mut self: Pin<&mut Self>) -> QString {
        let json = state_store().to_json();
        tracing::info!(
            path = ?state_store().path(),
            "state loaded"
        );
        self.as_mut().state_loaded(QString::from(&json));
        QString::from(&json)
    }

    pub fn flush_state(&self) {
        if state_store().flush() {
            tracing::info!("state written before exit");
        }
    }

    pub fn note_groups(&self, json: QString) {
        let json = json.to_string();
        match serde_json::from_str::<persistence::PersistedGroups>(&json) {
            Ok(reported) => {
                note_state(|s| s.set_groups(reported.groups, reported.active_workspace))
            }
            // Refused rather than applied as an empty arrangement: that would
            // record every group as deleted over a bug in the caller.
            Err(e) => tracing::warn!("noteGroups: unparseable arrangement ({e})"),
        }
    }

    pub fn note_editors(&self, workspace_id: QString, json: QString) {
        let workspace = workspace_id.to_string();
        let Some((open, active)) = parse_editors(&json.to_string()) else {
            tracing::warn!("noteEditors: unparseable editor list for {workspace}");
            return;
        };
        note_state(|s| {
            if open.is_empty() {
                s.open_editors.remove(&workspace);
            } else {
                s.open_editors.insert(workspace.clone(), open);
            }
            match active {
                Some(active) => {
                    s.active_editor.insert(workspace, active);
                }
                None => {
                    s.active_editor.remove(&workspace);
                }
            }
        });
    }

    pub fn note_splitter(&self, sizes_json: QString, swapped: bool) {
        let sizes: Vec<i32> = match serde_json::from_str(&sizes_json.to_string()) {
            Ok(sizes) => sizes,
            Err(e) => {
                tracing::warn!("noteSplitter: unparseable sizes ({e})");
                return;
            }
        };
        note_state(|s| {
            s.splitter_sizes = sizes;
            s.swapped = swapped;
        });
    }

    pub fn note_window(&self, state_b64: QString, geometry_b64: QString) {
        let (window, geometry) = (state_b64.to_string(), geometry_b64.to_string());
        note_state(|s| {
            s.window_state_b64 = window;
            s.geometry_b64 = geometry;
        });
    }

    pub fn recent_repos(&self) -> QString {
        let json = state_store().with(|s| serde_json::to_string(&s.recent_repos));
        QString::from(&json.unwrap_or_else(|_| "[]".into()))
    }

    pub fn note_recent_repo(&self, path: QString) {
        let path = path.to_string();
        note_state(|s| s.note_recent(&path));
    }

    pub fn port_override(&self, workspace_id: QString, config: QString) -> i32 {
        let (workspace, config) = (workspace_id.to_string(), config.to_string());
        state_store()
            .with(|s| s.port_override(&workspace, &config))
            .map(i32::from)
            .unwrap_or(0)
    }

    pub fn set_port_override(&self, workspace_id: QString, config: QString, port: i32) {
        let (workspace, config) = (workspace_id.to_string(), config.to_string());
        // A spin box at its minimum means "no override", and so does anything
        // that is not a port number at all.
        let port = u16::try_from(port).ok().filter(|p| *p != 0);
        note_state(|s| s.set_port_override(&workspace, &config, port));
    }

    // -----------------------------------------------------------------------
    // Merge, pull request and discard. Each is one request; the daemon decides
    // everything about it, and what is here is only the crossing.
    // -----------------------------------------------------------------------

    pub fn is_workspace_busy(&self, workspace_id: QString) -> bool {
        self.rust().busy.contains(&workspace_id.to_string())
    }

    /// Books an operation in for `workspace`, or answers false because one is
    /// already out.
    ///
    /// False is a refusal, not a queue: the two things that must never overlap
    /// are a merge and the destroy that deletes the objects it is still packing
    /// out, and running the second one late is the same data loss as running it
    /// now. The user is told, and can ask again when the first has answered.
    fn begin_workspace_op(mut self: Pin<&mut Self>, workspace: &str) -> bool {
        if !self.as_mut().rust_mut().busy.begin(workspace) {
            return false;
        }
        self.workspace_busy_changed(QString::from(workspace), true);
        true
    }

    /// Books it out again. Called for every answer, success or failure, and
    /// harmless for a workspace that was never booked in.
    fn end_workspace_op(mut self: Pin<&mut Self>, workspace: &str) {
        if !self.as_mut().rust_mut().busy.end(workspace) {
            return;
        }
        self.workspace_busy_changed(QString::from(workspace), false);
    }

    pub fn merge_workspace(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        mode: QString,
        message: QString,
    ) {
        let qt = self.qt_thread();
        let workspace = workspace_id.to_string();
        let word = mode.to_string();
        let Some(mode) = parse_merge_mode(&word) else {
            report_workspace_failure(
                &qt,
                workspace,
                "workspace.merge",
                format!("unknown merge mode {word:?}"),
                String::new(),
            );
            return;
        };
        // Empty means "not supplied": the daemon then takes the summary from
        // the workspace's last commit, which is the whole point of the field
        // being optional.
        let summary = message.to_string();
        let summary = (!summary.trim().is_empty()).then_some(summary);
        if !self.as_mut().begin_workspace_op(&workspace) {
            report_workspace_failure(
                &qt,
                workspace,
                "workspace.merge",
                WORKSPACE_BUSY.to_owned(),
                String::new(),
            );
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(e) => {
                end_workspace_op_with_failure(
                    &qt,
                    workspace,
                    "workspace.merge",
                    e.to_owned(),
                    String::new(),
                );
                return;
            }
        };
        runtime().spawn(async move {
            let params = WorkspaceMergeParams {
                workspace_id: WorkspaceId(workspace.clone()),
                mode,
                message: summary,
            };
            match shared
                .client
                .request::<MergeResult>(Request::WorkspaceMerge(params))
                .await
            {
                Ok(result) => {
                    let conflicts = serde_json::to_string(&result.conflicts)
                        .unwrap_or_else(|_| "[]".to_owned());
                    let reason = result.reason.unwrap_or_default();
                    tracing::info!(
                        %workspace,
                        ok = result.ok,
                        conflicts = result.conflicts.len(),
                        %reason,
                        "workspace.merge answered"
                    );
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_workspace_op(&workspace);
                        q.merge_finished(
                            QString::from(&workspace),
                            result.ok,
                            QString::from(&conflicts),
                            QString::from(&reason),
                        )
                    });
                }
                Err(e) => {
                    let (message, data) = failure_parts(&e);
                    end_workspace_op_with_failure(&qt, workspace, "workspace.merge", message, data);
                }
            }
        });
    }

    pub fn create_pr(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        title: QString,
        body: QString,
        draft: bool,
    ) {
        let qt = self.qt_thread();
        let workspace = workspace_id.to_string();
        let params = WorkspaceCreatePrParams {
            workspace_id: WorkspaceId(workspace.clone()),
            title: title.to_string(),
            body: body.to_string(),
            draft,
        };
        if !self.as_mut().begin_workspace_op(&workspace) {
            report_workspace_failure(
                &qt,
                workspace,
                "workspace.create_pr",
                WORKSPACE_BUSY.to_owned(),
                String::new(),
            );
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(e) => {
                end_workspace_op_with_failure(
                    &qt,
                    workspace,
                    "workspace.create_pr",
                    e.to_owned(),
                    String::new(),
                );
                return;
            }
        };
        runtime().spawn(async move {
            match shared
                .client
                .request::<CreatePrResult>(Request::WorkspaceCreatePr(params))
                .await
            {
                Ok(result) => {
                    tracing::info!(%workspace, url = %result.url, "workspace.create_pr answered");
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_workspace_op(&workspace);
                        q.pr_created(QString::from(&workspace), QString::from(&result.url))
                    });
                }
                Err(e) => {
                    let (message, data) = failure_parts(&e);
                    end_workspace_op_with_failure(
                        &qt,
                        workspace,
                        "workspace.create_pr",
                        message,
                        data,
                    );
                }
            }
        });
    }

    pub fn discard_workspace(mut self: Pin<&mut Self>, workspace_id: QString) {
        let qt = self.qt_thread();
        let workspace = workspace_id.to_string();
        if !self.as_mut().begin_workspace_op(&workspace) {
            report_workspace_failure(
                &qt,
                workspace,
                "workspace.destroy",
                WORKSPACE_BUSY.to_owned(),
                String::new(),
            );
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(e) => {
                end_workspace_op_with_failure(
                    &qt,
                    workspace,
                    "workspace.destroy",
                    e.to_owned(),
                    String::new(),
                );
                return;
            }
        };
        runtime().spawn(async move {
            let params = WorkspaceDestroyParams {
                workspace_id: WorkspaceId(workspace.clone()),
                // A discard is exactly a forced destroy. The confirmation
                // naming what is lost happens in front of the user, in the
                // toolbar, and is the whole of the protection.
                force: true,
            };
            match shared
                .client
                .request_raw(Request::WorkspaceDestroy(params))
                .await
            {
                Ok(_) => {
                    note_state(|s| s.forget_workspace(&workspace));
                    tracing::info!(%workspace, "workspace discarded");
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_workspace_op(&workspace);
                        q.workspace_destroyed(QString::from(&workspace))
                    });
                }
                Err(e) => {
                    let (message, data) = failure_parts(&e);
                    end_workspace_op_with_failure(
                        &qt,
                        workspace,
                        "workspace.destroy",
                        message,
                        data,
                    );
                }
            }
        });
    }

    pub fn workspace_summary(self: Pin<&mut Self>, workspace_id: QString) {
        let qt = self.qt_thread();
        let workspace = workspace_id.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(e) => {
                // Not a failure worth a banner: the confirmation it feeds can
                // stand without a count, and the connection has already said
                // so everywhere else.
                tracing::warn!("workspace summary for {workspace}: {e}");
                let _ = qt.queue(move |q| {
                    q.workspace_summarized(
                        QString::from(&workspace),
                        QString::from(UNKNOWN_SUMMARY),
                    )
                });
                return;
            }
        };
        runtime().spawn(async move {
            let id = WorkspaceId(workspace.clone());
            let status = shared
                .client
                .request::<WorkspaceStatusResult>(Request::WorkspaceStatus(WorkspaceIdParams {
                    workspace_id: id.clone(),
                }))
                .await;
            let changes = shared
                .client
                .request::<ChangesResult>(Request::WorkspaceChanges(WorkspaceIdParams {
                    workspace_id: id,
                }))
                .await;
            // Either half failing leaves the count unknown rather than zero: a
            // confirmation that says "its 0 changed files will be lost" over a
            // workspace nobody could ask about is the one wording that would
            // talk a user into a discard.
            let json = match (status, changes) {
                (Ok(status), Ok(changes)) => serde_json::json!({
                    "dirty": !status.entries.is_empty(),
                    "changed_files": changes.files.len(),
                })
                .to_string(),
                (status, changes) => {
                    let reason = status
                        .err()
                        .map(|e| e.to_string())
                        .or_else(|| changes.err().map(|e| e.to_string()))
                        .unwrap_or_default();
                    tracing::warn!("workspace summary for {workspace}: {reason}");
                    UNKNOWN_SUMMARY.to_owned()
                }
            };
            let _ = qt.queue(move |q| {
                q.workspace_summarized(QString::from(&workspace), QString::from(&json))
            });
        });
    }
}

/// What `workspaceSummarized` carries when the daemon could not be asked. The
/// -1 is the "unknown" the Discard confirmation branches on.
const UNKNOWN_SUMMARY: &str = r#"{"dirty":false,"changed_files":-1}"#;
