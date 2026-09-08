use crate::server::broadcast::EventBus;
use crate::server::dispatch::{ConnCtx, Handler};
use bondsymphonic_proto::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// How long in-flight request tasks may keep running after the connection loop stops, so
/// an already-computed reply still reaches the writer before the socket is torn down.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

pub async fn serve_connection(
    stream: TcpStream,
    handler: Arc<dyn Handler>,
    events: EventBus,
    shutdown: CancellationToken,
) {
    let (r, mut w) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);

    // Writer task: serialises all output (responses + events) onto the socket.
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader task: owns the read half and forwards whole lines over an mpsc.
    // `AsyncBufReadExt::read_line` is *not* cancellation safe (the future takes ownership of
    // the `String`'s buffer, so bytes already consumed from the `BufReader` are lost if the
    // future is dropped mid-line), which makes it unusable directly inside the `select!`
    // below. `mpsc::Receiver::recv` is cancel safe, so the read lives in its own task.
    let (line_tx, mut line_rx) = mpsc::channel::<String>(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line_tx.send(std::mem::take(&mut line)).await.is_err() {
                break;
            }
        }
    });

    let ctx = Arc::new(ConnCtx {
        authenticated: Arc::new(AtomicBool::new(false)),
        events: events.clone(),
        shutdown: shutdown.clone(),
    });
    let mut event_rx = events.subscribe();
    // Requests run in their own tasks so a slow one (e.g. `system.check_prereqs`) blocks
    // neither later requests nor event delivery on the same connection.
    let mut inflight: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            // Reaps finished request tasks. On an empty set `join_next` yields `None`, the
            // pattern fails to match and the branch is simply disabled for this iteration.
            Some(_) = inflight.join_next() => {}
            ev = event_rx.recv() => {
                if let Ok(msg) = ev {
                    if ctx.is_authenticated() && out_tx.send(codec::encode(&msg)).await.is_err() {
                        break;
                    }
                }
                // Lagged or closed: keep serving requests.
            }
            got = line_rx.recv() => {
                let Some(line) = got else { break };
                let trimmed = line.trim_end();
                if trimmed.is_empty() { continue; }
                // Recover the caller's id before the typed decode. An unknown method name is
                // an unknown enum variant, which is what version skew looks like: answering
                // with id 0 would leave that request unresolvable on the client forever.
                let recovered_id = serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
                    .unwrap_or(0);
                let (id, request) = match codec::decode::<ClientMessage>(trimmed) {
                    Ok(ClientMessage::Request { id, request }) => (id, request),
                    Err(e) => {
                        let err = ServerMessage::err(recovered_id, RpcError::invalid_params(e.to_string()));
                        if out_tx.send(codec::encode(&err)).await.is_err() { break; }
                        continue;
                    }
                };
                if matches!(request, Request::Hello(_)) {
                    // `hello` is handled inline, not spawned: the decision to close the
                    // connection after a bad token must be deterministic and must happen
                    // after the reply has been queued.
                    let resp = respond(&*handler, id, request, &ctx).await;
                    let _ = out_tx.send(codec::encode(&resp)).await;
                    // A bad `hello` closes the connection after the reply. A non-hello
                    // request before hello only gets `Unauthorized`, and the connection
                    // stays open.
                    if !ctx.is_authenticated() { break; }
                } else {
                    let h = handler.clone();
                    let c = ctx.clone();
                    let tx = out_tx.clone();
                    inflight.spawn(async move {
                        let resp = respond(&*h, id, request, &c).await;
                        let _ = tx.send(codec::encode(&resp)).await;
                    });
                }
            }
        }
    }

    drop(out_tx);
    reader.abort();
    // Let in-flight handlers finish queueing their replies, then stop waiting on the slow
    // ones: either the peer is gone or the daemon is shutting down.
    let _ = tokio::time::timeout(DRAIN_GRACE, async {
        while inflight.join_next().await.is_some() {}
    })
    .await;
    inflight.shutdown().await;
    let _ = writer.await;
}

async fn respond(handler: &dyn Handler, id: u64, request: Request, ctx: &ConnCtx) -> ServerMessage {
    match handler.handle(request, ctx).await {
        Ok(v) => ServerMessage::Response {
            id,
            result: Some(v),
            error: None,
        },
        Err(e) => ServerMessage::err(id, e),
    }
}
