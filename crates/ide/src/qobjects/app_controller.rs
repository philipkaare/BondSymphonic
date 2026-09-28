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
use crate::model::app_state::{compose_status, ConnectionState, Workspaces};
use crate::model::persistence::{self, StateFile, StateStore};
use crate::model::transcript::agent_state_word;
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

/// The same, for a change that may turn out not to be one: nothing is written
/// and no timer is started when `f` reports that the state already said this.
/// See [`StateStore::update_changed`].
pub fn note_state_if_changed(f: impl FnOnce(&mut StateFile) -> bool) {
    if let Some(token) = state_store().update_changed(f) {
        schedule_flush(token);
    }
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
    // The same rule `GroupModel::addTab` files the tab itself under, so the tab
    // the user sees and the entry saved for it name one group.
    let group = crate::qobjects::group_model::group_or_unsorted(group);
    note_state(|s| {
        s.add_to_group(group, workspace);
        s.note_recent(repo_path);
    });
}

/// The `workspace.create` the New Agent dialog's answers make. The base branch
/// of an in-place create is not sent: the daemon ignores it, and a branch name
/// on the wire would read as though one had been chosen.
pub fn create_params(
    repo_path: String,
    base_branch: String,
    name: String,
    init_if_missing: bool,
    in_place: bool,
) -> WorkspaceCreateParams {
    WorkspaceCreateParams {
        repo_path,
        base_branch: if in_place { String::new() } else { base_branch },
        name,
        init_if_missing,
        in_place,
    }
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
        /// Whether Claude Code is logged in, from the daemon's last
        /// `system.check_prereqs`. False before the first answer.
        ///
        /// Every Claude transcript hides its composer behind a "Log in to
        /// Claude Code…" button while this is false, because an agent runs
        /// `claude -p` and `-p` mode cannot log in. It becomes true on the
        /// re-check a setup terminal triggers when it exits, so the composer
        /// comes back without an IDE restart.
        #[qproperty(bool, claude_logged_in)]
        /// Whether a `system.check_prereqs` has answered on the live
        /// connection. False at start-up, true from the first `prereqs_checked`
        /// on, and false again when the connection is lost until the reconnect's
        /// own check answers.
        ///
        /// `claude_logged_in` is false before the first answer, and a composer
        /// gate that read that alone said "Claude Code is not logged in" and
        /// offered the login to a user who was. On a freshly booted distro the
        /// first check can take ten seconds, which is plenty of time to open
        /// Settings and quit. This is what lets the gate say it is still
        /// checking instead. A check the daemon could not answer leaves this
        /// false: the question is still open, and the status bar already says
        /// why.
        #[qproperty(bool, prereqs_answered)]
        /// What the Claude Code CLI itself said when an agent could not
        /// authenticate, or empty. Non-empty overrides the daemon's
        /// `claude_auth` tick: `claude_logged_in` is false for as long as this
        /// is set, whatever the last check answered.
        ///
        /// The daemon's check runs `claude auth status`, which reads the
        /// credentials file and says logged in even when the refresh token in
        /// it is dead. Only the CLI finds that out, at start, and says so once
        /// in its exit detail -- and a re-check afterwards comes back green
        /// again. See `AppControllerRust::claude_auth_override` for when it is
        /// set and cleared. The composer gate shows this sentence verbatim.
        #[qproperty(QString, claude_auth_failure)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn prereq_warning(self: Pin<&mut AppController>, message: QString);

        /// Every prerequisite the daemon reported, as a JSON array of
        /// `PrereqStatus`. Emitted after every check: the one at start-up and
        /// every `recheck_prereqs`. This is what the setup page draws its rows
        /// from, and what the window decides to show the page at all from.
        #[qsignal]
        fn prereqs_checked(self: Pin<&mut AppController>, json: QString);

        /// The newest model of each family, as a JSON array of
        /// `{"id", "label"}` objects (see
        /// [`crate::model::models::model_choices`]). Emitted once per
        /// connection, after `system.list_models` has answered -- never on a
        /// failure, which is a warning in the log rather than a signal: the
        /// model combo's C++ fallback list is what the dropdowns already show,
        /// and losing it over a fetch nobody asked to see fail would be a
        /// worse day than staying on it.
        #[qsignal]
        fn models_checked(self: Pin<&mut AppController>, json: QString);

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

        /// A create succeeded. `group`, `adapter`, `command`,
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

        /// An unforced `destroy_workspace` was refused because the workspace
        /// has uncommitted changes (`dirty`) or commits its base branch does
        /// not have (`unmerged`) -- or, for a worktree git no longer lists,
        /// because the daemon cannot tell and says dirty. Emitted instead of
        /// a failure: this is a question for the user, and a forced destroy
        /// is the answer that goes ahead.
        #[qsignal]
        fn workspace_destroy_refused(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            dirty: bool,
            unmerged: bool,
        );

        /// A `restart_workspace` call failed. `kind` is a
        /// [`RestartFailure`](super::RestartFailure) code: the daemon's own
        /// reason (the workspace is now `Error` with `message`), a refusal
        /// that left the workspace as it was, a request that timed out with
        /// the connection still up, or no connection at all. Only the first is
        /// news about the workspace; the others are about the request, and the
        /// daemon may well have done the restart. Emitted alongside
        /// `operation_failed`, in one step.
        #[qsignal]
        fn workspace_restart_failed(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            message: QString,
            kind: i32,
        );

        /// A `restart_workspace` call succeeded: `info_json` is the
        /// `WorkspaceInfo` the daemon answered with, its sandbox running again.
        /// Separate from `workspace_changed` because the window does one thing
        /// more for it -- the user asked for the workspace back, so its agent
        /// is started again too.
        #[qsignal]
        fn workspace_restarted(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            info_json: QString,
        );

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

        /// The daemon warned about one workspace and said more than one line
        /// about it: `message` is the sentence and, under it, the detail.
        ///
        /// Emitted for the shape rather than for any particular warning. The
        /// daemon stops an in-place sandbox whose protected git files were
        /// replaced with exactly this pair -- the sentence the workspace's
        /// `Error` state will carry, and a diff of what changed -- and the
        /// model keeps the detail against the workspace so its banner can
        /// offer it. A one-line warning carries nothing to keep and is only
        /// logged.
        #[qsignal]
        fn workspace_warned(self: Pin<&mut AppController>, workspace_id: QString, message: QString);

        /// An asynchronous operation failed. `op` is the daemon method name.
        ///
        /// The catch-all, and the one a consumer should reach for last. A
        /// window that has to tell a repository inspection from a prerequisite
        /// check from a workspace operation by comparing `op` against a list of
        /// method-name strings is one daemon rename away from putting a modal
        /// box over a dialog that already reported the same failure inline, so
        /// the three failures that are routed rather than shown each have a
        /// signal of their own below. Those are emitted **alongside** this one,
        /// not instead of it.
        ///
        /// Both halves of such a pair go out from inside **one** queued
        /// closure, and that is a contract with the window rather than a
        /// convenience. `MainWindow::noteFailureRouted` records a routed
        /// failure so the `operationFailed` behind it does not report the same
        /// thing twice, and expires the record at the end of the turn of the
        /// event loop that delivered it. Splitting a reporter into two queued
        /// closures would let that expiry fire between the two signals, and the
        /// failure would be reported twice.
        #[qsignal]
        fn operation_failed(self: Pin<&mut AppController>, op: QString, message: QString);

        /// A `system.check_prereqs` did not answer. Never a box: the commonest
        /// way to see it is closing Settings during a reconnect, the status bar
        /// is already saying the connection is down, and the check is re-run on
        /// every reconnect.
        #[qsignal]
        fn prereqs_check_failed(self: Pin<&mut AppController>, message: QString);

        /// A `repo.inspect` failed, naming the path it was asked about. The
        /// New Agent dialog reports its own inspections in place, and `path`
        /// is what lets a consumer tell the inspection it is showing from one
        /// for a repository the user has since moved off.
        #[qsignal]
        fn repo_inspect_failed(self: Pin<&mut AppController>, path: QString, message: QString);

        /// An operation belonging to one workspace failed, naming the
        /// workspace and the daemon method. For the failures whose home is a
        /// workspace's own pane rather than a box over the window;
        /// `workspaceOperationFailed` is the richer form, carrying the error's
        /// `data` for a banner that renders it.
        #[qsignal]
        fn workspace_op_failed(
            self: Pin<&mut AppController>,
            workspace_id: QString,
            op: QString,
            message: QString,
        );

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

        /// Create a workspace. Answers with `workspaceCreated` or
        /// `operationFailed`; `group`, `adapter`, `command` and `run_config`
        /// are not sent to the daemon, only echoed back to the caller so the
        /// window can place the tab and open the Run panel on the configuration
        /// the user picked.
        ///
        /// `init_if_missing` lets the daemon create and initialise the folder
        /// when it is not a git repository. It is the New Agent dialog's
        /// answer, never a default: the dialog only sets it once it has told
        /// the user, in the sentence under the path, that the folder is about
        /// to become a repository.
        ///
        /// `in_place` works directly in the checkout; `base_branch` is then
        /// ignored.
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
            init_if_missing: bool,
            in_place: bool,
        );

        /// Create a workspace and start a Claude agent in it. Answers with
        /// `workspaceCreated` (adapter "claude"), then `agentStarted`, then
        /// sends `initial_prompt` if it is not empty; any step can answer with
        /// `operationFailed` instead. `options_json` is an `AgentStartOptions`
        /// object without the API key, which the controller merges in, and is
        /// echoed back so the tab can start the agent again with it.
        ///
        /// `run_config` and `init_if_missing` mean what they do on
        /// `createWorkspaceWithRun`.
        ///
        /// `in_place` works directly in the checkout; `base_branch` is then
        /// ignored.
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
            init_if_missing: bool,
            in_place: bool,
        );

        /// Start a Claude agent in an existing workspace. Answers with
        /// `agent_started` or `operation_failed("agent.start", ...)`.
        #[qinvokable]
        fn start_agent(self: Pin<&mut AppController>, workspace_id: QString, options_json: QString);

        /// Destroy a workspace. Answers with `workspace_destroyed` or
        /// `operation_failed`.
        #[qinvokable]
        fn destroy_workspace(self: Pin<&mut AppController>, id: QString, force: bool);

        /// Bring a workspace whose sandbox is down, or which failed to start,
        /// back up (`workspace.restart`). Answers with `workspace_restarted`,
        /// or with `workspace_restart_failed` saying what kind of failure it
        /// was.
        #[qinvokable]
        fn restart_workspace(self: Pin<&mut AppController>, workspace_id: QString);

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

        /// Empty when `name` is one `workspace.create` will accept, and
        /// otherwise the sentence to show under the field, worded by the rule
        /// itself and shown verbatim.
        ///
        /// The New Agent dialog calls it on every keystroke. The rule is
        /// `bondsymphonic_proto::workspace_name::validate`, the one both the
        /// IDE and the daemon ask, so the dialog cannot accept a name the
        /// create then refuses -- which is what a rule of the IDE's own used
        /// to do to `fix.the.bug`.
        #[qinvokable]
        fn validate_workspace_name(self: &AppController, name: QString) -> QString;

        /// The last `prereqsChecked` payload, or empty before the first check
        /// has answered.
        ///
        /// The setup page is built when the Settings dialog opens, which is
        /// long after the check it draws. Without this it would show an empty
        /// list until something happened to trigger another one.
        #[qinvokable]
        fn prereqs_json(self: &AppController) -> QString;

        /// Whether anything in the last prerequisite answer failed, blocking
        /// or not. What the status bar's "Set up…" link is shown on: Settings
        /// is a dialog the user closes, so after closing it there has to be a
        /// way back.
        #[qinvokable]
        fn prereqs_any_failed(self: &AppController) -> bool;

        /// Whether the last prerequisite answer should open Settings on Setup
        /// by itself. See [`super::SetupPrompt`]: the answer is no once the
        /// dialog has been shown during this run of blocking failures, which is
        /// what keeps a still-blocked machine from reopening it every time it
        /// is closed.
        #[qinvokable]
        fn should_auto_open_setup(self: &AppController) -> bool;

        /// Records that Settings has been opened. Call it from every open, not
        /// only the automatic one, and before the dialog runs.
        #[qinvokable]
        fn note_setup_shown(self: Pin<&mut AppController>);

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
        ///
        /// Takes `Pin<&mut>` because storing a key is one of the two ways
        /// `claudeLoggedIn` can become true, and the property is recomputed
        /// here rather than waiting for the next prerequisite check.
        #[qinvokable]
        fn set_api_key(self: Pin<&mut AppController>, key: QString) -> bool;

        /// Removes the stored API key. Removing one that is not there
        /// succeeds: the caller asked for there to be none, and there is none.
        /// Recomputes `claudeLoggedIn`, which may shut the composer again.
        #[qinvokable]
        fn clear_api_key(self: Pin<&mut AppController>) -> bool;

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

        /// The palette the user chose: `system`, `light` or `dark`.
        #[qinvokable]
        fn theme(self: &AppController) -> QString;

        /// Records the palette choice. A word this does not know is ignored,
        /// because the settings file is the only writer and an unknown one
        /// could only come from a hand edit.
        #[qinvokable]
        fn set_theme(self: &AppController, theme: QString);

        /// Whether transcripts show the turn cost and the agent's own system
        /// lines.
        #[qinvokable]
        fn show_agent_meta(self: &AppController) -> bool;

        /// Records whether those lines are shown.
        #[qinvokable]
        fn set_show_agent_meta(self: &AppController, show: bool);

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

        /// Whether the New Agent dialog should open on "Work directly in this
        /// checkout".
        #[qinvokable]
        fn new_agent_in_place(self: &AppController) -> bool;

        /// Records the dialog's choice for next time.
        #[qinvokable]
        fn set_new_agent_in_place(self: &AppController, in_place: bool);

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

