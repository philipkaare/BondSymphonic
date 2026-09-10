//! Pure-Rust application state. This module must never import Qt types.

use crate::model::persistence::PersistedGroup;
use bondsymphonic_proto::{
    AgentAdapterKind, AgentId, AgentState, AgentSummary, WorkspaceId, WorkspaceInfo, WorkspaceState,
};
use serde::{Deserialize, Serialize};

/// Where a workspace the daemon has but no group claims is filed. Looked up by
/// name rather than by a stable id, because the user can rename it and the
/// next unclaimed workspace should still land somewhere sensible.
pub const UNSORTED_GROUP: &str = "Unsorted";

/// The workspaces with a merge, pull request, discard or destroy in flight.
///
/// One set, owned by `AppController`, because the Changes toolbar is not the
/// only thing that starts those: the tab context menu destroys and the
/// close-group runner merges and discards, and each of them used to walk past
/// a set the toolbar kept to itself. What the set prevents is a destroy landing
/// while a merge is still packing the merged commits out of the workspace's
/// private object directory -- after which the base branch names a commit whose
/// parents have been deleted, and the user's own repository will not read.
///
/// Split out of the QObject so it can be tested without a Qt event loop: the
/// controller's half is two calls and a signal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BusyWorkspaces(std::collections::HashSet<String>);

impl BusyWorkspaces {
    /// Books an operation in for `workspace`. False when one is already out,
    /// which is a refusal rather than a queue: running the second one late is
    /// the same loss as running it now, and the user can ask again once the
    /// first has answered.
    pub fn begin(&mut self, workspace: &str) -> bool {
        self.0.insert(workspace.to_owned())
    }

    /// Books it out again. False when it was not booked in, so a caller can
    /// tell whether anything changed before announcing that it did.
    pub fn end(&mut self, workspace: &str) -> bool {
        self.0.remove(workspace)
    }

    pub fn contains(&self, workspace: &str) -> bool {
        self.0.contains(workspace)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Lifecycle of the IDE's connection to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Launching,
    Connecting,
    Connected,
    /// The connection ended and nothing is being done about it: the IDE is on
    /// its way out, or the very first connect failed. An ordinary loss goes to
    /// [`ConnectionState::Reconnecting`] instead.
    Lost,
    Error,
    /// The connection ended and the IDE is relaunching the daemon. `attempt`
    /// counts from 1 and never stops climbing; the delay before each attempt
    /// is the controller's `backoff_delay`.
    Reconnecting {
        attempt: u32,
    },
}

impl ConnectionState {
    /// Stable numeric code exposed to C++/QML as the `connection_state`
    /// property. The attempt count is deliberately not in it: the property is
    /// one integer and the number the user reads is in `status_message`.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Disconnected => 0,
            Self::Launching => 1,
            Self::Connecting => 2,
            Self::Connected => 3,
            Self::Lost => 4,
            Self::Error => 5,
            Self::Reconnecting { .. } => 6,
        }
    }

    /// Inverse of [`ConnectionState::as_i32`]. Codes outside 0..=6 map to
    /// [`ConnectionState::Error`], since the `connection_state` property is
    /// writable from C++ and may hold anything.
    ///
    /// A code of 6 comes back as attempt 0, because the code never carried the
    /// attempt. The controller keeps the live number itself and re-applies it,
    /// so nothing that recomposes the status text from the property alone can
    /// invent an attempt that was never made.
    pub fn from_i32(code: i32) -> Self {
        match code {
            0 => Self::Disconnected,
            1 => Self::Launching,
            2 => Self::Connecting,
            3 => Self::Connected,
            4 => Self::Lost,
            6 => Self::Reconnecting { attempt: 0 },
            _ => Self::Error,
        }
    }

    /// Human readable text for the status bar, without the attempt count.
    /// [`compose_status`] is what adds that.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "daemon: not started",
            Self::Launching => "daemon: launching",
            Self::Connecting => "daemon: connecting",
            Self::Connected => "daemon: connected",
            Self::Lost => "daemon: connection lost",
            Self::Error => "daemon: error",
            Self::Reconnecting { .. } => "daemon: reconnecting",
        }
    }
}

/// Builds the status bar text from a connection state and the daemon version.
/// This is the only place the two are combined, so the C++ shell never has to
/// branch on either.
///
/// An empty version contributes nothing. While reconnecting the version
/// contributes nothing either, whatever it holds: it describes the daemon that
/// has just died, and repeating it beside "reconnecting" would claim a version
/// for a daemon that has not answered yet. The attempt takes its place, so the
/// user can see that something is still being tried.
pub fn compose_status(state: ConnectionState, version: &str) -> String {
    if let ConnectionState::Reconnecting { attempt } = state {
        // Attempt 0 only arises from `from_i32` on the raw property; there is
        // no such attempt, so the bare label is the honest text.
        return match attempt {
            0 => state.label().to_owned(),
            n => format!("{} (attempt {n})", state.label()),
        };
    }
    if version.is_empty() {
        state.label().to_owned()
    } else {
        format!("{} v{version}", state.label())
    }
}

/// Status of one agent tab, driven by the daemon's `WorkspaceState` plus
/// local agent activity (working / waiting on a permission prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabStatus {
    Idle,
    Working,
    WaitingPermission,
    Error,
    Done,
    Creating,
    SandboxDown,
}

