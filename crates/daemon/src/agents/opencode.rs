//! One OpenCode ACP process per agent; prompts run outside the manager lock.
use super::{
    acp::{error, AcpRpc, AcpSecrets, Incoming},
    adapter::AgentAdapter,
    backend::PreparedAgent,
    codex_stream::text,
    opencode_stream::{approval_id, OpenCodeStream},
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
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};

struct Approval {
    id: Value,
    options: Vec<Value>,
}
#[derive(Default)]
struct TurnState {
    active: bool,
    approvals: HashMap<String, Approval>,
}
struct Prompt {
    text: String,
    generation: u64,
}
pub struct OpenCodeAdapter {
    sink: AgentSink,
    handle: Arc<dyn SandboxHandle>,
    prepared: PreparedAgent,
    rpc: Option<Arc<AcpRpc>>,
    session: String,
    state: Arc<Mutex<TurnState>>,
    generation: Arc<AtomicU64>,
    dispatch: Arc<tokio::sync::Mutex<()>>,
    replaying: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    prompts: Option<mpsc::Sender<Prompt>>,
    killer: Option<Arc<dyn Fn() + Send + Sync>>,
    monitor: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
    timeout: Duration,
    secrets: Arc<AcpSecrets>,
}
async fn finish(sink: &AgentSink, exited: &AtomicBool, detail: Option<String>) {
    if exited.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), sink.ended()).await;
    sink.publish_state(AgentState::Exited, detail);
}
impl OpenCodeAdapter {
    pub fn new(sink: AgentSink, handle: Arc<dyn SandboxHandle>, prepared: PreparedAgent) -> Self {
        let secrets = Arc::new(AcpSecrets::new(
            &prepared.env,
            prepared.options.api_key.as_deref(),
        ));
        Self {
            sink,
            handle,
            prepared,
            rpc: None,
            session: String::new(),
            state: Arc::new(Mutex::new(TurnState::default())),
            generation: Arc::new(AtomicU64::new(0)),
            dispatch: Arc::new(tokio::sync::Mutex::new(())),
            replaying: Arc::new(AtomicBool::new(true)),
            exited: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            prompts: None,
            killer: None,
            monitor: None,
            worker: None,
            timeout: Duration::from_secs(30),
            secrets,
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    fn rpc(&self) -> Result<Arc<AcpRpc>, RpcError> {
        if self.exited.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) {
            return Err(error("OpenCode agent has exited"));
        }
        self.rpc
            .clone()
            .ok_or_else(|| error("OpenCode agent has not started"))
    }
    async fn initialize(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let result=rpc.call("initialize",json!({"protocolVersion":1,"clientInfo":{"name":"bondsymphonic","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}})).await?;
        if result["protocolVersion"].as_u64() != Some(1) {
            return Err(error(
                "OpenCode requires an unsupported ACP protocol version; expected version 1",
            ));
        }
        let sandboxed = self
            .prepared
            .env
            .iter()
            .any(|(k, v)| k == "BS_OPENCODE_SANDBOX" && v == "1");
        let servers = if sandboxed {
            json!([{"name":crate::mcp::SERVER_NAME,"command":crate::mcp::DAEMON_IN_SANDBOX,"args":crate::mcp::bridge_args(),"env":[]}])
        } else {
            json!([])
        };
        let mut params = json!({"cwd":self.prepared.cwd,"mcpServers":servers});
        let resume = self
            .prepared
            .options
            .resume_session
            .as_deref()
            .filter(|s| !s.is_empty());
        let method = if let Some(session) = resume {
            params["sessionId"] = json!(session);
            if result["agentCapabilities"]["sessionCapabilities"]
                .get("resume")
                .is_some()
            {
                "session/resume"
            } else if result["agentCapabilities"]["loadSession"] == true {
                "session/load"
            } else {
                return Err(error("This OpenCode version cannot resume ACP sessions"));
            }
        } else {
            "session/new"
        };
        let created = rpc.call(method, params).await?;
        self.session = created["sessionId"]
            .as_str()
            .or(resume)
            .unwrap_or_default()
            .into();
        if self.session.is_empty() {
            return Err(error("OpenCode did not return a session ID"));
        }
        // The transport barrier consumes replay before the load response.
        self.replaying.store(false, Ordering::SeqCst);
        self.sink.session_id(self.session.clone()).await;
        // Resume can retain native build mode; explicitly select the primary
        // agent whose rules implement the IDE permission choice.
        if let Some(agent) = self
            .prepared
            .env
            .iter()
            .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
            .and_then(|(_, value)| serde_json::from_str::<Value>(value).ok())
            .and_then(|config| config["default_agent"].as_str().map(str::to_owned))
        {
            rpc.call(
                "session/set_mode",
                json!({"sessionId":self.session,"modeId":agent}),
            )
            .await?;
        }
        if let Some(model) = self
            .prepared
            .options
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
        {
            let config = created["configOptions"]
                .as_array()
                .and_then(|o| {
                    o.iter()
                        .find(|o| o["category"] == "model" || o["id"] == "model")
                })
                .and_then(|o| o["id"].as_str());
            if let Some(config) = config {
                rpc.call(
                    "session/set_config_option",
                    json!({"sessionId":self.session,"configId":config,"value":model}),
                )
                .await?;
            } else {
                rpc.call(
                    "session/set_model",
                    json!({"sessionId":self.session,"modelId":model}),
                )
                .await?;
            }
        }
        Ok(())
    }
    fn start_worker(&mut self) {
        let (sender, mut receiver) = mpsc::channel::<Prompt>(64);
        self.prompts = Some(sender);
        let rpc = self.rpc.as_ref().unwrap().clone();
        let sink = self.sink.clone();
        let session = self.session.clone();
        let state = self.state.clone();
        let generation = self.generation.clone();
        let dispatch = self.dispatch.clone();
        let exited = self.exited.clone();
        self.worker = Some(tokio::spawn(async move {
            while let Some(prompt) = receiver.recv().await {
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                let started = std::time::Instant::now();
                // Serialize only the prompt's write with cancellation, so a
                // cancel cannot run just before the prompt it meant to stop.
                let dispatched = {
                    let _dispatch = dispatch.lock().await;
                    if prompt.generation != generation.load(Ordering::SeqCst) {
                        continue;
                    }
                    let pending = rpc.start_prompt(json!({"sessionId":session,"prompt":[{"type":"text","text":prompt.text}]})).await;
                    let mut current = state.lock();
                    current.active = true;
                    sink.publish_state(
                        if current.approvals.is_empty() {
                            AgentState::Working
                        } else {
                            AgentState::WaitingPermission
                        },
                        None,
                    );
                    pending
                };
                let result = match dispatched {
                    Ok(pending) => pending.wait().await,
                    Err(failure) => Err(failure),
                };
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                {
                    let mut current = state.lock();
                    current.active = false;
                    current.approvals.clear();
                }
                let (data, detail) = match result {
                    Ok(value) => (value, None),
                    Err(failure) => (json!({"error":failure.message}), Some(failure.message)),
                };
                sink.message(AgentMessageBody::System {
                    subtype: "opencode_turn".into(),
                    data,
                })
                .await;
                if detail.is_none() {
                    sink.message(AgentMessageBody::Result {
                        cost_usd: 0.0,
                        duration_ms: started.elapsed().as_millis() as u64,
                        num_turns: 1,
                        session_id: session.clone(),
                    })
                    .await;
                }
                if !exited.load(Ordering::SeqCst) {
                    sink.publish_state(
                        if detail.is_some() {
                            AgentState::Error
                        } else {
                            AgentState::Idle
                        },
                        detail,
                    );
                }
            }
        }));
    }
}
#[async_trait]
impl AgentAdapter for OpenCodeAdapter {
    async fn start(&mut self) -> Result<(), RpcError> {
        if self.rpc.is_some() {
            return Err(error("OpenCode already started"));
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
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            killer();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.exit).await;
            finish(&self.sink, &self.exited, None).await;
            return Err(error("OpenCode ACP pipes unavailable"));
        };
        let secrets = self.secrets.clone();
        let tail = Arc::new(Mutex::new(String::new()));
        let stderr_tail = tail.clone();
        let stderr_secrets = secrets.clone();
        let mut stderr_task = tokio::spawn(async move {
            if let Some(stderr) = child.stderr.take() {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut value = json!(line);
                    stderr_secrets.redact(&mut value);
                    *stderr_tail.lock() = value
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(2000)
                        .collect();
                }
            }
        });
        let (rpc, mut events) = AcpRpc::connect(stdout, stdin, secrets, self.timeout);
        self.rpc = Some(rpc.clone());
        let sink = self.sink.clone();
        let state = self.state.clone();
        let replaying = self.replaying.clone();
        let event_rpc = rpc.clone();
        let mut pump = tokio::spawn(async move {
            let mut stream = OpenCodeStream::default();
            while let Some(incoming) = events.recv().await {
                let frame = match incoming {
                    Incoming::Barrier(ready) => {
                        let _ = ready.send(());
                        continue;
                    }
                    Incoming::Message(frame) => frame,
                };
                let p = &frame["params"];
                if matches!(
                    text(&frame, "method"),
                    "session/update" | "session/request_permission"
                ) {
                    let expected = sink.entry().session_id.lock().clone();
                    if !replaying.load(Ordering::SeqCst)
                        && expected.as_deref() != p["sessionId"].as_str()
                    {
                        if let Some(id) = frame.get("id") {
                            let _ = event_rpc.reject_session(id.clone()).await;
                        }
                        continue;
                    }
                    if replaying.load(Ordering::SeqCst)
                        && text(&frame, "method") == "session/request_permission"
                    {
                        let _ = event_rpc
                            .reply(
                                frame["id"].clone(),
                                json!({"outcome":{"outcome":"cancelled"}}),
                            )
                            .await;
                        continue;
                    }
                }
                match text(&frame, "method") {
                    "session/update" => {
                        if !replaying.load(Ordering::SeqCst) {
                            for body in stream.ingest(p["update"].clone()) {
                                sink.message(body).await;
                            }
                        }
                    }
                    "session/request_permission" if frame.get("id").is_some() => {
                        let id = frame["id"].clone();
                        let options = p["options"].as_array().cloned().unwrap_or_default();
                        let request_id = approval_id(&id);
                        {
                            let mut current = state.lock();
                            current.approvals.insert(
                                request_id.clone(),
                                Approval {
                                    id,
                                    options: options.clone(),
                                },
                            );
                            sink.publish_state(AgentState::WaitingPermission, None);
                        }
                        sink.message(AgentMessageBody::PermissionRequest {
                            request_id,
                            tool_name: text(&p["toolCall"], "title").into(),
                            input: p["toolCall"].clone(),
                            suggestions: options,
                        })
                        .await;
                    }
                    _ => {
                        if let Some(id) = frame.get("id") {
                            if event_rpc.reject_unsupported(id.clone()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        let sink = self.sink.clone();
        let exited = self.exited.clone();
        let stopping = self.stopping.clone();
        let state = self.state.clone();
        self.monitor = Some(tokio::spawn(async move {
            let mut exit = child.exit;
            let code = tokio::select! {code=&mut exit=>{if tokio::time::timeout(Duration::from_secs(2),&mut pump).await.is_err(){pump.abort();}code.ok()},_=&mut pump=>{killer();tokio::time::timeout(Duration::from_secs(2),&mut exit).await.ok().and_then(Result::ok)}};
            rpc.close().await;
            if tokio::time::timeout(Duration::from_millis(500), &mut stderr_task)
                .await
                .is_err()
            {
                stderr_task.abort();
            }
            state.lock().approvals.clear();
            let detail = if stopping.load(Ordering::SeqCst) {
                None
            } else {
                Some(format!("OpenCode exited ({code:?}): {}", tail.lock()))
            };
            finish(&sink, &exited, detail).await;
        }));
        if let Err(failure) = self.initialize().await {
            let _ = self.stop().await;
            return Err(failure);
        }
        self.start_worker();
        if !self.exited.load(Ordering::SeqCst) {
            self.sink.publish_state(AgentState::Idle, None);
        }
        Ok(())
    }
    async fn send(&mut self, text: String) -> Result<(), RpcError> {
        self.rpc()?;
        let sender = self
            .prompts
            .as_ref()
            .ok_or_else(|| error("OpenCode agent has not initialized"))?;
        // Never wait for queue capacity under the lock approvals also need.
        let permit = sender
            .try_reserve()
            .map_err(|_| error("OpenCode prompt queue is full or closed"))?;
        self.sink
            .message(AgentMessageBody::UserText { text: text.clone() })
            .await;
        permit.send(Prompt {
            text,
            generation: self.generation.load(Ordering::SeqCst),
        });
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
                "OpenCode approvals cannot edit tool input",
            ));
        }
        let rpc = self.rpc()?;
        let kind = match decision {
            PermissionDecision::Allow => "allow_once",
            PermissionDecision::AllowForSession => "allow_always",
            PermissionDecision::Deny => "reject_once",
        };
        let (id, option) = {
            let mut current = self.state.lock();
            let approval = current
                .approvals
                .get(&request_id)
                .ok_or_else(|| RpcError::invalid_params("unknown or answered OpenCode approval"))?;
            let option = approval
                .options
                .iter()
                .find(|o| o["kind"] == kind)
                .and_then(|o| o["optionId"].as_str())
                .ok_or_else(|| {
                    RpcError::invalid_params("OpenCode did not offer this permission choice")
                })?
                .to_owned();
            let id = approval.id.clone();
            current.approvals.remove(&request_id);
            self.sink.publish_state(
                if current.approvals.is_empty() {
                    AgentState::Working
                } else {
                    AgentState::WaitingPermission
                },
                None,
            );
            (id, option)
        };
        self.sink
            .message(AgentMessageBody::System {
                subtype: "permission_reply".into(),
                data: json!({"request_id":request_id,"decision":decision}),
            })
            .await;
        rpc.reply(
            id,
            json!({"outcome":{"outcome":"selected","optionId":option}}),
        )
        .await
    }
    async fn interrupt(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let _dispatch = self.dispatch.lock().await;
        self.generation.fetch_add(1, Ordering::SeqCst);
        let approvals: Vec<Approval> = self
            .state
            .lock()
            .approvals
            .drain()
            .map(|(_, a)| a)
            .collect();
        rpc.notify("session/cancel", json!({"sessionId":self.session}))
            .await?;
        for approval in approvals {
            let _ = rpc
                .reply(approval.id, json!({"outcome":{"outcome":"cancelled"}}))
                .await;
        }
        Ok(())
    }
    async fn stop(&mut self) -> Result<(), RpcError> {
        self.stopping.store(true, Ordering::SeqCst);
        self.prompts.take();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        if let Some(rpc) = &self.rpc {
            rpc.close_input().await;
        }
        if let Some(mut monitor) = self.monitor.take() {
            if tokio::time::timeout(Duration::from_secs(2), &mut monitor)
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
        if let Some(rpc) = &self.rpc {
            rpc.close().await;
        }
        finish(&self.sink, &self.exited, None).await;
        Ok(())
    }
}
impl Drop for OpenCodeAdapter {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
        if !self.exited.load(Ordering::SeqCst) {
            if let Some(kill) = &self.killer {
                kill();
            }
        }
    }
}
