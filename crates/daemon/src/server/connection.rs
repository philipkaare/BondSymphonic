use crate::server::broadcast::EventBus;
use crate::server::dispatch::{ConnCtx, Handler};
use bondsymphonic_proto::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// How long in-flight request tasks may keep running after the connection loop stops, so
/// an already-computed reply still reaches the writer before the socket is torn down.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The longest request line an authenticated peer may send. Comfortably above the
/// largest legal request, an `fs.write_file` carrying [`crate::fs::MAX_READ`] bytes of
/// content with room for JSON escaping; a line that reaches this without a newline is
/// not a request, and the connection ends rather than buffering on.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// The longest line accepted before `hello`. A `hello` is a few hundred bytes, and an
/// unauthenticated peer is nobody: it does not get to make the daemon hold megabytes.
pub const MAX_FRAME_BEFORE_HELLO: usize = 64 * 1024;

/// How long a peer may take to send each line before `hello`. A connection that says
/// nothing is holding a slot for nothing.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Notifies once on drop, however the scope it lives in is left.
struct NotifyOnDrop<'a>(&'a Notify);

impl Drop for NotifyOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

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

    let ctx = Arc::new(ConnCtx {
        authenticated: Arc::new(AtomicBool::new(false)),
        events: events.clone(),
        shutdown: shutdown.clone(),
        disconnected: CancellationToken::new(),
    });

    // Reader task: owns the read half and forwards whole lines over an mpsc.
    // `AsyncBufReadExt::read_line` is *not* cancellation safe (the future takes ownership of
    // the `String`'s buffer, so bytes already consumed from the `BufReader` are lost if the
    // future is dropped mid-line), which makes it unusable directly inside the `select!`
    // below. `mpsc::Receiver::recv` is cancel safe, so the read lives in its own task.
    //
    // Before `hello` the reader is strict: a short line cap and a deadline per line, and
    // a peer that breaks either is simply disconnected, with no reply. After `hello` the
    // cap is the one a real request can need and there is no deadline. Whether `hello`
    // has happened is decided by the loop below, which handles it inline, so after each
    // pre-hello line the reader waits for the loop to have processed it (`processed`)
    // before deciding which regime the next line falls under: without that wait a
    // `hello` followed in the same burst by a large legal request would see the request
    // read under the pre-hello cap.
    let (line_tx, mut line_rx) = mpsc::channel::<String>(64);
    let processed = Arc::new(Notify::new());
    let reader = tokio::spawn({
        let authenticated = ctx.authenticated.clone();
        let processed = processed.clone();
        async move {
            let mut reader = BufReader::new(r).take(0);
            let mut line = String::new();
            loop {
                line.clear();
                let pre_hello = !authenticated.load(Ordering::SeqCst);
                reader.set_limit(if pre_hello {
                    MAX_FRAME_BEFORE_HELLO
                } else {
                    MAX_FRAME
                } as u64);
                let read = if pre_hello {
                    match tokio::time::timeout(HELLO_TIMEOUT, reader.read_line(&mut line)).await {
                        Ok(read) => read,
                        Err(_) => {
                            tracing::info!("no hello within {HELLO_TIMEOUT:?}; disconnecting");
                            break;
                        }
                    }
                } else {
                    reader.read_line(&mut line).await
                };
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                // `read_line` stops at the cap the way it stops at end of file: with no
                // newline. The two are told apart by whether the cap was consumed.
                if !line.ends_with('\n') && reader.limit() == 0 {
                    tracing::warn!(pre_hello, "request line exceeds the cap; disconnecting");
                    break;
                }
                if line_tx.send(std::mem::take(&mut line)).await.is_err() {
                    break;
                }
                if pre_hello {
                    processed.notified().await;
                }
            }
        }
    });
    let mut event_rx = events.subscribe();
    // Requests run in their own tasks so a slow one (e.g. `system.check_prereqs`) blocks
    // neither later requests nor event delivery on the same connection.
    let mut inflight: JoinSet<()> = JoinSet::new();
    // Replies come back through this loop rather than going straight to the writer, so
    // the events a request published on its way out are written before its response.
    // See `pending` below.
    let (resp_tx, mut resp_rx) = mpsc::channel::<ServerMessage>(256);
    // A reply waiting to be written, with the number of events that were already queued
    // for this connection when it was parked. See the flush at the top of the loop.
    let mut pending: Option<(ServerMessage, usize)> = None;

    loop {
        // A handler publishes its events (`workspace.state` Ready, say) before it
        // returns, so by the time its reply reaches this loop they are already in this
        // connection's broadcast queue. Flushing that queue first turns "the event
        // arrives before the response" from a scheduling race into a guarantee.
        //
        // Only those events go out first, though: the flush can block on a full writer
        // channel, and an event a background task publishes meanwhile (a `pty.output`
        // from the reader `pty.open` spawned, say) must not overtake the reply that
        // carries the id it refers to.
        if let Some((resp, queued)) = pending.take() {
            let dropped = match forward_events(&mut event_rx, &ctx, &out_tx, queued).await {
                Some(dropped) => dropped,
                None => break,
            };
            // A lag can happen inside this bounded flush too (a burst that overflows
            // `EVENT_BUS_CAPACITY` before the reply was even parked, say), not just in
            // the main loop's `event_rx.recv()` branch below. Report it the same way,
            // once, ahead of the reply it was queued alongside.
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    "client lagged during pre-reply flush; events dropped"
                );
                if ctx.is_authenticated() {
                    let notice = ServerMessage::event(None, Event::events_dropped(dropped));
                    if out_tx.send(codec::encode(&notice)).await.is_err() {
                        break;
                    }
                }
            }
            if out_tx.send(codec::encode(&resp)).await.is_err() {
                break;
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            // Reaps finished request tasks. On an empty set `join_next` yields `None`, the
            // pattern fails to match and the branch is simply disabled for this iteration.
            Some(_) = inflight.join_next() => {}
            Some(resp) = resp_rx.recv() => { pending = Some((resp, event_rx.len())); }
            ev = event_rx.recv() => {
                match ev {
                    Ok(msg) => {
                        if ctx.is_authenticated() && out_tx.send(codec::encode(&msg)).await.is_err() {
                            break;
                        }
                    }
                    // The bus dropped events this connection never read (a slow client, or
                    // a burst that outran `EVENT_BUS_CAPACITY`). Tell the client once, with
                    // the count, so it knows its view of `pty.output`/`daemon.log` has a
                    // gap; reception then resumes at the oldest event still buffered.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "client lagged; events dropped");
                        if ctx.is_authenticated() {
                            let notice = ServerMessage::event(None, Event::events_dropped(n));
                            if out_tx.send(codec::encode(&notice)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            got = line_rx.recv() => {
                let Some(line) = got else { break };
                // Tells the reader this line has been dealt with, whichever way this
                // branch is left, so it can see whether `hello` went through. Sent for
                // every line rather than only the pre-hello ones: a notification nobody
                // is waiting for is kept as a permit and spent by nobody, since the
                // reader stops waiting for good once `hello` is through, while getting
                // the condition wrong in the other direction would hang the reader.
                let _ack = NotifyOnDrop(&processed);
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
                    let tx = resp_tx.clone();
                    inflight.spawn(async move {
                        // The handler runs in a task of its own so that a panic inside
                        // it is a `JoinError` here rather than the silent end of this
                        // task: the caller still gets a reply carrying its id, and the
                        // connection goes on serving. Aborting this task (the drain
                        // below) aborts the handler with it.
                        let handler = AbortOnDropHandle::new(tokio::spawn(async move {
                            respond(&*h, id, request, &c).await
                        }));
                        let resp = match handler.await {
                            Ok(resp) => resp,
                            Err(e) => {
                                tracing::error!(id, "request handler panicked: {e}");
                                ServerMessage::err(
                                    id,
                                    RpcError::internal("request handler panicked"),
                                )
                            }
                        };
                        let _ = tx.send(resp).await;
                    });
                }
            }
        }
    }

    reader.abort();
    // Whatever a handler tied to this connection — a host setup terminal, say — is let go
    // now, before the drain: the peer is gone, or the daemon is.
    ctx.disconnected.cancel();
    // A reply the loop had in hand when it stopped is still owed to the caller.
    if let Some((resp, _)) = pending.take() {
        let _ = out_tx.send(codec::encode(&resp)).await;
    }
    // Let in-flight handlers finish queueing their replies, then stop waiting on the slow
    // ones: either the peer is gone or the daemon is shutting down.
    let _ = tokio::time::timeout(DRAIN_GRACE, async {
        while inflight.join_next().await.is_some() {
            drain_responses(&mut resp_rx, &out_tx).await;
        }
    })
    .await;
    inflight.shutdown().await;
    drain_responses(&mut resp_rx, &out_tx).await;
    drop(out_tx);
    let _ = writer.await;
}

/// Writes up to `max` events already queued for this connection, and returns how many
/// were dropped by a lag encountered along the way (the caller reports that count to the
/// client as a single notice). Returns `None` when the writer is gone and the connection
/// should be torn down.
///
/// The bound is what keeps a reply ahead of events published after it was parked: those
/// are written by the connection loop's own event branch on a later iteration, in order.
async fn forward_events(
    event_rx: &mut tokio::sync::broadcast::Receiver<ServerMessage>,
    ctx: &ConnCtx,
    out_tx: &mpsc::Sender<String>,
    max: usize,
) -> Option<u64> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut dropped: u64 = 0;
    for _ in 0..max {
        match event_rx.try_recv() {
            Ok(msg) => {
                if ctx.is_authenticated() && out_tx.send(codec::encode(&msg)).await.is_err() {
                    return None;
                }
            }
            // `Lagged` is reported once and then reception resumes, so it is skipped
            // rather than ending the flush; the loss is accumulated and reported by the
            // caller instead. It still consumes one of `max`: the dropped events it
            // stands for were among the ones counted.
            Err(TryRecvError::Lagged(n)) => dropped += n,
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return Some(dropped),
        }
    }
    Some(dropped)
}

/// Writes replies that landed after the connection loop stopped.
async fn drain_responses(
    resp_rx: &mut mpsc::Receiver<ServerMessage>,
    out_tx: &mpsc::Sender<String>,
) {
    while let Ok(resp) = resp_rx.try_recv() {
        if out_tx.send(codec::encode(&resp)).await.is_err() {
            return;
        }
    }
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
