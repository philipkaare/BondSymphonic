//! ACP JSON-RPC 2.0 calls; peer request IDs never consume client waiters.
use super::codex_stream::redact;
use crate::sandbox::{ChildReader, ChildWriter};
use bondsymphonic_proto::{ErrorCode, RpcError};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
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

/// Native credentials can live in auth.json or provider JSON/JSONC config,
/// without entering process env. Refresh before consuming outputs and retain
/// old values after rotations.
pub struct AcpSecrets {
    auth: Option<PathBuf>,
    config: Vec<PathBuf>,
    known: Mutex<HashSet<String>>,
}
fn secret_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace(['_', '-'], "").as_str(),
        "key"
            | "access"
            | "refresh"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "apikey"
            | "authorization"
            | "password"
            | "clientsecret"
    )
}
fn collect_secrets(value: &Value, known: &mut HashSet<String>) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if secret_field(key) {
                    if let Some(secret) = value
                        .as_str()
                        .filter(|v| !v.is_empty() && *v != "[redacted]")
                    {
                        known.insert(secret.into());
                    }
                }
                collect_secrets(value, known);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_secrets(value, known);
            }
        }
        _ => (),
    }
}

/// Collect literal secret-valued property pairs from JSONC without loading or
/// rewriting the configuration. Decode quoted keys/values with serde so escaped
/// names and Unicode credentials work; comments are skipped only outside strings.
fn collect_config_literals(bytes: &[u8], known: &mut HashSet<String>) {
    let mut index = 0;
    let mut key: Option<String> = None;
    let mut secret_value = false;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && &bytes[index..index + 2] != b"*/" {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                        continue;
                    }
                    if bytes[index] == b'"' {
                        break;
                    }
                    index += 1;
                }
                if index == bytes.len() {
                    break;
                }
                index += 1;
                if let Ok(value) = serde_json::from_slice::<String>(&bytes[start..index]) {
                    if secret_value {
                        if !value.is_empty() && value != "[redacted]" {
                            known.insert(value);
                        }
                        key = None;
                        secret_value = false;
                    } else {
                        key = Some(value);
                    }
                } else {
                    key = None;
                    secret_value = false;
                }
            }
            b':' => {
                secret_value = key.take().is_some_and(|key| secret_field(&key));
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => {
                index += 1;
            }
            _ => {
                key = None;
                secret_value = false;
                index += 1;
            }
        }
    }
}
fn redact_native_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if secret_field(key) {
                    *value = json!("[redacted]");
                } else {
                    redact_native_fields(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_native_fields(value);
            }
        }
        Value::String(content) => {
            // Serialized tool output may contain fresh tokens not yet saved.
            if let Ok(mut nested @ (Value::Object(_) | Value::Array(_))) =
                serde_json::from_str::<Value>(content)
            {
                redact_native_fields(&mut nested);
                *content = nested.to_string();
            }
        }
        _ => (),
    }
}
impl AcpSecrets {
    pub fn new(env: &[(String, String)], api_key: Option<&str>) -> Self {
        let auth = env
            .iter()
            .find(|(k, _)| k == "XDG_DATA_HOME")
            .map(|(_, v)| PathBuf::from(v).join("opencode/auth.json"));
        let config = env
            .iter()
            .find(|(key, _)| key == "XDG_CONFIG_HOME")
            .map(|(_, root)| {
                ["config.json", "opencode.json", "opencode.jsonc"]
                    .iter()
                    .map(|name| PathBuf::from(root).join("opencode").join(name))
                    .collect()
            })
            .unwrap_or_default();
        let mut known = env
            .iter()
            .filter(|(k, _)| k.contains("KEY") || k.contains("TOKEN"))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .collect::<HashSet<_>>();
        if let Some(key) = api_key.filter(|key| !key.is_empty()) {
            known.insert(key.into());
        }
        let source = Self {
            auth,
            config,
            known: Mutex::new(known),
        };
        source.refresh();
        source
    }
    fn refresh(&self) {
        // Auth retains its existing 1 MiB bound. Accepted mirrored configs
        // must also fit the credential scan, including late literal keys.
        for (path, limit) in self.auth.iter().map(|path| (path, 1024 * 1024)).chain(
            self.config
                .iter()
                .map(|path| (path, super::opencode_config::CONFIG_READ_LIMIT)),
        ) {
            let Ok(bytes) = super::opencode_config::read_regular_file(path, limit, false) else {
                continue;
            };
            let mut known = self.known.lock();
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                collect_secrets(&value, &mut known);
            } else {
                collect_config_literals(&bytes, &mut known);
            }
        }
    }
    pub fn redact(&self, value: &mut Value) {
        self.refresh();
        let mut known = self.known.lock().iter().cloned().collect::<Vec<_>>();
        known.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        redact_native_fields(value);
        redact(value, &known);
    }
}
pub enum Incoming {
    Message(Value),
    /// Ensure preceding streamed messages are consumed before a call returns.
    Barrier(oneshot::Sender<()>),
}
#[derive(Default)]
struct Calls {
    closed: bool,
    pending: HashMap<u64, oneshot::Sender<Answer>>,
}

