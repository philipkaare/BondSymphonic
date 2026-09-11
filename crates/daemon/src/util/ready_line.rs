//! The readiness handshake the daemon's two in-sandbox helpers speak.
//!
//! Both the proxy shim ([`crate::net::shim`]) and the port forwarder
//! ([`crate::net::forward`]) are started inside a workspace's sandbox, bind a
//! socket there, and print one line to say they have. The daemon waits for that
//! line because the alternative is a race it always loses on a slow host: the
//! first request out of the sandbox arriving before the shim's port exists, or
//! the first readiness probe of a run reaching a run directory with no socket
//! in it yet.
//!
//! The wait, the logging of every line that is not the announcement, and the
//! draining of both pipes afterwards are the same at both call sites. What is
//! *not* the same is what happens when the line never comes -- the workspace
//! opens anyway with no route out, while the run refuses to start at all -- so
//! that half stays where it belongs, at the call site.

use crate::sandbox::ChildReader;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tracing::{debug, warn};

type PipeLines = Lines<BufReader<ChildReader>>;

/// The stdout and stderr of one helper process, as lines.
pub struct Helper {
    /// Names the program in every log line, so two helpers in one workspace can
    /// be told apart: `proxy-shim`, `forward`.
    label: &'static str,
    /// The workspace the helper belongs to, for the log field of the same name.
    ws: String,
    out: PipeLines,
    err: PipeLines,
}

impl Helper {
    pub fn new(label: &'static str, ws: &str, stdout: ChildReader, stderr: ChildReader) -> Self {
        Self {
            label,
            ws: ws.to_owned(),
            out: BufReader::new(stdout).lines(),
            err: BufReader::new(stderr).lines(),
        }
    }

    /// Whether a line beginning with `pattern` arrived on stdout inside
    /// `budget`.
    ///
    /// Anything else the helper says on the way is logged and passed over, so a
    /// program that greets before it binds is not mistaken for one that never
    /// bound. `false` covers all three ways the announcement can fail to
    /// arrive -- the budget ran out, the pipe ended, the read failed -- because
    /// no caller can do anything different about them: what each one has to
    /// decide is what to do about a helper that is not listening, and that is
    /// the same decision in every case.
    ///
    /// Bounded, because the far side is a process in a sandbox: one that starts
    /// and never binds would otherwise hold a workspace open, or a `run.start`,
    /// for as long as it cared to live.
    pub async fn ready(&mut self, pattern: &str, budget: std::time::Duration) -> bool {
        let Self { label, ws, out, .. } = self;
        tokio::time::timeout(budget, async {
            while let Ok(Some(line)) = out.next_line().await {
                if line.starts_with(pattern) {
                    return true;
                }
                debug!(ws = %ws, "{label}: {line}");
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    /// Logs both pipes to the end of input, then answers.
    ///
    /// The two are read at once rather than one after the other: a helper that
    /// fills its stderr pipe while nothing is reading it blocks on the write,
    /// and a drain that was still working through stdout would never reach the
    /// stderr that explains why.
    ///
    /// stdout is debug and stderr is a warning, which is the same split the
    /// helpers themselves make: the one carries progress, the other carries the
    /// reason something did not work.
    pub async fn drain(self) {
        let Self {
            label,
            ws,
            mut out,
            mut err,
        } = self;
        tokio::join!(
            async {
                while let Ok(Some(line)) = out.next_line().await {
                    debug!(ws = %ws, "{label}: {line}");
                }
            },
            async {
                while let Ok(Some(line)) = err.next_line().await {
                    warn!(ws = %ws, "{label}: {line}");
                }
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipes(out: &str, err: &str) -> Helper {
        let stdout: ChildReader = Box::pin(std::io::Cursor::new(out.as_bytes().to_vec()));
        let stderr: ChildReader = Box::pin(std::io::Cursor::new(err.as_bytes().to_vec()));
        Helper::new("test-helper", "ws_1", stdout, stderr)
    }

    /// The announcement is recognised by its opening, because the helpers put
    /// the socket or port they bound on the end of the same line.
    #[tokio::test]
    async fn the_announcement_is_found_past_whatever_came_before_it() {
        let mut h = pipes("starting up\nbs-listening on 9000\n", "");
        assert!(
            h.ready("bs-listening", std::time::Duration::from_secs(5))
                .await
        );
    }

    /// A helper that says everything except the one line, and then ends, is a
    /// helper that is not listening -- not one worth waiting the whole budget
    /// for.
    #[tokio::test]
    async fn a_pipe_that_ends_without_the_line_answers_at_once() {
        let mut h = pipes("starting up\ngiving up\n", "");
        let began = std::time::Instant::now();
        assert!(
            !h.ready("bs-listening", std::time::Duration::from_secs(30))
                .await
        );
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A helper that holds its pipe open and says nothing has to cost the
    /// budget and no more.
    #[tokio::test(start_paused = true)]
    async fn a_silent_helper_costs_the_budget_and_no_more() {
        // A reader that never yields a line and never ends.
        let stdout: ChildReader = Box::pin(Pending);
        let stderr: ChildReader = Box::pin(tokio::io::empty());
        let mut h = Helper::new("test-helper", "ws_1", stdout, stderr);
        assert!(
            !h.ready("bs-listening", std::time::Duration::from_secs(5))
                .await
        );
    }

    /// Never readable, never finished: what a process holding its stdout open
    /// and saying nothing looks like from this side.
    struct Pending;

    impl tokio::io::AsyncRead for Pending {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }
}
