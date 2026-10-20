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

/// The host-owned OpenCode home: the mirrored config and the provider login
/// that `opencode auth login` writes. Host-run commands (model discovery,
/// sign-in) use it as their XDG home. A sandbox sees only its `config`, and
/// only read-only; nothing a sandbox writes ever lands in here.
pub fn opencode_home(root: &Path) -> PathBuf {
    root.join(super::opencode_config::HOST_HOME_DIR)
}

/// One workspace's OpenCode data, state and cache -- its sessions among them.
/// Bound read-write into that workspace's OpenCode sandbox at its own path, so
/// the same paths work with and without a sandbox and a restarted daemon finds
/// the sessions again. A sibling of the agent's home rather than a child: a
/// child of a writable home could be swapped for a link before the next bind.
/// Removed with the workspace.
pub fn workspace_state(root: &Path, ws: &WorkspaceId) -> PathBuf {
    root.join("agent-xdg/opencode").join(ws.as_str())
}

pub use super::opencode_config::seed_native_files;

/// Set by the IDE, in place of `BS_OPENCODE_CONFIG_ROOT`, when the Windows
/// user's OpenCode config folder cannot be reached from the distro. Its value
/// says why.
pub const UNMAPPED_CONFIG_ENV: &str = "BS_OPENCODE_CONFIG_UNMAPPED";

fn unmapped_config_error(reason: Option<&std::ffi::OsStr>) -> Option<std::io::Error> {
    let reason = reason.filter(|reason| !reason.is_empty())?;
    Some(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "The Windows OpenCode config folder cannot be reached from WSL: {}",
            reason.to_string_lossy()
        ),
    ))
}

pub fn ensure_native_home(root: &Path) -> std::io::Result<()> {
    // Not a fallback to the distro user's config: that is another user's.
    if let Some(error) = unmapped_config_error(std::env::var_os(UNMAPPED_CONFIG_ENV).as_deref()) {
        return Err(error);
    }
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

fn xdg_env(config: &Path, rest: &Path, api_key: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![("XDG_CONFIG_HOME".into(), config.display().to_string())];
    env.extend(
        [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
        ]
        .into_iter()
        .map(|(key, sub)| (key.into(), rest.join(sub).display().to_string())),
    );
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        env.push(("OPENAI_API_KEY".into(), key.into()));
    }
    env
}

/// The environment of an `opencode` the daemon runs on the host, outside
/// every sandbox. Project config is off: such a command starts in no
/// workspace, and must not load `.opencode` plugins from wherever it starts.
pub fn host_env(root: &Path, api_key: Option<&str>) -> Vec<(String, String)> {
    let home = opencode_home(root);
    let mut env = xdg_env(&home.join("config"), &home, api_key);
    env.push(("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()));
    env
}

/// The environment of a workspace's OpenCode agent: the host's config, the
/// workspace's own data, state and cache.
pub fn sandbox_env(root: &Path, ws: &WorkspaceId, api_key: Option<&str>) -> Vec<(String, String)> {
    xdg_env(
        &opencode_home(root).join("config"),
        &workspace_state(root, ws),
        api_key,
    )
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
            permission_note: "OpenCode uses its own sandbox, provider sign-in and configuration. Models use provider/model IDs. Configure local providers in opencode.json; no API key is required for unauthenticated local servers.".into(), credential_label: "OpenAI API key".into(), prerequisite_names: vec!["opencode".into(), "opencode_auth".into(), "opencode_config".into()], setup_actions: vec![SetupAction::InstallOpencode, SetupAction::OpencodeLogin, SetupAction::OpencodeLogout] }
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
            // First run downloads ripgrep from GitHub releases; the npm
            // registry is contacted as well (probe 2026-10-20).
            "github.com",
            "release-assets.githubusercontent.com",
            "registry.npmjs.org",
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
        super::opencode_config::copy_login(
            &opencode_home(&d.dirs.root),
            &workspace_state(&d.dirs.root, &ws.id),
        )
        .map_err(|error| {
            RpcError::new(
                ErrorCode::IoError,
                format!("OpenCode sign-in could not be given to the workspace: {error}"),
            )
        })?;
        let mut env = sandbox_env(&d.dirs.root, &ws.id, options.api_key.as_deref());
        if d.backend.name() == super::claude::SANDBOXED_BACKEND {
            env.push(("BS_OPENCODE_SANDBOX".into(), "1".into()));
        }
        let agent_name = format!("bondsymphonic_{}_{}", ws.id, crate::ids::new_id("launch_"));
        let native = if mode == "yolo" { "allow" } else { "ask" };
        env.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            // Native per-agent permissions are appended after global rules.
            // A fresh workspace-scoped name also avoids inherited rules on a
            // prior private agent. An agent's permission must be the wildcard
            // object: 1.14.48 rejects a bare string there and exits before
            // `initialize` (docs/superpowers/plans/notes/2026-10-20-opencode-probe.md).
            json!({
                "permission": native,
                "default_agent": agent_name,
                "agent": {agent_name: {
                    "mode":"primary", "hidden":false, "disable":false,
                    "description":"BondSymphonic workspace agent",
                    "permission": {"*": native},
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
                env: host_env(&d.dirs.root, api_key),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The IDE could not map the Windows OpenCode config folder into WSL, so
    /// it launched the daemon without `BS_OPENCODE_CONFIG_ROOT` and with the
    /// reason instead. The daemon must not fall back to the distro user's own
    /// config: the reason is the `opencode_config` prerequisite's detail.
    #[tokio::test]
    async fn an_unmapped_windows_config_is_reported_as_the_config_prerequisite() {
        let dir = tempfile::tempdir().unwrap();
        let reason = r"\server\private is a network path, which the distro cannot reach";
        let error = unmapped_config_error(Some(std::ffi::OsStr::new(reason)))
            .expect("a reason must be an error");
        assert!(error.to_string().contains(reason), "{error}");
        assert!(unmapped_config_error(None).is_none());
        assert!(unmapped_config_error(Some(std::ffi::OsStr::new(""))).is_none());

        std::env::set_var(UNMAPPED_CONFIG_ENV, reason);
        let checks = prerequisites(dir.path()).await;
        std::env::remove_var(UNMAPPED_CONFIG_ENV);
        let config = checks
            .iter()
            .find(|check| check.name == "opencode_config")
            .expect("the config prerequisite must be reported");
        assert!(!config.ok);
        assert!(config.detail.contains(reason), "{}", config.detail);
        assert!(OpenCodeBackend
            .descriptor()
            .prerequisite_names
            .contains(&"opencode_config".to_owned()));
    }
}
