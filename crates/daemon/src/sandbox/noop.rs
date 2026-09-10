//! The no-sandbox backend: processes run as ordinary children of the daemon.
//!
//! It is the fallback on hosts without bubblewrap and the backend the test
//! suite uses on Windows, so it has to support both pipes and a real PTY.

use super::*;
use futures::StreamExt;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize as PPtySize};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::{Poll, Waker};

/// What [`NoopHandle::helper_exe`] answers with: a path chosen so that a spawn
/// which should never have happened fails with an explanation.
const NO_HELPERS: &str = "/nonexistent/the-no-sandbox-backend-has-no-in-sandbox-helpers";

/// A killer closure registered with a handle so `shutdown` can reach the child.
type Killer = Box<dyn Fn() + Send + Sync>;

/// Live children, keyed by a registration id rather than a pid. Entries are
/// removed the moment a child exits, so `shutdown` only ever signals processes
/// that are still running. Keying by pid would be unsafe on Unix, where pids are
/// reused: a stale entry could signal an unrelated process.
type Children = Mutex<HashMap<u64, Killer>>;

/// How long a pipe child's process group gets between SIGTERM and SIGKILL.
#[cfg(unix)]
const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The signal numbers that mean "end this process". Spelled out because `libc`,
/// and signals at all, exist only on Unix, and the Windows path still has to
/// recognise the two numbers it can act on.
#[cfg(not(unix))]
const TERMINATING_SIGNALS: [i32; 2] = [9 /* SIGKILL */, 15 /* SIGTERM */];

fn next_child_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct NoopBackend;

struct NoopHandle {
    spec: SandboxSpec,
    children: Arc<Children>,
}

#[async_trait]
impl SandboxBackend for NoopBackend {
    fn name(&self) -> &'static str {
        "noop"
    }

    async fn check(&self) -> Vec<PrereqStatus> {
        vec![PrereqStatus {
            name: "sandbox".into(),
            ok: true,
            detail: "noop backend: processes run unsandboxed".into(),
            fix_hint: None,
        }]
    }

    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        // Creating directories hits the filesystem, so it goes to a blocking
        // thread instead of stalling a runtime worker.
        let home = spec.home.clone();
        let run_dir = spec.run_dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&home)?;
            std::fs::create_dir_all(&run_dir)
        })
        .await
        .map_err(|e| sandbox_error(format!("create sandbox dirs: {e}")))?
        .map_err(|e| RpcError::io(&e))?;
        Ok(Arc::new(NoopHandle {
            spec: spec.clone(),
            children: Arc::new(Mutex::new(HashMap::new())),
        }))
    }
}

impl NoopHandle {
    fn env_for(&self, cmd: &SandboxCommand) -> Vec<(String, String)> {
        let mut env = vec![(
            "HOME".to_string(),
            self.spec.home.to_string_lossy().into_owned(),
        )];
        env.extend(self.spec.env.iter().cloned());
        env.extend(cmd.env.iter().cloned());
        env
    }

    fn cwd_for(&self, cmd: &SandboxCommand) -> PathBuf {
        let cwd = cmd.cwd.clone().unwrap_or_else(|| self.spec.cwd.clone());
        if cwd.exists() {
            cwd
        } else {
            std::env::temp_dir()
        }
    }
}

#[async_trait]
impl SandboxHandle for NoopHandle {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        if cmd.argv.is_empty() {
            return Err(RpcError::invalid_params("empty argv"));
        }
        let env = self.env_for(&cmd);
        let cwd = self.cwd_for(&cmd);
        match cmd.pty {
            None => spawn_piped(&cmd.argv, &env, &cwd, &self.children),
            Some(size) => spawn_pty(&cmd.argv, &env, &cwd, size, &self.children).await,
        }
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        let killers = std::mem::take(&mut *self.children.lock().unwrap());
        for (_, k) in killers {
            k();
        }
        Ok(())
    }

    /// A path that does not exist, on purpose.
    ///
    /// The helpers — the proxy shim, the port forwarder — exist only to make up
    /// for what a real sandbox takes away, and this backend takes nothing away:
    /// a child here is an ordinary child of the daemon, with the host's network
    /// and the host's ports. So nothing should ever ask, and something that does
    /// has a bug. `current_exe()` would answer it with the test harness under
    /// `cargo test` and with the daemon itself in production, either of which
    /// starts a process that quietly does the wrong thing; this fails at the
    /// spawn instead, with the reason in the path.
    fn helper_exe(&self) -> PathBuf {
        PathBuf::from(NO_HELPERS)
    }
}