impl TabStatus {
    /// Single-glyph indicator shown in the tab bar.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Working => "●",
            Self::WaitingPermission => "!",
            Self::Error => "✕",
            Self::Done => "✓",
            Self::Creating => "…",
            Self::SandboxDown => "⏸",
        }
    }

    /// Maps a daemon-reported `WorkspaceState` onto a tab status.
    pub fn from_workspace_state(s: &WorkspaceState) -> TabStatus {
        match s {
            WorkspaceState::Creating => Self::Creating,
            WorkspaceState::Ready => Self::Idle,
            WorkspaceState::SandboxDown => Self::SandboxDown,
            WorkspaceState::Error(_) => Self::Error,
            WorkspaceState::Destroying => Self::Done,
        }
    }

    /// Maps an agent's state onto a tab status. A Claude tab's status comes
    /// from here for the whole session: its workspace stays `Ready` while the
    /// agent moves between working, waiting on a permission and idle. An agent
    /// that has exited leaves a finished tab, not a broken one.
    pub fn from_agent_state(s: &AgentState) -> TabStatus {
        match s {
            AgentState::Idle => Self::Idle,
            AgentState::Working => Self::Working,
            AgentState::WaitingPermission => Self::WaitingPermission,
            AgentState::Error => Self::Error,
            AgentState::Exited => Self::Done,
        }
    }

    /// Stable numeric code exposed to C++/QML.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Idle => 0,
            Self::Working => 1,
            Self::WaitingPermission => 2,
            Self::Error => 3,
            Self::Done => 4,
            Self::Creating => 5,
            Self::SandboxDown => 6,
        }
    }
}

/// One open agent session, backed by a daemon workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTab {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub repo_path: String,
    pub branch: String,
    /// The branch this workspace was forked from and merges back into, from
    /// `WorkspaceInfo`. The Changes toolbar names it in every confirmation, so
    /// a user reads which branch is about to move before they move it.
    /// Defaulted so a session saved before it was carried still loads;
    /// [`Workspaces::apply_workspace_info`] fills it in again.
    #[serde(default)]
    pub base_branch: String,
    pub status: TabStatus,
    /// `Error(detail)` text from the daemon, or empty for any other status.
    pub detail: String,
    /// The workspace's worktree, from `WorkspaceInfo`. The Run panel detects
    /// configurations against this path, since `repo.detect_run_configs` takes
    /// a path rather than a workspace. Defaulted so a session saved before it
    /// was carried still loads; `apply_workspace_info` fills it in again.
    #[serde(default)]
    pub worktree_path: String,
    pub adapter: AgentAdapterKind,
    pub command: Option<String>,
    /// The run configuration chosen for this workspace, if any, so the Run
    /// panel opens on it when the tab is selected.
    #[serde(default)]
    pub run_config: Option<String>,
    /// The agent running in this tab, once `agent.start` has answered.
    /// Defaulted rather than required so a session saved before agent tabs
    /// existed still loads.
    #[serde(default)]
    pub agent_id: Option<AgentId>,
    /// The last status the agent itself reported, kept separately from
    /// `status` so a workspace event can take the badge for the duration of a
    /// sandbox outage and hand it back on recovery. `None` until the agent has
    /// reported anything.
    #[serde(default)]
    pub agent_status: Option<TabStatus>,
    /// The detail belonging to `agent_status`, restored with it.
    #[serde(default)]
    pub agent_detail: String,
    /// The `AgentStartOptions` this tab's agent was started with, as JSON, or
    /// empty for a tab that never asked for one. Kept so restarting an agent --
    /// after it exited, or after a daemon restart left the workspace with none
    /// -- uses the model and permission mode the user chose rather than the
    /// defaults. Never carries the API key: the controller merges that in at
    /// call time and this is written to `session.json`.
    #[serde(default)]
    pub options_json: String,
    /// A merge, pull request or discard on this workspace that failed, as the
    /// sentence the user is shown. While it is set the tab reads as
    /// [`TabStatus::Error`] whatever the workspace and its agent report, and
    /// clearing it hands the badge straight back to them.
    ///
    /// Kept beside `status` rather than written into it because the two have
    /// different owners: `status` belongs to the daemon's events, and this
    /// belongs to a request the user made from the Changes toolbar. Overlaying
    /// is what lets a `workspace.state` arriving mid-banner keep the model
    /// current without taking the banner's badge down, and what makes clearing
    /// the banner a single assignment rather than a guess at what the badge
    /// used to say.
    #[serde(default)]
    pub op_error: Option<String>,
    /// Why this tab wants the user, as the sentence both the tab bar and the
    /// status bar show, or empty for a tab that is not asking for anything.
    ///
    /// Today the only writer is a permission request raised by an agent in a
    /// tab the user is *not* looking at: the permission bar lives on the
    /// workspace's own pane, so without this the question is invisible until
    /// they happen to switch to it. Held as the finished sentence rather than
    /// as a flag so the dot and the hint cannot word it differently.
    ///
    /// Not persisted as anything meaningful: a session file written while a
    /// question was open reloads against a daemon that has since answered or
    /// forgotten it, and `agent.state` says what is true now.
    #[serde(default)]
    pub attention: String,
}

/// The sentence a tab carries while its agent is waiting to be allowed a tool.
///
/// One function so the tab's dot, its tooltip and the status bar hint are the
/// same words; `name` is the workspace's, which is what the user named the
/// agent and what the tab is labelled with.
pub fn permission_attention(name: &str) -> String {
    format!("{name} is waiting for permission")
}

