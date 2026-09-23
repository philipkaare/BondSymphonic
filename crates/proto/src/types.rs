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

/// The setup commands a client may ask the daemon to run in a host terminal:
/// the ones that fix a prerequisite a person has to fix interactively, and the
/// two that undo a login again. Each names a command the daemon already knows;
/// the request never carries a command line.
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
    /// Log out of Claude again, breaking `claude_auth` on purpose.
    ///
    /// The one setup action whose point is to make a prerequisite fail: a
    /// session that has to be replaced -- an account switch, or an OAuth login
    /// that has gone stale in a way `claude auth login` will not overwrite --
    /// starts by getting rid of the one that is there.
    ClaudeLogout,
    /// Log out of GitHub again, breaking `gh_auth` on purpose.
    GhLogout,
}

/// What `repo.inspect` knows about a path the user picked.
///
/// It answers for a path that is *not* a repository as well as for one that is,
/// because the New Agent dialog asks about a folder before anything has been
/// created in it: a folder that is not a repository yet is an ordinary starting
/// point, not an error to report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub default_branch: String,
    pub branches: Vec<String>,
    pub is_dirty: bool,
    pub remotes: Vec<String>,
    /// False when the path is a directory git does not recognise. The other
    /// fields are then the empty answer: no branches, `main` as the branch such
    /// a folder would be initialised with.
    ///
    /// It defaults to **true**, which is the only value that keeps a newer
    /// client honest against an older daemon: a daemon that predates this field
    /// failed the whole call for a non-repository, so every answer it ever sent
    /// was about a repository.
    #[serde(default = "yes")]
    pub is_repo: bool,
    /// Whether the directory itself is there. Only meaningful with
    /// `is_repo: false`: `false` means the folder would have to be created as
    /// well as initialised.
    #[serde(default)]
    pub exists: bool,
    /// The branch checked out in the repository, or `None` when `HEAD` is
    /// detached or the path is not a repository. `default_branch` is the
    /// remote's answer and is not this: an in-place workspace works on what is
    /// checked out, and the New Agent dialog shows that.
    #[serde(default)]
    pub head_branch: Option<String>,
    /// Why an agent cannot work directly in this checkout -- a linked worktree,
    /// a `.git` that is a file -- as the sentence `workspace.create` would
    /// refuse with, or `None` when it can.
    #[serde(default)]
    pub in_place_refusal: Option<String>,
    /// The repository's effective `core.hooksPath`, relative to the root, when
    /// the programs it names are ones an in-place agent could write: hooks the
    /// agent can edit and the user's own git then runs. `None` otherwise.
    ///
    /// That is the whole working tree (`.husky/_`, which husky does), and also
    /// anything under `.git` except the six directories the sandbox binds
    /// read-only — `hooks`, `info`, `modules`, `worktrees`, `remotes`,
    /// `branches` — so `.git/my-hooks` is reported and `.git/hooks` is not.
    /// A path outside the repository root is the user's own and is never
    /// reported.
    #[serde(default)]
    pub hooks_path_in_tree: Option<String>,
}

/// The default for [`RepoInfo::is_repo`]. Serde takes a path to a function, not
/// a literal.
fn yes() -> bool {
    true
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

/// Which of the two shapes a workspace has.
///
/// `Worktree` is the original: a `bs/<name>/work` branch checked out in a
/// worktree of the daemon's own. `InPlace` is an agent working directly in the
/// repository's checkout, on whatever branch is checked out there; its
/// `worktree_path` is the repository root and nothing is ever merged out of it.
/// Defaults to `Worktree`, which is what every registry and reply written before
/// the field existed describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Worktree,
    InPlace,
}

/// One agent a workspace has, as clients see it in
/// [`WorkspaceInfo::agent_records`].
///
/// The id alone was not enough to bring a tab back: a client that restarts sees
/// the workspace and its agents in `workspace.list`, and without the adapter it
/// cannot tell a Claude agent -- whose transcript the daemon is still serving,
/// and whose session is still resumable -- from a plain terminal. `adapter` and
/// the two option fields are what rebuild the pane, `state` says whether the
/// agent is still running, and `session_id` is what a resume needs -- repeated
/// here rather than left only in the transcript so a client can tell a
/// resumable agent from one that never got a session.
///
/// There is deliberately no field an API key could travel in. `options` on the
/// daemon's own `AgentRecord` is a full [`AgentStartOptions`], which carries
/// the user's key; the three fields below are that struct with `api_key`
/// stripped and `resume_session` left out, expressed as separate fields so
/// there is no key-shaped hole for a future call site to forget to clear. A
/// client starting another agent like this one supplies the key itself, which
/// is what `agent.start` has always expected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: AgentId,
    pub adapter: AgentAdapterKind,
    /// What the agent is doing now. A restored agent is `Exited`, which is what
    /// puts its pane on Restart rather than on a prompt box.
    pub state: AgentState,
    /// The last session the agent reported, or `None` for one that never
    /// reported a session. What `resume_session` takes.
    ///
    /// This and the three below are left out of the wire form when they are
    /// unset rather than written as `null`: a record for an agent started with
    /// nothing is `{id, adapter, state}`, and a client cannot mistake "the
    /// daemon chose the default" for "the user asked for null".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The command a terminal agent was started with, if it was given one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Which kind of workspace this is. See [`WorkspaceKind`].
    #[serde(default)]
    pub kind: WorkspaceKind,
    #[serde(flatten)]
    pub state: WorkspaceState,
    /// The workspace's agents by id, oldest first, running and ended alike.
    ///
    /// Kept as bare ids beside [`WorkspaceInfo::agent_records`], which holds the
    /// same agents in the same order with everything else about them. Two lists
    /// rather than one changed list, so a daemon and a client of different
    /// vintages still understand each other: an older client reads `agents` off
    /// a newer daemon exactly as before, and a newer client reading an older
    /// daemon finds `agent_records` defaulted to empty instead of failing to
    /// parse the reply at all.
    pub agents: Vec<AgentId>,
    /// The same agents, with the adapter, state, session and start options a
    /// client needs to rebuild a tab for one. Empty from a daemon too old to
    /// send it, which a client must read as "nothing is known about them"
    /// rather than as "there are none" -- `agents` is the list.
    #[serde(default)]
    pub agent_records: Vec<AgentSummary>,
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
