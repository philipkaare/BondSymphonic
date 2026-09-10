use crate::ids::*;
use crate::types::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Event {
    #[serde(rename = "daemon.log")]
    DaemonLog {
        level: LogLevel,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    #[serde(rename = "workspace.state")]
    /// Boxed because `WorkspaceInfo` is much the largest payload in the
    /// protocol -- seven strings and three lists -- and every `ServerMessage`,
    /// including the ordinary responses, would otherwise be sized for it.
    /// A state change is also the rarest of these events; the common ones
    /// (`pty.output`, `agent.message`) stay small and pay nothing. `Box`
    /// serialises as the value it holds, so the wire form is unchanged.
    WorkspaceStateChanged { info: Box<WorkspaceInfo> },
    #[serde(rename = "agent.state")]
    AgentStateChanged {
        agent_id: AgentId,
        state: AgentState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "agent.message")]
    AgentMessage {
        agent_id: AgentId,
        message: crate::types::AgentMessage,
    },
    #[serde(rename = "pty.output")]
    PtyOutput { pty_id: PtyId, data_b64: String },
    #[serde(rename = "pty.exit")]
    PtyExit { pty_id: PtyId, code: i32 },
    #[serde(rename = "run.output")]
    RunOutput { run_id: RunId, line: String },
    #[serde(rename = "run.state")]
    RunStateChanged {
        run_id: RunId,
        state: RunState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Why the run reached this state, when the state alone does not say:
        /// the exit status behind a `failed`, or the reason a `stopped` run
        /// stopped. Absent for the ordinary transitions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "fs.changed")]
    FsChanged { paths: Vec<String> },
}

/// Prefix of the `daemon.log` message the daemon sends when a client's event
/// queue overflowed and events were discarded.
///
/// The wording is a contract between the daemon (which emits the notice) and
/// every client that reacts to it (the IDE marks the gap in each open
/// terminal). Neither side spells it out: both go through
/// [`Event::events_dropped`] and [`Event::dropped_event_count`], so rewording
/// the line here cannot leave one end matching on text the other no longer
/// sends.
pub const EVENT_DROP_PREFIX: &str = "events dropped: ";

/// Prefix of the `daemon.log` message the daemon sends when a workspace's proxy
/// refused a connection because the host was not on the allowlist.
///
/// Same contract as [`EVENT_DROP_PREFIX`], for the same reason: the daemon
/// builds the notice with [`Event::network_denied`] and the IDE recognises it
/// with [`Event::denied_host`], so the wording lives in one place and the toast
/// offering "Allow host" cannot drift away from the line the daemon sends.
pub const NETWORK_DENIED_PREFIX: &str = "network denied: ";

impl Event {
    /// The warn-level `daemon.log` event announcing that `count` events were
    /// discarded for a lagging client.
    pub fn events_dropped(count: u64) -> Event {
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!("{EVENT_DROP_PREFIX}{count}"),
            host: None,
        }
    }

    /// How many events this notice says were discarded, or `None` when the
    /// event is not a drop notice at all. The inverse of
    /// [`Event::events_dropped`].
    pub fn dropped_event_count(&self) -> Option<u64> {
        match self {
            Event::DaemonLog {
                level: LogLevel::Warn,
                message,
                ..
            } => message.strip_prefix(EVENT_DROP_PREFIX)?.trim().parse().ok(),
            _ => None,
        }
    }

    /// The warn-level `daemon.log` event announcing that the workspace's proxy
    /// refused a connection to `host`.
    ///
    /// The host travels twice: in the message a human reads in the log, and in
    /// the `host` field the IDE reads to offer "Allow host". Parsing it back
    /// out of the message would make the toast depend on the wording.
    pub fn network_denied(host: &str) -> Event {
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!("{NETWORK_DENIED_PREFIX}{host}"),
            host: Some(host.into()),
        }
    }

    /// The host this notice says was refused, or `None` when the event is not a
    /// denial at all. The inverse of [`Event::network_denied`].
    pub fn denied_host(&self) -> Option<&str> {
        match self {
            Event::DaemonLog {
                level: LogLevel::Warn,
                message,
                host,
            } if message.starts_with(NETWORK_DENIED_PREFIX) => host.as_deref(),
            _ => None,
        }
    }

    pub fn examples() -> Vec<Event> {
        use Event::*;
        vec![
            DaemonLog {
                level: LogLevel::Info,
                message: "m".into(),
                host: None,
            },
            WorkspaceStateChanged {
                info: Box::new(WorkspaceInfo {
                    id: "ws_1".into(),
                    name: "a".into(),
                    repo_path: "/r".into(),
                    base_branch: "main".into(),
                    branch: "bs/a/work".into(),
                    worktree_path: "/w".into(),
                    created_at: "2026-09-08T10:00:00Z".into(),
                    allowlist: vec![],
                    state: WorkspaceState::Error("x".into()),
                    agents: vec![],
                    agent_records: vec![],
                    runs: vec![],
                }),
            },
            AgentStateChanged {
                agent_id: "ag_1".into(),
                state: AgentState::Idle,
                detail: None,
            },
            AgentMessage {
                agent_id: "ag_1".into(),
                message: crate::types::AgentMessage {
                    seq: 1,
                    ts: "2026-09-08T10:00:00Z".into(),
                    body: AgentMessageBody::ToolUse {
                        id: "t".into(),
                        name: "Bash".into(),
                        input: serde_json::json!({"command":"ls"}),
                    },
                },
            },
            PtyOutput {
                pty_id: "pty_1".into(),
                data_b64: "aGk=".into(),
            },
            PtyExit {
                pty_id: "pty_1".into(),
                code: 0,
            },
            RunOutput {
                run_id: "run_1".into(),
                line: "ready".into(),
            },
            RunStateChanged {
                run_id: "run_1".into(),
                state: RunState::Ready,
                url: Some("http://localhost:1".into()),
                detail: None,
            },
            RunStateChanged {
                run_id: "run_1".into(),
                state: RunState::Failed,
                url: None,
                detail: Some("exited with status 1".into()),
            },
            Event::network_denied("evil.example"),
            FsChanged {
                paths: vec!["a.rs".into()],
            },
        ]
    }
}