impl AgentTab {
    /// The status the tab bar paints: [`TabStatus::Error`] while an operation
    /// error is showing, and the daemon's own status otherwise.
    pub fn display_status(&self) -> TabStatus {
        match self.op_error {
            Some(_) => TabStatus::Error,
            None => self.status,
        }
    }

    /// The detail belonging to [`AgentTab::display_status`].
    pub fn display_detail(&self) -> &str {
        match &self.op_error {
            Some(detail) => detail.as_str(),
            None => self.detail.as_str(),
        }
    }

    /// A tab for a workspace the daemon described but the IDE was not tracking:
    /// what [`Workspaces::reconcile`] files into "Unsorted" and what
    /// [`Workspaces::from_persisted`] rebuilds a restored group from.
    ///
    /// Nothing local is invented -- no command, no run configuration -- but the
    /// agent is not local: `WorkspaceInfo.agent_records` is the daemon's own
    /// list, and the tab adopts the last of them. That is what makes an agent survive an
    /// *IDE* restart as well as a daemon one. Without it every Claude workspace
    /// came back as a terminal tab bound to no agent, and the transcript the
    /// daemon was still serving had no pane in the UI that could reach it.
    ///
    /// The last entry rather than a search for a running one: the daemon lists
    /// a workspace's agents oldest first, handing restored ones their place
    /// before any new agent can take it, and an agent that has ended is exactly
    /// the one worth coming back to -- its transcript replays and its Restart
    /// resumes the session.
    pub fn from_workspace_info(info: &WorkspaceInfo) -> AgentTab {
        // `agent_records` rather than `agents`: the bare ids beside it say
        // nothing about the adapter, and a daemon too old to send the records
        // leaves them empty, which is the terminal tab this built before.
        let agent = info.agent_records.last();
        AgentTab {
            workspace_id: info.id.clone(),
            name: info.name.clone(),
            repo_path: info.repo_path.clone(),
            branch: info.branch.clone(),
            base_branch: info.base_branch.clone(),
            status: TabStatus::from_workspace_state(&info.state),
            detail: match &info.state {
                WorkspaceState::Error(detail) => detail.clone(),
                _ => String::new(),
            },
            worktree_path: info.worktree_path.clone(),
            adapter: agent.map_or(AgentAdapterKind::Terminal, |a| a.adapter),
            command: None,
            run_config: None,
            agent_id: agent.map(|a| a.id.clone()),
            agent_status: None,
            agent_detail: String::new(),
            options_json: agent.map(restart_options_json).unwrap_or_default(),
            op_error: None,
            attention: String::new(),
        }
    }
}

/// The `options_json` a tab restored onto `agent` carries: the options the
/// daemon says it was started with, as an `AgentStartOptions` object.
///
/// Only the keys that are set, so an agent started with nothing leaves the
/// field empty -- which is what `TranscriptModel::restartOptions` reads as
/// "the daemon's defaults", and is honest about there being no choice to
/// restore.
///
/// `resume_session` is seeded from the summary and then *overridden* by the
/// transcript when the replay finds a newer id. The transcript is the better
/// source when it has one, but it is not always able to have one: a history
/// the daemon could not serve, or one that never reached a `result` message,
/// leaves it with nothing while the daemon still holds the id the agent
/// reported. Restarting into a fresh session there loses the conversation the
/// user is looking at, which is the one thing Restart exists to keep.
fn restart_options_json(agent: &AgentSummary) -> String {
    let mut options = serde_json::Map::new();
    for (key, value) in [
        ("command", &agent.command),
        ("model", &agent.model),
        ("permission_mode", &agent.permission_mode),
        ("resume_session", &agent.session_id),
    ] {
        if let Some(value) = value {
            options.insert(key.to_owned(), serde_json::Value::String(value.clone()));
        }
    }
    if options.is_empty() {
        return String::new();
    }
    serde_json::Value::Object(options).to_string()
}

/// A user-defined collection of agent tabs, shown as a section in the sidebar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub tabs: Vec<AgentTab>,
}

/// Root of the IDE's tab model: groups of agent tabs plus which one is active.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspaces {
    pub groups: Vec<Group>,
    pub active_group: usize,
    pub active_tab: usize,
    /// The next number [`Workspaces::allocate_group_id`] will try.
    ///
    /// A group id is the handle the C++ side holds between two reads of
    /// `state_json` -- the tab bar keeps one per strip -- so the model must
    /// never hand the same id to two groups. The old scheme numbered a group
    /// by its position (`grp_<len>`), which does exactly that the first time a
    /// group in the middle is closed and another is made: the survivors keep
    /// the ids they were built with, and `len` has gone backwards.
    ///
    /// Carried through `state_json` (defaulted for a file written before it
    /// existed) so a round trip cannot reset it; `allocate_group_id` skips any
    /// number already in use, which is what makes a defaulted 0 safe.
    #[serde(default)]
    next_group_id: usize,
}

