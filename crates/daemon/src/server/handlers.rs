//! The daemon's request handler: repo and workspace methods, with everything
//! else delegated to [`SystemHandler`].

use crate::daemon::Daemon;
use crate::server::dispatch::{ConnCtx, Handler, SystemHandler};
use crate::workspace::lifecycle;
use async_trait::async_trait;
use bondsymphonic_proto::*;
use serde_json::Value;
use std::sync::Arc;

pub struct WorkspaceHandler {
    pub system: SystemHandler,
    pub daemon: Arc<Daemon>,
}

fn ok<T: serde::Serialize>(v: T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(e.to_string()))
}

#[async_trait]
impl Handler for WorkspaceHandler {
    async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<Value, RpcError> {
        // `hello` carries the token, so `SystemHandler` is the one that authenticates.
        if matches!(req, Request::Hello(_)) {
            return self.system.handle(req, ctx).await;
        }
        if !ctx.is_authenticated() {
            return Err(RpcError::unauthorized());
        }
        let d = &self.daemon;
        match req {
            // Overrides `SystemHandler`'s default (noop-backend) check: the daemon
            // knows which backend it actually started with.
            Request::SystemCheckPrereqs {} => ok(CheckPrereqsResult {
                items: crate::prereqs::check_all_with_backend(d.backend.as_ref()).await,
            }),
            Request::RepoInspect(p) => {
                ok(crate::git::repo::inspect(&d.git, std::path::Path::new(&p.path)).await?)
            }
            Request::WorkspaceCreate(p) => ok(lifecycle::create(d, p).await?),
            Request::WorkspaceList {} => ok(WorkspaceListResult {
                workspaces: d.registry.list().iter().map(|w| w.info()).collect(),
            }),
            Request::WorkspaceGet(p) => ok(d.workspace(&p.workspace_id)?.info()),
            Request::WorkspaceDestroy(p) => {
                ok(lifecycle::destroy(d, &p.workspace_id, p.force).await?)
            }
            Request::WorkspaceStatus(p) => ok(lifecycle::status(d, &p.workspace_id).await?),
            Request::PtyOpen(p) => ok(d.ptys.open(d, p).await?),
            Request::PtyWrite(p) => ok(d.ptys.write(p).await?),
            Request::PtyResize(p) => ok(d.ptys.resize(p).await?),
            Request::PtyClose(p) => ok(d.ptys.close(&p.pty_id).await?),
            Request::FsListDir(p) => {
                let root = d.workspace(&p.workspace_id)?.worktree_path;
                ok(
                    tokio::task::spawn_blocking(move || crate::fs::list_dir(&root, &p.path))
                        .await
                        .map_err(|e| RpcError::internal(e.to_string()))??,
                )
            }
            Request::FsReadFile(p) => {
                let root = d.workspace(&p.workspace_id)?.worktree_path;
                ok(
                    tokio::task::spawn_blocking(move || crate::fs::read_file(&root, &p.path))
                        .await
                        .map_err(|e| RpcError::internal(e.to_string()))??,
                )
            }
            Request::FsWriteFile(p) => {
                let root = d.workspace(&p.workspace_id)?.worktree_path;
                ok(tokio::task::spawn_blocking(move || {
                    crate::fs::write_file(&root, &p.path, &p.content)
                })
                .await
                .map_err(|e| RpcError::internal(e.to_string()))??)
            }
            // `fs.watch` stays not-implemented until Milestone 3.
            other => self.system.handle(other, ctx).await,
        }
    }
}
