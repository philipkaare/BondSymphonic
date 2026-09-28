//! Agent-side plumbing shared by every adapter: where a transcript is kept on
//! disk, and the one funnel through which an adapter records and announces
//! what its agent did.
//!
//! The [`AgentManager`] that owns processes lives alongside this; what is here
//! is the part that has no process in it, so it can be tested on its own.

pub mod adapter;
pub mod backend;
pub mod claude;
pub mod claude_backend;
pub mod claude_stream;
pub mod credentials;
pub mod persist;
pub mod token;
pub mod token_scan;

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::server::broadcast::EventBus;
use crate::workspace::lifecycle::log_phase;
use crate::workspace::now_rfc3339;
use adapter::AgentAdapter;
use bondsymphonic_proto::*;
use parking_lot::Mutex;
use persist::{AgentRecord, AgentRecords};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

/// Transcripts on disk: one newline-delimited JSON file per agent, appended to
/// as messages arrive.
///
/// Append-only and line-oriented so that a daemon killed mid-write costs at
/// most the last message: [`read`](TranscriptStore::read) skips a line it
/// cannot parse rather than refusing the whole file, which is what makes a
/// torn final write survivable.
///
/// The file for a live agent is held open between messages. A streaming turn
/// arrives as one `AgentMessage` per text delta -- dozens a second -- and every
/// one of them used to cost a `create_dir_all`, an `open` and a `close` on the
/// path of the very task reading the agent's stdout. The handle is opened on
/// the agent's first message and let go when it ends, so a turn costs one open
/// rather than one per word.
pub struct TranscriptStore {
    dir: PathBuf,
    /// The open file of every agent that has spoken and not yet ended.
    ///
    /// The outer lock is taken only to look an agent up; the inner one is what
    /// an append holds, so two agents writing at once never wait for each
    /// other. `parking_lot` rather than tokio's, because nothing awaits while
    /// the map is locked and [`remove`](TranscriptStore::remove) is called from
    /// synchronous code.
    open: Mutex<HashMap<AgentId, Arc<tokio::sync::Mutex<tokio::fs::File>>>>,
    /// Whether the transcript directory has been made. It is made once, on the
    /// first agent to say anything, rather than at startup: a daemon whose
    /// agents never speak leaves no empty directory behind, and one whose
    /// agents do pays for it once.
    dir_made: std::sync::atomic::AtomicBool,
}

