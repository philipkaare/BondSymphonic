//! Agent-side plumbing shared by every adapter: where a transcript is kept on
//! disk, and the one funnel through which an adapter records and announces
//! what its agent did.
//!
//! The [`AgentManager`] that owns processes lives alongside this; what is here
//! is the part that has no process in it, so it can be tested on its own.

pub mod claude;
pub mod claude_stream;
pub mod credentials;
pub mod persist;

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::server::broadcast::EventBus;
use crate::workspace::now_rfc3339;
use bondsymphonic_proto::*;
use claude::{AgentAdapter, ClaudeAdapter};
use parking_lot::Mutex;
use persist::{AgentRecord, AgentRecords};
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

    /// Deletes the transcript, if there is one.
    ///
    /// Only a workspace going away calls this: an agent's transcript outlives
    /// the agent's process on purpose, and the record beside it is what makes
    /// it readable again after a restart.
    pub fn remove(&self, agent: &AgentId) {
        let path = self.path(agent);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(agent = %agent, path = %path.display(), error = %e, "could not remove the transcript")
            }
        }
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
    /// Bumped on every state change, so a caller can tell "still `Working`"
    /// from "`Working` again". The state alone cannot: an agent that finished
    /// one turn and started another looks identical to one that never left the
    /// first, and something armed against the first turn must not act on the
    /// second.
    epoch: AtomicU64,
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
            epoch: AtomicU64::new(0),
        }
    }

    /// The current state and its detail.
    pub fn state(&self) -> (AgentState, Option<String>) {
        self.state.lock().clone()
    }

    /// How many state changes this agent has been through. Only comparisons
    /// between two readings mean anything.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
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
    /// Serialises [`message`](AgentSink::message) across every clone of this
    /// sink, so numbering, persistence and publication are one indivisible
    /// step. Shared through the `Arc` rather than owned, because the clones are
    /// the concurrent callers: the adapter's stdout reader and the request task
    /// recording the user's prompt.
    order: Arc<tokio::sync::Mutex<()>>,
    /// Where this agent's record is kept, so a session id and an exit reach the
    /// file the next daemon reads. `None` in the unit tests below, which have no
    /// data directory and nothing to restore into.
    records: Option<Arc<AgentRecords>>,
}

impl AgentSink {
    /// One sink per agent, then cloned for every task that reports through it:
    /// the ordering lock lives behind this sink's `Arc`, so a second `new` for
    /// the same agent would make a second lock and the two would interleave
    /// exactly as they did before there was one.
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
            order: Arc::new(tokio::sync::Mutex::new(())),
            records: None,
        }
    }

    /// Keeps this agent's record up to date as it runs.
    ///
    /// Separate from [`new`](AgentSink::new) so the sink stays constructible
    /// without a data directory: only [`AgentManager`] has one.
    pub fn with_records(mut self, records: Arc<AgentRecords>) -> Self {
        self.records = Some(records);
        self
    }

    /// Records one transcript entry and publishes it.
    ///
    /// The sequence number is taken first and the message published last, so a
    /// client that reads the transcript and then subscribes sees each `seq`
    /// once. A failed append is logged rather than propagated: losing a
    /// transcript line is not a reason to stop relaying the agent's output.
    ///
    /// The whole of that is under one lock. Four await points sit between the
    /// sequence number and the publish, so without it two callers interleave
    /// and the message numbered 2 can reach the file, and the bus, ahead of the
    /// one numbered 1 -- and the transcript is read back in file order.
    /// Contention costs nothing: the lock is per agent, and one agent's output
    /// is a single stream anyway.
    pub async fn message(&self, body: AgentMessageBody) -> AgentMessage {
        let _order = self.order.lock().await;
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
        // Before the event: a client that reacts to `Exited` by restarting the
        // daemon must not find the record still open.
        if state == AgentState::Exited {
            self.ended();
        }
        *self.entry.state.lock() = (state, detail.clone());
        // After the state itself, so anyone who sees a new epoch also sees the
        // state that goes with it.
        self.entry.epoch.fetch_add(1, Ordering::SeqCst);
        self.events.publish(
            Some(self.workspace_id.clone()),
            Event::AgentStateChanged {
                agent_id: self.agent_id.clone(),
                state,
                detail,
            },
        );
    }

    /// The session the agent is in, as it last reported it.
    ///
    /// Recorded as well as remembered: it is the one thing a client needs to
    /// carry a conversation across a daemon restart, by starting a new agent
    /// with `options.resume_session`.
    pub fn session_id(&self, id: String) {
        // The CLI reports the session on every `init` line, which is once per
        // turn on a resumed conversation, and the record is a file: nothing is
        // written unless the id actually moved.
        let changed = {
            let mut current = self.entry.session_id.lock();
            let changed = current.as_deref() != Some(id.as_str());
            *current = Some(id.clone());
            changed
        };
        if !changed {
            return;
        }
        if let Some(records) = &self.records {
            records.update(&self.agent_id, |r| r.session_id = Some(id));
        }
    }

    /// The agent's process is gone: closes its record, so the next daemon
    /// reports it as an agent that ended rather than one it lost.
    ///
    /// Idempotent, and it keeps the first answer: the moment the process ended
    /// is what the record wants, not the moment somebody noticed again.
    pub fn ended(&self) {
        let Some(records) = &self.records else {
            return;
        };
        records.update(&self.agent_id, |r| {
            r.ended_at.get_or_insert_with(now_rfc3339);
        });
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn entry(&self) -> &Arc<AgentEntry> {
        &self.entry
    }
}

