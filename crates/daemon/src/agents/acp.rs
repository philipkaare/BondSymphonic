//! Bounded ACP v1 newline JSON-RPC transport. Peer IDs are kept as JSON values.
use super::codex_stream::redact;
use crate::sandbox::{ChildReader, ChildWriter};
use bondsymphonic_proto::{ErrorCode, RpcError};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};
pub const MAX_FRAME: usize = 1024 * 1024;
pub fn error(s: impl Into<String>) -> RpcError {
    RpcError::new(ErrorCode::AgentError, s)
}
type Answer = Result<Value, RpcError>;
/// Local transport events cannot be forged by a wire method name.
/// Completing a turn/load waits until the event pump acknowledges this event.
pub enum AcpEvent {
    Frame(Value),
    Response {
        method: String,
        answer: Result<Value, RpcError>,
        complete: oneshot::Sender<Result<Value, RpcError>>,
    },
}
#[derive(Default)]
struct Calls {
    closed: bool,
    pending: HashMap<u64, (oneshot::Sender<Answer>, String)>,
}
impl Calls {
    fn fail(&mut self, s: &str) {
        self.closed = true;
        for (_, (tx, _)) in self.pending.drain() {
            let _ = tx.send(Err(error(s)));
        }
    }
}
pub struct AcpClient {
    writer: tokio::sync::Mutex<Option<ChildWriter>>,
    calls: Arc<Mutex<Calls>>,
    next: AtomicU64,
    timeout: Duration,
    reader: tokio::task::AbortHandle,
}
impl AcpClient {
    pub fn connect(
        mut reader: ChildReader,
        writer: ChildWriter,
        secrets: Vec<String>,
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::Receiver<AcpEvent>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let incoming = calls.clone();
        let (tx, rx) = mpsc::channel(128);
        let task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            'read: loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for &b in &chunk[..n] {
                    if b != b'\n' {
                        if buf.len() >= MAX_FRAME {
                            incoming.lock().fail("ACP frame exceeds limit");
                            break 'read;
                        }
                        buf.push(b);
                        continue;
                    }
                    let Ok(frame) = serde_json::from_slice::<Value>(&buf) else {
                        incoming.lock().fail("ACP invalid JSON");
                        break 'read;
                    };
                    buf.clear();
                    if frame["jsonrpc"] != "2.0" {
                        incoming.lock().fail("ACP invalid JSON-RPC version");
                        break 'read;
                    }
                    if frame.get("method").is_some() {
                        if tx.try_send(AcpEvent::Frame(frame)).is_err() {
                            incoming.lock().fail("ACP peer event queue overflow");
                            break 'read;
                        }
                    } else if let Some(id) = frame["id"].as_u64() {
                        let sender = incoming.lock().pending.remove(&id);
                        if let Some((sender, method)) = sender {
                            let answer = if let Some(e) = frame.get("error") {
                                let mut diagnostic = e.clone();
                                redact(&mut diagnostic, &secrets);
                                Err(error(format!("ACP: {diagnostic}")))
                            } else {
                                Ok(frame["result"].clone())
                            };
                            if matches!(method.as_str(), "session/load" | "session/prompt") {
                                if let Err(failed) = tx.try_send(AcpEvent::Response {
                                    method,
                                    answer,
                                    complete: sender,
                                }) {
                                    incoming.lock().fail("ACP peer event queue overflow");
                                    if let AcpEvent::Response { complete, .. } = failed.into_inner()
                                    {
                                        let _ =
                                            complete.send(Err(error("ACP event queue overflow")));
                                    }
                                    break 'read;
                                }
                            } else {
                                let _ = sender.send(answer);
                            }
                        }
                    }
                }
            }
            incoming.lock().fail("ACP connection closed");
        });
        (
            Arc::new(Self {
                writer: tokio::sync::Mutex::new(Some(writer)),
                calls,
                next: AtomicU64::new(1),
                timeout,
                reader: task.abort_handle(),
            }),
            rx,
        )
    }
    async fn write(&self, frame: Value) -> Result<(), RpcError> {
        let mut bytes = serde_json::to_vec(&frame).map_err(|_| error("ACP encoding failed"))?;
        if bytes.len() > MAX_FRAME {
            return Err(error("ACP outgoing frame exceeds limit"));
        }
        bytes.push(b'\n');
        tokio::time::timeout(self.timeout, async {
            let mut lock = self.writer.lock().await;
            let w = lock.as_mut().ok_or_else(|| error("ACP closed"))?;
            w.write_all(&bytes)
                .await
                .map_err(|_| error("ACP write failed"))?;
            w.flush().await.map_err(|_| error("ACP flush failed"))
        })
        .await
        .map_err(|_| error("ACP write timed out"))?
    }
    pub async fn call(&self, method: &str, params: Value) -> Answer {
        self.call_with_timeout(method, params, self.timeout).await
    }
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut c = self.calls.lock();
            if c.closed {
                return Err(error("ACP closed"));
            }
            if c.pending.len() >= 32 {
                return Err(error("ACP pending request limit"));
            }
            c.pending.insert(id, (tx, method.to_owned()));
        }
        struct Guard<'a>(&'a Mutex<Calls>, u64);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.lock().pending.remove(&self.1);
            }
        }
        let _guard = Guard(&self.calls, id);
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| error(format!("ACP {method} timed out")))?
            .map_err(|_| error("ACP closed"))?
    }
    pub async fn notify(&self, m: &str, p: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0","method":m,"params":p}))
            .await
    }
    pub async fn reply(&self, id: Value, result: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }
    pub async fn reject(&self, id: Value, code: i32, message: &str) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
            .await
    }
    pub async fn close(&self) {
        self.calls.lock().fail("ACP closed");
        self.reader.abort();
        self.writer.lock().await.take();
    }
}
impl Drop for AcpClient {
    fn drop(&mut self) {
        self.calls.lock().fail("ACP closed");
        self.reader.abort();
    }
}
