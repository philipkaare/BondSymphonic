use bondsymphonic_daemon::server::connection::MAX_LINES_BEFORE_HELLO;
use bondsymphonic_daemon::server::dispatch::{ConnCtx, DelayingHandler, Handler, SystemHandler};
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_proto::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

async fn start() -> (u16, String, CancellationToken, tokio::task::JoinHandle<()>) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let h = tokio::spawn(async move { server.run(c2).await.unwrap() });
    (port, token, cancel, h)
}

async fn connect(
    port: u16,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (r, w) = s.into_split();
    (BufReader::new(r), w)
}

async fn send(w: &mut tokio::net::tcp::OwnedWriteHalf, id: u64, req: Request) {
    w.write_all(codec::encode(&ClientMessage::Request { id, request: req }).as_bytes())
        .await
        .unwrap();
}

async fn recv(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<ServerMessage> {
    let mut line = String::new();
    let n = r.read_line(&mut line).await.unwrap();
    if n == 0 {
        return None;
    }
    Some(codec::decode(line.trim_end()).unwrap())
}

/// [`recv`] for the tests that expect the daemon to hang up on them, where a
/// failed read counts as the hang-up. Closing a socket that still has unread
/// bytes in its receive buffer resets the connection rather than ending it
/// cleanly, so a peer the daemon cut off mid-flood sees an error where a peer
/// it let finish sees end of file. Both are the disconnect.
async fn recv_or_reset(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<ServerMessage> {
    let mut line = String::new();
    match r.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(codec::decode(line.trim_end()).unwrap()),
    }
}

#[tokio::test]
async fn hello_with_good_token_returns_version() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            result: Some(v),
            error: None,
        } => {
            let hr: HelloResult = codec::parse_result(v).unwrap();
            assert_eq!(hr.daemon_version, env!("CARGO_PKG_VERSION"));
            // The answer names the protocol as well as the build, so a client
            // one version ahead can tell the two apart.
            assert_eq!(hr.protocol_version, Some(PROTOCOL_VERSION));
        }
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

#[tokio::test]
async fn bad_token_gets_unauthorized_and_disconnect() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token: "nope".into(),
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response { error: Some(e), .. } => {
            assert_eq!(e.code, ErrorCode::Unauthorized)
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(recv(&mut r).await.is_none(), "connection should be closed");
    cancel.cancel();
}

#[tokio::test]
async fn request_before_hello_is_rejected() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(&mut w, 5, Request::SystemCheckPrereqs {}).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 5,
            error: Some(e),
            ..
        } => assert_eq!(e.code, ErrorCode::Unauthorized),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

#[tokio::test]
async fn unimplemented_method_returns_internal_error() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();
    send(&mut w, 2, Request::WorkspaceList {}).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 2,
            error: Some(e),
            ..
        } => {
            assert_eq!(e.code, ErrorCode::Internal);
            assert!(e.message.contains("workspace.list"));
        }
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

#[tokio::test]
async fn shutdown_request_stops_server() {
    let (port, token, _cancel, h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();
    send(&mut w, 2, Request::SystemShutdown {}).await;
    let _ = recv(&mut r).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), h)
        .await
        .expect("server exits")
        .unwrap();
}

