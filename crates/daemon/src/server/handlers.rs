//! The daemon's request handler: repo and workspace methods, with everything
//! else delegated to [`SystemHandler`].

use crate::daemon::Daemon;
use crate::server::dispatch::{ConnCtx, Handler, SystemHandler};
use crate::workspace::{changes, lifecycle};
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
            let value = self.system.handle(req, ctx).await?;
            let mut hello: HelloResult =
                serde_json::from_value(value).map_err(|e| RpcError::internal(e.to_string()))?;
            let backends = crate::agents::backend::known_backends();
            hello.capabilities.adapters = crate::agents::backend::runnable_adapters(&backends);
            hello.capabilities.backends = backends.iter().map(|b| b.descriptor()).collect();
            return ok(hello);
        }
        if !ctx.is_authenticated() {
            return Err(RpcError::unauthorized());
        }
        let d = &self.daemon;
        match req {
            // Overrides `SystemHandler`'s default (noop-backend) check: the daemon
            // knows which backend it actually started with.
            Request::SystemCheckPrereqs {} => {
                let mut items = crate::prereqs::check_all_with_backend(
                    d.backend.as_ref(),
                    Some(&crate::agents::token::token_path(&d.dirs.root)),
                )
                .await;
                items.extend(crate::agents::codex_backend::prerequisites(&d.dirs.root).await);
                ok(CheckPrereqsResult { items })
            }
            // The daemon fetches on the agent's own credentials (or the
            // caller's `api_key`), so this lives beside the other workspace
            // and agent methods rather than in `SystemHandler`, which has no
            // `Daemon` to read them from.
            Request::SystemListModels(p) => {
                let backend = crate::agents::backend::backend_for(
                    p.adapter.unwrap_or(AgentAdapterKind::Claude),
                )?;
                ok(backend.list_models(d, p.api_key.as_deref()).await?)
            }
            Request::RepoInspect(p) => {
                ok(crate::git::repo::inspect(&d.git, std::path::Path::new(&p.path)).await?)
            }
            // Detection walks a directory the user has just picked, on whatever
            // filesystem it lives on, so it goes to the blocking pool rather
            // than stalling every other request behind a slow stat.
            Request::RepoDetectRunConfigs(p) => {
                // The repository's `[network] allow` travels with the
                // configurations: creating a workspace from this repo extends
                // what its agent may reach, and the dialog that offers Create
                // is the last place a person can see that before it happens.
                let (runs, network_allow) = tokio::task::spawn_blocking(move || {
                    let root = std::path::Path::new(&p.path);
                    let runs = crate::runs::config::configs_for(root);
                    let allow = crate::runs::config::load_repo_config(root)
                        .ok()
                        .flatten()
                        .map(|c| c.network.allow)
                        .unwrap_or_default();
                    (runs, allow)
                })
                .await
                .map_err(|e| RpcError::internal(e.to_string()))?;
                ok(DetectRunConfigsResult {
                    configs: runs.configs,
                    network_allow,
                    // One line per `[[run]]` block that had to be dropped, or
                    // per file that would not parse. The IDE shows these beside
                    // the run list; nothing else in the daemon reads them.
                    warnings: runs.warnings,
                })
            }
            Request::WorkspaceCreate(p) => ok(lifecycle::create(d, p).await?),
            Request::WorkspaceList {} => ok(WorkspaceListResult {
                workspaces: d
                    .registry
                    .list()
                    .iter()
                    .map(|w| d.workspace_info(w))
                    .collect(),
            }),
            Request::WorkspaceGet(p) => ok(d.workspace_info(&d.workspace(&p.workspace_id)?)),
            Request::WorkspaceDestroy(p) => {
                ok(lifecycle::destroy(d, &p.workspace_id, p.force).await?)
            }
            Request::WorkspaceStatus(p) => ok(lifecycle::status(d, &p.workspace_id).await?),
            Request::WorkspaceRestart(p) => ok(lifecycle::restart(d, &p.workspace_id).await?),
            // Replaces the effective list outright rather than extending the
            // defaults: this is the user saying what the workspace may reach,
            // and taking a host away has to be possible.
            Request::WorkspaceSetAllowlist(p) => {
                ok(lifecycle::set_allowlist(d, &p.workspace_id, &p.hosts).await?)
            }
            Request::WorkspaceChanges(p) => ok(changes::changes(d, &p.workspace_id).await?),
            Request::WorkspaceDiff(p) => ok(changes::diff(d, &p.workspace_id, &p.path).await?),
            // Merging runs as the daemon in the main repository, never in the
            // sandbox: the branch it is folding in was written by an agent, and
            // the base checkout is the user's own.
            Request::WorkspaceMerge(p) => {
                ok(crate::git::merge::merge(d, &p.workspace_id, p.mode, p.message).await?)
            }
            // On the host, as the user, for the same reason a merge is: the
            // sandbox has no credentials and a read-only view of the refs.
            Request::WorkspaceFetch(p) => ok(crate::git::fetch::fetch(d, &p.workspace_id).await?),
            Request::WorkspaceCreatePr(p) => {
                ok(
                    crate::git::pr::create_pr(d, &p.workspace_id, &p.title, &p.body, p.draft)
                        .await?,
                )
            }
            Request::AgentStart(p) => ok(d.agents.start(d, p).await?),
            Request::AgentSend(p) => ok(d.agents.send(p).await?),
            Request::AgentPermissionReply(p) => ok(d.agents.permission_reply(p).await?),
            Request::AgentInterrupt(p) => ok(d.agents.interrupt(p).await?),
            Request::AgentStop(p) => ok(d.agents.stop(p).await?),
            Request::AgentHistory(p) => ok(d.agents.history(p).await?),
            Request::RunStart(p) => ok(d.runs.start(d, p).await?),
            Request::RunStop(p) => ok(d.runs.stop(&p.run_id).await?),
            Request::RunList(p) => ok(RunListResult {
                runs: d.runs.list(&p.workspace_id),
            }),
            // A setup terminal runs on the host rather than in a sandbox, so
            // it needs the daemon and cannot live in `SystemHandler` with the
            // other `system.*` methods.
            Request::SystemSetupPty(p) => {
                // For a logout this removes the long-lived token even if the
                // terminal below then fails to open. Acceptable: the user sees
                // that error and can press the button again, and removing the
                // token first is the order the spec (§4.4) sets: once a user
                // has asked to log out, no agent started afterwards is given it.
                crate::setup::before_setup(&d.dirs.root, p.action);
                // `claude setup-token` prints the long-lived token once and never
                // again, so its terminal's output is scanned on the way past and
                // the token stored where agents read it from.
                let tap = (p.action == SetupAction::ClaudeSetupToken).then(|| {
                    Box::new(crate::agents::token_scan::TokenCapture::new(
                        crate::agents::token::token_path(&d.dirs.root),
                    )) as Box<dyn crate::pty::OutputTap>
                });
                let opened = d
                    .ptys
                    .open_host_with_env(
                        d,
                        crate::setup::setup_argv(p.action),
                        crate::sandbox::PtySize {
                            cols: p.cols.max(2),
                            rows: p.rows.max(1),
                        },
                        tap,
                        crate::setup::setup_env(&d.dirs.root, p.action),
                    )
                    .await?;
                // A host terminal runs unsandboxed and belongs to the connection that
                // asked for it: when that connection goes, so does the terminal.
                d.ptys
                    .close_on(opened.pty_id.clone(), ctx.disconnected.clone());
                ok(opened)
            }
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
            Request::FsWatch(p) => {
                if p.enable {
                    let root = d.workspace(&p.workspace_id)?.worktree_path;
                    d.watchers
                        .enable(p.workspace_id.clone(), root, d.events.clone());
                } else {
                    d.watchers.disable(&p.workspace_id);
                }
                ok(Empty {})
            }
            other => self.system.handle(other, ctx).await,
        }
    }
}