impl Workspaces {
    /// Rebuilds the tab model from `state.json`'s groups and the daemon's
    /// authoritative workspace list.
    ///
    /// The persisted order is the user's order, so groups come back in the
    /// order they were written and each keeps its workspaces in the order it
    /// listed them. Everything else follows from the daemon being the authority
    /// on what exists: a persisted id the daemon no longer has is dropped, a
    /// workspace the daemon has that no group claims lands in
    /// [`UNSORTED_GROUP`], and every tab is rebuilt from the daemon's
    /// `WorkspaceInfo` rather than from anything the file remembered about it.
    ///
    /// An empty persisted group is kept: the user made it, and a group that
    /// vanished every time its last workspace was destroyed would have to be
    /// made again by hand. A workspace named by two groups is placed once, in
    /// the first that claims it, so a hand-edited file cannot produce two tabs
    /// for one workspace.
    ///
    /// `active` is the workspace that was in front. It is restored when the
    /// daemon still has it; otherwise the first tab of the first non-empty
    /// group is selected, which is what the user sees anyway.
    pub fn from_persisted(
        groups: &[PersistedGroup],
        list: &[WorkspaceInfo],
        active: Option<&str>,
    ) -> Workspaces {
        let by_id: std::collections::HashMap<&str, &WorkspaceInfo> =
            list.iter().map(|info| (info.id.as_str(), info)).collect();
        let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();

        let mut model = Workspaces {
            groups: Vec::new(),
            active_group: 0,
            active_tab: 0,
            next_group_id: 0,
        };
        for persisted in groups {
            let mut tabs = Vec::new();
            for id in &persisted.workspace_ids {
                let Some(info) = by_id.get(id.as_str()) else {
                    continue;
                };
                if !placed.insert(info.id.as_str()) {
                    continue;
                }
                tabs.push(AgentTab::from_workspace_info(info));
            }
            let id = model.allocate_group_id();
            model.groups.push(Group {
                id,
                name: persisted.name.clone(),
                tabs,
            });
        }

        let unclaimed: Vec<&WorkspaceInfo> = list
            .iter()
            .filter(|info| !placed.contains(info.id.as_str()))
            .collect();
        if !unclaimed.is_empty() {
            let idx = match model.groups.iter().position(|g| g.name == UNSORTED_GROUP) {
                Some(idx) => idx,
                None => model.add_group(UNSORTED_GROUP),
            };
            for info in unclaimed {
                model.groups[idx]
                    .tabs
                    .push(AgentTab::from_workspace_info(info));
            }
        }

        // Nothing persisted and nothing running: the sidebar still needs a
        // group for the first workspace to be created into.
        if model.groups.is_empty() {
            return Workspaces::new_default();
        }

        match active
            .map(|id| WorkspaceId(id.to_owned()))
            .and_then(|id| model.find(&id))
        {
            Some((g, t)) => {
                model.active_group = g;
                model.active_tab = t;
            }
            None => model.fallback_active(),
        }
        model
    }

    /// The arrangement to write to `state.json`: every group by name, in order,
    /// with the workspaces it holds. The inverse of
    /// [`Workspaces::from_persisted`].
    pub fn persisted_groups(&self) -> Vec<PersistedGroup> {
        self.groups
            .iter()
            .map(|g| PersistedGroup {
                name: g.name.clone(),
                workspace_ids: g.tabs.iter().map(|t| t.workspace_id.0.clone()).collect(),
            })
            .collect()
    }

    /// A fresh model with a single group named "Default" and no tabs.
    pub fn new_default() -> Self {
        Workspaces {
            groups: vec![Group {
                id: "grp_0".to_owned(),
                name: "Default".to_owned(),
                tabs: Vec::new(),
            }],
            active_group: 0,
            active_tab: 0,
            next_group_id: 1,
        }
    }

    /// A group id no group in this model is using, and a counter that has
    /// moved past it.
    ///
    /// The loop is what makes the field's `#[serde(default)]` safe: a
    /// `state.json` written before the counter existed deserialises with 0
    /// beside groups already called `grp_0` and `grp_1`, and this walks past
    /// them rather than reissuing one.
    fn allocate_group_id(&mut self) -> String {
        loop {
            let id = format!("grp_{}", self.next_group_id);
            self.next_group_id += 1;
            if !self.groups.iter().any(|g| g.id == id) {
                return id;
            }
        }
    }

    /// Appends a new, empty group and returns its index -- or the index of the
    /// group that already has that name, without adding a second.
    ///
    /// Group names are the model's only handle on a group: [`UNSORTED_GROUP`]
    /// is found by name, [`Workspaces::move_tab_to_group`] takes a name, and
    /// [`Workspaces::remove_group`] removes the first match. Two groups sharing
    /// one would each be reachable only by accident, and the second "Unsorted"
    /// would never receive an unclaimed workspace however many the user put in
    /// it by hand.
    pub fn add_group(&mut self, name: &str) -> usize {
        if let Some(existing) = self.groups.iter().position(|g| g.name == name) {
            return existing;
        }
        let id = self.allocate_group_id();
        self.groups.push(Group {
            id,
            name: name.to_owned(),
            tabs: Vec::new(),
        });
        self.groups.len() - 1
    }

    /// Renames the group at `idx`. False for an index that is not there, and
    /// for a name another group already has -- see [`Workspaces::add_group`]
    /// for why a duplicate is refused rather than allowed and disambiguated.
    ///
    /// Renaming a group to what it is already called succeeds: the rename
    /// dialog opens on the current name, so that is what pressing OK without
    /// typing means, and it is not a clash with itself.
    pub fn rename_group(&mut self, idx: usize, name: &str) -> bool {
        if idx >= self.groups.len() {
            return false;
        }
        let taken = self
            .groups
            .iter()
            .enumerate()
            .any(|(i, g)| i != idx && g.name == name);
        if taken {
            return false;
        }
        self.groups[idx].name = name.to_owned();
        true
    }

