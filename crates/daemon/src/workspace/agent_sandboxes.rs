//! Companion sandboxes share project files, never backend homes or proxies.
use super::{
    in_place::{InPlaceLayout, ProtectedSnapshot},
    lifecycle, Workspace,
};
use crate::{
    agents::backend::Backend,
    daemon::Daemon,
    net::{
        allowlist::{Allowlist, Origin},
        proxy::ProxyRegistry,
    },
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
        AgentAdapterKind::OpenCode => "opencode",
        AgentAdapterKind::Terminal => "terminal",
    }
}

/// A companion sandbox's runtime directory, `/run/bs` inside it.
fn companion_run_dir(root: &std::path::Path, kind: AgentAdapterKind, ws: &WorkspaceId) -> PathBuf {
    root.join("agent-run")
        .join(backend_name(kind))
        .join(ws.as_str())
}

/// The runtime directory of the sandbox an agent of `kind` in `ws` runs in:
/// the workspace sandbox's for Claude, its own companion's otherwise. This is
/// where that agent's MCP server sockets and env files go.
pub fn agent_run_dir(d: &Daemon, ws: &WorkspaceId, kind: AgentAdapterKind) -> PathBuf {
    match kind {
        AgentAdapterKind::Claude => d.dirs.run(ws),
        _ => companion_run_dir(&d.dirs.root, kind, ws),
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
    spec.run_dir = companion_run_dir(&d.dirs.root, kind, &ws.id);
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
    if kind == AgentAdapterKind::OpenCode {
        // The host's OpenCode home is what host-run `opencode` loads config
        // and plugins from: the sandbox gets its config read-only and nothing
        // else of it. Sessions and the copied login are the workspace's own.
        let config = crate::agents::opencode_backend::opencode_home(&d.dirs.root).join("config");
        spec.ro_binds.push((config.clone(), config));
        let state = crate::agents::opencode_backend::workspace_state(&d.dirs.root, &ws.id);
        spec.rw_binds.push((state.clone(), state));
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

/// The proxies for one backend's companion sandboxes, which say whose they are
/// on every denial so the IDE can show it in that backend's transcript.
pub fn companion_proxy(kind: AgentAdapterKind) -> ProxyRegistry {
    ProxyRegistry::for_backend(kind)
}

pub fn companion_allowlist(ws: &Workspace, extra: &[String]) -> Allowlist {
    let mut hosts = ws.allowlist.clone();
    hosts.extend_from_slice(extra);
    Allowlist::from_strings(&hosts).with_blocks(&ws.blocklist)
}

/// The exact origins a companion is granted beyond its allowlist: the model
/// servers named in OpenCode's mirrored config, for OpenCode, and nothing for
/// any other backend. Read afresh at each start, after `prepare` refreshed the
/// mirror, so an edit to the config is what the next agent gets.
pub fn companion_grants(d: &Daemon, kind: AgentAdapterKind) -> Vec<Origin> {
    if kind != AgentAdapterKind::OpenCode {
        return Vec::new();
    }
    crate::agents::opencode_config::provider_origins(
        &crate::agents::opencode_backend::opencode_home(&d.dirs.root).join("config"),
    )
}

/// Where inside the sandbox each loopback grant is listened for, so a client
/// that dials it there -- directly, as `NO_PROXY` tells it to -- is carried
/// to the host's loopback instead. `localhost` is both families' loopback.
/// An address is claimed once, by the first grant naming it, and never the
/// proxy shim's own.
pub fn tunnel_listens(grants: &[Origin]) -> Vec<(Origin, Vec<String>)> {
    let mut claimed: Vec<String> = vec![lifecycle::PROXY_LISTEN.to_string()];
    let mut out = Vec::new();
    for origin in grants.iter().filter(|o| o.is_loopback()) {
        let port = origin.port();
        let addrs = if origin.host() == "localhost" {
            vec![format!("127.0.0.1:{port}"), format!("[::1]:{port}")]
        } else {
            vec![origin.authority()]
        };
        let addrs: Vec<String> = addrs
            .into_iter()
            .filter(|addr| {
                let free = !claimed.contains(addr);
                if !free {
                    tracing::warn!(endpoint = %origin.authority(), "{addr} is already taken in the sandbox; not tunnelled");
                }
                free
            })
            .collect();
        if addrs.is_empty() {
            continue;
        }
        claimed.extend(addrs.iter().cloned());
        out.push((origin.clone(), addrs));
    }
    out
}

struct Entry {
    handle: Arc<dyn SandboxHandle>,
    base: Arc<dyn SandboxHandle>,
    proxy: Arc<ProxyRegistry>,
    generation: u64,
    extra_hosts: Vec<String>,
    grants: Vec<Origin>,
    /// The running tunnel for each loopback grant, by what ends it.
    tunnels: HashMap<Origin, Box<dyn Fn() + Send + Sync>>,
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
        let grants = companion_grants(d, kind);
        if let Some(entry) = state.live.get_mut(&key) {
            if Arc::ptr_eq(&entry.base, &base)
                && !entry
                    .handle
                    .died()
                    .is_some_and(|died| *died.borrow() || died.has_changed().is_err())
            {
                // The config may have changed since this sandbox started: an
                // origin no longer named is refused from the next connection
                // on, and a new one is reachable before the agent starts.
                if entry.grants != grants {
                    entry.proxy.set_allowlist(
                        &ws.id,
                        companion_allowlist(ws, &entry.extra_hosts).with_origins(grants.clone()),
                    );
                    entry.grants = grants;
                    if d.backend.name() == "linux_bwrap" {
                        sync_tunnels(&ws.id, &entry.handle, &entry.grants, &mut entry.tunnels)
                            .await;
                    }
                }
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
        let opencode = crate::agents::opencode_backend::opencode_home(&d.dirs.root);
        for (host, _) in spec.rw_binds.iter().chain(&spec.ro_binds) {
            if host.starts_with(d.dirs.root.join("agent-caches"))
                || host.starts_with(d.dirs.root.join("agent-xdg"))
                || host == &d.dirs.root.join("codex-home")
                || host == &opencode.join("config")
            {
                tokio::fs::create_dir_all(host)
                    .await
                    .map_err(|e| RpcError::io(&e))?;
            }
        }
        let proxy = state
            .proxies
            .entry(kind)
            .or_insert_with(|| Arc::new(companion_proxy(kind)))
            .clone();
        let extra = backend.extra_hosts();
        let generation = proxy
            .start(
                &ws.id,
                &spec.run_dir.join(lifecycle::PROXY_SOCKET_FILE),
                companion_allowlist(ws, &extra).with_origins(grants.clone()),
                d.events.clone(),
            )
            .await?;
        // The companion's own socket, in its own run directory: inside, it is
        // the same `/run/bs/mcp.sock` the workspace sandbox has.
        if let Err(e) = d.mcp.start(
            d.weak(),
            &ws.id,
            &spec.run_dir.join(crate::mcp::SOCKET_FILE),
            crate::mcp::registry::ListenerKind::Tools,
        ) {
            tracing::warn!(ws = %ws.id, "agent tools unavailable: {}", e.message);
        }
        let mut startup = Startup {
            proxy: proxy.clone(),
            id: ws.id.clone(),
            generation,
            handle: None,
            committed: false,
        };
        let handle = d.backend.start(&spec).await?;
        startup.handle = Some(handle.clone());
        let mut tunnels = HashMap::new();
        if d.backend.name() == "linux_bwrap" {
            start_shim(&ws.id, &handle).await?;
            sync_tunnels(&ws.id, &handle, &grants, &mut tunnels).await;
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
                grants,
                tunnels,
            },
        );
        startup.committed = true;
        self.watch(
            d.weak(),
            spec.run_dir.clone(),
            key,
            handle.clone(),
            base,
            protection.filter(|_| d.backend.name() == "linux_bwrap"),
        );
        Ok(handle)
    }

    /// Ends the companion when it or its workspace sandbox dies, or it
    /// breaches the protected snapshot: its proxy, its process, and the MCP
    /// listeners in `run_dir` (the git tools' and the user's servers'), so no
    /// socket outlives the sandbox it was for. The next start makes them anew.
    fn watch(
        &self,
        daemon: std::sync::Weak<Daemon>,
        run_dir: PathBuf,
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
                        // Under the lock, like `ensure`'s start of the tools
                        // listener, so a replacement's cannot be stopped here.
                        // The server sockets are started earlier, in the
                        // backend's `prepare`: one started by an agent start
                        // racing this death goes too, and that agent finds its
                        // user servers unreachable until its next start.
                        if let Some(d) = daemon.upgrade() {
                            d.mcp.stop_dir(&run_dir);
                        }
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
                entry.proxy.set_allowlist(
                    &ws.id,
                    companion_allowlist(ws, &entry.extra_hosts).with_origins(entry.grants.clone()),
                );
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

/// Brings the sandbox's tunnels in line with `grants`: ends the tunnel of
/// every loopback origin no longer granted, and starts one for each new one.
/// A tunnel that will not start is logged and left out; the agent then fails
/// to connect to that server, which says more than refusing the start would.
async fn sync_tunnels(
    id: &WorkspaceId,
    handle: &Arc<dyn SandboxHandle>,
    grants: &[Origin],
    tunnels: &mut HashMap<Origin, Box<dyn Fn() + Send + Sync>>,
) {
    let wanted = tunnel_listens(grants);
    tunnels.retain(|origin, kill| {
        let keep = wanted.iter().any(|(o, _)| o == origin);
        if !keep {
            kill();
        }
        keep
    });
    for (origin, listen) in wanted {
        if tunnels.contains_key(&origin) {
            continue;
        }
        match start_tunnel(id, handle, &origin, &listen).await {
            Ok(kill) => {
                tunnels.insert(origin, kill);
            }
            Err(e) => {
                tracing::warn!(ws = %id, endpoint = %origin.authority(), "endpoint tunnel did not start: {}", e.message)
            }
        }
    }
}

async fn start_tunnel(
    id: &WorkspaceId,
    handle: &Arc<dyn SandboxHandle>,
    origin: &Origin,
    listen: &[String],
) -> Result<Box<dyn Fn() + Send + Sync>, RpcError> {
    let mut argv = vec![
        handle.helper_exe().display().to_string(),
        "proxy-tunnel".into(),
        "--socket".into(),
        lifecycle::PROXY_SOCKET_IN_SANDBOX.into(),
        "--target".into(),
        origin.authority(),
    ];
    for addr in listen {
        argv.extend(["--listen".into(), addr.clone()]);
    }
    let mut child = handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        (child.killer)();
        return Err(RpcError::internal("endpoint tunnel has no pipes"));
    };
    let mut helper =
        crate::util::ready_line::Helper::new("proxy-tunnel", id.as_str(), stdout, stderr);
    if !helper
        .ready(crate::net::shim::TUNNEL_READY_LINE, Duration::from_secs(10))
        .await
    {
        (child.killer)();
        helper.drain().await;
        return Err(RpcError::internal("endpoint tunnel did not become ready"));
    }
    tokio::spawn(async move {
        helper.drain().await;
        let _ = child.exit.await;
    });
    Ok(child.killer)
}
