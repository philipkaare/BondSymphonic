use bondsymphonic_ide::client::{ClientError, DaemonClient};
use bondsymphonic_proto::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};

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

/// Like `fake_daemon`, but only handles `hello` and then signals `eof_tx` the moment its
/// read side observes EOF (i.e. the client closed the connection). Used to verify that
/// dropping every `DaemonClient` clone actually tears down the socket rather than merely
/// stopping outgoing writes.
async fn fake_daemon_signaling_eof(
    token: &'static str,
) -> (std::net::SocketAddr, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (eof_tx, eof_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap() == 0 {
                let _ = eof_tx.send(());
                break;
            }
            let ClientMessage::Request { id, request } = codec::decode(line.trim_end()).unwrap();
            if let Request::Hello(p) = request {
                if p.token == token {
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
                    continue;
                }
            }
            let err = ServerMessage::err(id, RpcError::unauthorized());
            w.write_all(codec::encode(&err).as_bytes()).await.unwrap();
        }
    });
    (addr, eof_rx)
}

/// Like `fake_daemon`, but handles requests concurrently: `system.check_prereqs` replies
/// only after a 100ms delay, while every other request replies immediately. This lets a
/// test prove responses are correlated by id rather than by arrival/completion order.
async fn fake_daemon_delayed_prereqs(token: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = stream.into_split();
        let mut r = BufReader::new(r);
        let w = Arc::new(Mutex::new(w));
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let ClientMessage::Request { id, request } = codec::decode(line.trim_end()).unwrap();
            match request {
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
                    w.lock()
                        .await
                        .write_all(codec::encode(&ok).as_bytes())
                        .await
                        .unwrap();
                }
                Request::Hello(_) => {
                    let err = ServerMessage::err(id, RpcError::unauthorized());
                    w.lock()
                        .await
                        .write_all(codec::encode(&err).as_bytes())
                        .await
                        .unwrap();
                }
                Request::SystemCheckPrereqs {} => {
                    let w = w.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let ok = ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "git".into(),
                                    ok: true,
                                    detail: "git version 2.43".into(),
                                    fix_hint: None,
                                }],
                            },
                        );
                        w.lock()
                            .await
                            .write_all(codec::encode(&ok).as_bytes())
                            .await
                            .unwrap();
                    });
                }
                other => {
                    let err = ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    );
                    w.lock()
                        .await
                        .write_all(codec::encode(&err).as_bytes())
                        .await
                        .unwrap();
                }
            }
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
    // The daemon delays the `system.check_prereqs` reply so the `workspace.list` error
    // arrives first, over the same connection. If the client matched responses by
    // arrival/completion order instead of by request id, `a` would receive the error
    // (a deserialization failure, not the expected result) and/or `b` would receive the
    // prereqs payload instead of an error, so a mis-correlation makes this test fail.
    let addr = fake_daemon_delayed_prereqs("secret").await;
    let (client, _h, _e) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let a = client.request::<CheckPrereqsResult>(Request::SystemCheckPrereqs {});
    let b = client.request::<WorkspaceListResult>(Request::WorkspaceList {});
    let (ra, rb) = tokio::join!(a, b);
    let ra = ra.expect("check_prereqs request should succeed");
    assert_eq!(ra.items[0].name, "git");
    assert!(matches!(rb.unwrap_err(), ClientError::Rpc(e) if e.code == ErrorCode::Internal));
}

#[tokio::test]
async fn dropping_all_clients_closes_the_socket() {
    let (addr, eof_rx) = fake_daemon_signaling_eof("secret").await;
    let (client, _hello, _events) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    drop(client);
    let observed_eof = tokio::time::timeout(Duration::from_secs(1), eof_rx).await;
    assert!(
        observed_eof.is_ok(),
        "fake daemon did not observe EOF within 1s of dropping the last DaemonClient clone"
    );
}

/// Answers `hello` and then goes silent: every later request is read and never replied to,
/// and the connection stays open so the client cannot detect the peer by EOF either.
async fn fake_daemon_silent_after_hello(token: &'static str) -> std::net::SocketAddr {
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
            if let Request::Hello(p) = request {
                if p.token == token {
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
                }
            }
            // Anything else: read and drop it. The caller must time out rather than wait
            // forever on a daemon that accepts a request and never answers it.
        }
    });
    addr
}

#[tokio::test]
async fn a_request_that_is_never_answered_times_out() {
    let addr = fake_daemon_silent_after_hello("secret").await;
    let (client, _h, _e) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let client = client.with_request_timeout(Duration::from_millis(200));
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.request::<WorkspaceListResult>(Request::WorkspaceList {}),
    )
    .await
    .expect("request_raw must bound its own wait")
    .expect_err("a request that is never answered must not succeed");
    assert!(
        matches!(err, ClientError::Timeout),
        "expected ClientError::Timeout, got {err:?}"
    );
    // The connection itself is still up: a timeout resolves one request, not the session.
    assert!(client.is_connected());
}
