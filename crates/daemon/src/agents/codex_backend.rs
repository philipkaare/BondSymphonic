//! Codex installation, process-only credentials, and uncached model discovery.
use super::{
    adapter::AgentAdapter,
    backend::{Backend, PreparedAgent},
    codex::{CodexAdapter, TESTED_CODEX_VERSION},
    codex_rpc::{error, CodexRpc},
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
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub struct CodexBackend;
pub fn codex_home(root: &Path) -> PathBuf {
    root.join("codex-home")
}
pub fn has_login(home: &Path) -> bool {
    let Ok(bytes) = std::fs::read(home.join("auth.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    value["tokens"]["access_token"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
}

pub fn host_argv() -> Result<Vec<String>, RpcError> {
    if let Ok(value) = std::env::var("BS_CODEX_BIN") {
        let args = shell_words::split(&value)
            .map_err(|_| RpcError::invalid_params("invalid BS_CODEX_BIN"))?;
        if !args.is_empty() {
            return Ok(args);
        }
    }
    let path = crate::setup::host_home().join(".local/bin/codex");
    if path.is_file() {
        Ok(vec![path.display().to_string()])
    } else {
        Err(RpcError::new(
            ErrorCode::PrereqMissing,
            "Install Codex in Settings or Setup",
        ))
    }
}

fn helper_path(binary: &Path) -> PathBuf {
    std::env::var_os("BS_CODEX_CODE_MODE_HOST")
        .map(PathBuf::from)
        .unwrap_or_else(|| binary.with_file_name("codex-code-mode-host"))
}

pub fn process_config(root: &Path, api_key: Option<&str>) -> (Vec<String>, Vec<(String, String)>) {
    let mut args = Vec::new();
    let mut env = vec![("CODEX_HOME".into(), codex_home(root).display().to_string())];
    // Persist ChatGPT credentials in the directory the companion binds, not
    // in a platform keyring that is absent in its namespace.
    args.extend(["-c".into(), "cli_auth_credentials_store=\"file\"".into()]);
    if let Some(key) = api_key.filter(|s| !s.trim().is_empty()) {
        env.push(("OPENAI_API_KEY".into(), key.into()));
        for config in [
            "model_provider=\"bondsymphonic_api\"",
            "model_providers.bondsymphonic_api.name=\"OpenAI API\"",
            "model_providers.bondsymphonic_api.base_url=\"https://api.openai.com/v1\"",
            "model_providers.bondsymphonic_api.env_key=\"OPENAI_API_KEY\"",
            "model_providers.bondsymphonic_api.wire_api=\"responses\"",
            "model_providers.bondsymphonic_api.requires_openai_auth=false",
        ] {
            args.extend(["-c".into(), config.into()]);
        }
    }
    (args, env)
}

fn require_auth(root: &Path, key: Option<&str>) -> Result<(), RpcError> {
    if key.is_some_and(|s| !s.trim().is_empty()) || has_login(&codex_home(root)) {
        Ok(())
    } else {
        Err(RpcError::new(
            ErrorCode::PrereqMissing,
            "Sign in to Codex or configure an OpenAI API key",
        )
        .with_data(json!({"reason":"codex_auth_failed","adapter":"codex"})))
    }
}

pub async fn prerequisites(root: &Path) -> Vec<PrereqStatus> {
    let available = CodexBackend.binary().is_some();
    let auth = has_login(&codex_home(root));
    vec![
        PrereqStatus {
            name: "codex".into(),
            ok: available,
            detail: if available {
                format!("Codex installed; verified version {TESTED_CODEX_VERSION}")
            } else {
                "Codex and matching Code Mode helper required".into()
            },
            fix_hint: (!available).then(|| "Install Codex".into()),
        },
        PrereqStatus {
            name: "codex_auth".into(),
            ok: auth,
            detail: if auth {
                "ChatGPT sign-in present"
            } else {
                "No Codex sign-in; an OpenAI API key may be configured in Settings"
            }
            .into(),
            fix_hint: (!auth).then(|| "Sign in to Codex or configure an OpenAI API key".into()),
        },
    ]
}

#[async_trait]
impl Backend for CodexBackend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor {
            id: AgentAdapterKind::Codex,
            label: "Codex".into(),
            permission_modes: vec![
                BackendChoice {
                    id: "never".into(),
                    label: "YOLO".into(),
                },
                BackendChoice {
                    id: "on-request".into(),
                    label: "Ask when Codex wants to".into(),
                },
            ],
            default_permission_mode: "never".into(),
            permission_note:
                "Codex runs inside its own sandbox. Asking before every command is unavailable."
                    .into(),
            credential_label: "OpenAI API key".into(),
            prerequisite_names: vec!["codex".into(), "codex_auth".into()],
            setup_actions: vec![
                SetupAction::InstallCodex,
                SetupAction::CodexLogin,
                SetupAction::CodexLogout,
            ],
        }
    }
    fn binary(&self) -> Option<PathBuf> {
        let args = host_argv().ok()?;
        let binary = PathBuf::from(&args[0]);
        if std::env::var_os("BS_CODEX_BIN").is_some()
            || (binary.is_file() && helper_path(&binary).is_file())
        {
            Some(binary)
        } else {
            None
        }
    }
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        self.binary()
            .map(|binary| {
                vec![
                    (
                        helper_path(&binary),
                        PathBuf::from("/opt/bs/codex-code-mode-host"),
                    ),
                    (binary, PathBuf::from("/opt/bs/codex")),
                ]
            })
            .unwrap_or_default()
    }
    fn extra_hosts(&self) -> Vec<String> {
        ["api.openai.com", "auth.openai.com", "chatgpt.com"]
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
        let mode = options.permission_mode.as_deref().unwrap_or("never");
        if !["never", "on-request"].contains(&mode) {
            return Err(RpcError::invalid_params(
                "Codex permission mode must be never or on-request",
            ));
        }
        require_auth(&d.dirs.root, options.api_key.as_deref())?;
        let host = host_argv()?;
        let probe = tokio::process::Command::new(&host[0])
            .args(&host[1..])
            .arg("--version")
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(Duration::from_secs(10), probe)
            .await
            .map_err(|_| error("Codex version probe timed out"))?
            .map_err(|_| RpcError::new(ErrorCode::PrereqMissing, "Cannot run Codex"))?;
        if !output.status.success() {
            return Err(RpcError::new(
                ErrorCode::PrereqMissing,
                "Codex version probe failed",
            ));
        }
        let version = String::from_utf8_lossy(&output.stdout);
        if !version.contains(TESTED_CODEX_VERSION) {
            tracing::warn!("Codex differs from tested version {TESTED_CODEX_VERSION}");
        }
        let mut argv = if d.backend.name() == "linux_bwrap" {
            if !helper_path(Path::new(&host[0])).is_file() {
                return Err(RpcError::new(
                    ErrorCode::PrereqMissing,
                    "Install the matching Codex Code Mode helper",
                ));
            }
            vec!["/opt/bs/codex".into()]
        } else {
            host
        };
        let (config, env) = process_config(&d.dirs.root, options.api_key.as_deref());
        argv.extend(config);
        argv.extend(["app-server".into(), "--listen".into(), "stdio://".into()]);
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
        Box::new(CodexAdapter::new(sink, handle, prepared))
    }
    async fn list_models(
        &self,
        d: &Daemon,
        api_key: Option<&str>,
    ) -> Result<ListModelsResult, RpcError> {
        require_auth(&d.dirs.root, api_key)?;
        let mut argv = host_argv()?;
        let (config, env) = process_config(&d.dirs.root, api_key);
        argv.extend(config);
        argv.extend(["app-server".into(), "--listen".into(), "stdio://".into()]);
        let host = d.host().await?;
        list_models_process(
            &host.handle,
            SandboxCommand {
                argv,
                env,
                cwd: Some(host.home.clone()),
                pty: None,
            },
        )
        .await
    }
}

