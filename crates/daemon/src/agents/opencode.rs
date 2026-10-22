//! One OpenCode ACP process per agent; prompts run outside the manager lock.
//!
//! What OpenCode 1.14.48 does that shapes this (recorded in
//! `tests/fixtures/opencode/probe-1.14.48`):
//!
//! * `session/cancel` ends the prompt with an ordinary `end_turn`, and text may
//!   still stream in between, so a cancelled turn is known only by our having
//!   cancelled it.
//! * A provider failure is never a JSON-RPC error. A rejected key, an unusable
//!   model or a refused tier ends the prompt with an empty `end_turn`; a 500 or
//!   a refused connection is retried for ever and the prompt never answers.
//!   The cause appears only in OpenCode's log, which is printed to stderr.
//!
//! Messages sent while a turn runs are queued here, not by OpenCode, and are
//! shown as queued until they are sent. An interrupt keeps them -- as Claude
//! Code does with input that arrives during a turn -- and they go out in order
//! once the interrupted turn has ended. An interrupt that finds messages an
//! earlier interrupt already kept discards them, so pressing it twice stops
//! everything; stopping the agent discards them too. Codex differs by nature:
//! what is sent during its turn is steered into that turn and ends with it.
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
use bondsymphonic_proto::{
    AgentMessageBody, AgentNotice, AgentState, DequeueOutcome, DequeuedPrompt, NoticeAction,
    NoticeKind, PermissionDecision, QueuedPrompt, QueuedPromptAction, RpcError, TURN_USAGE_KEY,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Notify,
    task::JoinHandle,
};

/// How long a turn may go without a frame from OpenCode before the user is
/// told. OpenCode retries a failing provider without a word, but a large local
/// model can also be silent this long while it loads, so the turn is flagged
/// rather than ended: only the user knows which it is.
const STALL_AFTER: Duration = Duration::from_secs(60);
/// How long an empty turn waits for its cause on stderr, a separate pipe that
/// can trail the prompt's answer.
const LOG_GRACE: Duration = Duration::from_millis(250);
const QUEUE_LIMIT: usize = 64;
/// How many of OpenCode's log lines a notice carries, and how long each may
/// be. At `--log-level WARN` a turn rarely prints more than a few.
const LOG_LINES: usize = 100;
const LOG_LINE_CHARS: usize = 500;

