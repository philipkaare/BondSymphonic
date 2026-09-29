//! Terminal sessions: a process with a pseudo-terminal attached, run either
//! inside a workspace sandbox or on the host.
//!
//! [`PtyManager::open`] starts one inside a workspace's sandbox;
//! [`PtyManager::open_host`] starts one of the fixed setup commands outside
//! every sandbox (see [`crate::setup`]). From there the two are the same kind
//! of session: both are registered by `adopt`, both stream through one pump
//! task that turns terminal output into `pty.output` events, and
//! `write`/`resize`/`close` reach either by id. The only visible difference is
//! the event envelope, which carries a workspace id for the first kind and none
//! for the second.
//!
//! Every session ends with exactly one `pty.exit` event, published by the pump
//! task, which then forgets the session; a request naming a session that has
//! ended is a `NotFound`.

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::sandbox::{PtySize, SandboxChild, SandboxCommand};
use crate::server::broadcast::EventBus;
use base64::Engine;
use bondsymphonic_proto::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// How long the pump task waits for the child's exit code once the terminal
/// has reached end of file. A child that outlives its terminal that long is
/// reported as `-1` rather than leaking the task and the session entry.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the pump keeps draining the terminal after the child has exited,
/// with the window restarting on every chunk that still arrives.
const DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

/// The whole draining phase is capped at this, measured from the child's exit.
/// [`DRAIN`] alone is a per-read timeout, so a grandchild that inherited the
/// terminal and chatters faster than that would restart it forever and the
/// session would never publish its exit.
const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `close` waits for a terminal to end on its own before escalating.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// How long each rung of the signal ladder gets before the next one: the
/// backend's killer, then SIGTERM, then SIGKILL. See [`escalate`].
const SIGNAL_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// SIGTERM and SIGKILL as plain numbers. `libc`, and signals at all, exist only
/// on Unix, while [`crate::sandbox::Signaller`] is cross-platform and the
/// Windows implementation recognises exactly these two, so spelling them out
/// keeps this one code path instead of two.
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// Ctrl-C: clears any half-typed line at an interactive prompt.
const INTERRUPT: &[u8] = b"\x03";

/// Ctrl-D: end of input, which makes an interactive shell at an empty prompt
/// log out. See [`PtyManager::close`].
const END_OF_INPUT: &[u8] = b"\x04";

/// Gap between those two bytes. The terminal discards everything still queued
/// for the shell when it sees Ctrl-C, so an end-of-input byte written in the
/// same breath is thrown away with the rest; it has to arrive after the shell
/// has taken the interrupt and redrawn its prompt.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// Terminal output is published in chunks of at most this many bytes.
const CHUNK: usize = 4096;

/// How long the pump waits for an [`OutputTap`] to finish before it announces
/// the exit anyway. The one tap's unfinished work is a single small file write,
/// so this is only ever reached on a disk that has stopped answering, and a
/// terminal whose `pty.exit` never comes would be worse than a late token.
const TAP_FINISH_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Something that sees a terminal's output as the pump reads it, before it is
/// published. The one user is the `claude setup-token` terminal, whose output
/// carries the token the daemon stores (see [`crate::agents::token_scan`]).
pub trait OutputTap: Send {
    /// Called on the pump task once per read, in order, so it must not block.
    fn feed(&mut self, bytes: &[u8]);

    /// Called once when the terminal ends, and awaited -- within
    /// [`TAP_FINISH_GRACE`] -- before `pty.exit` is published.
    ///
    /// That ordering is the point. `claude setup-token` exits the moment it has
    /// printed the token, and a client re-checks its prerequisites as soon as
    /// it sees the exit; if the token were still being written then, the check
    /// would find no token and report the login as if nothing had happened.
    fn finish(self: Box<Self>) -> futures::future::BoxFuture<'static, ()>;
}

/// One live terminal. The reader half lives in the pump task instead, so
/// nothing here is held across a read.
struct Session {
    /// The workspace this terminal belongs to, or `None` for a host setup
    /// terminal, which belongs to no workspace. It is what the event envelope
    /// carries, and what `close_workspace` matches on.
    workspace_id: Option<WorkspaceId>,
    /// Name of the backend that started this terminal. `close` needs it because
    /// how far a killer goes differs between backends.
    backend: &'static str,
    /// Serialises concurrent `pty.write` calls on one terminal.
    writer: Mutex<crate::sandbox::ChildWriter>,
    resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync>,
    killer: Box<dyn Fn() + Send + Sync>,
    /// Delivers one signal to the terminal's process group. `killer` remains the
    /// ordinary way to end a session; this is for the callers that need a
    /// specific signal — an interrupt, or the escalation in [`escalate`] that a
    /// backend's own killer cannot express.
    signaller: crate::sandbox::Signaller,
}

