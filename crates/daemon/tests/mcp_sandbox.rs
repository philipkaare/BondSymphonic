use async_trait::async_trait;
use bondsymphonic_daemon::mcp::{bridge::read_frame, broker::native_supplement};
use bondsymphonic_daemon::{
    mcp::{auth::RemoteMcpManager, bridge::write_message, broker::McpBroker, McpManager},
    sandbox::{SandboxChild, SandboxCommand, SandboxHandle},
};
use bondsymphonic_proto::AgentAdapterKind;
use bondsymphonic_proto::*;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

struct FakeSandbox {
    commands: Mutex<Vec<SandboxCommand>>,
    killed: Arc<AtomicUsize>,
    death: tokio::sync::watch::Sender<bool>,
}
impl FakeSandbox {
    fn new() -> Arc<Self> {
        let (death, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            commands: Mutex::new(vec![]),
            killed: Arc::new(AtomicUsize::new(0)),
            death,
        })
    }
}
#[async_trait]
impl SandboxHandle for FakeSandbox {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        self.commands.lock().unwrap().push(cmd);
        let (host, child) = tokio::io::duplex(16384);
        let (out, input) = tokio::io::split(host);
        let (read, mut write) = tokio::io::split(child);
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let (exit, exit_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            let exchange = async {
                while let Ok(Some(frame)) = read_frame(&mut reader, 16384).await {
                    let m: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                    if m.get("id").is_none() || m["method"] == "stall" {
                        continue;
                    }
                    let result = if m["method"] == "initialize" {
                        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}})
                    } else {
                        json!({"tools":[{"name":"read_example"}]})
                    };
                    if write_message(
                        &mut write,
                        &json!({"jsonrpc":"2.0","id":m["id"],"result":result}),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
            };
            tokio::select! {_=stopped.cancelled()=>{},_=exchange=>{}}
            let _ = exit.send(0);
        });
        let killed = self.killed.clone();
        Ok(SandboxChild {
            pid: 1,
            stdin: Some(Box::pin(input)),
            stdout: Some(Box::pin(out)),
            stderr: Some(Box::pin(tokio::io::empty())),
            pty: None,
            exit: exit_rx,
            killer: Box::new(move || {
                killed.fetch_add(1, Ordering::SeqCst);
                cancel.cancel();
            }),
            signal: Box::new(|_| {}),
        })
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        self.death.send_replace(true);
        Ok(())
    }
    fn helper_exe(&self) -> PathBuf {
        PathBuf::from("/opt/bs/helper")
    }
    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        Some(self.death.subscribe())
    }
}
fn setup() -> (
    tempfile::TempDir,
    Arc<McpManager>,
    Arc<McpBroker>,
    Arc<FakeSandbox>,
) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(McpManager::load(dir.path().into()).unwrap());
    registry
        .upsert(McpUpsertParams {
            server: McpServerConfig {
                id: "example".into(),
                name: "Example".into(),
                transport: McpTransportConfig::Stdio {
                    command: "fake-server".into(),
                    args: vec!["--stdio".into()],
                    env: [("MODE".into(), "test".into())].into(),
                    env_secrets: [("API_TOKEN".into(), "local-token".into())].into(),
                },
                enabled: true,
                default_workspace_enabled: true,
                enabled_backends: vec!["claude".into(), "codex".into()],
            },
            secrets: vec![McpSecretWrite::Set {
                reference: "local-token".into(),
                value: McpSecretValue("LOCAL_SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    let remote = Arc::new(RemoteMcpManager::new(registry.clone()));
    let broker = Arc::new(McpBroker::new(registry.clone(), remote));
    (dir, registry, broker, FakeSandbox::new())
}
async fn connect(
    broker: &Arc<McpBroker>,
    binding: Arc<bondsymphonic_daemon::mcp::broker::McpBinding>,
    token: &str,
) -> (
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<Result<(), RpcError>>,
) {
    let (client, server) = tokio::io::duplex(16384);
    let (read, mut write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let broker = broker.clone();
    let task = tokio::spawn(async move { broker.serve(binding, server_read, server_write).await });
    write_message(&mut write, &json!({"token":token}))
        .await
        .unwrap();
    (BufReader::new(read), write, task)
}
async fn request(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    method: &str,
) -> serde_json::Value {
    write_message(writer, &json!({"jsonrpc":"2.0","id":"x","method":method}))
        .await
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), read_frame(reader, 16384))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_slice(&frame).unwrap()
}

#[tokio::test]
async fn cross_workspace_bridge_rejected() {
    let (_dir, _registry, broker, handle) = setup();
    let a = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let b = broker
        .bind(
            "ws_b".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (_read, _write, task) = connect(&broker, a, b.token()).await;
    assert!(task.await.unwrap().is_err());
    assert!(handle.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stdio_uses_sandbox_environment() {
    let (_dir, _registry, broker, handle) = setup();
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
    assert_eq!(
        request(&mut read, &mut write, "tools/list").await["result"]["tools"][0]["name"],
        "read_example"
    );
    {
        let commands = handle.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].argv, vec!["fake-server", "--stdio"]);
        assert!(commands[0]
            .env
            .contains(&("API_TOKEN".into(), "LOCAL_SECRET_SENTINEL".into())));
        assert_eq!(commands[0].cwd, Some(PathBuf::from("/repo")));
    }
    drop(write);
    drop(read);
    task.await.unwrap().unwrap();
    assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
    assert_eq!(
        broker
            .list()
            .statuses
            .into_iter()
            .find(|s| s.server_id.as_str() == "example")
            .unwrap()
            .state,
        McpConnectionState::Disconnected
    );
}

#[tokio::test]
async fn disable_closes_live_session() {
    let (_dir, registry, broker, handle) = setup();
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
    request(&mut read, &mut write, "tools/list").await;
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":"pending","method":"stall"}),
    )
    .await
    .unwrap();
    registry
        .set_override(
            McpOverrideParams {
                server_id: "example".into(),
                workspace_id: "ws_a".into(),
                value: McpWorkspaceOverride::Disabled,
            },
            None,
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(binding.cancelled());
    assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
    assert!(read_frame(&mut read, 16384).await.unwrap().is_none());
}

#[tokio::test]
async fn sandbox_death_and_launch_end_close_children() {
    for died in [false, true] {
        let (_dir, _registry, broker, handle) = setup();
        let lifetime = CancellationToken::new();
        let binding = broker
            .bind(
                "ws_a".into(),
                "claude",
                &"example".into(),
                handle.clone(),
                "/repo".into(),
                lifetime.clone(),
            )
            .unwrap();
        let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
        request(&mut read, &mut write, "tools/list").await;
        if died {
            handle.shutdown().await.unwrap();
        } else {
            lifetime.cancel();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn unauthenticated_preset_does_not_block_start() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(McpManager::load(dir.path().into()).unwrap());
    let remote = Arc::new(RemoteMcpManager::new(registry.clone()));
    let broker = Arc::new(McpBroker::new(registry, remote));
    let launch = broker
        .prepare(
            "ws_a".into(),
            AgentAdapterKind::Claude,
            FakeSandbox::new(),
            dir.path().join("run"),
            "/run/bs".into(),
            "/repo".into(),
            &[],
        )
        .await
        .unwrap();
    assert!(launch.servers.is_empty());
}

#[tokio::test]
async fn test_action_only_discovers_tools_in_sandbox() {
    let (_dir, _registry, broker, handle) = setup();
    let result = broker
        .test(
            McpTestParams {
                server_id: "example".into(),
                workspace_id: "ws_a".into(),
                backend: "claude".into(),
            },
            handle.clone(),
            "/repo".into(),
        )
        .await;
    assert_eq!(result.status.state, McpConnectionState::Connected);
    assert_eq!(result.tool_names, vec!["read_example"]);
    assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unrelated_workspace_change_preserves_connection() {
    let (_dir, registry, broker, handle) = setup();
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle,
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
    registry
        .set_override(
            McpOverrideParams {
                server_id: "example".into(),
                workspace_id: "ws_b".into(),
                value: McpWorkspaceOverride::Disabled,
            },
            None,
        )
        .unwrap();
    request(&mut read, &mut write, "tools/list").await;
    assert!(!binding.cancelled());
    drop(read);
    drop(write);
    task.await.unwrap().unwrap();
}

#[test]
fn native_name_collision_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".mcp.json");
    std::fs::write(
        &file,
        r#"{"mcpServers":{"bondsymphonic_mcp_example":{"command":"native"}}}"#,
    )
    .unwrap();
    assert!(bondsymphonic_daemon::mcp::broker::check_native_collision(
        &[file],
        "bondsymphonic_mcp_example"
    )
    .is_err());
}

#[test]
fn escaped_native_keys_and_jsonc_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("opencode.jsonc");
    std::fs::write(
        &file,
        r#"// native configuration
    {"mcp":{"bondsymphonic_mcp_ex\u0061mple" /* comment */ : {"type":"local",},},}"#,
    )
    .unwrap();
    assert!(bondsymphonic_daemon::mcp::broker::check_native_collision(
        std::slice::from_ref(&file),
        "bondsymphonic_mcp_example"
    )
    .is_err());
    std::fs::write(&file, r#"{"mcp":{"native":{"command":"other"}},}"#).unwrap();
    assert!(bondsymphonic_daemon::mcp::broker::check_native_collision(
        &[file],
        "bondsymphonic_mcp_example"
    )
    .is_ok());
}

#[tokio::test]
async fn rpc_requires_auth_and_restores_preset_without_secrets() {
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::backend_for,
        server::{
            broadcast::EventBus,
            dispatch::{ConnCtx, Handler, SystemHandler},
            handlers::WorkspaceHandler,
        },
        workspace::DataDirs,
    };
    let dir = tempfile::tempdir().unwrap();
    let events = EventBus::new(16);
    let daemon = Daemon::new(
        DataDirs::new(dir.path()),
        backend_for("noop"),
        events.clone(),
    )
    .unwrap();
    let ctx = ConnCtx {
        authenticated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        events,
        shutdown: CancellationToken::new(),
        disconnected: CancellationToken::new(),
    };
    let handler = WorkspaceHandler {
        system: SystemHandler {
            token: "test-auth".into(),
            capabilities: Capabilities {
                workspace_ready_snapshot: false,
                sandbox_backend: "noop".into(),
                git_protect: false,
                adapters: vec![],
                backends: vec![],
                mcp: None,
            },
        },
        daemon,
    };
    assert!(handler.handle(Request::McpList {}, &ctx).await.is_err());
    assert!(handler
        .handle(Request::McpRestorePreset {}, &ctx)
        .await
        .is_err());
    ctx.authenticated.store(true, Ordering::SeqCst);
    assert!(handler
        .handle(
            Request::McpTest(McpTestParams {
                server_id: "example".into(),
                workspace_id: "ws_a".into(),
                backend: "claude".into()
            }),
            &ctx
        )
        .await
        .unwrap_err()
        .message
        .contains("Linux sandbox"));
    let listed = handler.handle(Request::McpList {}, &ctx).await.unwrap();
    let id: McpServerId =
        serde_json::from_value(listed["registry"]["servers"][0]["id"].clone()).unwrap();
    handler
        .handle(
            Request::McpRemove(McpServerIdParams {
                server_id: id.clone(),
            }),
            &ctx,
        )
        .await
        .unwrap();
    let restored = handler
        .handle(Request::McpRestorePreset {}, &ctx)
        .await
        .unwrap();
    assert_eq!(restored["servers"][0]["id"], id.0);
    let export = handler.handle(Request::McpExport {}, &ctx).await.unwrap();
    assert!(export.as_str().unwrap().contains("atlassian"));
    assert!(!export.as_str().unwrap().contains("access_token"));
}

#[tokio::test]
async fn partial_frame_survives_other_ready_branches() {
    use bondsymphonic_daemon::mcp::bridge::FrameBuffer;
    use tokio::io::AsyncWriteExt;
    let (mut input, output) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(output);
    let mut frame = FrameBuffer::default();
    input.write_all(b"{\"jsonrpc\":").await.unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(10),
        frame.read(&mut reader, 1024)
    )
    .await
    .is_err());
    input.write_all(b"\"2.0\"}\n").await.unwrap();
    assert_eq!(
        frame.read(&mut reader, 1024).await.unwrap().unwrap(),
        b"{\"jsonrpc\":\"2.0\"}\n"
    );
}

#[tokio::test]
async fn secret_edit_revokes_existing_grant() {
    let (_dir, registry, broker, handle) = setup();
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
    request(&mut read, &mut write, "tools/list").await;
    registry
        .upsert(McpUpsertParams {
            server: registry.server(&"example".into()).unwrap(),
            secrets: vec![McpSecretWrite::Set {
                reference: "local-token".into(),
                value: McpSecretValue("REPLACEMENT".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(binding.cancelled());
    assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
}

struct HttpPeer {
    requests: Mutex<Vec<bondsymphonic_daemon::mcp::transport::HttpRequest>>,
    next: AtomicUsize,
}
#[async_trait]
impl bondsymphonic_daemon::mcp::transport::HttpClient for HttpPeer {
    async fn execute(
        &self,
        request: bondsymphonic_daemon::mcp::transport::HttpRequest,
    ) -> Result<bondsymphonic_daemon::mcp::transport::HttpResponse, RpcError> {
        let m: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        self.requests.lock().unwrap().push(request);
        let notification = m.get("id").is_none();
        let mut headers = vec![];
        let result = if m["method"] == "initialize" {
            headers.push((
                "Mcp-Session-Id".into(),
                format!("session{}", self.next.fetch_add(1, Ordering::SeqCst)),
            ));
            json!({"protocolVersion":"2025-11-25","capabilities":{}})
        } else {
            json!({"tools":[{"name":"read_remote"}]})
        };
        Ok(bondsymphonic_daemon::mcp::transport::HttpResponse {
            status: if notification { 202 } else { 200 },
            headers,
            body: if notification {
                vec![]
            } else {
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":m["id"],"result":result})).unwrap()
            },
        })
    }
}

#[tokio::test]
async fn remote_sessions_are_isolated_and_tokens_stay_host_side() {
    let (_dir, registry, _broker, handle) = setup();
    registry
        .upsert(McpUpsertParams {
            server: McpServerConfig {
                id: "remote".into(),
                name: "Remote".into(),
                transport: McpTransportConfig::Http {
                    url: "https://example.com/mcp".into(),
                    auth: McpAuthConfig::Bearer {
                        token: "remote-token".into(),
                    },
                },
                enabled: true,
                default_workspace_enabled: true,
                enabled_backends: vec!["claude".into()],
            },
            secrets: vec![McpSecretWrite::Set {
                reference: "remote-token".into(),
                value: McpSecretValue("REMOTE_SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    let peer = Arc::new(HttpPeer {
        requests: Mutex::new(vec![]),
        next: AtomicUsize::new(0),
    });
    let remote = Arc::new(RemoteMcpManager::with_client(
        registry.clone(),
        peer.clone(),
    ));
    let broker = Arc::new(McpBroker::with_client(
        registry,
        remote.clone(),
        peer.clone(),
    ));
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"remote".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut ra, mut wa, ta) = connect(&broker, binding.clone(), binding.token()).await;
    let (mut rb, mut wb, tb) = connect(&broker, binding.clone(), binding.token()).await;
    for (r, w) in [(&mut ra, &mut wa), (&mut rb, &mut wb)] {
        write_message(w,&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"sampling":{},"elicitation":{}}}})).await.unwrap();
        read_frame(r, 16384).await.unwrap().unwrap();
        write_message(
            w,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await
        .unwrap();
        assert!(!request(r, w, "tools/list")
            .await
            .to_string()
            .contains("REMOTE_SECRET_SENTINEL"));
    }
    {
        let requests = peer.requests.lock().unwrap();
        for request in requests.iter() {
            assert!(request.headers.contains(&(
                "Authorization".into(),
                "Bearer REMOTE_SECRET_SENTINEL".into()
            )));
            let message: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if message["method"] == "initialize" {
                assert_eq!(message["params"]["capabilities"], json!({}));
            }
        }
        let sessions: Vec<_> = requests
            .iter()
            .filter(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["method"]
                    == "tools/list"
            })
            .map(|r| {
                r.headers
                    .iter()
                    .find(|(n, _)| n == "Mcp-Session-Id")
                    .unwrap()
                    .1
                    .clone()
            })
            .collect();
        assert_ne!(sessions[0], sessions[1]);
    }
    assert!(handle.commands.lock().unwrap().is_empty());
    remote.disconnect(&"remote".into()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), ta)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), tb)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(binding.cancelled());
}

#[tokio::test(start_paused = true)]
async fn local_request_deadline_kills_stalled_server() {
    let (_dir, _registry, broker, handle) = setup();
    let binding = broker
        .bind(
            "ws_a".into(),
            "claude",
            &"example".into(),
            handle.clone(),
            "/repo".into(),
            CancellationToken::new(),
        )
        .unwrap();
    let (mut read, mut write, task) = connect(&broker, binding.clone(), binding.token()).await;
    request(&mut read, &mut write, "tools/list").await;
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":"stalled","method":"stall"}),
    )
    .await
    .unwrap();
    let result = task.await.unwrap();
    assert!(result.unwrap_err().message.contains("deadline"));
    assert_eq!(handle.killed.load(Ordering::SeqCst), 1);
}

struct HoldingSandbox {
    inner: Arc<FakeSandbox>,
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl SandboxHandle for HoldingSandbox {
    async fn spawn(&self, _cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        self.entered.notify_one();
        std::future::pending().await
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        Ok(())
    }
    fn helper_exe(&self) -> PathBuf {
        self.inner.helper_exe()
    }
}
#[tokio::test]
async fn stale_test_cannot_publish_status_for_replacement() {
    let (_dir, registry, broker, handle) = setup();
    let entered = Arc::new(tokio::sync::Notify::new());
    let held = Arc::new(HoldingSandbox {
        inner: handle,
        entered: entered.clone(),
    });
    let test_broker = broker.clone();
    let task = tokio::spawn(async move {
        test_broker
            .test(
                McpTestParams {
                    server_id: "example".into(),
                    workspace_id: "ws_a".into(),
                    backend: "claude".into(),
                },
                held,
                "/repo".into(),
            )
            .await
    });
    entered.notified().await;
    let mut server = registry.server(&"example".into()).unwrap();
    server.name = "Replacement".into();
    registry
        .upsert(McpUpsertParams {
            server,
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    let old = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.status.state, McpConnectionState::Error);
    let new = broker
        .list()
        .statuses
        .into_iter()
        .find(|status| status.server_id.as_str() == "example")
        .unwrap();
    assert_eq!(new.state, McpConnectionState::Disconnected);
    assert!(new.reason.is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_bwrap_socket_bridge_keeps_child_in_sandbox() {
    use bondsymphonic_daemon::sandbox::{backend_for, SandboxSpec};
    if std::env::var_os("BS_DAEMON_EXE").is_none()
        || !std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-all",
                "--die-with-parent",
                "true",
            ])
            .status()
            .is_ok_and(|status| status.success())
    {
        eprintln!("SKIP: real MCP bridge requires BS_DAEMON_EXE and working bwrap user namespaces");
        return;
    }
    let (dir, registry, broker, _fake) = setup();
    let work = dir.path().join("work");
    let run = dir.path().join("run");
    std::fs::create_dir_all(&work).unwrap();
    let marker = work.join("namespace");
    let script = work.join("mcp-server.sh");
    std::fs::write(&script,r#"set -eu
test -n "$API_TOKEN"
readlink /proc/self/ns/pid > "$MCP_TEST_NAMESPACE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"sandbox","version":"1"}}}' ;;
    *'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"sandbox_read"}]}}' ;;
  esac
done
"#).unwrap();
    let mut config = registry.server(&"example".into()).unwrap();
    if let McpTransportConfig::Stdio {
        command, args, env, ..
    } = &mut config.transport
    {
        *command = "/bin/sh".into();
        *args = vec![script.display().to_string()];
        env.insert("MCP_TEST_NAMESPACE".into(), marker.display().to_string());
    }
    registry
        .upsert(McpUpsertParams {
            server: config,
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    let handle = backend_for("linux_bwrap")
        .start(&SandboxSpec {
            id: "ws_a".into(),
            rw_binds: vec![(work.clone(), work.clone())],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: dir.path().join("home"),
            run_dir: run.clone(),
            env: vec![],
            cwd: work.clone(),
        })
        .await
        .unwrap();
    let launch = broker
        .prepare(
            "ws_a".into(),
            AgentAdapterKind::Claude,
            handle.clone(),
            run,
            "/run/bs".into(),
            work,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(launch.servers.len(), 1);
    let declaration = &launch.servers[0];
    assert!(!declaration.to_string().contains("LOCAL_SECRET_SENTINEL"));
    let mut argv = vec![declaration["command"].as_str().unwrap().into()];
    argv.extend(
        declaration["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned()),
    );
    let mut bridge = handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    let mut input = bridge.stdin.take().unwrap();
    let mut output = BufReader::new(bridge.stdout.take().unwrap());
    write_message(&mut input,&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame(&mut output, 16384),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    write_message(
        &mut input,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await
    .unwrap();
    write_message(
        &mut input,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await
    .unwrap();
    let frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame(&mut output, 16384),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(String::from_utf8(frame).unwrap().contains("sandbox_read"));
    assert_ne!(
        std::fs::read_to_string(marker).unwrap().trim(),
        std::fs::read_link("/proc/self/ns/pid")
            .unwrap()
            .to_str()
            .unwrap()
    );
    launch.lease.close();
    tokio::time::timeout(std::time::Duration::from_secs(5), bridge.exit)
        .await
        .unwrap()
        .unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn framing_is_bounded_before_newline() {
    let data = vec![b'x'; 1025];
    assert!(read_frame(&mut BufReader::new(data.as_slice()), 1024)
        .await
        .is_err());
}

#[test]
fn native_config_preserved() {
    let servers = vec![
        json!({"name":"bondsymphonic_mcp_example", "command":"/opt/bs/helper", "args":["mcp-bridge","--socket","/run/bs/one"], "env":[]}),
    ];
    let mut argv = vec!["claude".into(), "--print".into()];
    native_supplement(AgentAdapterKind::Claude, &mut argv, &servers).unwrap();
    assert_eq!(&argv[..2], ["claude", "--print"]);
    assert!(!argv.iter().any(|s| s == "--strict-mcp-config"));
    let added: serde_json::Value = serde_json::from_str(argv.last().unwrap()).unwrap();
    assert_eq!(
        added["mcpServers"]["bondsymphonic_mcp_example"]["command"],
        "/opt/bs/helper"
    );
    let mut codex = vec!["codex".into(), "app-server".into()];
    native_supplement(AgentAdapterKind::Codex, &mut codex, &servers).unwrap();
    assert!(codex
        .iter()
        .any(|s| s.starts_with("mcp_servers.bondsymphonic_mcp_example.command=")));
    assert!(!codex.iter().any(|s| s.starts_with("mcp_servers=")));
}
