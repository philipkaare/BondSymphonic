//! PID 1 inside the bubblewrap sandbox. Spawns processes on request and hands
//! their stdio / PTY master back to the daemon over a Unix socket with SCM_RIGHTS.
//!
//! This runs in a separate process from the daemon, inside the sandbox, so it
//! deliberately uses blocking std plus threads: no tokio runtime is started
//! here.

use super::protocol::{encode, InitReply, InitRequest};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, IoSlice, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Live child pids, keyed so the reaper only reports processes we started.
type Children = Arc<Mutex<HashSet<i32>>>;
/// Every daemon connection, so exits can be broadcast to all of them.
type Conns = Arc<Mutex<Vec<UnixStream>>>;

/// How long a child gets between SIGTERM and SIGKILL during shutdown.
const GRACE: Duration = Duration::from_secs(5);

pub fn run(socket: &std::path::Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let children: Children = Arc::new(Mutex::new(HashSet::new()));
    let conns: Conns = Arc::new(Mutex::new(Vec::new()));

    // Reaper: waitpid(-1) loop broadcasting Exited to every connection.
    {
        let children = children.clone();
        let conns = conns.clone();
        std::thread::spawn(move || loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => {
                    notify_exit(&conns, &children, pid.as_raw(), code)
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    notify_exit(&conns, &children, pid.as_raw(), 128 + sig as i32)
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        });
    }

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => break,
        };
        conns.lock().unwrap().push(stream.try_clone()?);
        let children = children.clone();
        let conns = conns.clone();
        std::thread::spawn(move || serve(stream, children, conns));
    }
    Ok(())
}

fn notify_exit(conns: &Conns, children: &Children, pid: i32, code: i32) {
    // Blocks until a concurrent Spawn has registered its pid, so a process that
    // exits before `serve` records it is still reported.
    if !children.lock().unwrap().remove(&pid) {
        return;
    }
    let msg = encode(&InitReply::Exited {
        pid: pid as u32,
        code,
    });
    conns
        .lock()
        .unwrap()
        .retain_mut(|c| c.write_all(&msg).is_ok());
}

fn serve(stream: UnixStream, children: Children, conns: Conns) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let req: InitRequest = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match req {
            InitRequest::Spawn {
                id,
                argv,
                env,
                cwd,
                pty,
            } => {
                // The lock is held across the spawn so the reaper cannot report
                // a fast-exiting child before it is registered.
                let mut registry = children.lock().unwrap();
                match spawn(&argv, &env, cwd.as_deref(), pty) {
                    Ok((pid, fds)) => {
                        registry.insert(pid);
                        drop(registry);
                        let msg = encode(&InitReply::Spawned {
                            id,
                            pid: pid as u32,
                            has_pty: pty.is_some(),
                        });
                        let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
                        let _ = sendmsg::<()>(
                            stream.as_raw_fd(),
                            &[IoSlice::new(&msg)],
                            &[ControlMessage::ScmRights(&raw)],
                            MsgFlags::empty(),
                            None,
                        );
                        // fds dropped here: the daemon now owns its copies.
                    }
                    Err(e) => {
                        drop(registry);
                        let _ = (&stream)
                            .write_all(&encode(&InitReply::SpawnFailed { id, message: e }));
                    }
                }
            }
            InitRequest::Kill { pid, signal } => {
                let _ = kill(Pid::from_raw(pid as i32), Signal::try_from(signal).ok());
            }
            InitRequest::Shutdown => {
                shutdown_all(&children);
                let _ = (&stream).write_all(&encode(&InitReply::ShuttingDown));
                std::process::exit(0);
            }
        }
    }
    // Daemon went away: tear everything down.
    conns
        .lock()
        .unwrap()
        .retain(|c| c.as_raw_fd() != stream.as_raw_fd());
    if conns.lock().unwrap().is_empty() {
        shutdown_all(&children);
        std::process::exit(0);
    }
}

fn shutdown_all(children: &Children) {
    let pids: Vec<i32> = children.lock().unwrap().iter().copied().collect();
    for p in &pids {
        let _ = kill(Pid::from_raw(-*p), Signal::SIGTERM);
        let _ = kill(Pid::from_raw(*p), Signal::SIGTERM);
    }
    let deadline = std::time::Instant::now() + GRACE;
    while std::time::Instant::now() < deadline && !children.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in &pids {
        let _ = kill(Pid::from_raw(-*p), Signal::SIGKILL);
        let _ = kill(Pid::from_raw(*p), Signal::SIGKILL);
    }
}

/// Marks `fd` close-on-exec so a concurrent spawn on another thread cannot
/// inherit it and hold a pipe or PTY open past its owner's exit.
fn set_cloexec(fd: &OwnedFd) -> Result<(), String> {
    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Returns (pid, fds to pass): `[stdin, stdout, stderr]` or `[pty_master]`.
fn spawn(
    argv: &[String],
    env: &[(String, String)],
    cwd: Option<&str>,
    pty: Option<(u16, u16)>,
) -> Result<(i32, Vec<OwnedFd>), String> {
    if argv.is_empty() {
        return Err("empty argv".into());
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    match pty {
        None => {
            // O_CLOEXEC: `Stdio::from` dup2s into 0/1/2 after fork, which clears
            // the flag for the child's own stdio.
            let (in_r, in_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(|e| e.to_string())?;
            let (out_r, out_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(|e| e.to_string())?;
            let (err_r, err_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(|e| e.to_string())?;
            cmd.stdin(Stdio::from(in_r))
                .stdout(Stdio::from(out_w))
                .stderr(Stdio::from(err_w));
            unsafe {
                cmd.pre_exec(|| {
                    nix::unistd::setsid().ok();
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            Ok((child.id() as i32, vec![in_w, out_r, err_r]))
        }
        Some((cols, rows)) => {
            let ws = Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let pty = openpty(Some(&ws), None).map_err(|e| e.to_string())?;
            set_cloexec(&pty.master)?;
            set_cloexec(&pty.slave)?;
            let slave_fd = pty.slave.as_raw_fd();
            cmd.stdin(Stdio::from(
                pty.slave.try_clone().map_err(|e| e.to_string())?,
            ))
            .stdout(Stdio::from(
                pty.slave.try_clone().map_err(|e| e.to_string())?,
            ))
            .stderr(Stdio::from(pty.slave));
            cmd.env("TERM", "xterm-256color");
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::setsid().map_err(std::io::Error::other)?;
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            Ok((child.id() as i32, vec![pty.master]))
        }
    }
}
