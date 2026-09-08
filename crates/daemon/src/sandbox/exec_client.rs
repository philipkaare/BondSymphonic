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
use std::collections::HashMap;
use std::io::{IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// `(pid, has_pty, fds)` for a successful spawn, or init's error message.
type SpawnReply = Result<(u32, bool, Vec<OwnedFd>), String>;

/// Waiters and already-delivered exit codes, under one lock.
///
/// They have to move together: a caller registering a waiter and the reader
/// thread recording an exit race for the same pid, and with two locks an exit
/// can be filed as "early" moments after a waiter appeared, leaving that waiter
/// to hang forever.
#[derive(Default)]
struct ExitTable {
    waiters: HashMap<u32, oneshot::Sender<i32>>,
    /// Exit codes that arrived before the caller had a channel to receive them.
    early: HashMap<u32, i32>,
}

pub struct ExecClient {
    writer: Mutex<UnixStream>,
    pending: Mutex<HashMap<u64, oneshot::Sender<SpawnReply>>>,
    exits: Mutex<ExitTable>,
    next_id: AtomicU64,
}

impl ExecClient {
    pub fn connect(path: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let stream = UnixStream::connect(path)?;
        let reader = stream.try_clone()?;
        let client = Arc::new(Self {
            writer: Mutex::new(stream),
            pending: Default::default(),
            exits: Default::default(),
            next_id: AtomicU64::new(1),
        });
        let c = client.clone();
        std::thread::spawn(move || c.read_loop(reader));
        Ok(client)
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
        let exits = std::mem::take(&mut *self.exits.lock().unwrap());
        for (_, tx) in exits.waiters {
            let _ = tx.send(-1);
        }
    }

    fn dispatch(&self, reply: InitReply, fds: &mut Vec<OwnedFd>) {
        match reply {
            InitReply::Spawned { id, pid, has_pty } => {
                let fds = std::mem::take(fds);
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(Ok((pid, has_pty, fds)));
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
                let waiter = {
                    let mut table = self.exits.lock().unwrap();
                    match table.waiters.remove(&pid) {
                        Some(tx) => Some(tx),
                        None => {
                            table.early.insert(pid, code);
                            None
                        }
                    }
                };
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
        let already_exited = {
            let mut table = self.exits.lock().unwrap();
            match table.early.remove(&pid) {
                Some(code) => Some((exit_tx, code)),
                None => {
                    table.waiters.insert(pid, exit_tx);
                    None
                }
            }
        };
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
