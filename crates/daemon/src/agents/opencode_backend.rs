//! OpenCode installation, isolated native configuration, and model discovery.
use super::{
    adapter::AgentAdapter,
    backend::{Backend, PreparedAgent},
    opencode::OpenCodeAdapter,
    AgentSink,
};
use crate::{
    daemon::Daemon,
    sandbox::{SandboxCommand, SandboxHandle},
    workspace::Workspace,
};
use async_trait::async_trait;
use bondsymphonic_proto::*;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;

pub const PINNED_OPENCODE_VERSION: &str = "1.14.48";
pub struct OpenCodeBackend;
pub fn opencode_home(root: &Path) -> PathBuf {
    root.join("opencode-home")
}

pub use super::opencode_config::seed_native_files;

pub fn ensure_native_home(root: &Path) -> std::io::Result<()> {
    let host = crate::setup::host_home();
    let configured = std::env::var_os(super::opencode_config::USER_CONFIG_ROOT_ENV);
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let config =
        super::opencode_config::native_config_root(&host, configured.as_deref(), xdg.as_deref());
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| host.join(".local/share"));
    seed_native_files(root, &config, &data)
}

pub fn process_env(root: &Path, api_key: Option<&str>) -> Vec<(String, String)> {
    let home = opencode_home(root);
    let mut env = [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
    ]
    .into_iter()
    .map(|(key, sub)| (key.into(), home.join(sub).display().to_string()))
    .collect::<Vec<_>>();
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        env.push(("OPENAI_API_KEY".into(), key.into()));
    }
    env
}

pub fn has_login(root: &Path) -> bool {
    let Ok(bytes) = std::fs::read(opencode_home(root).join("data/opencode/auth.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    value.as_object().is_some_and(|providers| {
        providers.values().any(|auth| match auth["type"].as_str() {
            Some("api") => auth["key"]
                .as_str()
                .is_some_and(|key| !key.trim().is_empty()),
            Some("oauth") => {
                auth["access"]
                    .as_str()
                    .is_some_and(|token| !token.trim().is_empty())
                    || auth["refresh"]
                        .as_str()
                        .is_some_and(|token| !token.trim().is_empty())
            }
            _ => false,
        })
    })
}

pub fn host_argv() -> Result<Vec<String>, RpcError> {
    if let Ok(value) = std::env::var("BS_OPENCODE_BIN") {
        let args = shell_words::split(&value)
            .map_err(|_| RpcError::invalid_params("invalid BS_OPENCODE_BIN"))?;
        if !args.is_empty() {
            return Ok(args);
        }
    }
    for path in [
        crate::setup::host_home().join(".local/bin/opencode"),
        crate::setup::host_home().join(".opencode/bin/opencode"),
    ] {
        if path.is_file() {
            return Ok(vec![path.display().to_string()]);
        }
    }
    Err(RpcError::new(
        ErrorCode::PrereqMissing,
        "Install OpenCode in Settings > Agents",
    ))
}

pub async fn prerequisites(root: &Path) -> Vec<PrereqStatus> {
    let config_error = ensure_native_home(root).err();
    if let Some(error) = &config_error {
        tracing::warn!("OpenCode native configuration could not be refreshed: {error}");
    }
    let available = OpenCodeBackend.binary().is_some();
    let auth = has_login(root);
    let mut checks = vec![
        PrereqStatus {
            name: "opencode".into(),
            ok: available,
            detail: if available {
                format!("OpenCode installed; ACP protocol 1; installer pins CLI {PINNED_OPENCODE_VERSION}")
            } else {
                "OpenCode is not installed".into()
            },
            fix_hint: (!available).then(|| "Install OpenCode".into()),
        },
        PrereqStatus {
            name: "opencode_auth".into(),
            ok: auth,
            detail: if auth {
                "OpenCode provider sign-in present"
            } else {
                "Sign in to a provider, use an OpenAI API key, or configure a local provider"
            }
            .into(),
            fix_hint: (!auth).then(|| "Sign in to OpenCode or configure a provider".into()),
        },
    ];
    if let Some(error) = config_error {
        checks.push(PrereqStatus {name:"opencode_config".into(),ok:false,detail:error.to_string(),fix_hint:Some("Check access to the regular OpenCode config files and the managed config directory".into())});
    }
    checks
}

