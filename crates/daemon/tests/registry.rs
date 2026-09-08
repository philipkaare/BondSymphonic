use bondsymphonic_daemon::workspace::registry::Registry;
use bondsymphonic_daemon::workspace::Workspace;
use bondsymphonic_proto::WorkspaceState;

fn sample(id: &str, name: &str) -> Workspace {
    Workspace {
        id: id.into(),
        name: name.into(),
        repo_path: "/repo".into(),
        base_branch: "main".into(),
        branch: Workspace::branch_for(name),
        worktree_path: format!("/data/worktrees/{id}").into(),
        created_at: "2026-09-08T10:00:00Z".into(),
        allowlist: vec![],
        state: WorkspaceState::Ready,
        agents: vec![],
        runs: vec![],
    }
}

#[test]
fn branch_naming_convention() {
    assert_eq!(Workspace::branch_for("agent-1"), "bs/agent-1/work");
}

#[test]
fn registry_roundtrips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    assert!(reg.list().is_empty());
    reg.insert(sample("ws_00000001", "a")).unwrap();
    reg.insert(sample("ws_00000002", "b")).unwrap();
    assert_eq!(reg.list().len(), 2);

    let reg2 = Registry::load(&path).unwrap();
    let names: Vec<String> = reg2.list().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["a", "b"]);
    assert_eq!(reg2.get(&"ws_00000001".into()).unwrap().branch, "bs/a/work");
}

#[test]
fn update_and_remove_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    reg.insert(sample("ws_00000001", "a")).unwrap();
    let updated = reg
        .update(&"ws_00000001".into(), |w| {
            w.state = WorkspaceState::Error("boom".into())
        })
        .unwrap();
    assert_eq!(updated.state, WorkspaceState::Error("boom".into()));
    assert!(reg.remove(&"ws_00000001".into()).unwrap().is_some());
    let reg2 = Registry::load(&path).unwrap();
    assert!(reg2.list().is_empty());
    assert!(reg.update(&"ws_nope".into(), |_| {}).is_err());
}

#[test]
fn ids_have_prefix_and_hex() {
    let id = bondsymphonic_daemon::ids::new_id("ws_");
    assert!(id.starts_with("ws_"));
    assert_eq!(id.len(), 11);
    assert!(id[3..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(id, bondsymphonic_daemon::ids::new_id("ws_"));
}
