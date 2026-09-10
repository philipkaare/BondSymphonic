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

/// One agent a workspace has, as clients see it in [`WorkspaceInfo::agents`].
///
/// The id alone was not enough to bring a tab back: a client that restarts sees
/// the workspace and its agents in `workspace.list`, and without the adapter it
/// cannot tell a Claude agent -- whose transcript the daemon is still serving,
/// and whose session is still resumable -- from a plain terminal. `adapter`,
/// `model` and `permission_mode` are what rebuild the pane; `session_id` is what
/// a resume needs, and is repeated here rather than only in the transcript so a
/// client can tell a resumable agent from one that never got a session.
///
/// There is deliberately no field an API key could travel in. The daemon holds
/// the options an agent was started with, and those carry the user's key; this
/// is the subset that is safe to hand back, so the key cannot reach a client
/// through a call site that forgot to clear it. A client that wants to start
/// another agent like this one fills the key in itself, which is what
/// `agent.start` has always expected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub adapter: AgentAdapterKind,
    /// The last session the agent reported, or `None` for one that never
    /// reported a session. What `resume_session` takes.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The command a terminal agent was started with, if it was given one.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
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
    /// The workspace's agents, oldest first, running and ended alike. The last
    /// is the one a client restoring this workspace's tab reattaches to.
    pub agents: Vec<AgentSummary>,
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

#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

/// Written by hand so the key cannot be formatted into a log line by accident.
///
/// Nothing prints these options today, but this is the one struct in the
/// protocol that carries the user's Anthropic API key, and a `#[derive(Debug)]`
/// here would put the key one `{:?}` away from a tracing field. The presence of
/// a key is worth knowing, so it is reported as `Some(<redacted>)` rather than
/// dropped.
impl std::fmt::Debug for AgentStartOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentStartOptions")
            .field("command", &self.command)
            .field("resume_session", &self.resume_session)
            .field("model", &self.model)
            .field("permission_mode", &self.permission_mode)
            .field("api_key", &self.api_key.as_ref().map(|_| RedactedApiKey))
            .finish()
    }
}

/// Stands in for the key in [`AgentStartOptions`]'s `Debug`, so the field reads
/// `Some(<redacted>)` rather than `Some("<redacted>")`.
struct RedactedApiKey;

impl std::fmt::Debug for RedactedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
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