#[async_trait]
impl Backend for OpenCodeBackend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor { id: AgentAdapterKind::OpenCode, label: "OpenCode".into(), permission_modes: vec![BackendChoice { id: "yolo".into(), label: "YOLO (sandboxed)".into() }, BackendChoice { id: "ask".into(), label: "Ask before tools".into() }], default_permission_mode: "ask".into(),
            permission_note: "OpenCode uses its own sandbox, provider sign-in and configuration. Models use provider/model IDs. Configure local providers in opencode.json; no API key is required for unauthenticated local servers.".into(), credential_label: "OpenAI API key".into(), prerequisite_names: vec!["opencode".into(), "opencode_auth".into()], setup_actions: vec![SetupAction::InstallOpencode, SetupAction::OpencodeLogin, SetupAction::OpencodeLogout] }
    }
    fn binary(&self) -> Option<PathBuf> {
        host_argv().ok().map(|args| PathBuf::from(&args[0]))
    }
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        self.binary()
            .map(|bin| vec![(bin, PathBuf::from("/opt/bs/opencode"))])
            .unwrap_or_default()
    }
    fn extra_hosts(&self) -> Vec<String> {
        [
            "opencode.ai",
            "models.dev",
            "api.openai.com",
            "auth.openai.com",
            "chatgpt.com",
            "openrouter.ai",
            "generativelanguage.googleapis.com",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    async fn prepare(
        &self,
        d: &Daemon,
        ws: &Workspace,
        options: &AgentStartOptions,
    ) -> Result<PreparedAgent, RpcError> {
        ensure_native_home(&d.dirs.root).map_err(|error| {
            RpcError::new(
                ErrorCode::IoError,
                format!("OpenCode config could not be refreshed: {error}"),
            )
        })?;
        let mode = options.permission_mode.as_deref().unwrap_or("ask");
        if !["ask", "yolo"].contains(&mode) {
            return Err(RpcError::invalid_params(
                "OpenCode permission mode must be ask or yolo",
            ));
        }
        let mut argv = if d.backend.name() == super::claude::SANDBOXED_BACKEND {
            self.binary().ok_or_else(|| {
                RpcError::new(
                    ErrorCode::PrereqMissing,
                    "Install OpenCode in Settings > Agents",
                )
            })?;
            vec!["/opt/bs/opencode".into()]
        } else {
            host_argv()?
        };
        argv.push("acp".into());
        let mut env = process_env(&d.dirs.root, options.api_key.as_deref());
        if d.backend.name() == super::claude::SANDBOXED_BACKEND {
            env.push(("BS_OPENCODE_SANDBOX".into(), "1".into()));
        }
        let agent_name = format!("bondsymphonic_{}_{}", ws.id, crate::ids::new_id("launch_"));
        env.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            // Native per-agent permissions are appended after global rules.
            // A fresh workspace-scoped name also avoids inherited rules on a
            // prior private agent; native parsing normalizes string rules to
            // wildcard objects before merging config sources.
            json!({
                "permission": if mode == "yolo" { "allow" } else { "ask" },
                "default_agent": agent_name,
                "agent": {agent_name: {
                    "mode":"primary", "hidden":false, "disable":false,
                    "description":"BondSymphonic workspace agent",
                    "permission":if mode == "yolo" { "allow" } else { "ask" },
                }},
            })
            .to_string(),
        ));
        Ok(PreparedAgent {
            argv,
            env,
            cwd: ws.worktree_path.clone(),
            options: options.clone(),
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
        api_key: Option<&str>,
    ) -> Result<ListModelsResult, RpcError> {
        ensure_native_home(&d.dirs.root).map_err(|error| {
            RpcError::new(
                ErrorCode::IoError,
                format!("OpenCode config could not be refreshed: {error}"),
            )
        })?;
        let mut argv = host_argv()?;
        argv.push("models".into());
        let host = d.host().await?;
        let mut child = host
            .handle
            .spawn(SandboxCommand {
                argv,
                env: process_env(&d.dirs.root, api_key),
                cwd: Some(host.home.clone()),
                pty: None,
            })
            .await?;
        struct Kill(Box<dyn Fn() + Send + Sync>);
        impl Drop for Kill {
            fn drop(&mut self) {
                (self.0)();
            }
        }
        let _kill = Kill(child.killer);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcError::internal("OpenCode stdout missing"))?;
        let drain = tokio::spawn(async move {
            if let Some(mut stderr) = child.stderr {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            }
        });
        let mut output = String::new();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            stdout
                .take(4 * 1024 * 1024)
                .read_to_string(&mut output)
                .await
                .map_err(|e| RpcError::io(&e))?;
            if child.exit.await.unwrap_or(-1) != 0 {
                return Err(RpcError::new(
                    ErrorCode::AgentError,
                    "OpenCode model discovery failed; check provider configuration",
                ));
            }
            Ok(ListModelsResult {
                models: output
                    .lines()
                    .map(str::trim)
                    .filter(|line| {
                        !line.is_empty()
                            && line.contains('/')
                            && !line.chars().any(char::is_whitespace)
                    })
                    .map(|id| ModelInfo {
                        id: id.into(),
                        display_name: id.into(),
                        created_at: String::new(),
                    })
                    .collect(),
            })
        })
        .await
        .unwrap_or_else(|_| {
            Err(RpcError::new(
                ErrorCode::AgentError,
                "OpenCode model discovery timed out",
            ))
        });
        drain.abort();
        result
    }
}
