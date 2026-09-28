//! Companion sandboxes share project files, never backend homes or proxies.
use super::{
    in_place::{InPlaceLayout, ProtectedSnapshot},
    lifecycle, Workspace,
};
use crate::{
    agents::backend::Backend,
    daemon::Daemon,
    net::{allowlist::Allowlist, proxy::ProxyRegistry},
    sandbox::{SandboxCommand, SandboxHandle, SandboxSpec},
};
use bondsymphonic_proto::{AgentAdapterKind, RpcError, WorkspaceId, WorkspaceKind, WorkspaceState};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentSandboxKey {
    pub workspace_id: WorkspaceId,
    pub adapter: AgentAdapterKind,
}

fn backend_name(kind: AgentAdapterKind) -> &'static str {
    match kind {
        AgentAdapterKind::Claude => "claude",
        AgentAdapterKind::Codex => "codex",
        AgentAdapterKind::Terminal => "terminal",
    }
}

async fn build_spec(
    d: &Daemon,
    ws: &Workspace,
    backend: &dyn Backend,
) -> Result<(SandboxSpec, Option<ProtectedSnapshot>), RpcError> {
    let (mut spec, protection) = match ws.kind {
        WorkspaceKind::Worktree => (
            lifecycle::spec_for(d, ws, &lifecycle::layout_for(d, ws).await?),
            None,
        ),
        WorkspaceKind::InPlace => {
            let layout = InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks());
            let snapshot = layout.snapshot().map_err(|e| RpcError::io(&e))?;
            (
                lifecycle::in_place_spec_for(d, ws, &layout, &snapshot),
                Some(snapshot),
            )
        }
    };
    let kind = backend.descriptor().id;
    let name = backend_name(kind);
    spec.home = d
        .dirs
        .root
        .join("agent-homes")
        .join(name)
        .join(ws.id.as_str());
    spec.run_dir = d
        .dirs
        .root
        .join("agent-run")
        .join(name)
        .join(ws.id.as_str());
    // Siblings of the base home, never children of it: a child would be
    // readable through Claude's HOME mount.
    let cache = d
        .dirs
        .root
        .join("agent-caches")
        .join(name)
        .join(ws.id.as_str());
    for (host, _) in &mut spec.rw_binds {
        if *host == d.dirs.cache(&ws.id) {
            *host = cache.clone();
        }
    }
    spec.ro_binds
        .retain(|(_, target)| target != &PathBuf::from(crate::agents::claude::CLAUDE_IN_SANDBOX));
    if d.backend.name() == "linux_bwrap" {
        spec.ro_binds.extend(backend.ro_binds());
    }
    if kind == AgentAdapterKind::Codex {
        let home = d.dirs.root.join("codex-home");
        spec.rw_binds.push((home.clone(), home));
    }
    Ok((spec, protection))
}

pub async fn companion_spec(
    d: &Daemon,
    ws: &Workspace,
    backend: &dyn Backend,
) -> Result<SandboxSpec, RpcError> {
    Ok(build_spec(d, ws, backend).await?.0)
}

pub fn companion_allowlist(ws: &Workspace, extra: &[String]) -> Allowlist {
    let mut hosts = ws.allowlist.clone();
    hosts.extend_from_slice(extra);
    Allowlist::from_strings(&hosts)
}

struct Entry {
    handle: Arc<dyn SandboxHandle>,
    base: Arc<dyn SandboxHandle>,
    proxy: Arc<ProxyRegistry>,
    generation: u64,
    extra_hosts: Vec<String>,
}

#[derive(Default)]
struct State {
    live: HashMap<AgentSandboxKey, Entry>,
    proxies: HashMap<AgentAdapterKind, Arc<ProxyRegistry>>,
    stopped: HashMap<AgentSandboxKey, Arc<dyn SandboxHandle>>,
    shutdown: bool,
}

#[derive(Default, Clone)]
pub struct AgentSandboxes {
    inner: Arc<tokio::sync::Mutex<State>>,
}