/// The prerequisite that says Claude Code can run without asking anyone to log
/// in. `claude --version` working is `claude`; this is the token behind it.
pub const CLAUDE_AUTH_PREREQ: &str = "claude_auth";

/// The daemon's half of the composer gate: whether its `claude_auth`
/// prerequisite passed.
///
/// Read here, once, where the daemon's answer is decoded, and kept on the
/// controller afterwards -- the list itself is not held in a form anything can
/// re-examine, and re-parsing the JSON to ask again is what AL4 removed.
///
/// An item that is not in the list at all counts as not logged in. A daemon
/// that never reported it has told the IDE nothing, and a shut gate with a
/// button on it costs one click, where an open one costs a lost prompt.
pub fn claude_auth_ok(items: &[PrereqStatus]) -> bool {
    items
        .iter()
        .any(|item| item.name == CLAUDE_AUTH_PREREQ && item.ok)
}

/// Whether a Claude agent can answer a prompt: [`claude_auth_ok`], **or** this
/// IDE has an Anthropic API key stored.
///
/// The gate on the chat composer, and it takes two inputs because there are two
/// ways to run Claude Code. An agent runs `claude -p`, which cannot log in --
/// typing `/login` into it answers "login is not available in this
/// environment" -- so a composer offered with neither credential can only
/// produce that sentence. The login itself happens in the setup terminal,
/// which is a PTY and can.
///
/// The key half is not optional. `claude_auth` is the daemon's own answer,
/// derived from `claude auth status`, the daemon user's credentials file and
/// the *daemon's* environment; it knows nothing about the key this IDE keeps in
/// the Windows credential store and merges into `AgentStartOptions.api_key`.
/// Without the second term, a user whose only credential is that key would be
/// shut out of every composer while their agents ran perfectly, and sent to a
/// dialog whose next section tells them the key is what to use instead of a
/// login.
///
/// Two arguments rather than the prerequisite list, because the two halves are
/// not decided at the same moment: the daemon's arrives with a check, and the
/// key can be stored or removed between two of them with no daemon round trip
/// at all. [`AppController::refresh_claude_logged_in`] is what puts them
/// together, and it is the only caller.
pub fn claude_gate_open(claude_auth_ok: bool, api_key_set: bool) -> bool {
    api_key_set || claude_auth_ok
}

/// The phrases the Claude Code CLI uses when it cannot authenticate, as they
/// appear in an agent's exit detail or in the detail of a turn that errored. Matched case-insensitively, as a small
/// explicit list: a rule that guessed from "auth" or "token" would also match
/// an agent that died of a network error while *using* a token, and the wrong
/// verdict here shuts every composer in the window.
const CLI_AUTH_FAILURES: [&str; 5] = [
    "oauth session expired",
    "could not be refreshed",
    "failed to authenticate",
    "not logged in",
    "invalid_grant",
];

/// What the setup page's `claude_auth` row says to do about an override, as
/// its `fix_hint`. A plain login does not always help: the CLI reads the
/// credentials file, finds the dead token there, and tries to refresh it again.
///
/// A new long-lived token comes first because it is the remedy that holds:
/// agents use it in preference to the credentials file, and it has no refresh
/// to race. Logging out and in stays as the alternative, but for a user who
/// already has a long-lived token it is the worse one -- the logout removes
/// that token and puts agents back on the refreshing login.
pub const CLAUDE_AUTH_OVERRIDE_FIX: &str =
    "Settings > Setup: Set up long-lived token, or Log out of Claude Code, then Log in to Claude Code again";

/// The CLI's own sentence about a login that has gone, if an agent's exit
/// detail -- or the detail of a turn that errored -- carries one; `None` for
/// every other detail.
///
/// The daemon's `claude_auth` prerequisite runs `claude auth status`, which only
/// reads `~/.claude/.credentials.json` and answers `loggedIn: true` for a file
/// whose refresh token the server has since revoked. The one thing that finds
/// out is the CLI itself, at start, and it says so exactly once -- in the exit
/// detail of the agent it killed: `Failed to authenticate: OAuth session
/// expired and could not be refreshed`. Before this rule, that sentence went
/// into a banner, the re-check the exit triggered came back green, the composer
/// stayed open, and the next prompt started another agent that died the same
/// way.
///
/// A token the server refuses once the agent is running -- a revoked one, or a
/// long-lived one gone bad -- does not kill the CLI at all: in the adapter's
/// stream-json mode it answers the turn with an error, `Failed to authenticate.
/// API Error: 401 OAuth access token is invalid.`, and waits for the next
/// message. The daemon puts that sentence in the agent's `error` state detail,
/// and it is read here the same way.
///
/// Returns the line of the detail that matched, without the daemon's own
/// `(exit code N)` suffix: it is shown in place of the gate's paraphrase, and
/// the user should read what the CLI said.
pub fn auth_failure_in(detail: &str) -> Option<String> {
    let line = detail.lines().map(str::trim).find(|line| {
        let lower = line.to_lowercase();
        CLI_AUTH_FAILURES
            .iter()
            .any(|phrase| lower.contains(phrase))
    })?;
    let line = match line.rfind(" (exit code ") {
        Some(at) if line.ends_with(')') => &line[..at],
        _ => line,
    };
    Some(line.to_owned())
}

/// `items` with its `claude_auth` entry failed by the CLI's own account: the
/// list as the setup page should draw it while an override is set.
///
/// The one place the rewrite happens. The page draws a cross and the **Log in**
/// button from `ok: false` on this row, so rewriting the payload is what lets
/// it do that without learning anything about overrides; the row's `detail` is
/// the CLI's sentence and its `fix_hint` is [`CLAUDE_AUTH_OVERRIDE_FIX`]. A list
/// with no such row -- an agent that died before the first check answered --
/// gets one appended, and a payload that does not parse is treated as empty
/// for the same reason: the cross must be there whatever the daemon managed to
/// say.
pub fn override_claude_auth(items_json: &str, sentence: &str) -> String {
    let mut items: Vec<PrereqStatus> = serde_json::from_str(items_json).unwrap_or_default();
    let row = PrereqStatus {
        name: CLAUDE_AUTH_PREREQ.to_owned(),
        ok: false,
        detail: sentence.to_owned(),
        fix_hint: Some(CLAUDE_AUTH_OVERRIDE_FIX.to_owned()),
    };
    match items
        .iter_mut()
        .find(|item| item.name == CLAUDE_AUTH_PREREQ)
    {
        Some(item) => *item = row,
        None => items.push(row),
    }
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
}