struct Approval {
    id: Value,
    options: Vec<Value>,
}
struct TurnState {
    active: bool,
    /// Set when we cancel the active turn; OpenCode does not say so itself.
    cancelled: bool,
    approvals: HashMap<String, Approval>,
    /// The last frame OpenCode sent this session, or the user's last answer.
    activity: Instant,
    stalled: bool,
    /// The last provider failure OpenCode logged during this turn.
    provider_error: Option<String>,
}
impl Default for TurnState {
    fn default() -> Self {
        Self {
            active: false,
            cancelled: false,
            approvals: HashMap::new(),
            activity: Instant::now(),
            stalled: false,
            provider_error: None,
        }
    }
}
/// A message waiting for the turn in flight to end.
struct Queued {
    id: String,
    text: String,
    /// Whether the transcript shows it as queued, so that sending it is
    /// announced. A message sent to an idle agent goes straight out.
    announced: bool,
    /// Whether an interrupt has already kept it; the next one discards it.
    kept: bool,
}
#[derive(Default)]
struct Prompts {
    queue: Mutex<VecDeque<Queued>>,
    ready: Notify,
    next_id: AtomicU64,
}
type Stream = Arc<tokio::sync::Mutex<OpenCodeStream>>;
/// OpenCode's last log lines, newest last, reduced and redacted.
type Log = Arc<Mutex<VecDeque<String>>>;
pub struct OpenCodeAdapter {
    sink: AgentSink,
    handle: Arc<dyn SandboxHandle>,
    prepared: PreparedAgent,
    rpc: Option<Arc<AcpRpc>>,
    session: String,
    state: Arc<Mutex<TurnState>>,
    /// Held while transcript rows are written, so a row from one task never
    /// lands inside text another is streaming.
    stream: Stream,
    dispatch: Arc<tokio::sync::Mutex<()>>,
    replaying: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    prompts: Option<Arc<Prompts>>,
    killer: Option<Arc<dyn Fn() + Send + Sync>>,
    monitor: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
    timeout: Duration,
    stall_after: Duration,
    secrets: Arc<AcpSecrets>,
    log: Log,
}
async fn finish(sink: &AgentSink, exited: &AtomicBool, detail: Option<String>) {
    if exited.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), sink.ended()).await;
    sink.publish_state(AgentState::Exited, detail);
}
fn notice(
    kind: NoticeKind,
    text: impl Into<String>,
    actions: Vec<NoticeAction>,
    log: &Log,
) -> AgentMessageBody {
    let log = if actions.contains(&NoticeAction::ShowLog) {
        log.lock().iter().cloned().collect()
    } else {
        vec![]
    };
    AgentNotice {
        text: text.into(),
        kind,
        actions,
        log,
    }
    .into_message()
}
/// What OpenCode's empty answer usually means is a configuration problem.
fn empty_turn_notice(cause: Option<&str>, log: &Log) -> AgentMessageBody {
    notice(
        NoticeKind::EmptyTurn,
        empty_turn_text(cause),
        vec![NoticeAction::OpenConfigFolder, NoticeAction::ShowLog],
        log,
    )
}
fn stall_notice(silent: Duration, cause: Option<&str>, log: &Log) -> AgentMessageBody {
    notice(
        NoticeKind::Stall,
        stall_text(silent, cause),
        vec![
            NoticeAction::Interrupt,
            NoticeAction::ShowLog,
            NoticeAction::OpenConfigFolder,
        ],
        log,
    )
}
/// The rows saying `discarded` will never be sent: one per message the
/// transcript shows as queued, and one notice naming them all.
fn discarded_rows(why: &str, discarded: &[Queued], log: &Log) -> Vec<AgentMessageBody> {
    if discarded.is_empty() {
        return vec![];
    }
    let mut rows: Vec<AgentMessageBody> = discarded
        .iter()
        .filter(|queued| queued.announced)
        .map(|queued| {
            DequeuedPrompt {
                id: queued.id.clone(),
                outcome: DequeueOutcome::Discarded,
            }
            .into_message()
        })
        .collect();
    let texts: Vec<String> = discarded.iter().map(|q| q.text.clone()).collect();
    rows.push(notice(
        NoticeKind::QueueDiscarded,
        discarded_text(why, &texts),
        vec![],
        log,
    ));
    rows
}
/// Writes `bodies` after finalising any text still streaming. Nothing at all
/// when there is nothing to write: finalising would split a bubble in two.
async fn put(sink: &AgentSink, stream: &Stream, bodies: Vec<AgentMessageBody>) {
    if bodies.is_empty() {
        return;
    }
    let mut stream = stream.lock().await;
    for body in stream.flush().into_iter().chain(bodies) {
        sink.message(body).await;
    }
}
fn no_tokens(usage: &Value) -> bool {
    usage.as_object().is_none_or(|fields| {
        fields
            .values()
            .all(|value| value.as_f64().is_none_or(|n| n == 0.0))
    })
}
fn empty_turn_text(cause: Option<&str>) -> String {
    let cause = cause
        .map(|cause| format!(" OpenCode logged: {cause}."))
        .unwrap_or_default();
    format!("The model returned nothing.{cause} Check the provider, model and sign-in in your OpenCode configuration (opencode.json, `opencode auth login`); OpenCode's log has the details.")
}
fn stall_text(silent: Duration, cause: Option<&str>) -> String {
    let cause = cause
        .map(|cause| format!(" The last error it logged: {cause}."))
        .unwrap_or_default();
    format!("No response from the model for {} s. OpenCode keeps retrying a provider that fails, without reporting it.{cause} A large local model may still be loading; interrupt to stop waiting.", silent.as_secs().max(1))
}
fn discarded_text(why: &str, discarded: &[String]) -> String {
    let quoted = discarded
        .iter()
        .map(|text| {
            let short: String = text.chars().take(60).collect();
            let more = if short.len() < text.len() { "…" } else { "" };
            format!("“{short}{more}”")
        })
        .collect::<Vec<_>>()
        .join(", ");
    match discarded.len() {
        1 => format!("{why}: 1 queued message was not sent: {quoted}"),
        n => format!("{why}: {n} queued messages were not sent: {quoted}"),
    }
}