// ---------------------------------------------------------------------------
// Pipes
// ---------------------------------------------------------------------------

fn spawn_piped(
    argv: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    children: &Arc<Children>,
) -> Result<SandboxChild, RpcError> {
    let mut c = tokio::process::Command::new(&argv[0]);
    c.args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    for (k, v) in env {
        c.env(k, v);
    }
    #[cfg(unix)]
    {
        // Put the child in its own process group so one kill(-pid) reaches it
        // and everything it spawns. See `make_pipe_killer`.
        use std::os::unix::process::CommandExt;
        c.as_std_mut().process_group(0);
    }
    let mut child = c
        .spawn()
        .map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    let pid = child.id().unwrap_or(0);
    let stdin: ChildWriter = Box::pin(child.stdin.take().expect("stdin piped"));
    let stdout: ChildReader = Box::pin(child.stdout.take().expect("stdout piped"));
    let stderr: ChildReader = Box::pin(child.stderr.take().expect("stderr piped"));

    let (tx, rx) = tokio::sync::oneshot::channel();
    let kill = Arc::new(tokio::sync::Notify::new());
    let exited = Arc::new(AtomicBool::new(false));
    let id = next_child_id();

    let watch_kill = kill.clone();
    let watch_exited = exited.clone();
    let watch_children = children.clone();
    tokio::spawn(async move {
        // Wait for whichever comes first: the child exiting on its own, or a
        // kill request. No polling, so an idle child costs nothing.
        let natural = tokio::select! {
            st = child.wait() => Some(st.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)),
            _ = watch_kill.notified() => None,
        };
        let code = match natural {
            Some(code) => code,
            None => terminate_pipe_child(&mut child, pid).await,
        };
        // Retire the registration before handing out the exit code, so anyone
        // who sees the child exit also sees it deregistered.
        watch_exited.store(true, Ordering::SeqCst);
        watch_children.lock().unwrap().remove(&id);
        let _ = tx.send(code);
    });

    let killer = make_pipe_killer(kill.clone(), exited.clone());
    #[cfg(unix)]
    let signal = make_group_signaller(pid, exited.clone());
    #[cfg(not(unix))]
    let signal = signaller_from_killer(make_pipe_killer(kill.clone(), exited.clone()));
    children
        .lock()
        .unwrap()
        .insert(id, make_pipe_killer(kill, exited));
    Ok(SandboxChild {
        pid,
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr: Some(stderr),
        pty: None,
        exit: rx,
        killer,
        signal,
    })
}

/// Builds a killer for a pipe child.
///
/// On Unix the child was given its own process group, and the request is served
/// by sending SIGTERM to that whole group, then SIGKILL to it after
/// [`TERM_GRACE`], so a shell and everything it started go down together.
///
/// On Windows only the direct child is terminated; grandchildren survive. That
/// is a known limit of this backend. In a real sandbox the bwrap backend's init
/// process is pid 1 of its own pid namespace and reaps the whole tree, so this
/// gap does not reach production Linux hosts.
///
/// The killer is a no-op once the child has exited: the pid may already have
/// been reused by an unrelated process.
fn make_pipe_killer(kill: Arc<tokio::sync::Notify>, exited: Arc<AtomicBool>) -> Killer {
    Box::new(move || {
        if exited.load(Ordering::SeqCst) {
            return;
        }
        kill.notify_one();
    })
}

/// Builds a signaller that delivers whatever signal it is given to the child's
/// process group — the same group [`make_pipe_killer`] terminates, and the same
/// group a PTY child leads.
///
/// Unlike the killer this does not escalate: the caller asked for one specific
/// signal, so one signal is what the child gets. Inert once the child has
/// exited, because the pid may already belong to somebody else.
#[cfg(unix)]
fn make_group_signaller(pid: u32, exited: Arc<AtomicBool>) -> Signaller {
    Box::new(move |n| {
        if exited.load(Ordering::SeqCst) {
            return;
        }
        signal_group(pid, n);
    })
}

