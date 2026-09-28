use crate::ids::*;
use crate::types::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request's parameter struct.
///
/// Fields may carry attributes, which is how an optional field added after a
/// method shipped gets its `#[serde(default)]`: an older client that never
/// learned to send it must keep working against a newer daemon.
macro_rules! params {
    ($name:ident { $($(#[$attr:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct $name { $($(#[$attr])* pub $field: $ty),* }
    };
}

params!(HelloParams {
    token: String,
    client_version: String,
    /// The wire protocol the client speaks ([`crate::PROTOCOL_VERSION`]).
    ///
    /// `None` is a client built before the field existed, which speaks version
    /// 1; [`crate::peer_protocol_version`] is the one place that is decided.
    #[serde(default)]
    protocol_version: Option<u32>
});
params!(SetupPtyParams {
    action: SetupAction,
    cols: u16,
    rows: u16
});
params!(RepoPathParams { path: String });
params!(WorkspaceCreateParams {
    repo_path: String,
    base_branch: String,
    name: String,
    /// Initialise `repo_path` as a git repository when it is not one already,
    /// creating the directory if it is missing.
    ///
    /// Off by default, and deliberately so: a client that does not know about
    /// this field is a client whose user was never shown that a folder is about
    /// to become a repository, and the daemon must not make that decision for
    /// them. Where it is set, the IDE has said so in the New Agent dialog.
    #[serde(default)]
    init_if_missing: bool,
    /// Work directly in the checkout at `repo_path` instead of a new worktree.
    /// `base_branch` is then ignored and may be empty. Off by default, and the
    /// protocol version went to 2 with it: an older daemon would ignore the
    /// flag and quietly make a worktree.
    #[serde(default)]
    in_place: bool
});
params!(WorkspaceIdParams {
    workspace_id: WorkspaceId
});
params!(WorkspaceDestroyParams {
    workspace_id: WorkspaceId,
    force: bool
});
params!(WorkspaceDiffParams {
    workspace_id: WorkspaceId,
    path: String
});
params!(WorkspaceMergeParams { workspace_id: WorkspaceId, mode: MergeMode, message: Option<String> });
params!(WorkspaceCreatePrParams {
    workspace_id: WorkspaceId,
    title: String,
    body: String,
    draft: bool
});
params!(WorkspaceSetAllowlistParams { workspace_id: WorkspaceId, hosts: Vec<String> });
params!(FsPathParams {
    workspace_id: WorkspaceId,
    path: String
});
params!(FsWriteParams {
    workspace_id: WorkspaceId,
    path: String,
    content: String
});
params!(FsWatchParams {
    workspace_id: WorkspaceId,
    enable: bool
});
params!(AgentStartParams {
    workspace_id: WorkspaceId,
    adapter: AgentAdapterKind,
    options: AgentStartOptions
});
params!(AgentSendParams {
    agent_id: AgentId,
    text: String
});
params!(AgentPermissionReplyParams { agent_id: AgentId, request_id: String, decision: PermissionDecision, updated_input: Option<Value>, message: Option<String> });
params!(AgentIdParams { agent_id: AgentId });
params!(PtyOpenParams { workspace_id: WorkspaceId, cols: u16, rows: u16, command: Option<String> });
params!(PtyWriteParams {
    pty_id: PtyId,
    data_b64: String
});
params!(PtyResizeParams {
    pty_id: PtyId,
    cols: u16,
    rows: u16
});
params!(PtyIdParams { pty_id: PtyId });
params!(RunStartParams {
    workspace_id: WorkspaceId,
    config_name: String,
    /// The port this start should use instead of the one the run configuration
    /// names, for a configuration whose port the daemon only guessed.
    ///
    /// `None` -- which is what an older client's request deserialises to -- is
    /// "use the configured port". A port is a property of the start, not of the
    /// configuration: nothing is written back to the repository's
    /// `bondsymphonic.toml`, and the IDE remembers the choice per workspace and
    /// configuration in its own state.
    #[serde(default)]
    port: Option<u16>
});
params!(RunIdParams { run_id: RunId });
// The one caller-supplied input to `system.list_models`: an API key to fetch
// with instead of whatever credential the daemon would otherwise choose.
// `#[serde(default)]` so a request missing the field -- there is no client
// built before this method existed, but every other params struct in this
// file defaults its optional fields the same way -- still deserialises.
params!(ListModelsParams {
    #[serde(default)]
    adapter: Option<AgentAdapterKind>,
    #[serde(default)]
    api_key: Option<String>
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "hello")]
    Hello(HelloParams),
    #[serde(rename = "system.check_prereqs")]
    SystemCheckPrereqs {},
    #[serde(rename = "system.shutdown")]
    SystemShutdown {},
    #[serde(rename = "system.setup_pty")]
    SystemSetupPty(SetupPtyParams),
    #[serde(rename = "repo.inspect")]
    RepoInspect(RepoPathParams),
    #[serde(rename = "repo.detect_run_configs")]
    RepoDetectRunConfigs(RepoPathParams),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateParams),
    #[serde(rename = "workspace.list")]
    WorkspaceList {},
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceIdParams),
    #[serde(rename = "workspace.destroy")]
    WorkspaceDestroy(WorkspaceDestroyParams),
    #[serde(rename = "workspace.status")]
    WorkspaceStatus(WorkspaceIdParams),
    /// Brings a workspace whose sandbox is down, or that is in `Error`, back
    /// up: re-registers its worktree with the repository if the repository has
    /// forgotten it, then starts the sandbox again. Answers `WorkspaceInfo`.
    #[serde(rename = "workspace.restart")]
    WorkspaceRestart(WorkspaceIdParams),
    #[serde(rename = "workspace.changes")]
    WorkspaceChanges(WorkspaceIdParams),
    #[serde(rename = "workspace.diff")]
    WorkspaceDiff(WorkspaceDiffParams),
    #[serde(rename = "workspace.merge")]
    WorkspaceMerge(WorkspaceMergeParams),
    #[serde(rename = "workspace.create_pr")]
    WorkspaceCreatePr(WorkspaceCreatePrParams),
    #[serde(rename = "workspace.set_allowlist")]
    WorkspaceSetAllowlist(WorkspaceSetAllowlistParams),
    #[serde(rename = "fs.list_dir")]
    FsListDir(FsPathParams),
    #[serde(rename = "fs.read_file")]
    FsReadFile(FsPathParams),
    #[serde(rename = "fs.write_file")]
    FsWriteFile(FsWriteParams),
    #[serde(rename = "fs.watch")]
    FsWatch(FsWatchParams),
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    #[serde(rename = "agent.send")]
    AgentSend(AgentSendParams),
    #[serde(rename = "agent.permission_reply")]
    AgentPermissionReply(AgentPermissionReplyParams),
    #[serde(rename = "agent.interrupt")]
    AgentInterrupt(AgentIdParams),
    #[serde(rename = "agent.stop")]
    AgentStop(AgentIdParams),
    #[serde(rename = "agent.history")]
    AgentHistory(AgentIdParams),
    #[serde(rename = "pty.open")]
    PtyOpen(PtyOpenParams),
    #[serde(rename = "pty.write")]
    PtyWrite(PtyWriteParams),
    #[serde(rename = "pty.resize")]
    PtyResize(PtyResizeParams),
    #[serde(rename = "pty.close")]
    PtyClose(PtyIdParams),
    #[serde(rename = "run.start")]
    RunStart(RunStartParams),
    #[serde(rename = "run.stop")]
    RunStop(RunIdParams),
    #[serde(rename = "run.list")]
    RunList(WorkspaceIdParams),
    /// The newest model per family, for the model dropdown: the daemon fetches
    /// `GET /v1/models` with whichever of the caller's Claude credentials it
    /// has, newest first as Anthropic returns them. Additive -- an old daemon
    /// answers it with the same "not implemented" every unknown method gets --
    /// so it does not move [`crate::PROTOCOL_VERSION`].
    #[serde(rename = "system.list_models")]
    SystemListModels(ListModelsParams),
}

