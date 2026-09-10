//! The tab model: groups of agent tabs, exposed to the Qt widgets as one
//! serialised JSON string plus a handful of cheap accessors.
//!
//! [`Workspaces`] is the state of record; the `state_json` property is a
//! cached serialisation of it, refreshed after every mutation so a view can
//! rebuild itself from a single property read. The accessors below exist so
//! the C++ side never has to parse that JSON just to paint a tab: it asks for
//! the label, the tooltip, the status word and the counts directly.

use crate::model::app_state::{parse_agent_state, AgentTab, TabStatus, Workspaces};
use crate::model::persistence::PersistedGroups;
use bondsymphonic_proto::{AgentAdapterKind, AgentId, WorkspaceId, WorkspaceInfo, WorkspaceState};

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

        /// Replaces the whole model from a serialised `Workspaces` (session
        /// restore). An unparseable or empty string installs the default
        /// single-group model instead. Callers must use this rather than
        /// `setStateJson`, which only overwrites the cached string.
        #[qinvokable]
        fn load_state(self: Pin<&mut GroupModel>, json: QString) -> bool;

        /// Installs the model the controller rebuilt from `state.json` and the
        /// daemon's first workspace list (`AppController::workspacesRestored`),
        /// before any `reconcile`.
        ///
        /// Distinct from `loadState`, which is the session-file path and falls
        /// back to the default single-group model. A restore that will not
        /// parse must leave what is on screen alone instead: the alternative is
        /// throwing away the user's groups because one string was malformed.
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
        /// active. A workspace that is already tracked is refreshed in place
        /// instead of being duplicated. `adapter` is "claude" or "terminal";
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

        /// The active tab workspace id, or empty when there is no tab.
        #[qinvokable]
        fn active_workspace_id(self: &GroupModel) -> QString;

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

        /// The tab status as a word ("idle", "working", "waiting", "error",
        /// "done", "creating", "sandbox down"), so the C++ side never switches
        /// on the numeric code. Empty for an unknown tab.
        #[qinvokable]
        fn status_word(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// Multi-line tooltip: name, repo, branch, adapter, status, and the
        /// daemon error detail when there is one.
        #[qinvokable]
        fn tab_tooltip(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The workspace id of one tab, or empty for an unknown tab.
        #[qinvokable]
        fn tab_workspace_id(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The agent id of one tab, or empty when it is not running an agent.
        /// This is how a restored session re-attaches a transcript to an agent
        /// that is still alive in the daemon.
        #[qinvokable]
        fn tab_agent_id(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

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

        /// The run configuration recorded for one tab, or empty when it has
        /// none.
        #[qinvokable]
        fn tab_run_config(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The worktree path of one tab, or empty for an unknown tab. The Run
        /// panel detects run configurations against this path.
        #[qinvokable]
        fn tab_worktree_path(self: &GroupModel, group_idx: i32, tab_idx: i32) -> QString;

        /// The active tab's worktree path, or empty when there is no tab. What
        /// `MainWindow` hands to `RunPanelModel::setWorkspace`.
        #[qinvokable]
        fn active_worktree_path(self: &GroupModel) -> QString;
    }
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;

pub struct GroupModelRust {
    state_json: QString,
    workspaces: Workspaces,
}

impl Default for GroupModelRust {
    fn default() -> Self {
        let workspaces = Workspaces::new_default();
        Self {
            state_json: QString::from(&workspaces.to_json()),
            workspaces,
        }
    }
}

/// Qt hands indices in as `i32`; anything negative is simply out of range.
fn index(value: i32) -> Option<usize> {
    usize::try_from(value).ok()
}

/// The serialised name of an adapter kind, as the daemon spells it.
fn adapter_name(kind: AgentAdapterKind) -> &'static str {
    match kind {
        AgentAdapterKind::Claude => "claude",
        AgentAdapterKind::Terminal => "terminal",
    }
}

/// Parses an adapter name coming from the UI. Anything unrecognised is a plain
/// terminal, which is the adapter that always works.
fn parse_adapter(name: &str) -> AgentAdapterKind {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" => AgentAdapterKind::Claude,
        _ => AgentAdapterKind::Terminal,
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

/// The `Error(detail)` text of a workspace state, or empty for any other state.
fn state_detail(state: &WorkspaceState) -> String {
    match state {
        WorkspaceState::Error(detail) => detail.clone(),
        _ => String::new(),
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
        let workspaces = &self.rust().workspaces;
        let reported = PersistedGroups {
            groups: workspaces.persisted_groups(),
            active_workspace: workspaces.active().map(|tab| tab.workspace_id.0.clone()),
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
        // A workspace the model already tracks (because `reconcile` filed it
        // into "Unsorted" as a plain terminal before the create call answered)
        // is refreshed, moved into the group the user asked for, and given the
        // adapter and command they chose, rather than added a second time.
        if self.as_ref().rust().workspaces.find(&info.id).is_some() {
            let adapter = adapter.to_string();
            let command = command.to_string();
            let options = options_json.to_string();
            {
                let mut rust = self.as_mut().rust_mut();
                rust.workspaces.apply_workspace_info(&info);
                if !name.is_empty() {
                    rust.workspaces.move_tab_to_group(&info.id, &name);
                }
                if let Some((g, t)) = rust.workspaces.find(&info.id) {
                    {
                        let tab = &mut rust.workspaces.groups[g].tabs[t];
                        // Empty means "not supplied": the caller is echoing a
                        // signal that carries no adapter or command.
                        if !adapter.is_empty() {
                            tab.adapter = parse_adapter(&adapter);
                        }
                        if !command.is_empty() {
                            tab.command = Some(command);
                        }
                        if !options.is_empty() {
                            tab.options_json = options;
                        }
                    }
                    rust.workspaces.set_active(g, t);
                }
            }
            self.publish();
            return true;
        }

        let existing = self
            .as_ref()
            .rust()
            .workspaces
            .groups
            .iter()
            .position(|g| g.name == name);
        let group_idx = match existing {
            Some(idx) => idx,
            None => self.as_mut().rust_mut().workspaces.add_group(&name),
        };
        let command = command.to_string();
        let tab = AgentTab {
            workspace_id: info.id.clone(),
            name: info.name.clone(),
            repo_path: info.repo_path.clone(),
            branch: info.branch.clone(),
            base_branch: info.base_branch.clone(),
            status: TabStatus::from_workspace_state(&info.state),
            detail: state_detail(&info.state),
            worktree_path: info.worktree_path.clone(),
            adapter: parse_adapter(&adapter.to_string()),
            command: (!command.is_empty()).then_some(command),
            run_config: None,
            agent_id: None,
            agent_status: None,
            agent_detail: String::new(),
            options_json: options_json.to_string(),
            op_error: None,
        };
        self.as_mut().rust_mut().workspaces.add_tab(group_idx, tab);
        self.publish();
        true
    }

    pub fn apply_workspace_info(mut self: Pin<&mut Self>, info_json: QString) -> bool {
        let Ok(info) = serde_json::from_str::<WorkspaceInfo>(&info_json.to_string()) else {
            tracing::warn!("apply_workspace_info: unparseable workspace info");
            return false;
        };
        let applied = self
            .as_mut()
            .rust_mut()
            .workspaces
            .apply_workspace_info(&info)
            .is_some();
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
        let ok = self.as_mut().rust_mut().workspaces.set_agent(&ws, agent);
        if ok {
            self.publish();
        }
        ok
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
        let applied = self
            .as_mut()
            .rust_mut()
            .workspaces
            .set_agent_status(&agent, status, &detail.to_string())
            .is_some();
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

    pub fn active_workspace_id(&self) -> QString {
        match self.rust().workspaces.active() {
            Some(tab) => QString::from(tab.workspace_id.as_str()),
            None => QString::from(""),
        }
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

    // The three below report the tab's *displayed* status, which is the
    // daemon's own unless a failed merge, pull request or discard is showing
    // on it. `stateJson` keeps carrying the underlying one, so nothing that
    // reads the model back loses what the daemon actually said.
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
            Some(tab) => QString::from(status_text(tab.display_status())),
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
        let detail = tab.display_detail();
        if !detail.is_empty() {
            text.push('\n');
            text.push_str(detail);
        }
        QString::from(&text)
    }

    pub fn tab_workspace_id(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self.tab_at(group_idx, tab_idx) {
            Some(tab) => QString::from(tab.workspace_id.as_str()),
            None => QString::from(""),
        }
    }

    pub fn tab_agent_id(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self
            .tab_at(group_idx, tab_idx)
            .and_then(|t| t.agent_id.as_ref())
        {
            Some(id) => QString::from(id.as_str()),
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

    pub fn tab_run_config(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self
            .tab_at(group_idx, tab_idx)
            .and_then(|t| t.run_config.as_deref())
        {
            Some(name) => QString::from(name),
            None => QString::from(""),
        }
    }

    pub fn tab_worktree_path(&self, group_idx: i32, tab_idx: i32) -> QString {
        match self.tab_at(group_idx, tab_idx) {
            Some(tab) => QString::from(&tab.worktree_path),
            None => QString::from(""),
        }
    }

    pub fn active_worktree_path(&self) -> QString {
        match self.rust().workspaces.active() {
            Some(tab) => QString::from(&tab.worktree_path),
            None => QString::from(""),
        }
    }

    fn tab_at(&self, group_idx: i32, tab_idx: i32) -> Option<&AgentTab> {
        let (g, t) = (index(group_idx)?, index(tab_idx)?);
        self.rust().workspaces.groups.get(g)?.tabs.get(t)
    }

    /// Re-serialises the model into `state_json` and announces the change.
    /// Every mutation ends here, so the two never drift apart.
    fn publish(mut self: Pin<&mut Self>) {
        let json = self.as_ref().rust().workspaces.to_json();
        self.as_mut().set_state_json(QString::from(&json));
        self.changed();
    }
}
