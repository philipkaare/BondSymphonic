mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::DataDirs;
use bondsymphonic_proto::*;
use common::{start_daemon, Client};

#[tokio::test]
async fn create_list_get_status_destroy_roundtrip_with_events() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let info: RepoInfo = serde_json::from_value(
        c.call(Request::RepoInspect(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(info.default_branch, "main");

    let id = c
        .send(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await;
    let mut events = Vec::new();
    let ws: WorkspaceInfo =
        serde_json::from_value(c.recv_response(id, &mut events).await.unwrap()).unwrap();
    assert_eq!(ws.branch, "bs/agent-1/work");
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(std::path::Path::new(&ws.worktree_path)
        .join("README.md")
        .exists());
    let states: Vec<String> = events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } => Some(format!("{:?}", info.state)),
            _ => None,
        })
        .collect();
    assert_eq!(states, vec!["Creating", "Ready"]);

    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(list.workspaces.len(), 1);
    let got: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(got.name, "agent-1");

    // Duplicate name in the same repo is a Conflict.
    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // Status sees an untracked file, and destroy without force refuses while dirty.
    std::fs::write(std::path::Path::new(&ws.worktree_path).join("wip.txt"), "x").unwrap();
    let st: WorkspaceStatusResult = serde_json::from_value(
        c.call(Request::WorkspaceStatus(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(st
        .entries
        .iter()
        .any(|e| e.path == "wip.txt" && e.status == FileStatus::Untracked));
    let err = c
        .call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(!std::path::Path::new(&ws.worktree_path).exists());
    assert!(daemon.registry.list().is_empty());
    let err = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

#[tokio::test]
async fn restore_marks_missing_worktree_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, _daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "a".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    cancel.cancel();
    std::fs::remove_dir_all(&ws.worktree_path).unwrap();

    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(&data),
        backend_for("noop"),
        server.event_bus(),
    )
    .unwrap();
    daemon.restore().await;
    let w = daemon.registry.get(&ws.id).unwrap();
    assert!(
        matches!(w.state, WorkspaceState::Error(_)),
        "got {:?}",
        w.state
    );
}
