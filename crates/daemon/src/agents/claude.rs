//! The Claude Code adapter: one `claude` process per agent, driven over the
//! `stream-json` protocol on its stdin and stdout.
//!
//! [`claude_stream`](super::claude_stream) owns the wire format; this module
//! owns the process. It starts `claude` inside the workspace sandbox, pumps its
//! stdout through the parser into an [`AgentSink`], writes user turns,
//! permission answers and interrupts to its stdin, and ends the process on
//! `stop`.
//!
//! Everything the agent says reaches the daemon through the sink, so the
//! transcript on disk and the events on the bus are the same sequence.

use super::claude_stream::{control_response_line, interrupt_line, parse_line, user_line, Parsed};
use super::AgentSink;
use crate::ids::new_id;
use crate::sandbox::{ChildWriter, SandboxCommand, SandboxHandle, Signaller};
use async_trait::async_trait;
use bondsymphonic_proto::{
    AgentMessageBody, AgentStartOptions, AgentState, ErrorCode, PermissionDecision, RpcError,
};
use futures::future::{FutureExt, Shared};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use tracing::warn;

/// The Claude Code release this adapter's command line was verified against.
/// A different version is not refused — the flags are stable and the protocol
/// degrades gracefully — but it is worth one warning in the log.
pub const TESTED_CLAUDE_VERSION: &str = "2.1.263";

/// Values the CLI accepts for `--permission-mode`. Checked here so a bad one is
/// an `InvalidParams` on `agent.start` rather than a process that exits with a
/// usage error a second later.
const PERMISSION_MODES: [&str; 7] = [
    "default",
    "acceptEdits",
    "plan",
    "dontAsk",
    "bypassPermissions",
    "auto",
    "manual",
];

/// How long `interrupt` waits for the CLI to abandon its turn on its own before
/// sending SIGINT.
const INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `stop` waits for the process to exit after its stdin is closed.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long `stop` waits after SIGKILL before giving up on an exit code.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `stop` lets the reader finish the output already in the pipe before
/// it is aborted.
const READER_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Lines of the agent's stderr kept for the exit detail. Enough to carry a
/// login prompt or a stack trace, not enough to fill an event.
const STDERR_TAIL: usize = 20;

/// The `System` subtype under which an answered permission request is recorded.
/// Shared with the IDE by value, not by type: it is wire format.
pub const PERMISSION_REPLY_SUBTYPE: &str = "permission_reply";

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// The backend whose sandbox has mounts of its own, and so needs the CLI bound
/// in rather than found on a path.
const SANDBOXED_BACKEND: &str = "linux_bwrap";

/// Where the daemon binds the host's `claude` inside a bubblewrap sandbox.
///
/// It has to be bound in because the sandbox replaces `/home` with a tmpfs and
/// mounts the *workspace's* home at `/home/<user>`: the daemon user's own
/// `~/.local/bin/claude` is not there to be found, whatever the sandbox `PATH`
/// says. One read-only bind of one file is the whole of what has to come in --
/// Claude Code installs as a single native executable, with no runtime or
/// library directory beside it.
///
/// `/opt/bs` rather than a path under `/usr`: `bwrap_args` gives the sandbox its
/// own empty `/opt`, so the mount point can be created inside the sandbox and
/// nothing is created on the host, and nothing the distribution installs is
/// shadowed.
pub const CLAUDE_IN_SANDBOX: &str = "/opt/bs/claude";

/// The path Claude Code installs itself at, which is the one the daemon trusts.
///
/// Not `PATH`: under WSL the Windows `PATH` is appended to the Linux one, so a
/// bare `claude` can resolve to a `/mnt/c/...` Windows build -- slow to start,
/// and the wrong answer for a binary that has to run inside a Linux sandbox.
pub fn pinned_claude_path() -> PathBuf {
    crate::setup::host_home()
        .join(".local")
        .join("bin")
        .join("claude")
}