#[tokio::test]
async fn events_are_broadcast_to_authenticated_clients() {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let bus = server.event_bus();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();
    bus.publish(
        None,
        Event::DaemonLog {
            level: LogLevel::Info,
            message: "hi".into(),
            host: None,
        },
    );
    match recv(&mut r).await.unwrap() {
        ServerMessage::Event {
            workspace_id: None,
            event: Event::DaemonLog { message, .. },
        } => assert_eq!(message, "hi"),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

#[tokio::test]
async fn server_survives_aborted_connection_and_accepts_next() {
    let (port, token, cancel, _h) = start().await;

    // First connection: write a malformed, newline-less fragment and then abort it
    // (drop both halves) mid-handshake, before completing `hello`.
    {
        let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (_r, mut w) = s.into_split();
        w.write_all(b"not json, no newline").await.unwrap();
    }

    // The accept loop must still be running: a fresh connection completes `hello` normally.
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            result: Some(_),
            error: None,
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// A slow request must not block a later fast one on the same connection. `workspace.list`
/// is delayed 500ms by `DelayingHandler`; `repo.inspect` is answered immediately by the
/// wrapped `SystemHandler`. With serial in-line dispatch the slow reply arrives first and
/// this test fails; with per-request task spawning the fast reply overtakes it.
#[tokio::test]
async fn a_slow_request_does_not_block_a_fast_one() {
    fn delay_for(req: &Request) -> Option<std::time::Duration> {
        match req {
            Request::WorkspaceList {} => Some(std::time::Duration::from_millis(500)),
            _ => None,
        }
    }

    let cfg = ServerConfig::default();
    let capabilities = cfg.capabilities.clone();
    let server = Server::bind(cfg).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let server = server.with_handler(std::sync::Arc::new(DelayingHandler {
        inner: std::sync::Arc::new(SystemHandler {
            token: token.clone(),
            capabilities,
        }),
        delay_for,
    }));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });

    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();

    send(&mut w, 2, Request::WorkspaceList {}).await;
    send(
        &mut w,
        3,
        Request::RepoInspect(RepoPathParams { path: "/r".into() }),
    )
    .await;

    let first = recv(&mut r).await.unwrap();
    match first {
        ServerMessage::Response { id, .. } => assert_eq!(
            id, 3,
            "the fast request must be answered while the slow one is still running"
        ),
        other => panic!("unexpected {other:?}"),
    }
    let second = recv(&mut r).await.unwrap();
    match second {
        ServerMessage::Response { id, .. } => assert_eq!(id, 2),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// A well-formed JSON line that fails the typed `ClientMessage` decode (here: a method
/// name this daemon does not know, which is what version skew looks like) must still be
/// answered with the caller's own id, otherwise the request can never be resolved.
#[tokio::test]
async fn undecodable_line_is_answered_with_the_recovered_id() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    w.write_all(b"{\"type\":\"request\",\"id\":42,\"method\":\"no.such.method\",\"params\":{}}\n")
        .await
        .unwrap();
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 42,
            error: Some(e),
            ..
        } => assert_eq!(e.code, ErrorCode::InvalidParams),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// A reply must not be overtaken by an event a background task publishes *after* the
/// reply was already queued for writing: `pty.open`'s reader starts producing
/// `pty.output` the moment the pty exists, and those events name a `pty_id` the client
/// only learns from the reply.
///
/// The handler publishes a burst large enough to fill the connection's writer channel
/// (256 messages) and both socket buffers, so the flush that runs ahead of the reply is
/// still blocked when the spawned task publishes `late`; the client deliberately reads
/// nothing until well after that. Without the bound, the flush drains whatever has
/// arrived by then and `late` jumps the reply.
#[tokio::test]
async fn an_event_published_after_a_reply_is_queued_is_written_after_it() {
    const BURST: usize = 600;
    const PAYLOAD: usize = 16 * 1024;

    fn log(message: String) -> Event {
        Event::DaemonLog {
            level: LogLevel::Info,
            message,
            host: None,
        }
    }

    struct BurstThenLate {
        inner: std::sync::Arc<dyn Handler>,
    }

    #[async_trait::async_trait]
    impl Handler for BurstThenLate {
        async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<serde_json::Value, RpcError> {
            match req {
                Request::WorkspaceList {} => {
                    let pad = "x".repeat(PAYLOAD);
                    for i in 0..BURST {
                        ctx.events.publish(None, log(format!("burst-{i}-{pad}")));
                    }
                    let bus = ctx.events.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        bus.publish(None, log("late".into()));
                    });
                    Ok(serde_json::json!({ "workspaces": [] }))
                }
                other => self.inner.handle(other, ctx).await,
            }
        }
    }

    let cfg = ServerConfig::default();
    let capabilities = cfg.capabilities.clone();
    let server = Server::bind(cfg).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let server = server.with_handler(std::sync::Arc::new(BurstThenLate {
        inner: std::sync::Arc::new(SystemHandler {
            token: token.clone(),
            capabilities,
        }),
    }));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });

    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();

    send(&mut w, 2, Request::WorkspaceList {}).await;
    // Read nothing while the burst piles up, so the flush is still running when the
    // spawned task publishes at 50ms.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut before = Vec::new();
    loop {
        match recv(&mut r).await.unwrap() {
            ServerMessage::Event {
                event: Event::DaemonLog { message, .. },
                ..
            } => before.push(message),
            ServerMessage::Response { id: 2, .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        before.len(),
        BURST,
        "only the events queued when the reply was parked may precede it; last was {:?}",
        before
            .last()
            .map(|m| m.chars().take(16).collect::<String>())
    );
    assert!(before.iter().all(|m| m.starts_with("burst-")));
    match recv(&mut r).await.unwrap() {
        ServerMessage::Event {
            event: Event::DaemonLog { message, .. },
            ..
        } => assert_eq!(message, "late"),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

#[tokio::test]
async fn a_lagging_client_is_told_how_many_events_it_missed() {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let (port, token, bus) = (
        server.port(),
        server.token().to_string(),
        server.event_bus(),
    );
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();
    // Publish far more than the bus capacity while the client reads nothing.
    for i in 0..(bondsymphonic_daemon::server::broadcast::EVENT_BUS_CAPACITY * 3) {
        bus.publish(
            None,
            Event::DaemonLog {
                level: LogLevel::Info,
                message: format!("m{i}"),
                host: None,
            },
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Drain: somewhere in the stream there must be exactly one drop notice with N > 0,
    // and the messages after it must be in order. The notice is recognised with the
    // proto helper the IDE uses, not with a copy of its wording.
    let mut dropped: Option<u64> = None;
    let mut last_seen: Option<u64> = None;
    for _ in 0..(bondsymphonic_daemon::server::broadcast::EVENT_BUS_CAPACITY * 3 + 5) {
        let Some(msg) = tokio::time::timeout(std::time::Duration::from_millis(500), recv(&mut r))
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        if let ServerMessage::Event { event, .. } = msg {
            if let Some(n) = event.dropped_event_count() {
                assert!(
                    dropped.replace(n).is_none(),
                    "only one drop notice expected"
                );
            } else if let Event::DaemonLog { message, .. } = event {
                if let Some(i) = message.strip_prefix('m') {
                    let i: u64 = i.parse().unwrap();
                    if let Some(prev) = last_seen {
                        assert!(i > prev, "events after a drop must stay ordered");
                    }
                    last_seen = Some(i);
                }
            }
        }
    }
    assert!(
        dropped.unwrap_or(0) > 0,
        "expected a drop notice, got {dropped:?}"
    );
    cancel.cancel();
}

/// A lag can happen inside the bounded pre-reply flush (`forward_events`), not just in
/// the main loop's `event_rx.recv()` branch: a burst that overflows `EVENT_BUS_CAPACITY`
/// before the reply is even parked lags this connection's receiver before the flush ever
/// runs. That loss must still be reported once, ahead of the reply, exactly like a
/// main-loop lag is.
///
/// `FloodOnList` publishes `EVENT_BUS_CAPACITY + 50` events synchronously (no `.await`
/// between them) from inside the request handler, before returning its reply. On the
/// default current-thread test runtime only one task runs at a time, and a future only
/// yields at an `.await` point; since the publish loop and the reply's journey back to
/// the connection loop (via `resp_tx`) contain no other `.await` that would block, they
/// run to completion in the same scheduling turn, so the connection loop's own
/// `event_rx.recv()` branch has no opportunity to interleave and drain any of the burst
/// first. By the time the reply is parked, the receiver has already lagged by 50 events,
/// and that lag is only discovered later, inside the flush.
#[tokio::test]
async fn a_lag_that_happens_during_the_pre_reply_flush_is_still_reported() {
    fn log(message: String) -> Event {
        Event::DaemonLog {
            level: LogLevel::Info,
            message,
            host: None,
        }
    }

    struct FloodOnList {
        inner: std::sync::Arc<dyn Handler>,
    }

    #[async_trait::async_trait]
    impl Handler for FloodOnList {
        async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<serde_json::Value, RpcError> {
            match req {
                Request::WorkspaceList {} => {
                    for i in 0..(bondsymphonic_daemon::server::broadcast::EVENT_BUS_CAPACITY + 50) {
                        ctx.events.publish(None, log(format!("m{i}")));
                    }
                    Ok(serde_json::json!({ "workspaces": [] }))
                }
                other => self.inner.handle(other, ctx).await,
            }
        }
    }

    let cfg = ServerConfig::default();
    let capabilities = cfg.capabilities.clone();
    let server = Server::bind(cfg).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let server = server.with_handler(std::sync::Arc::new(FloodOnList {
        inner: std::sync::Arc::new(SystemHandler {
            token: token.clone(),
            capabilities,
        }),
    }));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });

    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();

    send(&mut w, 2, Request::WorkspaceList {}).await;
    // Read nothing while the flood piles up, so draining below starts only once the
    // flush has already run to completion.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut dropped: Option<u64> = None;
    let mut got_response = false;
    loop {
        let Some(msg) = tokio::time::timeout(std::time::Duration::from_millis(2000), recv(&mut r))
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        match msg {
            ServerMessage::Event { event, .. } => {
                if let Some(n) = event.dropped_event_count() {
                    assert!(
                        dropped.replace(n).is_none(),
                        "only one drop notice expected"
                    );
                    assert!(
                        !got_response,
                        "the drop notice must precede the reply it was queued ahead of"
                    );
                }
            }
            ServerMessage::Response { id: 2, .. } => {
                got_response = true;
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(got_response, "the parked reply must still arrive");
    assert!(
        dropped.unwrap_or(0) >= 50,
        "expected a drop notice for the flush overflow, got {dropped:?}"
    );

    // The connection must still be healthy after a flush-time lag: a later request gets
    // its own reply, with nothing silently missing in between.
    send(&mut w, 3, Request::SystemCheckPrereqs {}).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response { id: 3, .. } => {}
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// Writes a line the typed `ClientMessage` cannot build. A `hello` from a peer
/// that predates `protocol_version` has no such key at all, and the typed form
/// always writes one.
async fn send_line(w: &mut tokio::net::tcp::OwnedWriteHalf, line: &str) {
    w.write_all(format!("{line}\n").as_bytes()).await.unwrap();
}

/// The Milestone 7 gate: a client speaking another version of the protocol is
/// told so, by name and with both numbers, instead of failing later on a
/// decode error nobody can act on. The connection goes with the reply, the way
/// a bad token's does: nothing this pair could agree on comes next.
#[tokio::test]
async fn hello_with_another_protocol_version_is_refused_and_the_socket_closes() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(99),
        }),
    )
    .await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            error: Some(e),
            ..
        } => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            let data = e.data.clone().expect("the reason travels in data");
            assert_eq!(data["reason"], "protocol_mismatch");
            assert_eq!(data["daemon"], PROTOCOL_VERSION);
            assert_eq!(data["client"], 99);
            assert_eq!(protocol_mismatch_versions(&e), Some((PROTOCOL_VERSION, 99)));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(recv(&mut r).await.is_none(), "connection should be closed");
    cancel.cancel();
}

/// A `hello` with no `protocol_version` at all is a client built before the
/// field existed. It speaks version 1, which is this daemon's version, so it
/// is accepted -- and the answer tells it which version it just reached.
#[tokio::test]
async fn hello_without_a_protocol_version_is_accepted_as_version_one() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    let line = serde_json::json!({
        "type": "request",
        "id": 1,
        "method": "hello",
        "params": { "token": token, "client_version": "0.1.0" }
    })
    .to_string();
    assert!(
        !line.contains("protocol_version"),
        "the pre-M7 hello must not carry the field: {line}"
    );
    send_line(&mut w, &line).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            result: Some(v),
            error: None,
        } => {
            let hr: HelloResult = codec::parse_result(v).unwrap();
            assert_eq!(hr.protocol_version, Some(PROTOCOL_VERSION));
        }
        other => panic!("unexpected {other:?}"),
    }
    // Accepted means authenticated: the next request is answered rather than
    // rejected, and the socket is still up.
    send(&mut w, 2, Request::SystemCheckPrereqs {}).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response { id: 2, error, .. } => assert!(error.is_none(), "{error:?}"),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// A handler that panics must not take its request's reply with it. Each request runs
