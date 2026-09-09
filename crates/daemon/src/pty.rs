//! Terminal sessions attached to a workspace sandbox.
//!
//! A PTY session is a process started inside the workspace's sandbox with a
//! pseudo-terminal attached. The manager owns the live sessions: `open` starts
//! one and spawns a pump task that turns terminal output into `pty.output`
//! events, `write`/`resize` reach the running session, and `close` terminates
//! it. Every session ends with exactly one `pty.exit` event, published by the
//! pump task, which then forgets the session; a request naming a session that
//! has ended is a `NotFound`.

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::sandbox::{PtySize, SandboxCommand};
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

/// Terminal output is published in chunks of at most this many bytes.
const CHUNK: usize = 4096;

/// One live terminal. The reader half lives in the pump task instead, so
/// nothing here is held across a read.
struct Session {
    workspace_id: WorkspaceId,
    /// Serialises concurrent `pty.write` calls on one terminal.
    writer: Mutex<crate::sandbox::ChildWriter>,
    resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync>,
    killer: Box<dyn Fn() + Send + Sync>,
}

/// `sessions` is an `Arc` so the per-PTY pump task can remove its own entry on exit.
pub struct PtyManager {
    events: EventBus,
    sessions: Arc<Mutex<HashMap<PtyId, Arc<Session>>>>,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
                let argv = shell_words::split(cmd)
                    .map_err(|e| RpcError::invalid_params(format!("command: {e}")))?;
                if argv.is_empty() {
                    return Err(RpcError::invalid_params("command: no program to run"));
                }
                argv
            }
            None => Self::default_command(d.backend.name()),
        };
        let size = PtySize {
            cols: p.cols.max(2),
            rows: p.rows.max(1),
        };
        let mut child = handle
            .spawn(SandboxCommand {
                argv,
                env: vec![],
                cwd: None,
                pty: Some(size),
            })
            .await?;
        let Some(pty) = child.pty.take() else {
            // The child is running but there is no terminal to reach it by, so
            // it would keep running with nobody able to stop it. Kill it here
            // rather than leaking a process into the sandbox.
            (child.killer)();
            return Err(RpcError::internal("backend returned no pty"));
        };
        let id: PtyId = new_id(PtyId::PREFIX).as_str().into();
        let session = Arc::new(Session {
            workspace_id: ws.id.clone(),
            writer: Mutex::new(pty.writer),
            resizer: pty.resizer,
            killer: child.killer,
        });
        self.sessions.lock().await.insert(id.clone(), session);

        // Output pump + exit watcher.
        let events = self.events.clone();
        let (pid, wsid) = (id.clone(), ws.id.clone());
        let mut reader = pty.reader;
        let exit = child.exit;
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; CHUNK];
            let mut exit = exit;
            let mut code: Option<i32> = None;
            loop {
                let read = match code {
                    // Still running: whichever comes first, output or the exit.
                    None => tokio::select! {
                        r = reader.read(&mut buf) => r,
                        c = &mut exit => {
                            code = Some(c.unwrap_or(-1));
                            continue;
                        }
                    },
                    // The child is gone. Anything it left in the terminal is
                    // still worth publishing, but the wait is time-boxed: a
                    // Windows ConPTY master stays readable while its handle
                    // lives, so end of file may never arrive.
                    Some(_) => match tokio::time::timeout(DRAIN, reader.read(&mut buf)).await {
                        Ok(r) => r,
                        Err(_) => break,
                    },
                };
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => events.publish(
                        Some(wsid.clone()),
                        Event::PtyOutput {
                            pty_id: pid.clone(),
                            data_b64: b64(&buf[..n]),
                        },
                    ),
                }
            }
            let code = match code {
                Some(code) => code,
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
            events.publish(Some(wsid), Event::PtyExit { pty_id: pid, code });
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
    pub async fn close(&self, id: &PtyId) -> Result<Empty, RpcError> {
        let s = self.session(id).await?;
        (s.killer)();
        Ok(Empty {})
    }

    /// Closes every PTY belonging to `ws`, used when the workspace goes away.
    pub async fn close_workspace(&self, ws: &WorkspaceId) {
        let victims: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|s| &s.workspace_id == ws)
            .cloned()
            .collect();
        for s in victims {
            (s.killer)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
