//! The sandboxes' MCP listeners, each a Unix socket in a sandbox's run
//! directory and bound to its workspace for its whole life:
//!
//! - `mcp.sock` (`/run/bs/mcp.sock` inside) serves the built-in git tools.
//!   Every tool call re-reads the workspace, so a listener that outlives its
//!   sandbox for a moment can do nothing to a stopped or destroyed one.
//! - `mcp-<id>.sock` (`/run/bs/mcp-<id>.sock`) brokers one user-configured
//!   remote server; see [`crate::mcp::servers::broker`]. These are stopped as
//!   soon as their server is disabled, removed, logged out of or turned off
//!   for the workspace: see [`McpRegistry::reconcile`].

use crate::daemon::Daemon;
use crate::mcp::servers::McpServers;
use bondsymphonic_proto::{McpServerId, McpWorkspaceOverride, RpcError, WorkspaceId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

/// What a listener serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerKind {
    /// The built-in git tools (`mcp.sock`).
    Tools,
    /// The broker to one user-configured remote server (`mcp-<id>.sock`).
    Server(McpServerId),
}

struct Listener {
    workspace: WorkspaceId,
    kind: ListenerKind,
    cancel: CancellationToken,
    /// For a server listener: whether the server had an OAuth login when
    /// last looked at. A login that goes away stops the listener; one that
    /// never existed does not (its tools answer "not connected" until the
    /// user connects, and then just work).
    logged_in: bool,
}

/// The live listeners, keyed by socket path: a workspace has its own sandbox's
/// and one set per companion agent sandbox, each in its own run directory.
#[derive(Default)]
pub struct McpRegistry {
    listeners: parking_lot::Mutex<HashMap<PathBuf, Listener>>,
}

