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
//!
//! The broker is the HTTP client the MCP spec makes responsible for an
//! expired session (a 404 to a request carrying `Mcp-Session-Id`), so it
//! keeps the client's handshake and replays it upstream: the CLI sent
//! `initialize` once over stdio and never will again. A request that failed
//! that way is retried once on the new session, except `tools/call`, which
//! may have had effects and is answered with an error asking for a retry.

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
/// The JSON-RPC error code for "the server dropped the session under this
/// call; it was not replayed".
pub const SESSION_RESET: i64 = -32002;
/// Requests one connection may have upstream at once; more are refused.
const MAX_IN_FLIGHT: usize = 64;

/// What an agent's tool call says when the server needs a login: the user
/// fixes it in Settings, not in the agent.
pub fn not_connected_message(name: &str) -> String {
    format!("{name} is not connected — connect it in Settings › MCP servers")
}

/// What a `tools/call` says when the server expired the session under it.
pub fn session_reset_message(name: &str) -> String {
    format!(
        "{name} reset its session, so this tool call may not have run; \
         the session has been renewed — retry the call"
    )
}

/// Whether `e` is one of the transport's fixed `reason`s.
fn is(e: &RpcError, reason: &str) -> bool {
    e.message == transport::error(reason).message
}

/// The transport's fixed reasons that mean "log in (again)".
fn is_not_connected(e: &RpcError) -> bool {
    is(e, "not connected") || is(e, "authentication_required")
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

/// The client's side of the MCP handshake, kept to replay it upstream.
#[derive(Default)]
struct Handshake {
    /// The client's `initialize`, its capabilities already cleared.
    initialize: Option<Value>,
    /// Whether the client has sent `notifications/initialized`.
    initialized: bool,
}

/// One connection's broker: its upstream session and what renewing it needs.
struct Broker {
    servers: Arc<McpServers>,
    server: McpServerId,
    session: Arc<RemoteSession>,
    /// Held while the handshake runs upstream, so concurrent requests that
    /// all found the session expired renew it once.
    handshake: tokio::sync::Mutex<Handshake>,
}

impl Broker {
    fn name(&self) -> String {
        self.servers
            .server(&self.server)
            .map_or_else(|| self.server.as_str().to_owned(), |s| s.name)
    }

    /// A request's answer: the upstream's messages, or one error reply
    /// carrying the transport's fixed reason (never upstream text).
    fn answer(&self, id: &Value, result: Result<Vec<Value>, RpcError>) -> Vec<Value> {
        match result {
            Ok(messages) => messages,
            Err(e) if is_not_connected(&e) => {
                vec![error_reply(
                    id,
                    NOT_CONNECTED,
                    &not_connected_message(&self.name()),
                )]
            }
            Err(e) => vec![error_reply(id, -32603, &e.message)],
        }
    }

    /// A successful upstream `initialize` is what proves a login works.
    fn mark_connected(&self, generation: u64) {
        if self.servers.status(&self.server).state != McpConnectionState::Connected {
            self.servers.record_status(
                &self.server,
                generation,
                McpConnectionState::Connected,
                None,
            );
        }
    }

    /// The client's `initialize`: forwarded, and kept for renewals.
    async fn initialize(&self, message: Value) -> Result<Vec<Value>, RpcError> {
        let mut handshake = self.handshake.lock().await;
        handshake.initialize = Some(message.clone());
        let generation = self.servers.generation(&self.server);
        let result = self.session.request(message).await;
        if result.is_ok() {
            self.mark_connected(generation);
        }
        result
    }

    /// Initializes the upstream session again from the client's handshake,
    /// unless it already is (another request renewed it first) or the
    /// client never initialized.
    async fn renew(&self) -> Result<(), RpcError> {
        let handshake = self.handshake.lock().await;
        let Some(initialize) = handshake.initialize.clone() else {
            return Ok(());
        };
        if self.session.is_initialized() {
            return Ok(());
        }
        let generation = self.servers.generation(&self.server);
        self.session.request(initialize).await?;
        self.mark_connected(generation);
        if handshake.initialized {
            self.session
                .request(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await?;
        }
        Ok(())
    }

    /// One client request, renewing an expired session on the way.
    async fn call(&self, message: Value) -> Vec<Value> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let tool_call = message.get("method").and_then(Value::as_str) == Some("tools/call");
        let result = match self.session.request(message.clone()).await {
            // The server dropped the session: this request reached it, but
            // was refused. Anything but a tool call is safe to send again.
            Err(e) if is(&e, "session_expired") => match self.renew().await {
                Ok(()) if tool_call => {
                    return vec![error_reply(
                        &id,
                        SESSION_RESET,
                        &session_reset_message(&self.name()),
                    )]
                }
                Ok(()) => self.session.request(message).await,
                Err(e) => Err(e),
            },
            // Another request's 404 left the session uninitialized before
            // this one was sent: nothing went upstream, so it may be sent.
            Err(e) if is(&e, "session not initialized") => match self.renew().await {
                Ok(()) => self.session.request(message).await,
                Err(e) => Err(e),
            },
            result => result,
        };
        self.answer(&id, result)
    }

    /// A notification, or the client's reply to a server request.
    async fn notify(&self, message: Value) {
        if message.get("method").and_then(Value::as_str) == Some("notifications/initialized") {
            self.handshake.lock().await.initialized = true;
        }
        let _ = self.session.request(message).await;
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
    let broker = Arc::new(Broker {
        servers,
        server,
        session,
        handshake: Default::default(),
    });
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
                let result = broker.initialize(message).await;
                send(broker.answer(&id, result));
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
                let (broker, out_tx, calls2, k) =
                    (broker.clone(), out_tx.clone(), calls.clone(), key(&id));
                // Registered under the lock the task takes to deregister, so
                // a call that finishes at once leaves no stale entry.
                let task = tokio::spawn(async move {
                    let answer = broker.call(message).await;
                    calls2.lock().remove(&key(&id));
                    for m in answer {
                        let _ = out_tx.send(m);
                    }
                });
                // A reused id replaces the older call, which is aborted.
                if let Some(old) = in_flight.insert(k, task.abort_handle()) {
                    old.abort();
                }
            }
            // A cancellation ends the call here at once; telling the server
            // must not hold up the messages behind it.
            (Some("notifications/cancelled"), None) => {
                if let Some(call) = message
                    .pointer("/params/requestId")
                    .and_then(|id| calls.lock().remove(&key(id)))
                {
                    call.abort();
                }
                let broker = broker.clone();
                tokio::spawn(async move { broker.notify(message).await });
            }
            // Any other notification, or the client's reply to a server
            // request: forwarded in order, and nothing to answer.
            _ => broker.notify(message).await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::servers::transport::{HttpClient, HttpRequest, HttpResponse};
    use async_trait::async_trait;
    use bondsymphonic_proto::{
        McpAuthConfig, McpServerConfig, McpTransportConfig, McpUpsertParams,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{DuplexStream, Lines};
    use tokio_util::sync::CancellationToken;

    /// A Streamable HTTP server: every `initialize` opens a session `s-<n>`,
    /// `tools/list` offers `echo`, `tools/call` echoes, and `slow` (and the
    /// `notifications/cancelled` POST) never answer. A session in `expired`
    /// gets 404.
    #[derive(Default)]
    struct Upstream {
        /// Each request: its method (`reply` for a response), its session id
        /// and its body.
        seen: parking_lot::Mutex<Vec<(String, Option<String>, Value)>>,
        sessions: AtomicUsize,
        expired: parking_lot::Mutex<Vec<String>>,
        /// `slow` requests whose future was dropped (aborted).
        dropped: Arc<AtomicUsize>,
    }

    struct DropCount(Arc<AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Upstream {
        fn methods(&self) -> Vec<String> {
            self.seen.lock().iter().map(|(m, _, _)| m.clone()).collect()
        }
        /// The server forgets the newest session, once the client's
        /// handshake has reached it.
        async fn expire(&self) {
            until("the handshake upstream", || {
                self.methods()
                    .iter()
                    .any(|m| m == "notifications/initialized")
            })
            .await;
            let n = self.sessions.load(Ordering::SeqCst);
            self.expired.lock().push(format!("s-{n}"));
        }
    }

    fn reply(status: u16, headers: Vec<(String, String)>, body: Option<Value>) -> HttpResponse {
        HttpResponse {
            status,
            headers,
            body: body.map_or_else(Vec::new, |b| b.to_string().into_bytes()),
        }
    }

    #[async_trait]
    impl HttpClient for Upstream {
        async fn send(
            &self,
            request: HttpRequest,
            _: CancellationToken,
        ) -> Result<HttpResponse, RpcError> {
            let message: Value = serde_json::from_slice(&request.body).unwrap();
            let method = message["method"].as_str().unwrap_or("reply").to_owned();
            let session = request
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("mcp-session-id"))
                .map(|(_, v)| v.clone());
            self.seen
                .lock()
                .push((method.clone(), session.clone(), message.clone()));
            if session.is_some_and(|s| self.expired.lock().contains(&s)) {
                return Ok(reply(404, vec![], None));
            }
            let json = vec![("Content-Type".into(), "application/json".into())];
            let id = &message["id"];
            let result = |result: Value| Some(json!({"jsonrpc":"2.0","id":id,"result":result}));
            Ok(match method.as_str() {
                "initialize" => {
                    let n = self.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut headers = json.clone();
                    headers.push(("Mcp-Session-Id".into(), format!("s-{n}")));
                    reply(
                        200,
                        headers,
                        result(json!({"protocolVersion":"2025-06-18",
                            "capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}})),
                    )
                }
                "tools/list" => reply(
                    200,
                    json,
                    result(json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})),
                ),
                "tools/call" => reply(
                    200,
                    json,
                    result(json!({"content":[{"type":"text",
                        "text":message["params"]["arguments"]["text"]}]})),
                ),
                "slow" | "notifications/cancelled" => {
                    let _count = DropCount(self.dropped.clone());
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                _ => reply(202, vec![], None),
            })
        }
    }

    fn remote(id: &str, auth: McpAuthConfig) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            name: format!("Remote {id}"),
            transport: McpTransportConfig::Http {
                url: "https://example.com/mcp".into(),
                auth,
            },
            enabled: true,
            default_workspace_enabled: true,
            enabled_backends: vec!["claude".into()],
        }
    }

    struct Client {
        lines: Lines<BufReader<DuplexStream>>,
        w: DuplexStream,
        served: tokio::task::JoinHandle<()>,
    }

    impl Client {
        async fn send(&mut self, msg: Value) {
            self.w
                .write_all(format!("{msg}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("a message from the broker")
                .unwrap()
                .expect("a line");
            serde_json::from_str(&line).unwrap()
        }
        async fn rpc(&mut self, msg: Value) -> Value {
            self.send(msg).await;
            self.next().await
        }
        async fn initialize(&mut self) -> Value {
            let init = self
                .rpc(
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-06-18","capabilities":{"sampling":{}},
                    "clientInfo":{"name":"cli","version":"7"}}}),
                )
                .await;
            self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await;
            init
        }
    }

    /// A registry holding `config`, served on one fresh connection.
    fn connect(
        config: McpServerConfig,
    ) -> (tempfile::TempDir, Arc<McpServers>, Arc<Upstream>, Client) {
        let dir = tempfile::tempdir().unwrap();
        let upstream = Arc::new(Upstream::default());
        let servers = Arc::new(
            McpServers::load(dir.path())
                .unwrap()
                .with_http(upstream.clone()),
        );
        let id = config.id.clone();
        servers
            .upsert(McpUpsertParams {
                server: config,
                secrets: vec![],
                expected_revision: None,
            })
            .unwrap();
        let (client_r, broker_w) = tokio::io::duplex(1 << 16);
        let (broker_r, client_w) = tokio::io::duplex(1 << 16);
        let served = tokio::spawn(serve(servers.clone(), id, broker_r, broker_w));
        let client = Client {
            lines: BufReader::new(client_r).lines(),
            w: client_w,
            served,
        };
        (dir, servers, upstream, client)
    }

    fn open() -> (tempfile::TempDir, Arc<McpServers>, Arc<Upstream>, Client) {
        connect(remote("remote", McpAuthConfig::None))
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn expired_session_is_renewed_and_a_list_is_retried() {
        let (_d, _s, upstream, mut c) = open();
        assert_eq!(c.initialize().await["result"]["serverInfo"]["name"], "fake");
        upstream.expire().await;
        let list = c
            .rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await;
        assert_eq!(list["id"], 2);
        assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
        let seen = upstream.seen.lock().clone();
        let sessions: Vec<(&str, Option<&str>)> = seen
            .iter()
            .map(|(m, s, _)| (m.as_str(), s.as_deref()))
            .collect();
        assert_eq!(
            sessions,
            [
                ("initialize", None),
                ("notifications/initialized", Some("s-1")),
                ("tools/list", Some("s-1")),
                ("initialize", None),
                ("notifications/initialized", Some("s-2")),
                ("tools/list", Some("s-2")),
            ]
        );
        // The renewal replays the client's own initialize, capabilities
        // still cleared.
        assert_eq!(seen[3].2, seen[0].2);
        assert_eq!(seen[3].2["params"]["clientInfo"]["name"], "cli");
        assert_eq!(seen[3].2["params"]["capabilities"], json!({}));
    }

    #[tokio::test]
    async fn expired_session_does_not_replay_a_tool_call() {
        let (_d, _s, upstream, mut c) = open();
        c.initialize().await;
        upstream.expire().await;
        let call = json!({"jsonrpc":"2.0","id":"c1","method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"hi"}}});
        let failed = c.rpc(call.clone()).await;
        assert_eq!(failed["id"], "c1");
        assert_eq!(failed["error"]["code"], SESSION_RESET, "{failed}");
        assert_eq!(
            failed["error"]["message"],
            session_reset_message("Remote remote")
        );
        let calls = |u: &Upstream| u.methods().iter().filter(|m| *m == "tools/call").count();
        assert_eq!(calls(&upstream), 1, "the failed call is not replayed");
        // The session was renewed already: the retry goes to the new one.
        assert_eq!(upstream.sessions.load(Ordering::SeqCst), 2);
        let retried = c.rpc(call).await;
        assert_eq!(retried["result"]["content"][0]["text"], "hi", "{retried}");
        assert_eq!(
            upstream.seen.lock().last().unwrap().1.as_deref(),
            Some("s-2")
        );
        assert_eq!(calls(&upstream), 2);
    }

    #[tokio::test]
    async fn cancellation_aborts_the_call_without_waiting_on_upstream() {
        let (_d, _s, upstream, mut c) = open();
        c.initialize().await;
        c.send(json!({"jsonrpc":"2.0","id":7,"method":"slow"}))
            .await;
        until("the slow call upstream", || {
            upstream.methods().iter().any(|m| m == "slow")
        })
        .await;
        // The upstream never accepts the cancellation's own POST, which must
        // not hold up the next request.
        c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":7}}))
            .await;
        until("the call to be aborted", || {
            upstream.dropped.load(Ordering::SeqCst) == 1
        })
        .await;
        let list = c
            .rpc(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
            .await;
        assert_eq!(list["id"], 8, "no answer for the cancelled call: {list}");
    }

    #[tokio::test]
    async fn requests_beyond_the_in_flight_bound_are_refused() {
        let (_d, _s, _upstream, mut c) = open();
        c.initialize().await;
        for id in 0..MAX_IN_FLIGHT {
            c.send(json!({"jsonrpc":"2.0","id":id,"method":"slow"}))
                .await;
        }
        let refused = c
            .rpc(json!({"jsonrpc":"2.0","id":"over","method":"tools/list"}))
            .await;
        assert_eq!(refused["id"], "over");
        assert_eq!(refused["error"]["code"], -32000, "{refused}");
    }

    #[tokio::test]
    async fn an_overlong_line_closes_the_connection() {
        let (_d, _s, upstream, c) = open();
        let Client {
            mut lines,
            mut w,
            served,
        } = c;
        let _writer = tokio::spawn(async move {
            let _ = w.write_all(&vec![b'x'; MAX_BODY + 1]).await;
            // Kept open: the broker, not the client, ends the connection.
            std::future::pending::<()>().await;
            drop(w);
        });
        tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("the broker stops reading")
            .unwrap();
        let eof = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap();
        assert!(matches!(eof, Ok(None)), "{eof:?}");
        assert!(upstream.seen.lock().is_empty());
    }
}
