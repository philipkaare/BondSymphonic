//! The tab model: groups of agent tabs, exposed to the Qt widgets as one
//! serialised JSON string plus a handful of cheap accessors.
//!
//! [`Workspaces`] is the state of record; the `state_json` property is a
//! cached serialisation of it, refreshed after every mutation so a view can
//! rebuild itself from a single property read. The accessors below exist so
//! the C++ side never has to parse that JSON just to paint a tab: it asks for
//! the label, the tooltip, the status word and the counts directly.

use crate::model::app_state::{
    adapter_from_name, adapter_name, AgentTab, TabStatus, Workspaces, UNSORTED_GROUP,
};
use crate::model::persistence::{PersistedGroup, PersistedGroups};
use crate::model::transcript::parse_agent_state;
use bondsymphonic_proto::{AgentId, WorkspaceId, WorkspaceInfo, WorkspaceState};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `state_json` is exposed as `getStateJson`/`stateJsonChanged`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qproperty(QString, state_json)]
        type GroupModel = super::GroupModelRust;

        /// Emitted after every mutation, once `state_json` has been refreshed.
        /// A view that only wants "something changed" connects to this rather
        /// than to `stateJsonChanged`.
        #[qsignal]
        fn changed(self: Pin<&mut GroupModel>);

        /// Emitted, after `changed`, only when the mutation altered what
        /// `groupsJson` reports: a group's name, which tabs it holds, the order
        /// of either, or which tab is in front.
        ///
        /// The signal exists because `changed` is not a usable trigger for
        /// writing `state.json`. It fires for every status glyph, every agent
        /// heartbeat and every attention mark -- none of which the arrangement
        /// file records -- so a window that recorded the arrangement on
        /// `changed` re-read, re-serialised and re-scheduled a write of the
        /// whole file dozens of times a minute while nothing about it had
        /// changed. This fires when there is something to write.
        #[qsignal]
        fn arrangement_changed(self: Pin<&mut GroupModel>);

        /// Replaces the whole model from a serialised `Workspaces`. An
        /// unparseable or empty string installs the default single-group model
        /// instead. Callers must use this rather than `setStateJson`, which
        /// only overwrites the cached string.
        ///
        /// The one caller is `GroupBar`'s menu-test seam, which moves the model
        /// while a context menu is up exactly as a daemon event would. Nothing
        /// reaches it unless `BS_MENU_TEST` armed it.
        #[qinvokable]
        fn load_state(self: Pin<&mut GroupModel>, json: QString) -> bool;

        /// Installs the model the controller rebuilt from `state.json` and the
        /// daemon's first workspace list (`AppController::workspacesRestored`),
        /// before any `reconcile`.
        ///
        /// Distinct from `loadState`, which falls back to the default
        /// single-group model. A restore that will not parse must leave what is
        /// on screen alone instead: the alternative is throwing away the user's
        /// groups because one string was malformed.
        #[qinvokable]
        fn load_workspaces(self: Pin<&mut GroupModel>, json: QString) -> bool;

        /// The arrangement to persist: an object with `groups` (each a name
        /// and its workspace ids, in order) and `active_workspace`. What the
        /// window hands to `AppController::noteGroups` on every `changed`.
        #[qinvokable]
        fn groups_json(self: &GroupModel) -> QString;

        /// Appends an empty group and returns its index.
        #[qinvokable]
        fn add_group(self: Pin<&mut GroupModel>, name: QString) -> i32;

        #[qinvokable]
        fn rename_group(self: Pin<&mut GroupModel>, idx: i32, name: QString) -> bool;

        /// Adds a tab for the `WorkspaceInfo` in `info_json` to the group named
        /// `group_name`, creating that group if it does not exist, and makes it
        /// active. An empty `group_name` is not a group called "": it means the
        /// caller named none, and the new tab is filed under "Unsorted" --
        /// where `AppController::noteGroups` records it too. A workspace that
        /// is already tracked is refreshed in place instead of being
        /// duplicated, and an empty `group_name` then leaves it in whatever
        /// group it is already in rather than dragging it out of one the user
        /// chose. `adapter` is "claude" or "terminal";
        /// an empty `command` means the adapter default. `options_json` is the
        /// `AgentStartOptions` a Claude tab was started with, kept on the tab so
        /// the agent can be started again with them; empty for anything else.
        #[qinvokable]
        fn add_tab(
            self: Pin<&mut GroupModel>,
            info_json: QString,
            group_name: QString,
            adapter: QString,
            command: QString,
            options_json: QString,
        ) -> bool;

        /// Refreshes status, branch and detail from a `WorkspaceInfo`. False
        /// when the JSON is unparseable or the workspace is not tracked.
        #[qinvokable]
        fn apply_workspace_info(self: Pin<&mut GroupModel>, info_json: QString) -> bool;

        /// Syncs against the daemon authoritative list (a JSON array of
        /// `WorkspaceInfo`): drops tabs the daemon no longer has and files
        /// unknown workspaces into an "Unsorted" group.
        #[qinvokable]
        fn reconcile(self: Pin<&mut GroupModel>, list_json: QString);

        /// Records the agent now running in the tab for `workspace_id`, so
        /// later `agent.state` events can find that tab. False when the
        /// workspace is not tracked.
        #[qinvokable]
        fn set_agent(self: Pin<&mut GroupModel>, workspace_id: QString, agent_id: QString) -> bool;

        /// Applies an `agent.state` event. `state` is the daemon's snake_case
        /// word ("idle", "working", "waiting_permission", "error", "exited")
        /// and `detail` its explanation, or empty. False when no tab is
        /// running that agent or the word is not one of the five.
        #[qinvokable]
        fn set_agent_status(
            self: Pin<&mut GroupModel>,
            agent_id: QString,
            state: QString,
            detail: QString,
        ) -> bool;

        #[qinvokable]
        fn remove_workspace(self: Pin<&mut GroupModel>, id: QString) -> bool;

        /// Marks `workspace_id`'s tab as carrying a failed merge, pull request
        /// or discard: the tab reads as an error, with `detail` as its
        /// tooltip, until `clearWorkspaceError`.
        ///
        /// Per workspace, not per agent: a merge conflict says nothing about
        /// whatever is running in the sandbox, and a terminal tab with no agent
        /// at all can still fail to merge. The daemon's own status carries on
        /// underneath, so clearing gives the badge straight back to it. False
        /// when the workspace is not tracked.
        #[qinvokable]
        fn set_workspace_error(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            detail: QString,
        ) -> bool;

        /// Takes that mark off again. False when the workspace is not tracked.
        #[qinvokable]
        fn clear_workspace_error(self: Pin<&mut GroupModel>, workspace_id: QString) -> bool;

        /// Marks `workspace_id`'s tab as wanting the user, with `text` as the
        /// sentence the tab's tooltip and the status bar show. What the window
        /// calls when an agent in a tab that is *not* in front asks to be
        /// allowed a tool: the permission bar is on that workspace's own pane
        /// and cannot be seen from here.
        ///
        /// False for a workspace this model does not track, and for one that
        /// already carries exactly this text -- so a caller that marks on every
        /// state event does not repaint the tab bar for nothing.
        #[qinvokable]
        fn set_workspace_attention(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            text: QString,
        ) -> bool;

        /// Takes the mark off again: the question was answered, or the user
        /// switched to that tab and can see it. False when there was none.
        #[qinvokable]
        fn clear_workspace_attention(self: Pin<&mut GroupModel>, workspace_id: QString) -> bool;

        /// Re-decides attention across a tab change: the newly active tab stops
        /// asking, and `previous_workspace_id` -- the tab just left -- starts,
        /// if its agent is still blocked on a permission request. The empty
        /// string means there was no previous tab.
        ///
        /// The window calls this instead of clearing by hand, because the case
        /// the feature exists for produces no `agent.state` event at all: an
        /// agent asks while its own tab is in front, and the user then switches
        /// away. Nothing about the agent changed, so nothing would fire.
        ///
        /// False when nothing moved, so a tab change with no question open does
        /// not republish the model.
        #[qinvokable]
        fn refresh_attention(self: Pin<&mut GroupModel>, previous_workspace_id: QString) -> bool;

        /// The sentence for the status bar -- the first tab, in the user's own
        /// order, that is asking for something -- or empty when none is.
        #[qinvokable]
        fn attention_text(self: &GroupModel) -> QString;

        /// What `setWorkspaceAttention` should be given for an agent that is
        /// blocked on a permission request, for the workspace called `name`.
        ///
        /// Here rather than in the window so the dot's tooltip and the status
        /// bar's line are one string built in one place.
        #[qinvokable]
        fn permission_attention(self: &GroupModel, name: QString) -> QString;

        /// Records that `workspace.restart` failed for `workspace_id` with
        /// `reason`: the tab reads as the failed workspace the daemon leaves
        /// behind, and its banner shows the new reason. False when the
        /// workspace is not tracked.
        #[qinvokable]
        fn note_restart_failed(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            reason: QString,
        ) -> bool;

        /// Records a warning the daemon sent about `workspace_id`, whose first
        /// line is a sentence and whose remaining lines explain it. False for a
        /// message with nothing under its first line, which is kept nowhere.
        ///
        /// The workspace need not have a tab: a warning is remembered by id and
        /// put on the banner when the state it explains arrives, whichever of
        /// the two the daemon sends first.
        #[qinvokable]
        fn note_workspace_warning(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            message: QString,
        ) -> bool;

        /// Whether `workspace_id` is a Claude tab whose agent is missing or has
        /// ended, which is what a successful Retry starts again. False for an
        /// unknown workspace.
        #[qinvokable]
        fn agent_needs_start(self: &GroupModel, workspace_id: QString) -> bool;

        /// The workspace whose tab is running `agent_id`, or empty. The window
        /// gets agent state events keyed by agent, and the attention calls
        /// above are keyed by workspace.
        #[qinvokable]
        fn agent_workspace_id(self: &GroupModel, agent_id: QString) -> QString;

        /// `workspace_id`'s tab name, or empty. The window builds the
        /// attention sentence out of it.
        #[qinvokable]
        fn workspace_name(self: &GroupModel, workspace_id: QString) -> QString;

        /// Whether `workspace_id` works directly in its repository's checkout.
        /// The window words Close and the menus from it; false for a workspace
        /// the model does not track.
        #[qinvokable]
        fn workspace_in_place(self: &GroupModel, workspace_id: QString) -> bool;

        /// Removes the group named `name`, moving whatever tabs it still holds
        /// into "Unsorted". What "Close group" does once the per-workspace
        /// merges and discards it asked for have all succeeded.
        ///
        /// False for a name no group has, and for "Unsorted" itself: its tabs
        /// would have nowhere to go and the next `reconcile` would only make it
        /// again.
        #[qinvokable]
        fn remove_group(self: Pin<&mut GroupModel>, name: QString) -> bool;

        #[qinvokable]
        fn set_active(self: Pin<&mut GroupModel>, group_idx: i32, tab_idx: i32) -> bool;

        /// The active tab as JSON (an `AgentTab`), or empty when there is none.
        #[qinvokable]
        fn active_tab_json(self: &GroupModel) -> QString;

        #[qinvokable]
        fn active_group_index(self: &GroupModel) -> i32;

        #[qinvokable]
        fn active_tab_index(self: &GroupModel) -> i32;

        #[qinvokable]
        fn group_count(self: &GroupModel) -> i32;

        /// Number of tabs in `group_idx`, or 0 for an out-of-range group.
        #[qinvokable]
        fn tab_count(self: &GroupModel, group_idx: i32) -> i32;

        #[qinvokable]
        fn group_name(self: &GroupModel, idx: i32) -> QString;

        /// The glyph and name joined by a space, for the tab bar; empty for an
        /// unknown tab.
        #[qinvokable]
        fn tab_label(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The tab [`TabStatus`] as its stable numeric code, or -1.
        #[qinvokable]
        fn tab_status(self: &GroupModel, group_idx: i32, tab_idx: i32) -> i32;

        /// The workspace's own status as a word ("idle", "working", "waiting",
        /// "error", "done", "creating", "sandbox down"), so the C++ side never
        /// switches on the numeric code. Empty for an unknown tab.
        ///
        /// The daemon's status, never the operation-error overlay `tabStatus`
        /// carries: the status bar frames this as "sandbox: <word>", and a
        /// merge that conflicted says nothing about the sandbox. The red glyph
        /// for that lives on the tab, which is where it can be dismissed.
        #[qinvokable]
        fn status_word(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// Multi-line tooltip: name, repo, branch, adapter, status, and the
        /// daemon error detail when there is one.
        #[qinvokable]
        fn tab_tooltip(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The workspace id of one tab, or empty for an unknown tab.
        #[qinvokable]
        fn tab_workspace_id(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// Records the run configuration chosen for `workspace_id`, so the Run
        /// panel opens on it whenever that tab becomes active. The empty name
        /// clears it. False when the workspace is not tracked.
        ///
        /// A separate call rather than another `addTab` parameter: the name is
        /// picked in the New Agent dialog and echoed back through
        /// `workspaceCreated`, so it arrives with the tab, but a tab created any
        /// other way must not have to pass one.
        #[qinvokable]
        fn set_tab_run_config(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            run_config: QString,
        ) -> bool;

        /// Replaces the `AgentStartOptions` the tab was created with. False
        /// when the workspace is not tracked, or when `options_json` is empty
        /// -- an empty string is the absence of a choice, and writing it would
        /// turn a model the user picked into "whatever the daemon defaults to".
        ///
        /// What the composer's model and permission dropdowns call. Not for
        /// the sake of the next IDE start: a restored tab reads its options
        /// from the daemon's record of what the agent was actually started
        /// with (see `app_state::restart_options_json`), so the switch already
        /// survives a restart without this. It is for the rest of *this*
        /// session -- Workspace > Restart agent, and everything else that
        /// reads the tab -- which would otherwise keep handing back the model
        /// the workspace was created with and quietly undo the switch on the
        /// next restart.
        #[qinvokable]
        fn set_tab_options(
            self: Pin<&mut GroupModel>,
            workspace_id: QString,
            options_json: QString,
        ) -> bool;
    }
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;

pub struct GroupModelRust {
    state_json: QString,
    workspaces: Workspaces,
    /// The arrangement as `groupsJson` last reported it.
    ///
    /// Kept so [`qobject::GroupModel::publish`] can tell a change to the groups
    /// from a change to a tab's status without every mutation having to declare
    /// which it was -- a declaration that would be wrong the first time someone
    /// added a mutation and copied the wrong neighbour. Maintained in exactly
    /// one place, and only when it actually moves.
    arrangement: Arrangement,
    /// `agent.state` events that arrived before any tab was running the agent
    /// they name. See [`HeldAgentStates`].
    held_states: HeldAgentStates,
}

impl Default for GroupModelRust {
    fn default() -> Self {
        let workspaces = Workspaces::new_default();
        Self {
            state_json: QString::from(&workspaces.to_json()),
            arrangement: arrangement_of(&workspaces),
            held_states: HeldAgentStates::default(),
            workspaces,
        }
    }
}

/// What `groupsJson` reports: every group by name with its workspace ids in
/// order, and the workspace whose tab is in front.
pub type Arrangement = (Vec<PersistedGroup>, Option<String>);

/// The arrangement of a model, which is the whole of what `state.json` records
/// about the tab bar and the only thing `arrangementChanged` fires on.
pub fn arrangement_of(workspaces: &Workspaces) -> Arrangement {
    (
        workspaces.persisted_groups(),
        workspaces.active().map(|tab| tab.workspace_id.0.clone()),
    )
}

/// The group a tab belongs in when the caller named one, and [`UNSORTED_GROUP`]
/// when it did not.
///
/// An empty name is not the name of a group: it means the caller supplied none.
/// Making one anyway produced a nameless strip in the tab bar that the user
/// could not have created and could not remove, while
/// `AppController::note_workspace_created` recorded the very same workspace
/// under "Unsorted" -- so the tab the user saw and the entry saved for it
/// disagreed, and the next restart moved the tab. Both callers route through
/// this, which is what keeps them from disagreeing again.
pub fn group_or_unsorted(name: &str) -> &str {
    if name.is_empty() {
        UNSORTED_GROUP
    } else {
        name
    }
}

/// The index of the group a new tab for `group_name` belongs in, creating the
/// group when the model does not have it yet.
pub fn group_index_for(workspaces: &mut Workspaces, group_name: &str) -> usize {
    let name = group_or_unsorted(group_name);
    match workspaces.groups.iter().position(|g| g.name == name) {
        Some(idx) => idx,
        None => workspaces.add_group(name),
    }
}

/// `agent.state` events for agents no tab is running yet, kept until one is.
///
/// `agent.start` answers on the request channel and `agent.state` streams on
/// the event channel, so the daemon can say an agent is working before the IDE
/// has been told which workspace that agent belongs to. The window binds the
/// tab to the agent in its `agentStarted` slot, which calls
/// [`qobject::GroupModel::set_agent`]; until that has run, `set_agent_status`
/// finds no tab and the event is gone. A Claude agent that goes straight to
/// work and stays there emits nothing else until it has finished, so what the
/// user saw was a tab reading "idle" for the whole turn.
///
/// It lives here, beside `set_agent`, rather than on the controller, because
/// this is where a tab learns its agent. A buffer on the controller could only
/// replay through the one call path that happened to go through it, and the
/// window is not the only thing that emits `agentStarted`.
///
/// Bounded, and the bound is the point: every `agent.state` the daemon sends
/// that no tab claims is offered to this, including states for agents this IDE
/// never started, and an agent that never appears in a tab would otherwise hold
/// its entry for the life of the process.
#[derive(Debug, Default)]
pub struct HeldAgentStates {
    /// Oldest first, one entry per agent, each holding the newest state seen.
    held: std::collections::VecDeque<(AgentId, TabStatus, String)>,
}

impl HeldAgentStates {
    /// How many agents are held at once. A handful would cover the race this
    /// exists for; this is generous enough that a daemon with many live agents
    /// cannot push a pending one out before its start answers, and small enough
    /// to be a rounding error beside one workspace list.
    pub const CAPACITY: usize = 64;

    /// Holds the newest state for `agent`, replacing any earlier one.
    pub fn note(&mut self, agent: &AgentId, status: TabStatus, detail: &str) {
        self.held.retain(|(id, _, _)| id != agent);
        self.held
            .push_back((agent.clone(), status, detail.to_owned()));
        while self.held.len() > Self::CAPACITY {
            self.held.pop_front();
        }
    }

    /// Takes the state held for `agent`, if there is one.
    ///
    /// Taken rather than read: it is replayed once, and replaying it a second
    /// time would push a state the tab has already moved past back onto it.
    pub fn take(&mut self, agent: &AgentId) -> Option<(TabStatus, String)> {
        let idx = self.held.iter().position(|(id, _, _)| id == agent)?;
        let (_, status, detail) = self.held.remove(idx)?;
        Some((status, detail))
    }
}

/// The status the daemon's own record for `agent_id` implies, or `None` when
/// the `WorkspaceInfo` says nothing about that agent.
///
/// For a tab rebuilt from `workspace.list`: it carries an agent id taken from
/// `agent_records` and no `agent.state` has ever been seen for it, so the model
/// falls back to `Idle` when the workspace goes `Ready`. The daemon said what
/// the agent was doing in the same message, and painting a working agent's tab
/// idle over its own record is a worse answer than the fallback exists to give.
pub fn agent_record_status(info: &WorkspaceInfo, agent_id: Option<&AgentId>) -> Option<TabStatus> {
    let agent_id = agent_id?;
    info.agent_records
        .iter()
        .find(|record| &record.id == agent_id)
        .map(|record| TabStatus::from_agent_state(&record.state))
}

/// Gives `tab` the status the daemon's own record for its agent implies, when
/// the tab has never heard from that agent itself. True when it did.
///
/// Two conditions, both narrowing. The workspace must be `Ready`, which is the
/// one state in which the badge belongs to the agent at all: a sandbox that is
/// down or a workspace that failed is news about the workspace, and an agent
/// record says nothing about either. And the tab must have no `agent_status`,
/// so a tab that has heard from its agent directly is left alone.
///
/// It writes `agent_status`, not only `status`, and that is the whole reason it
/// is a function rather than four lines at the call site.
/// `Workspaces::refresh_attention` decides the attention bullet and the
/// status-bar sentence from `agent_status` alone, so a tab rebuilt from
/// `workspace.list` whose record says the agent is waiting for permission would
/// otherwise paint the glyph and never the bullet -- and the bullet exists for
/// exactly the tab the user is not looking at. `status` is written alongside
/// because this runs after `Workspaces::apply_workspace_info` has already
/// decided it; every later one derives it from `agent_status` itself.
pub fn adopt_agent_record(tab: &mut AgentTab, info: &WorkspaceInfo) -> bool {
    if !matches!(info.state, WorkspaceState::Ready) {
        return false;
    }
    // An agent the daemon says has ended has ended, whatever the tab last
    // heard: `Exited` is final for an agent id. What a `workspace.restart`
    // answers with is exactly this, and deciding whether to start the agent
    // again must not hang on its `agent.state` having arrived first.
    if tab.agent_status.is_some() {
        if agent_record_status(info, tab.agent_id.as_ref()) != Some(TabStatus::Done)
            || tab.agent_status == Some(TabStatus::Done)
        {
            return false;
        }
        tab.agent_status = Some(TabStatus::Done);
        tab.agent_detail.clear();
        tab.status = TabStatus::Done;
        tab.detail.clear();
        return true;
    }
    let Some(status) = agent_record_status(info, tab.agent_id.as_ref()) else {
        return false;
    };
    tab.agent_status = Some(status);
    tab.status = status;
    true
}

/// Qt hands indices in as `i32`; anything negative is simply out of range.
fn index(value: i32) -> Option<usize> {
    usize::try_from(value).ok()
}

/// What the user chose for a tab, as `workspaceCreated` echoes it back.
///
/// Every field is "not supplied" when empty: the signal carries no adapter for
/// a create that did not pick one, and a tab rebuilt from `workspace.list`
/// already decided its own from the daemon's agent records. Applying them is
/// one function because `addTab` does it on both of its paths -- the tab it
/// creates and the one `reconcile` filed under "Unsorted" first -- and the two
/// must not disagree about what an empty string means.
pub struct TabChoices {
    pub adapter: String,
    pub command: String,
    pub options_json: String,
}

impl TabChoices {
    pub fn apply(&self, tab: &mut AgentTab) {
        if !self.adapter.is_empty() {
            tab.adapter = adapter_from_name(&self.adapter);
        }
        if !self.command.is_empty() {
            tab.command = Some(self.command.clone());
        }
        if !self.options_json.is_empty() {
            tab.options_json = self.options_json.clone();
        }
    }
}

/// One word per status, for the C++ side to show verbatim.
fn status_text(status: TabStatus) -> &'static str {
    match status {
        TabStatus::Idle => "idle",
        TabStatus::Working => "working",
        TabStatus::WaitingPermission => "waiting",
        TabStatus::Error => "error",
        TabStatus::Done => "done",
        TabStatus::Creating => "creating",
        TabStatus::SandboxDown => "sandbox down",
    }
}

impl qobject::GroupModel {
    pub fn load_state(mut self: Pin<&mut Self>, json: QString) -> bool {
        let parsed = Workspaces::from_json(&json.to_string());
        let ok = parsed.is_some();
        self.as_mut().rust_mut().workspaces = parsed.unwrap_or_else(Workspaces::new_default);
        self.publish();
        ok
    }

    pub fn load_workspaces(mut self: Pin<&mut Self>, json: QString) -> bool {
        let Some(restored) = Workspaces::from_json(&json.to_string()) else {
            tracing::warn!("load_workspaces: unparseable restored model; keeping the current one");
            return false;
        };
        self.as_mut().rust_mut().workspaces = restored;
        self.publish();
        true
    }

    pub fn groups_json(&self) -> QString {
        // Through `arrangement_of`, so what this reports and what
        // `arrangementChanged` fires on are one definition rather than two that
        // can drift.
        let (groups, active_workspace) = arrangement_of(&self.rust().workspaces);
        let reported = PersistedGroups {
            groups,
            active_workspace,
        };
        QString::from(&serde_json::to_string(&reported).unwrap_or_default())
    }

    pub fn add_group(mut self: Pin<&mut Self>, name: QString) -> i32 {
        let idx = self
            .as_mut()
            .rust_mut()
            .workspaces
            .add_group(&name.to_string());
        self.publish();
        idx as i32
    }

    pub fn rename_group(mut self: Pin<&mut Self>, idx: i32, name: QString) -> bool {
        let Some(idx) = index(idx) else {
            return false;
        };
        let ok = self
            .as_mut()
            .rust_mut()
            .workspaces
            .rename_group(idx, &name.to_string());
        if ok {
            self.publish();
        }
        ok
    }

    pub fn add_tab(
        mut self: Pin<&mut Self>,
        info_json: QString,
        group_name: QString,
        adapter: QString,
        command: QString,
        options_json: QString,
    ) -> bool {
        let Ok(info) = serde_json::from_str::<WorkspaceInfo>(&info_json.to_string()) else {
            tracing::warn!("add_tab: unparseable workspace info");
            return false;
        };
        let name = group_name.to_string();
        let chosen = TabChoices {
            adapter: adapter.to_string(),
            command: command.to_string(),
            options_json: options_json.to_string(),
        };
        // A workspace the model already tracks (because `reconcile` filed it
        // into "Unsorted" as a plain terminal before the create call answered)
        // is refreshed, moved into the group the user asked for, and given the
        // adapter and command they chose, rather than added a second time.
        if self.as_ref().rust().workspaces.find(&info.id).is_some() {
            {
                let mut rust = self.as_mut().rust_mut();
                rust.workspaces.apply_workspace_info(&info);
                if !name.is_empty() {
                    rust.workspaces.move_tab_to_group(&info.id, &name);
                }
                if let Some((g, t)) = rust.workspaces.find(&info.id) {
                    chosen.apply(&mut rust.workspaces.groups[g].tabs[t]);
                    rust.workspaces.set_active(g, t);
                }
            }
            self.publish();
            return true;
        }

        // A create that carried no group is filed under "Unsorted", which is
        // where `AppController::note_workspace_created` records it in
        // `state.json` -- not into a group named after the empty string.
        let group_idx = {
            let mut rust = self.as_mut().rust_mut();
            group_index_for(&mut rust.workspaces, &name)
        };
        // Built the one way a tab is ever built from a `WorkspaceInfo`, then
        // given the user's choices by the same three lines the branch above
        // uses. This used to name all seventeen fields, which meant a field
        // added to `AgentTab` was live for a restored tab and silently default
        // for a created one -- exactly how `options_json` came to be missing
        // from the tab of an agent the user had just configured.
        let mut tab = AgentTab::from_workspace_info(&info);
        chosen.apply(&mut tab);
        self.as_mut().rust_mut().workspaces.add_tab(group_idx, tab);
        self.publish();
        true
    }

    pub fn apply_workspace_info(mut self: Pin<&mut Self>, info_json: QString) -> bool {
        let Ok(info) = serde_json::from_str::<WorkspaceInfo>(&info_json.to_string()) else {
            tracing::warn!("apply_workspace_info: unparseable workspace info");
            return false;
        };
        let placed = {
            let mut rust = self.as_mut().rust_mut();
            let placed = rust.workspaces.apply_workspace_info(&info);
            // A tab rebuilt from `workspace.list` has an agent id and has never
            // been sent an `agent.state` for it, so the model hands the badge
            // back as `Idle` the moment the workspace reads `Ready`. The daemon
            // said what that agent is doing in this very message: prefer its
            // record over the fallback, and leave a tab that has heard from its
            // agent directly alone.
            //
            // Only while the workspace itself is `Ready`, which is the one
            // state in which the badge belongs to the agent at all: a sandbox
            // that is down or a workspace that failed is news about the
            // workspace, and an agent record says nothing about either.
            if let Some((g, t)) = placed {
                adopt_agent_record(&mut rust.workspaces.groups[g].tabs[t], &info);
            }
            placed
        };
        let applied = placed.is_some();
        if applied {
            self.publish();
        }
        applied
    }

    pub fn reconcile(mut self: Pin<&mut Self>, list_json: QString) {
        let Ok(list) = serde_json::from_str::<Vec<WorkspaceInfo>>(&list_json.to_string()) else {
            tracing::warn!("reconcile: unparseable workspace list");
            return;
        };
        self.as_mut().rust_mut().workspaces.reconcile(&list);
        self.publish();
    }

    pub fn set_agent(mut self: Pin<&mut Self>, workspace_id: QString, agent_id: QString) -> bool {
        let ws = WorkspaceId(workspace_id.to_string());
        let agent = AgentId(agent_id.to_string());
        let ok = self
            .as_mut()
            .rust_mut()
            .workspaces
            .set_agent(&ws, agent.clone());
        if !ok {
            return false;
        }
        // The tab now knows its agent, so an `agent.state` that overtook the
        // `agent.start` reply finally has somewhere to go. See
        // [`HeldAgentStates`]. Applied before `publish`, so the tab is painted
        // once, already carrying the state, rather than flashing idle first.
        {
            let mut rust = self.as_mut().rust_mut();
            if let Some((status, detail)) = rust.held_states.take(&agent) {
                tracing::info!(
                    "agent {} reported {} before its start was announced; applying it",
                    agent.0,
                    status_text(status)
                );
                rust.workspaces.set_agent_status(&agent, status, &detail);
            }
        }
        self.publish();
        true
    }

    pub fn set_agent_status(
        mut self: Pin<&mut Self>,
        agent_id: QString,
        state: QString,
        detail: QString,
    ) -> bool {
        let word = state.to_string();
        let Some(agent_state) = parse_agent_state(&word) else {
            tracing::warn!("set_agent_status: unknown agent state {word:?}");
            return false;
        };
        let agent = AgentId(agent_id.to_string());
        let status = TabStatus::from_agent_state(&agent_state);
        let detail = detail.to_string();
        let applied = {
            let mut rust = self.as_mut().rust_mut();
            let applied = rust
                .workspaces
                .set_agent_status(&agent, status, &detail)
                .is_some();
            if !applied {
                // No tab is running this agent yet. Held rather than dropped:
                // the `agent.start` that names it may simply not have answered,
                // and `set_agent` replays it the moment a tab claims the agent.
                //
                // Logged because it is the readable half of a race: a tab that
                // does not move when the daemon says its agent has is otherwise
                // a silence, and this line and the one in `set_agent` say
                // between them exactly what happened and when.
                tracing::info!(
                    "agent {} reported {} before any tab was running it; holding it",
                    agent.0,
                    status_text(status)
                );
                rust.held_states.note(&agent, status, &detail);
            }
            applied
        };
        if applied {
            self.publish();
        }
        applied
    }

    pub fn remove_workspace(mut self: Pin<&mut Self>, id: QString) -> bool {
        let id = WorkspaceId(id.to_string());
        let removed = self.as_mut().rust_mut().workspaces.remove_workspace(&id);
        if removed {
            self.publish();
        }
        removed
    }

    pub fn set_workspace_error(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        detail: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let detail = detail.to_string();
        let marked = self
            .as_mut()
            .rust_mut()
            .workspaces
            .set_workspace_error(&id, &detail);
        if marked {
            self.publish();
        }
        marked
    }

    pub fn clear_workspace_error(mut self: Pin<&mut Self>, workspace_id: QString) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let cleared = self
            .as_mut()
            .rust_mut()
            .workspaces
            .clear_workspace_error(&id);
        if cleared {
            self.publish();
        }
        cleared
    }

    pub fn set_workspace_attention(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        text: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let text = text.to_string();
        let marked = self
            .as_mut()
            .rust_mut()
            .workspaces
            .set_workspace_attention(&id, &text);
        if marked {
            self.publish();
        }
        marked
    }

    pub fn clear_workspace_attention(mut self: Pin<&mut Self>, workspace_id: QString) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let cleared = self
            .as_mut()
            .rust_mut()
            .workspaces
            .clear_workspace_attention(&id);
        if cleared {
            self.publish();
        }
        cleared
    }

    pub fn refresh_attention(mut self: Pin<&mut Self>, previous_workspace_id: QString) -> bool {
        let previous = previous_workspace_id.to_string();
        let previous = (!previous.is_empty()).then_some(WorkspaceId(previous));
        let moved = self
            .as_mut()
            .rust_mut()
            .workspaces
            .refresh_attention(previous.as_ref());
        if moved {
            self.publish();
        }
        moved
    }

    pub fn attention_text(&self) -> QString {
        QString::from(self.rust().workspaces.attention().unwrap_or_default())
    }

    pub fn permission_attention(&self, name: QString) -> QString {
        QString::from(&crate::model::app_state::permission_attention(
            &name.to_string(),
        ))
    }

    pub fn note_restart_failed(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        reason: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let noted = self
            .as_mut()
            .rust_mut()
            .workspaces
            .note_restart_failed(&id, &reason.to_string());
        if noted {
            self.publish();
        }
        noted
    }

    pub fn note_workspace_warning(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        message: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let noted = self
            .as_mut()
            .rust_mut()
            .workspaces
            .note_workspace_warning(&id, &message.to_string());
        if noted {
            self.publish();
        }
        noted
    }

    pub fn agent_needs_start(&self, workspace_id: QString) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let workspaces = &self.rust().workspaces;
        workspaces
            .find(&id)
            .is_some_and(|(g, t)| workspaces.groups[g].tabs[t].agent_needs_start())
    }

    pub fn agent_workspace_id(&self, agent_id: QString) -> QString {
        let id = AgentId(agent_id.to_string());
        match self
            .rust()
            .workspaces
            .find_agent(&id)
            .and_then(|(g, t)| self.rust().workspaces.groups.get(g)?.tabs.get(t))
        {
            Some(tab) => QString::from(tab.workspace_id.as_str()),
            None => QString::from(""),
        }
    }

    pub fn workspace_name(&self, workspace_id: QString) -> QString {
        let id = WorkspaceId(workspace_id.to_string());
        match self
            .rust()
            .workspaces
            .find(&id)
            .and_then(|(g, t)| self.rust().workspaces.groups.get(g)?.tabs.get(t))
        {
            Some(tab) => QString::from(tab.name.as_str()),
            None => QString::from(""),
        }
    }

    pub fn workspace_in_place(&self, workspace_id: QString) -> bool {
        self.rust()
            .workspaces
            .is_in_place(&WorkspaceId(workspace_id.to_string()))
    }

    pub fn remove_group(mut self: Pin<&mut Self>, name: QString) -> bool {
        let name = name.to_string();
        let removed = self.as_mut().rust_mut().workspaces.remove_group(&name);
        if removed {
            self.publish();
        }
        removed
    }

    pub fn set_active(mut self: Pin<&mut Self>, group_idx: i32, tab_idx: i32) -> bool {
        let (Some(g), Some(t)) = (index(group_idx), index(tab_idx)) else {
            return false;
        };
        let ok = self.as_mut().rust_mut().workspaces.set_active(g, t);
        if ok {
            self.publish();
        }
        ok
    }

    pub fn active_tab_json(&self) -> QString {
        match self.rust().workspaces.active() {
            Some(tab) => QString::from(&serde_json::to_string(tab).unwrap_or_default()),
            None => QString::from(""),
        }
    }

    pub fn active_group_index(&self) -> i32 {
        self.rust().workspaces.active_group as i32
    }

    pub fn active_tab_index(&self) -> i32 {
        self.rust().workspaces.active_tab as i32
    }

    pub fn group_count(&self) -> i32 {
        self.rust().workspaces.groups.len() as i32
    }

    pub fn tab_count(&self, group_idx: i32) -> i32 {
        index(group_idx)
            .and_then(|g| self.rust().workspaces.groups.get(g))
            .map(|g| g.tabs.len() as i32)
            .unwrap_or(0)
    }

    pub fn group_name(&self, idx: i32) -> QString {
        match index(idx).and_then(|i| self.rust().workspaces.groups.get(i)) {
            Some(group) => QString::from(&group.name),
            None => QString::from(""),
        }
    }

    // `tab_label`, `tab_status` and `tab_tooltip` report the tab's *displayed*
    // status: the daemon's own, unless a failed merge, pull request or discard
    // is showing on it. They are the tab bar's, and the tab bar is where an
    // error the user has to dismiss belongs.
    //
    // `status_word` deliberately does not. Its one caller frames it as
    // "sandbox: <word>", and a merge conflict says nothing about the sandbox,
    // which is running perfectly. `stateJson` carries the underlying status
    // too, so nothing that reads the model back loses what the daemon said.
    pub fn tab_label(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self.tab_at(group_idx, tab_idx) {
            Some(tab) => QString::from(&format!("{} {}", tab.display_status().glyph(), tab.name)),
            None => QString::from(""),
        }
    }

    pub fn tab_status(&self, group_idx: i32, tab_idx: i32) -> i32 {
        self.tab_at(group_idx, tab_idx)
            .map(|tab| tab.display_status().as_i32())
            .unwrap_or(-1)
    }

    pub fn status_word(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self.tab_at(group_idx, tab_idx) {
            Some(tab) => QString::from(status_text(tab.status)),
            None => QString::from(""),
        }
    }

    pub fn tab_tooltip(&self, group_idx: i32, tab_idx: i32) -> QString {
        let Some(tab) = self.tab_at(group_idx, tab_idx) else {
            return QString::from("");
        };
        let mut text = format!(
            "{}\nrepo: {}\nbranch: {}\nadapter: {}\nstatus: {}",
            tab.name,
            tab.repo_path,
            tab.branch,
            adapter_name(tab.adapter),
            status_text(tab.display_status()),
        );
        if tab.is_in_place() {
            text.push_str("\nworks in place: no branch or merge of its own");
        }
        let detail = tab.display_detail();
        if !detail.is_empty() {
            text.push('\n');
            text.push_str(detail);
        }
        // Last, because it is the newest thing about the tab and the reason the
        // user is hovering over a dot they did not expect.
        if !tab.attention.is_empty() {
            text.push('\n');
            text.push_str(&tab.attention);
        }
        QString::from(&text)
    }

    pub fn tab_workspace_id(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self.tab_at(group_idx, tab_idx) {
            Some(tab) => QString::from(tab.workspace_id.as_str()),
            None => QString::from(""),
        }
    }

    pub fn set_tab_run_config(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        run_config: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let name = run_config.to_string();
        let Some((g, t)) = self.as_ref().rust().workspaces.find(&id) else {
            return false;
        };
        {
            let mut rust = self.as_mut().rust_mut();
            rust.workspaces.groups[g].tabs[t].run_config = (!name.is_empty()).then_some(name);
        }
        self.publish();
        true
    }

    pub fn set_tab_options(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        options_json: QString,
    ) -> bool {
        let id = WorkspaceId(workspace_id.to_string());
        let options = options_json.to_string();
        if !self
            .as_mut()
            .rust_mut()
            .workspaces
            .set_tab_options(&id, &options)
        {
            return false;
        }
        self.publish();
        true
    }

    fn tab_at(&self, group_idx: i32, tab_idx: i32) -> Option<&AgentTab> {
        let (g, t) = (index(group_idx)?, index(tab_idx)?);
        self.rust().workspaces.groups.get(g)?.tabs.get(t)
    }

    /// Re-serialises the model into `state_json` and announces the change.
    /// Every mutation ends here, so the two never drift apart.
    ///
    /// `arrangementChanged` follows `changed` when, and only when, the groups
    /// themselves moved. Decided here by comparing against the copy the Rust
    /// state carries rather than by each mutation saying so, because the
    /// mutations are where it would be got wrong: the rule is one comparison in
    /// one place, and a new invokable inherits it by calling `publish` at all.
    fn publish(mut self: Pin<&mut Self>) {
        let json = self.as_ref().rust().workspaces.to_json();
        self.as_mut().set_state_json(QString::from(&json));
        let now = arrangement_of(&self.as_ref().rust().workspaces);
        let moved = now != self.as_ref().rust().arrangement;
        if moved {
            self.as_mut().rust_mut().arrangement = now;
        }
        self.as_mut().changed();
        if moved {
            self.arrangement_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::{AgentAdapterKind, AgentState, AgentSummary};

    fn tab_with_error() -> AgentTab {
        AgentTab {
            workspace_id: WorkspaceId("ws_x".to_owned()),
            name: "x".to_owned(),
            repo_path: "/r".to_owned(),
            branch: "bs/x/work".to_owned(),
            base_branch: "main".to_owned(),
            status: TabStatus::Working,
            detail: String::new(),
            worktree_path: String::new(),
            kind: bondsymphonic_proto::WorkspaceKind::default(),
            adapter: AgentAdapterKind::Terminal,
            command: None,
            run_config: None,
            agent_id: None,
            agent_status: None,
            agent_detail: String::new(),
            options_json: String::new(),
            op_error: Some("Merge stopped: conflicts in README.md".to_owned()),
            attention: String::new(),
            workspace_problem: None,
        }
    }

    /// The two consumers of a tab's status read different things off it, and
    /// which one gets the operation-error overlay is not a detail.
    ///
    /// `tabStatus` and `tabLabel` are the tab bar's: a failed merge has to be
    /// visible from a group the user is not looking at, and dismissible.
    /// `statusWord` is the status bar's, which frames it as
    /// "sandbox: <word>" -- and a merge that conflicted says nothing about the
    /// sandbox, which is running perfectly. Reporting "sandbox: error" there
    /// sends the user to the setup page over a conflict in a text file.
    #[test]
    fn the_status_bar_word_is_the_sandbox_status_not_the_operation_error() {
        let tab = tab_with_error();
        assert_eq!(status_text(tab.display_status()), "error");
        assert_eq!(status_text(tab.status), "working");
    }

    fn info(id: &str, name: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId(id.to_owned()),
            name: name.to_owned(),
            repo_path: "/repo".to_owned(),
            base_branch: "main".to_owned(),
            branch: format!("bs/{name}/work"),
            worktree_path: format!("/wt/{id}"),
            created_at: "2026-09-11T10:00:00Z".to_owned(),
            allowlist: Vec::new(),
            kind: bondsymphonic_proto::WorkspaceKind::Worktree,
            state: WorkspaceState::Ready,
            agents: Vec::new(),
            agent_records: Vec::new(),
            runs: Vec::new(),
        }
    }

    /// A create that carries no group is not a create into a group called "".
    /// The controller has always filed one under "Unsorted" in `state.json`;
    /// the model used to make a nameless group beside it instead, so the tab
    /// the user could see and the entry that was saved for it disagreed about
    /// where it lived, and the next restart moved the tab.
    #[test]
    fn a_tab_with_no_group_is_filed_under_unsorted() {
        let mut model = Workspaces::new_default();
        let idx = group_index_for(&mut model, "");
        assert_eq!(model.groups[idx].name, UNSORTED_GROUP);
        assert!(
            !model.groups.iter().any(|g| g.name.is_empty()),
            "no group may be named the empty string: {:?}",
            model.groups.iter().map(|g| &g.name).collect::<Vec<_>>()
        );
        assert_eq!(
            group_index_for(&mut model, ""),
            idx,
            "the Unsorted group is reused rather than made twice"
        );
        // A name the user did supply is still their name, made if it is new.
        let named = group_index_for(&mut model, "Feature-A");
        assert_eq!(model.groups[named].name, "Feature-A");
        assert_eq!(group_index_for(&mut model, "Feature-A"), named);
    }

    /// What `arrangementChanged` fires on, and what it deliberately does not.
    ///
    /// The signal exists so the window can record the arrangement without
    /// writing `state.json` on every agent heartbeat. So the rule is exactly
    /// "what `groupsJson` reports has changed": group names, membership, order
    /// and the tab in front. A status, a detail or an attention mark is none of
    /// those.
    #[test]
    fn only_a_change_to_the_groups_themselves_is_an_arrangement_change() {
        let one = info("ws_1", "alpha");
        let two = info("ws_2", "beta");
        let mut model = Workspaces::new_default();
        model.add_tab(0, AgentTab::from_workspace_info(&one));
        model.add_tab(0, AgentTab::from_workspace_info(&two));
        let agent = AgentId("ag_1".to_owned());
        assert!(model.set_agent(&one.id, agent.clone()));

        let before = arrangement_of(&model);
        assert!(model
            .set_agent_status(&agent, TabStatus::Working, "busy")
            .is_some());
        assert!(model.set_workspace_attention(&one.id, "alpha is waiting for permission"));
        assert!(model.set_workspace_error(&one.id, "merge stopped"));
        assert_eq!(
            arrangement_of(&model),
            before,
            "a status, an error or an attention mark is not an arrangement change"
        );

        // Membership, names and order are.
        model.add_group("Feature-A");
        let after_group = arrangement_of(&model);
        assert_ne!(after_group, before, "a new group is an arrangement change");
        assert!(model.move_tab_to_group(&one.id, "Feature-A"));
        let after_move = arrangement_of(&model);
        assert_ne!(after_move, after_group, "a moved tab is one too");
        assert!(model.remove_group("Feature-A"));
        let after_remove = arrangement_of(&model);
        assert_ne!(after_remove, after_move, "and so is a closed group");

        // The tab in front is part of what is written, so switching tabs is an
        // arrangement change as well: without it the session would come back on
        // whichever tab happened to be first. To `one`, because `two` was added
        // last and is the tab already in front -- selecting the tab that is
        // already selected changes nothing, and asserting it did would only
        // prove the assertion was never run.
        assert_eq!(
            model.active().map(|tab| tab.workspace_id.clone()),
            Some(two.id.clone()),
            "the last tab added is the one in front"
        );
        let (g, t) = model.find(&one.id).expect("alpha is tracked");
        assert!(model.set_active(g, t));
        assert_ne!(arrangement_of(&model), after_remove);
    }

    /// A tab rebuilt from the daemon's list carries an agent it has never heard
    /// a state event for. The daemon says what that agent is doing in the very
    /// same `WorkspaceInfo`, so a workspace update must not paint the tab idle
    /// while the record beside it says the agent is working.
    #[test]
    fn a_restored_tab_takes_its_status_from_the_daemons_agent_record() {
        let mut info = info("ws_1", "alpha");
        let agent = AgentId("ag_1".to_owned());
        info.agent_records = vec![AgentSummary {
            id: agent.clone(),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Working,
            session_id: None,
            command: None,
            model: None,
            permission_mode: None,
        }];

        assert_eq!(
            agent_record_status(&info, Some(&agent)),
            Some(TabStatus::Working)
        );
        // An agent the records do not name, and a daemon too old to send any:
        // nothing is derived, and the workspace's own status stands.
        assert_eq!(
            agent_record_status(&info, Some(&AgentId("ag_other".to_owned()))),
            None
        );
        assert_eq!(agent_record_status(&info, None), None);
        info.agent_records.clear();
        assert_eq!(agent_record_status(&info, Some(&agent)), None);
    }

    /// The record fallback has to write `agent_status`, not only `status`.
    ///
    /// `Workspaces::refresh_attention` decides the attention bullet and the
    /// status-bar sentence from `agent_status`, so a tab rebuilt from
    /// `workspace.list` whose record says the agent is waiting for permission
    /// would otherwise show the glyph and never the bullet -- and the whole
    /// point of the bullet is the tab the user is *not* looking at.
    #[test]
    fn a_restored_tab_waiting_for_permission_asks_for_attention_when_it_loses_focus() {
        let mut waiting = info("ws_1", "alpha");
        let agent = AgentId("ag_1".to_owned());
        waiting.agent_records = vec![AgentSummary {
            id: agent.clone(),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::WaitingPermission,
            session_id: None,
            command: None,
            model: None,
            permission_mode: None,
        }];
        let other = info("ws_2", "beta");

        let mut model = Workspaces::new_default();
        model.add_tab(0, AgentTab::from_workspace_info(&waiting));
        model.add_tab(0, AgentTab::from_workspace_info(&other));
        assert!(model.set_agent(&waiting.id, agent.clone()));

        // What `Workspaces::apply_workspace_info` does, then the rule this
        // test is about -- the same function `GroupModel::apply_workspace_info`
        // calls, not a copy of it.
        let (g, t) = model
            .apply_workspace_info(&waiting)
            .expect("the tab is tracked");
        let tab = &mut model.groups[g].tabs[t];
        assert!(
            tab.agent_status.is_none(),
            "the premise: no agent.state has ever been seen for this tab"
        );
        assert!(
            adopt_agent_record(tab, &waiting),
            "the daemon's record says what the agent is doing"
        );
        assert_eq!(tab.status, TabStatus::WaitingPermission, "the glyph");

        // The user is on `alpha` and switches to `beta`. The tab they left is
        // the one with the question, so it is the one that has to ask.
        let (g, t) = model.find(&other.id).expect("beta is tracked");
        assert!(model.set_active(g, t));
        assert!(
            model.refresh_attention(Some(&waiting.id)),
            "leaving a tab whose agent is waiting must move something"
        );
        let (g, t) = model.find(&waiting.id).expect("alpha is tracked");
        assert!(
            !model.groups[g].tabs[t].attention.is_empty(),
            "the tab the user left carries the attention mark"
        );
        assert!(
            model.attention().is_some_and(|t| t.contains("alpha")),
            "and the status bar names it: {:?}",
            model.attention()
        );
    }

    /// A `workspace.restart` answers with the stopped agent's record saying
    /// `Exited`. That decides whether Retry starts the agent again, so it must
    /// win over whatever the tab last heard -- an `agent.state` for the stop
    /// may not have arrived yet, or may have been dropped.
    #[test]
    fn an_exited_record_overrides_what_the_tab_last_heard() {
        let mut ready = info("ws_1", "alpha");
        let agent = AgentId("ag_1".to_owned());
        ready.agent_records = vec![AgentSummary {
            id: agent.clone(),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: None,
            command: None,
            model: None,
            permission_mode: None,
        }];
        let mut model = Workspaces::new_default();
        let mut tab = AgentTab::from_workspace_info(&ready);
        tab.adapter = AgentAdapterKind::Claude;
        model.add_tab(0, tab);
        assert!(model.set_agent(&ready.id, agent.clone()));
        assert!(model
            .set_agent_status(&agent, TabStatus::Working, "thinking")
            .is_some());

        let (g, t) = model.apply_workspace_info(&ready).expect("tracked");
        let tab = &mut model.groups[g].tabs[t];
        assert!(
            !tab.agent_needs_start(),
            "the premise: the tab thinks it is working"
        );
        assert!(adopt_agent_record(tab, &ready));
        assert_eq!(tab.agent_status, Some(TabStatus::Done));
        assert_eq!(tab.status, TabStatus::Done);
        assert!(tab.agent_needs_start());
        // Said once: a second pass has nothing to change.
        assert!(!adopt_agent_record(tab, &ready));

        // A live record does not override a live tab.
        let mut working = ready.clone();
        working.agent_records[0].state = AgentState::Idle;
        tab.agent_status = Some(TabStatus::Working);
        assert!(!adopt_agent_record(tab, &working));
        assert_eq!(tab.agent_status, Some(TabStatus::Working));
    }
}