impl TranscriptStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            open: Mutex::new(HashMap::new()),
            dir_made: std::sync::atomic::AtomicBool::new(false),
        }
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

    /// The agent's open transcript, opening it on first use.
    ///
    /// Opened in append mode, so nothing that happens between two messages --
    /// this daemon losing the handle and taking a new one, a previous daemon's
    /// file already being there -- can write over what is already recorded.
    async fn file(
        &self,
        agent: &AgentId,
    ) -> std::io::Result<Arc<tokio::sync::Mutex<tokio::fs::File>>> {
        if let Some(file) = self.open.lock().get(agent) {
            return Ok(file.clone());
        }
        if !self.dir_made.load(Ordering::Relaxed) {
            tokio::fs::create_dir_all(&self.dir).await?;
            self.dir_made.store(true, Ordering::Relaxed);
        }
        let file = Arc::new(tokio::sync::Mutex::new(
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path(agent))
                .await?,
        ));
        // Two first messages for one agent can both have missed the map above.
        // Whichever landed first is the agent's handle from here on and the
        // other is dropped: both are append handles on the same file, so
        // neither could have lost anything, but one map entry per agent is what
        // makes `ended` a promise that the file is really closed.
        Ok(self
            .open
            .lock()
            .entry(agent.clone())
            .or_insert(file)
            .clone())
    }

    /// Appends one message.
    ///
    /// A write that fails takes the handle with it, so the next message opens
    /// the file again rather than going on writing into a descriptor the
    /// operating system has already given up on.
    pub async fn append(&self, agent: &AgentId, msg: &AgentMessage) -> std::io::Result<()> {
        let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
        line.push('\n');
        let file = self.file(agent).await?;
        let mut open = file.lock().await;
        let outcome = match open.write_all(line.as_bytes()).await {
            Ok(()) => open.flush().await,
            Err(e) => Err(e),
        };
        if outcome.is_err() {
            drop(open);
            self.ended(agent);
        }
        outcome
    }

    /// Lets go of the agent's open transcript.
    ///
    /// Called when the agent's process ends: the file is what the *live* agent
    /// writes through, while everything afterwards reads it by path. A message
    /// that arrives after this -- the exit's own, say -- simply opens it again.
    pub fn ended(&self, agent: &AgentId) {
        self.open.lock().remove(agent);
    }

    /// Deletes the transcript, if there is one.
    ///
    /// Only a workspace going away calls this: an agent's transcript outlives
    /// the agent's process on purpose, and the record beside it is what makes
    /// it readable again after a restart.
    pub fn remove(&self, agent: &AgentId) {
        // Closed before it is unlinked: on Windows an open handle refuses the
        // delete outright, and everywhere else a handle nobody can reach any
        // more would keep the bytes on disk until the daemon exited.
        self.ended(agent);
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

/// Runs one records-file write on a thread whose job is to block, and waits for
/// it there.
///
/// Every mutation of the records file is a read-modify-write that ends in an
/// `fsync` — milliseconds on a busy disk, unbounded on a failing one. The
/// callers are the task reading an agent's stdout and the handler answering
/// `agent.start`, both on tokio workers, and a worker inside `sync_all` is a
/// worker polling nothing at all: one slow disk stalls every other connection
/// the daemon is serving.
///
/// Awaited rather than abandoned, so the writes still happen in the order they
/// were asked for. That ordering is load-bearing: `agent.start` records the
/// agent before it spawns the process precisely so the first session id the
/// reader sees has a record to land in, and a write that could overtake another
/// would give that back. A running agent's own writes keep their order through
/// the records file's [`persist::WriteQueue`].
async fn off_the_runtime(what: &'static str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = tokio::task::spawn_blocking(f).await {
        warn!(error = %e, "{what} panicked");
    }
}