/// Builds a signaller for a platform without signals, out of the killer for the
/// same child.
///
/// SIGTERM and SIGKILL both mean "end this process", which Windows can do. There
/// is nothing honest to map any other number onto, and terminating on all of
/// them would turn an interrupt into a kill, so the rest are dropped.
#[cfg(not(unix))]
fn signaller_from_killer(killer: Killer) -> Signaller {
    Box::new(move |n| {
        if TERMINATING_SIGNALS.contains(&n) {
            killer();
        } else {
            tracing::debug!(signal = n, "no signals on this platform; ignored");
        }
    })
}

/// Terminates a pipe child and returns its exit code.
async fn terminate_pipe_child(child: &mut tokio::process::Child, pid: u32) -> i32 {
    #[cfg(unix)]
    {
        signal_group(pid, libc::SIGTERM);
        if let Ok(st) = tokio::time::timeout(TERM_GRACE, child.wait()).await {
            return st.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        }
        signal_group(pid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = child.start_kill();
    }
    child
        .wait()
        .await
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1)
}

/// Sends `signal` to the process group led by `pid`.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    if pid == 0 {
        return;
    }
    // SAFETY: kill(2) with a negative pid signals that process group. A group
    // that is already gone just yields ESRCH, which is why the result is
    // ignored; no memory is touched.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

// ---------------------------------------------------------------------------
// PTY
// ---------------------------------------------------------------------------

/// The blocking pieces of a PTY spawn, handed back from the blocking thread.
type PtyParts = (
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn std::io::Read + Send>,
    Box<dyn std::io::Write + Send>,
    Box<dyn portable_pty::MasterPty + Send>,
);

/// `openpty` and `spawn_command` both block; this runs on a blocking thread.
fn open_pty_child(
    argv: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    size: PtySize,
) -> Result<PtyParts, RpcError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PPtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| sandbox_error(format!("openpty: {e}")))?;
    let mut cb = CommandBuilder::new(&argv[0]);
    cb.args(&argv[1..]);
    cb.cwd(cwd);
    for (k, v) in env {
        cb.env(k, v);
    }
    let child = pair
        .slave
        .spawn_command(cb)
        .map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| sandbox_error(e.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| sandbox_error(e.to_string()))?;
    Ok((child, reader, writer, pair.master))
}

