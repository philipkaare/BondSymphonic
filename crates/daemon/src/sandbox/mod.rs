//! Sandbox backends: the abstraction every workspace process goes through.
//!
//! A backend is started once per workspace ([`SandboxBackend::start`]) and hands
//! back a [`SandboxHandle`] that spawns processes inside that sandbox. The
//! `noop` backend runs processes unsandboxed and works on every platform, so
//! tests and non-Linux hosts have a working path.

#[cfg(target_os = "linux")]
pub mod exec_client;
#[cfg(target_os = "linux")]
pub mod init;
#[cfg(target_os = "linux")]
pub mod linux_bwrap;
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
    /// `(host, sandbox)` paths bound read-only *after* the read-write binds.
    ///
    /// bwrap applies binds in order, so a read-write bind of a directory
    /// re-exposes anything already bound read-only inside it. These are for the
    /// individual files that must stay read-only within an otherwise writable
    /// tree, and they only mean anything to a backend that has mounts at all.
    pub late_ro_binds: Vec<(PathBuf, PathBuf)>,
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

/// Delivers a signal number to a child's process group. See
/// [`SandboxChild::signal`].
pub type Signaller = Box<dyn Fn(i32) + Send + Sync>;

pub struct SandboxChild {
    pub pid: u32,
    pub stdin: Option<ChildWriter>,
    pub stdout: Option<ChildReader>,
    pub stderr: Option<ChildReader>,
    pub pty: Option<PtyIo>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    /// Terminates the process (group).
    pub killer: Box<dyn Fn() + Send + Sync>,
    /// Delivers signal `n` to the process (group), for callers that need
    /// something other than termination — an interrupt, say. `killer` remains
    /// the way to end a child, because a backend may wrap termination in an
    /// escalation this cannot express.
    ///
    /// A backend on a platform without signals maps what it can: the noop
    /// backend on Windows terminates the child for SIGTERM and SIGKILL and
    /// ignores every other number. Signalling a child that has already exited
    /// does nothing, so a recycled pid is never hit.
    pub signal: Signaller,
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

    /// The daemon's own binary, at the path it can be executed from *inside*
    /// this sandbox.
    ///
    /// The daemon ships several helpers that run in the sandbox as subcommands
    /// of itself — the proxy shim, and whatever follows it — and only the
    /// backend knows where its binary ended up: bwrap binds it in at a fixed
    /// path when the real one is hidden by a tmpfs, and leaves it where it is
    /// otherwise. A caller that hardcodes either answer is right on one host and
    /// silently wrong on the next, so the decision is asked for rather than
    /// repeated.
    fn helper_exe(&self) -> PathBuf;

    /// A watch that flips to `true` when this sandbox stops being usable, so a
    /// workspace whose sandbox dies underneath it can be reported rather than
    /// left claiming to be `Ready` until the next `pty.open` fails.
    ///
    /// `None` from a backend whose sandbox has no independent life of its own:
    /// `noop` runs processes as plain children of the daemon, so there is
    /// nothing that can die separately.
    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        None
    }
}

/// Picks a backend by name. Unknown or unsupported names fall back to `noop`
/// with a warning.
pub fn backend_for(name: &str) -> Arc<dyn SandboxBackend> {
    match name {
        "noop" => Arc::new(noop::NoopBackend),
        #[cfg(target_os = "linux")]
        "linux_bwrap" => Arc::new(linux_bwrap::BwrapBackend::default()),
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