/// Whether a prerequisite answer should open the Settings dialog on Setup by
/// itself.
///
/// `blocked` is [`prereqs_blocking`]; `already_shown` is whether the dialog has
/// already been opened during the current run of failures. The second argument
/// is the whole point. Closing Settings re-checks the prerequisites, so on a
/// machine where a blocking one is still failing -- a first run with no
/// bubblewrap, which is exactly what the setup flow exists for -- an
/// unconditional auto-open makes the dialog impossible to close: close it,
/// the re-check answers "still blocked", and it opens again.
///
/// Once per run of failures is the rule. The caller sets `already_shown` when
/// it opens the dialog and clears it the moment `blocked` goes false, so a
/// prerequisite that breaks again later opens it again. The status-bar
/// "Set up…" link and File > Settings remain the way in meanwhile, and a
/// blocking failure that the user has decided to live with no longer traps
/// them: closing the dialog is what "carry on regardless" now means.
pub fn should_auto_open_setup(blocked: bool, already_shown: bool) -> bool {
    blocked && !already_shown
}

/// The auto-open decision as one value: what the last prerequisite answer said,
/// and whether Settings has been shown since that run of failures began.
///
/// It used to be three things. `MainWindow` held the "already shown" flag,
/// cleared it on any answer that did not block, and passed it back into
/// [`should_auto_open_setup`] beside a `blocked` it had asked the controller
/// for separately -- which meant re-parsing the prerequisite JSON the
/// controller had just parsed, and keeping half a state machine in a file no
/// test can reach. The rule above stays pure and keeps its own test; this is
/// the state it is applied to, and the window is left asking one question.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SetupPrompt {
    /// Whether the last answer failed a prerequisite the IDE cannot work
    /// around. False before the first answer, which is why a fresh prompt
    /// opens nothing.
    blocked: bool,
    /// Whether Settings has been opened during the current run of blocking
    /// failures, by the rule or by the user.
    shown: bool,
}

impl SetupPrompt {
    /// Records a prerequisite answer. An answer that does not block re-arms
    /// the rule, so a prerequisite that breaks again later is a new run of
    /// failures and gets a dialog of its own.
    pub fn checked(&mut self, blocked: bool) {
        self.blocked = blocked;
        if !blocked {
            self.shown = false;
        }
    }

    /// Records that Settings was shown. Called for every open and not only the
    /// automatic one: a user who reaches Settings by hand while a blocking
    /// prerequisite is failing must not have it thrown back at them the moment
    /// they close it either.
    pub fn note_shown(&mut self) {
        self.shown = true;
    }

    /// Whether this answer should open Settings on Setup by itself.
    pub fn should_open(self) -> bool {
        should_auto_open_setup(self.blocked, self.shown)
    }
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

/// The seven setup terminals, by the names the UI and the daemon both use.
/// Anything else is refused here rather than sent on: the enum is the whole
/// point of `system.setup_pty`, and a typo should fail loudly and locally.
fn parse_setup_action(action: &str) -> Option<SetupAction> {
    match action {
        "claude_login" => Some(SetupAction::ClaudeLogin),
        "gh_login" => Some(SetupAction::GhLogin),
        "install_claude" => Some(SetupAction::InstallClaude),
        "install_gh" => Some(SetupAction::InstallGh),
        // The two that undo a sign-in rather than make one, offered on a
        // prerequisite that already passes. Nothing else about them is
        // different: the same host terminal, and the same re-check when it
        // exits, which is what turns the row's tick back into a cross.
        "claude_logout" => Some(SetupAction::ClaudeLogout),
        "gh_logout" => Some(SetupAction::GhLogout),
        // Also a host terminal on `claude_auth`, offered whether the row
        // passes or fails: `claude setup-token` neither needs nor disturbs an
        // existing login.
        "claude_setup_token" => Some(SetupAction::ClaudeSetupToken),
        _ => None,
    }
}

/// What the connection state becomes the instant a connection ends, before
/// anything slow is done about it.
///
/// Published in the same queued step that drops the dead client rather than
/// after the old daemon has been reaped. `shutdown` closes the daemon's stdin
/// and waits for the process to go, which takes as long as the daemon takes to
/// exit, and for every millisecond of that window [`require_connection`] is
/// already failing: a status bar still reading "daemon: connected" over it is
/// the IDE claiming a connection each of its own operations is refusing.
///
/// `next` is the attempt the loop is about to make, so the number the user is
/// watching does not jump when the backoff starts counting it.
/// Drops the client and publishes `losing`, in one step on the Qt thread.
///
/// One function because it is one invariant, and two statements side by side
/// are how an invariant stops being one. The property must never read
/// "connected" while the handle every operation reaches for is already gone:
/// [`on_connection_lost`] has emptied the shared slot by the time this is
/// called, so [`require_connection`] is already answering every invokable with
/// "daemon connection lost". Split across two `queue` calls, the Qt thread
/// could run the first and paint a frame before the second arrived.
///
/// Called before the old daemon is reaped, not after. `shutdown` closes the
/// daemon's stdin and waits for the process to exit, which takes as long as the
/// daemon takes to go, and a status bar still reading "daemon: connected" for
/// all of that is the IDE claiming a connection it is itself refusing.
fn publish_connection_loss(qt: &QtHandle, losing: ConnectionState) {
    let _ = qt.queue(move |mut q| {
        q.as_mut().rust_mut().client = None;
        // The answer went with the connection: the reconnect runs its own
        // check, and until that answers a composer gate must say it is
        // checking rather than claim an answer nobody has given yet. The
        // answer's contents stay -- the gate and the setup page keep drawing
        // the last thing the daemon said.
        q.as_mut().set_prereqs_answered(false);
        // The CLI's verdict went with it too. The reconnect's own check starts
        // from nothing, and an override held across a daemon restart would
        // shut the composer over a login the user may have renewed meanwhile.
        q.as_mut().set_claude_auth_override(None);
        q.set_state(losing);
    });
}

pub fn state_after_loss(quitting: bool, fatal: bool, next: u32) -> ConnectionState {
    match (quitting, fatal) {
        // The IDE is on its way out: nothing is reconnected, and the bar says
        // the connection is gone rather than that something is being tried.
        (true, _) => ConnectionState::Lost,
        // Nothing a relaunch can get past. The sentence explaining it follows
        // once the old daemon has been reaped.
        (false, true) => ConnectionState::Error,
        (false, false) => ConnectionState::Reconnecting { attempt: next },
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
    /// Whether the daemon's last prerequisite check said Claude Code is logged
    /// in. Backs the `claudeLoggedIn` property; see [`claude_gate_open`] for
    /// what the transcript panes do with it.
    claude_logged_in: bool,
    /// Whether the live connection's prerequisite check has answered. Backs
    /// the `prereqsAnswered` property; see there.
    prereqs_answered: bool,
    /// The last `prereqs_checked` payload, so a setup page built later can
    /// draw its rows without waiting for another check. See `prereqs_json`.
    prereqs_json: QString,
    /// Whether anything in that payload failed, blocking or not. Decided where
    /// the list is parsed rather than parsed again in C++ to be counted; it is
    /// what the status bar's "Set up…" link is shown on.
    prereqs_any_failed: bool,
    /// The daemon's half of [`claude_gate_open`], from the same answer. Kept so
    /// a key stored or removed later recomputes the gate without re-reading
    /// the list.
    claude_auth_ok: bool,
    /// The CLI's own sentence about a login that has gone, overriding
    /// `claude_auth_ok` while it is set. Backs the `claudeAuthFailure`
    /// property; see [`auth_failure_in`] for the sentence.
    ///
    /// The rule, because the daemon cannot apply it: the daemon's check reads
    /// the credentials file and cannot see that the refresh token in it is
    /// dead. Only the CLI can, and it says so exactly once, at exit. So:
    ///
    /// * **Set** when an agent exits, or ends a turn in error, with a detail
    ///   [`auth_failure_in`] recognises. From that moment `claudeLoggedIn` is false and the setup
    ///   page's `claude_auth` row is a cross, whatever the daemon answers.
    /// * **Cleared** when the Claude login, logout, or setup-token terminal
    ///   opened through `open_setup_pty` exits -- the login is the fix, and
    ///   the re-check the setup page sends on that exit then decides the gate
    ///   on its own -- and on connection loss, where the reconnect's check
    ///   starts from nothing.
    /// * **Not cleared** by a re-check on its own. That is the case that lied:
    ///   the window re-checks on every agent exit, and the check came back
    ///   green over the very login the agent had just died of.
    claude_auth_override: Option<String>,
    /// The same sentence as the `claudeAuthFailure` property reads it: the
    /// override, or empty. Kept beside it rather than instead of it because the
    /// property is a `QString` and the rule above is written in `Option`.
    claude_auth_failure: QString,
    /// The PTY of the last Claude login, logout, or setup-token terminal
    /// `open_setup_pty` opened, so its `pty.exit` is recognised as the login
    /// having been done. Only those three: an `install_claude` or a GitHub
    /// login says nothing about whether the Claude token is alive.
    login_pty: Option<PtyId>,
    /// Whether a prerequisite answer should open Settings on Setup, and
    /// whether one already has. See [`SetupPrompt`].
    setup_prompt: SetupPrompt,
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
            // Shut until a check says otherwise: the composer must not be open
            // in the seconds before the first `system.check_prereqs` answers.
            claude_logged_in: false,
            prereqs_answered: false,
            prereqs_json: QString::from(""),
            prereqs_any_failed: false,
            claude_auth_ok: false,
            claude_auth_override: None,
            claude_auth_failure: QString::from(""),
            login_pty: None,
            setup_prompt: SetupPrompt::default(),
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

/// What `workspaceCreated` carries back to the window, verbatim.
///
/// None of it is sent to the daemon. The controller keeps no tab state, so the
/// choices the New Agent dialog collected -- the group to file the tab under,
/// the adapter and command for a terminal tab, the agent options for a Claude
/// one, the run configuration the Run panel should open on -- travel out with
/// the request and come back with its answer.
struct CreateEcho {
    group: String,
    adapter: String,
    command: String,
    options_json: String,
    run_config: String,
}

/// Queues an `operation_failed` for `op` back onto the Qt thread.
fn report_failure(qt: &QtHandle, op: &'static str, message: String) {
    tracing::warn!("{op} failed: {message}");
    let _ = qt.queue(move |q| q.operation_failed(QString::from(op), QString::from(&message)));
}

/// The same for a prerequisite check, which also gets its own signal so a
/// consumer does not have to recognise it by the method name.
///
/// One queue call, not two: see the note on the `operation_failed` signal for
/// why a typed failure and the `operationFailed` behind it have to reach the
/// window in the same turn of its event loop.
fn report_prereqs_failure(qt: &QtHandle, message: String) {
    tracing::warn!("system.check_prereqs failed: {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().prereqs_check_failed(QString::from(&message));
        q.operation_failed(
            QString::from("system.check_prereqs"),
            QString::from(&message),
        )
    });
}

/// The same for a repository inspection, which carries the path it was asked
/// about: a dialog showing one repository must not report a failure for
/// another it has since moved off.
///
/// One queue call, as above.
fn report_inspect_failure(qt: &QtHandle, path: String, message: String) {
    tracing::warn!("repo.inspect failed for {path}: {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut()
            .repo_inspect_failed(QString::from(&path), QString::from(&message));
        q.operation_failed(QString::from("repo.inspect"), QString::from(&message))
    });
}

/// The same for a failure that belongs to one workspace but is reported as a
/// plain `operationFailed` today: both go out, so a consumer can move onto the
/// typed one without the other changing under it.
///
/// One queue call, as above.
fn report_workspace_op_failure(
    qt: &QtHandle,
    workspace: String,
    op: &'static str,
    message: String,
) {
    tracing::warn!("{op} failed for {workspace}: {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().workspace_op_failed(
            QString::from(&workspace),
            QString::from(op),
            QString::from(&message),
        );
        q.operation_failed(QString::from(op), QString::from(&message))
    });
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