    /// Adds `tab` to the group at `group_idx` and makes it the active tab.
    pub fn add_tab(&mut self, group_idx: usize, tab: AgentTab) -> (usize, usize) {
        self.groups[group_idx].tabs.push(tab);
        let tab_idx = self.groups[group_idx].tabs.len() - 1;
        self.active_group = group_idx;
        self.active_tab = tab_idx;
        (group_idx, tab_idx)
    }

    /// Removes the tab for `id` from whichever group holds it, fixing up the
    /// active selection if it pointed at the removed tab (or beyond it).
    pub fn remove_workspace(&mut self, id: &WorkspaceId) -> bool {
        let Some((g, t)) = self.find(id) else {
            return false;
        };
        self.groups[g].tabs.remove(t);
        if self.active_group == g && self.active_tab > t {
            self.active_tab -= 1;
        }
        let active_valid = self
            .groups
            .get(self.active_group)
            .is_some_and(|grp| self.active_tab < grp.tabs.len());
        if !active_valid {
            self.fallback_active();
        }
        true
    }

    /// Moves the tab for `id` into the group named `group_name`, creating that
    /// group at the end if it does not exist yet. Returns false when the
    /// workspace is not tracked or is already in that group.
    ///
    /// This is how a workspace filed into "Unsorted" by [`Workspaces::reconcile`]
    /// reaches the group the user actually asked for once the create call
    /// answers. A tab that was active stays active in its new home, and the
    /// selection is repaired if removing the tab shifted it.
    pub fn move_tab_to_group(&mut self, id: &WorkspaceId, group_name: &str) -> bool {
        let Some((from, idx)) = self.find(id) else {
            return false;
        };
        if self.groups[from].name == group_name {
            return false;
        }
        let was_active = self.active_group == from && self.active_tab == idx;
        let tab = self.groups[from].tabs.remove(idx);
        if self.active_group == from && self.active_tab > idx {
            self.active_tab -= 1;
        }
        let to = match self.groups.iter().position(|g| g.name == group_name) {
            Some(existing) => existing,
            None => self.add_group(group_name),
        };
        self.groups[to].tabs.push(tab);
        if was_active {
            self.active_group = to;
            self.active_tab = self.groups[to].tabs.len() - 1;
            return true;
        }
        let active_valid = self
            .groups
            .get(self.active_group)
            .is_some_and(|grp| self.active_tab < grp.tabs.len());
        if !active_valid {
            self.fallback_active();
        }
        true
    }

    /// Points the active selection at the first non-empty group, or leaves it
    /// at `(0, 0)` if every group is empty.
    fn fallback_active(&mut self) {
        for (gi, grp) in self.groups.iter().enumerate() {
            if !grp.tabs.is_empty() {
                self.active_group = gi;
                self.active_tab = 0;
                return;
            }
        }
        self.active_group = 0;
        self.active_tab = 0;
    }

    pub fn find(&self, id: &WorkspaceId) -> Option<(usize, usize)> {
        for (gi, g) in self.groups.iter().enumerate() {
            if let Some(ti) = g.tabs.iter().position(|t| &t.workspace_id == id) {
                return Some((gi, ti));
            }
        }
        None
    }

    /// Updates the tab's status, branch and detail from a daemon `WorkspaceInfo`.
    /// Returns `None` without modifying anything if `info.id` is not tracked.
    ///
    /// On a tab running an agent the badge changes hands rather than being
    /// overwritten. While the workspace is anything but `Ready` it is the
    /// workspace's news that matters, so that wins; when the workspace goes
    /// `Ready` the agent owns the badge again and whatever it last reported is
    /// restored (`Idle` if it has not reported yet).
    ///
    /// Both halves are needed. Letting a `Ready` event overwrite "working"
    /// with "idle" blanks the indicator mid-turn, because the daemon emits
    /// `workspace.state` for reasons that have nothing to do with the agent.
    /// Ignoring the `Ready` event instead would be worse: recovery is exactly
    /// the event that would then be dropped, leaving a "sandbox down" badge
    /// that an idle agent, which emits no state of its own, never clears.
    pub fn apply_workspace_info(&mut self, info: &WorkspaceInfo) -> Option<(usize, usize)> {
        let (g, t) = self.find(&info.id)?;
        let tab = &mut self.groups[g].tabs[t];
        let agent_owns_status =
            tab.agent_id.is_some() && matches!(info.state, WorkspaceState::Ready);
        if agent_owns_status {
            tab.status = tab.agent_status.unwrap_or(TabStatus::Idle);
            tab.detail = tab.agent_detail.clone();
        } else {
            tab.status = TabStatus::from_workspace_state(&info.state);
            tab.detail = match &info.state {
                WorkspaceState::Error(detail) => detail.clone(),
                _ => String::new(),
            };
        }
        tab.branch = info.branch.clone();
        // Refreshed rather than set once: a tab restored from a session file
        // written before this field existed has none, and the Run panel cannot
        // detect anything without it.
        tab.worktree_path = info.worktree_path.clone();
        // Same reason, and the Changes toolbar has to name it in every
        // confirmation it puts up.
        tab.base_branch = info.base_branch.clone();
        Some((g, t))
    }

