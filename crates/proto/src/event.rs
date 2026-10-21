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
        /// Whose sandbox a network notice is about: the backend whose
        /// companion sandbox's proxy refused the connection, or `claude` for
        /// the workspace's own sandbox, which Claude agents share with the
        /// workspace's terminals and runs. Absent on anything else, and on
        /// every notice from a daemon older than the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<AgentAdapterKind>,
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
    /// An `agent.start` in the envelope's workspace reached `phase`. There is
    /// no agent id yet -- the start has not answered -- and none is needed:
    /// the daemon runs one start per workspace at a time.
    ///
    /// Sent only to a client that asked for it in `hello`
    /// ([`FEATURE_AGENT_START_PROGRESS`]); an older IDE never sees one.
    /// `phase` is one of [`start_phase`] and `label` the sentence to show for
    /// it, so a phase this IDE has not heard of still reads properly.
    #[serde(rename = "agent.start_progress")]
    AgentStartProgress {
        adapter: AgentAdapterKind,
        phase: String,
        #[serde(default)]
        label: String,
    },
}

/// The `hello` feature that turns [`Event::AgentStartProgress`] on for a
/// connection.
pub const FEATURE_AGENT_START_PROGRESS: &str = "agent.start_progress";

/// The phases an `agent.start` reports, in the order it reaches them. Wire
/// values: the IDE may key on them, though it shows the event's own label.
pub mod start_phase {
    /// The backend writes the agent's home and checks its CLI.
    pub const PREPARE: &str = "prepare";
    /// The agent's sandbox is made ready (a companion sandbox, for Codex and
    /// OpenCode, the first time).
    pub const SANDBOX: &str = "sandbox";
    /// The agent process is spawned and its protocol handshake done.
    pub const LAUNCH: &str = "launch";
    /// The same, resuming an earlier conversation.
    pub const RESUME: &str = "resume";
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

/// Prefix of the warning the daemon sends when an OpenCode companion's proxy
/// refused one of its model-server tunnels: a host the workspace allowlist
/// cannot fix, so the notice offers nothing to allow. Built by
/// [`Event::tunnel_refused`] and recognised by [`Event::refused_tunnel_host`].
pub const TUNNEL_REFUSED_PREFIX: &str = "The agent's connection to ";

impl Event {
    /// The warn-level `daemon.log` event announcing that `count` events were
    /// discarded for a lagging client.
    pub fn events_dropped(count: u64) -> Event {
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!("{EVENT_DROP_PREFIX}{count}"),
            host: None,
            backend: None,
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
            backend: None,
        }
    }

    /// The same denial, saying whose sandbox asked: what lets the IDE show it
    /// in that agent's transcript rather than only as a toast.
    pub fn network_denied_by(host: &str, backend: AgentAdapterKind) -> Event {
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!("{NETWORK_DENIED_PREFIX}{host}"),
            host: Some(host.into()),
            backend: Some(backend),
        }
    }

    /// The warning for a model-server tunnel the companion's proxy refused.
    ///
    /// Two lines on purpose: the IDE keeps a multi-line warning against the
    /// workspace. It carries the host and the backend so the IDE can also show
    /// it beside the agent, but it is not a denial -- [`Event::denied_host`]
    /// answers `None` for it -- because the workspace allowlist cannot fix a
    /// server missing from the backend's own config.
    pub fn tunnel_refused(host: &str, backend: AgentAdapterKind) -> Event {
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!(
                "{TUNNEL_REFUSED_PREFIX}{host} was refused\n\
                 {host} is not a model server in the agent's own config, \
                 and only the servers it names can be reached this way."
            ),
            host: Some(host.into()),
            backend: Some(backend),
        }
    }

    /// The host of a [`Event::tunnel_refused`] warning, or `None` for anything
    /// else.
    pub fn refused_tunnel_host(&self) -> Option<&str> {
        match self {
            Event::DaemonLog {
                level: LogLevel::Warn,
                message,
                host,
                ..
            } if message.starts_with(TUNNEL_REFUSED_PREFIX) => host.as_deref(),
            _ => None,
        }
    }

    /// Whose sandbox a network notice came from, when the daemon said.
    pub fn denial_backend(&self) -> Option<AgentAdapterKind> {
        match self {
            Event::DaemonLog { backend, .. }
                if self.denied_host().is_some() || self.refused_tunnel_host().is_some() =>
            {
                *backend
            }
            _ => None,
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
                ..
            } if message.starts_with(NETWORK_DENIED_PREFIX) => host.as_deref(),
            _ => None,
        }
    }

    /// The `hello` feature a client must have asked for to be sent this
    /// event, or `None` for the events every client gets.
    pub fn opt_in_feature(&self) -> Option<&'static str> {
        match self {
            Event::AgentStartProgress { .. } => Some(FEATURE_AGENT_START_PROGRESS),
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
                backend: None,
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
                    kind: WorkspaceKind::InPlace,
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
            Event::network_denied_by("evil.example", AgentAdapterKind::Codex),
            Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode),
            FsChanged {
                paths: vec!["a.rs".into()],
            },
            AgentStartProgress {
                adapter: AgentAdapterKind::OpenCode,
                phase: start_phase::PREPARE.into(),
                label: "Preparing OpenCode".into(),
            },
        ]
    }
}