/// The live sessions. An `Arc` so the per-PTY pump task can remove its own
/// entry on exit, and so the detached task `close` spawns can outlive the call.
type Sessions = Arc<Mutex<HashMap<PtyId, Arc<Session>>>>;

pub struct PtyManager {
    events: EventBus,
    sessions: Sessions,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Ends a session that survived its backend's killer: SIGTERM to the terminal's
/// process group, then SIGKILL to whatever is still there.
///
/// This exists because the no-sandbox backend cannot do it itself. Its killer
/// ends a PTY child through `portable_pty`, whose cloned killer is a
/// `ProcessSignaller` that sends a bare SIGHUP and never escalates. A command
/// that traps or ignores SIGHUP therefore survives both the killer and the
/// retry, never reaches the pump with an exit code, never publishes `pty.exit`,
/// and leaves its session registered forever — and a client waiting on
/// `pty.exit` before re-checking its prerequisites would wait forever with it.
///
/// That matters most on the host path, which is the no-sandbox backend on every
/// machine: `install_claude` runs a non-interactive `bash -lc "curl … | bash"`,
/// which neither dies on SIGHUP nor passes it to the installer it started.
///
/// Each rung is skipped if the session has already retired, so a process that
/// goes down politely is never signalled twice.
async fn escalate(sessions: &Sessions, id: &PtyId, s: &Session) {
    (s.signaller)(SIGTERM);
    tokio::time::sleep(SIGNAL_GRACE).await;
    if sessions.lock().await.contains_key(id) {
        (s.signaller)(SIGKILL);
    }
}

impl PtyManager {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The shell a terminal opens when the caller names no command. The noop
    /// backend on Windows has no `bash`, so it gets `cmd`; every sandboxed
    /// backend is Linux and has a login shell.
    fn default_command(backend: &str) -> Vec<String> {
        if backend == "noop" && cfg!(windows) {
            vec!["cmd".into()]
        } else {
            vec!["bash".into(), "-l".into()]
        }
    }

    pub async fn open(&self, d: &Daemon, p: PtyOpenParams) -> Result<PtyOpenResult, RpcError> {
        let ws = d.workspace(&p.workspace_id)?;
        let handle = d.sandbox(&ws.id)?;
        let argv = match p
            .command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(cmd) => {
                crate::util::argv::split(cmd, "command").map_err(RpcError::invalid_params)?
            }
            None => Self::default_command(d.backend.name()),
        };
        let size = PtySize {
            cols: p.cols.max(2),
            rows: p.rows.max(1),
        };
        let child = handle
            .spawn(SandboxCommand {
                argv,
                env: vec![],
                cwd: None,
                pty: Some(size),
            })
            .await?;
        self.adopt(child, Some(ws.id), d.backend.name(), None).await
    }

    /// Opens a terminal on the host, outside every sandbox, running one of the
    /// fixed setup commands from [`crate::setup`].
    ///
    /// The caller passes an argv rather than a command string on purpose: the
    /// only argv that ever reaches here comes from [`crate::setup::setup_argv`],
    /// so no client can name the program that runs unsandboxed.
    ///
    /// The command inherits the daemon's whole environment, with `HOME` and
    /// `PATH` overridden — `HOME` by the handle, to the home the handle was
    /// built with, and `PATH` here, to lead with that home's `.local/bin`, which
    /// is where `claude` installs itself and need not be on a daemon's inherited
    /// path. Inheriting the rest is deliberate: a setup terminal should behave
    /// like the user's own shell, and it runs as that same user.
    ///
    /// `tap`, when given, sees the terminal's output as it is read; see
    /// [`OutputTap`].
    pub async fn open_host(
        &self,
        d: &Daemon,
        argv: Vec<String>,
        size: PtySize,
        tap: Option<Box<dyn OutputTap>>,
    ) -> Result<PtyOpenResult, RpcError> {
        self.open_host_with_env(d,argv,size,tap,Vec::new()).await
    }

