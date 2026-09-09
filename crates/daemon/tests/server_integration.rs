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