/// The daemon user's real `claude`, resolved to the file a bind can point at.
///
/// `~/.local/bin/claude` is a symlink into `~/.local/share/claude/versions/`,
/// and a read-only bind has to name the file the symlink resolves to, so the
/// answer is always canonicalised. `PATH` is the fallback for an install that
/// put the binary somewhere else, with anything under `/mnt/` refused for the
/// reason [`pinned_claude_path`] gives.
pub fn host_claude_bin() -> Option<PathBuf> {
    let usable = |p: &std::path::Path| p.is_file() && !p.starts_with("/mnt/");
    if let Ok(real) = std::fs::canonicalize(pinned_claude_path()) {
        if usable(&real) {
            return Some(real);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty() && !dir.starts_with("/mnt/"))
        .filter_map(|dir| std::fs::canonicalize(dir.join("claude")).ok())
        .find(|real| usable(real))
}

/// The read-only bind that makes [`host_claude_bin`] reachable at
/// [`CLAUDE_IN_SANDBOX`], or `None` when Claude Code is not installed.
///
/// `workspace::lifecycle::spec_for` puts this in every bwrap workspace's spec,
/// and the bwrap integration test builds the same pair, so the path the adapter
/// spawns and the path the sandbox binds cannot drift apart.
pub fn claude_ro_bind() -> Option<(PathBuf, PathBuf)> {
    Some((host_claude_bin()?, PathBuf::from(CLAUDE_IN_SANDBOX)))
}

fn claude_not_installed() -> RpcError {
    RpcError::new(
        ErrorCode::PrereqMissing,
        format!(
            "Claude Code is not installed: no executable at {} and none on the daemon's PATH.              Install it from the setup page, or point BS_CLAUDE_BIN at a stand-in.",
            pinned_claude_path().display()
        ),
    )
}

/// The program to run: the stand-in named by `$BS_CLAUDE_BIN`, or the real CLI.
///
/// `BS_CLAUDE_BIN` is a test and development hook: it names the command to run
/// instead of `claude`, parsed the way a shell would (`"python" "fake.py"`
/// becomes two argv entries), so a stand-in can be an interpreter plus a
/// script. It wins on every backend; under `linux_bwrap` whatever it names has
/// to be reachable *inside* the sandbox, which means under the worktree or
/// another bound path. The daemon reads it when it spawns an agent, so it has
/// to be set in the daemon's own environment -- when the IDE launches the
/// daemon through `wsl.exe`, that means passing it through on the command line.
///
/// Without the hook the answer is an absolute path, never a bare `claude`:
/// under `linux_bwrap` the fixed path the binary is bound to, and otherwise the
/// host path it was resolved to. Nothing depends on the sandbox `PATH`, and a
/// missing install is a `PrereqMissing` naming the path rather than an exec
/// failure a second later.
fn claude_bin(backend: &str) -> Result<Vec<String>, RpcError> {
    if let Ok(raw) = std::env::var("BS_CLAUDE_BIN") {
        let argv = shell_words::split(&raw)
            .map_err(|e| RpcError::invalid_params(format!("BS_CLAUDE_BIN: {e}")))?;
        if argv.is_empty() {
            return Err(RpcError::invalid_params("BS_CLAUDE_BIN: no program to run"));
        }
        return Ok(argv);
    }
    let host = host_claude_bin().ok_or_else(claude_not_installed)?;
    if backend == SANDBOXED_BACKEND {
        Ok(vec![CLAUDE_IN_SANDBOX.to_owned()])
    } else {
        Ok(vec![host.to_string_lossy().into_owned()])
    }
}

/// The full command line for one agent.
///
/// The base flags are pinned: `-p` with `stream-json` in both directions is the
/// only mode that gives a line-per-message protocol,`--verbose` is what makes
/// the CLI emit the `system init` and per-tool lines rather than a single final
/// result, `--include-partial-messages` is what makes text deltas arrive at
/// all, and `--permission-prompts host` is what makes the CLI ask us
/// (`control_request`) instead of denying anything that would prompt.
/// `backend` is the sandbox backend's [`name`](crate::sandbox::SandboxBackend::name),
/// which is what decides whether the program is named by its host path or by
/// the path it is bound to inside the sandbox.
pub fn claude_argv(options: &AgentStartOptions, backend: &str) -> Result<Vec<String>, RpcError> {
    let mut argv = claude_bin(backend)?;
    argv.extend(
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompts",
            "host",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    if let Some(session) = options.resume_session.as_deref().filter(|s| !s.is_empty()) {
        argv.push("--resume".to_owned());
        argv.push(session.to_owned());
    }
    if let Some(model) = options.model.as_deref().filter(|s| !s.is_empty()) {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(mode) = options.permission_mode.as_deref().filter(|s| !s.is_empty()) {
        if !PERMISSION_MODES.contains(&mode) {
            return Err(RpcError::invalid_params(format!(
                "permission_mode must be one of {}",
                PERMISSION_MODES.join(", ")
            )));
        }
        argv.push("--permission-mode".to_owned());
        argv.push(mode.to_owned());
    }
    Ok(argv)
}

/// Warns once per daemon lifetime when the installed CLI is not the version
/// this adapter was verified against.
///
/// Skipped entirely when `BS_CLAUDE_BIN` is set: that hook points at a stand-in
/// whose version says nothing about the protocol, and running it would start a
/// second copy of it for no reason.
async fn warn_on_untested_version() {
    static CHECKED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    CHECKED
        .get_or_init(|| async {
            if std::env::var_os("BS_CLAUDE_BIN").is_some() {
                return;
            }
            let Some(bin) = host_claude_bin() else {
                warn!(
                    path = %pinned_claude_path().display(),
                    "claude is not installed; agents cannot start"
                );
                return;
            };
            let out = tokio::process::Command::new(&bin)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .output()
                .await;
            match out {
                Ok(out) => {
                    let version = String::from_utf8_lossy(&out.stdout);
                    let version = version.trim();
                    if !version.starts_with(TESTED_CLAUDE_VERSION) {
                        warn!(
                            found = version,
                            tested = TESTED_CLAUDE_VERSION,
                            "claude version differs from the one this adapter was tested against"
                        );
                    }
                }
                Err(e) => {
                    warn!(bin = %bin.display(), error = %e, "could not run `claude --version`")
                }
            }
        })
        .await;
}

fn agent_error(msg: impl Into<String>) -> RpcError {
    RpcError::new(ErrorCode::AgentError, msg)
}

/// The last few stderr lines, kept so an exit can say why.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// The half of the adapter that only exists while the process does.
struct Running {
    /// Serialises the writers: `send`, `permission_reply` and `interrupt` can
    /// all be in flight at once and each writes one whole line.
    stdin: tokio::sync::Mutex<Option<ChildWriter>>,
    signal: Arc<Signaller>,
    /// The exit code, awaitable more than once.
    exit: Shared<futures::future::BoxFuture<'static, i32>>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

/// One `claude` process and everything needed to talk to it.
pub struct ClaudeAdapter {
    sink: AgentSink,
    handle: Arc<dyn SandboxHandle>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    child: Option<Running>,
    /// Permission requests the CLI has asked and nobody has answered. Shared
    /// with the reader task, which is what adds to it.
    pending_request_ids: Arc<Mutex<HashSet<String>>>,
    stderr_tail: StderrTail,
    /// Claimed by whichever of the reader task and `stop` gets there first, so
    /// one process produces exactly one `Exited`.
    exit_announced: Arc<AtomicBool>,
}

/// What every adapter must do, whatever it drives underneath.
#[async_trait]
pub trait AgentAdapter: Send {
    async fn start(&mut self) -> Result<(), RpcError>;
    async fn send(&mut self, text: String) -> Result<(), RpcError>;
    async fn permission_reply(
        &mut self,
        request_id: String,
        decision: PermissionDecision,
        updated_input: Option<Value>,
        message: Option<String>,
    ) -> Result<(), RpcError>;
    async fn interrupt(&mut self) -> Result<(), RpcError>;
    async fn stop(&mut self) -> Result<(), RpcError>;
}

impl ClaudeAdapter {
    pub fn new(
        sink: AgentSink,
        handle: Arc<dyn SandboxHandle>,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            sink,
            handle,
            argv,
            env,
            cwd,
            child: None,
            pending_request_ids: Arc::new(Mutex::new(HashSet::new())),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            exit_announced: Arc::new(AtomicBool::new(false)),
        }
    }

    fn running(&self) -> Result<&Running, RpcError> {
        self.child
            .as_ref()
            .ok_or_else(|| agent_error("agent is not running"))
    }

    /// Writes one protocol line to the CLI's stdin.
    ///
    /// A closed stdin means the process is gone: the caller gets an
    /// `AgentError` rather than a silent no-op, which is what tells the IDE the
    /// turn it just typed went nowhere.
    async fn write_line(&self, line: String) -> Result<(), RpcError> {
        let running = self.running()?;
        let mut guard = running.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| agent_error("agent stdin is closed"))?;
        // The flush is as likely to be where a broken pipe surfaces as the
        // write, so both failures take the same path.
        let outcome = match stdin.write_all(line.as_bytes()).await {
            Ok(()) => stdin.flush().await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                // The process is gone; drop the writer so later calls fail
                // quickly and the descriptor is not held open.
                *guard = None;
                Err(agent_error(format!("writing to the agent: {e}")))
            }
        }
    }

    /// What the agent said on its way out, with the exit code after it; just
    /// the code when it said nothing.
    ///
    /// The stderr tail leads because it is the part a person can act on. The
    /// commonest way for a Claude agent to die is dying immediately -- not
    /// logged in, no API key, a bad flag -- and this string is what the
    /// transcript's banner shows, so "Invalid API key" has to be the first
    /// thing in it rather than the tail of a sentence about an exit code.
    fn exit_detail(code: i32, tail: &StderrTail) -> String {
        let tail = tail.lock();
        if tail.is_empty() {
            format!("exit code {code}")
        } else {
            format!(
                "{} (exit code {code})",
                tail.iter().cloned().collect::<Vec<_>>().join("\n")
            )
        }
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    async fn start(&mut self) -> Result<(), RpcError> {
        warn_on_untested_version().await;
        let mut child = self
            .handle
            .spawn(SandboxCommand {
                argv: self.argv.clone(),
                env: self.env.clone(),
                cwd: Some(self.cwd.clone()),
                pty: None,
            })
            .await?;
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (i, o, e),
                _ => {
                    // Nothing can talk to this process, so it must not be left
                    // running inside the sandbox.
                    (child.killer)();
                    return Err(RpcError::internal(
                        "backend returned no pipes for the agent",
                    ));
                }
            };

        let exit_rx = child.exit;
        let exit: Shared<futures::future::BoxFuture<'static, i32>> =
            async move { exit_rx.await.unwrap_or(-1) }.boxed().shared();

        // stderr: warned line by line, and the tail kept for the exit detail.
        // This is where "not logged in" shows up.
        let agent_id = self.sink.agent_id().clone();
        let tail = self.stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                warn!(agent = %agent_id, "claude: {line}");
                let mut tail = tail.lock();
                if tail.len() == STDERR_TAIL {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });

        // stdout: the protocol.
        let sink = self.sink.clone();
        let pending = self.pending_request_ids.clone();
        let announced = self.exit_announced.clone();
        let tail = self.stderr_tail.clone();
        let exit_for_reader = exit.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        warn!(agent = %sink.agent_id(), error = %e, "agent stdout read failed");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                for item in parse_line(&line) {
                    match item {
                        Parsed::Message(body) => {
                            if let AgentMessageBody::PermissionRequest { request_id, .. } = &body {
                                pending.lock().insert(request_id.clone());
                            }
                            sink.message(body).await;
                        }
                        Parsed::State(state, detail) => sink.state(state, detail).await,
                        Parsed::SessionId(id) => {
                            *sink.entry().session_id.lock() = Some(id);
                        }
                        Parsed::Nothing => {}
                    }
                }
            }
            // stdout is closed, so the process is on its way out.
            let code = exit_for_reader.await;
            // An error result already said why the turn failed, and it is the
            // more useful message of the two.
            if sink.entry().state().0 == AgentState::Error {
                return;
            }
            // `stop` may be ending this same process; the flag makes one of the
            // two announce and the other stay quiet, so a process that exits on
            // its own and is then stopped still produces one `Exited`.
            if !announced.swap(true, Ordering::SeqCst) {
                sink.state(
                    AgentState::Exited,
                    Some(ClaudeAdapter::exit_detail(code, &tail)),
                )
                .await;
            }
        });

        self.child = Some(Running {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            signal: Arc::new(child.signal),
            exit,
            reader_task,
            stderr_task,
        });
        Ok(())
    }

    /// The user's turn is recorded before it is sent, so it is in the
    /// transcript even if the write fails.
    async fn send(&mut self, text: String) -> Result<(), RpcError> {
        self.running()?;
        self.sink
            .message(AgentMessageBody::UserText { text: text.clone() })
            .await;
        self.write_line(user_line(&text)).await?;
        self.sink.state(AgentState::Working, None).await;
        Ok(())
    }

    async fn permission_reply(
        &mut self,
        request_id: String,
        decision: PermissionDecision,
        updated_input: Option<Value>,
        message: Option<String>,
    ) -> Result<(), RpcError> {
        if !self.pending_request_ids.lock().contains(&request_id) {
            return Err(RpcError::not_found(format!(
                "permission request {request_id}"
            )));
        }
        self.write_line(control_response_line(
            &request_id,
            decision,
            updated_input,
            message,
        ))
        .await?;
        // Only once the answer is really on its way: a failed write leaves the
        // request pending so the IDE can try again.
        self.pending_request_ids.lock().remove(&request_id);
        // The answer goes into the transcript, because the question did. A
        // permission request is a message and is replayed from disk; without a
        // matching answer beside it, a client that re-attaches folds the
        // request back up and raises a bar over a settled question -- and the
        // reply it then sends names a request this adapter has just forgotten.
        self.sink
            .message(AgentMessageBody::System {
                subtype: PERMISSION_REPLY_SUBTYPE.to_owned(),
                data: serde_json::json!({
                    "request_id": request_id,
                    "decision": decision,
                }),
            })
            .await;
        self.sink.state(AgentState::Working, None).await;
        Ok(())
    }

    /// Asks the CLI to abandon the turn, and follows up with SIGINT if it does
    /// not. The control request is the polite path and keeps the session usable;
    /// the signal is for a CLI that is wedged in a tool and not reading stdin.
    ///
    /// The fallback belongs to the turn it was armed for and to no other. The
    /// CLI normally answers in milliseconds and the user's next move is to type
    /// the corrected prompt, so two seconds later the agent is usually busy
    /// again on a different turn; a check of the state alone cannot tell the two
    /// apart, and the signal reaches the whole process group, so firing into the
    /// new turn kills the agent outright. The epoch is what distinguishes them:
    /// any state change at all -- the acknowledgement, the new prompt, an error
    /// -- retires this timer.
    async fn interrupt(&mut self) -> Result<(), RpcError> {
        let signal = self.running()?.signal.clone();
        self.write_line(interrupt_line(&new_id("req_"))).await?;
        let entry = self.sink.entry().clone();
        let agent_id = self.sink.agent_id().clone();
        // Snapshotted after the write, so a state change the CLI makes in
        // response to this very request already counts as an answer.
        let armed_at = entry.epoch();
        tokio::spawn(async move {
            tokio::time::sleep(INTERRUPT_GRACE).await;
            let unanswered = entry.epoch() == armed_at
                && matches!(
                    entry.state().0,
                    AgentState::Working | AgentState::WaitingPermission
                );
            if unanswered {
                warn!(agent = %agent_id, "interrupt was not acknowledged; sending SIGINT");
                (signal)(SIGINT);
            }
        });
        Ok(())
    }

    /// Ends the process: close stdin, which is how the CLI is asked to finish,
    /// then SIGKILL whatever is still there.
    async fn stop(&mut self) -> Result<(), RpcError> {
        let Some(mut running) = self.child.take() else {
            // Already stopped, or never started; `stop` has to stay idempotent
            // for `destroy`, and the flag keeps it from announcing twice.
            if !self.exit_announced.swap(true, Ordering::SeqCst) {
                self.sink.state(AgentState::Exited, None).await;
            }
            return Ok(());
        };
        // Dropping the writer closes the pipe, which the CLI reads as the end
        // of the conversation.
        drop(running.stdin.lock().await.take());

        let mut code = tokio::time::timeout(STOP_GRACE, running.exit.clone())
            .await
            .ok();
        if code.is_none() {
            warn!(agent = %self.sink.agent_id(), "agent did not exit; killing it");
            (running.signal)(SIGKILL);
            code = tokio::time::timeout(KILL_GRACE, running.exit.clone())
                .await
                .ok();
        }
        // Whatever the process wrote before it went is still worth recording,
        // so the reader gets a moment to finish; it is aborted only if it does
        // not, which is what a SIGKILLed grandchild still holding the pipe open
        // looks like.
        if tokio::time::timeout(READER_DRAIN, &mut running.reader_task)
            .await
            .is_err()
        {
            running.reader_task.abort();
        }
        running.stderr_task.abort();
        self.pending_request_ids.lock().clear();
        if !self.exit_announced.swap(true, Ordering::SeqCst) {
            let detail = code.map(|c| Self::exit_detail(c, &self.stderr_tail));
            self.sink.state(AgentState::Exited, detail).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BS_CLAUDE_BIN` is process-wide, so every case that touches it lives in
    /// one test.
    #[test]
    fn claude_argv_pins_the_flags_the_program_and_the_permission_mode() {
        const BWRAP: &str = SANDBOXED_BACKEND;
        const NOOP: &str = "noop";
        let plain = AgentStartOptions {
            command: None,
            resume_session: None,
            model: None,
            permission_mode: None,
            api_key: None,
        };
        // The flag assertions run under the hook, so they say nothing about
        // whether this host has Claude Code installed.
        std::env::set_var("BS_CLAUDE_BIN", "claude");
        assert_eq!(
            claude_argv(&plain, NOOP).unwrap(),
            [
                "claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompts",
                "host",
            ]
        );

        let full = AgentStartOptions {
            resume_session: Some("sess-9".into()),
            model: Some("sonnet".into()),
            permission_mode: Some("acceptEdits".into()),
            ..plain.clone()
        };
        let argv = claude_argv(&full, NOOP).unwrap();
        assert_eq!(
            &argv[argv.len() - 6..],
            [
                "--resume",
                "sess-9",
                "--model",
                "sonnet",
                "--permission-mode",
                "acceptEdits"
            ]
        );

        let bad = AgentStartOptions {
            permission_mode: Some("whatever".into()),
            ..plain.clone()
        };
        assert_eq!(
            claude_argv(&bad, NOOP).unwrap_err().code,
            ErrorCode::InvalidParams
        );
        // Every mode the CLI documents, plus `default`, is accepted.
        for mode in PERMISSION_MODES {
            let opts = AgentStartOptions {
                permission_mode: Some(mode.into()),
                ..plain.clone()
            };
            assert!(claude_argv(&opts, NOOP).is_ok(), "{mode} must be accepted");
        }

        // The hook replaces the program, is split the way a shell would, and
        // wins on every backend -- including the sandboxed one, where what it
        // names has to be reachable from inside.
        std::env::set_var("BS_CLAUDE_BIN", "\"python\" \"/tmp/fake claude.py\"");
        for backend in [NOOP, BWRAP] {
            let argv = claude_argv(&plain, backend).unwrap();
            assert_eq!(
                &argv[..3],
                ["python", "/tmp/fake claude.py", "-p"],
                "the hook must win on {backend}"
            );
        }
        std::env::set_var("BS_CLAUDE_BIN", "   ");
        assert_eq!(
            claude_argv(&plain, NOOP).unwrap_err().code,
            ErrorCode::InvalidParams
        );

        // Without the hook the program is always an absolute path, never a bare
        // `claude`: the sandbox `PATH` leads with the *workspace's* home, so a
        // bare name does not resolve there at all.
        std::env::remove_var("BS_CLAUDE_BIN");
        match host_claude_bin() {
            Some(host) => {
                assert_eq!(claude_argv(&plain, BWRAP).unwrap()[0], CLAUDE_IN_SANDBOX);
                assert_eq!(
                    claude_argv(&plain, NOOP).unwrap()[0],
                    host.to_string_lossy()
                );
                assert!(host.is_absolute(), "{host:?}");
            }
            // A host without Claude Code -- every Windows developer machine,
            // and CI -- must refuse the start with a prerequisite error naming
            // the path, not spawn something that cannot exist.
            None => {
                for backend in [NOOP, BWRAP] {
                    let e = claude_argv(&plain, backend).unwrap_err();
                    assert_eq!(e.code, ErrorCode::PrereqMissing, "{backend}");
                    assert!(
                        e.message
                            .contains(&pinned_claude_path().display().to_string()),
                        "the error must name the path it looked at: {}",
                        e.message
                    );
                }
            }
        }
    }

    /// The bind and the argv agree by construction: whatever
    /// `workspace::lifecycle::spec_for` mounts is exactly what the adapter
    /// spawns, so the two cannot drift apart.
    #[test]
    fn the_sandbox_bind_lands_where_the_argv_points() {
        match claude_ro_bind() {
            Some((host, in_sandbox)) => {
                assert_eq!(in_sandbox, PathBuf::from(CLAUDE_IN_SANDBOX));
                assert_eq!(Some(host), host_claude_bin());
            }
            None => assert!(host_claude_bin().is_none()),
        }
    }

    #[test]
    fn the_exit_detail_carries_the_stderr_tail() {
        let tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        assert_eq!(ClaudeAdapter::exit_detail(1, &tail), "exit code 1");
        tail.lock().push_back("Invalid API key".to_owned());
        // The tail leads: this is what the transcript banner shows, and the
        // reason is more use than the number.
        assert_eq!(
            ClaudeAdapter::exit_detail(1, &tail),
            "Invalid API key (exit code 1)"
        );
    }
}
