//! Agent-side plumbing shared by every adapter: where a transcript is kept on
//! disk, and the one funnel through which an adapter records and announces
//! what its agent did.
//!
//! The [`AgentManager`] that owns processes lives alongside this; what is here
//! is the part that has no process in it, so it can be tested on its own.

pub mod claude;
pub mod claude_stream;
pub mod credentials;

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::server::broadcast::EventBus;
use bondsymphonic_proto::*;
use claude::{AgentAdapter, ClaudeAdapter};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

/// Transcripts on disk: one newline-delimited JSON file per agent, appended to
/// as messages arrive.
///
/// Append-only and line-oriented so that a daemon killed mid-write costs at
/// most the last message: [`read`](TranscriptStore::read) skips a line it
/// cannot parse rather than refusing the whole file, which is what makes a
/// torn final write survivable.
pub struct TranscriptStore {
    dir: PathBuf,
}

impl TranscriptStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The file for `agent`. Agent ids are minted by the daemon and are always
    /// `ag_` plus hex, but the id crosses the wire before it gets here, so
    /// anything outside that alphabet is folded to `_` rather than allowed to
    /// address a path of its own.
    fn path(&self, agent: &AgentId) -> PathBuf {
        let name: String = agent
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{name}.ndjson"))
    }

    /// Appends one message. Creates the transcript directory on the first
    /// write, so nothing has to be set up when the daemon starts.
    pub async fn append(&self, agent: &AgentId, msg: &AgentMessage) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(agent))
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await
    }

    /// The whole transcript, oldest first. An agent that has never spoken (or
    /// whose file is gone) reads as empty; a line that will not parse is
    /// logged and skipped, so one bad line never hides the rest.
    pub async fn read(&self, agent: &AgentId) -> std::io::Result<Vec<AgentMessage>> {
        let path = self.path(agent);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<AgentMessage>(line) {
                Ok(msg) => out.push(msg),
                Err(e) => warn!(
                    agent = %agent,
                    path = %path.display(),
                    error = %e,
                    "skipping unparseable transcript line"
                ),
            }
        }
        Ok(out)
    }
}

/// The daemon's live record of one agent: what it belongs to, what it is, and
/// where it currently stands.
///
/// The two `Mutex`es are held only for the moment it takes to read or replace
/// the value they hold; nothing awaits while one is locked.
pub struct AgentEntry {
    pub workspace_id: WorkspaceId,
    pub adapter: AgentAdapterKind,
    pub state: Mutex<(AgentState, Option<String>)>,
    pub session_id: Mutex<Option<String>>,
    pub seq: AtomicU64,
}

impl AgentEntry {
    /// A freshly started agent: idle, no session yet, no messages yet.
    pub fn new(workspace_id: WorkspaceId, adapter: AgentAdapterKind) -> Self {
        Self {
            workspace_id,
            adapter,
            state: Mutex::new((AgentState::Idle, None)),
            session_id: Mutex::new(None),
            seq: AtomicU64::new(0),
        }
    }

    /// The current state and its detail.
    pub fn state(&self) -> (AgentState, Option<String>) {
        self.state.lock().clone()
    }
}

/// The single path by which an adapter reports what its agent did.
///
/// Numbering, timestamping, persistence and publication happen here and only
/// here, so a transcript replayed from disk and one watched live carry the
/// same messages in the same order.
#[derive(Clone)]
pub struct AgentSink {
    events: EventBus,
    store: Arc<TranscriptStore>,
    agent_id: AgentId,
    workspace_id: WorkspaceId,
    entry: Arc<AgentEntry>,
}

impl AgentSink {
    pub fn new(
        events: EventBus,
        store: Arc<TranscriptStore>,
        agent_id: AgentId,
        workspace_id: WorkspaceId,
        entry: Arc<AgentEntry>,
    ) -> Self {
        Self {
            events,
            store,
            agent_id,
            workspace_id,
            entry,
        }
    }

    /// Records one transcript entry and publishes it.
    ///
    /// The sequence number is taken first and the message published last, so a
    /// client that reads the transcript and then subscribes sees each `seq`
    /// once. A failed append is logged rather than propagated: losing a
    /// transcript line is not a reason to stop relaying the agent's output.
    pub async fn message(&self, body: AgentMessageBody) -> AgentMessage {
        let seq = self.entry.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let msg = AgentMessage { seq, ts, body };
        if let Err(e) = self.store.append(&self.agent_id, &msg).await {
            warn!(agent = %self.agent_id, error = %e, "failed to append to transcript");
        }
        self.events.publish(
            Some(self.workspace_id.clone()),
            Event::AgentMessage {
                agent_id: self.agent_id.clone(),
                message: msg.clone(),
            },
        );
        msg
    }