/// in its own task, and a task that panics simply ends: without a reply carrying the
/// caller's id, the client waits on that request forever, and the panic is invisible.
/// The reply is an `Internal` error, and the connection goes on serving.
#[tokio::test]
async fn a_panicking_handler_is_answered_with_an_internal_error_and_the_connection_survives() {
    struct PanicOnList {
        inner: std::sync::Arc<dyn Handler>,
    }

    #[async_trait::async_trait]
    impl Handler for PanicOnList {
        async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<serde_json::Value, RpcError> {
            match req {
                Request::WorkspaceList {} => panic!("test panic inside a handler"),
                other => self.inner.handle(other, ctx).await,
            }
        }
    }

    let cfg = ServerConfig::default();
    let capabilities = cfg.capabilities.clone();
    let server = Server::bind(cfg).await.unwrap();
    let port = server.port();
    let token = server.token().to_string();
    let server = server.with_handler(std::sync::Arc::new(PanicOnList {
        inner: std::sync::Arc::new(SystemHandler {
            token: token.clone(),
            capabilities,
        }),
    }));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.run(c2).await.unwrap() });

    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(PROTOCOL_VERSION),
        }),
    )
    .await;
    recv(&mut r).await.unwrap();

    send(&mut w, 2, Request::WorkspaceList {}).await;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), recv(&mut r))
        .await
        .expect("the panicking request must still be answered")
        .expect("the connection must stay open");
    match reply {
        ServerMessage::Response {
            id: 2,
            error: Some(e),
            ..
        } => assert_eq!(e.code, ErrorCode::Internal, "{e:?}"),
        other => panic!("unexpected {other:?}"),
    }

    send(&mut w, 3, Request::SystemCheckPrereqs {}).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response { id: 3, error, .. } => assert!(error.is_none(), "{error:?}"),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// Before `hello` a peer is nobody, and nobody gets to make the daemon buffer an