/// The environment given to the one command an agent's CLI runs -- never to
/// the sandbox spec, which every process in the workspace inherits.
///
/// An `api_key` the IDE sent is an explicit choice and wins over the
/// long-lived token; a blank key (the IDE's "unset" spelling) is treated as
/// absent. With neither, nothing is added: the CLI falls back to whatever
/// `.credentials.json` seeding already put in the workspace home.
pub(crate) fn agent_auth_env(
    api_key: Option<&str>,
    token: Option<String>,
) -> Vec<(String, String)> {
    match (api_key.filter(|k| !k.is_empty()), token) {
        (Some(key), _) => vec![("ANTHROPIC_API_KEY".to_owned(), key.to_owned())],
        (None, Some(t)) => vec![("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), t)],
        (None, None) => vec![],
    }
}

/// How long an agent's stdout reader waits for the session id it reported to
/// reach the records file before it reads on.
///
/// The id is what a client resumes the conversation with after a daemon
/// restart, so the reader used to wait for it outright: once a client had seen
/// anything after the `init` line, the id was on disk. On a disk a parallel
/// build holds up for seconds, that wait kept the reader from the line saying
/// why the turn failed until `stop` had given up and announced an exit that
/// could not say it. Bounded, the guarantee holds on any disk that answers in
/// this long, and the write stays queued on one that does not.
pub const SESSION_RECORD_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Queues `write` on the records file's [`persist::WriteQueue`], and answers a
/// receiver that resolves once it has run -- or has panicked, which is logged.
fn queue_write(
    records: &Arc<AgentRecords>,
    agent: Option<&AgentId>,
    what: &'static str,
    write: impl FnOnce(&AgentRecords) + Send + 'static,
) -> tokio::sync::oneshot::Receiver<()> {
    let (done, finished) = tokio::sync::oneshot::channel();
    let target = records.clone();
    let agent = agent.cloned();
    records.queue().push(Box::new(move || {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(&target)));
        if ran.is_err() {
            match &agent {
                Some(agent) => warn!(agent = %agent, "{what} panicked"),
                None => warn!("{what} panicked"),
            }
        }
        let _ = done.send(());
    }));
    finished
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
    /// Set by the first [`ended`](AgentSink::ended), shared by every clone. The
    /// record is closed once: the moment the process ended is what it wants,
    /// and a second write would only put an `fsync` in front of whoever asked.
    closed: Arc<AtomicBool>,
    /// The workspace home the agent ran with, so a login it refreshed can be
    /// written back when it exits: see [`credentials::write_back_login_from`].
    /// `None` in the unit tests below, which have no home.
    home: Option<PathBuf>,
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
            closed: Arc::new(AtomicBool::new(false)),
            home: None,
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

    /// Names the workspace home the agent runs with, so the login in it is
    /// written back to the daemon user when the agent exits.
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
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
    ///
    /// `Exited` closes the record first, which is a disk write. A caller that
    /// has claimed the one exit announcement must not wait on that between the
    /// claim and the publish; it calls [`ended`](AgentSink::ended) before
    /// claiming and [`publish_state`](AgentSink::publish_state) after.
    pub async fn state(&self, state: AgentState, detail: Option<String>) {
        // Before the event: a client that reacts to `Exited` by restarting the
        // daemon must not find the record still open.
        if state == AgentState::Exited {
            self.ended().await;
        }
        self.publish_state(state, detail);
    }

    /// [`state`](AgentSink::state) without closing the record: updates the
    /// entry and publishes, and cannot yield, so it completes once it starts.
    pub fn publish_state(&self, state: AgentState, detail: Option<String>) {
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
    /// with `options.resume_session`. The write is queued and waited for up to
    /// [`SESSION_RECORD_WAIT`]; the entry has the id at once, which is what
    /// everything in this daemon reads.
    pub async fn session_id(&self, id: String) {
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
        let Some(records) = &self.records else {
            return;
        };
        let agent = self.agent_id.clone();
        let written = queue_write(
            records,
            Some(&self.agent_id),
            "recording an agent's session id",
            move |records| records.update(&agent, |r| r.session_id = Some(id)),
        );
        if tokio::time::timeout(SESSION_RECORD_WAIT, written)
            .await
            .is_err()
        {
            warn!(agent = %self.agent_id, "the session id is not on disk yet; reading on");
        }
    }

    /// The agent's process is gone: closes its transcript file and its record,
    /// so the next daemon reports it as an agent that ended rather than one it
    /// lost.
    ///
    /// Idempotent, and it keeps the first answer: the moment the process ended
    /// is what the record wants, not the moment somebody noticed again. Only
    /// the first call writes; the others return at once, even while that write
    /// is still going, so the exit paths can each call it without queueing
    /// behind a disk that is slow or stuck.
    ///
    /// The first call waits for the write with no bound of its own. The exit
    /// paths bound it, and publish `Exited` when the bound runs out with the
    /// close still queued: on a disk that stuck, a restart can find the record
    /// open, and restores the agent as one that ended when the daemon
    /// restarted -- the alternative being an exit nobody ever announces.
    pub async fn ended(&self) {
        // Ahead of the record, and ahead of the early returns below: an agent
        // with no record on disk still holds a file descriptor, and a daemon
        // that runs for a week must not keep one per agent it has ever run.
        self.store.ended(&self.agent_id);
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // The process is gone, so its login file is settled: if it refreshed
        // the tokens, the host's refresh token is dead and this is the copy
        // that works. Ahead of the record close, which can queue behind a
        // disk that is stuck; a client that restarts the daemon on `Exited`
        // must find the login written back as surely as the record closed.
        if let Some(home) = self.home.clone() {
            off_the_runtime("writing a refreshed login back", move || {
                credentials::write_back_login(&home);
            })
            .await;
        }
        let Some(records) = &self.records else {
            return;
        };
        let agent = self.agent_id.clone();
        // Taken now, not when the queue gets to it: the moment the process
        // ended is what the record wants.
        let now = now_rfc3339();
        // Waited for, behind whatever was queued before it: a client that
        // reacts to `Exited` by restarting the daemon must find the record
        // closed.
        let _ = queue_write(
            records,
            Some(&self.agent_id),
            "closing an agent's record",
            move |records| {
                records.update(&agent, |r| {
                    r.ended_at.get_or_insert(now);
                })
            },
        )
        .await;
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn entry(&self) -> &Arc<AgentEntry> {
        &self.entry
    }
}

/// One agent the daemon knows about, live or not.
/// The half of an agent's start options that is safe to report back.
///
/// The full [`AgentStartOptions`] carries the user's API key, and the whole
/// point of holding these three separately is that there is then no field for
/// the key to travel in when `WorkspaceInfo` is built. See
/// [`bondsymphonic_proto::AgentSummary`].
#[derive(Clone, Default)]
struct StartedWith {
    command: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
}

impl StartedWith {
    fn from_options(options: &AgentStartOptions) -> Self {
        Self {
            command: options.command.clone(),
            model: options.model.clone(),
            permission_mode: options.permission_mode.clone(),
        }
    }
}

struct Agent {
    entry: Arc<AgentEntry>,
    /// Insertion order, so `records_of` reports agents in the order they were
    /// started rather than in whatever order the map happens to hold them.
    /// Restored agents are numbered first, in the order they were started, so a
    /// restart does not reshuffle a workspace's tabs.
    ordinal: u64,
    /// What this agent was started with, minus the API key: what a client needs
    /// to offer "start another one like this".
    started_with: StartedWith,
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

/// The `data.reason` on the `Conflict` a second simultaneous `agent.start` in
/// one workspace gets. Wire format, shared with the IDE by value.
///
/// It names a start that is in flight, which is not the same as an agent that
/// is running: a workspace with three agents in it takes a fourth start
/// perfectly happily, and what the gate refuses is only a *second start at the
/// same moment*. The earlier spelling, `agent_running`, described a rule the
/// daemon does not have, and a client acting on it would tell the user to stop
/// an agent when the answer is to try again in a second.
pub const REASON_AGENT_STARTING: &str = "agent_starting";

/// Every agent the daemon is running, and the requests that reach them.
///
/// The manager owns the list, not the workspace registry: an agent is a live
/// process, and the registry is a file that survives restarts. `records_of` is
/// what puts them back into [`WorkspaceInfo`].
pub struct AgentManager {
    events: EventBus,
    store: Arc<TranscriptStore>,
    agents: Mutex<HashMap<AgentId, Arc<Agent>>>,
    next_ordinal: AtomicU64,
    /// One gate per workspace, held for the whole of a start: see
    /// [`start`](AgentManager::start).
    starts: Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
    /// The agents on disk, which is what makes the map survivable: see
    /// [`restore`](AgentManager::restore).
    records: Arc<AgentRecords>,
    /// Which programs this daemon has already asked for a version: see
    /// [`claude::Probed`].
    probed: claude::Probed,
}

impl AgentManager {
    pub fn new(events: EventBus, transcripts: PathBuf, records: PathBuf) -> Self {
        Self {
            events,
            store: Arc::new(TranscriptStore::new(transcripts)),
            agents: Mutex::new(HashMap::new()),
            next_ordinal: AtomicU64::new(0),
            starts: Mutex::new(HashMap::new()),
            records: Arc::new(AgentRecords::new(records)),
            probed: claude::Probed::default(),
        }
    }

    /// Test hook: see [`AgentRecords::delay_closing_writes`].
    #[doc(hidden)]
    pub fn delay_record_closing_for_tests(&self, by: std::time::Duration) {
        self.records.delay_closing_writes(by);
    }

    /// Waits, for at most `within`, until every record write queued so far has
    /// happened. Answers whether they all did.
    ///
    /// For a daemon on its way out: the runtime does not outlive `main`, and a
    /// session id still queued at that point is a conversation the next daemon
    /// cannot offer to resume.
    pub async fn flush_records(&self, within: std::time::Duration) -> bool {
        let flushed = queue_write(&self.records, None, "flushing the agent records", |_| {});
        tokio::time::timeout(within, flushed).await.is_ok()
    }

    /// Test hook: see [`AgentRecords::delay_every_write`].
    #[doc(hidden)]
    pub fn delay_every_record_write_for_tests(&self, by: std::time::Duration) {
        self.records.delay_every_write(by);
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
                    started_with: StartedWith::from_options(&record.options),
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
        let backend = backend::backend_for(p.adapter)?;
        // Each phase up to the spawn is timed and said at `info`, the way the
        // startup restore's are (`lifecycle::log_phase`): an `agent.start`
        // sent while the daemon was still restoring once went unanswered for
        // the client's whole timeout, and nothing in the log said which of
        // these steps it was waiting in.
        let started = Instant::now();
        let phase = Instant::now();
        let ws = d.workspace(&p.workspace_id)?;
        log_phase(&ws.id, "agent.start", "workspace", phase);
        // `d.workspace` shows a workspace the startup restore has not reached
        // yet as `Creating`, so this is also what refuses a start against a
        // sandbox that is not there yet, at once rather than after a timeout.
        if ws.state != WorkspaceState::Ready {
            return Err(RpcError::invalid_params(format!(
                "workspace {} is not ready",
                ws.id
            )));
        }
        let phase = Instant::now();
        let handle = d.sandbox(&ws.id)?;
        log_phase(&ws.id, "agent.start", "sandbox", phase);
        // One start at a time in a workspace, from here to the moment the
        // process is up.
        //
        // Everything below writes into the *workspace's* home before it spawns
        // anything, and the settings copy in particular unlinks the destination
        // and then creates it: two starts at once leave the second agent's
        // settings file missing or half written, and neither agent is running
        // under the settings its repository pins. Refused rather than queued,
        // because the client that asked has a user waiting on it, and a start
        // held behind a spawn is a start that looks hung.
        let gate = {
            let mut gates = self.starts.lock();
            gates
                .entry(ws.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let phase = Instant::now();
        let _gate = gate.try_lock_owned().map_err(|_| {
            RpcError::new(
                ErrorCode::Conflict,
                format!("another agent is already starting in workspace {}", ws.id),
            )
            .with_data(serde_json::json!({ "reason": REASON_AGENT_STARTING }))
        })?;
        log_phase(&ws.id, "agent.start", "gate", phase);
        let prepared = backend.prepare(d, &ws, &p.options).await?;
        let home = d.dirs.home(&ws.id);

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
        let sink = if p.adapter == AgentAdapterKind::Claude {
            sink.with_home(home)
        } else {
            sink
        };
        let mut adapter = backend.adapter(sink, handle, prepared);
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
        let started_with = StartedWith::from_options(&p.options);
        let record = AgentRecord {
            agent_id: id.clone(),
            workspace_id: ws.id.clone(),
            adapter: p.adapter,
            session_id: p.options.resume_session.clone(),
            options: p.options,
            started_at: now_rfc3339(),
            ended_at: None,
        };
        let records = self.records.clone();
        let phase = Instant::now();
        off_the_runtime("recording a started agent", move || records.upsert(record)).await;
        log_phase(&ws.id, "agent.start", "records.upsert", phase);
        // Registered only once it is really running, so a failed start leaves
        // no agent behind for the IDE to find -- and no record either, since
        // nothing ever ran under this id.
        let phase = Instant::now();
        let spawned = adapter.start().await;
        log_phase(&ws.id, "agent.start", "adapter.start", phase);
        if let Err(e) = spawned {
            let records = self.records.clone();
            let gone = id.clone();
            off_the_runtime("taking back a failed start's record", move || {
                records.remove(&gone)
            })
            .await;
            return Err(e);
        }
        self.agents.lock().insert(
            id.clone(),
            Arc::new(Agent {
                entry,
                ordinal: self.next_ordinal.fetch_add(1, Ordering::SeqCst),
                started_with,
                adapter: Some(tokio::sync::Mutex::new(adapter)),
            }),
        );
        log_phase(&ws.id, "agent.start", "total", started);
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

    /// The agents belonging to `ws`, oldest first, with everything a client
    /// needs to rebuild a tab for one.
    ///
    /// The id alone does not let a client that restarted do that: it cannot
    /// tell a Claude agent, whose transcript this daemon is still serving and
    /// whose session is still resumable, from a plain terminal. So each entry
    /// carries the adapter, the current state, the session id and the
    /// non-secret half of the options the agent was started with.
    ///
    /// Oldest first, and restored agents were given their ordinals before any
    /// new one could take theirs, so the last entry is the workspace's most
    /// recent agent whether or not this daemon started it.
    pub fn records_of(&self, ws: &WorkspaceId) -> Vec<AgentSummary> {
        let mut found: Vec<(u64, AgentSummary)> = self
            .agents
            .lock()
            .iter()
            .filter(|(_, a)| &a.entry.workspace_id == ws)
            .map(|(id, a)| {
                (
                    a.ordinal,
                    AgentSummary {
                        id: id.clone(),
                        adapter: a.entry.adapter,
                        state: a.entry.state().0,
                        session_id: a.entry.session_id.lock().clone(),
                        command: a.started_with.command.clone(),
                        model: a.started_with.model.clone(),
                        permission_mode: a.started_with.permission_mode.clone(),
                    },
                )
            })
            .collect();
        found.sort_by_key(|(ordinal, _)| *ordinal);
        found.into_iter().map(|(_, summary)| summary).collect()
    }

    /// The same agents as [`records_of`](AgentManager::records_of), by id alone
    /// and in the same order. `WorkspaceInfo` carries both, so a client older
    /// than the records field still reads the list it has always read.
    pub fn agents_of(&self, ws: &WorkspaceId) -> Vec<AgentId> {
        self.records_of(ws).into_iter().map(|a| a.id).collect()
    }

    /// Stops every agent in `ws` without forgetting any of them. Called before a
    /// workspace's sandbox is torn down, so the agents end through their own
    /// `stop` path (stdin closed, exit awaited, `Exited` announced) instead of
    /// vanishing with the sandbox.
    ///
    /// The agents stay in the map. A destroy can still fail after this point --
    /// a worktree git will not delete, a repository that has gone read-only --
    /// and it leaves the workspace behind in `Error` with its agents' history
    /// the one thing worth having out of it. Forgetting them here made
    /// `agent.history` a `NotFound` for a workspace the user can still see.
    /// [`forget_workspace`](AgentManager::forget_workspace) is what removes
    /// them, and the destroy calls it only once the worktree is really gone.
    pub async fn stop_all_in(&self, ws: &WorkspaceId) {
        let victims: Vec<(AgentId, Arc<Agent>)> = self
            .agents
            .lock()
            .iter()
            .filter(|(_, a)| &a.entry.workspace_id == ws)
            .map(|(id, a)| (id.clone(), a.clone()))
            .collect();
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
    /// The transcripts to delete come from the file as well as from the map, so
    /// an agent this daemon never held an entry for -- one restored and then
    /// stopped, or one whose entry was lost -- takes its transcript with it too.
    pub fn forget_workspace(&self, ws: &WorkspaceId) {
        let mut gone: Vec<AgentId> = {
            let mut agents = self.agents.lock();
            let ids: Vec<AgentId> = agents
                .iter()
                .filter(|(_, a)| &a.entry.workspace_id == ws)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                agents.remove(id);
            }
            ids
        };
        // The workspace is gone, so the gate that serialised its starts is one
        // entry nobody will ask for again.
        self.starts.lock().remove(ws);
        for id in self.records.remove_workspace(ws) {
            if !gone.contains(&id) {
                gone.push(id);
            }
        }
        for id in &gone {
            self.store.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_key_the_ide_sent_wins_over_the_token() {
        let env = agent_auth_env(Some("sk-ant-api03-k"), Some("sk-ant-oat01-t".into()));
        assert_eq!(
            env,
            vec![("ANTHROPIC_API_KEY".to_owned(), "sk-ant-api03-k".to_owned())]
        );
    }

    #[test]
    fn the_token_is_given_when_there_is_no_api_key() {
        for key in [None, Some("")] {
            let env = agent_auth_env(key, Some("sk-ant-oat01-t".into()));
            assert_eq!(
                env,
                vec![(
                    "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                    "sk-ant-oat01-t".to_owned()
                )]
            );
        }
    }

    #[test]
    fn nothing_is_given_when_there_is_neither() {
        assert!(agent_auth_env(None, None).is_empty());
    }

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

    /// A streaming turn is one message per text delta, dozens a second, and
    /// every one of them arrives on the task reading the agent's stdout. One
    /// handle per agent is what keeps that from being one `create_dir_all`, one
    /// `open` and one `close` per word.
    ///
    /// The handle is the live agent's alone: it is let go when the agent ends,
    /// and a message that arrives after that opens the file again rather than
    /// being lost.
    #[tokio::test]
    async fn one_handle_serves_an_agent_for_as_long_as_it_runs_and_no_longer() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::new(dir.path().join("transcripts"));
        let one: AgentId = "ag_one".into();
        let two: AgentId = "ag_two".into();
        let say = |n: u64, text: &str| AgentMessage {
            seq: n,
            ts: "2026-09-11T00:00:00Z".into(),
            body: AgentMessageBody::AssistantDelta { text: text.into() },
        };

        assert!(store.open.lock().is_empty(), "nothing is opened eagerly");
        for i in 1..=5u64 {
            store.append(&one, &say(i, &format!("d{i}"))).await.unwrap();
        }
        store.append(&two, &say(1, "other")).await.unwrap();
        assert_eq!(
            store.open.lock().len(),
            2,
            "one handle each, however many messages"
        );

        // Ended: the handle goes, and every byte is on disk without it.
        store.ended(&one);
        assert_eq!(store.open.lock().len(), 1);
        assert_eq!(store.read(&one).await.unwrap().len(), 5);

        // And a late message is appended to what is already there rather than
        // over it.
        store.append(&one, &say(6, "last")).await.unwrap();
        let all = store.read(&one).await.unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[5].seq, 6);

        // A workspace going away takes the handle with the file: an open one
        // refuses the delete outright on Windows.
        store.remove(&one);
        store.remove(&two);
        assert!(store.open.lock().is_empty());
        assert!(store.read(&one).await.unwrap().is_empty());
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
        s.session_id("sess-1".into()).await;
        records.upsert(blank());
        s.session_id("sess-1".into()).await;
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
        s.session_id("sess-1".into()).await;
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-1"));
        s.session_id("sess-1".into()).await;
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-1"));
        // And a session that really does move is followed.
        s.session_id("sess-2".into()).await;
        assert_eq!(records.load()[0].session_id.as_deref(), Some("sess-2"));
    }

    /// `ended()` closes the record and is idempotent: an agent that dies the
    /// instant it starts must not come back from a restart as one the daemon
    /// thinks it lost, and a second exit report must not move the timestamp.
    #[tokio::test]
    async fn an_exit_closes_the_record_once_and_keeps_the_first_answer() {
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
        sink.ended().await;
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