async fn spawn_pty(
    argv: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    size: PtySize,
    children: &Arc<Children>,
) -> Result<SandboxChild, RpcError> {
    let owned_argv = argv.to_vec();
    let owned_env = env.to_vec();
    let owned_cwd = cwd.to_path_buf();
    let (mut child, mut master_reader, mut master_writer, master) =
        tokio::task::spawn_blocking(move || {
            open_pty_child(&owned_argv, &owned_env, &owned_cwd, size)
        })
        .await
        .map_err(|e| sandbox_error(format!("pty spawn task: {e}")))??;
    let pid = child.process_id().unwrap_or(0);
    let master = Arc::new(Mutex::new(master));

    // Reader pump: blocking thread -> mpsc<Vec<u8>> -> an AsyncRead adapter.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut sink = Some(out_tx);
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(tx) = &sink {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            // Nobody is reading any more, but the master stays
                            // open through the resizer, so the child would block
                            // on a full pty buffer if we stopped now. Drop the
                            // sender and keep draining until the master ends.
                            sink = None;
                        }
                    }
                }
            }
        }
    });
    let reader: ChildReader = Box::pin(tokio_util::io::StreamReader::new(
        tokio_stream::wrappers::ReceiverStream::new(out_rx)
            .map(|v| Ok::<_, std::io::Error>(bytes::Bytes::from(v))),
    ));

    // Writer pump: async mpsc<Vec<u8>> -> blocking write thread.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let waker = write_waker();
    let pump_waker = waker.clone();
    std::thread::spawn(move || {
        while let Some(chunk) = in_rx.blocking_recv() {
            // A slot just came free; release any writer parked on a full channel.
            wake_writer(&pump_waker);
            if master_writer.write_all(&chunk).is_err() || master_writer.flush().is_err() {
                break;
            }
        }
        // Either the writer was shut down or the pty broke. Dropping
        // `master_writer` here closes the write end and sends EOF to the child.
        wake_writer(&pump_waker);
    });
    let writer: ChildWriter = Box::pin(ChannelWriter::new(in_tx, waker));

    let m2 = master.clone();
    let resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> = Box::new(move |s| {
        m2.lock()
            .unwrap()
            .resize(PPtySize {
                rows: s.rows,
                cols: s.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let killer_child = Arc::new(Mutex::new(Some(child.clone_killer())));
    let exited = Arc::new(AtomicBool::new(false));
    let id = next_child_id();

    let watch_killer = killer_child.clone();
    let watch_exited = exited.clone();
    let watch_children = children.clone();
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        watch_exited.store(true, Ordering::SeqCst);
        watch_children.lock().unwrap().remove(&id);
        // Release the duplicated child handle; on Windows it is an OS HANDLE
        // that would otherwise be held until the workspace shuts down.
        *watch_killer.lock().unwrap() = None;
        let _ = tx.send(code);
    });

    let killer = make_pty_killer(killer_child.clone(), exited.clone());
    // `portable-pty` makes the child a session leader, so it leads its own
    // process group and a group signal reaches the terminal's foreground job.
    #[cfg(unix)]
    let signal = make_group_signaller(pid, exited.clone());
    #[cfg(not(unix))]
    let signal = signaller_from_killer(make_pty_killer(killer_child.clone(), exited.clone()));
    children
        .lock()
        .unwrap()
        .insert(id, make_pty_killer(killer_child, exited));
    Ok(SandboxChild {
        pid,
        stdin: None,
        stdout: None,
        stderr: None,
        pty: Some(PtyIo {
            reader,
            writer,
            resizer,
        }),
        exit: rx,
        killer,
        signal,
    })
}

/// Builds a killer for a PTY child.
///
/// `portable-pty` makes the child a session leader owning the pty, so signalling
/// it also reaches the foreground job of any shell running in that terminal. The
/// killer is a no-op once the child has exited, both because the pid may have
/// been reused and because the duplicated handle is released at that point.
fn make_pty_killer(
    killer_child: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    exited: Arc<AtomicBool>,
) -> Killer {
    Box::new(move || {
        if exited.load(Ordering::SeqCst) {
            return;
        }
        if let Some(k) = killer_child.lock().unwrap().as_mut() {
            let _ = k.kill();
        }
    })
}

// ---------------------------------------------------------------------------
// Async writer over a blocking pump thread
// ---------------------------------------------------------------------------

/// Slot where a [`ChannelWriter`] blocked on a full channel parks its waker.
pub(crate) type WriteWaker = Arc<Mutex<Option<Waker>>>;

/// Creates the waker slot shared by a [`ChannelWriter`] and its pump thread.
pub(crate) fn write_waker() -> WriteWaker {
    Arc::new(Mutex::new(None))
}

/// Releases a writer parked on a full channel. The pump thread must call this
/// after every receive, which is when a slot becomes free.
pub(crate) fn wake_writer(slot: &WriteWaker) {
    if let Some(waker) = slot.lock().unwrap().take() {
        waker.wake();
    }
}

/// `AsyncWrite` that forwards chunks to a blocking writer thread.
///
/// `poll_shutdown` drops the sender, which ends the pump thread and closes the
/// underlying writer. That is what lets a child blocked on reading its stdin see
/// EOF, so callers must shut the writer down rather than merely dropping it into
/// a long-lived structure.
pub(crate) struct ChannelWriter {
    tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    waker: WriteWaker,
}

impl ChannelWriter {
    /// `waker` must be the same slot the pump thread passes to [`wake_writer`].
    pub(crate) fn new(tx: tokio::sync::mpsc::Sender<Vec<u8>>, waker: WriteWaker) -> Self {
        Self {
            tx: Some(tx),
            waker,
        }
    }
}

impl AsyncWrite for ChannelWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        use tokio::sync::mpsc::error::TrySendError;
        let this = self.get_mut();
        let Some(tx) = this.tx.as_ref() else {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "writer shut down",
            )));
        };
        match tx.try_reserve() {
            Ok(permit) => {
                permit.send(buf.to_vec());
                Poll::Ready(Ok(buf.len()))
            }
            Err(TrySendError::Full(())) => {
                // Park the waker, then re-check: the pump may have drained a slot
                // between the failed reservation and the registration, in which
                // case no further wake is coming.
                *this.waker.lock().unwrap() = Some(cx.waker().clone());
                match tx.try_reserve() {
                    Ok(permit) => {
                        permit.send(buf.to_vec());
                        Poll::Ready(Ok(buf.len()))
                    }
                    Err(TrySendError::Full(())) => Poll::Pending,
                    Err(_) => Poll::Ready(Err(std::io::Error::other("pty closed"))),
                }
            }
            Err(_) => Poll::Ready(Err(std::io::Error::other("pty closed"))),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Dropping the sender ends the pump thread, which closes the writer.
        self.get_mut().tx = None;
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn handle(dir: &std::path::Path) -> NoopHandle {
        NoopHandle {
            spec: SandboxSpec {
                id: "ws_unit".into(),
                rw_binds: vec![],
                ro_binds: vec![],
                late_ro_binds: vec![],
                home: dir.join("home"),
                run_dir: dir.join("run"),
                env: vec![("FROM_SPEC".into(), "1".into())],
                cwd: dir.to_path_buf(),
            },
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn command() -> SandboxCommand {
        SandboxCommand {
            argv: vec!["true".into()],
            env: vec![],
            cwd: None,
            pty: None,
        }
    }

    fn quick_exit() -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/c".into(), "exit 0".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 0".into()]
        }
    }

    #[test]
    fn env_sets_home_then_spec_then_command() {
        let dir = tempfile::tempdir().unwrap();
        let h = handle(dir.path());
        let mut cmd = command();
        cmd.env = vec![("FROM_SPEC".into(), "overridden".into())];
        let env = h.env_for(&cmd);
        assert_eq!(env[0].0, "HOME");
        assert_eq!(env[0].1, dir.path().join("home").to_string_lossy());
        // Later entries win, so a per-command value overrides the spec's.
        assert_eq!(env[1], ("FROM_SPEC".into(), "1".into()));
        assert_eq!(env[2], ("FROM_SPEC".into(), "overridden".into()));
    }

    #[test]
    fn cwd_falls_back_when_the_directory_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let h = handle(dir.path());
        assert_eq!(h.cwd_for(&command()), dir.path());

        let mut cmd = command();
        cmd.cwd = Some(dir.path().join("nope"));
        assert_eq!(h.cwd_for(&cmd), std::env::temp_dir());
    }

    #[tokio::test]
    async fn empty_argv_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let h = handle(dir.path());
        let mut cmd = command();
        cmd.argv.clear();
        let Err(err) = h.spawn(cmd).await else {
            panic!("empty argv must be rejected");
        };
        assert_eq!(err.code, bondsymphonic_proto::ErrorCode::InvalidParams);
    }

    /// A child that exits on its own must leave the registry, so a later
    /// `shutdown` cannot signal a pid the OS has since handed to someone else.
    #[tokio::test]
    async fn an_exited_child_is_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let h = handle(dir.path());
        let mut cmd = command();
        cmd.argv = quick_exit();
        let child = h.spawn(cmd).await.unwrap();
        assert_eq!(h.children.lock().unwrap().len(), 1);

        assert_eq!(child.exit.await.unwrap(), 0);
        // The watcher deregisters before it publishes the exit code.
        assert!(h.children.lock().unwrap().is_empty());

        // And the killer handed to the caller is inert now.
        (child.killer)();
        assert!(h.children.lock().unwrap().is_empty());
        h.shutdown().await.unwrap();
    }

    /// Shutting the writer down has to close the pty's write end, otherwise a
    /// child reading its input to EOF never returns.
    #[tokio::test]
    async fn shutting_the_writer_down_reports_further_writes_as_broken() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let waker = write_waker();
        let mut w = ChannelWriter::new(tx, waker);
        w.write_all(b"hello").await.unwrap();
        w.shutdown().await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), b"hello".to_vec());
        // The sender is gone, so the pump thread's `blocking_recv` ends.
        assert!(rx.recv().await.is_none());
        let err = w.write_all(b"more").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A writer parked on a full channel must be woken by the drain, not by a
    /// busy spin.
    #[tokio::test]
    async fn a_full_channel_parks_the_writer_until_a_slot_frees() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let waker = write_waker();
        let pump_waker = waker.clone();
        let mut w = ChannelWriter::new(tx, waker);
        w.write_all(b"first").await.unwrap();

        // The channel is full; this write cannot complete until something drains.
        let writing = tokio::spawn(async move {
            w.write_all(b"second").await.unwrap();
            w
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!writing.is_finished(), "the writer must park while full");

        assert_eq!(rx.recv().await.unwrap(), b"first".to_vec());
        wake_writer(&pump_waker);
        let _w = tokio::time::timeout(std::time::Duration::from_secs(5), writing)
            .await
            .expect("the drain wakes the parked writer")
            .unwrap();
        assert_eq!(rx.recv().await.unwrap(), b"second".to_vec());
    }
}
