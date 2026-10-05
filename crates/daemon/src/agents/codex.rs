//! One Codex app-server process per agent, inside the selected backend sandbox.
use super::{
    adapter::AgentAdapter,
    backend::PreparedAgent,
    codex_rpc::{error, CodexRpc},
    codex_stream::{approval_id, redact, text, CodexStream},
    AgentSink,
};
use crate::sandbox::{SandboxCommand, SandboxHandle};
use async_trait::async_trait;
use bondsymphonic_proto::{AgentMessageBody, AgentState, PermissionDecision, RpcError};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    task::JoinHandle,
};

pub const TESTED_CODEX_VERSION: &str = "0.158.0";

#[derive(Default)]
struct TurnState {
    active: Option<String>,
    last_finished: Option<String>,
    approvals: HashMap<String, Value>,
}

pub struct CodexAdapter {
    sink: AgentSink,
    handle: Arc<dyn SandboxHandle>,
    prepared: PreparedAgent,
    rpc: Option<Arc<CodexRpc>>,
    thread: String,
    state: Arc<Mutex<TurnState>>,
    changed: Arc<tokio::sync::Notify>,
    exited: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    killer: Option<Arc<dyn Fn() + Send + Sync>>,
    monitor: Option<JoinHandle<()>>,
    timeout: Duration,
}

async fn finish(sink: &AgentSink, exited: &AtomicBool, detail: Option<String>) {
    if exited.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), sink.ended()).await;
    sink.publish_state(AgentState::Exited, detail);
}

impl CodexAdapter {
    pub fn new(sink: AgentSink, handle: Arc<dyn SandboxHandle>, prepared: PreparedAgent) -> Self {
        Self {
            sink,
            handle,
            prepared,
            rpc: None,
            thread: String::new(),
            state: Arc::new(Mutex::new(TurnState::default())),
            changed: Arc::new(tokio::sync::Notify::new()),
            exited: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            killer: None,
            monitor: None,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn rpc(&self) -> Result<Arc<CodexRpc>, RpcError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(error("Codex agent has exited"));
        }
        self.rpc
            .clone()
            .ok_or_else(|| error("Codex agent has not started"))
    }