/// The cause in one of OpenCode's `ERROR` log lines about a model request,
/// without the request body the line also carries. `None` for anything else.
pub fn provider_error(line: &str) -> Option<String> {
    let rest = line.strip_prefix("ERROR")?;
    let (head, error) = rest.split_once(" error=")?;
    let field = |key: &str| {
        head.split_whitespace()
            .find_map(|word| word.strip_prefix(key))
    };
    // Title generation fails for the same reason, after the turn's own call.
    if !matches!(field("service="), Some("llm" | "session.processor"))
        || field("agent=") == Some("title")
    {
        return None;
    }
    let cause = if error.starts_with('{') {
        let value = serde_json::Deserializer::from_str(error)
            .into_iter::<Value>()
            .next()?
            .ok()?;
        describe(&value["error"])
    } else {
        error
            .split(" stack=")
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let model = match (field("providerID="), field("modelID=")) {
        (Some(provider), Some(model)) => format!("{provider}/{model}: "),
        _ => String::new(),
    };
    Some(format!("{model}{cause}").chars().take(400).collect())
}
fn describe(error: &Value) -> String {
    // A retry error wraps every attempt; the last says why it gave up.
    if let Some(last) = error["errors"].as_array().and_then(|e| e.last()) {
        return describe(last);
    }
    let mut parts = vec![];
    if let Some(status) = error["statusCode"].as_u64() {
        parts.push(format!("HTTP {status}"));
    }
    if let Some(code) = error["cause"]["code"].as_str() {
        parts.push(code.to_owned());
    }
    if let Some(message) = error["data"]["error"]["message"]
        .as_str()
        .or(error["message"].as_str())
    {
        parts.push(message.to_owned());
    }
    if let Some(url) = error["url"].as_str() {
        parts.push(format!("from {url}"));
    }
    if parts.is_empty() {
        parts.push(text(error, "name").to_owned());
    }
    parts.join(" ")
}

/// Waits for a prompt's answer, telling the user once per silence when
/// OpenCode has gone quiet while nobody is being asked anything.
async fn wait_reporting_stalls(
    answer: impl std::future::Future<Output = Result<Value, RpcError>>,
    state: &Mutex<TurnState>,
    sink: &AgentSink,
    stream: &Stream,
    log: &Log,
    stall_after: Duration,
) -> Result<Value, RpcError> {
    tokio::pin!(answer);
    let check = (stall_after / 4).max(Duration::from_millis(10));
    loop {
        tokio::select! {
            answer = &mut answer => return answer,
            _ = tokio::time::sleep(check) => {
                let stalled = {
                    let mut current = state.lock();
                    let silent = current.activity.elapsed();
                    let stalled = current.approvals.is_empty() && !current.stalled && silent >= stall_after;
                    current.stalled |= stalled;
                    stalled.then(|| (silent, current.provider_error.clone()))
                };
                if let Some((silent, cause)) = stalled {
                    put(sink, stream, vec![stall_notice(silent, cause.as_deref(), log)]).await;
                }
            }
        }
    }
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
            stream: Stream::default(),
            dispatch: Arc::new(tokio::sync::Mutex::new(())),
            replaying: Arc::new(AtomicBool::new(true)),
            exited: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            prompts: None,
            killer: None,
            monitor: None,
            worker: None,
            timeout: Duration::from_secs(30),
            stall_after: STALL_AFTER,
            secrets,
            log: Log::default(),
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn with_stall_after(mut self, stall_after: Duration) -> Self {
        self.stall_after = stall_after;
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
    /// Cancels the turn in flight, if any, answering its open approvals as
    /// cancelled. Called with the dispatch lock held.
    async fn cancel_turn(&self, rpc: &AcpRpc) -> Result<(), RpcError> {
        let approvals: Vec<Approval> = {
            let mut current = self.state.lock();
            current.cancelled |= current.active;
            let approvals: Vec<Approval> = current.approvals.drain().map(|(_, a)| a).collect();
            if current.active && !approvals.is_empty() {
                self.sink.publish_state(AgentState::Working, None);
            }
            approvals
        };
        rpc.notify("session/cancel", json!({"sessionId":self.session}))
            .await?;
        for approval in approvals {
            let _ = rpc
                .reply(approval.id, json!({"outcome":{"outcome":"cancelled"}}))
                .await;
        }
        Ok(())
    }
    fn start_worker(&mut self) {
        let prompts = Arc::new(Prompts::default());
        self.prompts = Some(prompts.clone());
        let rpc = self.rpc.as_ref().unwrap().clone();
        let sink = self.sink.clone();
        let session = self.session.clone();
        let state = self.state.clone();
        let stream = self.stream.clone();
        let dispatch = self.dispatch.clone();
        let exited = self.exited.clone();
        let stall_after = self.stall_after;
        let log = self.log.clone();
        self.worker = Some(tokio::spawn(async move {
            loop {
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                let started = Instant::now();
                // Take and write a prompt under the lock cancellation holds, so
                // an interrupt either stops this prompt or finds it queued.
                let dispatched = {
                    let _dispatch = dispatch.lock().await;
                    let Some(queued) = prompts.queue.lock().pop_front() else {
                        drop(_dispatch);
                        prompts.ready.notified().await;
                        continue;
                    };
                    if queued.announced {
                        // Only now is it the user's turn in the conversation.
                        put(
                            &sink,
                            &stream,
                            vec![
                                DequeuedPrompt {
                                    id: queued.id,
                                    outcome: DequeueOutcome::Sent,
                                }
                                .into_message(),
                                AgentMessageBody::UserText {
                                    text: queued.text.clone(),
                                },
                            ],
                        )
                        .await;
                    }
                    let text = queued.text;
                    {
                        // Active before the write: the turn's first frame can
                        // follow it at once.
                        let mut current = state.lock();
                        current.active = true;
                        current.cancelled = false;
                        current.stalled = false;
                        current.provider_error = None;
                        current.activity = Instant::now();
                        sink.publish_state(
                            if current.approvals.is_empty() {
                                AgentState::Working
                            } else {
                                AgentState::WaitingPermission
                            },
                            None,
                        );
                    }
                    rpc.start_prompt(
                        json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
                    )
                    .await
                };
                let result = match dispatched {
                    Ok(pending) => {
                        wait_reporting_stalls(
                            pending.wait(),
                            &state,
                            &sink,
                            &stream,
                            &log,
                            stall_after,
                        )
                        .await
                    }
                    Err(failure) => Err(failure),
                };
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                let (produced, usage) = {
                    // Under the stream lock, so a frame after the answer is
                    // either part of this turn or seen as late.
                    let mut stream = stream.lock().await;
                    state.lock().active = false;
                    let usage = result
                        .as_ref()
                        .ok()
                        .and_then(|answer| stream.turn_usage(&answer["usage"]));
                    let (bodies, produced) = stream.finish_turn();
                    for body in bodies {
                        sink.message(body).await;
                    }
                    (produced, usage)
                };
                let (cancelled, mut cause) = {
                    let mut current = state.lock();
                    current.approvals.clear();
                    (current.cancelled, current.provider_error.take())
                };
                let (mut data, detail) = match result {
                    Ok(value) => (value, None),
                    Err(failure) => (json!({"error":failure.message}), Some(failure.message)),
                };
                if detail.is_none() && cancelled {
                    data["reportedStopReason"] = data["stopReason"].clone();
                    data["stopReason"] = json!("cancelled");
                } else if detail.is_none()
                    && !produced
                    && data["stopReason"] == "end_turn"
                    && no_tokens(&data["usage"])
                {
                    if cause.is_none() {
                        tokio::time::sleep(LOG_GRACE).await;
                        cause = state.lock().provider_error.take();
                    }
                    sink.message(empty_turn_notice(cause.as_deref(), &log))
                        .await;
                }
                if let Some(usage) = usage {
                    data[TURN_USAGE_KEY] = json!(usage);
                }
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
        let mut argv = self.prepared.argv.clone();
        // A provider failure reaches us only as a log line. WARN keeps those
        // and leaves out the per-request INFO lines.
        if argv.iter().any(|arg| arg == "acp") && !argv.iter().any(|arg| arg == "--print-logs") {
            argv.extend(["--print-logs", "--log-level", "WARN"].map(String::from));
        }
        let mut child = self
            .handle
            .spawn(SandboxCommand {
                argv,
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
        let stderr_state = self.state.clone();
        let stderr_log = self.log.clone();
        let mut stderr_task = tokio::spawn(async move {
            if let Some(stderr) = child.stderr.take() {
                // Lossy: a reader that stopped at invalid UTF-8 would leave
                // OpenCode blocked on a full pipe.
                let mut reader = BufReader::new(stderr);
                let mut bytes = Vec::new();
                while matches!(reader.read_until(b'\n', &mut bytes).await, Ok(n) if n > 0) {
                    let line = String::from_utf8_lossy(&bytes).trim_end().to_owned();
                    bytes.clear();
                    // A stack trace's frames say nothing to the user.
                    if line.starts_with(char::is_whitespace) && line.trim_start().starts_with("at ")
                    {
                        continue;
                    }
                    // Only the cause: the whole line repeats the request,
                    // conversation included.
                    let cause = provider_error(&line);
                    let mut value = json!(cause.as_deref().unwrap_or(&line));
                    stderr_secrets.redact(&mut value);
                    let shown: String = value
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(2000)
                        .collect();
                    if cause.is_some() {
                        stderr_state.lock().provider_error = Some(shown.clone());
                    }
                    if !shown.trim().is_empty() {
                        let mut log = stderr_log.lock();
                        if log.len() == LOG_LINES {
                            log.pop_front();
                        }
                        log.push_back(shown.chars().take(LOG_LINE_CHARS).collect());
                    }
                    *stderr_tail.lock() = shown;
                }
            }
        });
        let (rpc, mut events) = AcpRpc::connect(stdout, stdin, secrets, self.timeout);
        self.rpc = Some(rpc.clone());
        let sink = self.sink.clone();
        let state = self.state.clone();
        let stream = self.stream.clone();
        let replaying = self.replaying.clone();
        let event_rpc = rpc.clone();
        let mut pump = tokio::spawn(async move {
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
                    // Commands are session metadata: retain replay and idle
                    // updates, but apply the session guard above first.
                    if text(&frame, "method") == "session/update"
                        && text(&p["update"], "sessionUpdate") == "available_commands_update"
                    {
                        sink.message(super::opencode_stream::command_metadata(&p["update"]))
                            .await;
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
                    if !replaying.load(Ordering::SeqCst) {
                        let mut current = state.lock();
                        current.activity = Instant::now();
                        current.stalled = false;
                    }
                }
                match text(&frame, "method") {
                    "session/update" => {
                        if replaying.load(Ordering::SeqCst) {
                            continue;
                        }
                        let mut stream = stream.lock().await;
                        // OpenCode streams on briefly after a cancel; what
                        // comes after the answer belongs to a finished turn.
                        if !state.lock().active {
                            continue;
                        }
                        for body in stream.ingest(p["update"].clone()) {
                            sink.message(body).await;
                        }
                    }
                    "session/request_permission" if frame.get("id").is_some() => {
                        let id = frame["id"].clone();
                        let options = p["options"].as_array().cloned().unwrap_or_default();
                        let request_id = approval_id(&id);
                        let mut stream = stream.lock().await;
                        let (tool_name, input) = stream.permission(&p["toolCall"]);
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
                        for body in stream.flush() {
                            sink.message(body).await;
                        }
                        sink.message(AgentMessageBody::PermissionRequest {
                            request_id,
                            tool_name,
                            input,
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
        let prompts = self
            .prompts
            .clone()
            .ok_or_else(|| error("OpenCode agent has not initialized"))?;
        if prompts.queue.lock().len() >= QUEUE_LIMIT {
            return Err(error("OpenCode prompt queue is full"));
        }
        // Under the lock the worker dispatches under, so "will this wait?"
        // cannot change before the message is queued.
        let _dispatch = self.dispatch.lock().await;
        let waits = self.state.lock().active || !prompts.queue.lock().is_empty();
        let id = format!("q{}", prompts.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        // After the text streaming now, not inside its bubble.
        let row = if waits {
            QueuedPrompt {
                id: id.clone(),
                text: text.clone(),
            }
            .into_message()
        } else {
            AgentMessageBody::UserText { text: text.clone() }
        };
        put(&self.sink, &self.stream, vec![row]).await;
        prompts.queue.lock().push_back(Queued {
            id,
            text,
            announced: waits,
            kept: false,
        });
        prompts.ready.notify_one();
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
            // Waiting on the user was not OpenCode's silence.
            current.activity = Instant::now();
            current.stalled = false;
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
        put(
            &self.sink,
            &self.stream,
            vec![AgentMessageBody::System {
                subtype: "permission_reply".into(),
                data: json!({"request_id":request_id,"decision":decision}),
            }],
        )
        .await;
        rpc.reply(
            id,
            json!({"outcome":{"outcome":"selected","optionId":option}}),
        )
        .await
    }
    /// Cancels the turn in flight. The messages queued behind it are kept and
    /// sent once it has ended, except those an earlier interrupt already
    /// kept, which are discarded, saying so. See the module comment.
    async fn interrupt(&mut self) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let _dispatch = self.dispatch.lock().await;
        let discarded: Vec<Queued> = match &self.prompts {
            Some(prompts) => {
                let mut queue = prompts.queue.lock();
                let (discarded, kept): (Vec<Queued>, Vec<Queued>) =
                    queue.drain(..).partition(|queued| queued.kept);
                queue.extend(kept.into_iter().map(|queued| Queued {
                    kept: true,
                    ..queued
                }));
                discarded
            }
            None => vec![],
        };
        self.cancel_turn(&rpc).await?;
        put(
            &self.sink,
            &self.stream,
            discarded_rows("Interrupted again", &discarded, &self.log),
        )
        .await;
        Ok(())
    }
    async fn queued_prompt(
        &mut self,
        prompt_id: String,
        action: QueuedPromptAction,
    ) -> Result<(), RpcError> {
        let rpc = self.rpc()?;
        let prompts = self
            .prompts
            .clone()
            .ok_or_else(|| error("OpenCode agent has not initialized"))?;
        let _dispatch = self.dispatch.lock().await;
        let queued = {
            let mut queue = prompts.queue.lock();
            let at = queue
                .iter()
                .position(|queued| queued.id == prompt_id)
                .ok_or_else(RpcError::queued_prompt_settled)?;
            queue.remove(at).expect("position is in range")
        };
        match action {
            QueuedPromptAction::Discard => {
                put(
                    &self.sink,
                    &self.stream,
                    vec![DequeuedPrompt {
                        id: queued.id,
                        outcome: DequeueOutcome::Discarded,
                    }
                    .into_message()],
                )
                .await;
            }
            QueuedPromptAction::SendNow => {
                // Ahead of everything else, and not discarded by the next
                // interrupt: the user has just asked for it.
                prompts.queue.lock().push_front(Queued {
                    kept: false,
                    ..queued
                });
                self.cancel_turn(&rpc).await?;
                prompts.ready.notify_one();
            }
        }
        Ok(())
    }
    async fn stop(&mut self) -> Result<(), RpcError> {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(prompts) = self.prompts.take() {
            let _dispatch = self.dispatch.lock().await;
            let discarded: Vec<Queued> = prompts.queue.lock().drain(..).collect();
            if !self.exited.load(Ordering::SeqCst) {
                put(
                    &self.sink,
                    &self.stream,
                    discarded_rows("Stopped", &discarded, &self.log),
                )
                .await;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(scenario: &str) -> Vec<String> {
        let path = format!(
            "{}/tests/fixtures/opencode/probe-1.14.48/frames/{scenario}.stderr.txt",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(provider_error)
            .collect()
    }

    /// The causes OpenCode 1.14.48 logged for each recorded provider failure,
    /// reduced to one line without the request it repeats.
    #[test]
    fn recorded_provider_failures_are_reduced_to_their_cause() {
        assert_eq!(
            logged("G401"),
            [
                "mock/m1: HTTP 401 Incorrect API key provided from http://127.0.0.1:18080/v1/chat/completions",
                "Incorrect API key provided"
            ]
        );
        assert_eq!(
            logged("B").last().unwrap(),
            "OpenCode 1.18.0 or newer is required to use the free tier"
        );
        assert!(logged("G500").iter().all(|cause| cause
            == "mock/m1: HTTP 500 Incorrect API key provided from http://127.0.0.1:18080/v1/chat/completions"));
        assert!(logged("Grefused").iter().all(|cause| cause
            == "mock/m1: ConnectionRefused from http://127.0.0.1:9/v1/chat/completions"));
        assert!(logged("Grefused")
            .iter()
            .all(|cause| !cause.contains("You are")));
    }

    #[test]
    fn other_lines_are_not_provider_errors() {
        for line in [
            "INFO  2026-10-20T13:37:49 +17ms service=session.processor process",
            "ERROR 2026-10-20T13:34:16 +23ms service=mcp key=catsrv error=MCP error -32601: Method not found local mcp startup failed",
            "WARN  2026-10-20T13:38:53 +27582ms service=snapshot exitCode=128 stderr=fatal: gc is already running",
            "Error handling request {",
        ] {
            assert_eq!(provider_error(line), None, "{line}");
        }
    }

    #[test]
    fn usage_without_tokens_is_empty() {
        assert!(no_tokens(&Value::Null));
        assert!(no_tokens(
            &json!({"totalTokens":0,"inputTokens":0,"outputTokens":0})
        ));
        assert!(!no_tokens(
            &json!({"totalTokens":8,"inputTokens":5,"outputTokens":3})
        ));
    }
}
