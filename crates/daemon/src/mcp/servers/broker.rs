//! The daemon side of a remote server's socket (`mcp-<id>.sock`): carries an
//! in-sandbox client's MCP session to the upstream server.
//!
//! The client is an agent CLI talking stdio through `mcp-bridge`, so the
//! socket speaks the stdio framing: newline-delimited JSON-RPC. Each accepted
//! connection gets its own [`RemoteSession`], so two agents (or two restarts of
//! one) never share an `Mcp-Session-Id`. The session reads the server's
//! configuration and credentials for every request, which is what makes a
//! disabled, removed or edited server stop answering even before its socket
//! is gone; the credentials themselves never cross the socket.
//!
//! Messages are forwarded as they are, request ids included, except that
//! `initialize` advertises no client capabilities: the broker answers the
//! server's own requests itself (`ping`) and refuses the rest, so the server
//! must not expect sampling or elicitation from the agent.

use super::transport::{self, RemoteSession, MAX_BODY};
use super::McpServers;
use bondsymphonic_proto::{McpConnectionState, McpServerId, RpcError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The JSON-RPC error code for "this server has no usable login".
pub const NOT_CONNECTED: i64 = -32001;
/// Requests one connection may have upstream at once; more are refused.
const MAX_IN_FLIGHT: usize = 64;

/// What an agent's tool call says when the server needs a login: the user
/// fixes it in Settings, not in the agent.
pub fn not_connected_message(name: &str) -> String {
    format!("{name} is not connected — connect it in Settings › MCP servers")
}

/// The transport's fixed reasons that mean "log in (again)".
fn is_not_connected(e: &RpcError) -> bool {
    ["not connected", "authentication_required"]
        .iter()
        .any(|reason| e.message == transport::error(reason).message)
}

fn error_reply(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

type Calls = Arc<parking_lot::Mutex<HashMap<String, tokio::task::AbortHandle>>>;

/// Ends the connection's work when `serve` returns or is dropped (the
/// listener stopped): in-flight requests, and with them their curl
/// processes, and the writer.
struct ConnectionGuard {
    calls: Calls,
    writer: tokio::task::AbortHandle,
    session: Arc<RemoteSession>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.session.close();
        for (_, call) in self.calls.lock().drain() {
            call.abort();
        }
        self.writer.abort();
    }
}

/// A request's answer: the upstream's messages, or one error reply carrying
/// the transport's fixed reason (never upstream text).
fn answer(
    servers: &McpServers,
    server: &McpServerId,
    id: &Value,
    result: Result<Vec<Value>, RpcError>,
) -> Vec<Value> {
    match result {
        Ok(messages) => messages,
        Err(e) if is_not_connected(&e) => {
            let name = servers
                .server(server)
                .map_or_else(|| server.as_str().to_owned(), |s| s.name);
            vec![error_reply(
                id,
                NOT_CONNECTED,
                &not_connected_message(&name),
            )]
        }
        Err(e) => vec![error_reply(id, -32603, &e.message)],
    }
}

/// The request id a cancellation refers to, as the key its call is kept
/// under: the JSON text, so `1` and `"1"` stay different.
fn key(id: &Value) -> String {
    id.to_string()
}

/// Serves one connection to `server`'s socket until the client closes it.
pub async fn serve<R, W>(servers: Arc<McpServers>, server: McpServerId, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session = Arc::new(RemoteSession::open(
        servers.clone(),
        server.clone(),
        servers.http(),
    ));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let calls: Calls = Arc::default();
    let guard = ConnectionGuard {
        calls: calls.clone(),
        writer: writer_task.abort_handle(),
        session: session.clone(),
    };
    let send = |messages: Vec<Value>| {
        for m in messages {
            let _ = out_tx.send(m);
        }
    };
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bounded like an upstream body: a longer line closes the connection.
        let n = match (&mut reader)
            .take(MAX_BODY as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 || buf.len() > MAX_BODY {
            break;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            send(vec![error_reply(&Value::Null, -32700, "parse error")]);
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(mut message) = serde_json::from_str::<Value>(text) else {
            send(vec![error_reply(&Value::Null, -32700, "parse error")]);
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method.as_deref(), id) {
            // Initialization runs to completion before anything after it is
            // read, so `notifications/initialized` and the first requests
            // reach the server in the order the client sent them.
            (Some("initialize"), Some(id)) => {
                if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) {
                    params.insert("capabilities".into(), json!({}));
                }
                let generation = servers.generation(&server);
                let result = session.request(message).await;
                if result.is_ok() && servers.status(&server).state != McpConnectionState::Connected
                {
                    servers.record_status(&server, generation, McpConnectionState::Connected, None);
                }
                send(answer(&servers, &server, &id, result));
            }
            (Some(_), Some(id)) => {
                let mut in_flight = calls.lock();
                if in_flight.len() >= MAX_IN_FLIGHT {
                    send(vec![error_reply(
                        &id,
                        -32000,
                        "too many MCP requests in flight",
                    )]);
                    continue;
                }
                let (session, servers, server, out_tx, calls2, k) = (
                    session.clone(),
                    servers.clone(),
                    server.clone(),
                    out_tx.clone(),
                    calls.clone(),
                    key(&id),
                );
                // Registered under the lock the task takes to deregister, so
                // a call that finishes at once leaves no stale entry.
                let task = tokio::spawn(async move {
                    let result = session.request(message).await;
                    calls2.lock().remove(&key(&id));
                    for m in answer(&servers, &server, &id, result) {
                        let _ = out_tx.send(m);
                    }
                });
                // A reused id replaces the older call, which is aborted.
                if let Some(old) = in_flight.insert(k, task.abort_handle()) {
                    old.abort();
                }
            }
            // A notification, or the client's reply to a server request:
            // forwarded in order, and nothing to answer.
            (method, _) => {
                if method == Some("notifications/cancelled") {
                    if let Some(call) = message
                        .pointer("/params/requestId")
                        .and_then(|id| calls.lock().remove(&key(id)))
                    {
                        call.abort();
                    }
                }
                let _ = session.request(message).await;
            }
        }
    }
    // The client is done: what is still upstream will never be read.
    for (_, call) in calls.lock().drain() {
        call.abort();
    }
    drop(out_tx);
    let _ = writer_task.await;
    drop(guard);
}
