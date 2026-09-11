//! Fans one daemon event stream out to the consumers that care about it.
//!
//! The reader loop in `AppController::start` owns the single [`EventStream`] the
//! client hands back and calls [`EventRouter::dispatch`] for every event. Five
//! kinds of consumer subscribe here:
//!
//! * [`EventRouter::subscribe_all`] — every event, in order. Used for
//!   workspace-state and daemon-log handling.
//! * [`EventRouter::subscribe_pty`] — only `pty.output`/`pty.exit` for one PTY.
//! * [`EventRouter::subscribe_agent`] — only `agent.message`/`agent.state` for
//!   one agent.
//! * [`EventRouter::subscribe_run`] — only `run.output`/`run.state` for one
//!   run.
//! * [`EventRouter::subscribe_fs`] — only `fs.changed` for one workspace. Every
//!   open editor takes one of these, rather than the whole stream it would
//!   otherwise have to filter.
//!
//! All four per-id kinds share one table keyed by [`StreamKey`], so a PTY, an
//! agent and a run whose ids collide as strings still get separate streams.
//! They differ in how many consumers a key may have, which is [`Fanout`]: a
//! PTY, an agent and a run are each shown in one place, while a workspace can
//! have any number of editors open on it at once.
//!
//! A PTY's first output, an agent's first messages and a run's banner usually
//! arrive before the widget that will display them has finished being constructed and
//! subscribed, so unclaimed per-id events are parked in a short-lived *early
//! buffer* and replayed on subscription. Worktree changes are not parked: a
//! change from before a file was opened says nothing about the file that is
//! open now, and replaying one would make a fresh editor reload or prompt.
//!
//! This module must never import Qt types.
//!
//! [`EventStream`]: crate::client::EventStream

use bondsymphonic_proto::{AgentId, Event, PtyId, RunId, WorkspaceId};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Receiving end of a subscription. Unbounded so that `dispatch` never blocks
/// the reader loop: a slow consumer costs memory, not the whole event stream.
pub type EventRx = mpsc::UnboundedReceiver<(Option<WorkspaceId>, Event)>;
type EventTx = mpsc::UnboundedSender<(Option<WorkspaceId>, Event)>;

/// How long output that arrives before anyone subscribes is kept.
pub const EARLY_BUFFER_TTL: Duration = Duration::from_secs(5);

/// Most events parked for one not-yet-subscribed stream. Older events are
/// dropped first, so what survives is the tail the view would have shown
/// anyway.
pub const EARLY_BUFFER_CAP: usize = 256;

/// What a per-id subscription is for. Four id spaces, one table: the daemon's
/// prefixes make a collision unlikely, but nothing in the protocol forbids one
/// and a terminal fed an agent's messages would be a mystery to debug.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamKey {
    Pty(PtyId),
    Agent(AgentId),
    Run(RunId),
    /// Worktree changes for one workspace.
    Fs(WorkspaceId),
}

/// How many consumers a key may have at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fanout {
    /// One, and subscribing again replaces it: a PTY, an agent or a run is
    /// shown in exactly one widget, and a re-opened one must not be shadowed
    /// by the consumer it replaced. Early-buffered events go to it.
    Exclusive,
    /// Any number, each hearing every event: several editors can be open on
    /// one workspace, and each of them needs its own copy.
    Shared,
}

impl StreamKey {
    pub fn fanout(&self) -> Fanout {
        match self {
            StreamKey::Pty(_) | StreamKey::Agent(_) | StreamKey::Run(_) => Fanout::Exclusive,
            StreamKey::Fs(_) => Fanout::Shared,
        }
    }
}

/// Identifies one subscription among the ones taken on the same key.
///
/// Handed out by [`EventRouter::subscribe_stream`] and given back to
/// [`EventRouter::unsubscribe_stream`], which is what keeps a consumer that has
/// already been replaced from cancelling the one that replaced it as it winds
/// down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubToken(u64);

/// One subscription: where to send, and which subscription it is.
struct Sub {
    token: SubToken,
    tx: EventTx,
}

