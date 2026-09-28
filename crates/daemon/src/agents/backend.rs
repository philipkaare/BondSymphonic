//! CLI-specific preparation, separate from sandbox and process ownership.
use super::{adapter::AgentAdapter, claude_backend::ClaudeBackend, AgentSink};
use crate::{daemon::Daemon, sandbox::SandboxHandle, workspace::Workspace};
use async_trait::async_trait;
use bondsymphonic_proto::*;
use std::{path::PathBuf, sync::Arc};

pub struct PreparedAgent {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub options: AgentStartOptions,
}

impl std::fmt::Debug for PreparedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAgent")
            .field("cwd", &self.cwd)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn descriptor(&self) -> BackendDescriptor;
    fn binary(&self) -> Option<PathBuf>;
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)>;
    fn extra_hosts(&self) -> Vec<String>;
    async fn prepare(
        &self,
        d: &Daemon,
        ws: &Workspace,
        options: &AgentStartOptions,
    ) -> Result<PreparedAgent, RpcError>;
    fn adapter(
        &self,
        sink: AgentSink,
        handle: Arc<dyn SandboxHandle>,
        prepared: PreparedAgent,
    ) -> Box<dyn AgentAdapter>;
    async fn list_models(
        &self,
        d: &Daemon,
        api_key: Option<&str>,
    ) -> Result<ListModelsResult, RpcError>;
}

pub fn backend_for(kind: AgentAdapterKind) -> Result<Arc<dyn Backend>, RpcError> {
    match kind {
        AgentAdapterKind::Claude => Ok(Arc::new(ClaudeBackend)),
        AgentAdapterKind::Terminal => Err(RpcError::invalid_params("terminal agents use pty.open")),
        AgentAdapterKind::Codex => Err(RpcError::new(
            ErrorCode::PrereqMissing,
            "Codex backend is not installed",
        )),
    }
}

pub fn known_backends() -> Vec<Arc<dyn Backend>> {
    vec![Arc::new(ClaudeBackend)]
}

pub fn runnable_adapters(backends: &[Arc<dyn Backend>]) -> Vec<AgentAdapterKind> {
    backends
        .iter()
        .filter(|backend| backend.binary().is_some())
        .map(|backend| backend.descriptor().id)
        .chain(std::iter::once(AgentAdapterKind::Terminal))
        .collect()
}

pub fn descriptors() -> Vec<BackendDescriptor> {
    known_backends()
        .iter()
        .map(|backend| backend.descriptor())
        .collect()
}
