//! The daemon's shared state: data directories, the workspace registry, the
//! sandbox backend and the live sandbox per workspace.
//!
//! Everything here is cheap to clone or shared behind an `Arc`, so request
//! handlers can run concurrently against one `Daemon`.

use crate::agents::AgentManager;
use crate::fs_watch::Watchers;
use crate::git::Git;
use crate::pty::PtyManager;
use crate::sandbox::{SandboxBackend, SandboxHandle, SandboxSpec};
use crate::server::broadcast::EventBus;
use crate::workspace::registry::Registry;
use crate::workspace::{lifecycle, DataDirs, Workspace};
use bondsymphonic_proto::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One daemon per data directory, enforced with an advisory lock on
/// `<data_dir>/daemon.lock`.
///
/// Two daemons on one data directory is not a slow start, it is two owners of
/// one registry, one agents file and one set of worktrees: each rewrites the
/// other's state, each restores the other's workspaces into sandboxes of its
/// own, and the second one's `workspace.destroy` deletes objects the first
/// one's merge is copying out. The IDE's launcher restarts the daemon whenever
/// it exits, and a user who starts a second IDE — or whose first one is still
/// shutting down — is exactly how the second daemon gets started.
///
/// The lock is *advisory* and taken on an open file, which is what makes it
/// self-healing: a daemon that is killed, or whose machine loses power, drops
/// it when the operating system closes the handle. Nothing has to be cleaned up
/// by hand, and a `daemon.lock` left on disk means nothing on its own.
///
/// The handle is held for the daemon's whole life and released when the process
/// ends. It is deliberately not stored in [`Daemon`]: it belongs to the process,
/// not to the shared state, and a test that builds a `Daemon` over a temporary
/// directory must not have to take a lock to do it.
pub struct InstanceLock {
    /// Kept only to hold the lock open; the lock is the file descriptor.
    _file: std::fs::File,
}

/// Why a data directory could not be claimed.
#[derive(Debug)]
pub enum InstanceLockError {
    /// Another live daemon holds it.
    Busy(PathBuf),
    /// The lock file itself could not be opened or locked.
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for InstanceLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(dir) => write!(f, "another bondsymphonic-daemon owns {}", dir.display()),
            Self::Io(path, e) => write!(f, "cannot lock {}: {e}", path.display()),
        }
    }
}

impl std::error::Error for InstanceLockError {}

/// The exit status a daemon that lost the race for its data directory ends with.
///
/// Its own code, not the 1 every other startup failure uses: the IDE's launcher
/// restarts a daemon that exits, and "someone else already owns this" is the one
/// failure that restarting cannot fix.
pub const BUSY_EXIT_CODE: i32 = 2;

impl InstanceLock {
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("daemon.lock")
    }

    /// Claims `data_dir` for this process, or says who has it.
    ///
    /// The directory is created first: this runs before anything else in the
    /// data directory is touched, so on a first start there is nothing there yet.
    pub fn acquire(data_dir: &Path) -> Result<Self, InstanceLockError> {
        let path = Self::path_in(data_dir);
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return Err(InstanceLockError::Io(path, e));
        }
        lock_exclusive(&path).map_err(|e| match e {
            LockFailure::Busy => InstanceLockError::Busy(data_dir.to_path_buf()),
            LockFailure::Io(e) => InstanceLockError::Io(path, e),
        })
    }
}

enum LockFailure {
    Busy,
    Io(std::io::Error),
}

/// Opens `path` and takes an exclusive lock on it without waiting.
///
/// Two different mechanisms, because the platforms have nothing in common here
/// and neither needs a dependency:
///
/// * **Unix:** `flock(LOCK_EX | LOCK_NB)`. The lock belongs to the open file
///   description, so it survives `exec` and is dropped by the kernel when the
///   last descriptor for it closes — including when the process is killed.
/// * **Windows:** the file is opened with a share mode of zero, which is the
///   platform's own way of saying "only this handle". A second opener gets
///   `ERROR_SHARING_VIOLATION`, and the claim ends when the handle closes.
#[cfg(unix)]
fn lock_exclusive(path: &Path) -> Result<InstanceLock, LockFailure> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(LockFailure::Io)?;
    // SAFETY: `file` is open for the whole call and `flock` only takes a file
    // descriptor and a flag word.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(InstanceLock { _file: file });
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN => Err(LockFailure::Busy),
        _ => Err(LockFailure::Io(e)),
    }
}

