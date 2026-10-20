use bondsymphonic_ide::{client::DaemonClient, qobjects::app_controller::start_agent_request};
use bondsymphonic_proto::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

async fn failed_start(new_workspace: bool, refuse_cleanup: bool) -> (bool, Vec<String>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut methods = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let ClientMessage::Request { id, request } = codec::decode(&line).unwrap();
            methods.push(request.method_name().to_owned());
            let response = match request {
                Request::Hello(_) => ServerMessage::ok(
                    id,
                    &HelloResult {
                        daemon_version: "test".into(),
                        protocol_version: Some(PROTOCOL_VERSION),
                        capabilities: Capabilities {
                            sandbox_backend: "noop".into(),
                            git_protect: false,
                            adapters: vec![],
                            backends: vec![],
                        },
                    },
                ),
                Request::AgentStart(_) => {
                    ServerMessage::err(id, RpcError::internal("start failed"))
                }
                Request::WorkspaceDestroy(params) => {
                    assert_eq!(params.workspace_id.0, "ws_new");
                    assert!(!params.force);
                    if refuse_cleanup {
                        ServerMessage::err(id, RpcError::internal("worktree has edits"))
                    } else {
                        ServerMessage::ok(id, &serde_json::json!({}))
                    }
                }
                _ => panic!("unexpected request"),
            };
            write
                .write_all(codec::encode(&response).as_bytes())
                .await
                .unwrap();
        }
        methods
    });
    let (client, _, events) = DaemonClient::connect(addr, "token", "test").await.unwrap();
    let failure = start_agent_request(
        &client,
        AgentStartParams {
            workspace_id: WorkspaceId("ws_new".into()),
            adapter: AgentAdapterKind::Codex,
            options: serde_json::from_str("{}").unwrap(),
        },
        new_workspace,
    )
    .await
    .expect_err("start must fail");
    assert!(failure.message.contains("start failed"));
    drop(events);
    drop(client);
    (
        failure.workspace_removed,
        server.await.unwrap(),
        failure.message,
    )
}

#[tokio::test]
async fn a_failed_initial_start_rolls_back_the_workspace() {
    let (removed, methods, _) = failed_start(true, false).await;
    assert_eq!(methods, ["hello", "agent.start", "workspace.destroy"]);
    assert!(removed);
}

#[tokio::test]
async fn a_failed_restart_preserves_the_existing_workspace() {
    let (removed, methods, _) = failed_start(false, false).await;
    assert_eq!(methods, ["hello", "agent.start"]);
    assert!(!removed);
}

#[tokio::test]
async fn cleanup_refusal_preserves_the_workspace_and_reports_both_failures() {
    let (removed, methods, message) = failed_start(true, true).await;
    assert_eq!(methods, ["hello", "agent.start", "workspace.destroy"]);
    assert!(!removed);
    assert!(message.contains("start failed"));
    assert!(message.contains("worktree has edits"));
}

/// A start the client gave up on is not a start the daemon refused: the daemon
/// may still be bringing the agent up (a first OpenCode start can outlast the
/// wait), so destroying the new workspace would tear down an agent that is
/// about to exist. Only an error the daemon itself returned rolls back.
#[tokio::test]
async fn a_timed_out_initial_start_keeps_the_new_workspace() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut methods = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let ClientMessage::Request { id, request } = codec::decode(&line).unwrap();
            methods.push(request.method_name().to_owned());
            let response = match request {
                Request::Hello(_) => ServerMessage::ok(
                    id,
                    &HelloResult {
                        daemon_version: "test".into(),
                        protocol_version: Some(PROTOCOL_VERSION),
                        capabilities: Capabilities {
                            sandbox_backend: "noop".into(),
                            git_protect: false,
                            adapters: vec![],
                            backends: vec![],
                        },
                    },
                ),
                // Still starting: never answered within the client's wait.
                Request::AgentStart(_) => continue,
                Request::WorkspaceDestroy(_) => ServerMessage::ok(id, &serde_json::json!({})),
                _ => panic!("unexpected request"),
            };
            write
                .write_all(codec::encode(&response).as_bytes())
                .await
                .unwrap();
        }
        methods
    });
    let (client, _, events) = DaemonClient::connect(addr, "token", "test").await.unwrap();
    let client = client.with_request_timeout(std::time::Duration::from_millis(200));
    let failure = start_agent_request(
        &client,
        AgentStartParams {
            workspace_id: WorkspaceId("ws_new".into()),
            adapter: AgentAdapterKind::OpenCode,
            options: serde_json::from_str("{}").unwrap(),
        },
        true,
    )
    .await
    .expect_err("a start with no answer must not succeed");
    assert!(!failure.workspace_removed);
    assert!(
        failure.message.contains("timed out"),
        "message: {}",
        failure.message
    );
    assert!(
        failure.message.contains("kept"),
        "the message must say the workspace was kept: {}",
        failure.message
    );
    drop(events);
    drop(client);
    assert_eq!(server.await.unwrap(), ["hello", "agent.start"]);
}

#[test]
fn agent_start_waits_as_long_as_a_workspace_create() {
    use bondsymphonic_ide::client::{default_timeout_for, REPOSITORY_REQUEST_TIMEOUT};
    assert!(default_timeout_for("agent.start") >= REPOSITORY_REQUEST_TIMEOUT);
}
