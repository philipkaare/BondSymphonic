use crate::ids::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub sandbox_backend: String, // "linux_bwrap" | "noop"
    pub git_protect: bool,
    pub adapters: Vec<AgentAdapterKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<BackendDescriptor>,
    /// The daemon answers `opencode.list_providers` and
    /// `opencode.test_provider`. A daemon that predates them sends nothing,
    /// which reads as `false`, and Settings then hides the model-server list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opencode_providers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdapterKind {
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendChoice {
    pub id: String,
    pub label: String,
}

/// Known backends include uninstalled ones, so Settings can offer installation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendDescriptor {
    pub id: AgentAdapterKind,
    pub label: String,
    pub permission_modes: Vec<BackendChoice>,
    pub default_permission_mode: String,
    pub permission_note: String,
    pub credential_label: String,
    pub prerequisite_names: Vec<String>,
    pub setup_actions: Vec<SetupAction>,
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
    /// Create a long-lived subscription token with `claude setup-token`. The
    /// daemon captures it from the terminal's output and gives it to agents
    /// as `CLAUDE_CODE_OAUTH_TOKEN`.
    ClaudeSetupToken,
    InstallCodex,
    CodexLogin,
    CodexLogout,
    InstallOpencode,
    OpencodeLogin,
    OpencodeLogout,
    /// Remove the Claude Code CLI BondSymphonic's installer put in the
    /// daemon user's home, breaking `claude` on purpose. Never the login or
    /// `~/.claude`: an uninstall is about the program, not the account.
    UninstallClaude,
    /// Remove BondSymphonic's own Codex install and its Code Mode helper.
    UninstallCodex,
    /// Remove BondSymphonic's own OpenCode install.
    UninstallOpencode,
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
    /// Explicit workspace blocks take precedence over all network grants.
    #[serde(default)]
    pub blocklist: Vec<String>,
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
    AllowForSession,
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

/// The `System` subtype an adapter writes a sentence for the user under. The
/// name predates the other backends: only the OpenCode adapter writes these
/// today, and the IDE matches on this spelling.
pub const NOTICE_SUBTYPE: &str = "opencode_notice";
/// The `System` subtype for a message the user sent that is waiting for the
/// turn in flight. Its data is a [`QueuedPrompt`].
pub const PROMPT_QUEUED_SUBTYPE: &str = "prompt_queued";
/// The `System` subtype saying what became of a queued message. Its data is a
/// [`DequeuedPrompt`]; one that was sent is followed by its `UserText`.
pub const PROMPT_DEQUEUED_SUBTYPE: &str = "prompt_dequeued";

/// A sentence an adapter writes for the user, the `data` of a
/// [`NOTICE_SUBTYPE`] message, with what the user can do about it.
///
/// Additive in both directions: a daemon that predates the structure wrote
/// `{text}` alone, which reads as [`NoticeKind::Other`] with no actions, and
/// an IDE that predates it reads `text` and ignores the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentNotice {
    pub text: String,
    #[serde(default)]
    pub kind: NoticeKind,
    /// The buttons the IDE offers beside the sentence, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NoticeAction>,
    /// The agent's own log lines up to the notice, newest last, redacted:
    /// what "Show log" shows. The daemon and the agent run in WSL, where a
    /// Windows client cannot reliably open the agent's log files, so the lines
    /// travel with the notice and survive a reconnect with the transcript.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// The model answered a prompt with nothing at all: a rejected key, an
    /// unusable model.
    EmptyTurn,
    /// The turn has been silent long enough that the user should know.
    Stall,
    /// Queued messages were thrown away: a second interrupt, or a stop.
    QueueDiscarded,
    /// A notice with no kind, from an older daemon, or of a kind this build
    /// does not know.
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeAction {
    /// Open the folder holding the backend's user configuration.
    OpenConfigFolder,
    /// Show [`AgentNotice::log`].
    ShowLog,
    /// Interrupt the turn, while it is still running.
    Interrupt,
    /// An action this build does not know; shown as nothing.
    #[serde(other)]
    Unknown,
}

impl AgentNotice {
    pub fn into_message(self) -> AgentMessageBody {
        AgentMessageBody::System {
            subtype: NOTICE_SUBTYPE.into(),
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }

    pub fn from_message(body: &AgentMessageBody) -> Option<Self> {
        system_data(body, NOTICE_SUBTYPE)
    }
}

/// A message waiting for the turn in flight, as [`PROMPT_QUEUED_SUBTYPE`]
/// carries it. `id` is what `agent.queued_prompt` names it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedPrompt {
    pub id: String,
    pub text: String,
}

/// What became of a queued message; see [`PROMPT_DEQUEUED_SUBTYPE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DequeuedPrompt {
    pub id: String,
    pub outcome: DequeueOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DequeueOutcome {
    Sent,
    Discarded,
}

/// What the user asked for a queued message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedPromptAction {
    /// Interrupt the turn in flight and send this message next.
    SendNow,
    /// Never send it.
    Discard,
}

