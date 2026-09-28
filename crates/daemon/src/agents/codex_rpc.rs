//! Bidirectional app-server calls. Server requests never consume client waiters.
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

type Answer = Result<Value, RpcError>;
#[derive(Default)]
struct Calls {
    closed: bool,
    pending: HashMap<u64, oneshot::Sender<Answer>>,
}

impl Calls {
    fn fail(&mut self, message: &str) {
        self.closed = true;
        for (_, sender) in self.pending.drain() {
            let _ = sender.send(Err(error(message)));
        }
    }
}

pub fn error(message: impl Into<String>) -> RpcError {
    let message = message.into();
    let lower = message.to_ascii_lowercase();
    if lower.contains("unauthorized")
        || lower.contains("authentication required")
        || lower.contains("invalid api key")
    {
        RpcError::new(ErrorCode::Unauthorized, message)
            .with_data(json!({"reason":"codex_auth_failed", "adapter":"codex"}))
    } else {
        RpcError::new(ErrorCode::AgentError, message)
    }
}

pub struct CodexRpc {
    writer: tokio::sync::Mutex<Option<ChildWriter>>,
    calls: Arc<Mutex<Calls>>,
    next: AtomicU64,
    timeout: Duration,
    reader: tokio::task::AbortHandle,
}

impl CodexRpc {
    pub fn connect(
        reader: ChildReader,
        writer: ChildWriter,
        secrets: Vec<String>,
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Value>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let incoming = calls.clone();
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut frame) = serde_json::from_str::<Value>(&line) else {
                    incoming.lock().fail("Codex returned invalid JSON");
                    return;
                };
                redact(&mut frame, &secrets);
                if frame.get("method").is_some() {
                    if events.send(frame).is_err() {
                        break;
                    }
                } else if let Some(id) = frame["id"].as_u64() {
                    let sender = incoming.lock().pending.remove(&id);
                    if let Some(sender) = sender {
                        let answer = if let Some(failure) = frame.get("error") {
                            Err(error(format!("Codex: {failure}")))
                        } else {
                            Ok(frame["result"].clone())
                        };
                        let _ = sender.send(answer);
                    }
                }
            }
            incoming.lock().fail("Codex connection closed");
        });
        (
            Arc::new(Self {
                writer: tokio::sync::Mutex::new(Some(writer)),
                calls,
                next: AtomicU64::new(1),
                timeout,
                reader: task.abort_handle(),
            }),
            receiver,
        )
    }

    async fn write(&self, frame: Value) -> Result<(), RpcError> {
        let mut bytes =
            serde_json::to_vec(&frame).map_err(|_| error("cannot encode Codex request"))?;
        bytes.push(b'\n');
        let result = tokio::time::timeout(self.timeout, async {
            let mut writer = self.writer.lock().await;
            let writer = writer
                .as_mut()
                .ok_or_else(|| error("Codex connection closed"))?;
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| error("cannot write to Codex"))?;
            writer
                .flush()
                .await
                .map_err(|_| error("cannot flush Codex request"))
        })
        .await
        .unwrap_or_else(|_| Err(error("Codex write timed out")));
        if result.is_err() {
            self.calls.lock().fail("Codex connection failed");
        }
        result
    }

    pub async fn call(&self, method: &str, params: Value) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock();
            if calls.closed {
                return Err(error("Codex connection closed"));
            }
            calls.pending.insert(id, sender);
        }
        // Cleanup also runs when the caller's future is cancelled.
        struct Pending<'a>(&'a Mutex<Calls>, u64);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.lock().pending.remove(&self.1);
            }
        }
        let _pending = Pending(&self.calls, id);
        self.write(json!({"id":id, "method":method, "params":params}))
            .await?;
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(error("Codex connection closed")),
            Err(_) => Err(error(format!("Codex {method} timed out"))),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.write(json!({"method":method, "params":params})).await
    }

    pub async fn reply(&self, id: Value, result: Value) -> Result<(), RpcError> {
        self.write(json!({"id":id,"result":result})).await
    }

    pub async fn close_input(&self) {
        self.calls.lock().fail("Codex connection closed");
        self.writer.lock().await.take();
    }

    pub async fn close(&self) {
        self.close_input().await;
        self.reader.abort();
    }
}

impl Drop for CodexRpc {
    fn drop(&mut self) {
        self.calls.lock().fail("Codex connection closed");
        self.reader.abort();
    }
}