/// A consumer's hold on one stream, and the only way it ends its own
/// subscription.
///
/// Every per-id consumer finishes the same way: its channel closes because the
/// subscription was replaced, or the QObject it feeds goes away, and it lets
/// go. Letting go *by key* is what IM6 was -- the replaced pump wakes precisely
/// because the replacement took its key, and removing by key then takes the
/// replacement with it. A `Release` carries the token as well as the key, so
/// there is no key-shaped release for a tail path to reach for by mistake.
///
/// Retiring the id itself is a different act and keeps its own method
/// ([`EventRouter::unsubscribe_pty`] and friends): that one is meant to take
/// everything, including anything parked in the early buffer.
#[derive(Clone)]
pub struct Release {
    router: EventRouter,
    key: StreamKey,
    token: SubToken,
}

impl Release {
    /// Ends exactly the subscription this handle was given out for. Safe to
    /// call from a tail path, a `Drop`, or twice.
    pub fn release(&self) {
        self.router.unsubscribe_stream(&self.key, self.token);
    }

    /// The stream this hold is on.
    pub fn key(&self) -> &StreamKey {
        &self.key
    }
}

#[derive(Default)]
struct Inner {
    all: Vec<EventTx>,
    streams: HashMap<StreamKey, Vec<Sub>>,
    early: HashMap<StreamKey, Vec<(Instant, Option<WorkspaceId>, Event)>>,
    /// Never reused, so a token can only ever name the subscription it was
    /// handed out for.
    next_token: u64,
}

impl Inner {
    /// Drops buffered events at least `age` old, and any id left with none.
    fn prune(&mut self, age: Duration) {
        self.early.retain(|_, buf| {
            buf.retain(|(at, _, _)| at.elapsed() < age);
            !buf.is_empty()
        });
    }

    fn mint(&mut self) -> SubToken {
        self.next_token += 1;
        SubToken(self.next_token)
    }
}

/// Cheap to clone; every clone talks to the same subscriber table.
#[derive(Clone, Default)]
pub struct EventRouter {
    inner: Arc<Mutex<Inner>>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event, in order.
    pub fn subscribe_all(&self) -> EventRx {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().all.push(tx);
        rx
    }

    /// Only events carrying `id`. Anything parked in the early buffer for that
    /// id is replayed first, so no output is lost between `pty.open` returning
    /// and the terminal widget subscribing. Subscribing twice for the same id
    /// replaces the earlier subscription (its receiver then sees the channel
    /// close), which keeps a re-opened terminal from being shadowed by a stale
    /// consumer.
    ///
    /// The [`Release`] handed back with the receiver is how the consumer ends
    /// *its own* subscription when its pump stops; see the type's own note on
    /// why it is not a bare key.
    pub fn subscribe_pty(&self, id: &PtyId) -> (EventRx, Release) {
        self.subscribe_held(StreamKey::Pty(id.clone()))
    }

    /// Retires the id: ends whatever subscription holds it and discards
    /// anything parked for it, since a closed PTY's output must not be
    /// replayed onto a later terminal reusing the id.
    ///
    /// For closing the PTY itself, not for a consumer letting go — a consumer
    /// that has been replaced would take the replacement's subscription with
    /// it. That one uses the [`Release`] it was given.
    pub fn unsubscribe_pty(&self, id: &PtyId) {
        self.unsubscribe_key(&StreamKey::Pty(id.clone()));
    }

    /// Only `agent.message`/`agent.state` for `id`, with the same early-buffer
    /// replay as [`EventRouter::subscribe_pty`]: an agent starts talking as
    /// soon as `agent.start` returns, which is before the transcript model has
    /// the id it needs in order to subscribe. Subscribing twice for the same
    /// id replaces the earlier subscription.
    pub fn subscribe_agent(&self, id: &AgentId) -> (EventRx, Release) {
        self.subscribe_held(StreamKey::Agent(id.clone()))
    }

