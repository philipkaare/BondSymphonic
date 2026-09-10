//! The Run panel's state: one app-wide instance, following the active tab.
//!
//! Like `ChangesModel`, one object serves every tab: pointing it at a workspace
//! detects that worktree's run configurations, lists the runs the daemon has
//! for it, and subscribes to each of them. What it holds is kept *per
//! workspace* rather than thrown away on every switch, because a run keeps
//! running (and keeps printing) while the user is looking at another tab, and
//! because a blocked host has to be offered to the workspace it was blocked
//! for, not to whichever one happens to be visible when the event arrives.
//!
//! Every decision lives in [`WorkspaceRuns`]; this object is the adapter that
//! moves JSON and signals across the boundary and owns the subscriptions.

use crate::client::router::EventRx;
use crate::model::run_config::{denial_owner, run_state_word, RunView, WorkspaceRuns};
use crate::qobjects::app_controller::{
    connection_generation, on_reconnect, require_connection, runtime, state_store, Shared,
};
use bondsymphonic_proto::{
    DetectRunConfigsResult, Event, RepoPathParams, Request, RunConfig, RunId, RunIdParams, RunInfo,
    RunListResult, RunStartParams, RunStartResult, RunState, WorkspaceId, WorkspaceIdParams,
    WorkspaceInfo, WorkspaceSetAllowlistParams,
};
use std::collections::BTreeMap;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `set_workspace` is exposed as `setWorkspace` and `active_run_id` as
    // `getActiveRunId`/`activeRunIdChanged`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        // `active_state` is the daemon's word for the run of the selected
        // configuration that is alive ("starting" or "ready"), and empty when
        // there is none: that empty string is what turns Stop and Open off.
        // `busy` is true while a request this panel made is in flight.
        #[qobject]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, selected_config)]
        #[qproperty(QString, active_state)]
        #[qproperty(QString, active_url)]
        #[qproperty(QString, active_run_id)]
        #[qproperty(bool, busy)]
        type RunPanelModel = super::RunPanelModelRust;

        /// The configuration list changed; rebuild the combo from
        /// `configsJson()`.
        #[qsignal]
        fn configs_changed(self: Pin<&mut RunPanelModel>);

        /// The run list changed; reread `runsJson()`.
        #[qsignal]
        fn runs_changed(self: Pin<&mut RunPanelModel>);

        /// One line of output arrived for `run_id`. The panel appends it when
        /// that is the run it is showing, and otherwise ignores it: the line is
        /// in this model's ring buffer either way and `logText` will have it.
        #[qsignal]
        fn output_appended(self: Pin<&mut RunPanelModel>, run_id: QString, line: QString);

        /// The active run, its state or its URL changed; reread the four
        /// properties.
        #[qsignal]
        fn state_changed(self: Pin<&mut RunPanelModel>);

        /// Show the denial toast for `host`. Exactly one host is offered at a
        /// time; the next (if any) follows once this one has been answered with
        /// `allowHost` or `dismissDenied`.
        #[qsignal]
        fn denied(self: Pin<&mut RunPanelModel>, host: QString);

        /// Take the denial toast down: the host it was showing has been
        /// answered, or the panel has moved to a workspace that has no denial
        /// waiting. A toast left up after a tab switch would answer for the
        /// wrong workspace, so the panel must act on this.
        #[qsignal]
        fn denied_cleared(self: Pin<&mut RunPanelModel>);

        /// A request this panel made failed. Nothing changed.
        #[qsignal]
        fn error_occurred(self: Pin<&mut RunPanelModel>, message: QString);

        /// An `allowHost` for `host` failed, and its toast is still up.
        ///
        /// Separate from `errorOccurred` and from the model-wide `busy`,
        /// because the toast's buttons are armed by the answer to *this* call:
        /// an unrelated `run.start` finishing mid-allow would otherwise re-arm
        /// the offer and let a second click send a duplicate `set_allowlist`.
        #[qsignal]
        fn denial_failed(self: Pin<&mut RunPanelModel>, host: QString, message: QString);

        /// Points the panel at a workspace and its worktree: detects the run
        /// configurations for `worktree_path`, lists the workspace's runs and
        /// subscribes to each of them. The empty id detaches. Naming the
        /// workspace and path it already holds does nothing, so this is safe to
        /// call on every tab switch.
        #[qinvokable]
        fn set_workspace(
            self: Pin<&mut RunPanelModel>,
            workspace_id: QString,
            worktree_path: QString,
        );

        /// Points the combo at `name`. The empty name detaches the selection;
        /// an unknown or disabled name is refused.
        #[qinvokable]
        fn select_config(self: Pin<&mut RunPanelModel>, name: QString);

        /// Starts the selected configuration, on the port the user set for it
        /// in this workspace (`AppController::setPortOverride`, kept in
        /// `state.json`) or, with none, on the configuration's own port.
        #[qinvokable]
        fn start(self: Pin<&mut RunPanelModel>);

        /// Stops the active run.
        #[qinvokable]
        fn stop(self: Pin<&mut RunPanelModel>);

        /// Re-detects the configurations and re-lists the runs of the current
        /// workspace.
        #[qinvokable]
        fn refresh(self: Pin<&mut RunPanelModel>);

        /// One run's whole output, for filling the log widget when the panel
        /// switches to it. Empty for a run with no output.
        #[qinvokable]
        fn log_text(self: &RunPanelModel, run_id: QString) -> QString;

        /// Adds `host` to the workspace's allowlist and takes its toast down.
        /// Reads the list the daemon currently has, so an allowlist changed
        /// elsewhere is extended rather than replaced.
        #[qinvokable]
        fn allow_host(self: Pin<&mut RunPanelModel>, host: QString);

        /// Takes `host`'s toast down without allowing it.
        #[qinvokable]
        fn dismiss_denied(self: Pin<&mut RunPanelModel>, host: QString);

        /// Queues a blocked host for `workspace_id`. This is what the window
        /// connects `AppController::networkDenied` to; a host blocked for a
        /// workspace the user is not looking at waits until they are.
        #[qinvokable]
        fn note_denied(self: Pin<&mut RunPanelModel>, workspace_id: QString, host: QString);

        /// Drops everything held for a workspace and unsubscribes from its
        /// runs. The window calls this when a workspace is destroyed.
        #[qinvokable]
        fn forget_workspace(self: Pin<&mut RunPanelModel>, workspace_id: QString);

        /// The detected configurations as a JSON array of `RunConfig`.
        #[qinvokable]
        fn configs_json(self: &RunPanelModel) -> QString;

        /// Everything `repo.detect_run_configs` had to complain about in this
        /// workspace's `bondsymphonic.toml`, one entry per complaint, joined
        /// with newlines. Empty when the file was clean or there was none. The
        /// panel hangs this on the configuration combo as its tooltip, which is
        /// where a complaint that runs to several lines -- a parse error, with
        /// the offending line and a caret -- belongs.
        #[qinvokable]
        fn config_warnings(self: &RunPanelModel) -> QString;

        /// The one line the panel shows under the row: the single complaint
        /// when there is one, and "N problems with bondsymphonic.toml" when
        /// there are more. Empty when there is nothing to say.
        #[qinvokable]
        fn config_warning_status(self: &RunPanelModel) -> QString;

        /// The selected configuration as a JSON `RunConfig`, or the literal
        /// `null`. The port hint is read out of this.
        #[qinvokable]
        fn selected_config_json(self: &RunPanelModel) -> QString;

        /// Every run of the current workspace, as a JSON array.
        #[qinvokable]
        fn runs_json(self: &RunPanelModel) -> QString;
    }

    impl cxx_qt::Threading for RunPanelModel {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::RunPanelModel>;

/// Ends one run's router subscription. Held on the Qt side so that dropping the
/// object releases it even though the task that would otherwise do it is
/// aborted rather than run to completion.
type Unsubscribe = Box<dyn FnOnce() + Send>;

/// One live `run.*` subscription.
struct Subscription {
    task: tokio::task::JoinHandle<()>,
    unsubscribe: Option<Unsubscribe>,
    /// The connection this was taken on; see [`needs_resubscribe`].
    generation: u64,
}

/// The repository file the daemon reads run configurations from, named in the
/// status line above. Spelled here rather than shared with the daemon by
/// import: the two crates share `bondsymphonic-proto`, not this file name's
/// meaning, and this is the IDE telling a user which file to open.
const CONFIG_FILE: &str = "bondsymphonic.toml";

/// The one line the panel shows under its row when `repo.detect_run_configs`
/// had something to complain about.
///
/// Pure, so the wording is settled without a Qt event loop.
///
/// A single complaint is shown as the daemon wrote it. Each entry is already a
/// finished sentence naming the file
/// (`bondsymphonic.toml: [[run]] #2 ("api") has no port; it is not offered`),
/// so anything this added would be said twice.
///
/// Several are counted rather than joined, and counted as *problems* rather
/// than as ignored configurations: the list is not one line per dropped
/// `[[run]]` entry. A file that will not parse at all yields exactly one
/// warning carrying the `toml` crate's own message, caret and all, for a file
/// that may have declared any number of runs -- so `warnings.len()` is a count
/// of complaints and nothing else. The complaints themselves are on the
/// configuration combo's tooltip, which is where a multi-line one belongs.
pub fn warning_status(warnings: &[String]) -> String {
    match warnings.len() {
        0 => String::new(),
        1 => warnings[0].clone(),
        n => format!("{n} problems with {CONFIG_FILE}"),
    }
}

/// Whether a run has to be subscribed to now.
///
/// `existing` is the connection generation of the subscription the panel
/// already holds for that run, if any, and `current` the connection that is
/// live. A run with no subscription needs one; so does a run whose
/// subscription was taken on a connection that has since been replaced,
/// because every connect builds a fresh `EventRouter` and the old one's sender
/// is never dispatched into again. Without this the panel would go silent for
/// a run that is still running, with `refresh()` unable to recover it.
pub fn needs_resubscribe(existing: Option<u64>, current: u64) -> bool {
    existing != Some(current)
}

impl Subscription {
    fn end(mut self) {
        self.task.abort();
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

#[derive(Default)]
pub struct RunPanelModelRust {
    workspace_id: QString,
    selected_config: QString,
    active_state: QString,
    active_url: QString,
    active_run_id: QString,
    busy: bool,
    /// State per workspace, keyed by workspace id.
    by_workspace: BTreeMap<String, WorkspaceRuns>,
    /// Each workspace's worktree, so `refresh` can re-detect without being
    /// handed the path again.
    worktrees: BTreeMap<String, String>,
    /// Which workspace each known run belongs to. Run events carry a run id
    /// and the envelope's workspace id is not something this depends on.
    run_workspace: BTreeMap<String, String>,
    /// One entry per run being followed.
    subscriptions: BTreeMap<String, Subscription>,
    /// The `(workspace, host)` the toast is showing, or `None` when none is up.
    /// Answering a toast uses this rather than the current workspace, so a tab
    /// switch cannot send a host to a workspace that never denied it.
    current_denial: Option<(String, String)>,
    /// Requests in flight; `busy` is this being non-zero.
    inflight: u32,
    /// Waits for the connection generation to move and then refreshes the
    /// panel against the daemon that came back. Replaced on every arm and
    /// aborted on Drop.
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RunPanelModelRust {
    fn drop(&mut self) {
        if let Some(task) = self.reconnect_task.take() {
            task.abort();
        }
        for (_, sub) in std::mem::take(&mut self.subscriptions) {
            sub.end();
        }
    }
}

/// Reports a failed request as `errorOccurred`, from a tokio task.
fn report(qt: &QtHandle, message: String) {
    tracing::warn!("{message}");
    let _ = qt.queue(move |q| q.error_occurred(QString::from(&message)));
}

/// Detects the configurations of `worktree` and lists `workspace`'s runs, then
/// applies both in one closure so the combo and the run list never disagree.
///
/// A failure of either half is reported and answered with an empty list rather
/// than abandoning the other: a worktree with no configurations is a normal
/// state of the panel, and a run list that could not be read must not leave a
/// stale one behind claiming another workspace's runs.
async fn detect_and_list(shared: Shared, qt: QtHandle, workspace: String, worktree: String) {
    let (configs, warnings) = if worktree.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let params = RepoPathParams {
            path: worktree.clone(),
        };
        match shared
            .client
            .request::<DetectRunConfigsResult>(Request::RepoDetectRunConfigs(params))
            .await
        {
            Ok(res) => (res.configs, res.warnings),
            Err(e) => {
                report(&qt, format!("repo.detect_run_configs failed: {e}"));
                (Vec::new(), Vec::new())
            }
        }
    };
    let params = WorkspaceIdParams {
        workspace_id: WorkspaceId(workspace.clone()),
    };
    let runs = match shared
        .client
        .request::<RunListResult>(Request::RunList(params))
        .await
    {
        Ok(res) => res.runs,
        Err(e) => {
            report(&qt, format!("run.list failed: {e}"));
            Vec::new()
        }
    };
    let _ = qt.queue(move |q| q.apply_detection(workspace, configs, warnings, runs));
}

/// Follows one run until it finishes or the panel lets go of it.
async fn follow_run(shared: Shared, qt: QtHandle, run: RunId, mut rx: EventRx) {
    while let Some((_, event)) = rx.recv().await {
        // A run that has stopped or failed sends nothing more, so the task ends
        // with it rather than parking on a channel forever.
        let finished = matches!(
            event,
            Event::RunStateChanged {
                state: RunState::Stopped | RunState::Failed,
                ..
            }
        );
        let id = run.to_string();
        if qt.queue(move |q| q.apply_run_event(id, event)).is_err() || finished {
            break;
        }
    }
    shared.router.unsubscribe_run(&run);
}

impl qobject::RunPanelModel {
    pub fn set_workspace(mut self: Pin<&mut Self>, workspace_id: QString, worktree_path: QString) {
        let workspace = workspace_id.to_string();
        let worktree = worktree_path.to_string();
        let unchanged = self.as_ref().rust().workspace_id.to_string() == workspace
            && self.as_ref().rust().worktrees.get(&workspace) == Some(&worktree);
        if unchanged {
            return;
        }
        self.as_mut().set_workspace_id(QString::from(&workspace));
        if workspace.is_empty() {
            self.as_mut().publish_current();
            // Detaching takes any toast down: it belongs to a workspace the
            // panel is no longer showing.
            self.pump_denial();
            return;
        }
        self.as_mut().arm_reconnect();
        {
            let mut rust = self.as_mut().rust_mut();
            rust.by_workspace.entry(workspace.clone()).or_default();
            rust.worktrees.insert(workspace.clone(), worktree.clone());
        }
        // Whatever is already known about this workspace is painted at once;
        // the detection below refreshes it when it answers.
        self.as_mut().publish_current();
        self.as_mut().pump_denial();
        self.as_mut().load(workspace, worktree);
    }

    pub fn refresh(self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.to_string();
        if workspace.is_empty() {
            return;
        }
        let worktree = self
            .as_ref()
            .rust()
            .worktrees
            .get(&workspace)
            .cloned()
            .unwrap_or_default();
        self.load(workspace, worktree);
    }

    pub fn select_config(mut self: Pin<&mut Self>, name: QString) {
        let workspace = self.as_ref().rust().workspace_id.to_string();
        let name = name.to_string();
        let changed = {
            let mut rust = self.as_mut().rust_mut();
            match rust.by_workspace.get_mut(&workspace) {
                Some(entry) => entry.select(&name),
                None => false,
            }
        };
        if changed {
            // The active run follows the selection, so both are republished.
            self.as_mut().publish_selection();
            self.publish_active();
        }
    }

    pub fn start(mut self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.to_string();
        let config = self
            .as_ref()
            .rust()
            .by_workspace
            .get(&workspace)
            .and_then(|entry| entry.selected.clone());
        let Some(config) = config else {
            self.fail("no run configuration is selected");
            return;
        };
        // One run per configuration per workspace (daemon spec §10.3). The
        // daemon would answer `Conflict`, but a model that knows a run is
        // already up should not have to ask.
        let already = self
            .as_ref()
            .rust()
            .by_workspace
            .get(&workspace)
            .is_some_and(|entry| entry.active_run().is_some());
        if already {
            self.fail("that configuration is already running");
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        self.as_mut().begin_request();
        // The port the user set on this configuration in this workspace, kept
        // in `state.json` so it survives a restart. `None` means the port the
        // configuration itself names.
        let port = state_store().with(|s| s.port_override(&workspace, &config));
        if let Some(port) = port {
            tracing::info!(%workspace, %config, port, "run.start port override");
        }
        let params = RunStartParams {
            workspace_id: WorkspaceId(workspace.clone()),
            config_name: config.clone(),
            port,
        };
        runtime().spawn(async move {
            match shared
                .client
                .request::<RunStartResult>(Request::RunStart(params))
                .await
            {
                Ok(res) => {
                    let _ = qt.queue(move |q| q.apply_started(workspace, config, res));
                }
                Err(e) => {
                    report(&qt, format!("run.start failed: {e}"));
                    let _ = qt.queue(|q| q.end_request());
                }
            }
        });
    }

    pub fn stop(mut self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.to_string();
        let run = self
            .as_ref()
            .rust()
            .by_workspace
            .get(&workspace)
            .and_then(|entry| entry.active_run())
            .map(|run| run.run_id.clone());
        let Some(run) = run else {
            self.fail("nothing is running");
            return;
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        self.as_mut().begin_request();
        let params = RunIdParams { run_id: RunId(run) };
        runtime().spawn(async move {
            // Success is silent: the `run.state stopped` event is what moves
            // the panel, and it arrives on the run's own subscription.
            if let Err(e) = shared.client.request_raw(Request::RunStop(params)).await {
                report(&qt, format!("run.stop failed: {e}"));
            }
            let _ = qt.queue(|q| q.end_request());
        });
    }

    pub fn log_text(&self, run_id: QString) -> QString {
        let workspace = self.rust().workspace_id.to_string();
        match self.rust().by_workspace.get(&workspace) {
            Some(entry) => QString::from(&entry.log_text(&run_id.to_string())),
            None => QString::from(""),
        }
    }

    pub fn allow_host(mut self: Pin<&mut Self>, host: QString) {
        let host = host.to_string();
        // Defence in depth: the daemon refuses to publish a denial whose host
        // is not a plain name or an address, and `HostPattern::parse` refuses a
        // wildcard with only a top-level domain behind it. Neither has to hold
        // for this click to be safe, so the wildcard stops here as well.
        if host.contains('*') {
            self.as_mut()
                .deny_failed(&host, "a blocked host is never a pattern");
            return;
        }
        let Some(workspace) = self.as_ref().denial_owner_of(&host) else {
            tracing::debug!("allowHost: no workspace has {host:?} blocked");
            return;
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                let message = message.to_owned();
                self.as_mut().deny_failed(&host, &message);
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        self.as_mut().begin_request();
        runtime().spawn(async move {
            // The daemon's own list, read at the moment of the click: an
            // allowlist edited elsewhere (the repo's `bondsymphonic.toml` at
            // creation, another window) must be extended, not replaced by a
            // stale copy that would silently un-allow hosts.
            let params = WorkspaceIdParams {
                workspace_id: WorkspaceId(workspace.clone()),
            };
            let mut hosts = match shared
                .client
                .request::<WorkspaceInfo>(Request::WorkspaceGet(params))
                .await
            {
                Ok(info) => info.allowlist,
                Err(e) => {
                    let message = format!("workspace.get failed: {e}");
                    let host = host.clone();
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_request();
                        q.deny_failed(&host, &message);
                    });
                    return;
                }
            };
            if !hosts.iter().any(|h| h == &host) {
                hosts.push(host.clone());
            }
            let params = WorkspaceSetAllowlistParams {
                workspace_id: WorkspaceId(workspace.clone()),
                hosts,
            };
            match shared
                .client
                .request_raw(Request::WorkspaceSetAllowlist(params))
                .await
            {
                // The toast comes down only once the host really is allowed;
                // a failed call leaves it up so the user can try again.
                Ok(_) => {
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_request();
                        q.clear_denial(&workspace, &host);
                    });
                }
                Err(e) => {
                    let message = format!("workspace.set_allowlist failed: {e}");
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().end_request();
                        q.deny_failed(&host, &message);
                    });
                }
            }
        });
    }

    pub fn dismiss_denied(self: Pin<&mut Self>, host: QString) {
        let host = host.to_string();
        let Some(workspace) = self.as_ref().denial_owner_of(&host) else {
            return;
        };
        self.clear_denial(&workspace, &host);
    }

    pub fn note_denied(mut self: Pin<&mut Self>, workspace_id: QString, host: QString) {
        let workspace = workspace_id.to_string();
        let host = host.to_string();
        if workspace.is_empty() || host.is_empty() {
            return;
        }
        let queued = {
            let mut rust = self.as_mut().rust_mut();
            rust.by_workspace
                .entry(workspace.clone())
                .or_default()
                .note_denied(&host)
        };
        let showing = self.as_ref().rust().workspace_id.to_string() == workspace;
        // Only a host that reached the head of the queue is offered now: the
        // one already on screen has to be answered first.
        if queued && showing {
            self.pump_denial();
        }
    }

    pub fn forget_workspace(mut self: Pin<&mut Self>, workspace_id: QString) {
        let workspace = workspace_id.to_string();
        let runs: Vec<String> = self
            .as_ref()
            .rust()
            .run_workspace
            .iter()
            .filter(|(_, ws)| *ws == &workspace)
            .map(|(run, _)| run.clone())
            .collect();
        for run in runs {
            self.as_mut().forget_run(&run);
        }
        {
            let mut rust = self.as_mut().rust_mut();
            rust.by_workspace.remove(&workspace);
            rust.worktrees.remove(&workspace);
        }
        if self.as_ref().rust().workspace_id.to_string() == workspace {
            self.as_mut().set_workspace_id(QString::from(""));
            self.as_mut().publish_current();
        }
        // The queue went with the workspace, so a toast raised for it has
        // nothing left to answer.
        self.pump_denial();
    }

    pub fn configs_json(&self) -> QString {
        match self.current() {
            Some(entry) => QString::from(&entry.configs_json()),
            None => QString::from("[]"),
        }
    }

    pub fn config_warnings(&self) -> QString {
        match self.current() {
            Some(entry) => QString::from(&entry.warnings.join("\n")),
            None => QString::from(""),
        }
    }

    pub fn config_warning_status(&self) -> QString {
        match self.current() {
            Some(entry) => QString::from(&warning_status(&entry.warnings)),
            None => QString::from(""),
        }
    }

    pub fn selected_config_json(&self) -> QString {
        match self.current() {
            Some(entry) => QString::from(&entry.selected_config_json()),
            None => QString::from("null"),
        }
    }

    pub fn runs_json(&self) -> QString {
        match self.current() {
            Some(entry) => QString::from(&entry.runs_json()),
            None => QString::from("[]"),
        }
    }

    /// Arms the watch that refreshes this panel when the connection is
    /// replaced. Any previous watch is dropped first, so a tab switch leaves
    /// exactly one armed.
    fn arm_reconnect(mut self: Pin<&mut Self>) {
        if let Some(previous) = self.as_mut().rust_mut().reconnect_task.take() {
            previous.abort();
        }
        let qt = self.as_ref().qt_thread();
        let watch = on_reconnect(qt, qobject::RunPanelModel::reload_after_reconnect);
        self.as_mut().rust_mut().reconnect_task = Some(watch);
    }

    /// Re-reads the shown workspace from the daemon that came back.
    ///
    /// `refresh` is the whole answer: it re-detects the configurations and
    /// re-lists the runs, and `apply_detection` then drops every run the new
    /// daemon does not have and re-subscribes to the ones it does -- on the
    /// new router, because `watch_run` compares the connection generation the
    /// old subscription was taken on. A daemon restart loses every run, so in
    /// practice what comes back is an empty list and a panel that says so
    /// rather than one still showing a run that stopped existing.
    fn reload_after_reconnect(mut self: Pin<&mut Self>) {
        if self.as_ref().rust().workspace_id.to_string().is_empty() {
            return;
        }
        tracing::info!("run panel refreshing after a reconnect");
        self.as_mut().arm_reconnect();
        self.refresh();
    }

    /// Detection plus a run list for one workspace, in the background.
    fn load(mut self: Pin<&mut Self>, workspace: String, worktree: String) {
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        self.as_mut().begin_request();
        runtime().spawn(detect_and_list(shared, qt, workspace, worktree));
    }

    /// Installs what `repo.detect_run_configs` and `run.list` answered.
    ///
    /// Applied to the workspace it was asked about even if the user has since
    /// switched tabs -- it is that workspace's own answer -- but published only
    /// while that workspace is the one on screen.
    fn apply_detection(
        mut self: Pin<&mut Self>,
        workspace: String,
        configs: Vec<RunConfig>,
        warnings: Vec<String>,
        runs: Vec<RunInfo>,
    ) {
        self.as_mut().end_request();
        let ids: Vec<String> = runs.iter().map(|r| r.run_id.to_string()).collect();
        {
            let mut rust = self.as_mut().rust_mut();
            // Gone: the workspace was destroyed while this was in flight.
            let Some(entry) = rust.by_workspace.get_mut(&workspace) else {
                return;
            };
            entry.set_configs(configs);
            entry.set_warnings(warnings);
            entry.apply_list(runs);
        }
        // The daemon's list is authoritative: a run it no longer has is one
        // this panel stops following, so `subscriptions` and the run list stay
        // in step even after a daemon restart lost a run without an event.
        let stale: Vec<String> = self
            .as_ref()
            .rust()
            .run_workspace
            .iter()
            .filter(|(run, ws)| *ws == &workspace && !ids.contains(run))
            .map(|(run, _)| run.clone())
            .collect();
        for run in stale {
            self.as_mut().forget_run(&run);
        }
        for id in ids {
            self.as_mut().watch_run(&workspace, &id);
        }
        if self.as_ref().rust().workspace_id.to_string() == workspace {
            self.publish_current();
        }
    }

    /// Installs the run `run.start` just created and starts following it.
    fn apply_started(
        mut self: Pin<&mut Self>,
        workspace: String,
        config: String,
        res: RunStartResult,
    ) {
        self.as_mut().end_request();
        let run_id = res.run_id.to_string();
        {
            let mut rust = self.as_mut().rust_mut();
            let Some(entry) = rust.by_workspace.get_mut(&workspace) else {
                return;
            };
            entry.runs.retain(|r| r.run_id != run_id);
            entry.runs.push(RunView {
                run_id: run_id.clone(),
                config_name: config,
                // The daemon publishes `starting` as well; recording it here
                // means Stop is offered from the moment the call answers rather
                // than from whenever that event arrives.
                state: run_state_word(RunState::Starting).to_owned(),
                host_port: res.host_port,
                url: res.url,
                detail: String::new(),
            });
        }
        // Subscribing after the reply is what the router's early buffer is
        // for: the banner a dev server printed in between is replayed.
        self.as_mut().watch_run(&workspace, &run_id);
        if self.as_ref().rust().workspace_id.to_string() == workspace {
            self.as_mut().runs_changed();
            self.publish_active();
        }
    }

    /// Applies one `run.output` or `run.state` for a run this panel follows.
    fn apply_run_event(mut self: Pin<&mut Self>, run_id: String, event: Event) {
        let Some(workspace) = self.as_ref().rust().run_workspace.get(&run_id).cloned() else {
            return;
        };
        let showing = self.as_ref().rust().workspace_id.to_string() == workspace;
        match event {
            Event::RunOutput { line, .. } => {
                {
                    let mut rust = self.as_mut().rust_mut();
                    let Some(entry) = rust.by_workspace.get_mut(&workspace) else {
                        return;
                    };
                    entry.apply_output(&run_id, line.clone());
                }
                if showing {
                    self.output_appended(QString::from(&run_id), QString::from(&line));
                }
            }
            Event::RunStateChanged {
                state, url, detail, ..
            } => {
                let changed = {
                    let mut rust = self.as_mut().rust_mut();
                    let Some(entry) = rust.by_workspace.get_mut(&workspace) else {
                        return;
                    };
                    entry.apply_state(&run_id, state, url.as_deref(), detail.as_deref())
                };
                if matches!(state, RunState::Stopped | RunState::Failed) {
                    // Nothing more will be said about this run, and a later run
                    // of the same configuration gets its own id and its own
                    // subscription.
                    self.as_mut().stop_watching(&run_id);
                }
                if changed && showing {
                    self.as_mut().runs_changed();
                    self.publish_active();
                }
            }
            _ => {}
        }
    }

    /// Subscribes to `run`, unless it is already being followed on the
    /// connection that is live.
    ///
    /// A subscription left over from before a reconnect is ended and replaced:
    /// its sender belongs to a router nothing dispatches into any more, so
    /// keeping it would leave a running run silent for the rest of the session
    /// and make `refresh()` a no-op for it.
    fn watch_run(mut self: Pin<&mut Self>, workspace: &str, run_id: &str) {
        let generation = connection_generation();
        {
            let mut rust = self.as_mut().rust_mut();
            rust.run_workspace
                .insert(run_id.to_owned(), workspace.to_owned());
            let existing = rust.subscriptions.get(run_id).map(|sub| sub.generation);
            if !needs_resubscribe(existing, generation) {
                return;
            }
        }
        // Ends the stale task and drops its row in the old router, so nothing
        // is left parked on a channel that will never close.
        self.as_mut().stop_watching(run_id);
        let Ok(shared) = require_connection() else {
            return;
        };
        let run = RunId(run_id.to_owned());
        let rx = shared.router.subscribe_run(&run);
        let unsubscribe = {
            let router = shared.router.clone();
            let run = run.clone();
            move || router.unsubscribe_run(&run)
        };
        let qt = self.as_ref().qt_thread();
        let task = runtime().spawn(follow_run(shared, qt, run, rx));
        self.as_mut().rust_mut().subscriptions.insert(
            run_id.to_owned(),
            Subscription {
                task,
                unsubscribe: Some(Box::new(unsubscribe)),
                generation,
            },
        );
    }

    /// Ends one run's subscription. The run's log and its entry in the list
    /// stay: a stopped run is still worth reading.
    fn stop_watching(mut self: Pin<&mut Self>, run_id: &str) {
        let sub = self.as_mut().rust_mut().subscriptions.remove(run_id);
        if let Some(sub) = sub {
            sub.end();
        }
    }

    /// Ends a run's subscription and forgets which workspace it belonged to.
    /// For a run that is gone for good, rather than one that merely finished.
    fn forget_run(mut self: Pin<&mut Self>, run_id: &str) {
        self.as_mut().stop_watching(run_id);
        self.as_mut().rust_mut().run_workspace.remove(run_id);
    }

    /// Takes `host` out of a workspace's queue and offers the next one.
    fn clear_denial(mut self: Pin<&mut Self>, workspace: &str, host: &str) {
        let cleared = {
            let mut rust = self.as_mut().rust_mut();
            match rust.by_workspace.get_mut(workspace) {
                Some(entry) => entry.clear_denied(host),
                None => false,
            }
        };
        if cleared && self.as_ref().rust().workspace_id.to_string() == workspace {
            self.pump_denial();
        }
    }

    /// Brings the toast in line with the head of the current workspace's queue.
    ///
    /// Raises `denied` for a new head, `deniedCleared` when there is no longer
    /// one, and says nothing when the head has not moved -- so a second blocked
    /// host arriving behind the one on screen, or a switch back to a workspace
    /// whose toast is already up, does not raise the same host twice.
    fn pump_denial(mut self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.to_string();
        let next = self
            .as_ref()
            .rust()
            .by_workspace
            .get(&workspace)
            .and_then(|entry| entry.current_denial())
            .map(|host| (workspace.clone(), host.to_owned()));
        if next == self.as_ref().rust().current_denial {
            return;
        }
        self.as_mut().rust_mut().current_denial = next.clone();
        match next {
            Some((_, host)) => self.denied(QString::from(&host)),
            None => self.denied_cleared(),
        }
    }

    /// Whose denial `host` is, for an `allowHost`/`dismissDenied` that carries
    /// only the host. The toast on screen wins; failing that, the current
    /// workspace, and only when it really has that host queued.
    fn denial_owner_of(&self, host: &str) -> Option<String> {
        if host.is_empty() {
            return None;
        }
        let workspace = self.rust().workspace_id.to_string();
        let queued_here = self
            .rust()
            .by_workspace
            .get(&workspace)
            .is_some_and(|entry| entry.denied_hosts.iter().any(|h| h == host));
        let shown = self
            .rust()
            .current_denial
            .as_ref()
            .map(|(ws, h)| (ws.as_str(), h.as_str()));
        denial_owner(shown, queued_here.then_some(workspace.as_str()), host)
    }

    /// Republishes everything the panel paints for the current workspace.
    fn publish_current(mut self: Pin<&mut Self>) {
        self.as_mut().publish_selection();
        self.as_mut().publish_active();
        self.as_mut().configs_changed();
        self.runs_changed();
    }

    fn publish_selection(self: Pin<&mut Self>) {
        let selected = self
            .as_ref()
            .current()
            .and_then(|entry| entry.selected.clone())
            .unwrap_or_default();
        self.set_selected_config(QString::from(&selected));
    }

    fn publish_active(mut self: Pin<&mut Self>) {
        let (state, url, id) = match self.as_ref().current().and_then(WorkspaceRuns::active_run) {
            Some(run) => (run.state.clone(), run.url.clone(), run.run_id.clone()),
            None => (String::new(), String::new(), String::new()),
        };
        self.as_mut().set_active_state(QString::from(&state));
        self.as_mut().set_active_url(QString::from(&url));
        self.as_mut().set_active_run_id(QString::from(&id));
        self.state_changed();
    }

    /// One more request in flight.
    fn begin_request(mut self: Pin<&mut Self>) {
        let count = {
            let mut rust = self.as_mut().rust_mut();
            rust.inflight += 1;
            rust.inflight
        };
        if count == 1 {
            self.set_busy(true);
        }
    }

    /// One fewer. Counted rather than a plain flag so two overlapping requests
    /// do not have the first to answer clear the second's spinner.
    fn end_request(mut self: Pin<&mut Self>) {
        let count = {
            let mut rust = self.as_mut().rust_mut();
            rust.inflight = rust.inflight.saturating_sub(1);
            rust.inflight
        };
        if count == 0 {
            self.set_busy(false);
        }
    }

    /// Raises `errorOccurred` from the Qt thread.
    fn fail(self: Pin<&mut Self>, message: &str) {
        tracing::warn!("run panel: {message}");
        self.error_occurred(QString::from(message));
    }

    /// Reports a failed `allowHost` on its own signal as well as the panel's
    /// error line, so the toast can re-arm exactly its own buttons.
    fn deny_failed(mut self: Pin<&mut Self>, host: &str, message: &str) {
        tracing::warn!("run panel: allowHost {host}: {message}");
        self.as_mut()
            .denial_failed(QString::from(host), QString::from(message));
        self.error_occurred(QString::from(message));
    }

    /// The current workspace's state, or `None` before `setWorkspace`.
    fn current(&self) -> Option<&WorkspaceRuns> {
        self.rust()
            .by_workspace
            .get(&self.rust().workspace_id.to_string())
    }
}
