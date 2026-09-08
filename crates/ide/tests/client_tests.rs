use bondsymphonic_ide::client::{ClientError, DaemonClient};
use bondsymphonic_proto::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Minimal fake daemon: accepts one connection, answers hello (checks token), echoes
/// `system.check_prereqs` with one item, answers `workspace.list` with an error, and
/// emits one event after hello.
async fn fake_daemon(token: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let ClientMessage::Request { id, request } = codec::decode(line.trim_end()).unwrap();
            let msg = match request {
                Request::Hello(p) if p.token == token => {
                    let ok = ServerMessage::ok(
                        id,
                        &HelloResult {
                            daemon_version: "9.9.9".into(),
                            capabilities: Capabilities {
                                sandbox_backend: "noop".into(),
                                git_protect: false,
                                adapters: vec![],
                            },
                        },
                    );
                    w.write_all(codec::encode(&ok).as_bytes()).await.unwrap();
                    ServerMessage::event(
                        None,
                        Event::DaemonLog {
                            level: LogLevel::Info,
                            message: "welcome".into(),
                            host: None,
                        },
                    )
                }
                Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                Request::SystemCheckPrereqs {} => ServerMessage::ok(
                    id,
                    &CheckPrereqsResult {
                        items: vec![PrereqStatus {
                            name: "git".into(),
                            ok: true,
                            detail: "git version 2.43".into(),
                            fix_hint: None,
                        }],
                    },
                ),
                other => ServerMessage::err(
                    id,
                    RpcError::internal(format!("not implemented: {}", other.method_name())),
                ),
            };
            w.write_all(codec::encode(&msg).as_bytes()).await.unwrap();
        }
    });
    addr
}

#[tokio::test]
async fn connects_and_receives_hello_and_events() {
    let addr = fake_daemon("secret").await;
    let (client, hello, mut events) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    assert_eq!(hello.daemon_version, "9.9.9");
    assert!(client.is_connected());
    let (ws, ev) = events.recv().await.unwrap();
    assert!(ws.is_none());
    assert!(matches!(ev, Event::DaemonLog { message, .. } if message == "welcome"));
}

#[tokio::test]
async fn typed_request_and_rpc_error() {
    let addr = fake_daemon("secret").await;
    let (client, _hello, _events) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let res: CheckPrereqsResult = client
        .request(Request::SystemCheckPrereqs {})
        .await
        .unwrap();
    assert_eq!(res.items[0].name, "git");
    let err = client
        .request::<WorkspaceListResult>(Request::WorkspaceList {})
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Rpc(e) if e.code == ErrorCode::Internal));
}

#[tokio::test]
async fn bad_token_fails_connect() {
    let addr = fake_daemon("secret").await;
    let err = DaemonClient::connect(addr, "wrong", "0.1.0")
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, ClientError::Rpc(e) if e.code == ErrorCode::Unauthorized));
}

#[tokio::test]
async fn concurrent_requests_are_correlated_by_id() {
    let addr = fake_daemon("secret").await;
    let (client, _h, _e) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let a = client.request::<CheckPrereqsResult>(Request::SystemCheckPrereqs {});
    let b = client.request::<WorkspaceListResult>(Request::WorkspaceList {});
    let (ra, rb) = tokio::join!(a, b);
    assert!(ra.is_ok());
    assert!(rb.is_err());
}
