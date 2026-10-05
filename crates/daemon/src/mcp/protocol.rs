//! The server side of MCP over the stdio transport's framing:
//! newline-delimited JSON-RPC 2.0, one message per line.
//!
//! Hand-written because the daemon needs five methods of it and no MCP crate
//! is a dependency. Transport-agnostic: `serve` takes any reader and writer,
//! which in production is one connection to a workspace's `mcp.sock` and in
//! tests is an in-memory pipe.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The longest line accepted, newline included; a longer one closes the connection.
pub const MAX_LINE: usize = 1 << 20;

type Calls = Arc<parking_lot::Mutex<HashMap<String, tokio::task::AbortHandle>>>;

/// Ends a session when `serve` returns or is dropped mid-await (the listener
/// being cancelled): aborts every in-flight call and the writer, so the
/// connection closes instead of lingering until the last call finishes.
struct SessionGuard {
    calls: Calls,
    writer: tokio::task::AbortHandle,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        for (_, h) in self.calls.lock().drain() {
            h.abort();
        }
        self.writer.abort();
    }
}
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub read_only: bool,
}

pub enum ToolOutcome {
    Ok(String),
    Failed(String),
    InvalidArguments(String),
}

#[async_trait]
pub trait ToolHost: Send + Sync + 'static {
    fn tools(&self) -> Vec<ToolSpec>;
    async fn call(&self, name: &str, args: Value) -> ToolOutcome;
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let version = VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": super::SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
    })
}

