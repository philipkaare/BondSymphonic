use bondsymphonic_ide::client::{
    default_timeout_for, ClientError, DaemonClient, DEFAULT_REQUEST_TIMEOUT,
};
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

/// How many events the flooding daemon emits before answering. The client's event
/// channel holds 1024, so this is comfortably more than enough to fill it.
const EVENT_FLOOD: usize = 2000;

/// Answers `hello`, then answers `workspace.list` only *after* emitting
/// [`EVENT_FLOOD`] events. This is a reconnect to a daemon with live PTYs: the
/// stream is already busy when the first request goes out. A caller that is not
/// draining the event stream stalls the client's socket reader on a full channel,
/// and its reply is never parsed.
async fn fake_daemon_flooding_events(token: &'static str) -> std::net::SocketAddr {
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
                    w.write_all(codec::encode(&ok).as_bytes()).await.unwrap();
                }
                Request::WorkspaceList {} => {
                    // One buffer, one write: the point of the test is the client's
                    // backpressure, not the daemon's syscall count.
                    let mut flood = String::new();
                    for i in 0..EVENT_FLOOD {
                        flood.push_str(&codec::encode(&ServerMessage::event(
                            Some("ws_1".into()),
                            Event::PtyOutput {
                                pty_id: "pty_1".into(),
                                data_b64: format!("e{i}"),
                            },
                        )));
                    }
                    if w.write_all(flood.as_bytes()).await.is_err() {
                        break;
                    }
                    let ok = ServerMessage::ok(id, &WorkspaceListResult { workspaces: vec![] });
                    if w.write_all(codec::encode(&ok).as_bytes()).await.is_err() {
                        break;
                    }
                }
                other => {
                    let err = ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    );
                    w.write_all(codec::encode(&err).as_bytes()).await.unwrap();
                }
            }
        }
    });
    addr
}

#[tokio::test]
async fn a_request_resolves_while_a_flood_of_events_is_drained() {
    // `AppController::start` spawns its event drain loop before it issues any
    // request, for exactly this reason: a daemon that is already streaming must
    // not be able to starve the connect-time `workspace.list`.
    let addr = fake_daemon_flooding_events("secret").await;
    let (client, _h, mut events) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let client = client.with_request_timeout(Duration::from_secs(10));
    let drain = tokio::spawn(async move {
        let mut n = 0usize;
        while events.recv().await.is_some() {
            n += 1;
        }
        n
    });
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        client.request::<WorkspaceListResult>(Request::WorkspaceList {}),
    )
    .await
    .expect("workspace.list must not hang while the event stream is drained")
    .expect("workspace.list must resolve");
    assert!(res.workspaces.is_empty());
    drop(client);
    let drained = tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .expect("drain task must end once the client is dropped")
        .unwrap();
    assert!(
        drained >= EVENT_FLOOD,
        "expected at least {EVENT_FLOOD} events, drained {drained}"
    );
}

#[tokio::test]
async fn a_flood_of_events_starves_an_undrained_request() {
    // The failure mode the test above guards against, pinned so it stays visible:
    // `_events` is deliberately never polled, the client's socket reader blocks on
    // the full channel, and the reply is never parsed.
    let addr = fake_daemon_flooding_events("secret").await;
    let (client, _h, _events) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let client = client.with_request_timeout(Duration::from_millis(500));
    let err = client
        .request::<WorkspaceListResult>(Request::WorkspaceList {})
        .await
        .expect_err("an undrained event stream must starve the reply");
    assert!(
        matches!(err, ClientError::Timeout),
        "expected ClientError::Timeout, got {err:?}"
    );
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

// ---------------------------------------------------------------------------
// Per-method request timeouts (final review I1).
// ---------------------------------------------------------------------------

/// The daemon spends up to 60 s on each git command and up to 120 s on `gh`, so a
/// merge and a pull request are the two calls the IDE's ordinary 30 s wait is
/// shorter than. A `create_pr` abandoned at 30 s is reported as failed while the
/// branch is on the remote and the pull request is being opened, and the retry it
/// invites answers "a pull request for branch ... already exists".
#[test]
fn merge_and_pr_wait_longer_than_the_daemon_spends_on_them() {
    assert_eq!(
        default_timeout_for("workspace.merge"),
        Duration::from_secs(120)
    );
    assert_eq!(
        default_timeout_for("workspace.create_pr"),
        Duration::from_secs(180)
    );
}

/// Everything else keeps the 30 s default, including the other two calls the
/// Changes toolbar makes.
#[test]
fn every_other_method_keeps_the_default_wait() {
    for method in [
        "workspace.list",
        "workspace.destroy",
        "workspace.changes",
        "agent.history",
        "hello",
        "a.method.this.build.has.never.heard.of",
    ] {
        assert_eq!(
            default_timeout_for(method),
            DEFAULT_REQUEST_TIMEOUT,
            "{method} must keep the default wait"
        );
    }
}

/// The selection is keyed off the request the caller actually built, not off a
/// string a call site had to remember to pass.
#[test]
fn the_wait_is_chosen_from_the_request_itself() {
    let merge = Request::WorkspaceMerge(WorkspaceMergeParams {
        workspace_id: WorkspaceId("ws_1".into()),
        mode: MergeMode::Merge,
        message: None,
    });
    let pr = Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
        workspace_id: WorkspaceId("ws_1".into()),
        title: "t".into(),
        body: String::new(),
        draft: false,
    });
    assert_eq!(
        default_timeout_for(merge.method_name()),
        Duration::from_secs(120)
    );
    assert_eq!(
        default_timeout_for(pr.method_name()),
        Duration::from_secs(180)
    );
    assert_eq!(
        default_timeout_for(Request::WorkspaceList {}.method_name()),
        DEFAULT_REQUEST_TIMEOUT
    );
}

/// A client given an explicit wait uses it for every method, long-running ones
/// included. Without that the tests below -- and the flood test above, which
/// asks for 500 ms -- would sit for two minutes on a merge that is never
/// answered, and there would be no way to bound one at all.
#[tokio::test]
async fn an_explicit_wait_overrides_the_per_method_one() {
    let addr = fake_daemon_silent_after_hello("secret").await;
    let (client, _h, _e) = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .unwrap();
    let client = client.with_request_timeout(Duration::from_millis(200));
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.request::<MergeResult>(Request::WorkspaceMerge(WorkspaceMergeParams {
            workspace_id: WorkspaceId("ws_1".into()),
            mode: MergeMode::Merge,
            message: None,
        })),
    )
    .await
    .expect("an explicit 200 ms wait must bound a merge too")
    .expect_err("the fake daemon never answers");
    assert!(
        matches!(err, ClientError::Timeout),
        "expected ClientError::Timeout, got {err:?}"
    );
}