    /// Retires the id, discarding anything parked for it, so a stopped
    /// agent's tail is not replayed onto a later transcript. Like
    /// [`EventRouter::unsubscribe_pty`], this is about the id and not about one
    /// consumer of it; a consumer letting go uses its [`Release`].
    pub fn unsubscribe_agent(&self, id: &AgentId) {
        self.unsubscribe_key(&StreamKey::Agent(id.clone()));
    }

    /// Only `run.output`/`run.state` for `id`, with the same early-buffer
    /// replay as [`EventRouter::subscribe_pty`]: a dev server prints its banner
    /// (often the very line whose `ready_regex` marks it up) between
    /// `run.start` returning and the Run panel subscribing with the id that
    /// reply carried. Subscribing twice for the same id replaces the earlier
    /// subscription.
    pub fn subscribe_run(&self, id: &RunId) -> (EventRx, Release) {
        self.subscribe_held(StreamKey::Run(id.clone()))
    }

    /// Retires the id, discarding anything parked for it, so a finished run's
    /// tail cannot be replayed into the log of a later run of the same
    /// configuration. About the id, not about one consumer of it; a consumer
    /// letting go uses its [`Release`].
    pub fn unsubscribe_run(&self, id: &RunId) {
        self.unsubscribe_key(&StreamKey::Run(id.clone()));
    }

    /// Only `fs.changed` for `workspace`, which is all an open editor or the
    /// changed-files list has ever wanted out of the stream.
    ///
    /// Shared, unlike the per-id streams: every editor open on the workspace
    /// gets its own copy, and none of them is replaced by the next one to open.
    /// Ending it is just dropping the receiver -- the next dispatch notices the
    /// closed channel and forgets it -- so there is no unsubscribe to forget to
    /// call from a Drop.
    pub fn subscribe_fs(&self, workspace: &WorkspaceId) -> EventRx {
        self.subscribe_stream(StreamKey::Fs(workspace.clone())).0
    }

    /// Subscribes to one stream, whatever kind of key it is, and hands back the
    /// token that names this subscription.
    ///
    /// A [`Fanout::Exclusive`] key replaces whoever held it and is replayed
    /// anything parked for it; a [`Fanout::Shared`] key simply gains one more
    /// listener.
    /// [`EventRouter::subscribe_stream`] with the token already wrapped in the
    /// [`Release`] that ends it.
    fn subscribe_held(&self, key: StreamKey) -> (EventRx, Release) {
        let (rx, token) = self.subscribe_stream(key.clone());
        let release = Release {
            router: self.clone(),
            key,
            token,
        };
        (rx, release)
    }

