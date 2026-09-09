//! The pure-Rust half of the QObject invokables.
//!
//! `GroupModel` is a thin shell over [`Workspaces`]: every invokable parses
//! JSON, calls one method, and re-serialises. The QObject wrappers themselves
//! need a Qt event loop, so they are exercised by the widgets in later tasks;
//! what is testable here is that the JSON the invokables move across the
//! boundary survives the round trip, including the state the widgets read back
//! out of `state_json`.

use bondsymphonic_ide::model::app_state::{AgentTab, TabStatus, Workspaces};
use bondsymphonic_ide::model::file_tree::FileTree;
use bondsymphonic_proto::{
    AgentAdapterKind, FileEntry, FileStatus, WorkspaceId, WorkspaceInfo, WorkspaceState,
};

fn info(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}"),
        worktree_path: format!("/wt/{name}"),
        created_at: "2026-09-09T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        state,
        agents: Vec::new(),
        runs: Vec::new(),
    }
}

fn tab(info: &WorkspaceInfo) -> AgentTab {
    AgentTab {
        workspace_id: info.id.clone(),
        name: info.name.clone(),
        repo_path: info.repo_path.clone(),
        branch: info.branch.clone(),
        status: TabStatus::from_workspace_state(&info.state),
        detail: String::new(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
    }
}

/// `state_json` is written after every mutation and read back by the views:
/// the model has to survive that round trip unchanged, active selection and
/// all.
#[test]
fn state_json_round_trips_a_populated_model() {
    let mut model = Workspaces::new_default();
    let idx = model.add_group("Backend");
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let two = info("ws_2", "beta", WorkspaceState::Creating);
    model.add_tab(0, tab(&one));
    model.add_tab(idx, tab(&two));
    assert!(model.set_active(0, 0));

    let restored = Workspaces::from_json(&model.to_json()).expect("state_json parses");
    assert_eq!(restored, model);
    assert_eq!(restored.groups.len(), 2);
    assert_eq!(restored.active().map(|t| t.name.as_str()), Some("alpha"));
}

/// What `load_state` does with a session file that is missing or corrupt.
#[test]
fn unparseable_state_json_is_rejected_rather_than_guessed() {
    assert!(Workspaces::from_json("").is_none());
    assert!(Workspaces::from_json("not json").is_none());
    assert!(Workspaces::from_json("{\"groups\":[]}").is_none());
    // The fallback the invokable installs instead.
    let fallback = Workspaces::new_default();
    assert_eq!(fallback.groups.len(), 1);
    assert!(fallback.groups[0].tabs.is_empty());
}

/// `add_tab` and `apply_workspace_info` are handed a serialised
/// `WorkspaceInfo` from the controller's signals.
#[test]
fn workspace_info_json_drives_tab_status_and_detail() {
    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    let json = serde_json::to_string(&ready).expect("info serialises");
    let parsed: WorkspaceInfo = serde_json::from_str(&json).expect("info parses");
    assert_eq!(parsed.id, ready.id);

    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&parsed));
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Idle);

    let broken = info("ws_1", "alpha", WorkspaceState::Error("disk full".into()));
    let json = serde_json::to_string(&broken).expect("info serialises");
    let parsed: WorkspaceInfo = serde_json::from_str(&json).expect("info parses");
    assert!(model.apply_workspace_info(&parsed).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(model.groups[0].tabs[0].detail, "disk full");
}

/// `reconcile` receives the controller's `workspaces_listed` payload verbatim.
#[test]
fn reconcile_accepts_a_serialised_workspace_list() {
    let list = vec![
        info("ws_1", "alpha", WorkspaceState::Ready),
        info("ws_2", "beta", WorkspaceState::SandboxDown),
    ];
    let json = serde_json::to_string(&list).expect("list serialises");
    let parsed: Vec<WorkspaceInfo> = serde_json::from_str(&json).expect("list parses");

    let mut model = Workspaces::new_default();
    model.reconcile(&parsed);
    let names: Vec<&str> = model
        .groups
        .iter()
        .flat_map(|g| g.tabs.iter())
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert!(model.find(&WorkspaceId("ws_2".into())).is_some());

    // An empty list drops everything, which is what a fresh daemon reports.
    model.reconcile(&[]);
    assert_eq!(model.groups.iter().map(|g| g.tabs.len()).sum::<usize>(), 0);
    assert!(model.active().is_none());
}

/// `active_tab_json` hands one tab to the widgets; it has to parse back.
#[test]
fn active_tab_json_round_trips() {
    let mut model = Workspaces::new_default();
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    model.add_tab(0, tab(&one));
    let active = model.active().expect("a tab is active");
    let json = serde_json::to_string(active).expect("tab serialises");
    let parsed: AgentTab = serde_json::from_str(&json).expect("tab parses");
    assert_eq!(&parsed, active);
}

/// `cached_entries` returns what `FileTreeModel` stored from `fs.list_dir`.
#[test]
fn file_entries_json_round_trips() {
    let entries = vec![
        FileEntry {
            name: "src".to_owned(),
            is_dir: true,
            size: 0,
            status: FileStatus::Unchanged,
        },
        FileEntry {
            name: "README.md".to_owned(),
            is_dir: false,
            size: 42,
            status: FileStatus::Modified,
        },
    ];
    let mut tree = FileTree::new();
    tree.set_dir("", entries.clone());
    assert!(tree.is_loaded(""));

    let json = FileTree::entries_json(tree.get_dir("").expect("root is cached"));
    let parsed: Vec<FileEntry> = serde_json::from_str(&json).expect("entries parse");
    assert_eq!(parsed, entries);

    // The empty array `cached_entries` returns for a directory that has not
    // been listed yet.
    assert!(tree.get_dir("src").is_none());
    let empty: Vec<FileEntry> = serde_json::from_str("[]").expect("empty entries parse");
    assert!(empty.is_empty());
}
