//! Source-candidate OpenCode ACP launch configuration. Live release acceptance remains gated.
use super::{
    adapter::AgentAdapter,
    backend::{Backend, ModelQuery, PreparedAgent},
    opencode_adapter::OpenCodeAdapter,
    AgentSink,
};
use crate::{
    daemon::Daemon,
    opencode::{ProviderEndpoint, ProviderStore, RoutingContext},
    sandbox::SandboxHandle,
    workspace::Workspace,
};
use async_trait::async_trait;
use bondsymphonic_proto::{opencode::*, *};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
pub const SOURCE_CANDIDATE_VERSION: &str = "1.18.33";
pub struct OpenCodeBackend;
pub fn host_argv() -> Result<Vec<String>, RpcError> {
    if let Ok(raw) = std::env::var("BS_OPENCODE_BIN") {
        let args = shell_words::split(&raw)
            .map_err(|_| RpcError::invalid_params("invalid BS_OPENCODE_BIN"))?;
        if !args.is_empty() {
            return Ok(args);
        }
    }
    let bin = crate::harness_updates::selected_binary(crate::harness_updates::Harness::OpenCode)
        .unwrap_or_else(|| crate::setup::host_home().join(".local/bin/opencode"));
    if bin.is_file() {
        Ok(vec![bin.display().to_string()])
    } else {
        Err(RpcError::new(
            ErrorCode::PrereqMissing,
            "Install OpenCode in Settings or Setup",
        ))
    }
}
pub fn resolve_selection(
    store: &ProviderStore,
    options: &AgentStartOptions,
) -> Result<(OpenCodeProvider, String), RpcError> {
    let id = options
        .provider_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params("Select an OpenCode provider in Settings > Agents > OpenCode")
        })?;
    let p = store.resolve(id)?;
    if options.resume_session.is_some() && options.model.is_none() {
        return Err(RpcError::invalid_params(
            "Cannot resume OpenCode without its saved model; select the original model in Settings",
        ));
    }
    let selected = options
        .model
        .as_deref()
        .or_else(|| p.models.first().map(|m| m.id.as_str()))
        .ok_or_else(|| {
            RpcError::invalid_params("Add a model ID to the OpenCode provider in Settings")
        })?;
    let model = selected
        .strip_prefix(&format!("{}/", p.id))
        .unwrap_or(selected);
    if !p.models.iter().any(|m| m.id == model) {
        return Err(RpcError::invalid_params(
            "Selected model is not configured for this OpenCode provider; update Settings",
        ));
    }
    let model = model.to_owned();
    Ok((p, model))
}
pub struct LaunchConfig {
    pub config: Value,
    pub env: Vec<(String, String)>,
    pub mcp_servers: Vec<Value>,
    pub provider_endpoints: Vec<(String, u16)>,
}
impl std::fmt::Debug for LaunchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchConfig")
            .field("provider_endpoints", &self.provider_endpoints)
            .finish_non_exhaustive()
    }
}
/// Shared MCP integration seam: ACP servers win over native entries of the same name.
/// Callers resolve secret references to process env, never native config values.
pub fn launch_config(
    p: &OpenCodeProvider,
    model: &str,
    options: &AgentStartOptions,
    routes: &RoutingContext,
    mcp_servers: Vec<Value>,
    mut native_mcp: Value,
) -> Result<LaunchConfig, RpcError> {
    let mode = options.permission_mode.as_deref().unwrap_or("allow");
    if !["allow", "ask"].contains(&mode) {
        return Err(RpcError::invalid_params(
            "OpenCode permission mode must be allow or ask",
        ));
    }
    let routed = ProviderEndpoint::parse(p)?.routed_url(routes)?;
    let mut routed_provider = p.clone();
    routed_provider.base_url = routed.clone();
    let endpoint = ProviderEndpoint::parse(&routed_provider)?;
    let mut models = serde_json::Map::new();
    for m in &p.models {
        let mut value = json!({"name":m.name});
        let mut limits = serde_json::Map::new();
        if let Some(n) = m.context_limit {
            limits.insert("context".into(), n.into());
        }
        if let Some(n) = m.output_limit {
            limits.insert("output".into(), n.into());
        }
        if !limits.is_empty() {
            value["limit"] = limits.into();
        }
        models.insert(m.id.clone(), value);
    }
    let mut provider_options = json!({"baseURL":routed});
    let mut env = vec![];
    if let Some(key) = options.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        provider_options["apiKey"] = json!("{env:BS_OPENCODE_PROVIDER_KEY}");
        env.push(("BS_OPENCODE_PROVIDER_KEY".into(), key.into()));
    }
    if !native_mcp.is_object() {
        return Err(RpcError::invalid_params(
            "native MCP configuration must be an object",
        ));
    }
    for server in &mcp_servers {
        if let Some(name) = server["name"].as_str() {
            native_mcp.as_object_mut().unwrap().remove(name);
        }
    }
    let mut providers = serde_json::Map::new();
    providers.insert(p.id.clone(),json!({"npm":"@ai-sdk/openai-compatible","name":p.name,"options":provider_options,"models":models}));
    let config = json!({"$schema":"https://opencode.ai/config.json","provider":providers,"enabled_providers":[p.id],"model":format!("{}/{model}",p.id),"permission":if mode=="ask"{json!({"*":"ask"})}else{json!("allow")},"mcp":native_mcp,"autoupdate":false});
    Ok(LaunchConfig {
        config,
        env,
        mcp_servers,
        provider_endpoints: vec![(endpoint.host, endpoint.port)],
    })
}
pub fn state_home(root: &Path, ws: &Workspace) -> PathBuf {
    root.join("agent-homes/opencode").join(ws.id.as_str())
}
async fn routing(p: &OpenCodeProvider) -> Result<RoutingContext, RpcError> {
    if p.endpoint_target != EndpointTarget::Windows {
        return Ok(RoutingContext::default());
    }
    let endpoint = ProviderEndpoint::parse(p)?;
    // Ask Windows for its actual interface addresses rather than treating a guessed WSL gateway as Windows.
    let output=tokio::time::timeout(Duration::from_secs(10),tokio::process::Command::new("powershell.exe").args(["-NoProfile","-NonInteractive","-Command","Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -ne '127.0.0.1' -and $_.IPAddress -notlike '169.254.*' } | Select-Object -ExpandProperty IPAddress"]).kill_on_drop(true).output()).await;
    if let Ok(Ok(output)) = output {
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Ok(ip) = line.trim().parse::<std::net::IpAddr>() {
                    if ip.is_unspecified() || ip.is_multicast() || ip.is_loopback() {
                        continue;
                    }
                    if matches!(
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            tokio::net::TcpStream::connect((ip, endpoint.port))
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        return Ok(RoutingContext {
                            windows_host: Some(ip),
                        });
                    }
                }
            }
        }
    }
    Err(RpcError::new(ErrorCode::PrereqMissing,format!("Windows model endpoint on port {} is unreachable from WSL; check Windows listener address and firewall, or select WSL/Remote in Settings",endpoint.port)))
}

