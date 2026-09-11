//! The daemon's side of the exec socket: sends [`InitRequest`]s to the
//! in-sandbox init and receives replies plus the file descriptors they carry.
//!
//! A blocking reader thread owns the socket's read end (`recvmsg` is the only
//! way to collect SCM_RIGHTS), and hands results to async callers through tokio
//! oneshot channels.

use super::protocol::{encode, InitReply, InitRequest};
use super::*;
use nix::cmsg_space;
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use std::collections::{HashMap, HashSet};
use std::io::{IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// `(pid, has_pty, fds)` for a successful spawn, or init's error message.
type SpawnReply = Result<(u32, bool, Vec<OwnedFd>), String>;

/// How long an exit nobody has claimed is kept. A spawn claims its exit as
/// soon as init's `Spawned` reply reaches it, which is the same socket the
/// exit arrives on, so a legitimate claim comes within milliseconds; anything
/// older belongs to a caller that is not coming back.
const EARLY_EXIT_TTL: Duration = Duration::from_secs(60);

/// Waiters and already-delivered exit codes, under one lock.
///
/// They have to move together: a caller registering a waiter and the reader
/// thread recording an exit race for the same pid, and with two locks an exit
/// can be filed as "early" moments after a waiter appeared, leaving that waiter
/// to hang forever.
struct ExitTable {
    waiters: HashMap<u32, oneshot::Sender<i32>>,
    /// Exit codes that arrived before the caller had a channel to receive
    /// them, with when they arrived. Pruned on every insert, and consumed by
    /// the spawn that claims them, so the table cannot grow with the
    /// sandbox's lifetime.
    early: HashMap<u32, (i32, Instant)>,
    /// Pids whose spawn nobody was left to receive: the process was killed on
    /// arrival, and its exit is dropped rather than filed as early.
    abandoned: HashSet<u32>,
    ttl: Duration,
}

impl ExitTable {
    fn new(ttl: Duration) -> Self {
        Self {
            waiters: HashMap::new(),
            early: HashMap::new(),
            abandoned: HashSet::new(),
            ttl,
        }
    }

    /// Files `pid`'s exit: returns its waiter when there is one, otherwise
    /// keeps the code for the spawn still on its way back, unless the spawn
    /// was abandoned.
    fn exited(&mut self, pid: u32, code: i32) -> Option<oneshot::Sender<i32>> {
        if let Some(tx) = self.waiters.remove(&pid) {
            return Some(tx);
        }
        if self.abandoned.remove(&pid) {
            return None;
        }
        let now = Instant::now();
        self.early
            .retain(|_, (_, at)| now.duration_since(*at) < self.ttl);
        self.early.insert(pid, (code, now));
        None
    }

    /// Claims an exit that already arrived for `pid`, handing back the sender
    /// to deliver it on, or registers `tx` as the waiter.
    fn claim(&mut self, pid: u32, tx: oneshot::Sender<i32>) -> Option<(oneshot::Sender<i32>, i32)> {
        match self.early.remove(&pid) {
            Some((code, _)) => Some((tx, code)),
            None => {
                self.waiters.insert(pid, tx);
                None
            }
        }
    }

    /// Notes that nobody will ever wait for `pid`.
    fn abandon(&mut self, pid: u32) {
        self.abandoned.insert(pid);
    }
}

pub struct ExecClient {
    writer: Mutex<UnixStream>,
    pending: Mutex<HashMap<u64, oneshot::Sender<SpawnReply>>>,
    exits: Mutex<ExitTable>,
    next_id: AtomicU64,
    /// Flipped to `true` when the exec socket reaches EOF, which is the daemon's
    /// only notice that init — and so the sandbox — is gone.
    died: tokio::sync::watch::Sender<bool>,
}

impl ExecClient {
    pub fn connect(path: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let stream = UnixStream::connect(path)?;
        let reader = stream.try_clone()?;
        Ok(Self::from_stream(stream, reader))
    }

    /// A client whose requests go out on `writer` and whose reader thread
    /// reads `reader`, the two ends of one connected socket.
    fn from_stream(writer: UnixStream, reader: UnixStream) -> Arc<Self> {
        let client = Arc::new(Self {
            writer: Mutex::new(writer),
            pending: Default::default(),
            exits: Mutex::new(ExitTable::new(EARLY_EXIT_TTL)),
            next_id: AtomicU64::new(1),
            died: tokio::sync::watch::channel(false).0,
        });
        let c = client.clone();
        std::thread::spawn(move || c.read_loop(reader));
        client
    }

    fn read_loop(&self, stream: UnixStream) {
        let fd = stream.as_raw_fd();
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        // A stream socket delivers SCM_RIGHTS at the tail of a read, so fds are
        // held here until the `Spawned` reply they belong to is parsed.
        let mut fds: Vec<OwnedFd> = Vec::new();
        loop {
            let n;
            {
                let mut cmsg = cmsg_space!([RawFd; 3]);
                let mut iov = [IoSliceMut::new(&mut buf)];
                // MSG_CMSG_CLOEXEC: received descriptors must not leak into the
                // next workspace's bwrap, or one sandbox would hold another's
                // pipes and PTY open.
                let msg = match recvmsg::<()>(
                    fd,
                    &mut iov,
                    Some(&mut cmsg),
                    MsgFlags::MSG_CMSG_CLOEXEC,
                ) {
                    Ok(m) => m,
                    Err(_) => break,
                };
                n = msg.bytes;
                if let Ok(cmsgs) = msg.cmsgs() {
                    for c in cmsgs {
                        if let ControlMessageOwned::ScmRights(list) = c {
                            // SAFETY: the kernel just installed these fds in
                            // this process and nothing else owns them.
                            fds.extend(
                                list.into_iter().map(|f| unsafe { OwnedFd::from_raw_fd(f) }),
                            );
                        }
                    }
                }
            }
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            while let Some(nl) = acc.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = acc.drain(..=nl).collect();
                if let Ok(reply) = serde_json::from_slice::<InitReply>(&line[..line.len() - 1]) {
                    self.dispatch(reply, &mut fds);
                }
            }
        }
        // Socket closed: fail everything pending.
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, tx) in pending {
            let _ = tx.send(Err("sandbox init went away".into()));
        }
        let waiters = std::mem::take(&mut self.exits.lock().unwrap().waiters);
        for (_, tx) in waiters {
            let _ = tx.send(-1);
        }
        // Last, so anything watching for the sandbox's death sees it only once
        // the outstanding work has already been failed.
        let _ = self.died.send(true);
    }

    /// Watches for the sandbox going away. The value is `false` while it is
    /// alive and `true` once the exec socket has closed.
    pub fn died(&self) -> tokio::sync::watch::Receiver<bool> {
        self.died.subscribe()
    }

    fn dispatch(&self, reply: InitReply, fds: &mut Vec<OwnedFd>) {
        match reply {
            InitReply::Spawned { id, pid, has_pty } => {
                let fds = std::mem::take(fds);
                let delivered = match self.pending.lock().unwrap().remove(&id) {
                    Some(tx) => tx.send(Ok((pid, has_pty, fds))).is_ok(),
                    None => false,
                };
                // The caller was cancelled between sending the request and
                // this reply: nobody will read the process's output or wait
                // for its exit, so it is killed outright and its exit is not
                // kept for a claim that will never come. Its descriptors were
                // dropped with the undelivered reply.
                if !delivered {
                    self.exits.lock().unwrap().abandon(pid);
                    let _ = self.send(&InitRequest::Kill {
                        pid,
                        signal: libc::SIGKILL,
                    });
                }
            }
            InitReply::SpawnFailed { id, message } => {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(Err(message));
                }
            }
            InitReply::Exited { pid, code } => {
                // Take the waiter, or record the exit, without ever releasing
                // the lock in between.
                let waiter = self.exits.lock().unwrap().exited(pid, code);
                if let Some(tx) = waiter {
                    let _ = tx.send(code);
                }
            }
            InitReply::ShuttingDown => {}
        }
    }

    fn send(&self, req: &InitRequest) -> Result<(), RpcError> {
        self.writer
            .lock()
            .unwrap()
            .write_all(&encode(req))
            .map_err(|e| sandbox_error(format!("exec socket: {e}")))
    }

    pub async fn spawn(
        self: &Arc<Self>,
        cmd: &SandboxCommand,
        base_env: &[(String, String)],
        default_cwd: &std::path::Path,
    ) -> Result<SandboxChild, RpcError> {
        if cmd.argv.is_empty() {
            return Err(bondsymphonic_proto::RpcError::invalid_params("empty argv"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut env = base_env.to_vec();
        env.extend(cmd.env.iter().cloned());
        let cwd = cmd.cwd.clone().unwrap_or_else(|| default_cwd.to_path_buf());
        self.send(&InitRequest::Spawn {
            id,
            argv: cmd.argv.clone(),
            env,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            pty: cmd.pty.map(|p| (p.cols, p.rows)),
        })?;
        let (pid, has_pty, mut fds) = rx
            .await
            .map_err(|_| sandbox_error("exec client closed"))?
            .map_err(sandbox_error)?;

        let (exit_tx, exit_rx) = oneshot::channel();
        // Claim an exit that already arrived, or register as its waiter, under
        // one lock: the reader thread may be handling this pid right now.
        let already_exited = self.exits.lock().unwrap().claim(pid, exit_tx);
        if let Some((tx, code)) = already_exited {
            let _ = tx.send(code);
        }

        let me = Arc::clone(self);
        let killer: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            let _ = me.send(&InitRequest::Kill {
                pid,
                signal: libc::SIGTERM,
            });
        });
        // init only signals pids it started, and sends to the process group
        // before the leader, so this reaches a shell's whole foreground job.
        let me = Arc::clone(self);
        let signal: Signaller = Box::new(move |n| {
            let _ = me.send(&InitRequest::Kill { pid, signal: n });
        });

        if has_pty {
            let master = fds
                .pop()
                .ok_or_else(|| sandbox_error("no pty fd received"))?;
            // Separate descriptors for reading and writing: one `tokio::fs::File`
            // used for both would serialize them behind its internal state, so a
            // blocked read would stall every write to the terminal.
            let read_fd = master
                .try_clone()
                .map_err(|e| sandbox_error(e.to_string()))?;
            let write_fd = master
                .try_clone()
                .map_err(|e| sandbox_error(e.to_string()))?;
            let reader: ChildReader = Box::pin(to_file(read_fd));
            let writer: ChildWriter = Box::pin(to_file(write_fd));
            // `master` lives inside the closure, keeping the fd valid for as
            // long as anyone can resize the terminal.
            let resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> =
                Box::new(move |s| {
                    let ws = libc::winsize {
                        ws_row: s.rows,
                        ws_col: s.cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    // SAFETY: `master` is an open PTY master for the call's duration.
                    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            return Ok(SandboxChild {
                pid,
                stdin: None,
                stdout: None,
                stderr: None,
                pty: Some(PtyIo {
                    reader,
                    writer,
                    resizer,
                }),
                exit: exit_rx,
                killer,
                signal,
            });
        }
        if fds.len() != 3 {
            return Err(sandbox_error(format!("expected 3 fds, got {}", fds.len())));
        }
        let stderr = fds.pop().expect("checked length");
        let stdout = fds.pop().expect("checked length");
        let stdin = fds.pop().expect("checked length");
        Ok(SandboxChild {
            pid,
            stdin: Some(Box::pin(to_file(stdin))),
            stdout: Some(Box::pin(to_file(stdout))),
            stderr: Some(Box::pin(to_file(stderr))),
            pty: None,
            exit: exit_rx,
            killer,
            signal,
        })
    }

    pub fn shutdown(&self) -> Result<(), RpcError> {
        self.send(&InitRequest::Shutdown)
    }
}

/// Wraps a pipe or PTY descriptor as an async file. Reads and writes land on
/// tokio's blocking pool, which is acceptable while each child has at most one
/// reader; Milestone 7 can move this to `AsyncFd`.
fn to_file(fd: OwnedFd) -> tokio::fs::File {
    tokio::fs::File::from_std(std::fs::File::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::path::Path;

    /// NT8 (c): an exit that arrived before its spawn returned is handed to
    /// that spawn once, and is gone from the table afterwards.
    #[test]
    fn an_early_exit_is_consumed_by_the_spawn_that_claims_it() {
        let mut table = ExitTable::new(EARLY_EXIT_TTL);
        assert!(table.exited(42, 7).is_none(), "nobody was waiting");
        let (tx, mut rx) = oneshot::channel();
        let claimed = table.claim(42, tx);
        let (tx, code) = claimed.expect("the early exit is claimed");
        assert_eq!(code, 7);
        tx.send(code).unwrap();
        assert_eq!(rx.try_recv(), Ok(7));
        assert!(table.early.is_empty(), "claimed entries are removed");
        // A second claim for the same pid finds nothing and registers a waiter.
        let (tx, _rx) = oneshot::channel();
        assert!(table.claim(42, tx).is_none());
        assert!(table.waiters.contains_key(&42));
    }

    /// NT8 (b): entries older than the TTL are dropped whenever a new one is
    /// filed, so a spawn whose caller went away cannot grow the table forever.
    #[test]
    fn early_exits_older_than_the_ttl_are_pruned() {
        let mut table = ExitTable::new(Duration::from_millis(10));
        assert!(table.exited(42, 0).is_none());
        std::thread::sleep(Duration::from_millis(30));
        assert!(table.exited(43, 0).is_none());
        assert!(!table.early.contains_key(&42), "42 outlived the TTL");
        assert!(table.early.contains_key(&43));
    }

    /// NT8 (a), the table half: the exit of a pid whose spawn nobody waited
    /// for is not filed at all.
    #[test]
    fn an_exit_for_an_abandoned_spawn_is_not_filed() {
        let mut table = ExitTable::new(EARLY_EXIT_TTL);
        table.abandon(42);
        assert!(table.exited(42, 0).is_none());
        assert!(table.early.is_empty());
        assert!(
            table.abandoned.is_empty(),
            "the note is used up by the exit"
        );
    }

    fn read_line(stream: &UnixStream) -> String {
        let mut line = String::new();
        std::io::BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        line
    }

    /// NT8 (a), over the wire: a `Spawned` reply whose caller has gone away
    /// gets its process killed, and the later `Exited` leaves no trace.
    #[tokio::test]
    async fn a_spawn_nobody_waits_for_is_killed_and_its_exit_dropped() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let client = ExecClient::from_stream(ours.try_clone().unwrap(), ours);
        let cmd = SandboxCommand {
            argv: vec!["sleep".into(), "9".into()],
            env: vec![],
            cwd: None,
            pty: None,
        };
        // The caller registers its request, then is cancelled before init
        // answers: an `agent.start` whose RPC connection dropped, say.
        let c = client.clone();
        let caller = tokio::spawn(async move { c.spawn(&cmd, &[], Path::new("/")).await });
        let request = tokio::task::spawn_blocking({
            let t = theirs.try_clone().unwrap();
            move || read_line(&t)
        })
        .await
        .unwrap();
        let InitRequest::Spawn { id, .. } = serde_json::from_str(&request).unwrap() else {
            panic!("expected a spawn request: {request}");
        };
        caller.abort();
        let join = caller.await.err().expect("the caller was cancelled");
        assert!(join.is_cancelled());

        (&theirs)
            .write_all(&encode(&InitReply::Spawned {
                id,
                pid: 42,
                has_pty: false,
            }))
            .unwrap();
        let kill = tokio::task::spawn_blocking({
            let t = theirs.try_clone().unwrap();
            move || read_line(&t)
        })
        .await
        .unwrap();
        assert!(
            matches!(
                serde_json::from_str::<InitRequest>(&kill).unwrap(),
                InitRequest::Kill {
                    pid: 42,
                    signal: libc::SIGKILL
                }
            ),
            "the orphaned process must be killed: {kill}"
        );

        (&theirs)
            .write_all(&encode(&InitReply::Exited { pid: 42, code: 137 }))
            .unwrap();
        // The reader thread files exits; give it a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !client.exits.lock().unwrap().abandoned.is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let table = client.exits.lock().unwrap();
        assert!(
            table.early.is_empty(),
            "the exit was filed: {:?}",
            table.early
        );
        assert!(table.abandoned.is_empty());
    }
}
