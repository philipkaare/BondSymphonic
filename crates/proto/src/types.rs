use crate::ids::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub sandbox_backend: String, // "linux_bwrap" | "noop"
    pub git_protect: bool,
    pub adapters: Vec<AgentAdapterKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdapterKind {
    Claude,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrereqStatus {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

/// The setup commands a client may ask the daemon to run in a host terminal,
/// one per prerequisite that a person has to fix interactively. Each names a
/// command the daemon already knows; the request never carries a command line.
///
/// That is the whole point of the enum. A setup terminal runs on the host,
/// outside every sandbox, with the daemon user's own home and network, so a
/// free-form command there would hand any client that got through `hello` a
/// shell on the developer's machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupAction {
    /// Log in to Claude, fixing the `claude_auth` prerequisite.
    ClaudeLogin,
    /// Log in to GitHub, fixing `gh_auth`.
    GhLogin,
    /// Install the Claude Code CLI, fixing `claude`.
    InstallClaude,
    /// Install the GitHub CLI, fixing `gh`.
    InstallGh,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub default_branch: String,
    pub branches: Vec<String>,
    pub is_dirty: bool,
    pub remotes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum WorkspaceState {
    Creating,
    Ready,
    SandboxDown,
    Error(String),
    Destroying,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub repo_path: String,
    pub base_branch: String,
    pub branch: String,
    pub worktree_path: String,
    pub created_at: String, // RFC 3339
    pub allowlist: Vec<String>,
    #[serde(flatten)]
    pub state: WorkspaceState,
    pub agents: Vec<AgentId>,
    pub runs: Vec<RunId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub path: String,
    pub status: FileStatus,
    pub staged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    Merge,
    Rebase,
    Squash,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub name: String,
    pub command: String,
    pub port: u16,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ready_regex: Option<String>,
    pub source: RunConfigSource,
    #[serde(default)]
    pub port_guessed: bool,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConfigSource {
    ConfigFile,
    Detected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Ready,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub run_id: RunId,
    pub config_name: String,
    pub state: RunState,
    pub host_port: u16,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    WaitingPermission,
    Error,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStartOptions {
    #[serde(default)]
    pub command: Option<String>, // terminal adapter
    #[serde(default)]
    pub resume_session: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// One transcript item. `seq` is assigned by the daemon and is monotonic per agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub seq: u64,
    pub ts: String,
    #[serde(flatten)]
    pub body: AgentMessageBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageBody {
    UserText {
        text: String,
    },
    AssistantText {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    PermissionRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        #[serde(default)]
        suggestions: Vec<Value>,
    },
    Result {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
        session_id: String,
    },
    System {
        subtype: String,
        data: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