/// One agent the daemon knows about, live or not.
struct Agent {
    entry: Arc<AgentEntry>,
    /// Insertion order, so `agents_of` reports agents in the order they were
    /// started rather than in whatever order the map happens to hold them.
    /// Restored agents are numbered first, in the order they were started, so a
    /// restart does not reshuffle a workspace's tabs.
    ordinal: u64,
    /// The adapter driving the process, or `None` for an agent restored from
    /// its record: the process is gone, and only its transcript is left.
    ///
    /// One request at a time per agent: the adapter methods take `&mut self`,
    /// and two turns written into the same stdin at once would interleave.
    adapter: Option<tokio::sync::Mutex<Box<dyn AgentAdapter>>>,
}

impl Agent {
    /// The adapter, or the error an agent without a process answers with.
    ///
    /// `NotFound` rather than a state error because that is what the id now
    /// means to the daemon: there is no process behind it and there never will
    /// be again. The way on is a new agent resuming the same session, which the
    /// message says because the id alone gives a client nothing to act on.
    fn adapter(
        &self,
        id: &AgentId,
    ) -> Result<&tokio::sync::Mutex<Box<dyn AgentAdapter>>, RpcError> {
        self.adapter.as_ref().ok_or_else(|| {
            RpcError::not_found(format!(
                "agent {id} ended; start a new one with resume_session to continue it"
            ))
        })
    }
}

/// The state detail a restored agent carries, for an agent that was still
/// running when the daemon went and for one that had already ended.
///
/// Both name the restart, because that is what the IDE has to explain: the tab
/// is there, the history is there, and the agent behind it is not.
const ENDED_AT_RESTART: &str = "the agent ended when the daemon restarted";
const ENDED_BEFORE_RESTART: &str = "the agent ended before the daemon restarted";

/// Every agent the daemon is running, and the requests that reach them.
///
/// The manager owns the list, not the workspace registry: an agent is a live
/// process, and the registry is a file that survives restarts. `agents_of` is
/// what puts them back into [`WorkspaceInfo`].
pub struct AgentManager {
    events: EventBus,
    store: Arc<TranscriptStore>,
    agents: Mutex<HashMap<AgentId, Arc<Agent>>>,
    next_ordinal: AtomicU64,
    /// The agents on disk, which is what makes the map survivable: see
    /// [`restore`](AgentManager::restore).
    records: Arc<AgentRecords>,
}