impl Request {
    pub fn method_name(&self) -> &'static str {
        use Request::*;
        match self {
            Hello(_) => "hello",
            SystemCheckPrereqs {} => "system.check_prereqs",
            SystemShutdown {} => "system.shutdown",
            SystemSetupPty(_) => "system.setup_pty",
            RepoInspect(_) => "repo.inspect",
            RepoDetectRunConfigs(_) => "repo.detect_run_configs",
            WorkspaceCreate(_) => "workspace.create",
            WorkspaceList {} => "workspace.list",
            WorkspaceGet(_) => "workspace.get",
            WorkspaceDestroy(_) => "workspace.destroy",
            WorkspaceStatus(_) => "workspace.status",
            WorkspaceRestart(_) => "workspace.restart",
            WorkspaceChanges(_) => "workspace.changes",
            WorkspaceDiff(_) => "workspace.diff",
            WorkspaceMerge(_) => "workspace.merge",
            WorkspaceCreatePr(_) => "workspace.create_pr",
            WorkspaceSetAllowlist(_) => "workspace.set_allowlist",
            FsListDir(_) => "fs.list_dir",
            FsReadFile(_) => "fs.read_file",
            FsWriteFile(_) => "fs.write_file",
            FsWatch(_) => "fs.watch",
            AgentStart(_) => "agent.start",
            AgentSend(_) => "agent.send",
            AgentPermissionReply(_) => "agent.permission_reply",
            AgentInterrupt(_) => "agent.interrupt",
            AgentStop(_) => "agent.stop",
            AgentHistory(_) => "agent.history",
            PtyOpen(_) => "pty.open",
            PtyWrite(_) => "pty.write",
            PtyResize(_) => "pty.resize",
            PtyClose(_) => "pty.close",
            RunStart(_) => "run.start",
            RunStop(_) => "run.stop",
            RunList(_) => "run.list",
            SystemListModels(_) => "system.list_models",
        }
    }

    /// One example value per variant, used by round-trip tests. Keep in sync when adding variants.
    pub fn examples() -> Vec<Request> {
        use Request::*;
        let ws: WorkspaceId = "ws_1".into();
        let ag: AgentId = "ag_1".into();
        let pty: PtyId = "pty_1".into();
        let run: RunId = "run_1".into();
        vec![
            Hello(HelloParams {
                token: "t".into(),
                client_version: "0.1.0".into(),
                protocol_version: Some(crate::PROTOCOL_VERSION),
            }),
            SystemCheckPrereqs {},
            SystemShutdown {},
            SystemSetupPty(SetupPtyParams {
                action: SetupAction::GhLogin,
                cols: 80,
                rows: 24,
            }),
            SystemSetupPty(SetupPtyParams {
                action: SetupAction::ClaudeSetupToken,
                cols: 80,
                rows: 24,
            }),
            RepoInspect(RepoPathParams { path: "/r".into() }),
            RepoDetectRunConfigs(RepoPathParams { path: "/r".into() }),
            WorkspaceCreate(WorkspaceCreateParams {
                repo_path: "/r".into(),
                base_branch: "main".into(),
                name: "a".into(),
                init_if_missing: true,
                in_place: true,
            }),
            WorkspaceList {},
            WorkspaceGet(WorkspaceIdParams {
                workspace_id: ws.clone(),
            }),
            WorkspaceDestroy(WorkspaceDestroyParams {
                workspace_id: ws.clone(),
                force: true,
            }),
            WorkspaceStatus(WorkspaceIdParams {
                workspace_id: ws.clone(),
            }),
            WorkspaceChanges(WorkspaceIdParams {
                workspace_id: ws.clone(),
            }),
            WorkspaceDiff(WorkspaceDiffParams {
                workspace_id: ws.clone(),
                path: "a.rs".into(),
            }),
            WorkspaceMerge(WorkspaceMergeParams {
                workspace_id: ws.clone(),
                mode: MergeMode::Squash,
                message: Some("m".into()),
            }),
            WorkspaceCreatePr(WorkspaceCreatePrParams {
                workspace_id: ws.clone(),
                title: "t".into(),
                body: "b".into(),
                draft: false,
            }),
            WorkspaceSetAllowlist(WorkspaceSetAllowlistParams {
                workspace_id: ws.clone(),
                hosts: vec!["*.x.org".into()],
            }),
            FsListDir(FsPathParams {
                workspace_id: ws.clone(),
                path: "src".into(),
            }),
            FsReadFile(FsPathParams {
                workspace_id: ws.clone(),
                path: "a.rs".into(),
            }),
            FsWriteFile(FsWriteParams {
                workspace_id: ws.clone(),
                path: "a.rs".into(),
                content: "x".into(),
            }),
            FsWatch(FsWatchParams {
                workspace_id: ws.clone(),
                enable: true,
            }),
            AgentStart(AgentStartParams {
                workspace_id: ws.clone(),
                adapter: AgentAdapterKind::Claude,
                options: AgentStartOptions {
                    command: None,
                    resume_session: None,
                    model: None,
                    permission_mode: None,
                    api_key: None,
                },
            }),
            AgentSend(AgentSendParams {
                agent_id: ag.clone(),
                text: "hi".into(),
            }),
            AgentPermissionReply(AgentPermissionReplyParams {
                agent_id: ag.clone(),
                request_id: "r1".into(),
                decision: PermissionDecision::Allow,
                updated_input: None,
                message: None,
            }),
            AgentInterrupt(AgentIdParams {
                agent_id: ag.clone(),
            }),
            AgentStop(AgentIdParams {
                agent_id: ag.clone(),
            }),
            AgentHistory(AgentIdParams {
                agent_id: ag.clone(),
            }),
            PtyOpen(PtyOpenParams {
                workspace_id: ws.clone(),
                cols: 80,
                rows: 24,
                command: None,
            }),
            PtyWrite(PtyWriteParams {
                pty_id: pty.clone(),
                data_b64: "bHM=".into(),
            }),
            PtyResize(PtyResizeParams {
                pty_id: pty.clone(),
                cols: 100,
                rows: 30,
            }),
            PtyClose(PtyIdParams {
                pty_id: pty.clone(),
            }),
            RunStart(RunStartParams {
                workspace_id: ws.clone(),
                config_name: "web".into(),
                port: Some(5173),
            }),
            RunStop(RunIdParams {
                run_id: run.clone(),
            }),
            RunList(WorkspaceIdParams {
                workspace_id: ws.clone(),
            }),
            SystemListModels(ListModelsParams {
                adapter: None,
                api_key: None,
            }),
        ]
    }
}

