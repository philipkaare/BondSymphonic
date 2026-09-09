use bondsymphonic_ide::model::app_state::*;
use bondsymphonic_ide::model::file_tree::*;
use bondsymphonic_proto::*;

fn info(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo {
        id: id.into(),
        name: name.into(),
        repo_path: "/r".into(),
        base_branch: "main".into(),
        branch: format!("bs/{name}/work"),
        worktree_path: "/w".into(),
        created_at: "t".into(),
        allowlist: vec![],
        state,
        agents: vec![],
        runs: vec![],
    }
}
fn tab(id: &str, name: &str) -> AgentTab {
    AgentTab {
        workspace_id: id.into(),
        name: name.into(),
        repo_path: "/r".into(),
        branch: format!("bs/{name}/work"),
        status: TabStatus::Creating,
        detail: String::new(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
    }
}

#[test]
fn groups_tabs_and_active_selection() {
    let mut w = Workspaces::new_default();
    assert_eq!(w.groups[0].name, "Default");
    let g = w.add_group("Frontend");
    assert_eq!(w.add_tab(g, tab("ws_1", "a")), (1, 0));
    assert_eq!(w.add_tab(0, tab("ws_2", "b")), (0, 0));
    assert_eq!(w.active().unwrap().name, "b");
    assert!(w.set_active(1, 0));
    assert_eq!(w.active().unwrap().workspace_id, WorkspaceId::from("ws_1"));
    assert!(w.remove_workspace(&"ws_1".into()));
    assert!(
        w.active().is_some(),
        "active must fall back to an existing tab"
    );
    assert!(!w.remove_workspace(&"ws_1".into()));
    let json = w.to_json();
    assert_eq!(Workspaces::from_json(&json).unwrap(), w);
}

#[test]
fn workspace_info_updates_status_and_reconcile_drops_and_adds() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "a"));
    assert_eq!(
        w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Ready)),
        Some((0, 0))
    );
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Idle);
    assert_eq!(
        w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Error("boom".into()))),
        Some((0, 0))
    );
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(w.groups[0].tabs[0].detail, "boom");
    assert_eq!(
        w.apply_workspace_info(&info("ws_9", "z", WorkspaceState::Ready)),
        None
    );

    w.reconcile(&[info("ws_9", "z", WorkspaceState::SandboxDown)]);
    assert!(
        w.find(&"ws_1".into()).is_none(),
        "ws_1 gone from daemon -> dropped"
    );
    let (g, t) = w.find(&"ws_9".into()).unwrap();
    assert_eq!(w.groups[g].name, "Unsorted");
    assert_eq!(w.groups[g].tabs[t].status, TabStatus::SandboxDown);
}

#[test]
fn tab_status_glyphs_and_mapping() {
    assert_eq!(
        TabStatus::from_workspace_state(&WorkspaceState::Creating),
        TabStatus::Creating
    );
    assert_eq!(
        TabStatus::from_workspace_state(&WorkspaceState::Destroying),
        TabStatus::Done
    );
    let all = [
        TabStatus::Idle,
        TabStatus::Working,
        TabStatus::WaitingPermission,
        TabStatus::Error,
        TabStatus::Done,
        TabStatus::Creating,
        TabStatus::SandboxDown,
    ];
    let glyphs: std::collections::HashSet<&str> = all.iter().map(|s| s.glyph()).collect();
    assert_eq!(glyphs.len(), all.len());
}

#[test]
fn file_tree_cache_paths_and_invalidation() {
    let mut t = FileTree::new();
    assert!(!t.is_loaded(""));
    let e = |n: &str, d: bool| FileEntry {
        name: n.into(),
        is_dir: d,
        size: 0,
        status: FileStatus::Unchanged,
    };
    t.set_dir("", vec![e("src", true), e("README.md", false)]);
    t.set_dir("src", vec![e("main.rs", false)]);
    assert_eq!(FileTree::child_path("", "src"), "src");
    assert_eq!(FileTree::child_path("src", "main.rs"), "src/main.rs");
    assert!(t.is_loaded("src"));
    t.invalidate("");
    assert!(!t.is_loaded("") && !t.is_loaded("src"));
    let json = FileTree::entries_json(&[e("a", true)]);
    assert!(json.contains("\"is_dir\":true"));
}