impl AgentManager {
    pub fn new(events: EventBus, transcripts: PathBuf, records: PathBuf) -> Self {
        Self {
            events,
            store: Arc::new(TranscriptStore::new(transcripts)),
            agents: Mutex::new(HashMap::new()),
            next_ordinal: AtomicU64::new(0),
            records: Arc::new(AgentRecords::new(records)),
        }
    }

    /// An agent id no agent in the map already holds.
    ///
    /// `new_id` is four random bytes, which was collision-free enough while the
    /// map held only live agents; now that restored ones stay in it for the life
    /// of their workspace, a collision would silently replace an agent and its
    /// history. Checking costs one lock on a map of tens of entries.
    fn mint_id(&self) -> AgentId {
        loop {
            let id: AgentId = new_id(AgentId::PREFIX).as_str().into();
            if !self.agents.lock().contains_key(&id) {
                return id;
            }
        }
    }

    fn get(&self, id: &AgentId) -> Result<Arc<Agent>, RpcError> {
        self.agents
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::not_found(format!("agent {id}")))
    }

    /// Puts the agents from the last run of the daemon back, as agents that
    /// have ended.
    ///
    /// Called once at startup and finished before the first connection is
    /// accepted (`main` runs `Daemon::restore_agents` ahead of the accept loop,
    /// which is the whole reason that half is split out), so no `agent.start`
    /// can race it: the restored ordinals come first and a workspace's tabs keep
    /// their order, and `mint_id` sees every id this daemon has ever handed out.
    ///
    /// The process behind each one is gone -- a daemon restart kills every
    /// sandbox -- so each becomes an entry with no adapter: `agent.history`
    /// still reads its transcript, `WorkspaceInfo.agents` still lists it, and
    /// anything that would talk to the process is a `NotFound` pointing at
    /// `resume_session`.
    ///
    /// Records for workspaces `known` does not name are dropped along with
    /// their transcripts. A workspace can only leave the registry through
    /// `workspace.destroy`, which calls
    /// [`forget_workspace`](AgentManager::forget_workspace) itself, so this is
    /// the path for a destroy that was interrupted or a registry edited by hand
    /// -- without it those records and transcripts would never be collected.
    pub fn restore(&self, known: &[WorkspaceId]) {
        let mut records = self.records.load();
        let mut dropped = Vec::new();
        records.retain(|r| {
            if known.contains(&r.workspace_id) {
                return true;
            }
            warn!(agent = %r.agent_id, ws = %r.workspace_id, "dropping the record of an agent whose workspace is gone");
            dropped.push(r.agent_id.clone());
            false
        });
        for id in &dropped {
            self.store.remove(id);
        }

        let mut restored = 0usize;
        let mut closed = 0usize;
        for record in records.iter_mut() {
            let entry = Arc::new(AgentEntry::new(record.workspace_id.clone(), record.adapter));
            let detail = if record.ended_at.is_none() {
                record.ended_at = Some(now_rfc3339());
                closed += 1;
                ENDED_AT_RESTART
            } else {
                ENDED_BEFORE_RESTART
            };
            *entry.state.lock() = (AgentState::Exited, Some(detail.to_owned()));
            *entry.session_id.lock() = record.session_id.clone();
            self.agents.lock().insert(
                record.agent_id.clone(),
                Arc::new(Agent {
                    entry,
                    ordinal: self.next_ordinal.fetch_add(1, Ordering::SeqCst),
                    adapter: None,
                }),
            );
            restored += 1;
        }
        // One write for the whole list rather than one per closed record, and
        // only when there is something to say: a daemon that shut down cleanly
        // rewrites nothing.
        if !dropped.is_empty() || closed > 0 {
            self.records.replace_all(records);
        }
        if restored > 0 {
            info!(restored, closed, "restored agents from the records file");
        }
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
        // The backend decides how the CLI is named: bound into the sandbox at a
        // fixed path under bwrap, and at its host path where there are no
        // mounts. Resolved before anything is spawned, so a missing install is
        // a `PrereqMissing` naming the path rather than an exec failure.
        let argv = claude::claude_argv(&p.options, d.backend.name())?;

        // Again at start, not only at creation: the user may have logged in
        // since this workspace was made, and a workspace that was created
        // logged out would otherwise stay that way forever.
        let home = d.dirs.home(&ws.id);
        let seeded = credentials::seed_claude_files(&home);
        if !seeded.is_empty() {
            info!(ws = %ws.id, files = ?seeded, "seeded claude credentials");
        }
        // After the seeding, because it overwrites what the seeding just put
        // there: a repository that pins its own settings is pinning the tools
        // the agent may use, and the daemon user's copy must not win.
        claude::apply_repo_settings(&ws.worktree_path, &home)?;

        // The key is given to this one command, never written into the sandbox
        // spec: the spec's environment reaches every process in the workspace,
        // including terminals the user opens.
        let env = match p.options.api_key.as_deref().filter(|k| !k.is_empty()) {
            Some(key) => vec![("ANTHROPIC_API_KEY".to_owned(), key.to_owned())],
            None => vec![],
        };

        let id = self.mint_id();
        let entry = Arc::new(AgentEntry::new(ws.id.clone(), p.adapter));
        let sink = AgentSink::new(
            self.events.clone(),
            self.store.clone(),
            id.clone(),
            ws.id.clone(),
            entry.clone(),
        )
        .with_records(self.records.clone());
        let mut adapter = ClaudeAdapter::new(sink, handle, argv, env, ws.worktree_path.clone());
        // The record goes down *before* the process is spawned, and this
        // ordering is the whole of what makes the session id survivable.
        //
        // `adapter.start` spawns the stdout reader, and the CLI's very first
        // line is the `init` that names the session. That reader records the id
        // by updating this record; an update against a record that is not there
        // yet is a no-op, and because the entry then already holds the id,
        // nothing later notices it was never written. The conversation would
        // come back after a restart with no session to resume and no way to
        // tell. Writing first costs one file write on a path that is already
        // spawning a process, and leaves no window at all.
        //
        // The same ordering means an agent that exits immediately -- a bad
        // flag, no login -- has a record for `ended()` to close, instead of
        // coming back as one the daemon claims to have lost at restart.
        self.records.upsert(AgentRecord {
            agent_id: id.clone(),
            workspace_id: ws.id.clone(),
            adapter: p.adapter,
            session_id: p.options.resume_session.clone(),
            options: p.options,
            started_at: now_rfc3339(),
            ended_at: None,
        });
        // Registered only once it is really running, so a failed start leaves
        // no agent behind for the IDE to find -- and no record either, since
        // nothing ever ran under this id.
        if let Err(e) = adapter.start().await {
            self.records.remove(&id);
            return Err(e);
        }
        self.agents.lock().insert(
            id.clone(),
            Arc::new(Agent {
                entry,
                ordinal: self.next_ordinal.fetch_add(1, Ordering::SeqCst),
                adapter: Some(tokio::sync::Mutex::new(Box::new(adapter))),
            }),
        );
        Ok(AgentStartResult { agent_id: id })
    }

    pub async fn send(&self, p: AgentSendParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent
            .adapter(&p.agent_id)?
            .lock()
            .await
            .send(p.text)
            .await?;
        Ok(Empty {})
    }

    pub async fn permission_reply(&self, p: AgentPermissionReplyParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent
            .adapter(&p.agent_id)?
            .lock()
            .await
            .permission_reply(p.request_id, p.decision, p.updated_input, p.message)
            .await?;
        Ok(Empty {})
    }

    pub async fn interrupt(&self, p: AgentIdParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        agent.adapter(&p.agent_id)?.lock().await.interrupt().await?;
        Ok(Empty {})
    }

    /// Ends the process but keeps the agent, so its transcript is still
    /// readable. Only the workspace going away removes it (see
    /// [`stop_all_in`](AgentManager::stop_all_in)).
    ///
    /// An agent that has already ended -- one restored from a record -- answers
    /// `Ok`: `stop` is a teardown verb, and the client is asking for a state the
    /// daemon is already in.
    pub async fn stop(&self, p: AgentIdParams) -> Result<Empty, RpcError> {
        let agent = self.get(&p.agent_id)?;
        let Some(adapter) = agent.adapter.as_ref() else {
            return Ok(Empty {});
        };
        adapter.lock().await.stop().await?;
        Ok(Empty {})
    }

    pub async fn history(&self, p: AgentIdParams) -> Result<HistoryResult, RpcError> {
        // Through the map, so an id nobody ever minted is a `NotFound` rather
        // than an empty transcript.
        let agent = self.get(&p.agent_id)?;
        let messages = self
            .store
            .read(&p.agent_id)
            .await
            .map_err(|e| RpcError::io(&e))?;
        // Read after the messages, so the state a client is given is never
        // older than the transcript it is given with it.
        let (state, detail) = agent.entry.state();
        Ok(HistoryResult {
            messages,
            state,
            detail,
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
        let victims: Vec<(AgentId, Arc<Agent>)> = {
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
            let Some(adapter) = agent.adapter.as_ref() else {
                continue;
            };
            if let Err(e) = adapter.lock().await.stop().await {
                warn!(agent = %id, error = %e, "stopping agent failed");
            }
        }
    }

    /// Forgets a destroyed workspace's agents for good: their records and their
    /// transcripts.
    ///
    /// Separate from [`stop_all_in`](AgentManager::stop_all_in), and called only
    /// once the destroy has actually got rid of the worktree. A destroy that
    /// fails late leaves the workspace in the registry in `Error`, and its
    /// agents' history is the one thing the user might still want out of it.
    ///
    /// The ids come from the file rather than from the map, so an agent this
    /// daemon never held an entry for -- one restored and then stopped, or one
    /// whose entry was lost -- takes its transcript with it too.
    pub fn forget_workspace(&self, ws: &WorkspaceId) {
        for id in self.records.remove_workspace(ws) {
            self.store.remove(&id);
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

    /// Why the record has to exist before the process does.
    ///
    /// The CLI's first line is the `init` that names the session, and the
    /// reader records it by *updating* the agent's record. An update against a
    /// record that is not there yet writes nothing — and the entry now holds
    /// the id, so the `!changed` guard swallows every later `init` carrying the
    /// same one. The session is then unrecoverable: the record is what a
    /// restarted daemon reads, and it says the conversation has no session to
    /// resume.
    ///
    /// This is the hazard `AgentManager::start` avoids by writing the record
    /// before it spawns anything. The ordering itself cannot be observed from
    /// outside without racing a process start, so it is pinned here instead.
    #[tokio::test]
    async fn a_session_id_reported_before_the_record_exists_is_lost_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let records = Arc::new(AgentRecords::new(dir.path().join("agents.json")));
        let ws: WorkspaceId = "ws_1".into();
        let id: AgentId = "ag_1".into();
        let blank = || AgentRecord {
            agent_id: id.clone(),
            workspace_id: ws.clone(),
            adapter: AgentAdapterKind::Claude,
            session_id: None,
            options: AgentStartOptions {
                command: None,
                resume_session: None,
                model: None,
                permission_mode: None,
                api_key: None,
            },
            started_at: now_rfc3339(),
            ended_at: None,
        };
        let sink = |entry: &Arc<AgentEntry>| {
            AgentSink::new(
                EventBus::new(16),
                Arc::new(TranscriptStore::new(dir.path().join("t"))),
                id.clone(),
                ws.clone(),
                entry.clone(),
            )
            .with_records(records.clone())
        };

        // The wrong order: the agent speaks, and only then is it recorded.
        let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Claude));
        let s = sink(&entry);
        s.session_id("sess-1".into());
        records.upsert(blank());
        s.session_id("sess-1".into());
        assert_eq!(
            records.load()[0].session_id,
            None,
            "this is the loss the ordering exists to prevent"
        );

        // The order `start` actually uses: recorded first, so the reader's
        // update lands and every later report is a cheap no-op.
        std::fs::remove_file(dir.path().join("agents.json")).unwrap();
        let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Claude));
        let s = sink(&entry);
        records.upsert(blank());
        s.session_id("sess-1".into());
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-1"));
        s.session_id("sess-1".into());
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-1"));
        // And a session that really does move is followed.
        s.session_id("sess-2".into());
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-2"));
    }

    /// `ended()` needs a record for the same reason: an agent that dies the
    /// instant it starts must not come back from a restart as one the daemon
    /// thinks it lost.
    #[tokio::test]
    async fn an_exit_reported_before_the_record_exists_leaves_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let records = Arc::new(AgentRecords::new(dir.path().join("agents.json")));
        let ws: WorkspaceId = "ws_1".into();
        let id: AgentId = "ag_1".into();
        let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Claude));
        let sink = AgentSink::new(
            EventBus::new(16),
            Arc::new(TranscriptStore::new(dir.path().join("t"))),
            id.clone(),
            ws.clone(),
            entry.clone(),
        )
        .with_records(records.clone());

        records.upsert(AgentRecord {
            agent_id: id.clone(),
            workspace_id: ws.clone(),
            adapter: AgentAdapterKind::Claude,
            session_id: None,
            options: AgentStartOptions {
                command: None,
                resume_session: None,
                model: None,
                permission_mode: None,
                api_key: None,
            },
            started_at: now_rfc3339(),
            ended_at: None,
        });
        sink.state(AgentState::Exited, Some("exit code 1".into()))
            .await;
        let closed = records.load()[0].ended_at.clone();
        assert!(closed.is_some(), "the exit must close the record");
        // Idempotent, and it keeps the first answer.
        sink.ended();
        assert_eq!(records.load()[0].ended_at, closed);
    }

    /// Two callers is the real shape: the adapter's stdout reader records what
    /// the agent said while the request task records the prompt the user just
    /// typed. Taking the sequence number and then awaiting the append would let
    /// the two interleave, and the transcript is read back in file order, so a
    /// user's own message would appear below the reply to it.
    /// On the multi-threaded runtime, so the two tasks really run at once
    /// rather than only interleaving at await points.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_messages_share_one_order_on_the_bus_and_on_disk() {
        const EACH: usize = 25;

        let dir = tempfile::tempdir().unwrap();
        let events = EventBus::new(256);
        let mut rx = events.subscribe();
        let ws: WorkspaceId = "ws_c".into();
        let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Claude));
        let sink = AgentSink::new(
            events,
            Arc::new(TranscriptStore::new(dir.path().join("t"))),
            "ag_c".into(),
            ws,
            entry,
        );

        let reader = {
            let sink = sink.clone();
            tokio::spawn(async move {
                for i in 0..EACH {
                    sink.message(AgentMessageBody::AssistantText {
                        text: format!("a{i}"),
                    })
                    .await;
                }
            })
        };
        let writer = {
            let sink = sink.clone();
            tokio::spawn(async move {
                for i in 0..EACH {
                    sink.message(AgentMessageBody::UserText {
                        text: format!("u{i}"),
                    })
                    .await;
                }
            })
        };
        reader.await.unwrap();
        writer.await.unwrap();

        let published: Vec<u64> = (0..EACH * 2)
            .map(|_| match rx.try_recv().unwrap() {
                ServerMessage::Event {
                    event: Event::AgentMessage { message, .. },
                    ..
                } => message.seq,
                other => panic!("expected an agent message, got {other:?}"),
            })
            .collect();
        let stored: Vec<u64> = sink
            .store
            .read(sink.agent_id())
            .await
            .unwrap()
            .iter()
            .map(|m| m.seq)
            .collect();

        assert_eq!(published.first(), Some(&1), "{published:?}");
        assert!(
            published.windows(2).all(|w| w[0] + 1 == w[1]),
            "the bus must see 1..n in order: {published:?}"
        );
        assert_eq!(
            stored, published,
            "the file must hold the same order the bus saw"
        );
    }
}
