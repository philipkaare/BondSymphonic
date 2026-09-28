//! Claude's existing preparation and authentication behind Backend.
use super::{
    adapter::AgentAdapter,
    agent_auth_env,
    backend::{Backend, PreparedAgent},
    claude::{self, ClaudeAdapter},
    credentials, off_the_runtime, AgentSink,
};
use crate::{
    daemon::Daemon,
    sandbox::SandboxHandle,
    workspace::{lifecycle::log_phase, Workspace},
};
use async_trait::async_trait;
use bondsymphonic_proto::*;
use std::{path::PathBuf, sync::Arc, time::Instant};
use tracing::info;

pub struct ClaudeBackend;

#[async_trait]
impl Backend for ClaudeBackend {
    fn descriptor(&self) -> BackendDescriptor {
        BackendDescriptor {
            id: AgentAdapterKind::Claude, label: "Claude".into(),
            permission_modes: [("bypassPermissions", "YOLO (sandboxed)"), ("acceptEdits", "Accept edits (other tools blocked)"), ("plan", "Plan only"), ("manual", "Ask every time (blocks instead)")]
                .into_iter().map(|(id, label)| BackendChoice { id: id.into(), label: label.into() }).collect(),
            default_permission_mode: "bypassPermissions".into(),
            permission_note: "Claude Code cannot reach this window to ask, so a tool that needs approval is refused rather than queued. YOLO runs everything — the sandbox, the worktree and the network proxy are what make that reasonable.".into(),
            credential_label: "Anthropic API key".into(),
            prerequisite_names: vec!["claude".into(), "claude_auth".into()],
            setup_actions: vec![SetupAction::InstallClaude, SetupAction::ClaudeLogin, SetupAction::ClaudeLogout, SetupAction::ClaudeSetupToken],
        }
    }
    fn binary(&self) -> Option<PathBuf> {
        claude::host_claude_bin()
    }
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        claude::claude_ro_bind().into_iter().collect()
    }
    fn extra_hosts(&self) -> Vec<String> {
        vec![]
    }
    async fn prepare(
        &self,
        d: &Daemon,
        ws: &Workspace,
        options: &AgentStartOptions,
    ) -> Result<PreparedAgent, RpcError> {
        // The backend decides how the CLI is named: bound into the sandbox at a
        // fixed path under bwrap, and at its host path where there are no
        // mounts. Resolved before anything is spawned, so a missing install is
        // a `PrereqMissing` naming the path rather than an exec failure.
        let argv = claude::claude_argv(options, d.backend.name())?;
        // And before anything is written: a CLI that cannot even say its
        // version is one this workspace should not be prepared for.
        let phase = Instant::now();
        claude::probe_claude(&d.agents.probed, d.backend.name()).await?;
        log_phase(&ws.id, "agent.start", "probe_claude", phase);

        // Again at start, not only at creation: the user may have logged in
        // since this workspace was made, and a workspace that was created
        // logged out would otherwise stay that way forever.
        let phase = Instant::now();
        let home = d.dirs.home(&ws.id);
        // And before the seeding, from every workspace: one whose agent is
        // still running may have refreshed the tokens, and the copy it holds
        // is then the only working one -- seeding the host's stale copy here
        // would start this agent with a refresh token that is already dead.
        let homes = d.dirs.homes.clone();
        off_the_runtime("pulling a refreshed login back", move || {
            credentials::write_back_any_refreshed_login(&homes);
        })
        .await;
        let seeded = credentials::seed_claude_files(&home, &ws.worktree_path);
        if !seeded.is_empty() {
            info!(ws = %ws.id, files = ?seeded, "seeded claude credentials");
        }
        // After the seeding, because it overwrites what the seeding just put
        // there: a repository that pins its own settings is pinning the tools
        // the agent may use, and the daemon user's copy must not win.
        claude::apply_repo_settings(&ws.worktree_path, &home)?;
        log_phase(&ws.id, "agent.start", "seed_home", phase);

        // The key or token is given to this one command, never written into
        // the sandbox spec: the spec's environment reaches every process in
        // the workspace, including terminals the user opens. A key the IDE
        // sent is an explicit choice and wins; otherwise the long-lived token,
        // which never needs refreshing -- see `token.rs` for why that matters.
        let root = d.dirs.root.clone();
        let token = tokio::task::spawn_blocking(move || {
            crate::agents::token::read(&crate::agents::token::token_path(&root))
        })
        .await
        .unwrap_or(None);
        let env = agent_auth_env(options.api_key.as_deref(), token);

        Ok(PreparedAgent {
            argv,
            env,
            cwd: ws.worktree_path.clone(),
            options: options.clone(),
        })
    }
    fn adapter(
        &self,
        sink: AgentSink,
        handle: Arc<dyn SandboxHandle>,
        prepared: PreparedAgent,
    ) -> Box<dyn AgentAdapter> {
        Box::new(ClaudeAdapter::new(
            sink,
            handle,
            prepared.argv,
            prepared.env,
            prepared.cwd,
        ))
    }
    async fn list_models(
        &self,
        d: &Daemon,
        api_key: Option<&str>,
    ) -> Result<ListModelsResult, RpcError> {
        crate::models::list_models(d, api_key).await
    }
}
