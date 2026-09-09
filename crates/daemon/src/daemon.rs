//! The daemon's shared state: data directories, the workspace registry, the
//! sandbox backend and the live sandbox per workspace.
//!
//! Everything here is cheap to clone or shared behind an `Arc`, so request
//! handlers can run concurrently against one `Daemon`.

use crate::agents::AgentManager;
use crate::fs_watch::Watchers;
use crate::git::Git;
use crate::pty::PtyManager;
use crate::sandbox::{SandboxBackend, SandboxHandle};
use crate::server::broadcast::EventBus;
use crate::workspace::registry::Registry;
use crate::workspace::{lifecycle, DataDirs, Workspace};
use bondsymphonic_proto::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

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
}

impl Daemon {
    pub fn new(
        dirs: DataDirs,
        backend: Arc<dyn SandboxBackend>,
        events: EventBus,
    ) -> anyhow::Result<Arc<Self>> {
        dirs.ensure()?;
        let registry = Registry::load(&dirs.registry_file())?;
        let agents = AgentManager::new(events.clone(), dirs.transcripts.clone());
        Ok(Arc::new(Self {
            dirs,
            registry,
            git: Git::new(),
            backend,
            sandboxes: Mutex::new(HashMap::new()),
            events: events.clone(),
            ptys: PtyManager::new(events),
            watchers: Watchers::default(),
            agents,
        }))
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

    /// A workspace as clients see it. `Workspace::info` cannot fill `agents`:
    /// the registry is a file that outlives the daemon and the agents are live
    /// processes, so the list comes from the manager here instead. Every path
    /// that hands a `WorkspaceInfo` to a client goes through this.
    pub fn workspace_info(&self, ws: &Workspace) -> WorkspaceInfo {
        let mut info = ws.info();
        info.agents = self.agents.agents_of(&ws.id);
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

    /// On startup: validate every registered workspace and restart its sandbox.
    pub async fn restore(self: &Arc<Self>) {
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