impl QueuedPrompt {
    pub fn into_message(self) -> AgentMessageBody {
        AgentMessageBody::System {
            subtype: PROMPT_QUEUED_SUBTYPE.into(),
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }

    pub fn from_message(body: &AgentMessageBody) -> Option<Self> {
        system_data(body, PROMPT_QUEUED_SUBTYPE)
    }
}

impl DequeuedPrompt {
    pub fn into_message(self) -> AgentMessageBody {
        AgentMessageBody::System {
            subtype: PROMPT_DEQUEUED_SUBTYPE.into(),
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }

    pub fn from_message(body: &AgentMessageBody) -> Option<Self> {
        system_data(body, PROMPT_DEQUEUED_SUBTYPE)
    }
}

/// The tokens a backend reported for one turn, normalised. Every field is
/// what the backend said, or absent: nothing here is estimated.
///
/// It travels as `turn_usage` in the `codex_turn` / `opencode_turn` row that
/// precedes the turn's `Result`, rather than on `Result` itself, so the shape
/// of `Result` -- and every client that builds or matches one -- is unchanged.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TurnUsage {
    /// Input tokens the turn used. Codex: the whole turn. OpenCode: its
    /// final model call, which is what OpenCode reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens in the model's context after the turn, and the window's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// What the backend says the session has cost so far, in US dollars.
    /// Only ever a price the backend reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cost_usd: Option<f64>,
}

/// The key [`TurnUsage`] travels under in a turn row's data.
pub const TURN_USAGE_KEY: &str = "turn_usage";

impl TurnUsage {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The usage on a `codex_turn` or `opencode_turn` row, if it carries any.
    pub fn from_turn_row(body: &AgentMessageBody) -> Option<Self> {
        match body {
            AgentMessageBody::System { subtype, data }
                if matches!(subtype.as_str(), "codex_turn" | "opencode_turn") =>
            {
                serde_json::from_value::<Self>(data.get(TURN_USAGE_KEY)?.clone())
                    .ok()
                    .filter(|usage| !usage.is_empty())
            }
            _ => None,
        }
    }
}

fn system_data<T: serde::de::DeserializeOwned>(body: &AgentMessageBody, want: &str) -> Option<T> {
    match body {
        AgentMessageBody::System { subtype, data } if subtype == want => {
            serde_json::from_value(data.clone()).ok()
        }
        _ => None,
    }
}

/// One model as the Anthropic Models API describes it, trimmed to what the
/// model dropdown needs: `system.list_models`' `parse_models` reads these
/// three fields out of a much larger object and ignores the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// How a sandboxed OpenCode agent would reach a provider's `baseURL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRoute {
    /// The origin is granted on the companion's proxy.
    Granted,
    /// A loopback origin: granted, and carried by the in-sandbox tunnel to the
    /// loopback of the WSL distribution the daemon runs in.
    Tunnelled,
    /// No route: not a literal `http(s)://` URL, or a loopback port the
    /// sandbox cannot listen on for it.
    NotGranted,
    /// The provider names no `baseURL`; OpenCode uses its built-in endpoint,
    /// which the workspace allowlist decides about.
    NoBaseUrl,
    /// A route a newer daemon knows and this build does not.
    #[serde(other)]
    Unknown,
}

/// One provider from the Windows user's OpenCode config, as Settings lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeProvider {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// As written in the config, `{env:...}` and all.
    #[serde(default)]
    pub base_url: Option<String>,
    /// How many entries the config's `models` object has.
    #[serde(default)]
    pub model_count: u32,
    pub sandbox_route: SandboxRoute,
    /// Why there is no route, or which address a tunnel carries it to.
    #[serde(default)]
    pub route_note: Option<String>,
}

/// What a connection test found. The daemon's `message` says it in words;
/// the outcome is for the IDE to colour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTestOutcome {
    /// Answered `GET <baseURL>/models` with a 2xx.
    Reachable,
    /// Connected, and answered with an HTTP error status.
    HttpError,
    /// Nothing listens at that address and port.
    Refused,
    /// The name, the connection or the answer took too long.
    Timeout,
    /// The host name does not resolve.
    DnsFailure,
    /// A sandbox would get no route to it at all.
    NotGranted,
    /// Anything else: unknown provider, no `baseURL`, a broken answer.
    Failed,
    #[serde(other)]
    Unknown,
}

/// The answer to `opencode.test_provider`. Never carries the API key; at most
/// where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenCodeProviderTest {
    pub outcome: ProviderTestOutcome,
    pub message: String,
    /// The URL that was fetched.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub model_count: Option<u32>,
    /// The first few model ids the server listed.
    #[serde(default)]
    pub model_ids: Vec<String>,
    /// Where the key sent came from ("config", "OpenCode sign-in", "stored
    /// key"), or `None` when the request carried no key.
    #[serde(default)]
    pub key_source: Option<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
}