// A newer query supersedes an older reply for the same provider. No key or
// derivative of a key is retained here; the generation lives only in memory.
static DISCOVERY_GENERATIONS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
fn next_discovery_generation(id: &str) -> u64 {
    let mut generations = DISCOVERY_GENERATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let current = generations.entry(id.to_owned()).or_insert(0);
    *current = current.wrapping_add(1);
    *current
}

fn apply_discovery_current(
    store: &ProviderStore,
    id: &str,
    ticket: &crate::opencode::DiscoveryTicket,
    models: Vec<OpenCodeModel>,
) -> Result<bool, RpcError> {
    let generations = DISCOVERY_GENERATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let current = *generations.get(id).unwrap_or(&0);
    // Hold the generation lock through publication: a newer request cannot
    // supersede this reply between the comparison and the store update.
    store.apply_discovery(ticket, current, 0, models)
}

pub fn parse_provider_models(bytes: &[u8]) -> Result<Vec<OpenCodeModel>, RpcError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| RpcError::invalid_params("provider returned invalid models JSON"))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("provider models response has no data array"))?;
    Ok(data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim();
            if id.is_empty() || id.chars().any(char::is_control) {
                return None;
            }
            let name = item
                .get("name")
                .or_else(|| item.get("display_name"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id);
            Some(OpenCodeModel {
                id: id.into(),
                name: name.into(),
                context_limit: None,
                output_limit: None,
            })
        })
        .collect())
}

