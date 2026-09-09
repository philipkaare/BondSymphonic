//! Pure-Rust application state. This module must never import Qt types.

use bondsymphonic_proto::{
    AgentAdapterKind, AgentId, AgentState, WorkspaceId, WorkspaceInfo, WorkspaceState,
};
use serde::{Deserialize, Serialize};

/// Lifecycle of the IDE's connection to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Launching,
    Connecting,
    Connected,
    /// The connection ended. Nothing reconnects in this milestone, so this is
    /// a terminal state: every operation fails immediately until the IDE is
    /// restarted.
    Lost,
    Error,
}

impl ConnectionState {
    /// Stable numeric code exposed to C++/QML as the `connection_state` property.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Disconnected => 0,
            Self::Launching => 1,
            Self::Connecting => 2,
            Self::Connected => 3,
            Self::Lost => 4,
            Self::Error => 5,
        }
    }

    /// Inverse of [`ConnectionState::as_i32`]. Codes outside 0..=5 map to
    /// [`ConnectionState::Error`], since the `connection_state` property is
    /// writable from C++ and may hold anything.
    pub fn from_i32(code: i32) -> Self {
        match code {
            0 => Self::Disconnected,
            1 => Self::Launching,
            2 => Self::Connecting,
            3 => Self::Connected,
            4 => Self::Lost,
            _ => Self::Error,
        }
    }

    /// Human readable text for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "daemon: not started",
            Self::Launching => "daemon: launching",
            Self::Connecting => "daemon: connecting",
            Self::Connected => "daemon: connected",
            Self::Lost => "daemon: connection lost",
            Self::Error => "daemon: error",
        }
    }
}

/// Builds the status bar text from a connection label and the daemon version.
/// An empty version contributes nothing. This is the only place the two are
/// combined, so the C++ shell never has to branch on the version.
pub fn compose_status(label: &str, version: &str) -> String {
    if version.is_empty() {
        label.to_owned()
    } else {
        format!("{label} v{version}")
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
    pub status: TabStatus,
    /// `Error(detail)` text from the daemon, or empty for any other status.
    pub detail: String,
    pub adapter: AgentAdapterKind,
    pub command: Option<String>,
    /// The agent running in this tab, once `agent.start` has answered.
    /// Defaulted rather than required so a session saved before agent tabs
    /// existed still loads.
    #[serde(default)]
    pub agent_id: Option<AgentId>,
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
}

impl Workspaces {
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
        }
    }

    /// Appends a new, empty group and returns its index.
    pub fn add_group(&mut self, name: &str) -> usize {
        let id = format!("grp_{}", self.groups.len());
        self.groups.push(Group {
            id,
            name: name.to_owned(),
            tabs: Vec::new(),
        });
        self.groups.len() - 1
    }

    pub fn rename_group(&mut self, idx: usize, name: &str) -> bool {
        match self.groups.get_mut(idx) {
            Some(g) => {
                g.name = name.to_owned();
                true
            }
            None => false,
        }
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
    /// A tab running an agent keeps its agent-driven status while its
    /// workspace is `Ready`: the daemon emits `workspace.state` for reasons
    /// that have nothing to do with the agent (a sandbox restart, a branch
    /// change), and letting one of those overwrite "working" with "idle" would
    /// blank the tab's indicator mid-turn. Anything other than `Ready` is news
    /// about the workspace the agent runs in, and does win.
    pub fn apply_workspace_info(&mut self, info: &WorkspaceInfo) -> Option<(usize, usize)> {
        let (g, t) = self.find(&info.id)?;
        let tab = &mut self.groups[g].tabs[t];
        let agent_owns_status =
            tab.agent_id.is_some() && matches!(info.state, WorkspaceState::Ready);
        if !agent_owns_status {
            tab.status = TabStatus::from_workspace_state(&info.state);
            tab.detail = match &info.state {
                WorkspaceState::Error(detail) => detail.clone(),
                _ => String::new(),
            };
        }
        tab.branch = info.branch.clone();
        Some((g, t))
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
            let group_idx = match self.groups.iter().position(|g| g.name == "Unsorted") {
                Some(idx) => idx,
                None => self.add_group("Unsorted"),
            };
            self.groups[group_idx].tabs.push(AgentTab {
                workspace_id: info.id.clone(),
                name: info.name.clone(),
                repo_path: info.repo_path.clone(),
                branch: info.branch.clone(),
                status: TabStatus::from_workspace_state(&info.state),
                detail: match &info.state {
                    WorkspaceState::Error(detail) => detail.clone(),
                    _ => String::new(),
                },
                adapter: AgentAdapterKind::Terminal,
                command: None,
                agent_id: None,
            });
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

    const ALL_STATES: [ConnectionState; 6] = [
        ConnectionState::Disconnected,
        ConnectionState::Launching,
        ConnectionState::Connecting,
        ConnectionState::Connected,
        ConnectionState::Lost,
        ConnectionState::Error,
    ];

    #[test]
    fn labels_and_codes_are_distinct() {
        let codes: std::collections::HashSet<i32> = ALL_STATES.iter().map(|s| s.as_i32()).collect();
        assert_eq!(codes.len(), ALL_STATES.len());
        assert_eq!(ConnectionState::Connected.label(), "daemon: connected");
        // The state the controller lands in when the event stream ends. It has
        // to say the connection is gone: nothing reconnects, so a status bar
        // promising a reconnect would be a lie the user waits on.
        assert_eq!(ConnectionState::Lost.label(), "daemon: connection lost");
        assert_eq!(
            compose_status(ConnectionState::Lost.label(), "0.1.0"),
            "daemon: connection lost v0.1.0"
        );
    }

    #[test]
    fn codes_round_trip_and_unknown_codes_are_errors() {
        for state in ALL_STATES {
            assert_eq!(ConnectionState::from_i32(state.as_i32()), state);
        }
        assert_eq!(ConnectionState::from_i32(-1), ConnectionState::Error);
        assert_eq!(ConnectionState::from_i32(99), ConnectionState::Error);
    }

    #[test]
    fn compose_status_appends_the_version_only_when_present() {
        assert_eq!(compose_status("daemon: connected", ""), "daemon: connected");
        assert_eq!(
            compose_status("daemon: connected", "0.1.0"),
            "daemon: connected v0.1.0"
        );
    }
}
