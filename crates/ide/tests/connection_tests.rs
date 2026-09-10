//! What the IDE does when the daemon connection ends, and how it recognises the
//! daemon's "events dropped" notice.
//!
//! Both are cross-crate contracts that no single-crate test covers: the notice
//! is built by the daemon and read here, and the connection handle every
//! QObject reaches for is process-wide state that outlives the connection that
//! filled it.

use bondsymphonic_ide::client::router::EventRouter;
use bondsymphonic_ide::client::DaemonClient;
use bondsymphonic_ide::qobjects::app_controller::{
    on_connection_lost, publish_shared, require_connection, shared, Shared, CONNECTION_LOST,
    NOT_CONNECTED,
};
use bondsymphonic_proto::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// A daemon that answers `hello`, optionally sends `extra` straight afterwards,
/// and then closes the connection when told to.
async fn fake_daemon(
    extra: Option<ServerMessage>,
) -> (std::net::SocketAddr, tokio::sync::mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (hangup_tx, mut hangup_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            tokio::select! {
                _ = hangup_rx.recv() => break,
                read = r.read_line(&mut line) => {
                    if read.unwrap_or(0) == 0 {
                        break;
                    }
                }
            }
            let ClientMessage::Request { id, request } = codec::decode(line.trim_end()).unwrap();
            let reply = match request {
                Request::Hello(_) => ServerMessage::ok(
                    id,
                    &HelloResult {
                        daemon_version: "9.9.9".into(),
                        capabilities: Capabilities {
                            sandbox_backend: "noop".into(),
                            git_protect: false,
                            adapters: vec![],
                        },
                        protocol_version: Some(PROTOCOL_VERSION),
                    },
                ),
                other => ServerMessage::err(
                    id,
                    RpcError::internal(format!("not implemented: {}", other.method_name())),
                ),
            };
            let mut batch = codec::encode(&reply);
            if let Some(extra) = extra.as_ref() {
                batch.push_str(&codec::encode(extra));
            }
            if w.write_all(batch.as_bytes()).await.is_err() {
                break;
            }
        }
        // Dropping `w` (and the listener's half of the socket) is the hangup the
        // IDE sees as the end of its event stream.
    });
    (addr, hangup_tx)
}

async fn connect(
    addr: std::net::SocketAddr,
) -> (DaemonClient, bondsymphonic_ide::client::EventStream) {
    let (client, _hello, events) = DaemonClient::connect(addr, "t", "0.0.0").await.unwrap();
    (client, events)
}

/// Losing the daemon must leave the IDE saying so, and must leave every
/// invokable failing at once. `AppController` drains the event stream for the
/// life of the connection and calls `on_connection_lost` when it ends; this is
/// that call and what the invokables see on either side of it.
#[tokio::test]
async fn an_operation_after_the_connection_is_lost_fails_immediately() {
    // Before anything has connected the invokables say the connection has not
    // been made, which is a different sentence from the one below.
    assert_eq!(require_connection().err(), Some(NOT_CONNECTED));

    let (addr, hangup) = fake_daemon(None).await;
    let (client, mut events) = connect(addr).await;
    publish_shared(Shared {
        client: client.clone(),
        router: EventRouter::new(),
    });
    assert!(
        shared().is_some(),
        "the connection is published for QObjects"
    );
    assert!(require_connection().is_ok());

    // The daemon goes away. The controller's drain loop ends with the event
    // stream, exactly as it does here.
    hangup.send(()).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the event stream ends when the connection does");
    assert!(ended.is_none());
    on_connection_lost();

    // Every invokable starts with `require_connection`, so this is what
    // `create_workspace`, `destroy_workspace`, `fs.list_dir` and the terminal's
    // `open` now do: report the failure and return, with no request issued and
    // nothing waiting on the request timeout.
    let started = Instant::now();
    assert_eq!(require_connection().err(), Some(CONNECTION_LOST));
    assert!(
        shared().is_none(),
        "the dead client is not handed out again"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the failure has to be immediate, not a timeout"
    );

    // The client itself is the second line of defence: a request on it fails as
    // soon as the socket is gone rather than waiting out `DEFAULT_REQUEST_TIMEOUT`.
    let started = Instant::now();
    let result = client.request_raw(Request::WorkspaceList {}).await;
    assert!(result.is_err(), "a dead client cannot answer");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a request on a dead client must not wait for the request timeout"
    );
}

/// The drop notice is written by the daemon and read here. Both ends go through
/// the proto helpers, so this is the daemon's own bytes arriving on the IDE's
/// own event stream and being classified by the call `AppController`'s drain
/// loop makes.
#[tokio::test]
async fn the_daemons_drop_notice_is_recognised_off_the_wire() {
    let notice = ServerMessage::event(None, Event::events_dropped(9));
    let (addr, _hangup) = fake_daemon(Some(notice)).await;
    let (_client, mut events) = connect(addr).await;

    let (_, event) = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the notice arrives")
        .expect("the stream is still open");
    // `drain_events` turns exactly this into the `output_dropped` signal that
    // writes `[output dropped]` into every open terminal.
    assert_eq!(event.dropped_event_count(), Some(9));
}