    pub fn subscribe_stream(&self, key: StreamKey) -> (EventRx, SubToken) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.lock();
        let token = inner.mint();
        match key.fanout() {
            Fanout::Exclusive => {
                if let Some(buf) = inner.early.remove(&key) {
                    for (_, ws, ev) in buf {
                        let _ = tx.send((ws, ev));
                    }
                }
                inner.streams.insert(key, vec![Sub { token, tx }]);
            }
            Fanout::Shared => inner
                .streams
                .entry(key)
                .or_default()
                .push(Sub { token, tx }),
        }
        (rx, token)
    }

    /// Ends exactly the subscription `token` names, and nothing else.
    ///
    /// The token is what makes this safe to call from a consumer's tail: a
    /// terminal that has already been replaced on its id unsubscribes as its
    /// task winds down, and by key alone that call would take the live
    /// subscription with it. A token that names no current subscription -- the
    /// usual case for that tail -- does nothing at all.
    pub fn unsubscribe_stream(&self, key: &StreamKey, token: SubToken) {
        let mut inner = self.lock();
        let Some(subs) = inner.streams.get_mut(key) else {
            return;
        };
        subs.retain(|sub| sub.token != token);
        if subs.is_empty() {
            inner.streams.remove(key);
            // Nobody is left to be shown it, and the next consumer of this id
            // is a different PTY, agent or run.
            inner.early.remove(key);
        }
    }

    /// Ends every subscription on `key` and discards anything parked for it,
    /// for the callers that hold no token: closing a PTY, an agent or a run is
    /// about the id, not about one consumer of it.
    fn unsubscribe_key(&self, key: &StreamKey) {
        let mut inner = self.lock();
        inner.streams.remove(key);
        inner.early.remove(key);
    }

    /// Called by the reader loop for every event received from the daemon.
    pub fn dispatch(&self, workspace_id: Option<WorkspaceId>, event: Event) {
        let mut inner = self.lock();
        // A dropped receiver un-registers its "all" subscription implicitly.
        inner
            .all
            .retain(|tx| tx.send((workspace_id.clone(), event.clone())).is_ok());

        let Some(key) = stream_key_of(workspace_id.as_ref(), &event) else {
            inner.prune(EARLY_BUFFER_TTL);
            return;
        };
        match key.fanout() {
            Fanout::Exclusive => {
                // Carried in an `Option` so the payload moves at most once: if
                // the registered subscriber has gone away, the send hands it
                // back and the event falls through into the early buffer.
                let mut undelivered = Some((workspace_id, event));
                if let Entry::Occupied(slot) = inner.streams.entry(key.clone()) {
                    // One subscriber, by construction: the `Exclusive` arm of
                    // `subscribe_stream` inserts exactly one and
                    // `unsubscribe_stream` drops the key when the last goes.
                    // Read rather than asserted, so a future mistake elsewhere
                    // cannot take the reader loop down from here.
                    if let Some(sub) = slot.get().first() {
                        let item = undelivered.take().expect("payload not yet delivered");
                        if let Err(returned) = sub.tx.send(item) {
                            slot.remove();
                            undelivered = Some(returned.0);
                        }
                    }
                }
                if let Some((ws, ev)) = undelivered {
                    let buf = inner.early.entry(key).or_default();
                    if buf.len() >= EARLY_BUFFER_CAP {
                        buf.remove(0);
                    }
                    buf.push((Instant::now(), ws, ev));
                }
            }
            // Every listener gets a copy, and one whose receiver has been
            // dropped is forgotten here rather than needing an unsubscribe.
            // Nothing is parked: see the note on the early buffer above.
            Fanout::Shared => {
                if let Entry::Occupied(mut slot) = inner.streams.entry(key) {
                    slot.get_mut()
                        .retain(|sub| sub.tx.send((workspace_id.clone(), event.clone())).is_ok());
                    if slot.get().is_empty() {
                        slot.remove();
                    }
                }
            }
        }
        inner.prune(EARLY_BUFFER_TTL);
    }

    /// Pruning hook. `dispatch` calls this with [`EARLY_BUFFER_TTL`]; tests use
    /// it to age the buffer out deterministically instead of sleeping.
    pub fn expire_early_buffers_older_than(&self, age: Duration) {
        self.lock().prune(age);
    }

    /// The lock is only ever held for the duration of a table update, and no
    /// user code runs while it is held, so poisoning would mean a bug here.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("event router mutex poisoned")
    }
}

/// The per-id stream an event belongs to, for the events that belong to one.
///
/// `fs.changed` is the one that needs the envelope rather than the payload: the
/// worktree it describes is named by the message's `workspace_id`, and one with
/// no workspace at all is the daemon talking about itself and belongs to no
/// editor.
fn stream_key_of(workspace_id: Option<&WorkspaceId>, event: &Event) -> Option<StreamKey> {
    match event {
        Event::FsChanged { .. } => workspace_id.cloned().map(StreamKey::Fs),
        Event::PtyOutput { pty_id, .. } | Event::PtyExit { pty_id, .. } => {
            Some(StreamKey::Pty(pty_id.clone()))
        }
        Event::AgentMessage { agent_id, .. } | Event::AgentStateChanged { agent_id, .. } => {
            Some(StreamKey::Agent(agent_id.clone()))
        }
        Event::RunOutput { run_id, .. } | Event::RunStateChanged { run_id, .. } => {
            Some(StreamKey::Run(run_id.clone()))
        }
        _ => None,
    }
}