    pub async fn open_host_with_env(
        &self,
        d: &Daemon,
        argv: Vec<String>,
        size: PtySize,
        tap: Option<Box<dyn OutputTap>>,
        mut env: Vec<(String,String)>,
    ) -> Result<PtyOpenResult, RpcError> {
        let host = d.host().await?;
        env.push(crate::setup::path_with_local_bin(&host.home));
        let child = host
            .handle
            .spawn(SandboxCommand {
                argv,
                env,
                cwd: None,
                pty: Some(size),
            })
            .await?;
        self.adopt(child, None, crate::setup::HOST_BACKEND, tap)
            .await
    }

    /// Takes ownership of a freshly spawned child: registers it as a session
    /// and starts the pump task that publishes its output and its one exit.
    ///
    /// `backend` is the name of the backend that started `child`, which is not
    /// always the daemon's own: a host terminal runs on the no-sandbox backend
    /// whatever the daemon uses for workspaces. [`PtyManager::close`] needs it,
    /// because how far a killer goes differs between backends.
    async fn adopt(
        &self,
        mut child: SandboxChild,
        workspace_id: Option<WorkspaceId>,
        backend: &'static str,
        mut tap: Option<Box<dyn OutputTap>>,
    ) -> Result<PtyOpenResult, RpcError> {
        let Some(pty) = child.pty.take() else {
            // The child is running but there is no terminal to reach it by, so
            // it would keep running with nobody able to stop it. Kill it here
            // rather than leaking a process into the sandbox.
            (child.killer)();
            return Err(RpcError::internal("backend returned no pty"));
        };
        let id: PtyId = new_id(PtyId::PREFIX).as_str().into();
        let session = Arc::new(Session {
            workspace_id: workspace_id.clone(),
            backend,
            writer: Mutex::new(pty.writer),
            resizer: pty.resizer,
            killer: child.killer,
            signaller: child.signal,
        });
        self.sessions.lock().await.insert(id.clone(), session);

        // Output pump + exit watcher.
        let events = self.events.clone();
        let pid = id.clone();
        let mut reader = pty.reader;
        let exit = child.exit;
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; CHUNK];
            let mut exit = exit;
            // The exit code, once known, with the instant draining must stop by.
            let mut ended: Option<(i32, tokio::time::Instant)> = None;
            loop {
                let read = match ended {
                    // Still running: whichever comes first, output or the exit.
                    None => tokio::select! {
                        r = reader.read(&mut buf) => r,
                        c = &mut exit => {
                            let until = tokio::time::Instant::now() + DRAIN_BUDGET;
                            ended = Some((c.unwrap_or(-1), until));
                            continue;
                        }
                    },
                    // The child is gone. Anything it left in the terminal is
                    // still worth publishing, but the wait is time-boxed twice
                    // over: a Windows ConPTY master stays readable while its
                    // handle lives, so end of file may never arrive, and a
                    // grandchild still holding the terminal can keep every
                    // single read inside `DRAIN` indefinitely.
                    Some((_, until)) => {
                        let quiet = tokio::time::Instant::now() + DRAIN;
                        match tokio::time::timeout_at(quiet.min(until), reader.read(&mut buf)).await
                        {
                            Ok(r) => r,
                            Err(_) => break,
                        }
                    }
                };
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(tap) = tap.as_mut() {
                            tap.feed(&buf[..n]);
                        }
                        events.publish(
                            workspace_id.clone(),
                            Event::PtyOutput {
                                pty_id: pid.clone(),
                                data_b64: b64(&buf[..n]),
                            },
                        )
                    }
                }
            }
            // Before the exit is announced, so whatever the tap still has to do
            // (the setup-token capture's write, or its log of a missed token)
            // lands ahead of it. See `OutputTap::finish`.
            if let Some(tap) = tap {
                if tokio::time::timeout(TAP_FINISH_GRACE, tap.finish())
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        pty = %pid,
                        "terminal output tap did not finish in time; announcing the exit anyway"
                    );
                }
            }
            let code = match ended {
                Some((code, _)) => code,
                // The terminal ended first; give the child a moment to be reaped.
                None => tokio::time::timeout(EXIT_GRACE, exit)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(-1),
            };
            // Retire the session before announcing the exit, so a client that
            // reacts to `pty.exit` never finds the id still writable.
            sessions.lock().await.remove(&pid);
            events.publish(workspace_id, Event::PtyExit { pty_id: pid, code });
        });
        Ok(PtyOpenResult { pty_id: id })
    }

    async fn session(&self, id: &PtyId) -> Result<Arc<Session>, RpcError> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::not_found(format!("pty {id}")))
    }

    pub async fn write(&self, p: PtyWriteParams) -> Result<Empty, RpcError> {
        let s = self.session(&p.pty_id).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(p.data_b64.as_bytes())
            .map_err(|e| RpcError::invalid_params(format!("data_b64: {e}")))?;
        s.writer
            .lock()
            .await
            .write_all(&bytes)
            .await
            .map_err(|e| RpcError::io(&e))?;
        Ok(Empty {})
    }

    pub async fn resize(&self, p: PtyResizeParams) -> Result<Empty, RpcError> {
        let s = self.session(&p.pty_id).await?;
        (s.resizer)(PtySize {
            cols: p.cols.max(2),
            rows: p.rows.max(1),
        })
        .map_err(|e| RpcError::io(&e))?;
        Ok(Empty {})
    }

    /// Terminates the session. The `pty.exit` event and the removal of the
    /// session follow from the pump task once the child is actually gone.
    ///
    /// The killer on its own is not always enough, and how it falls short
    /// differs by backend, so a session still registered after [`CLOSE_GRACE`]
    /// is asked a second time in the way that backend needs.
    ///
    /// On bubblewrap the killer sends SIGTERM to the sandboxed process group,
    /// and an interactive shell — the default command — ignores SIGTERM. So it
    /// is typed at instead: Ctrl-C then Ctrl-D, which an interactive shell reads
    /// as end of input, and then killed again for whatever the shell left
    /// behind. A genuine hard kill belongs one layer down there: the sandbox
    /// init protocol's `Kill` request already carries a signal number, so the
    /// exec client could offer a SIGKILL path instead of hardcoding SIGTERM.
    /// That change spans the sandbox backends and is ledgered for Milestone 2b.
    ///
    /// Everywhere else the backend is the no-sandbox one, whose killer is a bare
    /// SIGHUP; sending it a second time would change nothing, so the signal
    /// ladder in [`escalate`] runs instead.
    pub async fn close(&self, id: &PtyId) -> Result<Empty, RpcError> {
        close_session(&self.sessions, id).await
    }

    /// Closes `id` once `token` is cancelled, if the session is still there by
    /// then. Used to tie a host terminal to the connection that opened it: an
    /// unsandboxed login left running with nobody watching is precisely what
    /// the host path must never leave behind.
    pub fn close_on(&self, id: PtyId, token: tokio_util::sync::CancellationToken) {
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            // `NotFound` here means the session ended on its own first.
            let _ = close_session(&sessions, &id).await;
        });
    }

    /// Ends every host setup terminal and waits, within a bounded budget, for
    /// them to actually go.
    ///
    /// Daemon shutdown calls this before the host handle's own `shutdown`,
    /// because that handle's killers are the same un-escalating SIGHUP: without
    /// this, "a login left open does not outlive the daemon" would hold only for
    /// commands that happen to honour SIGHUP, and an installer mid-write would
    /// be left running with nothing left to stop it.
    ///
    /// The terminals are escalated concurrently, so the wait is one ladder long
    /// however many are open.
    pub async fn close_host(&self) {
        let victims: Vec<(PtyId, Arc<Session>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, s)| s.workspace_id.is_none())
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        if victims.is_empty() {
            return;
        }
        for (_, s) in &victims {
            (s.killer)();
        }
        // The polite signal first: an interactive login sitting at a prompt is
        // gone by the time this elapses and is never signalled again.
        tokio::time::sleep(SIGNAL_GRACE).await;
        futures::future::join_all(victims.iter().map(|(id, s)| async move {
            if self.sessions.lock().await.contains_key(id) {
                escalate(&self.sessions, id, s).await;
            }
        }))
        .await;
    }

    /// Closes every PTY belonging to `ws`, used when the workspace goes away.
    ///
    /// The killer alone is not enough here any more than it is in [`close`]:
    /// on the no-sandbox backend it is a bare SIGHUP, which a command can
    /// ignore and go on running after the worktree under it has been deleted.
    /// The workspace is on its way out, so there is nothing to be polite to:
    /// after one [`SIGNAL_GRACE`] whatever is still registered gets the signal
    /// ladder, on every backend.
    ///
    /// **This does not wait for the processes to go.** Each ladder runs in a
    /// task of its own and this returns as soon as every terminal has been
    /// asked, so a `workspace.destroy` that calls it goes straight on to remove
    /// the worktree with the last of the terminals possibly still dying. That
    /// is deliberate, and it is the difference between a destroy that answers
    /// at once and one that spends a second per open terminal doing nothing:
    /// the ladder itself is what guarantees they go, and nothing the destroy
    /// does afterwards needs them gone first. A worktree removal is not blocked
    /// by a process whose working directory is inside it, and [`close_host`],
    /// which *does* await its grace, has the opposite reason -- it runs on the
    /// way out of the daemon, where a task nobody waits for is a task that
    /// never runs.
    ///
    /// A caller that has to know a workspace's terminals are really gone has to
    /// watch for their `pty.exit` events; this answering is not that promise.
    ///
    /// [`close`]: PtyManager::close
    /// [`close_host`]: PtyManager::close_host
    pub async fn close_workspace(&self, ws: &WorkspaceId) {
        let victims: Vec<(PtyId, Arc<Session>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, s)| s.workspace_id.as_ref() == Some(ws))
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        for (id, s) in victims {
            (s.killer)();
            let sessions = self.sessions.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SIGNAL_GRACE).await;
                if sessions.lock().await.contains_key(&id) {
                    escalate(&sessions, &id, &s).await;
                }
            });
        }
    }
}

