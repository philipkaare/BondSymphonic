//! The no-sandbox backend: processes run as ordinary children of the daemon.
//!
//! It is the fallback on hosts without bubblewrap and the backend the test
//! suite uses on Windows, so it has to support both pipes and a real PTY.

use super::*;
use futures::StreamExt;
use portable_pty::{native_pty_system, CommandBuilder, PtySize as PPtySize};
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// A killer closure registered with a handle so `shutdown` can reach the child.
type Killer = Box<dyn Fn() + Send + Sync>;

pub struct NoopBackend;

struct NoopHandle {
    spec: SandboxSpec,
    children: Mutex<Vec<Killer>>,
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
        std::fs::create_dir_all(&spec.home).map_err(|e| RpcError::io(&e))?;
        std::fs::create_dir_all(&spec.run_dir).map_err(|e| RpcError::io(&e))?;
        Ok(Arc::new(NoopHandle {
            spec: spec.clone(),
            children: Mutex::new(Vec::new()),
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
            Some(size) => spawn_pty(&cmd.argv, &env, &cwd, size, &self.children),
        }
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        let killers = std::mem::take(&mut *self.children.lock().unwrap());
        for k in killers {
            k();
        }
        Ok(())
    }
}

fn spawn_piped(
    argv: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    children: &Mutex<Vec<Killer>>,
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
    let mut child = c
        .spawn()
        .map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    let pid = child.id().unwrap_or(0);
    let stdin: ChildWriter = Box::pin(child.stdin.take().expect("stdin piped"));
    let stdout: ChildReader = Box::pin(child.stdout.take().expect("stdout piped"));
    let stderr: ChildReader = Box::pin(child.stderr.take().expect("stderr piped"));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let kill_flag = Arc::new(AtomicBool::new(false));
    let kf = kill_flag.clone();
    tokio::spawn(async move {
        let code = loop {
            if kf.load(Ordering::SeqCst) {
                let _ = child.start_kill();
            }
            match tokio::time::timeout(std::time::Duration::from_millis(100), child.wait()).await {
                Ok(Ok(st)) => break st.code().unwrap_or(-1),
                Ok(Err(_)) => break -1,
                Err(_) => continue,
            }
        };
        let _ = tx.send(code);
    });
    // Two closures over the same flag: one handed back to the caller, one kept
    // for shutdown().
    let kf_caller = kill_flag.clone();
    let killer: Killer = Box::new(move || kf_caller.store(true, Ordering::SeqCst));
    let kf_shutdown = kill_flag.clone();
    children
        .lock()
        .unwrap()
        .push(Box::new(move || kf_shutdown.store(true, Ordering::SeqCst)));
    Ok(SandboxChild {
        pid,
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr: Some(stderr),
        pty: None,
        exit: rx,
        killer,
    })
}

fn spawn_pty(
    argv: &[String],
    env: &[(String, String)],
    cwd: &std::path::Path,
    size: PtySize,
    children: &Mutex<Vec<Killer>>,
) -> Result<SandboxChild, RpcError> {
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
    let mut child = pair
        .slave
        .spawn_command(cb)
        .map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let mut master_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| sandbox_error(e.to_string()))?;
    let mut master_writer = pair
        .master
        .take_writer()
        .map_err(|e| sandbox_error(e.to_string()))?;
    let master = Arc::new(Mutex::new(pair.master));

    // Reader pump: blocking thread -> mpsc<Vec<u8>> -> an AsyncRead adapter.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
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
    std::thread::spawn(move || {
        while let Some(chunk) = in_rx.blocking_recv() {
            if master_writer.write_all(&chunk).is_err() || master_writer.flush().is_err() {
                break;
            }
        }
    });
    let writer: ChildWriter = Box::pin(ChannelWriter { tx: in_tx });

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
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        let _ = tx.send(code);
    });
    let kc = killer_child.clone();
    let killer: Killer = Box::new(move || {
        if let Some(k) = kc.lock().unwrap().as_mut() {
            let _ = k.kill();
        }
    });
    let kc2 = killer_child.clone();
    children.lock().unwrap().push(Box::new(move || {
        if let Some(k) = kc2.lock().unwrap().as_mut() {
            let _ = k.kill();
        }
    }));
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
    })
}

/// `AsyncWrite` that forwards chunks to a blocking writer thread.
pub(crate) struct ChannelWriter {
    pub(crate) tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl AsyncWrite for ChannelWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.tx.try_reserve() {
            Ok(permit) => {
                permit.send(buf.to_vec());
                std::task::Poll::Ready(Ok(buf.len()))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::other("pty closed"))),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(dir: &std::path::Path) -> NoopHandle {
        NoopHandle {
            spec: SandboxSpec {
                id: "ws_unit".into(),
                rw_binds: vec![],
                ro_binds: vec![],
                home: dir.join("home"),
                run_dir: dir.join("run"),
                env: vec![("FROM_SPEC".into(), "1".into())],
                cwd: dir.to_path_buf(),
            },
            children: Mutex::new(Vec::new()),
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
}