    async fn initialize(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        rpc.call(
            "initialize",
            json!({"clientInfo":{"name":"bondsymphonic", "version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        rpc.notify("initialized", json!({})).await?;
        // `developerInstructions` is on both `thread/start` and `thread/resume`
        // (checked against the 0.160.0 schema), so a resumed thread is told too.
        let mut params = json!({"cwd":self.prepared.cwd, "approvalPolicy": self.prepared.options.permission_mode.as_deref().unwrap_or("never"), "sandbox":"danger-full-access", "developerInstructions": super::SANDBOX_GIT_NOTE});
        if let Some(model) = self
            .prepared
            .options
            .model
            .as_deref()
            .filter(|model| !model.is_empty())
        {
            params["model"] = json!(model);
        }
        let method = if let Some(session) = &self.prepared.options.resume_session {
            params["threadId"] = json!(session);
            "thread/resume"
        } else {
            "thread/start"
        };
        let result = rpc.call(method, params).await?;
        self.thread = text(&result["thread"], "id").into();
        if self.thread.is_empty() {
            return Err(error("Codex did not return a thread ID"));
        }
        self.sink.session_id(self.thread.clone()).await;
        Ok(())
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    async fn start(&mut self) -> Result<(), RpcError> {
        if self.rpc.is_some() {
            return Err(error("Codex already started"));
        }
        let mut child = self
            .handle
            .spawn(SandboxCommand {
                argv: self.prepared.argv.clone(),
                env: self.prepared.env.clone(),
                cwd: Some(self.prepared.cwd.clone()),
                pty: None,
            })
            .await?;
        let killer: Arc<dyn Fn() + Send + Sync> = Arc::from(child.killer);
        self.killer = Some(killer.clone());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| error("Codex stdout unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| error("Codex stdin unavailable"))?;
        let secrets: Vec<String> = self
            .prepared
            .env
            .iter()
            .filter(|(k, _)| k.contains("KEY") || k.contains("TOKEN"))
            .map(|(_, v)| v.clone())
            .collect();
        let tail = Arc::new(Mutex::new(String::new()));
        let stderr_tail = tail.clone();
        let stderr_secrets = secrets.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = child.stderr.take() {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut value = json!(line);
                    redact(&mut value, &stderr_secrets);
                    let line = value.as_str().unwrap_or_default();
                    *stderr_tail.lock() = line.chars().take(2000).collect();
                }
            }
        });
        let (rpc, mut events) = CodexRpc::connect(stdout, stdin, secrets, self.timeout);
        self.rpc = Some(rpc.clone());
        let state = self.state.clone();
        let changed = self.changed.clone();
        let sink = self.sink.clone();
        let event_rpc = rpc.clone();
        let mut pump = tokio::spawn(async move {
            let mut stream = CodexStream::default();
            while let Some(frame) = events.recv().await {
                let p = &frame["params"];
                match text(&frame, "method") {
                    "turn/started" => {
                        state.lock().active = Some(text(&p["turn"], "id").into());
                        sink.publish_state(AgentState::Working, None);
                        changed.notify_waiters();
                    }
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        state
                            .lock()
                            .approvals
                            .insert(approval_id(&frame["id"]), frame["id"].clone());
                        sink.publish_state(AgentState::WaitingPermission, None);
                    }
                    "turn/completed" => {
                        let mut current = state.lock();
                        current.active = None;
                        current.last_finished = Some(text(&p["turn"], "id").into());
                        current.approvals.clear();
                        drop(current);
                        let failed = text(&p["turn"], "status") == "failed";
                        sink.publish_state(
                            if failed {
                                AgentState::Error
                            } else {
                                AgentState::Idle
                            },
                            failed.then(|| format!("Codex: {}", p["turn"]["error"])),
                        );
                        changed.notify_waiters();
                    }
                    _ => {
                        if let Some(id) = frame.get("id") {
                            if event_rpc.reject_unsupported(id.clone()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                for body in stream.ingest(frame) {
                    sink.message(body).await;
                }
            }
        });
        let sink = self.sink.clone();
        let exited = self.exited.clone();
        let stopping = self.stopping.clone();
        let changed = self.changed.clone();
        self.monitor = Some(tokio::spawn(async move {
            let mut exit = child.exit;
            let code = tokio::select! {
                code = &mut exit => {
                    if tokio::time::timeout(Duration::from_secs(2), &mut pump).await.is_err() { pump.abort(); }
                    code.ok()
                }
                _ = &mut pump => {
                    killer();
                    tokio::time::timeout(Duration::from_secs(2), &mut exit).await.ok().and_then(Result::ok)
                }
            };
            rpc.close().await;
            let mut stderr_task = stderr_task;
            if tokio::time::timeout(Duration::from_millis(500), &mut stderr_task)
                .await
                .is_err()
            {
                stderr_task.abort();
            }
            let detail = if stopping.load(Ordering::SeqCst) {
                None
            } else {
                Some(format!("Codex exited ({code:?}): {}", tail.lock()))
            };
            finish(&sink, &exited, detail).await;
            changed.notify_waiters();
        }));
        if let Err(e) = self.initialize().await {
            let _ = self.stop().await;
            return Err(e);
        }
        Ok(())
    }

    async fn send(&mut self, text: String) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let active = {
            let state = self.state.lock();
            if !state.approvals.is_empty() {
                return Err(error("Codex is waiting for permission"));
            }
            state.active.clone()
        };
        let input = json!([{"type":"text", "text":text, "text_elements":[]}]);
        self.sink.message(AgentMessageBody::UserText { text }).await;
        if let Some(turn) = active {
            rpc.call(
                "turn/steer",
                json!({"threadId":self.thread, "expectedTurnId":turn,"input":input}),
            )
            .await?;
        } else {
            let result = rpc
                .call("turn/start", json!({"threadId":self.thread, "input":input}))
                .await?;
            let id = text_id(&result)?;
            // A turn/start response precedes turn activation. Do not allow the
            // next steer/interrupt to race that activation (observed live).
            tokio::time::timeout(self.timeout, async {
                loop {
                    let notified = self.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    {
                        let state = self.state.lock();
                        if state.active.as_ref() == Some(&id)
                            || state.last_finished.as_ref() == Some(&id)
                        {
                            return Ok(());
                        }
                        if self.exited.load(Ordering::SeqCst) {
                            return Err(error("Codex exited during turn startup"));
                        }
                    }
                    notified.await;
                }
            })
            .await
            .map_err(|_| error("Codex turn activation timed out"))??;
        }
        Ok(())
    }

    async fn permission_reply(
        &mut self,
        request_id: String,
        decision: PermissionDecision,
        updated_input: Option<Value>,
        _message: Option<String>,
    ) -> Result<(), RpcError> {
        if updated_input.is_some() {
            return Err(RpcError::invalid_params(
                "Codex approvals cannot edit tool input",
            ));
        }
        let rpc = self.rpc()?;
        let id = {
            let mut state = self.state.lock();
            let id = state
                .approvals
                .remove(&request_id)
                .ok_or_else(|| RpcError::invalid_params("unknown or answered Codex approval"))?;
            self.sink.publish_state(
                if state.approvals.is_empty() {
                    AgentState::Working
                } else {
                    AgentState::WaitingPermission
                },
                None,
            );
            id
        };
        let answer = match decision {
            PermissionDecision::Allow => "accept",
            PermissionDecision::AllowForSession => "acceptForSession",
            PermissionDecision::Deny => "decline",
        };
        // Publish before writing: a completion can arrive immediately after it.
        self.sink
            .message(AgentMessageBody::System {
                subtype: "permission_reply".into(),
                data: json!({"request_id":request_id,"decision":decision}),
            })
            .await;
        rpc.reply(id, json!({"decision":answer})).await
    }

    async fn interrupt(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let turn = self.state.lock().active.clone();
        if let Some(turn) = turn {
            rpc.call(
                "turn/interrupt",
                json!({"threadId":self.thread,"turnId":turn}),
            )
            .await?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), RpcError> {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(rpc) = &self.rpc {
            rpc.close_input().await;
        }
        if let Some(mut monitor) = self.monitor.take() {
            if tokio::time::timeout(Duration::from_secs(5), &mut monitor)
                .await
                .is_err()
            {
                if let Some(kill) = &self.killer {
                    kill();
                }
                if tokio::time::timeout(Duration::from_secs(3), &mut monitor)
                    .await
                    .is_err()
                {
                    monitor.abort();
                }
            }
        }
        finish(&self.sink, &self.exited, None).await;
        Ok(())
    }
}

fn text_id(result: &Value) -> Result<String, RpcError> {
    let id = text(&result["turn"], "id");
    if id.is_empty() {
        Err(error("Codex did not return a turn ID"))
    } else {
        Ok(id.into())
    }
}

impl Drop for CodexAdapter {
    fn drop(&mut self) {
        if !self.exited.load(Ordering::SeqCst) {
            if let Some(kill) = &self.killer {
                kill();
            }
        }
    }
}