/// What kind of failure a `workspace.restart` ended in, which decides what the
/// window may conclude about the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartFailure {
    /// The daemon tried and failed; the workspace is `Error(message)`.
    Reason = 0,
    /// The daemon would not try -- the workspace is being created or
    /// destroyed -- and left it as it was.
    Refused = 1,
    /// No answer in time, on a connection that is still up. The restart may
    /// have happened.
    Timeout = 2,
    /// No connection, or it went while waiting. The restart may have happened.
    NoConnection = 3,
}

impl RestartFailure {
    pub fn of(e: &ClientError) -> RestartFailure {
        match e {
            ClientError::Rpc(rpc) if rpc.code == ErrorCode::InvalidParams => Self::Refused,
            ClientError::Rpc(_) => Self::Reason,
            ClientError::Timeout => Self::Timeout,
            _ => Self::NoConnection,
        }
    }
}

/// Queues `workspace_restart_failed` and the `operation_failed` behind it, in
/// one closure: see the note on `operation_failed`.
fn report_restart_failure(qt: &QtHandle, workspace: String, message: String, kind: RestartFailure) {
    tracing::warn!("workspace.restart failed for {workspace} ({kind:?}): {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().workspace_restart_failed(
            QString::from(&workspace),
            QString::from(&message),
            kind as i32,
        );
        q.operation_failed(QString::from("workspace.restart"), QString::from(&message))
    });
}

/// Whether `e` is the daemon refusing an unforced destroy for what it would
/// discard, as `(dirty, unmerged)`. `None` for every other failure, including
/// a `Conflict` that names neither.
pub fn destroy_refusal(e: &ClientError) -> Option<(bool, bool)> {
    let ClientError::Rpc(rpc) = e else {
        return None;
    };
    if rpc.code != ErrorCode::Conflict {
        return None;
    }
    let data = rpc.data.as_ref()?;
    let flag = |key: &str| data.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    let (dirty, unmerged) = (flag("dirty"), flag("unmerged"));
    (dirty || unmerged).then_some((dirty, unmerged))
}

