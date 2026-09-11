//! The last few lines a child process said, and the exit detail built out of
//! them.
//!
//! Two things in the daemon watch a process die and have to explain it: a run
//! ([`crate::runs::manager`]) and an agent ([`crate::agents::claude`]). Both
//! keep a bounded ring of the most recent output lines, both build a sentence
//! out of that ring and the exit code, and both wrap the backend's exit channel
//! in a future that more than one waiter can hold.
//!
//! What they do *not* share is the order the sentence reads in, which is why
//! [`exit_detail`] takes a [`Layout`] rather than picking one. See the variants
//! for why each of them is the way round it is.

use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// The most recent lines of one process's output.
///
/// Bounded, and cheap to clone: the reader task that fills it and the teardown
/// path that reads it hold the same ring. A process is free to print for as
/// long as it lives, and what it said an hour ago explains nothing about the
/// way it went, so only the end of it is worth the memory.
#[derive(Clone)]
pub struct Tail {
    lines: Arc<Mutex<VecDeque<String>>>,
    /// How many lines are kept. Enough to carry a login prompt or a stack
    /// trace, never enough to fill an event.
    keep: usize,
}

impl Tail {
    pub fn new(keep: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
            keep,
        }
    }

    /// Adds `line`, dropping the oldest once the ring is full.
    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock();
        if lines.len() == self.keep {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Whether the process has said anything worth repeating.
    pub fn is_empty(&self) -> bool {
        self.lines.lock().is_empty()
    }

    /// What is kept, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().iter().cloned().collect()
    }
}

/// Which way round an exit detail reads.
///
/// The choice is not cosmetic: the whole string is what a client shows a person
/// in one line of a banner, so whichever half comes first is the half that
/// survives being cut off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// `<what it said> (exit code N)`.
    ///
    /// For an agent, whose commonest way to die is dying immediately -- not
    /// logged in, no API key, a bad flag. "Invalid API key" is the part a
    /// person can act on, so it has to lead rather than trail a sentence about
    /// an exit code.
    TailFirst,
    /// `exit code N` with what it said on the lines below.
    ///
    /// For a run, whose output is a server log the IDE is already showing in
    /// full beside this. What the detail adds is the status, and the lines
    /// below it are there to save the reader a scroll.
    CodeFirst,
}

/// Why a process went: its exit code and the last it said, in `layout`'s order.
///
/// A process that said nothing gets the code alone, in both layouts: there is
/// no tail to put on either side of it.
pub fn exit_detail(code: i32, tail: &Tail, layout: Layout) -> String {
    let lines = tail.lines();
    if lines.is_empty() {
        return format!("exit code {code}");
    }
    let said = lines.join("\n");
    match layout {
        Layout::TailFirst => format!("{said} (exit code {code})"),
        Layout::CodeFirst => format!("exit code {code}\n{said}"),
    }
}

/// A child's exit code, awaitable by more than one waiter and more than once.
///
/// The backend hands out a `oneshot`, which exactly one waiter may take. Both a
/// run and an agent have at least two -- the supervisor or reader watching for
/// the death, and the stop path waiting out its own grace -- and neither may
/// consume the answer out from under the other.
pub type ExitCode = Shared<BoxFuture<'static, i32>>;

/// The code a process gets when the backend never reported one.
///
/// A dropped sender means the backend lost track of the child rather than that
/// the child exited zero, and no real status is negative, so this cannot be
/// mistaken for one.
pub const NO_CODE: i32 = -1;

/// Wraps the backend's exit channel so every waiter can hold it.
pub fn exit_code(rx: tokio::sync::oneshot::Receiver<i32>) -> ExitCode {
    async move { rx.await.unwrap_or(NO_CODE) }.boxed().shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_end_and_drops_the_beginning() {
        let tail = Tail::new(3);
        assert!(tail.is_empty());
        for i in 1..=5 {
            tail.push(format!("line {i}"));
        }
        assert!(!tail.is_empty());
        assert_eq!(tail.lines(), vec!["line 3", "line 4", "line 5"]);
    }

    /// Every clone is the same ring: the reader task fills one and the teardown
    /// path reads another.
    #[test]
    fn a_clone_shares_the_ring_rather_than_copying_it() {
        let tail = Tail::new(4);
        let filled_by_the_reader = tail.clone();
        filled_by_the_reader.push("Invalid API key".into());
        assert_eq!(tail.lines(), vec!["Invalid API key"]);
    }

    /// Which half leads is the point of the parameter: an agent's banner has to
    /// open with what the agent said, a run's with its status.
    #[test]
    fn the_layout_decides_which_half_of_the_detail_leads() {
        let tail = Tail::new(20);
        tail.push("starting".into());
        tail.push("Invalid API key".into());

        let agent = exit_detail(1, &tail, Layout::TailFirst);
        assert_eq!(agent, "starting\nInvalid API key (exit code 1)");

        let run = exit_detail(1, &tail, Layout::CodeFirst);
        assert_eq!(run, "exit code 1\nstarting\nInvalid API key");
    }

    /// A process that said nothing has no tail to put on either side, so both
    /// layouts answer with the code alone rather than with stray punctuation.
    #[test]
    fn a_process_that_said_nothing_gets_the_code_alone_in_either_layout() {
        let silent = Tail::new(20);
        for layout in [Layout::TailFirst, Layout::CodeFirst] {
            assert_eq!(exit_detail(7, &silent, layout), "exit code 7", "{layout:?}");
        }
    }

    /// Two waiters, one channel: the shape both a run and an agent need, since
    /// the teardown path and the watcher are each sitting on the same exit.
    #[tokio::test]
    async fn every_waiter_gets_the_same_code() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let exit = exit_code(rx);
        tx.send(3).unwrap();
        assert_eq!(exit.clone().await, 3);
        assert_eq!(exit.clone().await, 3, "and again, for the second waiter");
    }

    /// A backend that dropped the sender lost the child; that is not an exit
    /// status of zero, and no real status is negative, so the two can never be
    /// confused.
    #[tokio::test]
    async fn a_lost_child_reports_a_code_no_process_could_have_returned() {
        let (tx, rx) = tokio::sync::oneshot::channel::<i32>();
        drop(tx);
        let lost = exit_code(rx).await;
        assert_eq!(lost, NO_CODE);
        assert!(lost < 0, "a real exit status is never negative");
    }
}