    /// Marks the workspace's tab as carrying a failed operation, with `detail`
    /// as the sentence to show. False when the workspace is not tracked.
    ///
    /// The error is the workspace's, not its agent's: a merge that conflicts
    /// says nothing about whatever is running inside the sandbox, and a tab
    /// with no agent at all can still fail to merge.
    pub fn set_workspace_error(&mut self, id: &WorkspaceId, detail: &str) -> bool {
        let Some((g, t)) = self.find(id) else {
            return false;
        };
        self.groups[g].tabs[t].op_error = Some(detail.to_owned());
        true
    }

    /// Takes that mark off again, handing the badge back to whatever the
    /// daemon last said about the workspace or its agent. False when the
    /// workspace is not tracked.
    pub fn clear_workspace_error(&mut self, id: &WorkspaceId) -> bool {
        let Some((g, t)) = self.find(id) else {
            return false;
        };
        self.groups[g].tabs[t].op_error = None;
        true
    }

    /// Removes the group named `name`, moving whatever tabs it still holds
    /// into [`UNSORTED_GROUP`]. False when there is no such group.
    ///
    /// The tabs move rather than going with the group: closing a group is a
    /// decision about the group, and the workspaces in it that the user chose
    /// to keep are still live in the daemon. A group being closed that *is*
    /// "Unsorted" is refused, because the tabs would have nowhere to go and
    /// the next `reconcile` would only make it again.
    /// Marks the tab for `ws` as wanting the user, with `text` as the sentence
    /// both the tab bar and the status bar show. False when that workspace is
    /// not tracked, or when it already carries exactly this text.
    ///
    /// Separate from [`Workspaces::set_workspace_error`] because the two mean
    /// different things and are answered differently: an error is a failed
    /// request the user has to dismiss, and this is a question the agent is
    /// blocked on, which goes away by itself as soon as it is answered.
    pub fn set_workspace_attention(&mut self, ws: &WorkspaceId, text: &str) -> bool {
        let Some((g, t)) = self.find(ws) else {
            return false;
        };
        if self.groups[g].tabs[t].attention == text {
            return false;
        }
        self.groups[g].tabs[t].attention = text.to_owned();
        true
    }

    /// Takes the attention mark off `ws`. False when that workspace is not
    /// tracked or was not carrying one, so a caller that clears on every state
    /// change does not repaint the tab bar for nothing.
    pub fn clear_workspace_attention(&mut self, ws: &WorkspaceId) -> bool {
        let Some((g, t)) = self.find(ws) else {
            return false;
        };
        if self.groups[g].tabs[t].attention.is_empty() {
            return false;
        }
        self.groups[g].tabs[t].attention.clear();
        true
    }

    /// The sentence for the status bar: the first tab, in the user's own group
    /// and tab order, that is asking for something. `None` when nothing is.
    ///
    /// One line rather than a list: two agents waiting at once is possible, and
    /// the dots on their tabs are what says how many. The status bar is a
    /// pointer at the nearest one, not a summary.
    pub fn attention(&self) -> Option<&str> {
        self.groups
            .iter()
            .flat_map(|g| g.tabs.iter())
            .map(|t| t.attention.as_str())
            .find(|text| !text.is_empty())
    }

    pub fn remove_group(&mut self, name: &str) -> bool {
        let Some(idx) = self.groups.iter().position(|g| g.name == name) else {
            return false;
        };
        if name == UNSORTED_GROUP {
            return false;
        }
        let active_id = self.active().map(|t| t.workspace_id.clone());
        let kept = std::mem::take(&mut self.groups[idx].tabs);
        self.groups.remove(idx);
        if !kept.is_empty() {
            let target = match self.groups.iter().position(|g| g.name == UNSORTED_GROUP) {
                Some(existing) => existing,
                None => self.add_group(UNSORTED_GROUP),
            };
            self.groups[target].tabs.extend(kept);
        }
        // Every index after the removed group has shifted, so the selection is
        // re-derived from the workspace that was in front rather than repaired
        // arithmetically.
        match active_id.and_then(|id| self.find(&id)) {
            Some((g, t)) => {
                self.active_group = g;
                self.active_tab = t;
            }
            None => self.fallback_active(),
        }
        // A model with no groups left has nowhere to create the next workspace.
        if self.groups.is_empty() {
            self.add_group(UNSORTED_GROUP);
            self.fallback_active();
        }
        true
    }

    /// Records the agent running in the tab for `ws`. False when that
    /// workspace is not tracked.
    pub fn set_agent(&mut self, ws: &WorkspaceId, agent_id: AgentId) -> bool {
        let Some((g, t)) = self.find(ws) else {
            return false;
        };
        self.groups[g].tabs[t].agent_id = Some(agent_id);
        true
    }

    /// Applies an `agent.state` event. Those carry an agent id and no
    /// workspace id, so the tab is found by the id recorded in
    /// [`Workspaces::set_agent`]. `None` when no tab is running that agent.
    pub fn set_agent_status(
        &mut self,
        agent_id: &AgentId,
        status: TabStatus,
        detail: &str,
    ) -> Option<(usize, usize)> {
        let (g, t) = self.find_agent(agent_id)?;
        let tab = &mut self.groups[g].tabs[t];
        // Recorded as well as shown, so a sandbox outage that takes the badge
        // hands this back when the workspace recovers.
        tab.agent_status = Some(status);
        tab.agent_detail = detail.to_owned();
        tab.status = status;
        tab.detail = detail.to_owned();
        Some((g, t))
    }

