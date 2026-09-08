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