pub struct PromptCall {
    id: u64,
    calls: Arc<Mutex<Calls>>,
    receiver: Option<oneshot::Receiver<Answer>>,
}
impl PromptCall {
    pub async fn wait(mut self) -> Answer {
        self.receiver
            .take()
            .unwrap()
            .await
            .unwrap_or_else(|_| Err(error("OpenCode connection closed")))
    }
}
impl Drop for PromptCall {
    fn drop(&mut self) {
        self.calls.lock().pending.remove(&self.id);
    }
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
    if lower.contains("auth_required")
        || lower.contains("unauthorized")
        || lower.contains("authentication required")
        || lower.contains("invalid api key")
    {
        RpcError::new(ErrorCode::Unauthorized, message)
            .with_data(json!({"reason":"opencode_auth_failed", "adapter":"opencode"}))
    } else {
        RpcError::new(ErrorCode::AgentError, message)
    }
}

pub struct AcpRpc {
    writer: tokio::sync::Mutex<Option<ChildWriter>>,
    calls: Arc<Mutex<Calls>>,
    next: AtomicU64,
    timeout: Duration,
    reader: tokio::task::AbortHandle,
}

impl AcpRpc {
    pub fn connect(
        reader: ChildReader,
        writer: ChildWriter,
        secrets: Arc<AcpSecrets>,
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Incoming>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let incoming = calls.clone();
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut frame) = serde_json::from_str::<Value>(&line) else {
                    incoming.lock().fail("OpenCode returned invalid JSON");
                    return;
                };
                secrets.redact(&mut frame);
                if frame["jsonrpc"] != "2.0" {
                    incoming
                        .lock()
                        .fail("OpenCode returned an invalid JSON-RPC version");
                    return;
                }
                if frame.get("method").is_some() {
                    if events.send(Incoming::Message(frame)).is_err() {
                        break;
                    }
                } else if let Some(id) = frame["id"].as_u64() {
                    let sender = incoming.lock().pending.remove(&id);
                    if let Some(sender) = sender {
                        let (ready, consumed) = oneshot::channel();
                        if events.send(Incoming::Barrier(ready)).is_err() || consumed.await.is_err()
                        {
                            let _ = sender.send(Err(error("OpenCode event consumer closed")));
                            break;
                        }
                        let answer = if let Some(failure) = frame.get("error") {
                            Err(error(format!("OpenCode: {failure}")))
                        } else {
                            Ok(frame["result"].clone())
                        };
                        let _ = sender.send(answer);
                    }
                }
            }
            incoming.lock().fail("OpenCode connection closed");
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
            serde_json::to_vec(&frame).map_err(|_| error("cannot encode OpenCode request"))?;
        bytes.push(b'\n');
        let result = tokio::time::timeout(self.timeout, async {
            let mut writer = self.writer.lock().await;
            let writer = writer
                .as_mut()
                .ok_or_else(|| error("OpenCode connection closed"))?;
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| error("cannot write to OpenCode"))?;
            writer
                .flush()
                .await
                .map_err(|_| error("cannot flush OpenCode request"))
        })
        .await
        .unwrap_or_else(|_| Err(error("OpenCode write timed out")));
        if result.is_err() {
            self.calls.lock().fail("OpenCode connection failed");
        }
        result
    }

    /// Prompt completion can take arbitrarily long, especially at approval.
    /// Writing and startup calls remain bounded; cancellation is a notification.
    pub async fn start_prompt(&self, params: Value) -> Result<PromptCall, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock();
            if calls.closed {
                return Err(error("OpenCode connection closed"));
            }
            calls.pending.insert(id, sender);
        }
        let pending = PromptCall {
            id,
            calls: self.calls.clone(),
            receiver: Some(receiver),
        };
        self.write(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":params}))
            .await?;
        Ok(pending)
    }

    pub async fn call(&self, method: &str, params: Value) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock();
            if calls.closed {
                return Err(error("OpenCode connection closed"));
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
        self.write(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
            .await?;
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(error("OpenCode connection closed")),
            Err(_) => Err(error(format!("OpenCode {method} timed out"))),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0", "method":method, "params":params}))
            .await
    }

    pub async fn reply(&self, id: Value, result: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0", "id":id,"result":result}))
            .await
    }

    pub async fn reject_unsupported(&self, id: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0", "id":id,"error":{"code":-32601,"message":"This OpenCode request is not supported by BondSymphonic"}})).await
    }

    pub async fn reject_session(&self, id: Value) -> Result<(), RpcError> {
        self.write(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"This ACP session is not active in BondSymphonic"}})).await
    }

    pub async fn close_input(&self) {
        self.calls.lock().fail("OpenCode connection closed");
        self.writer.lock().await.take();
    }

    pub async fn close(&self) {
        self.close_input().await;
        self.reader.abort();
    }
}

impl Drop for AcpRpc {
    fn drop(&mut self) {
        self.calls.lock().fail("OpenCode connection closed");
        self.reader.abort();
    }
}