fn list(host: &dyn ToolHost) -> Value {
    let tools: Vec<Value> = host
        .tools()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
                "annotations": {"readOnlyHint": t.read_only},
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// The key a request id is remembered under for cancellation: its JSON text,
/// so `1` and `"1"` stay different ids.
fn key(id: &Value) -> String {
    id.to_string()
}

pub async fn serve<R, W>(reader: R, mut writer: W, host: Arc<dyn ToolHost>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
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
    let session = SessionGuard {
        calls: calls.clone(),
        writer: writer_task.abort_handle(),
    };
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bounded read: `take` stops a line from growing past the cap.
        let n = match (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(n) => n,
            Err(_) => break,
        };
        if buf.len() > MAX_LINE {
            tracing::warn!("mcp: line over {MAX_LINE} bytes, closing the connection");
            break;
        }
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                let _ = out_tx.send(error(&Value::Null, -32700, "parse error"));
                continue;
            }
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            ("notifications/cancelled", None) => {
                if let Some(h) = calls.lock().remove(&key(&params["requestId"])) {
                    h.abort();
                }
            }
            (_, None) => {} // any other notification: nothing to say
            ("initialize", Some(id)) => {
                let _ = out_tx.send(reply(&id, initialize(&params)));
            }
            ("ping", Some(id)) => {
                let _ = out_tx.send(reply(&id, json!({})));
            }
            ("tools/list", Some(id)) => {
                let _ = out_tx.send(reply(&id, list(&*host)));
            }
            ("tools/call", Some(id)) => {
                let Some(name) = params["name"].as_str().map(str::to_owned) else {
                    let _ = out_tx.send(error(&id, -32602, "tools/call needs a name"));
                    continue;
                };
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => json!({}),
                    Some(v) => v.clone(),
                };
                let (host, out_tx, calls2, k) =
                    (host.clone(), out_tx.clone(), calls.clone(), key(&id));
                // Registered under the lock the task takes to deregister, so a call
                // that finishes at once cannot leave a stale entry behind.
                let mut guard = calls.lock();
                let task = tokio::spawn(async move {
                    let msg = match host.call(&name, args).await {
                        ToolOutcome::Ok(text) => reply(
                            &id,
                            json!({"content":[{"type":"text","text":text}],"isError":false}),
                        ),
                        ToolOutcome::Failed(text) => reply(
                            &id,
                            json!({"content":[{"type":"text","text":text}],"isError":true}),
                        ),
                        ToolOutcome::InvalidArguments(why) => error(&id, -32602, &why),
                    };
                    calls2.lock().remove(&key(&id));
                    let _ = out_tx.send(msg);
                });
                // A reused id replaces the older call, which is aborted rather than orphaned.
                if let Some(old) = guard.insert(k, task.abort_handle()) {
                    old.abort();
                }
                drop(guard);
            }
            (_, Some(id)) => {
                let _ = out_tx.send(error(&id, -32601, "method not found"));
            }
        }
    }
    // Graceful end (reader closed): abort calls, let queued replies flush.
    for (_, h) in calls.lock().drain() {
        h.abort();
    }
    drop(out_tx);
    let _ = writer_task.await;
    drop(session);
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct Fake;
    #[async_trait::async_trait]
    impl ToolHost for Fake {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "echo",
                description: "echo",
                input_schema: json!({"type":"object"}),
                read_only: true,
            }]
        }
        async fn call(&self, name: &str, args: Value) -> ToolOutcome {
            match name {
                "echo" => ToolOutcome::Ok(args["text"].as_str().unwrap_or("").into()),
                "slow" => {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    ToolOutcome::Ok("late".into())
                }
                "fail" => ToolOutcome::Failed("it broke".into()),
                other => ToolOutcome::InvalidArguments(format!("unknown tool {other}")),
            }
        }
    }

    /// A client end and the lines it reads back.
    async fn session() -> (
        tokio::io::DuplexStream,
        tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
    ) {
        let (client_w, server_r) = tokio::io::duplex(1 << 16);
        let (server_w, client_r) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(server_r, server_w, Arc::new(Fake)));
        (client_w, BufReader::new(client_r).lines())
    }

    async fn send(w: &mut tokio::io::DuplexStream, v: Value) {
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }
    async fn recv(r: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>) -> Value {
        serde_json::from_str(&r.next_line().await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_version_and_offers_tools() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "bondsymphonic");
        assert!(v["result"]["capabilities"]["tools"].is_object());
        send(&mut w, json!({"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"1999-01-01"}})).await;
        assert_eq!(
            recv(&mut r).await["result"]["protocolVersion"],
            "2025-06-18"
        );
    }

    #[tokio::test]
    async fn tools_list_and_call() {
        let (mut w, mut r) = session().await;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let v = recv(&mut r).await;
        assert_eq!(
            v["id"], 2,
            "a notification gets no reply, so this is the first line"
        );
        assert_eq!(v["result"]["tools"][0]["name"], "echo");
        assert_eq!(v["result"]["tools"][0]["annotations"]["readOnlyHint"], true);
        assert!(v["result"]["tools"][0]["inputSchema"].is_object());
        send(&mut w, json!({"jsonrpc":"2.0","id":"s","method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["id"], "s");
        assert_eq!(
            v["result"]["content"][0],
            json!({"type":"text","text":"hi"})
        );
        assert_eq!(v["result"]["isError"], false);
        send(&mut w, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fail","arguments":{}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(v["result"]["content"][0]["text"], "it broke");
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}),
        )
        .await;
        assert_eq!(recv(&mut r).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_methods_ping_and_malformed_lines() {
        let (mut w, mut r) = session().await;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":5,"method":"resources/list"}),
        )
        .await;
        assert_eq!(recv(&mut r).await["error"]["code"], -32601);
        send(&mut w, json!({"jsonrpc":"2.0","id":6,"method":"ping"})).await;
        assert_eq!(recv(&mut r).await["result"], json!({}));
        w.write_all(b"{not json\n").await.unwrap();
        let v = recv(&mut r).await;
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["id"].is_null());
    }

    #[tokio::test]
    async fn concurrent_requests_both_answer() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"slow","arguments":{}}})).await;
        send(&mut w, json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"fast"}}})).await;
        // The fast one answers while the slow one is still running.
        let v = tokio::time::timeout(std::time::Duration::from_secs(5), recv(&mut r))
            .await
            .unwrap();
        assert_eq!(v["id"], 11);
    }

    #[tokio::test]
    async fn a_cancelled_call_sends_no_reply() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"slow","arguments":{}}})).await;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":20}}),
        )
        .await;
        send(&mut w, json!({"jsonrpc":"2.0","id":21,"method":"ping"})).await;
        assert_eq!(recv(&mut r).await["id"], 21);
        // Nothing else arrives for id 20.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), r.next_line())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_oversize_line_ends_the_session() {
        let (mut w, mut r) = session().await;
        let big = vec![b'x'; MAX_LINE + 10];
        w.write_all(&big).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        assert!(
            r.next_line().await.unwrap().is_none(),
            "the server hangs up"
        );
    }

    #[tokio::test]
    async fn closing_the_client_write_half_closes_the_connection() {
        let (mut w, mut r) = session().await;
        w.shutdown().await.unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), r.next_line()).await;
        assert!(next.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn dropping_serve_closes_the_connection() {
        let (mut client_w, server_r) = tokio::io::duplex(1 << 16);
        let (server_w, client_r) = tokio::io::duplex(1 << 16);
        let mut r = BufReader::new(client_r).lines();
        let task = tokio::spawn(serve(server_r, server_w, Arc::new(Fake)));
        send(&mut client_w, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow","arguments":{}}})).await;
        send(
            &mut client_w,
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        )
        .await;
        assert_eq!(recv(&mut r).await["id"], 2);
        task.abort();
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), r.next_line()).await;
        assert!(next.expect("prompt EOF").unwrap().is_none());
    }
}