async fn fetch_provider_models(
    url: &str,
    key: Option<&str>,
) -> Result<Vec<OpenCodeModel>, RpcError> {
    use tokio::io::AsyncWriteExt;
    let mut command = tokio::process::Command::new("curl");
    command.args([
        "--fail",
        "--silent",
        "--show-error",
        "--max-time",
        "15",
        "--max-filesize",
        "1048576",
        "--max-redirs",
        "0",
        "--config",
        "-",
        "--url",
        url,
    ]);
    let mut config = String::new();
    if let Some(key) = key.filter(|s| !s.is_empty()) {
        if key.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
            return Err(RpcError::invalid_params(
                "provider API key contains unsupported characters",
            ));
        }
        config = format!("header = \"Authorization: Bearer {key}\"\n");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            RpcError::new(
                ErrorCode::PrereqMissing,
                "curl is required to discover provider models",
            )
        })?;
    let mut stdin = child.stdin.take().expect("curl stdin is piped");
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(config.as_bytes()).await;
    });
    let output = tokio::time::timeout(Duration::from_secs(18), child.wait_with_output())
        .await
        .map_err(|_| {
            RpcError::new(
                ErrorCode::PrereqMissing,
                "provider model discovery timed out",
            )
        })?
        .map_err(|e| RpcError::io(&e))?;
    let _ = feed.await;
    if !output.status.success() {
        return Err(RpcError::new(
            ErrorCode::PrereqMissing,
            "provider model endpoint is unavailable; check its URL, key and service",
        ));
    }
    parse_provider_models(&output.stdout)
}
pub async fn prerequisites() -> Vec<PrereqStatus> {
    let available = OpenCodeBackend.binary().is_some();
    vec![PrereqStatus {
        name: "opencode".into(),
        ok: available,
        detail: if available {
            "OpenCode installed; ACP capabilities are checked when starting an agent".into()
        } else {
            "OpenCode ACP executable required".into()
        },
        fix_hint: (!available).then(|| "Install OpenCode".into()),
    }]
}
#[async_trait]
impl Backend for OpenCodeBackend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor {
            id: AgentAdapterKind::OpenCode,
            label: "OpenCode".into(),
            permission_modes: vec![
                BackendChoice {
                    id: "allow".into(),
                    label: "YOLO".into(),
                },
                BackendChoice {
                    id: "ask".into(),
                    label: "Ask".into(),
                },
            ],
            default_permission_mode: "allow".into(),
            permission_note:
                "OpenCode uses native permission rules. Project overrides require live validation."
                    .into(),
            credential_label: "Provider API key (optional)".into(),
            prerequisite_names: vec!["opencode".into()],
            setup_actions: vec![SetupAction::InstallOpenCode],
        }
    }
    fn binary(&self) -> Option<PathBuf> {
        host_argv().ok().map(|a| PathBuf::from(&a[0]))
    }
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        self.binary()
            .map(|bin| vec![(bin, PathBuf::from("/opt/bs/opencode"))])
            .unwrap_or_default()
    }
    fn extra_hosts(&self) -> Vec<String> {
        vec![]
    }
    async fn prepare(
        &self,
        d: &Daemon,
        ws: &Workspace,
        options: &AgentStartOptions,
    ) -> Result<PreparedAgent, RpcError> {
        let (p, model) = resolve_selection(&d.opencode_providers, options)?;
        let routes = routing(&p).await?;
        let launch = launch_config(&p, &model, options, &routes, vec![], json!({}))?;
        let host = host_argv()?;
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&host[0])
                .args(&host[1..])
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| RpcError::new(ErrorCode::PrereqMissing, "OpenCode version probe timed out"))?
        .map_err(|_| {
            RpcError::new(
                ErrorCode::PrereqMissing,
                "Cannot run OpenCode; reinstall in Settings",
            )
        })?;
        if !out.status.success() || out.stdout.is_empty() {
            return Err(RpcError::new(
                ErrorCode::PrereqMissing,
                "OpenCode version probe failed; update or reinstall OpenCode in Settings",
            ));
        }
        let mut argv = if d.backend.name() == "linux_bwrap" {
            vec!["/opt/bs/opencode".into()]
        } else {
            host
        };
        argv.push("acp".into());
        let home = state_home(&d.dirs.root, ws);
        let cache = d
            .dirs
            .root
            .join("agent-caches/opencode")
            .join(ws.id.as_str());
        for path in [
            home.join("config"),
            home.join("data"),
            home.join("state"),
            cache.clone(),
        ] {
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|e| RpcError::io(&e))?;
        }
        let mut env = launch.env;
        env.extend([
            ("OPENCODE_CONFIG_CONTENT".into(), launch.config.to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                home.join("config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".into(),
                home.join("data").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".into(),
                home.join("state").display().to_string(),
            ),
            ("XDG_CACHE_HOME".into(), cache.display().to_string()),
            ("OPENCODE_DISABLE_AUTOUPDATE".into(), "true".into()),
        ]);
        let mut options = options.clone();
        options.provider_id = Some(p.id);
        options.model = Some(format!(
            "{}/{model}",
            options.provider_id.as_deref().unwrap()
        ));
        options
            .permission_mode
            .get_or_insert_with(|| "allow".into());
        Ok(PreparedAgent {
            argv,
            env,
            cwd: ws.worktree_path.clone(),
            options,
            mcp_servers: launch.mcp_servers,
            provider_endpoints: launch.provider_endpoints,
        })
    }
    fn adapter(
        &self,
        sink: AgentSink,
        handle: Arc<dyn SandboxHandle>,
        prepared: PreparedAgent,
    ) -> Box<dyn AgentAdapter> {
        Box::new(OpenCodeAdapter::new(sink, handle, prepared))
    }
    async fn list_models(
        &self,
        d: &Daemon,
        query: ModelQuery<'_>,
    ) -> Result<ListModelsResult, RpcError> {
        let id = query.provider_id.ok_or_else(|| {
            RpcError::invalid_params("Select an OpenCode provider in Settings to list models")
        })?;
        let generation = next_discovery_generation(id);
        let (p, ticket) = d.opencode_providers.discovery_context(id, generation, 0)?;
        let discovered = async {
            let routed = ProviderEndpoint::parse(&p)?.routed_url(&routing(&p).await?)?;
            let mut routed_provider = p.clone();
            routed_provider.base_url = routed;
            let url = ProviderEndpoint::parse(&routed_provider)?.models_url();
            fetch_provider_models(&url, query.api_key).await
        }
        .await;
        match discovered {
            Ok(models) => {
                let mut seen = std::collections::HashSet::new();
                let new_models: Vec<_> = models
                    .into_iter()
                    .filter(|m| {
                        seen.insert(m.id.clone())
                            && !p.models.iter().any(|existing| existing.id == m.id)
                    })
                    .collect();
                if !new_models.is_empty() {
                    apply_discovery_current(&d.opencode_providers, id, &ticket, new_models)?;
                }
            }
            Err(error) if p.models.is_empty() => return Err(error),
            Err(_) => {} // Manual models remain usable when discovery is unavailable.
        }
        let p = d.opencode_providers.resolve(id)?;
        Ok(ListModelsResult {
            models: p
                .models
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.id,
                    display_name: m.name,
                    created_at: String::new(),
                })
                .collect(),
        })
    }
}