/// [`PtyManager::close`] over the shared session map, so it can run from a
/// task that holds the map rather than the manager.
async fn close_session(sessions: &Sessions, id: &PtyId) -> Result<Empty, RpcError> {
    let s = sessions
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| RpcError::not_found(format!("pty {id}")))?;
    (s.killer)();

    let sessions = sessions.clone();
    let id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CLOSE_GRACE).await;
        // The pump retires the session as soon as it has an exit code, so a
        // session still present here did not take the hint.
        let still_live = sessions.lock().await.get(&id).cloned();
        let Some(s) = still_live else {
            return;
        };
        if s.backend == "linux_bwrap" {
            // The lock is released between the two writes: holding it across
            // the settle would stall any concurrent `pty.write`.
            let _ = s.writer.lock().await.write_all(INTERRUPT).await;
            tokio::time::sleep(SETTLE).await;
            let _ = s.writer.lock().await.write_all(END_OF_INPUT).await;
            (s.killer)();
        } else {
            escalate(&sessions, &id, &s).await;
        }
    });
    Ok(Empty {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A child that prints `output` and exits 0 at once, the way
    /// `claude setup-token` does after printing its token.
    fn child_that_prints(output: &'static [u8]) -> SandboxChild {
        let (tx, exit) = tokio::sync::oneshot::channel();
        tx.send(0).unwrap();
        SandboxChild {
            pid: 1,
            stdin: None,
            stdout: None,
            stderr: None,
            pty: Some(crate::sandbox::PtyIo {
                reader: Box::pin(output),
                writer: Box::pin(tokio::io::sink()),
                resizer: Box::new(|_| Ok(())),
            }),
            exit,
            killer: Box::new(|| {}),
            signal: Box::new(|_| {}),
        }
    }

    /// A tap whose `finish` takes a while, and says when it is done.
    struct SlowTap(Arc<AtomicBool>);

    impl OutputTap for SlowTap {
        fn feed(&mut self, _: &[u8]) {}
        fn finish(self: Box<Self>) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                self.0.store(true, Ordering::SeqCst);
            })
        }
    }

    #[tokio::test]
    async fn the_exit_is_announced_only_after_the_tap_has_finished() {
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let ptys = PtyManager::new(events);
        let finished = Arc::new(AtomicBool::new(false));
        ptys.adopt(
            child_that_prints(b"bye"),
            None,
            "noop",
            Some(Box::new(SlowTap(finished.clone()))),
        )
        .await
        .unwrap();
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("pty.exit in time")
                .unwrap();
            if let ServerMessage::Event {
                event: Event::PtyExit { .. },
                ..
            } = msg
            {
                break;
            }
        }
        assert!(
            finished.load(Ordering::SeqCst),
            "pty.exit was published before the tap finished"
        );
    }

    #[test]
    fn the_default_command_is_a_login_shell_except_for_noop_on_windows() {
        assert_eq!(PtyManager::default_command("linux_bwrap"), ["bash", "-l"]);
        if cfg!(windows) {
            assert_eq!(PtyManager::default_command("noop"), ["cmd"]);
        } else {
            assert_eq!(PtyManager::default_command("noop"), ["bash", "-l"]);
        }
    }
}