/// Shared by discovery and fake-process tests; no cache may cross accounts.
pub async fn list_models_process(
    handle: &Arc<dyn SandboxHandle>,
    command: SandboxCommand,
) -> Result<ListModelsResult, RpcError> {
    let secrets = command
        .env
        .iter()
        .filter(|(k, _)| k.contains("KEY"))
        .map(|(_, v)| v.clone())
        .collect();
    let mut child = handle.spawn(command).await?;
    struct Kill(Box<dyn Fn() + Send + Sync>);
    impl Drop for Kill {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    let _kill = Kill(child.killer);
    let (rpc, mut events) = CodexRpc::connect(
        child
            .stdout
            .take()
            .ok_or_else(|| error("Codex has no stdout"))?,
        child
            .stdin
            .take()
            .ok_or_else(|| error("Codex has no stdin"))?,
        secrets,
        Duration::from_secs(20),
    );
    let events = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let stderr = tokio::spawn(async move {
        if let Some(mut stderr) = child.stderr.take() {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        }
    });
    let result=tokio::time::timeout(Duration::from_secs(30),async {
        rpc.call("initialize",json!({"clientInfo":{"name":"bondsymphonic-models","version":env!("CARGO_PKG_VERSION")}})).await?;
        rpc.notify("initialized",json!({})).await?;
        let mut cursor=Value::Null; let mut seen=HashSet::new(); let mut models=Vec::new();
        loop {
            let page=rpc.call("model/list",json!({"cursor":cursor,"limit":100})).await?;
            let data=page["data"].as_array().ok_or_else(||error("Codex returned an invalid model list"))?;
            for model in data {if let Some(id)=model["id"].as_str(){models.push(ModelInfo{id:id.into(),display_name:model["displayName"].as_str().unwrap_or(id).into(),created_at:String::new()});}}
            cursor=page["nextCursor"].clone();
            if cursor.is_null(){break;}
            if !seen.insert(cursor.to_string())||seen.len()>100 {return Err(error("Codex model pagination did not finish"));}
        }
        Ok(ListModelsResult{models})
    }).await.unwrap_or_else(|_|Err(error("Codex model discovery timed out")));
    rpc.close().await;
    events.abort();
    stderr.abort();
    result
}