#[cfg(windows)]
fn lock_exclusive(path: &Path) -> Result<InstanceLock, LockFailure> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `ERROR_SHARING_VIOLATION`: someone else has the file open. A share mode
    /// of zero refuses *every* other handle, so that someone is the daemon that
    /// got here first — or, for a moment, an antivirus scanner, an indexer or a
    /// backup agent that has the file open for reading. Those let go; a rival
    /// daemon does not. So the open is retried for a second before the
    /// violation is taken to mean `Busy`, an answer the launcher treats as
    /// final and never restarts from.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const RETRIES: u32 = 20;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    let mut attempt = 0;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => return Ok(InstanceLock { _file: file }),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                if attempt < RETRIES {
                    attempt += 1;
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
                return Err(LockFailure::Busy);
            }
            Err(e) => return Err(LockFailure::Io(e)),
        }
    }
}

/// The handle the setup terminals run under, with the home it was built with.
///
/// The home is captured here rather than recomputed per spawn so that a `$HOME`
/// that changes underneath the daemon cannot make a terminal's `HOME` and its
/// `PATH` prefix point at two different places.
pub struct HostHandle {
    pub handle: Arc<dyn SandboxHandle>,
    pub home: PathBuf,
}

pub struct Daemon {
    pub dirs: DataDirs,
    pub registry: Registry,
    pub git: Git,
    pub backend: Arc<dyn SandboxBackend>,
    pub sandboxes: Mutex<HashMap<WorkspaceId, Arc<dyn SandboxHandle>>>,
    pub events: EventBus,
    pub ptys: PtyManager,
    /// Live `fs.watch` subscriptions, one per workspace worktree.
    pub watchers: Watchers,
    /// The running agents, which is where `WorkspaceInfo.agents` comes from.
    pub agents: AgentManager,
    /// One allowlisting proxy per live workspace: the only route out of a
    /// sandbox, and the only place the allowlist is enforced.
    pub proxies: crate::net::proxy::ProxyRegistry,
    /// The running runs, which is where `WorkspaceInfo.runs` comes from.
    pub runs: crate::runs::manager::RunManager,
    /// The handle the setup terminals (`system.setup_pty`) run under: not a
    /// sandbox at all, but the daemon user's own home and environment. See
    /// [`Daemon::host`]. Built on first use, because most daemons never open a
    /// setup terminal and building it creates directories.
    pub host: tokio::sync::OnceCell<HostHandle>,
}

impl Daemon {
    pub fn new(
        dirs: DataDirs,
        backend: Arc<dyn SandboxBackend>,
        events: EventBus,
    ) -> anyhow::Result<Arc<Self>> {
        dirs.ensure()?;
        let registry = Registry::load(&dirs.registry_file())?;
        let agents =
            AgentManager::new(events.clone(), dirs.transcripts.clone(), dirs.agents_file());
        Ok(Arc::new(Self {
            dirs,
            registry,
            git: Git::new(),
            backend,
            sandboxes: Mutex::new(HashMap::new()),
            events: events.clone(),
            ptys: PtyManager::new(events.clone()),
            watchers: Watchers::default(),
            agents,
            proxies: crate::net::proxy::ProxyRegistry::default(),
            runs: crate::runs::manager::RunManager::new(events),
            host: tokio::sync::OnceCell::new(),
        }))
    }

    /// The host handle, built on first use and then shared.
    ///
    /// It is deliberately *not* `self.backend`: a setup command logs in or
    /// installs software, so it needs the daemon user's real home, the real
    /// network and the real filesystem — everything a workspace sandbox exists
    /// to take away. The no-sandbox backend gives exactly that, on every host,
    /// and going through a `SandboxHandle` at all is what lets the PTY manager
    /// treat a setup terminal like any other.
    ///
    /// `OnceCell::get_or_try_init` leaves the cell empty when the build fails,
    /// so a transient failure does not poison every later attempt.
    pub async fn host(&self) -> Result<&HostHandle, RpcError> {
        self.host
            .get_or_try_init(|| async {
                let home = crate::setup::host_home();
                let handle = crate::sandbox::backend_for(crate::setup::HOST_BACKEND)
                    .start(&SandboxSpec {
                        id: "host".into(),
                        rw_binds: vec![],
                        ro_binds: vec![],
                        late_ro_binds: vec![],
                        cwd: home.clone(),
                        home: home.clone(),
                        run_dir: self.dirs.run.join("host"),
                        env: vec![],
                    })
                    .await?;
                Ok(HostHandle { handle, home })
            })
            .await
    }