// ---- results ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub daemon_version: String,
    pub capabilities: Capabilities,
    /// The wire protocol the daemon speaks ([`crate::PROTOCOL_VERSION`]).
    ///
    /// `None` is a daemon built before the field existed, which speaks version
    /// 1; [`crate::peer_protocol_version`] is the one place that is decided.
    #[serde(default)]
    pub protocol_version: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckPrereqsResult {
    pub items: Vec<PrereqStatus>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectRunConfigsResult {
    pub configs: Vec<RunConfig>,
    /// The hosts the repository's own `bondsymphonic.toml` `[network] allow`
    /// adds to the workspace's allowlist, exactly as written there.
    ///
    /// A repository the user has not read extends what its agent may reach the
    /// moment the workspace is created, so the New Agent dialog says so before
    /// the user clicks Create. `serde(default)` because a daemon that predates
    /// the field simply adds nothing.
    #[serde(default)]
    pub network_allow: Vec<String>,
    /// One line per part of the repository's `bondsymphonic.toml` that could
    /// not be used, so a mistake in it is reported rather than silently
    /// dropping the entry it appears in.
    ///
    /// `serde(default)` because a daemon that predates the field warns about
    /// nothing, which is not the same as a broken answer.
    #[serde(default)]
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<WorkspaceInfo>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatusResult {
    pub entries: Vec<GitStatusEntry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangesResult {
    pub files: Vec<ChangedFile>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffResult {
    pub base_text: String,
    pub work_text: String,
    /// Either side was cut at the daemon's read cap (4 MiB), so the diff is of
    /// the beginning of the file rather than of the file. Distinct from the
    /// IDE's own alignment budget, which also runs out on large inputs but
    /// leaves both texts whole.
    ///
    /// `serde(default)` so a reply from a daemon that predates the field still
    /// deserialises, as "not truncated".
    #[serde(default)]
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeResult {
    pub ok: bool,
    /// Repo-relative paths git left with conflict markers, taken before the
    /// merge was aborted. Empty unless `ok` is false.
    pub conflicts: Vec<String>,
    /// Why `ok` is false, as a stable machine-readable tag the IDE can branch
    /// on rather than matching on prose. `"conflict"` today.
    ///
    /// A merge the daemon refuses outright — a dirty base checkout — is an
    /// `RpcError` with `data.reason` instead, because no merge was attempted
    /// and there is no result to report.
    ///
    /// `serde(default)` so a reply from a daemon that predates the field still
    /// deserialises, as "no reason given".
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatePrResult {
    pub url: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListDirResult {
    pub entries: Vec<FileEntry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub content: String,
    pub encoding: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStartResult {
    pub agent_id: AgentId,
}
/// What `agent.history` answers: the transcript, and the agent's state as of
/// the moment the transcript was read.
///
/// The state travels with the history because state changes are events, not
/// transcript entries: a client attaching to an agent that is already running
/// has missed every one of them, and without this it would start from `Idle`
/// and paint a live agent as finished -- or leave a permission bar up over a
/// request that has already been answered. Both fields default, so a daemon
/// that predates them still deserialises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResult {
    pub messages: Vec<AgentMessage>,
    #[serde(default = "idle_state")]
    pub state: AgentState,
    #[serde(default)]
    pub detail: Option<String>,
}

fn idle_state() -> AgentState {
    AgentState::Idle
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtyOpenResult {
    pub pty_id: PtyId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStartResult {
    pub run_id: RunId,
    pub host_port: u16,
    pub url: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunListResult {
    pub runs: Vec<RunInfo>,
}
/// What `system.list_models` answers: every model the Models API reported,
/// exactly in the order it reported them -- newest first, per family, which is
/// the order the model dropdown wants and no daemon-side sort is needed to get.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListModelsResult {
    pub models: Vec<ModelInfo>,
}
/// For methods with no meaningful result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Empty {}