// Cancellation during startup must release the proxy and any sandbox already
// created, even when the request future never reaches its error handler.
struct Startup {
    proxy: Arc<ProxyRegistry>,
    id: WorkspaceId,
    generation: u64,
    handle: Option<Arc<dyn SandboxHandle>>,
    committed: bool,
}
impl Drop for Startup {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.proxy.stop_generation(&self.id, self.generation);
        if let Some(handle) = self.handle.take() {
            tokio::spawn(async move {
                let _ = handle.shutdown().await;
            });
        }
    }
}

impl AgentSandboxes {
    pub async fn ensure(
        &self,
        d: &Daemon,
        ws: &Workspace,
        backend: &dyn Backend,
    ) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        let kind = backend.descriptor().id;
        if kind == AgentAdapterKind::Claude {
            return d.sandbox(&ws.id);
        }
        if kind == AgentAdapterKind::Terminal {
            return Err(RpcError::invalid_params("terminal agents use pty.open"));
        }
        let key = AgentSandboxKey {
            workspace_id: ws.id.clone(),
            adapter: kind,
        };
        // Serializes first starts and teardown. AgentManager also holds the
        // workspace lifecycle gate through process creation, so destruction
        // cannot remove project files beneath this startup.
        let mut state = self.inner.lock().await;
        let current_workspace = d.workspace(&ws.id)?;
        let ws = &current_workspace;
        if state.shutdown || d.workspace(&ws.id)?.state != WorkspaceState::Ready {
            return Err(RpcError::invalid_params("workspace is not ready"));
        }
        let base = d.sandbox(&ws.id)?;
        if state
            .stopped
            .get(&key)
            .is_some_and(|old| Arc::ptr_eq(old, &base))
        {
            return Err(RpcError::invalid_params("workspace sandbox is stopping"));
        }
        if let Some(entry) = state.live.get(&key) {
            if Arc::ptr_eq(&entry.base, &base)
                && !entry
                    .handle
                    .died()
                    .is_some_and(|died| *died.borrow() || died.has_changed().is_err())
            {
                return Ok(entry.handle.clone());
            }
        }
        if let Some(old) = state.live.remove(&key) {
            old.proxy.stop_generation(&ws.id, old.generation);
            let _ = old.handle.shutdown().await;
        }
        let (spec, protection) = build_spec(d, ws, backend).await?;
        for path in [&spec.home, &spec.run_dir] {
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|e| RpcError::io(&e))?;
        }
        for (host, _) in &spec.rw_binds {
            if host.starts_with(d.dirs.root.join("agent-caches"))
                || host == &d.dirs.root.join("codex-home")
            {
                tokio::fs::create_dir_all(host)
                    .await
                    .map_err(|e| RpcError::io(&e))?;
            }
        }
        let proxy = state.proxies.entry(kind).or_default().clone();
        let extra = backend.extra_hosts();
        let generation = proxy
            .start(
                &ws.id,
                &spec.run_dir.join(lifecycle::PROXY_SOCKET_FILE),
                companion_allowlist(ws, &extra),
                d.events.clone(),
            )
            .await?;
        let mut startup = Startup {
            proxy: proxy.clone(),
            id: ws.id.clone(),
            generation,
            handle: None,
            committed: false,
        };
        let handle = d.backend.start(&spec).await?;
        startup.handle = Some(handle.clone());
        if d.backend.name() == "linux_bwrap" {
            start_shim(&ws.id, &handle).await?;
        }
        let current = d.sandbox(&ws.id)?;
        if !Arc::ptr_eq(&base, &current) || d.workspace(&ws.id)?.state != WorkspaceState::Ready {
            return Err(RpcError::invalid_params(
                "workspace changed during agent sandbox startup",
            ));
        }
        state.stopped.remove(&key);
        state.live.insert(
            key.clone(),
            Entry {
                handle: handle.clone(),
                base: base.clone(),
                proxy,
                generation,
                extra_hosts: extra,
            },
        );
        startup.committed = true;
        self.watch(
            key,
            handle.clone(),
            base,
            protection.filter(|_| d.backend.name() == "linux_bwrap"),
        );
        Ok(handle)
    }

    fn watch(
        &self,
        key: AgentSandboxKey,
        handle: Arc<dyn SandboxHandle>,
        base: Arc<dyn SandboxHandle>,
        protection: Option<ProtectedSnapshot>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            let protection = protection.map(Arc::new);
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                {
                    let state = registry.inner.lock().await;
                    if !state
                        .live
                        .get(&key)
                        .is_some_and(|e| Arc::ptr_eq(&e.handle, &handle))
                    {
                        return;
                    }
                }
                let dead = handle
                    .died()
                    .is_some_and(|v| *v.borrow() || v.has_changed().is_err())
                    || base
                        .died()
                        .is_some_and(|v| *v.borrow() || v.has_changed().is_err());
                let breached = if let Some(snapshot) = protection.clone() {
                    let pid = handle.host_pid().unwrap_or(0);
                    !matches!(
                        tokio::task::spawn_blocking(move || snapshot.check(Some(pid))).await,
                        Ok(None)
                    )
                } else {
                    false
                };
                if dead || breached {
                    let mut state = registry.inner.lock().await;
                    if state
                        .live
                        .get(&key)
                        .is_some_and(|e| Arc::ptr_eq(&e.handle, &handle))
                    {
                        let entry = state.live.remove(&key).unwrap();
                        entry
                            .proxy
                            .stop_generation(&key.workspace_id, entry.generation);
                        drop(state);
                        let _ = handle.shutdown().await;
                    }
                    return;
                }
            }
        });
    }

    pub async fn set_allowlist(&self, ws: &Workspace) {
        let state = self.inner.lock().await;
        for (key, entry) in &state.live {
            if key.workspace_id == ws.id {
                entry
                    .proxy
                    .set_allowlist(&ws.id, companion_allowlist(ws, &entry.extra_hosts));
            }
        }
    }

    pub async fn stop_workspace(&self, id: &WorkspaceId) {
        let mut state = self.inner.lock().await;
        let keys: Vec<_> = state
            .live
            .keys()
            .filter(|key| &key.workspace_id == id)
            .cloned()
            .collect();
        for key in keys {
            let entry = state.live.remove(&key).unwrap();
            state.stopped.insert(key, entry.base);
            entry.proxy.stop_generation(id, entry.generation);
            let _ = entry.handle.shutdown().await;
        }
    }

    pub async fn shutdown(&self) {
        let mut state = self.inner.lock().await;
        state.shutdown = true;
        for (key, entry) in state.live.drain() {
            entry
                .proxy
                .stop_generation(&key.workspace_id, entry.generation);
            let _ = entry.handle.shutdown().await;
        }
    }
}

async fn start_shim(id: &WorkspaceId, handle: &Arc<dyn SandboxHandle>) -> Result<(), RpcError> {
    let mut child = handle
        .spawn(SandboxCommand {
            argv: vec![
                handle.helper_exe().display().to_string(),
                "proxy-shim".into(),
                "--socket".into(),
                lifecycle::PROXY_SOCKET_IN_SANDBOX.into(),
                "--listen".into(),
                lifecycle::PROXY_LISTEN.into(),
            ],
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        (child.killer)();
        return Err(RpcError::internal("backend proxy has no pipes"));
    };
    let mut helper =
        crate::util::ready_line::Helper::new("agent-proxy", id.as_str(), stdout, stderr);
    if !helper
        .ready(crate::net::shim::READY_LINE, Duration::from_secs(10))
        .await
    {
        (child.killer)();
        return Err(RpcError::internal("backend proxy did not become ready"));
    }
    let sandbox = handle.clone();
    tokio::spawn(async move {
        helper.drain().await;
        let _ = child.exit.await;
        let _ = sandbox.shutdown().await;
    });
    Ok(())
}