    /// Moves the agent's state and announces it. The entry is updated before
    /// the event goes out, so a client that reacts by asking for the agent's
    /// status never sees the older value.
    pub async fn state(&self, state: AgentState, detail: Option<String>) {
        *self.entry.state.lock() = (state, detail.clone());
        self.events.publish(
            Some(self.workspace_id.clone()),
            Event::AgentStateChanged {
                agent_id: self.agent_id.clone(),
                state,
                detail,
            },
        );
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn entry(&self) -> &Arc<AgentEntry> {
        &self.entry
    }
}

/// One live agent: what the daemon knows about it, and the adapter that drives
/// it.
struct LiveAgent {
    entry: Arc<AgentEntry>,
    /// Insertion order, so `agents_of` reports agents in the order they were
    /// started rather than in whatever order the map happens to hold them.
    ordinal: u64,
    /// One request at a time per agent: the adapter methods take `&mut self`,
    /// and two turns written into the same stdin at once would interleave.
    adapter: tokio::sync::Mutex<Box<dyn AgentAdapter>>,
}

/// Every agent the daemon is running, and the requests that reach them.
///
/// The manager owns the list, not the workspace registry: an agent is a live
/// process, and the registry is a file that survives restarts. `agents_of` is
/// what puts them back into [`WorkspaceInfo`].
pub struct AgentManager {
    events: EventBus,
    store: Arc<TranscriptStore>,
    agents: Mutex<HashMap<AgentId, Arc<LiveAgent>>>,
    next_ordinal: AtomicU64,
}

impl AgentManager {
    pub fn new(events: EventBus, transcripts: PathBuf) -> Self {
        Self {
            events,
            store: Arc::new(TranscriptStore::new(transcripts)),
            agents: Mutex::new(HashMap::new()),
            next_ordinal: AtomicU64::new(0),
        }
    }

    fn get(&self, id: &AgentId) -> Result<Arc<LiveAgent>, RpcError> {
        self.agents
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::not_found(format!("agent {id}")))
    }

    /// Starts a Claude Code agent in a ready workspace's sandbox.
    ///
    /// The `Terminal` adapter kind is refused here on purpose: a terminal is a
    /// PTY the IDE drives directly through `pty.open`, not a process the daemon
    /// pumps a protocol through. The kind stays in the protocol so the IDE can
    /// name what a pane is; starting one as an agent is a client mistake.
    pub async fn start(
        &self,
        d: &Daemon,
        p: AgentStartParams,
    ) -> Result<AgentStartResult, RpcError> {
        if p.adapter == AgentAdapterKind::Terminal {
            return Err(RpcError::invalid_params("terminal agents use pty.open"));
        }
        let ws = d.workspace(&p.workspace_id)?;
        if ws.state != WorkspaceState::Ready {
            return Err(RpcError::invalid_params(format!(
                "workspace {} is not ready",
                ws.id
            )));
        }
        let handle = d.sandbox(&ws.id)?;
        let argv = claude::claude_argv(&p.options)?;

        // Again at start, not only at creation: the user may have logged in
        // since this workspace was made, and a workspace that was created
        // logged out would otherwise stay that way forever.
        let home = d.dirs.home(&ws.id);
        let seeded = credentials::seed_claude_files(&home);
        if !seeded.is_empty() {
            info!(ws = %ws.id, files = ?seeded, "seeded claude credentials");
        }

        // The key is given to this one command, never written into the sandbox
        // spec: the spec's environment reaches every process in the workspace,
        // including terminals the user opens.
        let env = match p.options.api_key.as_deref().filter(|k| !k.is_empty()) {
            Some(key) => vec![("ANTHROPIC_API_KEY".to_owned(), key.to_owned())],
            None => vec![],
        };

        let id: AgentId = new_id(AgentId::PREFIX).as_str().into();
        let entry = Arc::new(AgentEntry::new(ws.id.clone(), p.adapter));
        let sink = AgentSink::new(
            self.events.clone(),
            self.store.clone(),
            id.clone(),
            ws.id.clone(),
            entry.clone(),
        );
        let mut adapter = ClaudeAdapter::new(sink, handle, argv, env, ws.worktree_path.clone());
        // Registered only once it is really running, so a failed start leaves
        // no agent behind for the IDE to find.
        adapter.start().await?;
        self.agents.lock().insert(
            id.clone(),
            Arc::new(LiveAgent {
                entry,
                ordinal: self.next_ordinal.fetch_add(1, Ordering::SeqCst),
                adapter: tokio::sync::Mutex::new(Box::new(adapter)),
            }),
        );
        Ok(AgentStartResult { agent_id: id })
    }

    pub async fn send(&self, p: AgentSendParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent.adapter.lock().await.send(p.text).await?;
        Ok(Empty {})
    }

    pub async fn permission_reply(&self, p: AgentPermissionReplyParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent
            .adapter
            .lock()
            .await
            .permission_reply(p.request_id, p.decision, p.updated_input, p.message)
            .await?;
        Ok(Empty {})
    }

    pub async fn interrupt(&self, p: AgentIdParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent.adapter.lock().await.interrupt().await?;
        Ok(Empty {})
    }