    /// Ends every setup terminal, so a half-finished login does not outlive the
    /// daemon that opened it. A daemon that never opened one has nothing to do.
    ///
    /// The terminals are closed through the PTY manager before the handle's own
    /// `shutdown`, because that shutdown is the same bare SIGHUP its killers
    /// send: only the manager holds the signaller that can escalate past a
    /// command which ignores it.
    pub async fn shutdown_host(&self) {
        let Some(host) = self.host.get() else {
            return;
        };
        self.ptys.close_host().await;
        let _ = host.handle.shutdown().await;
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Result<Workspace, RpcError> {
        self.registry
            .get(id)
            .ok_or_else(|| RpcError::not_found(format!("workspace {id}")))
    }

    pub fn sandbox(&self, id: &WorkspaceId) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        self.sandboxes.lock().get(id).cloned().ok_or_else(|| {
            RpcError::new(
                ErrorCode::SandboxError,
                format!("sandbox for {id} is not running"),
            )
        })
    }

    /// A workspace as clients see it. `Workspace::info` cannot fill `agents` or
    /// `runs`: the registry is a file that outlives the daemon while both of
    /// those are live processes, so the lists come from their managers here
    /// instead. Every path that hands a `WorkspaceInfo` to a client goes
    /// through this.
    pub fn workspace_info(&self, ws: &Workspace) -> WorkspaceInfo {
        let mut info = ws.info();
        info.agent_records = self.agents.records_of(&ws.id);
        // The same agents by id, for a client older than the records field.
        info.agents = info.agent_records.iter().map(|a| a.id.clone()).collect();
        info.runs = self.runs.runs_of(&ws.id);
        info
    }

    pub fn emit_state(&self, ws: &Workspace) {
        self.events.publish(
            Some(ws.id.clone()),
            Event::WorkspaceStateChanged {
                info: Box::new(self.workspace_info(ws)),
            },
        );
    }

    pub fn set_state(
        &self,
        id: &WorkspaceId,
        state: WorkspaceState,
    ) -> Result<Workspace, RpcError> {
        let ws = self.registry.update(id, |w| w.state = state)?;
        self.emit_state(&ws);
        Ok(ws)
    }

    /// Puts the agents from the last run of the daemon back.
    ///
    /// Split from [`restore_workspaces`](Self::restore_workspaces) and cheap --
    /// one file read -- because it has to be finished before anything else can
    /// touch the agent map, while restarting sandboxes takes seconds and must
    /// not hold the accept loop up. `main` runs this one to completion and
    /// spawns the other.
    pub fn restore_agents(self: &Arc<Self>) {
        let known: Vec<WorkspaceId> = self.registry.list().into_iter().map(|w| w.id).collect();
        self.agents.restore(&known);
    }

    /// On startup: put the agents back, then validate every registered
    /// workspace and restart its sandbox.
    ///
    /// The agents come first because a workspace's state event carries its agent
    /// list: restoring them afterwards would publish a workspace with no agents
    /// and then never correct it.
    pub async fn restore(self: &Arc<Self>) {
        self.restore_agents();
        self.restore_workspaces().await;
    }

    /// Validates every registered workspace and restarts its sandbox.
    pub async fn restore_workspaces(self: &Arc<Self>) {
        for ws in self.registry.list() {
            if !ws.worktree_path.exists() {
                let _ = self.set_state(
                    &ws.id,
                    WorkspaceState::Error("worktree directory is missing".into()),
                );
                continue;
            }
            match lifecycle::start_sandbox(self, &ws).await {
                Ok(()) => {
                    let _ = self.set_state(&ws.id, WorkspaceState::Ready);
                }
                Err(e) => {
                    tracing::warn!(ws = %ws.id, "sandbox restore failed: {e}");
                    let _ = self.set_state(&ws.id, WorkspaceState::SandboxDown);
                }
            }
        }
    }
}
