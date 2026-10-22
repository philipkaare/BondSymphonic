//! OpenCode ACP lifecycle. No peer command is ever executed on the host.
use super::{
    acp::{error, AcpClient, AcpEvent},
    adapter::AgentAdapter,
    backend::PreparedAgent,
    codex_stream::{redact, text},
    AgentSink,
};
use crate::sandbox::{SandboxCommand, SandboxHandle};
use async_trait::async_trait;
use bondsymphonic_proto::{AgentMessageBody, AgentState, PermissionDecision, RpcError};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
#[derive(Default)]
struct State {
    active: bool,
    epoch: u64,
    queue: VecDeque<String>,
    next_approval: u64,
    approvals: HashMap<String, (Value, Value)>,
    tools: HashMap<String, Value>,
    replay: bool,
}
pub struct OpenCodeAdapter {
    sink: AgentSink,
    handle: Arc<dyn SandboxHandle>,
    prepared: PreparedAgent,
    rpc: Option<Arc<AcpClient>>,
    session: String,
    state: Arc<Mutex<State>>,
    exited: Arc<AtomicBool>,
    killer: Option<Arc<dyn Fn() + Send + Sync>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
    timeout: Duration,
    prompt_timeout: Duration,
    mcp: Vec<Value>,
    secrets: Vec<String>,
    event_delay: Duration,
}
fn sanitize_error(mut e: RpcError, secrets: &[String]) -> RpcError {
    let mut message = json!(e.message);
    redact(&mut message, secrets);
    e.message = message.as_str().unwrap_or_default().into();
    if let Some(data) = e.data.as_mut() {
        redact(data, secrets);
    }
    e
}
async fn record(sink: &AgentSink, body: AgentMessageBody, secrets: &[String]) {
    // Execution retains the raw frame; only its separate presentation copy changes.
    if let Ok(mut data) = serde_json::to_value(body) {
        redact(&mut data, secrets);
        if let Ok(body) = serde_json::from_value(data) {
            sink.message(body).await;
        }
    }
}
async fn finish(sink: &AgentSink, exited: &AtomicBool) {
    if !exited.swap(true, Ordering::SeqCst) {
        let _ = tokio::time::timeout(Duration::from_secs(2), sink.ended()).await;
        sink.publish_state(AgentState::Exited, None);
    }
}
pub fn workspace_relative(root: &Path, path: &str) -> Result<String, RpcError> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(root)
            .map_err(|_| RpcError::invalid_params("path escapes worktree"))?
    } else {
        p
    };
    if rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(RpcError::invalid_params("path escapes worktree"));
    }
    rel.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params("invalid path"))
}
async fn filesystem(root: PathBuf, method: String, p: Value) -> Result<Value, RpcError> {
    tokio::task::spawn_blocking(move || {
        let rel = workspace_relative(&root, text(&p, "path"))?;
        if method == "fs/write_text_file" {
            crate::fs::write_file(&root, &rel, text(&p, "content"))?;
            Ok(json!({}))
        } else {
            let r = crate::fs::read_file(&root, &rel)?;
            if r.encoding != "utf-8" || r.truncated {
                return Err(error("ACP requires complete UTF-8 file"));
            }
            let start = p["line"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
            let limit = p["limit"].as_u64().unwrap_or(u64::MAX) as usize;
            let content = if p.get("line").is_some() || p.get("limit").is_some() {
                r.content
                    .lines()
                    .skip(start)
                    .take(limit)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                r.content
            };
            Ok(json!({"content":content}))
        }
    })
    .await
    .map_err(|_| error("ACP filesystem worker failed"))?
}
impl OpenCodeAdapter {
    pub fn new(
        sink: AgentSink,
        handle: Arc<dyn SandboxHandle>,
        mut prepared: PreparedAgent,
    ) -> Self {
        let mcp = std::mem::take(&mut prepared.mcp_servers);
        Self {
            sink,
            handle,
            prepared,
            rpc: None,
            session: String::new(),
            state: Arc::new(Mutex::new(State::default())),
            exited: Arc::new(AtomicBool::new(false)),
            killer: None,
            monitor: None,
            timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(600),
            mcp,
            secrets: vec![],
            event_delay: Duration::ZERO,
        }
    }
    /// Supplemental resolved ACP MCP declarations; attach here OR in native config.
    pub fn with_mcp_servers(mut self, mcp: Vec<Value>) -> Self {
        self.mcp = mcp;
        self
    }
    /// Test seam for proving output ordering with a delayed transcript pump.
    #[doc(hidden)]
    pub fn with_event_delay_for_tests(mut self, d: Duration) -> Self {
        self.event_delay = d;
        self
    }
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self.prompt_timeout = d;
        self
    }
    fn rpc(&self) -> Result<Arc<AcpClient>, RpcError> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(error("OpenCode exited"));
        }
        self.rpc
            .clone()
            .ok_or_else(|| error("OpenCode not started"))
    }
    async fn initialize(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let init=rpc.call("initialize",json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":false},"clientInfo":{"name":"bondsymphonic","version":env!("CARGO_PKG_VERSION")}})).await?;
        if init["protocolVersion"] != 1 {
            return Err(error("Unsupported ACP protocol version"));
        }
        for server in &self.mcp {
            if let Some(kind) = server["type"].as_str() {
                if matches!(kind, "http" | "sse")
                    && init["agentCapabilities"]["mcpCapabilities"][kind] != true
                {
                    return Err(error("ACP MCP transport unsupported"));
                }
            }
        }
        let mut params = json!({"cwd":self.prepared.cwd,"mcpServers":self.mcp});
        let method = if let Some(id) = &self.prepared.options.resume_session {
            if init["agentCapabilities"]["loadSession"] != true {
                return Err(error("ACP session reload unsupported"));
            }
            params["sessionId"] = json!(id);
            self.state.lock().replay = true;
            "session/load"
        } else {
            "session/new"
        };
        let result = rpc.call(method, params).await?;

        self.session = result["sessionId"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| self.prepared.options.resume_session.clone())
            .ok_or_else(|| error("ACP missing sessionId"))?;
        if self.session.is_empty() {
            return Err(error("ACP empty sessionId"));
        }
        if let Some(model) = self.prepared.options.model.as_deref() {
            let configs = result["configOptions"]
                .as_array()
                .ok_or_else(|| error("ACP missing model configuration"))?;
            let config = configs
                .iter()
                .find(|c| c["category"] == "model" || c["id"] == "model")
                .ok_or_else(|| error("ACP model selector unavailable"))?;
            fn offered(v: &Value, m: &str) -> bool {
                v.as_array().is_some_and(|a| {
                    a.iter()
                        .any(|o| o["value"] == m || offered(&o["options"], m))
                })
            }
            if !offered(&config["options"], model) {
                return Err(error(
                    "Configured provider/model unavailable; no fallback selected",
                ));
            }
            rpc.call(
                "session/set_config_option",
                json!({"sessionId":self.session,"configId":config["id"],"value":model}),
            )
            .await?;
        }
        self.sink.session_id(self.session.clone()).await;
        Ok(())
    }
}
#[async_trait]
impl AgentAdapter for OpenCodeAdapter {
    async fn start(&mut self) -> Result<(), RpcError> {
        if self.rpc.is_some() {
            return Err(error("OpenCode already started"));
        }
        let mut secrets = self
            .prepared
            .env
            .iter()
            .filter(|(k, _)| k.contains("KEY") || k.contains("TOKEN"))
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>();
        if let Some(k) = &self.prepared.options.api_key {
            secrets.push(k.clone());
        }
        for s in &self.mcp {
            if let Some(a) = s["headers"].as_array() {
                for h in a {
                    if let Some(v) = h["value"].as_str() {
                        secrets.push(v.into());
                    }
                }
            }
            if let Some(a) = s["env"].as_array() {
                for e in a {
                    if let Some(v) = e["value"].as_str() {
                        secrets.push(v.into());
                    }
                }
            }
        }
        secrets.retain(|s| !s.is_empty());
        self.secrets = secrets.clone();
        let mut child = self
            .handle
            .spawn(SandboxCommand {
                argv: self.prepared.argv.clone(),
                env: self.prepared.env.clone(),
                cwd: Some(self.prepared.cwd.clone()),
                pty: None,
            })
            .await
            .map_err(|e| sanitize_error(e, &self.secrets))?;
        let killer: Arc<dyn Fn() + Send + Sync> = Arc::from(child.killer);
        self.killer = Some(killer.clone());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| error("ACP stdout unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| error("ACP stdin unavailable"))?;
        // Drain stderr in fixed chunks without accumulating unbounded log lines.
        let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_capture = stderr_tail.clone();
        let diagnostic_secrets = secrets.clone();
        let stderr = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            if let Some(mut r) = child.stderr.take() {
                let mut buf = [0u8; 4096];
                loop {
                    match r.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut tail = stderr_capture.lock();
                            tail.extend_from_slice(&buf[..n]);
                            let excess = tail.len().saturating_sub(4096);
                            if excess > 0 {
                                tail.drain(..excess);
                            }
                        }
                    }
                }
            }
        });
        let (rpc, mut events) = AcpClient::connect(stdout, stdin, secrets, self.timeout);
        self.rpc = Some(rpc.clone());
        let sink = self.sink.clone();
        let state = self.state.clone();
        let root = self.prepared.cwd.clone();
        let peer = rpc.clone();
        let event_secrets = self.secrets.clone();
        let event_delay = self.event_delay;
        let mut pump = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let frame = match event {
                    AcpEvent::Frame(frame) => frame,
                    AcpEvent::Response {
                        method,
                        answer,
                        complete,
                    } => {
                        if method == "session/load" {
                            state.lock().replay = false;
                        }
                        let _ = complete.send(answer);
                        continue;
                    }
                };
                if !event_delay.is_zero() {
                    tokio::time::sleep(event_delay).await;
                }
                let p = &frame["params"];
                let method = text(&frame, "method");
                if method == "session/request_permission" {
                    let id = frame["id"].clone();
                    let key = {
                        let mut s = state.lock();
                        s.next_approval = s.next_approval.wrapping_add(1);
                        format!("acp:permission:{}", s.next_approval)
                    };
                    let overflow = {
                        let mut s = state.lock();
                        if s.approvals.len() >= 64 {
                            true
                        } else {
                            s.approvals
                                .insert(key.clone(), (id.clone(), p["options"].clone()));
                            false
                        }
                    };
                    if overflow {
                        let _ = peer
                            .reply(id, json!({"outcome":{"outcome":"cancelled"}}))
                            .await;
                        continue;
                    }
                    sink.publish_state(AgentState::WaitingPermission, None);
                    record(
                        &sink,
                        AgentMessageBody::PermissionRequest {
                            request_id: key,
                            tool_name: text(&p["toolCall"], "title").into(),
                            input: p["toolCall"].clone(),
                            suggestions: p["options"].as_array().cloned().unwrap_or_default(),
                        },
                        &event_secrets,
                    )
                    .await;
                    continue;
                }
                if let Some(id) = frame.get("id") {
                    if matches!(method, "fs/read_text_file" | "fs/write_text_file") {
                        match filesystem(root.clone(), method.into(), p.clone()).await {
                            Ok(r) => {
                                let _ = peer.reply(id.clone(), r).await;
                            }
                            Err(_) => {
                                let _ = peer
                                    .reject(id.clone(), -32602, "Workspace file request rejected")
                                    .await;
                            }
                        }
                    } else {
                        let _ = peer
                            .reject(
                                id.clone(),
                                -32601,
                                "Unsupported ACP peer method (terminal disabled)",
                            )
                            .await;
                    }
                    continue;
                }
                if method != "session/update" {
                    continue;
                }
                if state.lock().replay {
                    continue;
                }
                let u = &p["update"];
                let body = match text(u, "sessionUpdate") {
                    "agent_message_chunk" => Some(AgentMessageBody::AssistantDelta {
                        text: text(&u["content"], "text").into(),
                    }),
                    "tool_call" | "tool_call_update" => {
                        let id = text(u, "toolCallId").to_owned();
                        let (first, snapshot) = {
                            let mut s = state.lock();
                            let first = !s.tools.contains_key(&id);
                            if first && s.tools.len() >= 64 {
                                s.tools.clear();
                            }
                            let tool = s.tools.entry(id.clone()).or_insert_with(|| json!({}));
                            if let Some(fields) = u.as_object() {
                                for (k, v) in fields {
                                    tool[k] = v.clone();
                                }
                            }
                            (first, tool.clone())
                        };
                        if first {
                            record(
                                &sink,
                                AgentMessageBody::ToolUse {
                                    id: id.clone(),
                                    name: text(&snapshot, "title").into(),
                                    input: snapshot.clone(),
                                },
                                &event_secrets,
                            )
                            .await;
                        }
                        if matches!(text(&snapshot, "status"), "completed" | "failed") {
                            // Redact structured fields before JSON escaping turns them into text.
                            let mut presentation = snapshot.clone();
                            redact(&mut presentation, &event_secrets);
                            Some(AgentMessageBody::ToolResult {
                                id,
                                output: presentation.to_string(),
                                is_error: snapshot["status"] == "failed",
                            })
                        } else {
                            None
                        }
                    }
                    _ => Some(AgentMessageBody::System {
                        subtype: "acp_update".into(),
                        data: u.clone(),
                    }),
                };
                if let Some(b) = body {
                    record(&sink, b, &event_secrets).await;
                }
            }
        });
        let sink = self.sink.clone();
        let exited = self.exited.clone();
        self.monitor = Some(tokio::spawn(async move {
            let mut exit = child.exit;
            tokio::select! {_= &mut exit=>{if tokio::time::timeout(Duration::from_secs(2),&mut pump).await.is_err(){pump.abort();}},_= &mut pump=>{killer();let _=tokio::time::timeout(Duration::from_secs(2),&mut exit).await;}}
            rpc.close().await;
            stderr.abort();
            let mut detail = json!(String::from_utf8_lossy(&stderr_tail.lock()).to_string());
            redact(&mut detail, &diagnostic_secrets);
            if !exited.load(Ordering::SeqCst) && detail.as_str().is_some_and(|s| !s.is_empty()) {
                sink.message(AgentMessageBody::System {
                    subtype: "acp_process_stderr".into(),
                    data: detail,
                })
                .await;
            }
            finish(&sink, &exited).await;
        }));
        if let Err(e) = self.initialize().await {
            let _ = self.stop().await;
            return Err(sanitize_error(e, &self.secrets));
        }
        Ok(())
    }
    async fn send(&mut self, text: String) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        // The manager serializes adapter access but does not queue ACP turns.
        let launch = {
            let mut s = self.state.lock();
            if s.queue.len() >= 16 {
                return Err(error("OpenCode prompt queue full"));
            }
            if s.active {
                s.queue.push_back(text.clone());
                false
            } else {
                s.active = true;
                s.epoch = s.epoch.wrapping_add(1);
                s.tools.clear();
                true
            }
        };
        let mut recorded = json!(text.clone());
        redact(&mut recorded, &self.secrets);
        self.sink
            .message(AgentMessageBody::UserText {
                text: recorded.as_str().unwrap_or_default().into(),
            })
            .await;
        if !launch {
            return Ok(());
        }
        self.sink.publish_state(AgentState::Working, None);
        let sink = self.sink.clone();
        let state = self.state.clone();
        let prompt_timeout = self.prompt_timeout;
        let killer = self.killer.clone();
        let secrets = self.secrets.clone();
        let session = self.session.clone();
        let exited = self.exited.clone();
        tokio::spawn(async move {
            let mut next = text;
            loop {
                let answer = rpc
                    .call_with_timeout(
                        "session/prompt",
                        json!({"sessionId":session,"prompt":[{"type":"text","text":next}]}),
                        prompt_timeout,
                    )
                    .await;
                if exited.load(Ordering::SeqCst) {
                    state.lock().active = false;
                    return;
                }
                match answer {
                    Ok(r) => {
                        record(
                            &sink,
                            AgentMessageBody::System {
                                subtype: "acp_stop_reason".into(),
                                data: r,
                            },
                            &secrets,
                        )
                        .await;
                        record(
                            &sink,
                            AgentMessageBody::Result {
                                cost_usd: 0.0,
                                duration_ms: 0,
                                num_turns: 1,
                                session_id: session.clone(),
                            },
                            &secrets,
                        )
                        .await;
                    }
                    Err(e) => {
                        {
                            let mut s = state.lock();
                            s.active = false;
                            s.queue.clear();
                        }
                        sink.publish_state(
                            AgentState::Error,
                            Some(sanitize_error(e, &secrets).message),
                        );
                        rpc.close().await;
                        if let Some(k) = killer {
                            k();
                        }
                        return;
                    }
                }
                if exited.load(Ordering::SeqCst) {
                    state.lock().active = false;
                    return;
                }
                let queued = {
                    let mut s = state.lock();
                    s.tools.clear();
                    let q = s.queue.pop_front();
                    if q.is_none() {
                        s.active = false;
                    } else {
                        s.epoch = s.epoch.wrapping_add(1);
                    }
                    q
                };
                if let Some(q) = queued {
                    next = q;
                } else {
                    sink.publish_state(AgentState::Idle, None);
                    return;
                }
            }
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
                "ACP cannot replace approval input",
            ));
        }
        let rpc = self.rpc()?;
        let (id, options) = self
            .state
            .lock()
            .approvals
            .get(&request_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("Unknown ACP permission"))?;
        let kind = match decision {
            PermissionDecision::Allow => "allow_once",
            PermissionDecision::AllowForSession => "allow_always",
            PermissionDecision::Deny => "reject_once",
        };
        let selected = options
            .as_array()
            .and_then(|a| a.iter().find(|o| o["kind"] == kind));
        let outcome = if let Some(option) = selected {
            json!({"outcome":"selected","optionId":option["optionId"]})
        } else if matches!(decision, PermissionDecision::Deny) {
            json!({"outcome":"cancelled"})
        } else {
            return Err(RpcError::invalid_params(
                "Offered ACP permission option unavailable",
            ));
        };
        rpc.reply(id, json!({"outcome":outcome})).await?;
        let waiting = {
            let mut s = self.state.lock();
            s.approvals.remove(&request_id);
            !s.approvals.is_empty()
        };
        self.sink.publish_state(
            if waiting {
                AgentState::WaitingPermission
            } else {
                AgentState::Working
            },
            None,
        );
        Ok(())
    }
    async fn interrupt(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        self.state.lock().queue.clear();
        let approvals = self
            .state
            .lock()
            .approvals
            .drain()
            .map(|(_, a)| a.0)
            .collect::<Vec<_>>();
        for id in approvals {
            rpc.reply(id, json!({"outcome":{"outcome":"cancelled"}}))
                .await?;
        }
        rpc.notify("session/cancel", json!({"sessionId":self.session}))
            .await?;
        let state = self.state.clone();
        let epoch = state.lock().epoch;
        let killer = self.killer.clone();
        let wait = self.timeout.min(Duration::from_secs(3));
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let still_cancelled_turn = {
                let s = state.lock();
                s.active && s.epoch == epoch
            };
            if still_cancelled_turn {
                if let Some(k) = killer {
                    k();
                }
            }
        });
        Ok(())
    }
    async fn stop(&mut self) -> Result<(), RpcError> {
        if let Some(rpc) = &self.rpc {
            let _ = rpc
                .notify("session/cancel", json!({"sessionId":self.session}))
                .await;
            rpc.close().await;
        }
        if let Some(k) = &self.killer {
            k();
        }
        if let Some(mut m) = self.monitor.take() {
            if tokio::time::timeout(Duration::from_secs(3), &mut m)
                .await
                .is_err()
            {
                m.abort();
            }
        }
        finish(&self.sink, &self.exited).await;
        Ok(())
    }
}
impl Drop for OpenCodeAdapter {
    fn drop(&mut self) {
        if let Some(k) = &self.killer {
            k();
        }
    }
}
