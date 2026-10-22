//! A launch grants access only to its resolved servers, workspace and backend.
use super::{
    auth::{random_secret, RemoteMcpManager},
    bridge::{read_frame, write_message, FrameBuffer},
    transport::{error, CurlHttpClient, HttpClient, RemoteMcpSession, MAX_BODY, PROTOCOL_VERSION},
    McpManager,
};
use crate::sandbox::{SandboxCommand, SandboxHandle};
use bondsymphonic_proto::*;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio_util::sync::CancellationToken;

pub fn backend_name(backend: AgentAdapterKind) -> &'static str {
    match backend {
        AgentAdapterKind::Claude => "claude",
        AgentAdapterKind::Codex => "codex",
        AgentAdapterKind::OpenCode => "opencode",
        AgentAdapterKind::Terminal => "terminal",
    }
}

pub fn capabilities(sandbox_backend: &str) -> McpCapabilities {
    McpCapabilities {
        registry: true,
        stdio: cfg!(unix) && sandbox_backend == "linux_bwrap",
        streamable_http: cfg!(unix) && sandbox_backend == "linux_bwrap",
        oauth: true,
    }
}

pub struct McpBroker {
    registry: Arc<McpManager>,
    remote: Arc<RemoteMcpManager>,
    http: Arc<dyn HttpClient>,
    bindings: Mutex<Vec<Weak<McpBinding>>>,
    statuses: Mutex<BTreeMap<McpServerId, (u64, u64, McpServerStatus)>>,
    shutdown: CancellationToken,
}

/// Only the broker can construct this grant. It is never deserialized from a peer.
pub struct McpBinding {
    workspace: WorkspaceId,
    backend: String,
    config: McpServerConfig,
    generation: u64,
    account: u64,
    token: String,
    handle: Arc<dyn SandboxHandle>,
    cwd: PathBuf,
    cancel: CancellationToken,
    active: AtomicUsize,
}
struct ConnectionActivity(Arc<McpBinding>);
impl ConnectionActivity {
    fn new(binding: Arc<McpBinding>) -> Self {
        binding.active.fetch_add(1, Ordering::SeqCst);
        Self(binding)
    }
}
impl Drop for ConnectionActivity {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl McpBinding {
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}
impl Drop for McpBinding {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct McpLaunchConfig {
    pub servers: Vec<Value>,
    pub lease: Arc<McpLaunchLease>,
}
impl McpLaunchConfig {
    pub fn empty() -> Self {
        Self {
            servers: vec![],
            lease: Arc::new(McpLaunchLease {
                cancel: CancellationToken::new(),
            }),
        }
    }
}
pub struct McpLaunchLease {
    cancel: CancellationToken,
}
impl McpLaunchLease {
    pub fn close(&self) {
        self.cancel.cancel();
    }
}
impl Drop for McpLaunchLease {
    fn drop(&mut self) {
        self.close();
    }
}

impl McpBroker {
    pub fn new(registry: Arc<McpManager>, remote: Arc<RemoteMcpManager>) -> Self {
        Self::with_client(registry, remote, Arc::new(CurlHttpClient))
    }
    pub fn with_client(
        registry: Arc<McpManager>,
        remote: Arc<RemoteMcpManager>,
        http: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            registry,
            remote,
            http,
            bindings: Mutex::new(Vec::new()),
            statuses: Mutex::new(BTreeMap::new()),
            shutdown: CancellationToken::new(),
        }
    }
    fn current(&self, binding: &McpBinding) -> bool {
        !binding.cancel.is_cancelled()
            && !self.shutdown.is_cancelled()
            && self.registry.generation(&binding.config.id) == binding.generation
            && self.remote.auth_generation(&binding.config.id) == binding.account
            && self
                .registry
                .resolve(binding.workspace.clone(), &binding.backend)
                .iter()
                .any(|s| s == &binding.config)
    }
    pub fn reconcile(&self) {
        self.bindings.lock().retain(|weak| {
            if let Some(binding) = weak.upgrade() {
                if !self.current(&binding) {
                    binding.cancel.cancel();
                }
                true
            } else {
                false
            }
        });
    }
    pub fn revoke(&self, id: &McpServerId) {
        for binding in self.bindings.lock().iter().filter_map(Weak::upgrade) {
            if &binding.config.id == id {
                binding.cancel.cancel();
            }
        }
        self.statuses.lock().remove(id);
    }
    pub fn revoke_workspace(&self, workspace: &WorkspaceId) {
        for binding in self.bindings.lock().iter().filter_map(Weak::upgrade) {
            if &binding.workspace == workspace {
                binding.cancel.cancel();
            }
        }
    }
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
    fn record(
        &self,
        id: &McpServerId,
        state: McpConnectionState,
        reason: Option<&str>,
    ) -> McpServerStatus {
        self.record_at(
            id,
            self.registry.generation(id),
            self.remote.auth_generation(id),
            state,
            reason,
        )
    }
    fn record_at(
        &self,
        id: &McpServerId,
        generation: u64,
        account: u64,
        state: McpConnectionState,
        reason: Option<&str>,
    ) -> McpServerStatus {
        let status = McpServerStatus {
            server_id: id.clone(),
            state,
            reason: reason.map(str::to_owned),
        };
        if generation == self.registry.generation(id) && account == self.remote.auth_generation(id)
        {
            let mut statuses = self.statuses.lock();
            if !statuses
                .get(id)
                .is_some_and(|(g, a, _)| (*g, *a) > (generation, account))
            {
                statuses.insert(id.clone(), (generation, account, status.clone()));
            }
        }
        status
    }
    pub fn list(&self) -> McpListResult {
        let registry = self.registry.snapshot();
        let statuses = registry
            .servers
            .iter()
            .map(|server| {
                if self.remote.has_pending_login(&server.id) {
                    return McpServerStatus {
                        server_id: server.id.clone(),
                        state: McpConnectionState::Connecting,
                        reason: None,
                    };
                }
                if let Some((generation, account, status)) =
                    self.statuses.lock().get(&server.id).cloned()
                {
                    if generation == self.registry.generation(&server.id)
                        && account == self.remote.auth_generation(&server.id)
                        && status.state == McpConnectionState::Error
                    {
                        return status;
                    }
                }
                let connected = self
                    .bindings
                    .lock()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|b| {
                        b.config.id == server.id
                            && self.current(&b)
                            && b.active.load(Ordering::SeqCst) > 0
                    })
                    || (matches!(
                        server.transport,
                        McpTransportConfig::Http {
                            auth: McpAuthConfig::OAuth { .. },
                            ..
                        }
                    ) && self.remote.has_credentials(&server.id));
                McpServerStatus {
                    server_id: server.id.clone(),
                    state: if connected {
                        McpConnectionState::Connected
                    } else {
                        McpConnectionState::Disconnected
                    },
                    reason: None,
                }
            })
            .collect();
        McpListResult { registry, statuses }
    }
    pub fn set_connected(&self, id: &McpServerId) {
        self.record(id, McpConnectionState::Connected, None);
    }