    /// The tab running `agent_id`, if any.
    pub fn find_agent(&self, agent_id: &AgentId) -> Option<(usize, usize)> {
        for (gi, g) in self.groups.iter().enumerate() {
            if let Some(ti) = g
                .tabs
                .iter()
                .position(|t| t.agent_id.as_ref() == Some(agent_id))
            {
                return Some((gi, ti));
            }
        }
        None
    }

    /// Syncs against the daemon's authoritative workspace list: tabs for
    /// workspaces the daemon no longer has are dropped, workspaces the IDE
    /// does not yet know about are added to an "Unsorted" group (created at
    /// the end if missing), and known workspaces have their info refreshed.
    pub fn reconcile(&mut self, daemon_list: &[WorkspaceInfo]) {
        // Remember which workspace (and which group it lived in) was active
        // before mutating, so we can re-point at it afterward instead of
        // relying on its numeric index, which `retain` below can shift out
        // from under an unrelated tab in the same group.
        let active_id = self.active().map(|t| t.workspace_id.clone());
        let prev_active_group = self.active_group;

        let daemon_ids: std::collections::HashSet<&WorkspaceId> =
            daemon_list.iter().map(|info| &info.id).collect();
        for group in &mut self.groups {
            group.tabs.retain(|t| daemon_ids.contains(&t.workspace_id));
        }

        for info in daemon_list {
            if self.find(&info.id).is_some() {
                self.apply_workspace_info(info);
                continue;
            }
            // Unknown workspaces are filed into a group named "Unsorted",
            // looked up (and created if absent) by name, not by a stable id.
            let group_idx = match self.groups.iter().position(|g| g.name == UNSORTED_GROUP) {
                Some(idx) => idx,
                None => self.add_group(UNSORTED_GROUP),
            };
            self.groups[group_idx]
                .tabs
                .push(AgentTab::from_workspace_info(info));
        }

        // Restore the active selection: keep pointing at the same workspace
        // if it survived reconciliation, else prefer another tab that is
        // still in its former group, else fall back to the first non-empty
        // group.
        match active_id.and_then(|id| self.find(&id)) {
            Some((g, t)) => {
                self.active_group = g;
                self.active_tab = t;
            }
            None if self
                .groups
                .get(prev_active_group)
                .is_some_and(|g| !g.tabs.is_empty()) =>
            {
                self.active_group = prev_active_group;
                self.active_tab = 0;
            }
            None => self.fallback_active(),
        }
    }

    pub fn active(&self) -> Option<&AgentTab> {
        self.groups
            .get(self.active_group)?
            .tabs
            .get(self.active_tab)
    }

    pub fn set_active(&mut self, group_idx: usize, tab_idx: usize) -> bool {
        let valid = self
            .groups
            .get(group_idx)
            .is_some_and(|g| tab_idx < g.tabs.len());
        if valid {
            self.active_group = group_idx;
            self.active_tab = tab_idx;
        }
        valid
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }
}

/// The daemon's snake_case spelling of an agent state. The `TranscriptModel`
/// publishes this as its `state` property, and `GroupModel::setAgentStatus`
/// parses it back, so the word crossing the boundary is defined once here.
pub fn agent_state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::WaitingPermission => "waiting_permission",
        AgentState::Error => "error",
        AgentState::Exited => "exited",
    }
}