impl McpRegistry {
    /// Serves `kind` for `id` on `socket`, replacing whatever listened there
    /// before (a restarted sandbox reuses its run directory).
    ///
    /// The socket is `0600`: the sandbox runs as the daemon's user, and nobody
    /// else on the host has any business driving the workspace's git or the
    /// user's MCP servers.
    #[cfg(unix)]
    pub fn start(
        &self,
        daemon: Weak<Daemon>,
        id: &WorkspaceId,
        socket: &Path,
        kind: ListenerKind,
    ) -> Result<(), RpcError> {
        use std::os::unix::fs::PermissionsExt;

        let logged_in = match &kind {
            ListenerKind::Tools => false,
            ListenerKind::Server(server) => daemon
                .upgrade()
                .ok_or_else(|| RpcError::internal("daemon is shutting down"))?
                .mcp_servers
                .has_login(server),
        };
        // One lock across replace-and-bind, so two starts on the same path
        // cannot interleave and leave a listener nobody can stop.
        let mut listeners = self.listeners.lock();
        if let Some(old) = listeners.remove(socket) {
            old.cancel.cancel();
        }
        // A file left by a listener that is gone (or by a daemon that crashed)
        // would make the bind fail with "address in use".
        let _ = std::fs::remove_file(socket);
        let listener = tokio::net::UnixListener::bind(socket).map_err(|e| RpcError::io(&e))?;
        if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(socket);
            return Err(RpcError::io(&e));
        }
        let cancel = CancellationToken::new();
        listeners.insert(
            socket.to_path_buf(),
            Listener {
                workspace: id.clone(),
                kind: kind.clone(),
                cancel: cancel.clone(),
                logged_in,
            },
        );
        drop(listeners);
        let id = id.clone();
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((s, _)) => s,
                        Err(e) => {
                            tracing::warn!(ws = %id, "mcp accept failed: {e}");
                            // An accept that keeps failing (out of descriptors)
                            // would otherwise spin a core.
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    },
                };
                let Some(conn) = connection(&daemon, &id, &kind, stream) else {
                    break;
                };
                // Stopping the listener closes its open connections too:
                // dropping the serve future aborts its in-flight calls and
                // its writer.
                let conn_cancel = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = conn_cancel.cancelled() => {}
                        _ = conn => {}
                    }
                });
            }
        });
        Ok(())
    }

    /// No Unix sockets, no sandboxes: the daemon only sandboxes on Linux.
    #[cfg(not(unix))]
    pub fn start(
        &self,
        _daemon: Weak<Daemon>,
        _id: &WorkspaceId,
        _socket: &Path,
        _kind: ListenerKind,
    ) -> Result<(), RpcError> {
        Ok(())
    }

    /// Stops the listeners `which` selects, with their connections, and
    /// deletes their socket files.
    fn stop_where(&self, which: impl Fn(&Listener) -> bool) {
        let mut listeners = self.listeners.lock();
        let gone: Vec<PathBuf> = listeners
            .iter()
            .filter(|(_, l)| which(l))
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            if let Some(l) = listeners.remove(&path) {
                l.cancel.cancel();
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Stops every listener of `id` -- its sandbox's and its companion agent
    /// sandboxes', tools and servers alike -- with their connections, and
    /// deletes their socket files.
    pub fn stop_workspace(&self, id: &WorkspaceId) {
        self.stop_where(|l| &l.workspace == id);
    }

    /// Stops `server`'s listeners in every sandbox.
    pub fn stop_server(&self, server: &McpServerId) {
        self.stop_where(|l| matches!(&l.kind, ListenerKind::Server(s) if s == server));
    }

    /// Stops `server`'s listeners in `ws`'s sandboxes only.
    pub fn stop_server_in(&self, ws: &WorkspaceId, server: &McpServerId) {
        self.stop_where(|l| {
            &l.workspace == ws && matches!(&l.kind, ListenerKind::Server(s) if s == server)
        });
    }

    /// Brings the server listeners in line with the registry: stops those
    /// whose server was removed or disabled, is now off for the listener's
    /// workspace, or lost its login. Called on every registry and status
    /// change. Nothing is started here: a server that comes on reaches an
    /// agent at its next start, when its set of servers is resolved.
    pub fn reconcile(&self, servers: &McpServers) {
        let snapshot = servers.snapshot();
        let mut stop: Vec<(WorkspaceId, McpServerId)> = vec![];
        for listener in self.listeners.lock().values_mut() {
            let ListenerKind::Server(server) = &listener.kind else {
                continue;
            };
            let on = snapshot
                .servers
                .iter()
                .find(|s| &s.id == server)
                .is_some_and(|s| {
                    s.enabled
                        && match snapshot.workspace_override(&listener.workspace, server) {
                            McpWorkspaceOverride::Inherit => s.default_workspace_enabled,
                            McpWorkspaceOverride::Enabled => true,
                            McpWorkspaceOverride::Disabled => false,
                        }
                });
            let logged_in = servers.has_login(server);
            let logged_out = listener.logged_in && !logged_in;
            listener.logged_in = logged_in;
            if !on || logged_out {
                stop.push((listener.workspace.clone(), server.clone()));
            }
        }
        for (ws, server) in stop {
            tracing::info!(ws = %ws, server = %server.as_str(), "stopping MCP server socket");
            self.stop_server_in(&ws, &server);
        }
    }
}

/// The session for one accepted connection, or `None` once the daemon is
/// gone (the listener then stops accepting).
#[cfg(unix)]
fn connection(
    daemon: &Weak<Daemon>,
    id: &WorkspaceId,
    kind: &ListenerKind,
    stream: tokio::net::UnixStream,
) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
    use super::{protocol, servers::broker, tools::WorkspaceTools};
    use std::sync::Arc;

    let (r, w) = stream.into_split();
    Some(match kind {
        ListenerKind::Tools => {
            let host = Arc::new(WorkspaceTools::new(daemon.clone(), id.clone()));
            Box::pin(protocol::serve(r, w, host))
        }
        ListenerKind::Server(server) => {
            // The connection holds the registry, not the daemon.
            let servers = daemon.upgrade()?.mcp_servers.clone();
            Box::pin(broker::serve(servers, server.clone(), r, w))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::servers::auth::TOKEN_SECRET;
    use bondsymphonic_proto::{McpOverrideParams, McpSecretValue};

    fn registry_with(listeners: &[(&str, &str, ListenerKind)]) -> McpRegistry {
        let registry = McpRegistry::default();
        for (path, ws, kind) in listeners {
            registry.listeners.lock().insert(
                PathBuf::from(path),
                Listener {
                    workspace: (*ws).into(),
                    kind: kind.clone(),
                    cancel: CancellationToken::new(),
                    logged_in: false,
                },
            );
        }
        registry
    }

    fn paths(registry: &McpRegistry) -> Vec<String> {
        let mut paths: Vec<String> = registry
            .listeners
            .lock()
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths
    }

    /// A server that never had a login keeps its sockets (its tools answer
    /// "not connected"); losing a login it had stops them.
    #[test]
    fn reconcile_stops_on_lost_login_and_workspace_override_only() {
        let dir = tempfile::tempdir().unwrap();
        let servers = McpServers::load(dir.path()).unwrap();
        let jira: McpServerId = "jira".into();
        let registry = registry_with(&[
            ("a-tools", "a", ListenerKind::Tools),
            ("a-jira", "a", ListenerKind::Server(jira.clone())),
            ("b-jira", "b", ListenerKind::Server(jira.clone())),
        ]);
        registry.reconcile(&servers);
        assert_eq!(paths(&registry), ["a-jira", "a-tools", "b-jira"]);

        servers
            .set_secret(&jira, TOKEN_SECRET, Some(McpSecretValue("{}".into())))
            .unwrap();
        registry.reconcile(&servers);
        assert_eq!(paths(&registry), ["a-jira", "a-tools", "b-jira"]);

        servers
            .set_override(McpOverrideParams {
                server_id: jira.clone(),
                workspace_id: "b".into(),
                value: McpWorkspaceOverride::Disabled,
            })
            .unwrap();
        registry.reconcile(&servers);
        assert_eq!(paths(&registry), ["a-jira", "a-tools"]);

        servers.set_secret(&jira, TOKEN_SECRET, None).unwrap();
        registry.reconcile(&servers);
        assert_eq!(paths(&registry), ["a-tools"], "the tools socket stays");
    }

    #[test]
    fn removal_stops_every_workspace_and_stop_workspace_takes_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let servers = McpServers::load(dir.path()).unwrap();
        let jira: McpServerId = "jira".into();
        let registry = registry_with(&[
            ("a-tools", "a", ListenerKind::Tools),
            ("a-jira", "a", ListenerKind::Server(jira.clone())),
            ("b-tools", "b", ListenerKind::Tools),
            ("b-jira", "b", ListenerKind::Server(jira.clone())),
        ]);
        registry.stop_workspace(&"b".into());
        assert_eq!(paths(&registry), ["a-jira", "a-tools"]);
        servers.remove(&jira).unwrap();
        registry.reconcile(&servers);
        assert_eq!(paths(&registry), ["a-tools"]);
    }
}
