use crate::ids::*;
use crate::types::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! params {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct $name { $(pub $field: $ty),* }
    };
}

params!(HelloParams {
    token: String,
    client_version: String
});
params!(RepoPathParams { path: String });
params!(WorkspaceCreateParams {
    repo_path: String,
    base_branch: String,
    name: String
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
    config_name: String
});
params!(RunIdParams { run_id: RunId });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "hello")]
    Hello(HelloParams),
    #[serde(rename = "system.check_prereqs")]
    SystemCheckPrereqs {},
    #[serde(rename = "system.shutdown")]
    SystemShutdown {},
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
}

impl Request {
    pub fn method_name(&self) -> &'static str {
        use Request::*;
        match self {
            Hello(_) => "hello",
            SystemCheckPrereqs {} => "system.check_prereqs",
            SystemShutdown {} => "system.shutdown",
            RepoInspect(_) => "repo.inspect",
            RepoDetectRunConfigs(_) => "repo.detect_run_configs",
            WorkspaceCreate(_) => "workspace.create",
            WorkspaceList {} => "workspace.list",
            WorkspaceGet(_) => "workspace.get",
            WorkspaceDestroy(_) => "workspace.destroy",
            WorkspaceStatus(_) => "workspace.status",
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
            }),
            SystemCheckPrereqs {},
            SystemShutdown {},
            RepoInspect(RepoPathParams { path: "/r".into() }),
            RepoDetectRunConfigs(RepoPathParams { path: "/r".into() }),
            WorkspaceCreate(WorkspaceCreateParams {
                repo_path: "/r".into(),
                base_branch: "main".into(),
                name: "a".into(),
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
            }),
            RunStop(RunIdParams {
                run_id: run.clone(),
            }),
            RunList(WorkspaceIdParams {
                workspace_id: ws.clone(),
            }),
        ]
    }
}

// ---- results ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub daemon_version: String,
    pub capabilities: Capabilities,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckPrereqsResult {
    pub items: Vec<PrereqStatus>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectRunConfigsResult {
    pub configs: Vec<RunConfig>,
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
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeResult {
    pub ok: bool,
    pub conflicts: Vec<String>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResult {
    pub messages: Vec<AgentMessage>,
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
/// For methods with no meaningful result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Empty {}