/// Inverse of [`agent_state_word`]. `None` for anything else, so a caller
/// decides what an unrecognised word means rather than being handed a guess.
pub fn parse_agent_state(word: &str) -> Option<AgentState> {
    match word {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "waiting_permission" => Some(AgentState::WaitingPermission),
        "error" => Some(AgentState::Error),
        "exited" => Some(AgentState::Exited),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [ConnectionState; 7] = [
        ConnectionState::Disconnected,
        ConnectionState::Launching,
        ConnectionState::Connecting,
        ConnectionState::Connected,
        ConnectionState::Lost,
        ConnectionState::Error,
        ConnectionState::Reconnecting { attempt: 0 },
    ];

    #[test]
    fn labels_and_codes_are_distinct() {
        let codes: std::collections::HashSet<i32> = ALL_STATES.iter().map(|s| s.as_i32()).collect();
        assert_eq!(codes.len(), ALL_STATES.len());
        assert_eq!(ConnectionState::Connected.label(), "daemon: connected");
        // The state the controller lands in when the event stream ends and it
        // is *not* going to try again: the IDE is quitting, or the first
        // connect never came up. An ordinary loss says "reconnecting" instead.
        assert_eq!(ConnectionState::Lost.label(), "daemon: connection lost");
        assert_eq!(
            compose_status(ConnectionState::Lost, "0.1.0"),
            "daemon: connection lost v0.1.0"
        );
    }

    #[test]
    fn every_reconnect_attempt_shares_one_code() {
        // The property is one integer; the attempt travels in the status text.
        assert_eq!(
            ConnectionState::Reconnecting { attempt: 1 }.as_i32(),
            ConnectionState::Reconnecting { attempt: 47 }.as_i32()
        );
    }

    #[test]
    fn codes_round_trip_and_unknown_codes_are_errors() {
        for state in ALL_STATES {
            assert_eq!(ConnectionState::from_i32(state.as_i32()), state);
        }
        // The attempt is not in the code, so a reconnecting state read back
        // out of the property has no attempt rather than a guessed one.
        assert_eq!(
            ConnectionState::from_i32(ConnectionState::Reconnecting { attempt: 9 }.as_i32()),
            ConnectionState::Reconnecting { attempt: 0 }
        );
        assert_eq!(ConnectionState::from_i32(-1), ConnectionState::Error);
        assert_eq!(ConnectionState::from_i32(99), ConnectionState::Error);
    }

    /// A tracked tab, built the way `reconcile` builds one, for the group
    /// operations below.
    fn tab(id: &str) -> AgentTab {
        AgentTab {
            workspace_id: WorkspaceId(id.to_owned()),
            name: id.to_owned(),
            repo_path: "/repo".to_owned(),
            branch: format!("bs/{id}/work"),
            base_branch: "main".to_owned(),
            status: TabStatus::Working,
            detail: "busy".to_owned(),
            worktree_path: String::new(),
            adapter: AgentAdapterKind::Terminal,
            command: None,
            run_config: None,
            agent_id: None,
            agent_status: None,
            agent_detail: String::new(),
            options_json: String::new(),
            op_error: None,
            attention: String::new(),
        }
    }

    #[test]
    fn a_workspace_error_overlays_the_badge_and_gives_it_back() {
        let mut model = Workspaces::new_default();
        model.add_tab(0, tab("ws_a"));
        let id = WorkspaceId("ws_a".to_owned());

        assert!(model.set_workspace_error(&id, "Merge stopped: conflicts in a.txt"));
        let showing = model.active().expect("a tab");
        assert_eq!(showing.display_status(), TabStatus::Error);
        assert_eq!(
            showing.display_detail(),
            "Merge stopped: conflicts in a.txt"
        );
        // The daemon's own status is untouched underneath, which is what makes
        // clearing the banner an assignment rather than a guess.
        assert_eq!(showing.status, TabStatus::Working);

        assert!(model.clear_workspace_error(&id));
        let showing = model.active().expect("a tab");
        assert_eq!(showing.display_status(), TabStatus::Working);
        assert_eq!(showing.display_detail(), "busy");

        // An id nobody is tracking is refused rather than inventing a tab.
        assert!(!model.set_workspace_error(&WorkspaceId("ws_gone".to_owned()), "x"));
        assert!(!model.clear_workspace_error(&WorkspaceId("ws_gone".to_owned())));
    }

    #[test]
    fn removing_a_group_keeps_its_tabs_in_unsorted() {
        let mut model = Workspaces::new_default();
        let feature = model.add_group("Feature");
        model.add_tab(feature, tab("ws_keep"));
        model.add_tab(feature, tab("ws_other"));
        // The tab that was in front stays in front, in its new home.
        model.set_active(feature, 1);

        assert!(model.remove_group("Feature"));
        assert!(model.groups.iter().all(|g| g.name != "Feature"));
        let unsorted = model
            .groups
            .iter()
            .find(|g| g.name == UNSORTED_GROUP)
            .expect("Unsorted");
        let ids: Vec<&str> = unsorted
            .tabs
            .iter()
            .map(|t| t.workspace_id.as_str())
            .collect();
        assert_eq!(ids, vec!["ws_keep", "ws_other"]);
        assert_eq!(
            model.active().map(|t| t.workspace_id.as_str()),
            Some("ws_other")
        );

        // No such group, and the one group tabs are moved *into*, are both
        // refused rather than half-applied.
        assert!(!model.remove_group("Feature"));
        assert!(!model.remove_group(UNSORTED_GROUP));
    }

    #[test]
    fn removing_the_last_group_leaves_one_to_create_into() {
        let mut model = Workspaces {
            groups: vec![Group {
                id: "grp_0".to_owned(),
                name: "Only".to_owned(),
                tabs: Vec::new(),
            }],
            active_group: 0,
            active_tab: 0,
            next_group_id: 1,
        };
        assert!(model.remove_group("Only"));
        assert_eq!(model.groups.len(), 1);
        assert_eq!(model.groups[0].name, UNSORTED_GROUP);
    }

    #[test]
    fn compose_status_appends_the_version_only_when_present() {
        assert_eq!(
            compose_status(ConnectionState::Connected, ""),
            "daemon: connected"
        );
        assert_eq!(
            compose_status(ConnectionState::Connected, "0.1.0"),
            "daemon: connected v0.1.0"
        );
    }

    #[test]
    fn compose_status_counts_the_reconnect_attempt_and_drops_the_version() {
        // The version belongs to the daemon that just died, so it is left out:
        // "reconnecting v0.1.0" would claim a version nothing has answered
        // with yet.
        assert_eq!(
            compose_status(ConnectionState::Reconnecting { attempt: 1 }, "0.1.0"),
            "daemon: reconnecting (attempt 1)"
        );
        assert_eq!(
            compose_status(ConnectionState::Reconnecting { attempt: 12 }, ""),
            "daemon: reconnecting (attempt 12)"
        );
        // Attempt 0 is what `from_i32` produces; there is no attempt 0 to
        // report, so the text stays bare rather than counting one.
        assert_eq!(
            compose_status(ConnectionState::Reconnecting { attempt: 0 }, "0.1.0"),
            "daemon: reconnecting"
        );
    }
}