    /// Performs only initialize and tools/list through the same sandbox grant.
    pub async fn test(
        self: &Arc<Self>,
        params: McpTestParams,
        handle: Arc<dyn SandboxHandle>,
        cwd: PathBuf,
    ) -> McpTestResult {
        let generation = self.registry.generation(&params.server_id);
        let account = self.remote.auth_generation(&params.server_id);
        let lifetime = CancellationToken::new();
        let lease = McpLaunchLease {
            cancel: lifetime.clone(),
        };
        let outcome = async {
            let binding = self.bind(
                params.workspace_id.clone(),
                &params.backend,
                &params.server_id,
                handle,
                cwd,
                lifetime,
            )?;
            let (client, server) = tokio::io::duplex(MAX_BODY);
            let (server_read, server_write) = tokio::io::split(server);
            let (read, mut write) = tokio::io::split(client);
            let token = binding.token.clone();
            let broker = self.clone();
            let task =
                tokio::spawn(async move { broker.serve(binding, server_read, server_write).await });
            // Dropping the lease cancels the task even if this RPC is cancelled.
            let mut reader = BufReader::new(read);
            let result=async {
                write_message(&mut write,&json!({"token":token})).await?;
                write_message(&mut write,&json!({"jsonrpc":"2.0","id":"bs-test-init","method":"initialize","params":{"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"BondSymphonic test","version":"1"}}})).await?;
                let response=read_test_response(&mut reader,&mut write,"bs-test-init").await?;
                if response.get("error").is_some() {return Err(error("initialize rejected"));}
                write_message(&mut write,&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
                write_message(&mut write,&json!({"jsonrpc":"2.0","id":"bs-test-tools","method":"tools/list"})).await?;
                let response=read_test_response(&mut reader,&mut write,"bs-test-tools").await?;
                let tools=response.pointer("/result/tools").and_then(Value::as_array).ok_or_else(||error("tool discovery failed"))?;
                if tools.len()>4096 {return Err(error("too many tools"));}
                Ok::<_,RpcError>(tools.iter().filter_map(|tool|tool["name"].as_str()).filter(|name|name.len()<=256 && !name.chars().any(char::is_control)).map(str::to_owned).collect())
            }.await;
            lease.close();
            let _ = task.await;
            result
        };
        match tokio::time::timeout(Duration::from_secs(35), outcome).await {
            Ok(Ok(names)) => McpTestResult {
                status: self.record_at(
                    &params.server_id,
                    generation,
                    account,
                    McpConnectionState::Connected,
                    None,
                ),
                tool_names: names,
            },
            _ => McpTestResult {
                status: self.record_at(
                    &params.server_id,
                    generation,
                    account,
                    McpConnectionState::Error,
                    Some("test_failed"),
                ),
                tool_names: vec![],
            },
        }
    }

    /// Creates a fixed grant and watches all revocation sources even when idle.
    pub fn bind(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        backend: &str,
        server: &McpServerId,
        handle: Arc<dyn SandboxHandle>,
        cwd: PathBuf,
        lifetime: CancellationToken,
    ) -> Result<Arc<McpBinding>, RpcError> {
        let mut registry_changes = self.registry.subscribe_changes();
        let mut auth_changes = self.remote.subscribe_auth_changes();
        let config = self
            .registry
            .resolve(workspace.clone(), backend)
            .into_iter()
            .find(|s| &s.id == server)
            .ok_or_else(|| error("server disabled for workspace/backend"))?;
        let binding = Arc::new(McpBinding {
            generation: self.registry.generation(server),
            account: self.remote.auth_generation(server),
            workspace,
            backend: backend.into(),
            config,
            token: random_secret(),
            handle: handle.clone(),
            cwd,
            cancel: lifetime.child_token(),
            active: AtomicUsize::new(0),
        });
        if !self.current(&binding) {
            return Err(error("configuration changed"));
        }
        self.bindings.lock().push(Arc::downgrade(&binding));
        let broker = Arc::downgrade(self);
        let weak = Arc::downgrade(&binding);
        let cancel = binding.cancel.clone();
        let shutdown = self.shutdown.clone();
        let mut died = handle.died();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = shutdown.cancelled() => break,
                    r = registry_changes.changed() => { if r.is_err() { break; } },
                    r = auth_changes.changed() => { if r.is_err() { break; } },
                    _ = async { match died.as_mut() { Some(watch) => { if !*watch.borrow() { let _ = watch.wait_for(|dead| *dead).await; } }, None => std::future::pending().await } } => break,
                }
                let (Some(broker), Some(binding)) = (broker.upgrade(), weak.upgrade()) else {
                    break;
                };
                if !broker.current(&binding) {
                    break;
                }
            }
            cancel.cancel();
        });
        Ok(binding)
    }

    /// Authenticates the capability before starting a local child or remote request.
    pub async fn serve<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send>(
        self: &Arc<Self>,
        binding: Arc<McpBinding>,
        reader: R,
        mut writer: W,
    ) -> Result<(), RpcError> {
        let mut reader = BufReader::new(reader);
        let result = async {
            let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader, 4096))
                .await
                .map_err(|_| error("bridge authentication expired"))??
                .ok_or_else(|| error("bridge authentication missing"))?;
            let hello: Value = serde_json::from_slice(&frame)
                .map_err(|_| error("bridge authentication failed"))?;
            if hello["token"].as_str() != Some(binding.token.as_str()) || !self.current(&binding) {
                return Err(error("bridge authentication failed"));
            }
            let _activity = ConnectionActivity::new(binding.clone());
            match &binding.config.transport {
                McpTransportConfig::Stdio {
                    command,
                    args,
                    env,
                    env_secrets,
                } => {
                    let mut child_env: Vec<_> =
                        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    for (name, reference) in env_secrets {
                        child_env.push((
                            name.clone(),
                            self.registry
                                .secret(reference)
                                .ok_or_else(|| error("credential missing"))?,
                        ));
                    }
                    if !self.current(&binding) {
                        return Err(error("configuration changed"));
                    }
                    let mut argv = vec![command.clone()];
                    argv.extend(args.clone());
                    let mut child = binding
                        .handle
                        .spawn(SandboxCommand {
                            argv,
                            env: child_env,
                            cwd: Some(binding.cwd.clone()),
                            pty: None,
                        })
                        .await
                        .map_err(|_| error("local server failed to start"))?;
                    struct Kill(Box<dyn Fn() + Send + Sync>);
                    impl Drop for Kill {
                        fn drop(&mut self) {
                            (self.0)();
                        }
                    }
                    let _kill = Kill(child.killer);
                    let mut input = child
                        .stdin
                        .take()
                        .ok_or_else(|| error("local server missing stdin"))?;
                    let mut output = BufReader::new(
                        child
                            .stdout
                            .take()
                            .ok_or_else(|| error("local server missing stdout"))?,
                    );
                    let mut diagnostic = child
                        .stderr
                        .take()
                        .ok_or_else(|| error("local server missing stderr"))?
                        .take(65537);
                    let mut discarded = Vec::new();
                    self.record_at(
                        &binding.config.id,
                        binding.generation,
                        binding.account,
                        McpConnectionState::Connected,
                        None,
                    );
                    tokio::select! {
                        r = relay_stdio(&mut reader,&mut writer,&mut output,&mut input) => r,
                        _ = &mut child.exit => Ok(()),
                        _ = async { let _ = diagnostic.read_to_end(&mut discarded).await; if discarded.len() <= 65536 { std::future::pending::<()>().await; } } => Err(error("local diagnostics too large")),
                    }
                }
                McpTransportConfig::Http { url, .. } => {
                    let session = Arc::new(RemoteMcpSession::new(
                        url.clone(),
                        self.remote.headers(&binding.config.id).await?,
                        self.http.clone(),
                    )?);
                    let mut pending = tokio::task::JoinSet::new();
                    let mut initialized = false;
                    let mut ids = std::collections::HashSet::new();
                    let mut frame_buffer = FrameBuffer::default();
                    loop {
                        tokio::select! {
                            result = pending.join_next(), if !pending.is_empty() => {
                                let (id,result) = result.ok_or_else(|| error("remote request failed"))?.map_err(|_| error("remote request failed"))?;
                                ids.remove(&id);
                                for message in result? { write_message(&mut writer,&message).await?; }
                            }
                            frame = frame_buffer.read(&mut reader,MAX_BODY) => {
                                let Some(frame) = frame? else { return Ok(()); };
                                if !self.current(&binding) { return Err(error("configuration changed")); }
                                let mut message:Value = serde_json::from_slice(&frame).map_err(|_|error("invalid MCP JSON"))?;
                                session.set_auth_headers(self.remote.headers(&binding.config.id).await?)?;
                                if message["method"] == "initialize" {
                                    // HTTP callbacks requiring interaction are not yet forwarded.
                                    if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) { params.insert("capabilities".into(),json!({})); }
                                    for response in session.send(message).await? { write_message(&mut writer,&response).await?; }
                                    initialized = true;
                                    self.record_at(&binding.config.id,binding.generation,binding.account,McpConnectionState::Connected,None);
                                } else if message["method"] == "notifications/initialized" {
                                    session.send(message).await?;
                                } else {
                                    if !initialized { return Err(error("session not initialized")); }
                                    let id = message.get("id").filter(|_| message.get("method").is_some()).map(Value::to_string).unwrap_or_default();
                                    if pending.len()>=64 || (!id.is_empty() && ids.len()>=32) {return Err(error("too many in-flight requests"));}
                                    if !id.is_empty() && !ids.insert(id.clone()) { return Err(error("duplicate in-flight ID")); }
                                    let session = session.clone();
                                    pending.spawn(async move { let result = session.send(message).await; (id,result) });
                                }
                            }
                        }
                    }
                }
            }
        };
        let outcome = tokio::select! { _ = binding.cancel.cancelled() => Ok(()), _ = self.shutdown.cancelled() => Ok(()), r = result => r };
        if let Err(err) = &outcome {
            if !self.current(&binding) {
                return outcome;
            }
            let reason = if err.message.contains("step_up_scope_required") {
                "step_up_scope_required"
            } else if err.message.contains("authentication_required") {
                "authentication_required"
            } else if err.message.contains("session_expired") {
                "session_expired"
            } else {
                "connection_failed"
            };
            self.record_at(
                &binding.config.id,
                binding.generation,
                binding.account,
                McpConnectionState::Error,
                Some(reason),
            );
        }
        outcome
    }

    // Keep both sides of the sandbox path mapping explicit at this boundary.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        backend: AgentAdapterKind,
        handle: Arc<dyn SandboxHandle>,
        run_dir: PathBuf,
        sandbox_run_dir: PathBuf,
        cwd: PathBuf,
        native_files: &[PathBuf],
    ) -> Result<McpLaunchConfig, RpcError> {
        let lease = Arc::new(McpLaunchLease {
            cancel: self.shutdown.child_token(),
        });
        #[allow(unused_mut)]
        let mut servers = Vec::new();
        for config in self
            .registry
            .resolve(workspace.clone(), backend_name(backend))
        {
            if matches!(
                config.transport,
                McpTransportConfig::Http {
                    auth: McpAuthConfig::OAuth { .. },
                    ..
                }
            ) && !self.remote.has_credentials(&config.id)
            {
                continue;
            }
            #[cfg(not(unix))]
            {
                let _ = (&handle, &run_dir, &sandbox_run_dir, &cwd, &native_files);
                self.record(
                    &config.id,
                    McpConnectionState::Error,
                    Some("unix_bridge_required"),
                );
                continue;
            }
            #[cfg(unix)]
            {
                let name = format!("bondsymphonic_mcp_{}", config.id.as_str());
                check_native_collision(native_files, &name)?;
                let binding = self.bind(
                    workspace.clone(),
                    backend_name(backend),
                    &config.id,
                    handle.clone(),
                    cwd.clone(),
                    lease.cancel.clone(),
                )?;
                std::fs::create_dir_all(&run_dir)
                    .map_err(|_| error("bridge directory unavailable"))?;
                let filename = format!("mcp-{}.sock", hex::encode(rand::random::<[u8; 12]>()));
                let socket = run_dir.join(&filename);
                let listener = tokio::net::UnixListener::bind(&socket)
                    .map_err(|_| error("bridge socket unavailable"))?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| error("bridge permissions failed"))?;
                servers.push(json!({"name":name,"command":handle.helper_exe(),"args":["mcp-bridge","--socket",sandbox_run_dir.join(filename),"--token",binding.token],"env":[]}));
                let broker = self.clone();
                tokio::spawn(async move {
                    let mut connections = tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {
                            _ = binding.cancel.cancelled() => break,
                            _ = broker.shutdown.cancelled() => break,
                            _ = connections.join_next(), if !connections.is_empty() => {},
                            accepted = listener.accept(), if connections.len() < 8 => {
                                let Ok((stream,_)) = accepted else { break; };
                                let (reader,writer) = stream.into_split();
                                let broker = broker.clone(); let binding = binding.clone();
                                connections.spawn(async move { let _ = broker.serve(binding,reader,writer).await; });
                            }
                        }
                    }
                    connections.abort_all();
                    while connections.join_next().await.is_some() {}
                    drop(listener);
                    let _ = std::fs::remove_file(socket);
                });
            }
        }
        Ok(McpLaunchConfig { servers, lease })
    }
}
impl Drop for McpBroker {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub fn check_native_collision(files: &[PathBuf], name: &str) -> Result<(), RpcError> {
    for file in files {
        match std::fs::File::open(file) {
            Ok(open) => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(
                    &mut std::io::Read::take(open, (MAX_BODY + 1) as u64),
                    &mut bytes,
                )
                .map_err(|_| error("native MCP configuration unreadable"))?;
                if bytes.len() > MAX_BODY {
                    return Err(error("native MCP configuration too large"));
                }
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| error("native MCP configuration unreadable"))?;
                let collision = if file
                    .extension()
                    .is_some_and(|extension| extension == "toml")
                {
                    let parsed: toml::Value = toml::from_str(text)
                        .map_err(|_| error("invalid native MCP configuration"))?;
                    contains_native_key(
                        &serde_json::to_value(parsed)
                            .map_err(|_| error("invalid native MCP configuration"))?,
                        name,
                    )
                } else {
                    jsonc_has_key(text, name)?
                };
                if collision {
                    return Err(error("native MCP name collision; rename managed server"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(error("native MCP configuration unreadable")),
        }
    }
    Ok(())
}

fn contains_native_key(value: &Value, name: &str) -> bool {
    match value {
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| key == name || contains_native_key(value, name)),
        Value::Array(entries) => entries.iter().any(|value| contains_native_key(value, name)),
        _ => false,
    }
}
/// Scan JSON/JSONC keys without changing or interpreting native configuration.
/// String decoding handles escaped names; comments and trailing commas remain valid.
fn jsonc_has_key(text: &str, name: &str) -> Result<bool, RpcError> {
    fn trivia(bytes: &[u8], i: &mut usize) -> Result<(), RpcError> {
        loop {
            while bytes.get(*i).is_some_and(u8::is_ascii_whitespace) {
                *i += 1;
            }
            if bytes.get(*i..*i + 2) == Some(b"//") {
                while bytes.get(*i).is_some_and(|b| *b != b'\n') {
                    *i += 1;
                }
            } else if bytes.get(*i..*i + 2) == Some(b"/*") {
                *i += 2;
                while bytes.get(*i..*i + 2) != Some(b"*/") {
                    if *i >= bytes.len() {
                        return Err(error("invalid native MCP configuration"));
                    }
                    *i += 1;
                }
                *i += 2;
            } else {
                return Ok(());
            }
        }
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        trivia(bytes, &mut i)?;
        if bytes.get(i) != Some(&b'"') {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        loop {
            match bytes.get(i) {
                Some(b'\\') => i += 2,
                Some(b'"') => {
                    i += 1;
                    break;
                }
                Some(_) => i += 1,
                None => return Err(error("invalid native MCP configuration")),
            }
        }
        let decoded: String = serde_json::from_str(&text[start..i])
            .map_err(|_| error("invalid native MCP configuration"))?;
        trivia(bytes, &mut i)?;
        if bytes.get(i) == Some(&b':') && decoded == name {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn native_supplement(
    backend: AgentAdapterKind,
    argv: &mut Vec<String>,
    servers: &[Value],
) -> Result<(), RpcError> {
    if servers.is_empty() {
        return Ok(());
    }
    match backend {
        AgentAdapterKind::Claude => {
            let mut map = serde_json::Map::new();
            for server in servers {
                let name = server["name"]
                    .as_str()
                    .ok_or_else(|| error("invalid bridge declaration"))?;
                map.insert(
                    name.into(),
                    json!({"type":"stdio","command":server["command"],"args":server["args"]}),
                );
            }
            argv.extend(["--mcp-config".into(), json!({"mcpServers":map}).to_string()]);
        }
        AgentAdapterKind::Codex => {
            let mut args = Vec::new();
            for server in servers {
                let name = server["name"]
                    .as_str()
                    .ok_or_else(|| error("invalid bridge declaration"))?;
                if !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(error("invalid bridge name"));
                }
                let command = toml::Value::String(
                    server["command"]
                        .as_str()
                        .ok_or_else(|| error("invalid bridge declaration"))?
                        .into(),
                )
                .to_string();
                let values = server["args"]
                    .as_array()
                    .ok_or_else(|| error("invalid bridge declaration"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(|s| toml::Value::String(s.into()))
                            .ok_or_else(|| error("invalid bridge declaration"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for value in [
                    format!("mcp_servers.{name}.command={command}"),
                    format!("mcp_servers.{name}.args={}", toml::Value::Array(values)),
                    format!("mcp_servers.{name}.required=false"),
                ] {
                    args.extend(["-c".into(), value]);
                }
            }
            argv.splice(1..1, args);
        }
        AgentAdapterKind::OpenCode => {}
        _ => return Err(error("unsupported backend")),
    }
    Ok(())
}

async fn read_test_response<R: tokio::io::AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    id: &str,
) -> Result<Value, RpcError> {
    for _ in 0..256 {
        let frame = read_frame(reader, MAX_BODY)
            .await?
            .ok_or_else(|| error("test connection closed"))?;
        let message: Value =
            serde_json::from_slice(&frame).map_err(|_| error("invalid test response"))?;
        if message["id"] == id && message.get("method").is_none() {
            return Ok(message);
        }
        if message["method"] == "ping" && message.get("id").is_some() {
            write_message(
                writer,
                &json!({"jsonrpc":"2.0","id":message["id"],"result":{}}),
            )
            .await?;
        }
    }
    Err(error("too many test notifications"))
}

fn track_message(
    message: &Value,
    requests: &mut BTreeMap<String, tokio::time::Instant>,
    responses: &mut BTreeMap<String, tokio::time::Instant>,
) -> Result<(), RpcError> {
    if !message.is_object() || message["jsonrpc"] != "2.0" {
        return Err(error("invalid stdio JSON-RPC"));
    }
    let Some(id) = message.get("id") else {
        return Ok(());
    };
    if !id.is_string() && !id.is_i64() && !id.is_u64() {
        return Err(error("invalid stdio ID"));
    }
    let id = id.to_string();
    if message.get("method").is_some() {
        if requests.len() >= 32
            || requests
                .insert(id, tokio::time::Instant::now() + Duration::from_secs(60))
                .is_some()
        {
            return Err(error("too many or duplicate stdio requests"));
        }
    } else if responses.remove(&id).is_none() {
        return Err(error("unknown stdio response ID"));
    }
    Ok(())
}

async fn relay_stdio<
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    U: tokio::io::AsyncBufRead + Unpin,
    V: AsyncWrite + Unpin,
>(
    client: &mut R,
    client_out: &mut W,
    upstream: &mut U,
    upstream_in: &mut V,
) -> Result<(), RpcError> {
    let calls = Mutex::new(StdioCalls::default());
    let deadline = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let state = calls.lock();
            if state
                .client
                .values()
                .chain(state.server.values())
                .any(|deadline| *deadline <= tokio::time::Instant::now())
            {
                return Err(error("stdio request deadline exceeded"));
            }
        }
    };
    // Each pipe direction keeps reading while the other writer backpressures.
    tokio::select! {
        r=pump_stdio(client,upstream_in,&calls,true)=>r,
        r=pump_stdio(upstream,client_out,&calls,false)=>r,
        r=deadline=>r,
    }
}

#[derive(Default)]
struct StdioCalls {
    client: BTreeMap<String, tokio::time::Instant>,
    server: BTreeMap<String, tokio::time::Instant>,
}
async fn pump_stdio<R: tokio::io::AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    calls: &Mutex<StdioCalls>,
    from_client: bool,
) -> Result<(), RpcError> {
    while let Some(frame) = read_frame(reader, MAX_BODY).await? {
        let value: Value =
            serde_json::from_slice(&frame).map_err(|_| error("invalid stdio JSON"))?;
        {
            let mut state = calls.lock();
            let StdioCalls { client, server } = &mut *state;
            if from_client {
                track_message(&value, client, server)?;
            } else {
                track_message(&value, server, client)?;
            }
        }
        tokio::time::timeout(Duration::from_secs(35), write_message(writer, &value))
            .await
            .map_err(|_| error("stdio write deadline exceeded"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stdio_simultaneous_large_frames_do_not_deadlock() {
        let (client, client_pipe) = tokio::io::duplex(128);
        let (server, server_pipe) = tokio::io::duplex(128);
        let (client_read, mut client_write) = tokio::io::split(client_pipe);
        let (server_read, mut server_write) = tokio::io::split(server_pipe);
        let relay = tokio::spawn(async move {
            relay_stdio(
                &mut BufReader::new(client_read),
                &mut client_write,
                &mut BufReader::new(server_read),
                &mut server_write,
            )
            .await
        });
        let payload = "x".repeat(65536);
        let client_peer = async {
            let (read, mut write) = tokio::io::split(client);
            write_message(
                &mut write,
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"payload":payload}}),
            )
            .await
            .unwrap();
            let frame = read_frame(&mut BufReader::new(read), MAX_BODY)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&frame).unwrap()["method"],
                "notifications/progress"
            );
        };
        let server_peer = async {
            let (read, mut write) = tokio::io::split(server);
            write_message(&mut write,&json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"payload":payload}})).await.unwrap();
            let frame = read_frame(&mut BufReader::new(read), MAX_BODY)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap()["id"], 1);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(client_peer, server_peer);
        })
        .await
        .unwrap();
        relay.await.unwrap().unwrap();
    }
}
