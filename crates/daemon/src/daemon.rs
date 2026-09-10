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
use std::path::PathBuf;
use std::sync::Arc;

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
        info.agents = self.agents.agents_of(&ws.id);
        info.runs = self.runs.runs_of(&ws.id);
        info
    }

    pub fn emit_state(&self, ws: &Workspace) {
        self.events.publish(
            Some(ws.id.clone()),
            Event::WorkspaceStateChanged {
                info: self.workspace_info(ws),
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

    /// On startup: put the agents from the last run back, then validate every
    /// registered workspace and restart its sandbox.
    ///
    /// The agents come first because a workspace's state event carries its agent
    /// list: restoring them afterwards would publish a workspace with no agents
    /// and then never correct it.
    pub async fn restore(self: &Arc<Self>) {
        let workspaces = self.registry.list();
        let known: Vec<WorkspaceId> = workspaces.iter().map(|w| w.id.clone()).collect();
        self.agents.restore(&known);
        for ws in workspaces {
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
