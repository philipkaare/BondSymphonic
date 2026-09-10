//! One Claude agent tab: the transcript, its running cost, and the permission
//! request waiting for an answer.
//!
//! The model owns a [`Transcript`] on the Qt side and one task on the tokio
//! side. `attach` subscribes to the agent's events *before* asking for
//! `agent.history`, so nothing said while the history request is in flight is
//! lost; the history and whatever arrived meanwhile are applied together in one
//! closure on the Qt thread, and only then does the live loop start. `seq`
//! de-duplication in [`Transcript::apply`] makes the overlap harmless.
//!
//! The replay itself is [`Transcript::replay`], a pure fold that this object
//! only drives, so the ordering and the de-duplication are testable without a
//! Qt event loop. It emits no per-item signal at all: the view rebuilds once
//! from `resetItems()`, and the properties are published once at the end.
//! After that, one signal per applied message.
//!
//! Permissions are decided here, not in C++: a request the session has already
//! been told to always allow is answered by this model without ever reaching
//! the permission bar.

use crate::model::app_state::agent_state_word;
use crate::model::transcript::{Applied, LiveEvent, Transcript};
use crate::qobjects::app_controller::{on_reconnect, require_connection, runtime, Shared};
use bondsymphonic_proto::{
    AgentId, AgentIdParams, AgentMessage, AgentPermissionReplyParams, AgentSendParams, AgentState,
    Event, HistoryResult, PermissionDecision, Request,
};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `pending_json` is exposed as `getPendingJson`/`pendingJsonChanged` and
    // `set_collapsed` as `setCollapsed`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        // `state` is the daemon's snake_case agent state; `state_detail` its
        // explanation, or empty. `pending_json` is the permission request the
        // bar is showing, or empty. `busy` is true only while history is
        // replaying, which is what greys the input box.
        #[qobject]
        #[qproperty(QString, agent_id)]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, state)]
        #[qproperty(QString, state_detail)]
        #[qproperty(f64, cost_usd)]
        #[qproperty(i32, turns)]
        #[qproperty(QString, pending_json)]
        #[qproperty(bool, busy)]
        // The `AgentStartOptions` the tab was created with, as the New Agent
        // dialog built them, or empty. The model never sends them anywhere; it
        // holds them so `restartOptionsJson` can hand them back with the
        // session filled in. Never the API key: that is merged into the
        // request by `AppController` and crosses this boundary nowhere.
        #[qproperty(QString, options_json)]
        type TranscriptModel = super::TranscriptModelRust;

        /// One item was added at `index`; append a frame from `itemJson(index)`.
        #[qsignal]
        fn item_appended(self: Pin<&mut TranscriptModel>, index: i32);

        /// The item at `index` changed; update that frame in place.
        #[qsignal]
        fn item_changed(self: Pin<&mut TranscriptModel>, index: i32);

        /// The whole list was replaced; rebuild from `itemsJson()`.
        #[qsignal]
        fn reset_items(self: Pin<&mut TranscriptModel>);

        /// A tool call is waiting for the user; `pendingJson` describes it.
        #[qsignal]
        fn permission_requested(self: Pin<&mut TranscriptModel>);

        /// Nothing is waiting any more; hide the permission bar.
        #[qsignal]
        fn permission_cleared(self: Pin<&mut TranscriptModel>);

        /// A request to the daemon failed. The transcript is unchanged.
        #[qsignal]
        fn error_occurred(self: Pin<&mut TranscriptModel>, message: QString);

        /// Points this model at an agent: subscribes, replays
        /// `agent.history`, then follows the live stream. Re-attaching drops
        /// the previous agent's subscription and clears the transcript. An
        /// empty `agent_id` detaches.
        #[qinvokable]
        fn attach(self: Pin<&mut TranscriptModel>, workspace_id: QString, agent_id: QString);

        /// Sends a prompt. The user message comes back as an `agent.message`,
        /// so nothing is appended locally.
        #[qinvokable]
        fn send(self: Pin<&mut TranscriptModel>, text: QString);

        /// Answers the pending permission request. `always_allow` (with
        /// `allow`) records the tool name so later requests for it are
        /// answered without asking. `message` is an optional reason, sent only
        /// when non-empty.
        #[qinvokable]
        fn reply(
            self: Pin<&mut TranscriptModel>,
            request_id: QString,
            allow: bool,
            always_allow: bool,
            message: QString,
        );

        /// Interrupts the current turn; the agent stays alive.
        #[qinvokable]
        fn interrupt(self: Pin<&mut TranscriptModel>);

        /// Stops the agent process.
        #[qinvokable]
        fn stop(self: Pin<&mut TranscriptModel>);

        /// Every item as a JSON array, for a full rebuild.
        #[qinvokable]
        fn items_json(self: &TranscriptModel) -> QString;

        /// One item as JSON, or empty for an index that does not exist.
        #[qinvokable]
        fn item_json(self: &TranscriptModel, index: i32) -> QString;

        /// Collapses or expands one tool card. Emits `itemChanged` when the
        /// item is a tool call and the value actually changed.
        #[qinvokable]
        fn set_collapsed(self: Pin<&mut TranscriptModel>, index: i32, collapsed: bool);

        /// The options a Restart should start the new agent with: this tab's
        /// own `optionsJson`, plus `resume_session` set to the last session id
        /// this transcript has seen.
        ///
        /// That is what makes Restart continue the conversation rather than
        /// begin a new one, which matters most after a daemon restart: the
        /// agent comes back as an `exited` record whose history is still
        /// readable, and the session id in that history is the one Claude
        /// resumes from. With no session id seen (a tab that never got as far
        /// as an init message) the key is left out entirely rather than sent
        /// as null, so the daemon starts a fresh session.
        #[qinvokable]
        fn restart_options_json(self: &TranscriptModel) -> QString;
    }

    impl cxx_qt::Threading for TranscriptModel {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::TranscriptModel>;

/// Ends this model's router subscription. Held on the Qt side so that
/// re-attaching, or dropping the object, releases it even though the task that
/// would otherwise do it is aborted rather than run to completion.
type Unsubscribe = Box<dyn FnOnce() + Send>;

pub struct TranscriptModelRust {
    agent_id: QString,
    workspace_id: QString,
    state: QString,
    state_detail: QString,
    cost_usd: f64,
    turns: i32,
    pending_json: QString,
    busy: bool,
    options_json: QString,
    /// The state of record. Every property above is derived from it.
    transcript: Transcript,
    /// Replay plus the live loop, aborted on re-attach and on Drop.
    task: Option<tokio::task::JoinHandle<()>>,
    /// Waits for the connection generation to move and then re-attaches this
    /// pane to the daemon that came back. Replaced on every `attach` and
    /// aborted on Drop, so a closed tab does not re-attach itself.
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
    unsubscribe: Option<Unsubscribe>,
    /// Bumped on every `attach`, so a reply for an earlier agent that lands
    /// late is dropped instead of being folded into the new transcript.
    generation: u64,
}

impl Default for TranscriptModelRust {
    fn default() -> Self {
        let transcript = Transcript::default();
        Self {
            agent_id: QString::from(""),
            workspace_id: QString::from(""),
            state: QString::from(agent_state_word(transcript.state)),
            state_detail: QString::from(""),
            cost_usd: 0.0,
            turns: 0,
            pending_json: QString::from(""),
            busy: false,
            options_json: QString::from(""),
            transcript,
            task: None,
            reconnect_task: None,
            unsubscribe: None,
            generation: 0,
        }
    }
}

impl Drop for TranscriptModelRust {
    fn drop(&mut self) {
        // The tab went away. `Drop` runs on the Rust struct, not the QObject,
        // so it repeats what `detach` does rather than calling it. The agent
        // itself keeps running: closing a transcript is not stopping an agent.
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(task) = self.reconnect_task.take() {
            task.abort();
        }
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

/// Replays `agent.history` and then follows the live stream until the
/// subscription ends or the QObject goes away.
///
/// `rx` is subscribed by the caller, on the Qt thread, before this task
/// starts: that is what guarantees the window between the subscription and the
/// history read is covered rather than merely small.
async fn replay_and_follow(
    shared: Shared,
    qt: QtHandle,
    agent: AgentId,
    generation: u64,
    mut rx: crate::client::router::EventRx,
) {
    let params = AgentIdParams {
        agent_id: agent.clone(),
    };
    let history = match shared
        .client
        .request::<HistoryResult>(Request::AgentHistory(params))
        .await
    {
        Ok(res) => res,
        Err(e) => {
            // The live stream is still worth following: a history that could
            // not be read costs the tab its scrollback, not its agent. Only
            // the error is raised here; the replay closure below is the one
            // place that ends `busy` and rebuilds the view, so a live event
            // that arrived meanwhile cannot be painted before that rebuild.
            let message = format!("agent.history failed: {e}");
            tracing::warn!("{message}");
            let _ = qt.queue(move |q| {
                if q.as_ref().rust().generation != generation {
                    return;
                }
                q.fail(&message);
            });
            HistoryResult {
                messages: Vec::new(),
                state: AgentState::Idle,
                detail: None,
            }
        }
    };

    // Everything that arrived while the history request was in flight, folded
    // in the same closure as the history itself so the view rebuilds once.
    let mut buffered = Vec::new();
    while let Ok((_, event)) = rx.try_recv() {
        if let Some(live) = live_event(event, &agent) {
            buffered.push(live);
        }
    }
    let queued = qt.queue(move |mut q| {
        if q.as_ref().rust().generation != generation {
            return;
        }
        {
            let mut rust = q.as_mut().rust_mut();
            // The daemon's state first, then the fold. State changes are events
            // rather than transcript entries, so every one of them predates
            // this attachment; without this the fold would end on `Idle` and a
            // running agent would paint as finished, an exited one would say
            // nothing, and a genuinely open permission request would have its
            // bar taken down by the guard at the end of `replay`.
            rust.transcript
                .set_state_from_history(history.state, history.detail.clone());
            rust.transcript.replay(&history.messages, &buffered);
        }
        // Once, at the end: replaying a long history through the per-message
        // path would emit four property notifications per item.
        q.as_mut().publish_totals();
        q.as_mut().set_busy(false);
        q.as_mut().reset_items();
        // The fold raises nothing itself, so a history ending on an unanswered
        // request reaches the bar (or is auto-answered) here.
        q.sync_pending();
    });
    if queued.is_err() {
        shared.router.unsubscribe_agent(&agent);
        return;
    }

    while let Some((_, event)) = rx.recv().await {
        let queued = qt.queue(move |q| {
            if q.as_ref().rust().generation != generation {
                return;
            }
            q.apply_event(event);
        });
        if queued.is_err() {
            break;
        }
    }
    shared.router.unsubscribe_agent(&agent);
}

impl qobject::TranscriptModel {
    pub fn attach(mut self: Pin<&mut Self>, workspace_id: QString, agent_id: QString) {
        self.as_mut().detach();
        let generation = {
            let mut rust = self.as_mut().rust_mut();
            rust.generation += 1;
            rust.transcript = Transcript::default();
            rust.generation
        };
        self.as_mut().set_workspace_id(workspace_id);
        self.as_mut().set_agent_id(agent_id.clone());
        self.as_mut().set_pending_json(QString::from(""));
        self.as_mut().set_busy(true);
        self.as_mut().publish_totals();

        let agent = agent_id.to_string();
        if agent.is_empty() {
            // Detaching: an empty transcript, and no request to make. No
            // reconnect watch either -- there is nothing to come back to.
            self.as_mut().set_busy(false);
            self.reset_items();
            return;
        }
        // Armed before the connection is even required. A pane that could not
        // attach because there was no connection is precisely the pane a
        // reconnect has to bring back, so this must not sit behind the check
        // below.
        {
            let qt = self.as_ref().qt_thread();
            let watch = on_reconnect(qt, qobject::TranscriptModel::reattach);
            self.as_mut().rust_mut().reconnect_task = Some(watch);
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.as_mut().set_busy(false);
                self.as_mut().reset_items();
                self.fail(message);
                return;
            }
        };
        // Subscribed here, on the Qt thread, before `agent.history` goes out:
        // messages the agent produces meanwhile are parked in the channel and
        // applied after the history, not missed.
        let agent = AgentId(agent);
        let rx = shared.router.subscribe_agent(&agent);
        let unsubscribe = {
            let router = shared.router.clone();
            let agent = agent.clone();
            move || router.unsubscribe_agent(&agent)
        };
        let qt = self.as_ref().qt_thread();
        let task = runtime().spawn(replay_and_follow(shared, qt, agent, generation, rx));
        {
            let mut rust = self.as_mut().rust_mut();
            rust.task = Some(task);
            rust.unsubscribe = Some(Box::new(unsubscribe));
        }
        self.reset_items();
    }

    pub fn send(self: Pin<&mut Self>, text: QString) {
        let text = text.to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some((shared, agent, qt)) = self.connection() else {
            return;
        };
        runtime().spawn(async move {
            let params = AgentSendParams {
                agent_id: agent,
                text,
            };
            if let Err(e) = shared.client.request_raw(Request::AgentSend(params)).await {
                report(&qt, format!("agent.send failed: {e}"));
            }
        });
    }

    pub fn reply(
        mut self: Pin<&mut Self>,
        request_id: QString,
        allow: bool,
        always_allow: bool,
        message: QString,
    ) {
        let request_id = request_id.to_string();
        // Only the request the bar is actually showing may be taken down by
        // this click. A late answer to a superseded request still goes to the
        // daemon, which knows the id, but must not hide a newer request that
        // is still waiting, nor allowlist that newer request's tool.
        let answers_pending = self
            .as_ref()
            .rust()
            .transcript
            .pending
            .as_ref()
            .is_some_and(|p| p.request_id == request_id);
        let tool = answers_pending
            .then(|| {
                self.as_ref()
                    .rust()
                    .transcript
                    .pending
                    .as_ref()
                    .map(|p| p.tool_name.clone())
            })
            .flatten();
        // "Always allow" only means anything alongside Allow: there is no
        // "always deny" in the permission bar.
        if allow && always_allow {
            if let Some(name) = tool {
                self.as_mut()
                    .rust_mut()
                    .transcript
                    .always_allow
                    .insert(name);
            }
        }
        if answers_pending {
            self.as_mut().rust_mut().transcript.pending = None;
            self.as_mut().sync_pending();
        }
        let decision = if allow {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny
        };
        let message = message.to_string();
        self.send_permission_reply(
            request_id,
            decision,
            (!message.is_empty()).then_some(message),
        );
    }

    pub fn interrupt(self: Pin<&mut Self>) {
        self.agent_request(Request::AgentInterrupt, "agent.interrupt");
    }

    pub fn stop(self: Pin<&mut Self>) {
        self.agent_request(Request::AgentStop, "agent.stop");
    }

    pub fn items_json(&self) -> QString {
        QString::from(&self.rust().transcript.items_json())
    }

    pub fn item_json(&self, index: i32) -> QString {
        match usize::try_from(index) {
            Ok(i) => QString::from(&self.rust().transcript.item_json(i)),
            Err(_) => QString::from(""),
        }
    }

    pub fn restart_options_json(&self) -> QString {
        QString::from(&restart_options(
            &self.options_json().to_string(),
            self.rust().transcript.session_id.as_deref(),
        ))
    }

    pub fn set_collapsed(mut self: Pin<&mut Self>, index: i32, collapsed: bool) {
        let Ok(i) = usize::try_from(index) else {
            return;
        };
        if self
            .as_mut()
            .rust_mut()
            .transcript
            .set_collapsed(i, collapsed)
        {
            self.item_changed(index);
        }
    }

    /// Folds one live message in and announces what changed. Only the live
    /// path comes through here: replay goes through [`Transcript::replay`] and
    /// ends in a single `resetItems()`, so this never runs while `busy`.
    fn apply_message(mut self: Pin<&mut Self>, message: &AgentMessage) {
        let applied = self.as_mut().rust_mut().transcript.apply(message);
        self.as_mut().publish_totals();
        match applied {
            Applied::Appended(i) => self.as_mut().item_appended(index_of(i)),
            Applied::Changed(i) => self.as_mut().item_changed(index_of(i)),
            Applied::Nothing => {}
        }
        self.sync_pending();
    }

    /// Applies one routed event. Anything that is not this agent's business is
    /// ignored rather than trusted: the router filters by id, but the model is
    /// the thing that would be corrupted by a mistake there.
    fn apply_event(mut self: Pin<&mut Self>, event: Event) {
        let agent = AgentId(self.as_ref().agent_id().to_string());
        let Some(live) = live_event(event, &agent) else {
            return;
        };
        match live {
            LiveEvent::Message(message) => self.apply_message(&message),
            LiveEvent::State(state, detail) => {
                self.as_mut().rust_mut().transcript.set_state(state, detail);
                self.as_mut().publish_totals();
                self.sync_pending();
            }
        }
    }

    /// Republishes every property derived from the transcript.
    fn publish_totals(mut self: Pin<&mut Self>) {
        let (state, detail, cost, turns) = {
            let this = self.as_ref();
            let t = &this.rust().transcript;
            (
                agent_state_word(t.state),
                t.state_detail.clone(),
                t.cost_usd,
                i32::try_from(t.turns).unwrap_or(i32::MAX),
            )
        };
        self.as_mut().set_state(QString::from(state));
        self.as_mut().set_state_detail(QString::from(&detail));
        self.as_mut().set_cost_usd(cost);
        self.as_mut().set_turns(turns);
    }

    /// Publishes, answers or clears the pending permission request.
    ///
    /// A tool the user has already said to always allow is answered here and
    /// never reaches the bar, which is the whole point of the checkbox: the
    /// decision is Rust's, not the view's. Idempotent, so it can be called
    /// after every applied message without re-raising the same request.
    fn sync_pending(mut self: Pin<&mut Self>) {
        let auto = self.as_mut().rust_mut().transcript.take_auto_allowed();
        let current = self.as_ref().pending_json().to_string();
        if let Some((pending, decision)) = auto {
            if !current.is_empty() {
                self.as_mut().set_pending_json(QString::from(""));
                self.as_mut().permission_cleared();
            }
            self.send_permission_reply(pending.request_id, decision, None);
            return;
        }
        let Some(pending) = self.as_ref().rust().transcript.pending.clone() else {
            if !current.is_empty() {
                self.as_mut().set_pending_json(QString::from(""));
                self.permission_cleared();
            }
            return;
        };
        let json = serde_json::to_string(&pending).unwrap_or_default();
        if json != current {
            self.as_mut().set_pending_json(QString::from(&json));
            self.permission_requested();
        }
    }

    fn send_permission_reply(
        self: Pin<&mut Self>,
        request_id: String,
        decision: PermissionDecision,
        message: Option<String>,
    ) {
        let Some((shared, agent, qt)) = self.connection() else {
            return;
        };
        runtime().spawn(async move {
            let params = AgentPermissionReplyParams {
                agent_id: agent,
                request_id,
                decision,
                updated_input: None,
                message,
            };
            if let Err(e) = shared
                .client
                .request_raw(Request::AgentPermissionReply(params))
                .await
            {
                report(&qt, format!("agent.permission_reply failed: {e}"));
            }
        });
    }

    /// Issues one of the fire-and-forget `agent.*` requests that take nothing
    /// but the agent id. The answer is the state change the daemon broadcasts.
    fn agent_request(self: Pin<&mut Self>, build: fn(AgentIdParams) -> Request, op: &'static str) {
        let Some((shared, agent, qt)) = self.connection() else {
            return;
        };
        runtime().spawn(async move {
            let params = AgentIdParams { agent_id: agent };
            if let Err(e) = shared.client.request_raw(build(params)).await {
                report(&qt, format!("{op} failed: {e}"));
            }
        });
    }

    /// The three things every request needs, or `None` after raising the
    /// reason it cannot be made. `None` without an error when no agent is
    /// attached: an empty tab is not a failure.
    fn connection(mut self: Pin<&mut Self>) -> Option<(Shared, AgentId, QtHandle)> {
        let agent = self.as_ref().agent_id().to_string();
        if agent.is_empty() {
            return None;
        }
        match require_connection() {
            Ok(shared) => {
                let qt = self.as_ref().qt_thread();
                Some((shared, AgentId(agent), qt))
            }
            Err(message) => {
                self.as_mut().fail(message);
                None
            }
        }
    }

    /// Ends the current agent's subscription and stops its tasks. Called
    /// before `attach` replaces the agent, and mirrored by `Drop`.
    fn detach(mut self: Pin<&mut Self>) {
        if let Some(task) = self.as_mut().rust_mut().task.take() {
            task.abort();
        }
        if let Some(task) = self.as_mut().rust_mut().reconnect_task.take() {
            task.abort();
        }
        if let Some(unsubscribe) = self.as_mut().rust_mut().unsubscribe.take() {
            unsubscribe();
        }
    }

    /// Points the model at the same agent again, on the connection that is
    /// live now. Queued by [`on_reconnect`] after a daemon restart.
    ///
    /// `attach` does the whole job: it subscribes on the new router, replays
    /// `agent.history` and takes the state from it. After a restart that state
    /// is `exited` (the daemon reloaded the agent as a record), so the pane
    /// comes back with its conversation intact and a Restart button, which is
    /// what `restartOptionsJson` is for.
    fn reattach(mut self: Pin<&mut Self>) {
        let agent = self.as_ref().agent_id().clone();
        if agent.to_string().is_empty() {
            return;
        }
        let workspace = self.as_ref().workspace_id().clone();
        tracing::info!("transcript re-attaching to {agent} after a reconnect");
        self.as_mut().attach(workspace, agent);
    }

    /// Raises a failure on the view. The transcript is never changed by one:
    /// an error is news about the request, not about the conversation.
    fn fail(self: Pin<&mut Self>, message: &str) {
        self.error_occurred(QString::from(message));
    }
}

/// The tab's start options with `resume_session` set to `session`, as JSON.
///
/// Pure, so the merge is testable without an agent or a Qt event loop. The
/// options travel as JSON rather than as `AgentStartOptions` because that is
/// what the tab holds and what `AppController::startAgent` takes back, so the
/// merge stays a merge: it changes one key and copies the rest through
/// verbatim. A key this build has never heard of survives *this* step, though
/// not the whole trip -- `AppController::start_options` parses the string into
/// `AgentStartOptions` before it builds the request, and that is where an
/// unknown field is dropped. Widening what a tab can carry is a proto change,
/// not something to work around here.
///
/// A string that is not a JSON object is replaced by one rather than refused:
/// the point of the call is to produce options that can start an agent, and
/// the daemon's own defaults are a working agent. `None` for the session
/// removes the key instead of writing null, so the request asks for a fresh
/// session rather than for one named "nothing".
pub fn restart_options(options_json: &str, session: Option<&str>) -> String {
    let mut options = serde_json::from_str::<serde_json::Value>(options_json)
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = options.as_object_mut() {
        match session {
            Some(session) => {
                map.insert(
                    "resume_session".to_owned(),
                    serde_json::Value::String(session.to_owned()),
                );
            }
            None => {
                map.remove("resume_session");
            }
        }
    }
    options.to_string()
}

/// This agent's half of one routed event, or `None` for anything else. The
/// router filters by id, but the model is the thing a mistake there would
/// corrupt, so the id is checked again here.
fn live_event(event: Event, agent: &AgentId) -> Option<LiveEvent> {
    match event {
        Event::AgentMessage { agent_id, message } if &agent_id == agent => {
            Some(LiveEvent::Message(message))
        }
        Event::AgentStateChanged {
            agent_id,
            state,
            detail,
        } if &agent_id == agent => Some(LiveEvent::State(state, detail)),
        _ => None,
    }
}

/// A transcript index as Qt sees it. Indices come from a `Vec` the same
/// process built, so saturating is a formality rather than a real case.
fn index_of(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Surfaces a tokio-side failure on the model's `errorOccurred` signal.
fn report(qt: &QtHandle, message: String) {
    tracing::warn!("{message}");
    let _ = qt.queue(move |q| q.fail(&message));
}
