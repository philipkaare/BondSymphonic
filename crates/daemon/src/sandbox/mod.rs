//! Sandbox backends: the abstraction every workspace process goes through.
//!
//! A backend is started once per workspace ([`SandboxBackend::start`]) and hands
//! back a [`SandboxHandle`] that spawns processes inside that sandbox. The
//! `noop` backend runs processes unsandboxed and works on every platform, so
//! tests and non-Linux hosts have a working path.

pub mod noop;
pub mod protocol;

use async_trait::async_trait;
use bondsymphonic_proto::{PrereqStatus, RpcError, WorkspaceId};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

/// Everything a backend needs to build one workspace's sandbox.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub id: WorkspaceId,
    /// `(host, sandbox)` paths bound read-write.
    pub rw_binds: Vec<(PathBuf, PathBuf)>,
    /// `(host, sandbox)` paths bound read-only.
    pub ro_binds: Vec<(PathBuf, PathBuf)>,
    /// Host dir mounted at `/home/<user>` (bwrap) or used as `HOME` (noop).
    pub home: PathBuf,
    /// Host dir for sockets, mounted at `/run/bs` (bwrap).
    pub run_dir: PathBuf,
    /// Applied to every process started in this sandbox.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub pty: Option<PtySize>,
}

pub type ChildReader = Pin<Box<dyn AsyncRead + Send>>;
pub type ChildWriter = Pin<Box<dyn AsyncWrite + Send>>;

/// The terminal end of a child started with a PTY.
pub struct PtyIo {
    pub reader: ChildReader,
    pub writer: ChildWriter,
    pub resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync>,
}

pub struct SandboxChild {
    pub pid: u32,
    pub stdin: Option<ChildWriter>,
    pub stdout: Option<ChildReader>,
    pub stderr: Option<ChildReader>,
    pub pty: Option<PtyIo>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    /// Terminates the process (group).
    pub killer: Box<dyn Fn() + Send + Sync>,
}

#[async_trait]
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Vec<PrereqStatus>;
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError>;
}

#[async_trait]
pub trait SandboxHandle: Send + Sync {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError>;
    async fn shutdown(&self) -> Result<(), RpcError>;
}

/// Picks a backend by name. Unknown or unsupported names fall back to `noop`
/// with a warning.
pub fn backend_for(name: &str) -> Arc<dyn SandboxBackend> {
    match name {
        "noop" => Arc::new(noop::NoopBackend),
        other => {
            tracing::warn!(
                backend = other,
                "unknown or unsupported sandbox backend; using noop"
            );
            Arc::new(noop::NoopBackend)
        }
    }
}

pub fn sandbox_error(msg: impl Into<String>) -> RpcError {
    RpcError::new(bondsymphonic_proto::ErrorCode::SandboxError, msg)
}