    /// Ends the process but keeps the agent, so its transcript is still
    /// readable. Only the workspace going away removes it (see
    /// [`stop_all_in`](AgentManager::stop_all_in)).
    pub async fn stop(&self, p: AgentIdParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent.adapter.lock().await.stop().await?;
        Ok(Empty {})
    }

    pub async fn history(&self, p: AgentIdParams) -> Result<HistoryResult, RpcError> {
        // Through the map, so an id nobody ever minted is a `NotFound` rather
        // than an empty transcript.
        self.get(&p.agent_id)?;
        Ok(HistoryResult {
            messages: self
                .store
                .read(&p.agent_id)
                .await
                .map_err(|e| RpcError::io(&e))?,
        })
    }

    /// The agents belonging to `ws`, oldest first.
    pub fn agents_of(&self, ws: &WorkspaceId) -> Vec<AgentId> {
        let mut found: Vec<(u64, AgentId)> = self
            .agents
            .lock()
            .iter()
            .filter(|(_, a)| &a.entry.workspace_id == ws)
            .map(|(id, a)| (a.ordinal, id.clone()))
            .collect();
        found.sort_by_key(|(ordinal, _)| *ordinal);
        found.into_iter().map(|(_, id)| id).collect()
    }

    /// Stops and forgets every agent in `ws`. Called before a workspace's
    /// sandbox is torn down, so the agents end through their own `stop` path
    /// (stdin closed, exit awaited, `Exited` announced) instead of vanishing
    /// with the sandbox.
    pub async fn stop_all_in(&self, ws: &WorkspaceId) {
        let victims: Vec<(AgentId, Arc<LiveAgent>)> = {
            let mut agents = self.agents.lock();
            let ids: Vec<AgentId> = agents
                .iter()
                .filter(|(_, a)| &a.entry.workspace_id == ws)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| agents.remove(&id).map(|a| (id, a)))
                .collect()
        };
        for (id, agent) in victims {
            if let Err(e) = agent.adapter.lock().await.stop().await {
                warn!(agent = %id, error = %e, "stopping agent failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transcript_store_round_trips_and_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::new(dir.path().join("transcripts"));
        let id: AgentId = "ag_t".into();
        let m1 = AgentMessage {
            seq: 1,
            ts: "2026-09-09T00:00:00Z".into(),
            body: AgentMessageBody::UserText { text: "hi".into() },
        };
        let m2 = AgentMessage {
            seq: 2,
            ts: "2026-09-09T00:00:01Z".into(),
            body: AgentMessageBody::Result {
                cost_usd: 0.5,
                duration_ms: 10,
                num_turns: 1,
                session_id: "s".into(),
            },
        };
        store.append(&id, &m1).await.unwrap();
        store.append(&id, &m2).await.unwrap();
        // Corrupt a line by hand.
        let path = dir.path().join("transcripts").join("ag_t.ndjson");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        std::fs::write(&path, text).unwrap();
        let read = store.read(&id).await.unwrap();
        assert_eq!(read, vec![m1, m2]);
        assert!(store.read(&"ag_missing".into()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sink_numbers_messages_from_one_and_publishes_state_with_the_entry_updated() {
        let dir = tempfile::tempdir().unwrap();
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let ws: WorkspaceId = "ws_1".into();
        let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Claude));
        let sink = AgentSink::new(
            events,
            Arc::new(TranscriptStore::new(dir.path().join("t"))),
            "ag_s".into(),
            ws,
            entry.clone(),
        );

        let first = sink
            .message(AgentMessageBody::AssistantText { text: "one".into() })
            .await;
        let second = sink
            .message(AgentMessageBody::AssistantText { text: "two".into() })
            .await;
        assert_eq!((first.seq, second.seq), (1, 2));
        assert!(!first.ts.is_empty());

        sink.state(AgentState::WaitingPermission, Some("Bash".into()))
            .await;
        assert_eq!(
            entry.state(),
            (AgentState::WaitingPermission, Some("Bash".into()))
        );

        // Both messages, then the state change, in that order, on the bus.
        let kinds: Vec<Event> = (0..3)
            .map(|_| match rx.try_recv().unwrap() {
                ServerMessage::Event { event, .. } => event,
                other => panic!("expected an event, got {other:?}"),
            })
            .collect();
        assert!(matches!(&kinds[0], Event::AgentMessage { message, .. } if message.seq == 1));
        assert!(matches!(&kinds[1], Event::AgentMessage { message, .. } if message.seq == 2));
        assert!(
            matches!(&kinds[2], Event::AgentStateChanged { state, detail, .. } if *state == AgentState::WaitingPermission && detail.as_deref() == Some("Bash"))
        );

        // What was published is what was persisted.
        let stored = sink.store.read(sink.agent_id()).await.unwrap();
        assert_eq!(stored, vec![first, second]);
    }
}
