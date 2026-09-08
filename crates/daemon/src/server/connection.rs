use crate::server::broadcast::EventBus;
use crate::server::dispatch::{ConnCtx, Handler};
use bondsymphonic_proto::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn serve_connection(
    stream: TcpStream,
    handler: Arc<dyn Handler>,
    events: EventBus,
    shutdown: CancellationToken,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);

    // Writer task: serialises all output (responses + events) onto the socket.
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut ctx = ConnCtx {
        authenticated: false,
        events: events.clone(),
        shutdown: shutdown.clone(),
    };
    let mut event_rx = events.subscribe();
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            ev = event_rx.recv() => {
                if let Ok(msg) = ev {
                    if ctx.authenticated { let _ = out_tx.send(codec::encode(&msg)).await; }
                }
                // Lagged or closed: keep serving requests.
            }
            n = reader.read_line(&mut line) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() { continue; }
                let (id, request) = match codec::decode::<ClientMessage>(trimmed) {
                    Ok(ClientMessage::Request { id, request }) => (id, request),
                    Err(e) => {
                        let _ = out_tx.send(codec::encode(&ServerMessage::err(0, RpcError::invalid_params(e.to_string())))).await;
                        continue;
                    }
                };
                let is_hello = matches!(request, Request::Hello(_));
                let resp = match handler.handle(request, &mut ctx).await {
                    Ok(v) => ServerMessage::Response { id, result: Some(v), error: None },
                    Err(e) => ServerMessage::err(id, e),
                };
                let _ = out_tx.send(codec::encode(&resp)).await;
                // A bad `hello` closes the connection after the reply. A non-hello request
                // before hello only gets `Unauthorized` and the connection stays open.
                if is_hello && !ctx.authenticated { break; }
            }
        }
    }
    drop(out_tx);
    let _ = writer.await;
}
