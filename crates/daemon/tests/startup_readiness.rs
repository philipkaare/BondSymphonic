mod common;

use bondsymphonic_daemon::{
    daemon::Daemon,
    sandbox::backend_for,
    server::broadcast::EventBus,
    workspace::{lifecycle, DataDirs},
};
use bondsymphonic_proto::*;

async fn persisted_workspace(root: &std::path::Path) -> WorkspaceId {
    let repo = common::init_repo(root);
    let daemon = Daemon::new(
        DataDirs::new(root.join("data")),
        backend_for("noop"),
        EventBus::new(64),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "restored".into(),
            init_if_missing: false,
            in_place: false,
        },
    )
    .await
    .unwrap();
    daemon.sandbox(&ws.id).unwrap().shutdown().await.unwrap();
    ws.id
}

#[tokio::test]
async fn completed_restore_is_ready_for_a_client_that_missed_its_event() {
    let temp = tempfile::tempdir().unwrap();
    let id = persisted_workspace(temp.path()).await;
    let (port, token, daemon, cancel) = common::start_daemon(&temp.path().join("data")).await;
    daemon.restore_workspaces().await;
    assert!(daemon.sandbox(&id).is_ok());
    // Restore finished before the connection subscribed: no event can announce it.
    let mut client = common::Client::connect(port, &token).await;
    let hello = client
        .call(Request::Hello(HelloParams {
            token,
            client_version: "test".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }))
        .await
        .unwrap();
    let listed: WorkspaceListResult =
        serde_json::from_value(client.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(listed.workspaces[0].state, WorkspaceState::Ready);
    assert!(!client
        .drain_events()
        .iter()
        .any(|(_, event)| matches!(event, Event::WorkspaceStateChanged { .. })));
    assert_eq!(hello["capabilities"]["workspace_ready_snapshot"], true);
    lifecycle::destroy(&daemon, &id, true).await.unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn pending_restore_never_lists_persisted_ready_without_its_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let id = persisted_workspace(temp.path()).await;
    let (port, token, daemon, cancel) = common::start_daemon(&temp.path().join("data")).await;
    let snapshot = lifecycle::restore_snapshot(&daemon);
    assert_eq!(
        daemon.registry.get(&id).unwrap().state,
        WorkspaceState::Ready
    );
    assert!(daemon.sandbox(&id).is_err());
    let mut client = common::Client::connect(port, &token).await;
    let listed: WorkspaceListResult =
        serde_json::from_value(client.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(listed.workspaces[0].state, WorkspaceState::Creating);
    let got: WorkspaceInfo = serde_json::from_value(
        client
            .call(Request::WorkspaceGet(WorkspaceIdParams {
                workspace_id: id.clone(),
            }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(got.state, WorkspaceState::Creating);
    daemon.restore_workspaces_from(snapshot).await;
    assert!(daemon.sandbox(&id).is_ok());
    let listed: WorkspaceListResult =
        serde_json::from_value(client.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(listed.workspaces[0].state, WorkspaceState::Ready);
    lifecycle::destroy(&daemon, &id, true).await.unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn persisted_ready_without_a_live_sandbox_is_not_a_ready_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let id = persisted_workspace(temp.path()).await;
    let (port, token, daemon, cancel) = common::start_daemon(&temp.path().join("data")).await;
    assert!(daemon.sandbox(&id).is_err());
    let mut client = common::Client::connect(port, &token).await;
    let listed: WorkspaceListResult =
        serde_json::from_value(client.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(listed.workspaces[0].state, WorkspaceState::SandboxDown);
    let got: WorkspaceInfo = serde_json::from_value(
        client
            .call(Request::WorkspaceGet(WorkspaceIdParams {
                workspace_id: id.clone(),
            }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(got.state, WorkspaceState::SandboxDown);
    lifecycle::destroy(&daemon, &id, true).await.unwrap();
    cancel.cancel();
}

struct DeadSandbox(tokio::sync::watch::Receiver<bool>);

#[async_trait::async_trait]
impl bondsymphonic_daemon::sandbox::SandboxHandle for DeadSandbox {
    async fn spawn(
        &self,
        _: bondsymphonic_daemon::sandbox::SandboxCommand,
    ) -> Result<bondsymphonic_daemon::sandbox::SandboxChild, RpcError> {
        Err(RpcError::internal("sandbox is dead"))
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        Ok(())
    }
    fn helper_exe(&self) -> std::path::PathBuf {
        std::path::PathBuf::new()
    }
    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn dead_sandbox_is_not_ready_before_its_registry_state_catches_up() {
    let temp = tempfile::tempdir().unwrap();
    let id = persisted_workspace(temp.path()).await;
    let (port, token, daemon, cancel) = common::start_daemon(&temp.path().join("data")).await;
    let (_death, receiver) = tokio::sync::watch::channel(true);
    daemon
        .sandboxes
        .lock()
        .insert(id.clone(), std::sync::Arc::new(DeadSandbox(receiver)));
    assert!(daemon.sandbox(&id).is_ok());
    assert_eq!(
        daemon.registry.get(&id).unwrap().state,
        WorkspaceState::Ready
    );
    let mut client = common::Client::connect(port, &token).await;
    let listed: WorkspaceListResult =
        serde_json::from_value(client.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(listed.workspaces[0].state, WorkspaceState::SandboxDown);
    let got: WorkspaceInfo = serde_json::from_value(
        client
            .call(Request::WorkspaceGet(WorkspaceIdParams {
                workspace_id: id.clone(),
            }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(got.state, WorkspaceState::SandboxDown);
    lifecycle::destroy(&daemon, &id, true).await.unwrap();
    cancel.cancel();
}