/// unbounded line. A `hello` is a few hundred bytes; two megabytes without a newline is
/// not a slow client, it is a flood, and it ends the connection with no reply.
#[tokio::test]
async fn a_flood_without_a_newline_before_hello_is_disconnected_without_a_reply() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    let flood = vec![b'x'; 2 * 1024 * 1024];
    // The daemon may close the socket before the whole flood is written, in which
    // case the write itself fails: that is the disconnect this test is after.
    let _ = w.write_all(&flood).await;
    let closed =
        tokio::time::timeout(std::time::Duration::from_secs(5), recv_or_reset(&mut r)).await;
    match closed {
        Ok(None) => {}
        Ok(Some(msg)) => panic!("no reply is owed to a flood, got {msg:?}"),
        Err(_) => panic!("the connection was not closed: the daemon is buffering the flood"),
    }
    cancel.cancel();
}

/// A peer that never says `hello` cannot keep the connection by talking.
///
/// The pre-hello line cap and the pre-hello deadline are both per line, so a
/// peer that sends one junk request every nine seconds is inside both for ever:
/// each is answered `Unauthorized`, each resets the clock, and the daemon holds
/// the slot. A budget of lines for the whole handshake is what ends it, and it
/// ends it the way the other two do - by disconnecting, with no reply.
#[tokio::test]
async fn a_peer_that_talks_without_saying_hello_runs_out_of_lines() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    // Every one of these is a well-formed request that is simply not `hello`.
    for id in 1..=MAX_LINES_BEFORE_HELLO as u64 {
        send(&mut w, id, Request::WorkspaceList {}).await;
        match tokio::time::timeout(std::time::Duration::from_secs(5), recv(&mut r))
            .await
            .expect("an unauthorized request is answered")
        {
            Some(ServerMessage::Response { id: got, error, .. }) => {
                assert_eq!(got, id);
                assert_eq!(error.expect("unauthorized").code, ErrorCode::Unauthorized);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    // One line past the budget: no reply, and the connection is gone.
    let over = MAX_LINES_BEFORE_HELLO as u64 + 1;
    send(&mut w, over, Request::WorkspaceList {}).await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), recv_or_reset(&mut r)).await {
        Ok(None) => {}
        Ok(Some(msg)) => panic!("nothing is owed past the budget, got {msg:?}"),
        Err(_) => panic!("the connection outlived its pre-hello line budget"),
    }
    cancel.cancel();
}

/// A peer that connects and then says nothing is holding a connection slot for
/// nothing. Ten seconds of silence before `hello` ends it.
///
/// Time is paused: the runtime jumps straight to the daemon's own timeout the moment
/// every task is idle, so the test takes milliseconds and still exercises the real
/// deadline rather than a shortened one.
#[tokio::test(start_paused = true)]
async fn silence_before_hello_is_disconnected() {
    let (port, _token, cancel, _h) = start().await;
    let (mut r, _w) = connect(port).await;
    let closed = tokio::time::timeout(std::time::Duration::from_secs(60), recv(&mut r)).await;
    match closed {
        Ok(None) => {}
        Ok(Some(msg)) => panic!("no reply is owed to silence, got {msg:?}"),
        Err(_) => {
            panic!("a silent peer was kept for a minute; it should be gone after ten seconds")
        }
    }
    cancel.cancel();
}