/// A `destroy_workspace` that failed: the booking goes back and the failure is
/// reported on `operationFailed`, which is where a plain destroy's errors have
/// always gone (a discard's go to the workspace's own banner instead).
fn end_destroy(qt: &QtHandle, workspace: String, message: String) {
    tracing::warn!("workspace.destroy failed for {workspace}: {message}");
    let _ = qt.queue(move |mut q| {
        q.as_mut().end_workspace_op(&workspace);
        q.as_mut().workspace_op_failed(
            QString::from(&workspace),
            QString::from("workspace.destroy"),
            QString::from(&message),
        );
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
                // Delivered and not buffered here. A state that names an
                // agent no tab is running yet is held by `GroupModel`, which is
                // where a tab learns its agent; see `group_model::
                // HeldAgentStates`.
                let _ = qt.queue(move |mut q| {
                    // Before the signal, so the window's handler -- which
                    // re-checks the prerequisites on every exit -- finds the
                    // gate already shut when it runs, and so does the pane
                    // that draws the banner. An errored turn counts as well
                    // as an exit: a token the server refuses leaves the CLI
                    // running, and the turn's error is all it says.
                    if matches!(state, AgentState::Exited | AgentState::Error) {
                        if let Some(sentence) = auth_failure_in(&detail) {
                            q.as_mut().set_claude_auth_override(Some(sentence));
                        }
                    }
                    q.agent_state_changed(
                        QString::from(&id),
                        QString::from(word),
                        QString::from(&detail),
                    )
                });
            }
            // A setup terminal ending. Only the Claude login, logout, or
            // setup-token one means anything here: its exit is the login
            // having been done, which is what the CLI's verdict about the old
            // one yields to.
            Event::PtyExit { pty_id, .. } => {
                let _ = qt.queue(move |mut q| {
                    if q.as_ref().rust().login_pty.as_ref() == Some(&pty_id) {
                        q.as_mut().rust_mut().login_pty = None;
                        q.as_mut().set_claude_auth_override(None);
                    }
                });
            }
            Event::DaemonLog { level, message, .. } => {
                // At the daemon's own level, not always at `info`: a warning
                // the daemon thought worth raising is a warning here too, and a
                // console filtered to warnings used to show none of them.
                match level {
                    LogLevel::Error => tracing::error!("{message}"),
                    LogLevel::Warn => tracing::warn!("{message}"),
                    LogLevel::Info => tracing::info!("{message}"),
                    LogLevel::Debug => tracing::debug!("{message}"),
                }
                // A warning about one workspace with more than one line in it
                // is kept by the model against that workspace; see
                // `workspace_warned`. Recognised by its shape, so nothing here
                // has to know which warning it is.
                if level == LogLevel::Warn && message.contains('\n') {
                    if let Some(id) = ws.map(|id| id.to_string()) {
                        let _ = qt.queue(move |q| {
                            q.workspace_warned(QString::from(&id), QString::from(&message))
                        });
                    }
                }
            }
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

/// How long to wait before the single `system.check_prereqs` retry.
///
/// A daemon that has just come up can be a moment away from being able to
/// answer: the check shells out to `git`, `bwrap`, `claude` and `gh`, and a
/// distro still starting its services can refuse the first one. A check that
/// failed used to stay failed for the session -- the setup page stayed empty,
/// `claudeLoggedIn` stayed false, and every Claude composer stayed shut behind
/// a "Log in to Claude Code…" button -- until the user found a "Re-check" they
/// had no reason to look for. Long enough to be worth waiting out, short enough
/// that the page fills before the user has finished reading it.
const PREREQ_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Runs `system.check_prereqs`, and runs it once more after
/// [`PREREQ_RETRY_DELAY`] if the daemon could not answer.
///
/// One retry, not a loop. A daemon that cannot answer twice, five seconds
/// apart, is not warming up, and the ordinary re-check points take it from
/// there: a setup terminal exiting, the "Re-check" button, closing Settings,
/// and the next connection.
async fn check_prereqs(client: DaemonClient, qt: QtHandle) {
    if run_prereq_check(&client, &qt).await {
        return;
    }
    // On its own task: a reconnect awaits this call before announcing itself,
    // and holding the whole re-sync for five seconds over a check nothing else
    // depends on would leave the window's banner up for no reason.
    let generation = connection_generation();
    runtime().spawn(async move {
        tokio::time::sleep(PREREQ_RETRY_DELAY).await;
        // A connection that has since been replaced has already run its own
        // check, and an IDE on its way out has no setup page to fill.
        if quitting() || connection_generation() != generation {
            return;
        }
        tracing::info!("re-running the prerequisite check the daemon could not answer");
        run_prereq_check(&client, &qt).await;
    });
}

/// One `system.check_prereqs`, reporting the answer twice: the whole list as
/// `prereqs_checked`, which is what the setup page draws, and the failures as
/// one sentence in `prereq_warning`, which is what the status bar shows.
///
/// Both are queued from the same closure, `prereqs_checked` first, so the
/// window has already decided which of the two views it is in by the time the
/// warning text reaches it.
///
/// Answers whether the daemon answered at all, which is what decides whether
/// the caller retries. A list full of failing prerequisites is an answer.
async fn run_prereq_check(client: &DaemonClient, qt: &QtHandle) -> bool {
    let items = match client
        .request::<CheckPrereqsResult>(Request::SystemCheckPrereqs {})
        .await
    {
        Ok(res) => res.items,
        Err(e) => {
            report_prereqs_failure(qt, e.to_string());
            return false;
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
    // Every question anyone downstream asks of this list is answered here,
    // where it is already decoded: the window used to parse it again to count
    // the failures and the controller a third time to classify them. The
    // credential store is deliberately *not* read here -- it is read where the
    // gate is composed, so a key stored between two checks still counts -- but
    // the daemon's half of that answer comes out of this list.
    let answer = PrereqAnswer {
        json,
        any_failed: !failures.is_empty(),
        blocked: prereqs_blocking(&items),
        claude_auth_ok: claude_auth_ok(&items),
    };
    let _ = qt.queue(move |mut q| {
        q.as_mut().apply_prereqs(answer);
        if !failures.is_empty() {
            q.prereq_warning(QString::from(&failures.join("\n")));
        }
    });
    true
}

/// One `system.list_models`, computing
/// [`crate::model::models::newest_per_family`] and reporting it as
/// `models_checked`. Never awaited by its callers, which spawn it instead:
/// see the call sites for why.
///
/// Any error -- no Claude credentials on the daemon side, the network, a
/// daemon too old to know the method -- is a warning, never a hard failure:
/// `agentchoices`'s C++ fallback list is what the dropdowns already show, and
/// there is nothing here worth retrying the way [`check_prereqs`] retries,
/// because the daemon answer is cached for an hour and the next connection
/// tries again regardless.
async fn fetch_models(client: DaemonClient, qt: QtHandle) {
    let params = ListModelsParams {
        api_key: api_key_for_start(),
    };
    let models = match client
        .request::<ListModelsResult>(Request::SystemListModels(params))
        .await
    {
        Ok(res) => res.models,
        Err(e) => {
            tracing::warn!("system.list_models failed: {e}");
            return;
        }
    };
    let choices = crate::model::models::model_choices(&models);
    if choices.is_empty() {
        // Every id failed to start with `claude-`, or the daemon answered
        // with no models at all: nothing here is worth showing over the
        // fallback list.
        return;
    }
    let Ok(json) = serde_json::to_string(&choices) else {
        return;
    };
    tracing::info!(
        families = choices.len(),
        "fetched the newest model per family"
    );
    let _ = qt.queue(move |mut q| q.as_mut().models_checked(QString::from(&json)));
}

/// One prerequisite check, as the answers the UI actually asks for.
///
/// The list crosses the boundary as JSON because the setup page draws a row per
/// entry, but nothing else parses it: the three booleans are what the status
/// bar, the composer gate and the auto-open rule each wanted, and they are
/// decided once, here, from the decoded reply.
struct PrereqAnswer {
    /// The list itself, as the setup page draws it.
    json: String,
    /// Whether anything failed, blocking or not.
    any_failed: bool,
    /// Whether any of what failed is one the IDE cannot work around.
    blocked: bool,
    /// The daemon's half of [`claude_gate_open`]; the key half is read from the
    /// credential store whenever the gate is recomposed.
    claude_auth_ok: bool,
}

/// Parses the options a dialog built and merges the stored API key in. An
/// unparseable string is not fatal: the daemon's defaults are a working agent,
/// which is a better answer than refusing to start one.
fn start_options(options_json: &str) -> AgentStartOptions {
    start_options_with_default(options_json, &Settings::load().default_permission_mode)
}

/// [`start_options`] with the user's default permission mode passed in.
///
/// A start that names no mode gets the default rather than the CLI's own,
/// which asks before every tool. A tab restored onto an agent the daemon
/// remembers as started without one -- anything started before the IDE sent a
/// mode, and so every restart and automatic resume of it since -- used to go
/// on asking for approvals whatever the user had chosen in Settings.
fn start_options_with_default(options_json: &str, default_mode: &str) -> AgentStartOptions {
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
    if options.permission_mode.as_deref().is_none_or(str::is_empty) && !default_mode.is_empty() {
        options.permission_mode = Some(default_mode.to_owned());
    }
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
            report_workspace_op_failure(&qt, workspace.to_string(), "agent.start", e.to_string());
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
        report_workspace_op_failure(&qt, workspace.to_string(), "agent.send", e.to_string());
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
        let ended = tokio::select! {
            _ = &mut drain => ConnectFailure::Retryable(
                "the daemon's event stream ended".to_owned(),
            ),
            verdict = wait_for_process(&process) => {
                // The stream has not ended yet, but nothing will answer on it.
                drain.abort();
                verdict
            }
        };
        // A daemon that exited because its data directory belongs to another
        // daemon is the one ending no relaunch can improve on.
        let fatal = matches!(ended, ConnectFailure::Fatal(_));
        let reason = ended.into_message();
        // How long the connection lasted is what decides whether the schedule
        // starts over. A daemon that answers `hello` and dies a second later is
        // crash-looping, not recovering, and must not be able to hold the
        // backoff at one second for the rest of the session.
        //
        // Worked out here rather than at the bottom of the loop because the
        // status published below names the attempt this produces.
        let held = connected_at.elapsed();
        let next = next_attempt(attempt, held);

        // The dead client goes first: an operation attempted in the gap then
        // fails at once, with a reason, instead of issuing a request nobody
        // will answer.
        on_connection_lost();
        publish_connection_loss(&qt, state_after_loss(quitting(), fatal, next));
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
        if fatal {
            tracing::error!("{reason}");
            let _ = qt.queue(move |mut q| {
                q.as_mut().set_state(ConnectionState::Error);
                q.set_status_message(QString::from(reason.as_str()));
            });
            return;
        }
        tracing::warn!("{reason} after {held:?}");
        attempt = next;
    }
}

/// Resolves when the daemon process ends, and never when there is none.
///
/// Holding the lock across the await is deliberate. The only other thing that
/// takes it is [`supervise`], and only after this future has been dropped by
/// the `select!` that owned it, so the wait can never block the reaping that
/// follows it.
async fn wait_for_process(process: &ProcessHandle) -> ConnectFailure {
    let mut guard = process.lock().await;
    match guard.as_mut() {
        Some(daemon) => {
            let status = daemon.wait_exit().await;
            tracing::warn!("the daemon process ended: {status:?}");
            classify_daemon_exit(
                status.ok().and_then(|status| status.code()),
                "the daemon process exited",
            )
        }
        // The `BS_DAEMON_ADDR` test hook: there is no process of ours to
        // watch, so the event stream is the only signal and this parks.
        None => std::future::pending().await,
    }
}

/// What the status bar says when the daemon refused to start because another
/// one already owns its data directory.
///
/// The directory is named because the fix is outside the IDE: the user has to
/// find the other daemon, or point this one somewhere else. Nothing here can
/// do either.
fn busy_data_dir_status() -> String {
    format!(
        "daemon: another daemon owns {} \u{2014} stop it or pick another data dir",
        launcher::DAEMON_DATA_DIR
    )
}

/// What a daemon exit means for the reconnect loop, from the exit code alone.
/// `retry_reason` is the sentence to carry when another attempt is worth
/// making; it is the caller's, because "the process exited" and "the launch
/// failed" are read in different places.
///
/// [`launcher::DAEMON_BUSY_EXIT_CODE`] is the one exit no relaunch can get
/// past: the daemon found the data directory locked by another daemon and
/// stopped before touching it, and a replacement would find exactly the same
/// lock. Backing off against that would spend the rest of the session starting
/// daemons that immediately exit, behind a status bar that says "reconnecting".
/// Every other exit -- a crash, a killed `wsl.exe` relay, a clean shutdown --
/// is worth another launch.
fn classify_daemon_exit(code: Option<i32>, retry_reason: &str) -> ConnectFailure {
    if code == Some(launcher::DAEMON_BUSY_EXIT_CODE) {
        ConnectFailure::Fatal(busy_data_dir_status())
    } else {
        ConnectFailure::Retryable(retry_reason.to_owned())
    }
}

/// Why one connection attempt did not produce a connection.
///
/// The distinction is what the backoff loop reads: a daemon that is not up yet
/// is worth trying again, and a daemon speaking another version of the protocol
/// is not -- every attempt would be refused in exactly the same way, behind a
/// status bar claiming something was being tried.
#[derive(Debug)]
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
///
/// The hint is the second question. [`launcher::launch`] installs the daemon
/// this IDE ships with before every spawn, so a mismatch is never a stale copy
/// in the distro: the binary beside the IDE is itself from another build, and
/// the fix is to rebuild it or to reinstall the package it came from.
fn mismatch_status(daemon: u32, client: u32) -> String {
    format!(
        "daemon: protocol mismatch (daemon {daemon}, IDE {client}) \u{2014} rebuild the daemon \
         (scripts\\build-daemon.ps1) or reinstall the package"
    )
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
    let (addr, token) = match launcher::test_endpoint() {
        Some((addr, token)) => {
            if first {
                tracing::warn!(
                    "{} is set: connecting to {addr} instead of launching a daemon",
                    launcher::TEST_ADDR_ENV
                );
            }
            (addr, token)
        }
        None => {
            if first {
                let _ = qt.queue(|q| q.set_state(ConnectionState::Launching));
            }
            let daemon = launcher::launch(spec).await.map_err(|e| {
                // A daemon that exited during start-up carries its exit code
                // out with it, and code 2 -- the data directory is another
                // daemon's -- is not something a relaunch can get past.
                let code = e
                    .downcast_ref::<launcher::DaemonExited>()
                    .and_then(|exited| exited.code);
                classify_daemon_exit(code, &format!("daemon: launch failed: {e:#}"))
            })?;
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], daemon.port));
            let token = daemon.token.clone();
            *process.lock().await = Some(daemon);
            (addr, token)
        }
    };
    // Only on the first connection: a reconnect's status text is
    // "daemon: reconnecting (attempt N)", and overwriting it with "connecting"
    // would take away the count the user is watching.
    if first {
        let _ = qt.queue(|q| q.set_state(ConnectionState::Connecting));
    }

    let (client, hello, events) =
        match DaemonClient::connect(addr, &token, env!("CARGO_PKG_VERSION")).await {
            Ok(connected) => connected,
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
                // Nothing to try again and nothing to reinstall: `launcher::launch`
                // already installs the daemon this IDE ships with before every
                // spawn, so the copy in the distro is the copy beside the IDE and
                // running that step again would change nothing. The pair really
                // cannot talk, so this stops rather than backing off against a
                // refusal that would be identical every time.
                return Err(ConnectFailure::Fatal(mismatch_status(daemon, client)));
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
        // Alongside the prerequisite check, on its own task: a model list
        // nobody has asked to see yet must never hold up a connection the
        // rest of the window is already waiting on.
        runtime().spawn(fetch_models(client.clone(), qt.clone()));
        check_prereqs(client, qt.clone()).await;
    } else {
        // A reconnect re-syncs in order and announces itself only once that is
        // done, so anything listening for `reconnected` can take it that the
        // prerequisites and the workspace list have already been reported.
        // Awaited rather than spawned for exactly that reason.
        runtime().spawn(fetch_models(client.clone(), qt.clone()));
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

    /// The whole of a create, with or without an agent to start in it.
    ///
    /// The two invokables differ in two things: what `workspaceCreated` echoes
    /// back, and whether an agent follows. Everything else -- the name rule
    /// applied where the request is built and not only where a name is typed,
    /// the group and repository recorded in `state.json`, the tab raised as
    /// soon as the workspace exists rather than when the agent answers, and the
    /// one sentence every failure is reported with -- is the same, and was
    /// written out twice.
    fn create_workspace_inner(
        self: Pin<&mut Self>,
        params: WorkspaceCreateParams,
        echo: CreateEcho,
        agent: Option<(AgentStartOptions, String)>,
    ) {
        let qt = self.qt_thread();
        // The same rule the dialog greys Create out on, applied again where the
        // request is actually built. Not a duplicate check for its own sake:
        // this is the only guard on a caller that is not the dialog, and a name
        // stopped here fails with the sentence the user was already shown
        // rather than with the daemon's wording for the same thing.
        if let Err(hint) = workspace_name::validate(&params.name) {
            report_failure(&qt, "workspace.create", hint);
            return;
        }
        // Kept for `state.json`, which the echo does not reach.
        let (group_for_state, repo_for_state) = (echo.group.clone(), params.repo_path.clone());
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
            let id = info.id.clone();
            // The tab appears as soon as the workspace exists, so a slow
            // `agent.start` happens in front of the user rather than behind a
            // dialog that has not closed yet.
            let _ = qt.queue(move |q| {
                q.workspace_created(
                    QString::from(&json),
                    QString::from(&echo.group),
                    QString::from(&echo.adapter),
                    QString::from(&echo.command),
                    QString::from(&echo.options_json),
                    QString::from(&echo.run_config),
                )
            });
            if let Some((options, prompt)) = agent {
                start_agent_and_prompt(shared, qt, id, options, prompt).await;
            }
        });
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
        init_if_missing: bool,
        in_place: bool,
    ) {
        self.create_workspace_inner(
            create_params(
                repo_path.to_string(),
                base_branch.to_string(),
                name.to_string(),
                init_if_missing,
                in_place,
            ),
            CreateEcho {
                group: group.to_string(),
                adapter: adapter.to_string(),
                command: command.to_string(),
                options_json: String::new(),
                run_config: run_config.to_string(),
            },
            None,
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
        init_if_missing: bool,
        in_place: bool,
    ) {
        let options_json = options_json.to_string();
        self.create_workspace_inner(
            create_params(
                repo_path.to_string(),
                base_branch.to_string(),
                name.to_string(),
                init_if_missing,
                in_place,
            ),
            CreateEcho {
                group: group.to_string(),
                adapter: "claude".to_owned(),
                command: String::new(),
                // Echoed back to the window verbatim, so the tab keeps what the
                // user asked for and can start the agent again with it. The API
                // key is not in it: `start_options` merges the key into the
                // request it builds and never into this string.
                options_json: options_json.clone(),
                run_config: run_config.to_string(),
            },
            Some((start_options(&options_json), initial_prompt.to_string())),
        );
    }

    pub fn start_agent(self: Pin<&mut Self>, workspace_id: QString, options_json: QString) {
        let qt = self.qt_thread();
        let workspace = WorkspaceId(workspace_id.to_string());
        let options = start_options(&options_json.to_string());
        let shared = match require_connection() {
            Ok(shared) => shared,
            // Named, like the in-flight failure in `start_agent_and_prompt`:
            // the pane that is showing "starting the agent" is this
            // workspace's, and it is the one that has to stop saying so.
            Err(message) => {
                report_workspace_op_failure(
                    &qt,
                    workspace.to_string(),
                    "agent.start",
                    message.to_owned(),
                );
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
            // Typed, like this function's other two failures, so every
            // workspace-scoped failure says which workspace it belongs to.
            // Deliberately not `end_destroy`: a refusal must leave the booking
            // alone, because the booking belongs to the operation that is still
            // running. Whether the window shows this as a box or a banner is
            // its own decision -- the signal forces neither.
            report_workspace_op_failure(
                &qt,
                id.clone(),
                "workspace.destroy",
                WORKSPACE_BUSY.to_owned(),
            );
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
                Err(e) => match destroy_refusal(&e) {
                    Some((dirty, unmerged)) if !force => {
                        tracing::info!(
                            "workspace.destroy refused for {id}: dirty={dirty} unmerged={unmerged}"
                        );
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().end_workspace_op(&id);
                            q.workspace_destroy_refused(QString::from(&id), dirty, unmerged)
                        });
                    }
                    _ => end_destroy(&qt, id, e.to_string()),
                },
            }
        });
    }

    pub fn restart_workspace(self: Pin<&mut Self>, workspace_id: QString) {
        let qt = self.qt_thread();
        let id = workspace_id.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_restart_failure(&qt, id, message.to_owned(), RestartFailure::NoConnection);
                return;
            }
        };
        runtime().spawn(async move {
            let params = WorkspaceIdParams {
                workspace_id: WorkspaceId(id.clone()),
            };
            // The wait is the client's per-method one: a sandbox start is slow,
            // and giving up early would report a failure for a Retry that is
            // about to work.
            match shared
                .client
                .request::<WorkspaceInfo>(Request::WorkspaceRestart(params))
                .await
            {
                Ok(info) => {
                    let json = serde_json::to_string(&info).unwrap_or_default();
                    let _ = qt.queue(move |q| {
                        q.workspace_restarted(QString::from(&id), QString::from(&json))
                    });
                }
                // The message alone, not `failure_parts`: the daemon's reason
                // is a sentence written for the user, and it is what the banner
                // shows as the workspace's new reason.
                Err(e) => {
                    let kind = RestartFailure::of(&e);
                    let message = match e {
                        ClientError::Rpc(rpc) => rpc.message,
                        other => other.to_string(),
                    };
                    report_restart_failure(&qt, id, message, kind);
                }
            }
        });
    }

    pub fn inspect_repo(self: Pin<&mut Self>, path: QString) {
        let qt = self.qt_thread();
        let path = path.to_string();
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_inspect_failure(&qt, path, message.to_owned());
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
                Err(e) => report_inspect_failure(&qt, path, e.to_string()),
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
        let workspace = workspace_id.to_string();
        let hosts: Vec<String> = match serde_json::from_str(&hosts_json.to_string()) {
            Ok(hosts) => hosts,
            Err(e) => {
                // Refused here rather than sent on: an unparseable list would
                // otherwise reach the daemon as an empty one and lock the
                // workspace out of the network entirely.
                report_workspace_op_failure(
                    &qt,
                    workspace,
                    "workspace.set_allowlist",
                    format!("host list is not a JSON array of strings: {e}"),
                );
                return;
            }
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                report_workspace_op_failure(
                    &qt,
                    workspace,
                    "workspace.set_allowlist",
                    message.to_owned(),
                );
                return;
            }
        };
        let params = WorkspaceSetAllowlistParams {
            workspace_id: WorkspaceId(workspace.clone()),
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
                report_workspace_op_failure(
                    &qt,
                    workspace,
                    "workspace.set_allowlist",
                    e.to_string(),
                );
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
            // The commonest way a prerequisite check fails, and the reason the
            // typed signal exists: closing Settings re-checks on the way out,
            // and during a reconnect there is no daemon to answer. It must
            // reach the window as a prerequisite failure rather than as an
            // unrecognised operation, which is what a modal box is put over.
            Err(message) => {
                report_prereqs_failure(&qt, message.to_owned());
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
                    // A Claude login, logout, or setup-token: remembered so
                    // its exit lifts the CLI's verdict about the old login.
                    // See `login_pty`. The token terminal earns the same
                    // treatment as a login: it runs `claude setup-token`
                    // against the same CLI, and a stale override must not
                    // survive it either.
                    let login = matches!(
                        action,
                        SetupAction::ClaudeLogin
                            | SetupAction::ClaudeLogout
                            | SetupAction::ClaudeSetupToken
                    )
                    .then(|| res.pty_id.clone());
                    let _ = qt.queue(move |mut q| {
                        if let Some(pty) = login {
                            q.as_mut().rust_mut().login_pty = Some(pty);
                        }
                        q.setup_pty_opened(QString::from(&name), QString::from(&id))
                    });
                }
                Err(e) => report_failure(&qt, "setup", e.to_string()),
            }
        });
    }

    pub fn prereqs_any_failed(&self) -> bool {
        // An override is a failing row, so the status bar's "Set up…" link is
        // offered for it as for any other cross on the page.
        self.rust().prereqs_any_failed || self.rust().claude_auth_override.is_some()
    }

    /// Sets or clears the CLI's verdict about the login, and everything that
    /// reads it: the `claudeAuthFailure` property, the composer gate, and the
    /// setup page's payload. See `claude_auth_override` for the rule.
    ///
    /// The page is told by a `prereqs_checked` carrying the rewritten payload,
    /// even though no check ran: a Settings dialog already open when the agent
    /// dies would otherwise keep its tick until the next answer, and the one
    /// the exit triggers is a second away at best and can fail. Nothing else
    /// changes: setting the same sentence twice -- a burst of agents dying of
    /// the same login -- costs one rewrite and no repaint.
    fn set_claude_auth_override(mut self: Pin<&mut Self>, sentence: Option<String>) {
        if self.as_ref().rust().claude_auth_override == sentence {
            return;
        }
        match &sentence {
            Some(text) => tracing::warn!("Claude Code says its login has gone: {text}"),
            None => tracing::info!("the Claude login has been redone; the daemon's check decides"),
        }
        self.as_mut().rust_mut().claude_auth_override = sentence.clone();
        self.as_mut()
            .set_claude_auth_failure(QString::from(&sentence.unwrap_or_default()));
        self.as_mut().refresh_claude_logged_in();
        // Only on the way up: the terminal exit that clears it is followed by
        // the setup page's own re-check, whose answer is the payload to draw.
        if self.as_ref().rust().claude_auth_override.is_some() {
            let json = self.as_ref().published_prereqs_json();
            self.prereqs_checked(json);
        }
    }

    /// The prerequisite list as everything outside this object sees it: the
    /// daemon's last answer, with its `claude_auth` row failed by the CLI's own
    /// account while an override is set. See [`override_claude_auth`].
    fn published_prereqs_json(&self) -> QString {
        match &self.rust().claude_auth_override {
            Some(sentence) => QString::from(&override_claude_auth(
                &self.rust().prereqs_json.to_string(),
                sentence,
            )),
            None => self.rust().prereqs_json.clone(),
        }
    }

    pub fn note_setup_shown(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().setup_prompt.note_shown();
    }

    /// Applies one prerequisite answer: the payload a setup page built later
    /// draws from, the gate on every Claude composer, what the status bar shows
    /// and whether Settings opens itself -- then the signal.
    ///
    /// Everything the window and the panes read is set before `prereqs_checked`
    /// is emitted, so a pane rebuilt on that signal and the window's own
    /// handler both see the answer that caused it rather than the one before.
    /// The whole point of the struct is that nothing downstream parses the list
    /// again: it is read once, where the daemon's reply is decoded.
    fn apply_prereqs(mut self: Pin<&mut Self>, answer: PrereqAnswer) {
        {
            let mut rust = self.as_mut().rust_mut();
            rust.prereqs_json = QString::from(&answer.json);
            rust.prereqs_any_failed = answer.any_failed;
            rust.claude_auth_ok = answer.claude_auth_ok;
            rust.setup_prompt.checked(answer.blocked);
        }
        self.as_mut().refresh_claude_logged_in();
        // After the gate, so a pane that hears the answer has already heard
        // whether it opens: the other way round, a logged-in user's composer
        // gate would read "not logged in" for the instant between the two.
        self.as_mut().set_prereqs_answered(true);
        // As published, not as answered: a check that comes back green over a
        // login the CLI has just reported dead is exactly the answer the
        // override exists to correct, and the page must not draw it.
        let json = self.as_ref().published_prereqs_json();
        self.prereqs_checked(json);
    }

    pub fn should_auto_open_setup(&self) -> bool {
        self.rust().setup_prompt.should_open()
    }

    pub fn prereqs_json(&self) -> QString {
        self.published_prereqs_json()
    }

    pub fn validate_workspace_name(&self, name: QString) -> QString {
        match workspace_name::validate(&name.to_string()) {
            Ok(()) => QString::from(""),
            Err(hint) => QString::from(&hint),
        }
    }

    pub fn capabilities_json(&self) -> QString {
        self.rust().capabilities.clone()
    }

    pub fn set_api_key(mut self: Pin<&mut Self>, key: QString) -> bool {
        // `key` is moved straight into the credential store and dropped. It is
        // deliberately not logged, not stored on this object, and not echoed
        // back through any signal.
        let stored = crate::qobjects::settings::set_api_key(&key.to_string());
        self.as_mut().refresh_claude_logged_in();
        stored
    }

    pub fn clear_api_key(mut self: Pin<&mut Self>) -> bool {
        let cleared = crate::qobjects::settings::clear_api_key();
        self.as_mut().refresh_claude_logged_in();
        cleared
    }

    /// Recomputes `claudeLoggedIn` from the last prerequisite answer and the
    /// credential store as it stands now.
    ///
    /// The prerequisite check is the usual trigger, but it is not the only one:
    /// storing or removing an API key changes the answer with no daemon round
    /// trip, and a composer that only reopened on the next check would leave a
    /// key-only user pressing "Log in to Claude Code…" after they had just
    /// supplied the credential in the section below it.
    ///
    /// The daemon's half of [`claude_gate_open`] was decided when its answer
    /// was decoded, so this re-reads the credential store and not the list.
    ///
    /// The CLI's verdict outranks the daemon's half. While an override is set,
    /// the daemon's `claude_auth: ok` is the green tick over a dead refresh
    /// token that this whole rule exists for, and it counts as `false` here.
    fn refresh_claude_logged_in(mut self: Pin<&mut Self>) {
        let daemon_half = {
            let rust = self.as_ref();
            let rust = rust.rust();
            rust.claude_auth_ok && rust.claude_auth_override.is_none()
        };
        let logged_in = claude_gate_open(daemon_half, crate::qobjects::settings::api_key_set());
        self.as_mut().set_claude_logged_in(logged_in);
    }

    pub fn api_key_set(&self) -> bool {
        crate::qobjects::settings::api_key_set()
    }

    pub fn default_permission_mode(&self) -> QString {
        QString::from(&Settings::load().default_permission_mode)
    }

    pub fn set_default_permission_mode(&self, mode: QString) {
        let mode = mode.to_string();
        // `try_load`, not `load`. This writes the whole object back, so a
        // `settings.json` that could not be read has to stop it: answering with
        // the defaults and saving them is how one bad character in a
        // hand-edited file used to cost the user their distro, their daemon
        // path and their log level, because they had changed the permission
        // mode in a combo box afterwards.
        let mut settings = match Settings::try_load() {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("the default permission mode was not recorded: {e}");
                return;
            }
        };
        if settings.default_permission_mode == mode {
            return;
        }
        settings.default_permission_mode = mode;
        if let Err(e) = settings.save() {
            tracing::warn!("settings.json could not be written: {e}");
        }
    }

    pub fn theme(&self) -> QString {
        QString::from(&Settings::load().theme)
    }

    pub fn set_theme(&self, theme: QString) {
        let theme = theme.to_string();
        if !["system", "light", "dark"].contains(&theme.as_str()) {
            return;
        }
        // `try_load` for the same reason [`Self::set_default_permission_mode`]
        // uses it: this writes the whole object back, so a `settings.json` that
        // could not be read must stop it rather than have the defaults saved
        // over the user's distro and daemon path.
        let mut settings = match Settings::try_load() {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("the theme choice was not recorded: {e}");
                return;
            }
        };
        if settings.theme == theme {
            return;
        }
        settings.theme = theme;
        if let Err(e) = settings.save() {
            tracing::warn!("the theme choice could not be saved: {e}");
        }
    }

    pub fn show_agent_meta(&self) -> bool {
        Settings::load().show_agent_meta
    }

    pub fn set_show_agent_meta(&self, show: bool) {
        // `try_load` for the same reason [`Self::set_theme`] uses it: an
        // unreadable `settings.json` must stop the write rather than have the
        // defaults saved over everything else the user chose.
        let mut settings = match Settings::try_load() {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("the transcript's small grey lines were not recorded: {e}");
                return;
            }
        };
        if settings.show_agent_meta == show {
            return;
        }
        settings.show_agent_meta = show;
        if let Err(e) = settings.save() {
            tracing::warn!("the transcript's small grey lines could not be saved: {e}");
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
        // Nothing is recorded until the first `workspace.list` has answered and
        // the restore has run. Until then the model on screen is the one-group
        // placeholder `GroupModel` starts life as: no restore has happened, so
        // it has never been told the user's groups, and what it reports is not
        // an arrangement the user made. Recording it replaces Feature-A and
        // Feature-B in `state.json` with "Default" plus whatever the user has
        // created in the meantime -- and the groups are gone before the daemon
        // has said a word. A daemon that is slow to start, or that refuses the
        // list twice, is exactly when a user creates a workspace to get on with
        // something.
        //
        // The restore sets the flag before it emits `workspacesRestored`, so
        // the very first arrangement the window reports back -- the restored one
        // -- is recorded, and every mutation after it as before.
        if !self.rust().first_list_done {
            tracing::debug!("noteGroups: ignored before the first workspace list");
            return;
        }
        let json = json.to_string();
        match serde_json::from_str::<persistence::PersistedGroups>(&json) {
            // Only when it moved. The window reports on every mutation of the
            // tab model and an arrangement is the smallest part of what moves,
            // so an equal report must not schedule a write of the whole file.
            Ok(reported) => {
                note_state_if_changed(|s| s.set_groups(reported.groups, reported.active_workspace))
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

    pub fn new_agent_in_place(&self) -> bool {
        state_store().with(|s| s.new_agent_in_place)
    }

    pub fn set_new_agent_in_place(&self, in_place: bool) {
        note_state_if_changed(|s| {
            let changed = s.new_agent_in_place != in_place;
            s.new_agent_in_place = in_place;
            changed
        });
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

    /// The prologue every workspace operation shares: book the workspace in,
    /// take the connection, and spawn `f` with both.
    ///
    /// The three operations that go through it -- merge, pull request, discard
    /// -- differ only in the request they send and what they do with the
    /// answer. What they must not differ in is any of this: the same refusal
    /// sentence when one is already out, the same banner for a connection that
    /// is gone, and the booking handed back in the same queued step that raises
    /// that banner. Each used to spell all of it out, which is how a fourth one
    /// would have been written with a subtly different order.
    ///
    /// The failures here are reported with [`report_workspace_failure`] and
    /// [`end_workspace_op_with_failure`] respectively, and the difference
    /// between them is the point: a refusal must *not* free the slot, because
    /// the slot belongs to the operation that is still running.
    fn workspace_op<F, Fut>(mut self: Pin<&mut Self>, op: &'static str, workspace: String, f: F)
    where
        F: FnOnce(Shared, QtHandle, String) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let qt = self.as_mut().qt_thread();
        if !self.as_mut().begin_workspace_op(&workspace) {
            report_workspace_failure(&qt, workspace, op, WORKSPACE_BUSY.to_owned(), String::new());
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(e) => {
                end_workspace_op_with_failure(&qt, workspace, op, e.to_owned(), String::new());
                return;
            }
        };
        runtime().spawn(f(shared, qt, workspace));
    }

    pub fn merge_workspace(
        self: Pin<&mut Self>,
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
        self.workspace_op(
            "workspace.merge",
            workspace,
            |shared, qt, workspace| async move {
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
                        end_workspace_op_with_failure(
                            &qt,
                            workspace,
                            "workspace.merge",
                            message,
                            data,
                        );
                    }
                }
            },
        );
    }

    pub fn create_pr(
        self: Pin<&mut Self>,
        workspace_id: QString,
        title: QString,
        body: QString,
        draft: bool,
    ) {
        let workspace = workspace_id.to_string();
        let params = WorkspaceCreatePrParams {
            workspace_id: WorkspaceId(workspace.clone()),
            title: title.to_string(),
            body: body.to_string(),
            draft,
        };
        self.workspace_op(
            "workspace.create_pr",
            workspace,
            |shared, qt, workspace| async move {
                match shared
                    .client
                    .request::<CreatePrResult>(Request::WorkspaceCreatePr(params))
                    .await
                {
                    Ok(result) => {
                        let url = result.url;
                        tracing::info!(%workspace, %url, "workspace.create_pr answered");
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().end_workspace_op(&workspace);
                            q.pr_created(QString::from(&workspace), QString::from(&url))
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
            },
        );
    }

    pub fn discard_workspace(self: Pin<&mut Self>, workspace_id: QString) {
        let workspace = workspace_id.to_string();
        self.workspace_op(
            "workspace.destroy",
            workspace,
            |shared, qt, workspace| async move {
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
            },
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tab restored onto an agent that was started without a mode carries
    /// none, and before this every restart of it asked for approvals whatever
    /// the user's default said. A mode that was chosen is left alone.
    #[test]
    fn a_start_with_no_permission_mode_gets_the_users_default() {
        let unset = start_options_with_default(r#"{"resume_session":"s1"}"#, "bypassPermissions");
        assert_eq!(unset.permission_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(unset.resume_session.as_deref(), Some("s1"));

        for empty in ["", "{}", "not json"] {
            let options = start_options_with_default(empty, "acceptEdits");
            assert_eq!(
                options.permission_mode.as_deref(),
                Some("acceptEdits"),
                "{empty:?}"
            );
        }

        let chosen =
            start_options_with_default(r#"{"permission_mode":"plan"}"#, "bypassPermissions");
        assert_eq!(chosen.permission_mode.as_deref(), Some("plan"));

        let blank = start_options_with_default(r#"{"permission_mode":""}"#, "bypassPermissions");
        assert_eq!(blank.permission_mode.as_deref(), Some("bypassPermissions"));
    }

    /// The reconnect loop's whole decision about a dead daemon is the exit
    /// code, so that is what this pins. Exit 2 is the daemon's "another daemon
    /// already owns this data directory": it names the directory and stops,
    /// because a replacement would find the same lock and exit the same way.
    /// Everything else -- a clean exit, a crash, a signal that leaves no code
    /// at all -- keeps the caller's own sentence and its place in the backoff.
    #[test]
    fn only_a_busy_data_dir_stops_the_reconnect_loop() {
        let fatal = classify_daemon_exit(Some(launcher::DAEMON_BUSY_EXIT_CODE), "retry me");
        let ConnectFailure::Fatal(text) = fatal else {
            panic!(
                "exit {} must be fatal, got {fatal:?}",
                launcher::DAEMON_BUSY_EXIT_CODE
            );
        };
        assert!(
            text.contains(launcher::DAEMON_DATA_DIR) && text.contains("another daemon owns"),
            "the text must name the directory the user has to free: {text:?}"
        );

        for code in [None, Some(0), Some(1), Some(3), Some(101)] {
            let verdict = classify_daemon_exit(code, "retry me");
            assert!(
                matches!(&verdict, ConnectFailure::Retryable(m) if m == "retry me"),
                "exit {code:?} must be retryable with the caller's reason, got {verdict:?}"
            );
        }
    }

    /// Both versions and the hint, because the status bar is the only place a
    /// mismatch is reported and "which half do I rebuild" is the next question.
    #[test]
    fn the_mismatch_status_carries_both_versions_and_the_fix() {
        let text = mismatch_status(2, 1);
        assert!(
            text.starts_with("daemon: protocol mismatch (daemon 2, IDE 1)"),
            "{text:?}"
        );
        assert!(text.contains("build-daemon.ps1"), "{text:?}");
        assert!(text.contains("reinstall the package"), "{text:?}");
    }

    /// The seventh setup terminal recognised alongside the other six, by the
    /// same name the daemon and `SetupPage` both use for it.
    #[test]
    fn parse_setup_action_recognises_the_setup_token_terminal() {
        assert_eq!(
            parse_setup_action("claude_setup_token"),
            Some(SetupAction::ClaudeSetupToken)
        );
    }

    /// The CLI's exact sentence from the day this was written, as the daemon
    /// delivers it: the reason leads and the exit code trails.
    const EXPIRED: &str =
        "Failed to authenticate: OAuth session expired and could not be refreshed (exit code 1)";

    /// Every phrase on the list, in either case, is the CLI saying the login
    /// has gone; the sentence handed back is the line that said so, without the
    /// daemon's exit-code suffix.
    #[test]
    fn each_cli_auth_phrase_is_recognised_in_any_case() {
        assert_eq!(
            auth_failure_in(EXPIRED).as_deref(),
            Some("Failed to authenticate: OAuth session expired and could not be refreshed")
        );
        for detail in [
            "OAuth session expired",
            "oauth SESSION expired, sorry",
            "the token could not be refreshed",
            "Failed to authenticate",
            "FAILED TO AUTHENTICATE: whatever",
            "Not logged in · Please run /login",
            "server answered invalid_grant",
            // A turn the server refused, as the daemon reports it from the
            // errored result (CLI 2.1.263, a revoked token).
            "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
        ] {
            assert_eq!(
                auth_failure_in(detail).as_deref(),
                Some(detail),
                "{detail:?} must be recognised"
            );
        }
        // A multi-line tail: the matching line, not the whole tail.
        let tail = "warning: something else\nFailed to authenticate: token gone\nexit code 1";
        assert_eq!(
            auth_failure_in(tail).as_deref(),
            Some("Failed to authenticate: token gone")
        );
    }

    /// An exit or an errored turn that says nothing about the login is not
    /// one, whatever else went wrong: a crash, a missing binary, a plain exit
    /// code, a sandbox that went away, a turn that ran out of turns. The wrong
    /// verdict here shuts every composer in the window.
    #[test]
    fn other_exits_are_not_auth_failures() {
        for detail in [
            "",
            "exit code 1",
            "claude: command not found",
            "the sandbox stopped (exit code 137)",
            "panicked at src/main.rs:1:1",
            "error: network is unreachable",
            "authorization header rejected by proxy",
            "the agent ended the turn with an error",
            "error_max_turns",
            "API Error: 529 Overloaded",
        ] {
            assert_eq!(auth_failure_in(detail), None, "{detail:?} must not match");
        }
    }

    /// The setup page draws its rows from the payload, so the override lands
    /// there as a failed `claude_auth` row carrying the CLI's sentence and the
    /// override's fix hint; every other row is left exactly as the daemon
    /// said it, and a list with no such row grows one.
    #[test]
    fn the_override_rewrites_only_the_claude_auth_row() {
        let daemon = serde_json::to_string(&[
            PrereqStatus {
                name: "git".into(),
                ok: true,
                detail: "git 2.43".into(),
                fix_hint: None,
            },
            PrereqStatus {
                name: CLAUDE_AUTH_PREREQ.into(),
                ok: true,
                detail: "logged in to Claude".into(),
                fix_hint: None,
            },
        ])
        .unwrap();
        let sentence = auth_failure_in(EXPIRED).unwrap();
        let items: Vec<PrereqStatus> =
            serde_json::from_str(&override_claude_auth(&daemon, &sentence)).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].ok && items[0].name == "git", "{items:?}");
        let auth = &items[1];
        assert_eq!(auth.name, CLAUDE_AUTH_PREREQ);
        assert!(!auth.ok);
        assert_eq!(auth.detail, sentence);
        assert_eq!(auth.fix_hint.as_deref(), Some(CLAUDE_AUTH_OVERRIDE_FIX));
        // The daemon's list itself is untouched by the rewrite.
        assert!(claude_auth_ok(
            &serde_json::from_str::<Vec<PrereqStatus>>(&daemon).unwrap()
        ));

        for empty in ["", "[]", "not json"] {
            let items: Vec<PrereqStatus> =
                serde_json::from_str(&override_claude_auth(empty, &sentence)).unwrap();
            assert_eq!(items.len(), 1, "{empty:?}");
            assert!(items[0].name == CLAUDE_AUTH_PREREQ && !items[0].ok);
        }
    }
}
