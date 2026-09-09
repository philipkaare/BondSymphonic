//! Fans one daemon event stream out to the consumers that care about it.
//!
//! The reader loop in `AppController::start` owns the single [`EventStream`] the
//! client hands back and calls [`EventRouter::dispatch`] for every event. Two
//! kinds of consumer subscribe here:
//!
//! * [`EventRouter::subscribe_all`] — every event, in order. Used for
//!   workspace-state and daemon-log handling.
//! * [`EventRouter::subscribe_pty`] — only `pty.output`/`pty.exit` for one PTY.
//!
//! A PTY's first output usually arrives before the widget that will display it
//! has finished being constructed and subscribed, so unclaimed PTY events are
//! parked in a short-lived per-id *early buffer* and replayed on subscription.
//!
//! This module must never import Qt types.
//!
//! [`EventStream`]: crate::client::EventStream

use bondsymphonic_proto::{Event, PtyId, WorkspaceId};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Receiving end of a subscription. Unbounded so that `dispatch` never blocks
/// the reader loop: a slow consumer costs memory, not the whole event stream.
pub type EventRx = mpsc::UnboundedReceiver<(Option<WorkspaceId>, Event)>;
type EventTx = mpsc::UnboundedSender<(Option<WorkspaceId>, Event)>;

/// How long PTY output that arrives before anyone subscribes is kept.
pub const EARLY_BUFFER_TTL: Duration = Duration::from_secs(5);

/// Most events parked for one not-yet-subscribed PTY. Older events are dropped
/// first, so what survives is the tail the terminal would have shown anyway.
pub const EARLY_BUFFER_CAP: usize = 256;

#[derive(Default)]
struct Inner {
    all: Vec<EventTx>,
    pty: HashMap<PtyId, EventTx>,
    early: HashMap<PtyId, Vec<(Instant, Option<WorkspaceId>, Event)>>,
}

impl Inner {
    /// Drops buffered events at least `age` old, and any id left with none.
    fn prune(&mut self, age: Duration) {
        self.early.retain(|_, buf| {
            buf.retain(|(at, _, _)| at.elapsed() < age);
            !buf.is_empty()
        });
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
    pub fn subscribe_pty(&self, id: &PtyId) -> EventRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.lock();
        if let Some(buf) = inner.early.remove(id) {
            for (_, ws, ev) in buf {
                let _ = tx.send((ws, ev));
            }
        }
        inner.pty.insert(id.clone(), tx);
        rx
    }

    /// Ends the subscription for `id`: dropping the sender closes the
    /// receiver, so the consumer sees `None` rather than hanging. Anything
    /// still parked for that id is discarded too, since a closed PTY's output
    /// must not be replayed onto a later terminal reusing the id.
    pub fn unsubscribe_pty(&self, id: &PtyId) {
        let mut inner = self.lock();
        inner.pty.remove(id);
        inner.early.remove(id);
    }

    /// Called by the reader loop for every event received from the daemon.
    pub fn dispatch(&self, workspace_id: Option<WorkspaceId>, event: Event) {
        let mut inner = self.lock();
        // A dropped receiver un-registers its "all" subscription implicitly.
        inner
            .all
            .retain(|tx| tx.send((workspace_id.clone(), event.clone())).is_ok());

        if let Some(id) = pty_id_of(&event).cloned() {
            // Carried in an `Option` so the payload moves at most once: if the
            // registered subscriber has gone away, the send hands it back and
            // the event falls through into the early buffer.
            let mut undelivered = Some((workspace_id, event));
            if let Entry::Occupied(slot) = inner.pty.entry(id.clone()) {
                let item = undelivered.take().expect("payload not yet delivered");
                if let Err(returned) = slot.get().send(item) {
                    slot.remove();
                    undelivered = Some(returned.0);
                }
            }
            if let Some((ws, ev)) = undelivered {
                let buf = inner.early.entry(id).or_default();
                if buf.len() >= EARLY_BUFFER_CAP {
                    buf.remove(0);
                }
                buf.push((Instant::now(), ws, ev));
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

/// The PTY an event belongs to, for the events that belong to one.
fn pty_id_of(event: &Event) -> Option<&PtyId> {
    match event {
        Event::PtyOutput { pty_id, .. } | Event::PtyExit { pty_id, .. } => Some(pty_id),
        _ => None,
    }
}
