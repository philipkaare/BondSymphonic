//! One MCP listener per sandbox, on `<run_dir>/mcp.sock`, which the sandbox
//! sees as `/run/bs/mcp.sock`. Each is bound to its workspace for its whole
//! life; every tool call re-reads the workspace, so a listener that outlives
//! its sandbox for a moment can do nothing to a stopped or destroyed one.

use crate::daemon::Daemon;
use bondsymphonic_proto::{RpcError, WorkspaceId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Weak;
use tokio_util::sync::CancellationToken;

struct Listener {
    workspace: WorkspaceId,
    cancel: CancellationToken,
}

/// The live listeners, keyed by socket path: a workspace has one for its own
/// sandbox and one per companion agent sandbox, each in its own run directory.
#[derive(Default)]
pub struct McpRegistry {
    listeners: parking_lot::Mutex<HashMap<PathBuf, Listener>>,
}

impl McpRegistry {
    /// Serves `id`'s tools on `socket`, replacing whatever listened there
    /// before (a restarted sandbox reuses its run directory).
    ///
    /// The socket is `0600`: the sandbox runs as the daemon's user, and nobody
    /// else on the host has any business driving the workspace's git.
    #[cfg(unix)]
    pub fn start(
        &self,
        daemon: Weak<Daemon>,
        id: &WorkspaceId,
        socket: &Path,
    ) -> Result<(), RpcError> {
        use super::{protocol, tools::WorkspaceTools};
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

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
                cancel: cancel.clone(),
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
                let (r, w) = stream.into_split();
                let host = Arc::new(WorkspaceTools::new(daemon.clone(), id.clone()));
                // Stopping the listener closes its open connections too:
                // dropping `serve` aborts its in-flight calls and its writer.
                let conn_cancel = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = conn_cancel.cancelled() => {}
                        _ = protocol::serve(r, w, host) => {}
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
    ) -> Result<(), RpcError> {
        Ok(())
    }

    /// Stops every listener of `id` -- its sandbox's and its companion agent
    /// sandboxes' -- with their connections, and deletes their socket files.
    pub fn stop_workspace(&self, id: &WorkspaceId) {
        let mut listeners = self.listeners.lock();
        let gone: Vec<PathBuf> = listeners
            .iter()
            .filter(|(_, l)| &l.workspace == id)
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            if let Some(l) = listeners.remove(&path) {
                l.cancel.cancel();
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}
