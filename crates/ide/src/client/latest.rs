//! Only the newest of a run of requests is answered.
//!
//! The New Agent dialog asks `repo.inspect` about one folder at a time, and on
//! a repository reached through `/mnt/c` the answer can take minutes. The user
//! may pick another folder in the meantime, or press Stop, or close the dialog
//! and open another; in every one of those cases the answer still on its way
//! belongs to a question nobody is asking any more. The daemon has no way to be
//! told to stop (the protocol has no per-request cancel), so the answer is
//! dropped on this side instead -- and the wait for it ends at once rather than
//! when the daemon finishes.

use std::future::Future;
use tokio::sync::watch;

/// A generation counter that each new request, and each cancel, moves on.
///
/// Cheap to clone: every clone shares the one counter.
#[derive(Clone)]
pub struct LatestOnly {
    generation: watch::Sender<u64>,
}

impl Default for LatestOnly {
    fn default() -> Self {
        // The receiver is dropped at once; every ticket subscribes its own, and
        // `send_modify` counts whether or not anything is listening.
        Self {
            generation: watch::channel(0).0,
        }
    }
}

impl LatestOnly {
    /// Starts a request, superseding whichever one was current.
    pub fn begin(&self) -> Ticket {
        self.bump();
        Ticket {
            mine: *self.generation.borrow(),
            generation: self.generation.subscribe(),
        }
    }

    /// Drops whichever request is current, without starting another.
    pub fn cancel(&self) {
        self.bump();
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g += 1);
    }
}

/// One request's claim to be the current one.
pub struct Ticket {
    mine: u64,
    generation: watch::Receiver<u64>,
}

impl Ticket {
    /// Whether nothing has superseded or cancelled this request yet.
    pub fn is_current(&self) -> bool {
        *self.generation.borrow() == self.mine
    }

    /// Runs `work` while this ticket is current: `None` as soon as it is not,
    /// without waiting for `work` to finish, and `None` for an answer that
    /// arrives after it stopped being current. Dropping `work` early is what
    /// ends the wait; the request's slot in the client goes with it.
    pub async fn run<F: Future>(self, work: F) -> Option<F::Output> {
        let mine = self.mine;
        let mut watching = self.generation.clone();
        let superseded = async move {
            // A counter that is gone can never move on again, so this request
            // stays current and is waited for like any other.
            if watching.wait_for(|g| *g != mine).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = superseded => None,
            out = work => self.is_current().then_some(out),
        }
    }

    /// A cheap handle that answers [`Ticket::is_current`] later, for the check
    /// made on the Qt thread just before an answer is emitted.
    pub fn handle(&self) -> TicketHandle {
        TicketHandle {
            mine: self.mine,
            generation: self.generation.clone(),
        }
    }
}

/// [`Ticket::is_current`] without the ticket, for a closure queued elsewhere.
pub struct TicketHandle {
    mine: u64,
    generation: watch::Receiver<u64>,
}

impl TicketHandle {
    pub fn is_current(&self) -> bool {
        *self.generation.borrow() == self.mine
    }
}
