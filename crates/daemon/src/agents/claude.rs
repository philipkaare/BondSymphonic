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

/// How long an exit waits for the stderr already in the pipe before it gives up
/// on saying why the agent went.
///
/// The tail is the useful half of an exit detail -- "Invalid API key", "Not
/// logged in" -- and it is read by a task of its own, so without this the detail
/// is whatever that task happened to have got through when the process died.
/// Bounded because a SIGKILLed grandchild can hold the write end open for ever,
/// and an exit nobody announces is worse than one that cannot say why.
const STDERR_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// How long `agent.start` waits for `claude --version` before it gives up on the
/// program it was about to run.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `data.reason` values on the errors a client is meant to match on rather than
/// read. Wire format, shared with the IDE by value.
pub const REASON_WAITING_PERMISSION: &str = "waiting_permission";
pub const REASON_AGENT_EXITED: &str = "agent_exited";
pub const REASON_PROBE_TIMEOUT: &str = "claude_probe_timeout";

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

/// Copies the repository's own `[claude] settings` file into the workspace
/// home, replacing the daemon user's `~/.claude/settings.json` for this agent.
///
/// This is how a repository pins what its agents may do -- allowed tools, hooks,
/// a permission mode -- for everyone who opens a workspace on it, which only
/// works if the repository's copy wins over whatever the user has. Read from the
/// *worktree*, not the source repository: the workspace is a checkout of a
/// branch of its own, and the file the user is looking at is the one that
/// applies.
///
/// Every failure is an error on `agent.start` rather than a warning, because
/// each one means the agent would run under settings nobody asked for. That
/// includes a `bondsymphonic.toml` that will not parse: it may be the one
/// naming the settings, and there is no way to tell.
pub fn apply_repo_settings(
    worktree: &std::path::Path,
    home: &std::path::Path,
) -> Result<(), RpcError> {
    let config = crate::runs::config::load_repo_config(worktree)
        .map_err(|e| RpcError::invalid_params(format!("[claude] settings: {e}")))?;
    let Some(rel) = config.and_then(|c| c.claude.settings) else {
        return Ok(());
    };
    // `fs::resolve` is the one place that decides what is inside a worktree:
    // absolute paths, `..` and symlinks that point out are all refused there,
    // and the message it gives names the rule that was broken.
    let from = crate::fs::resolve(worktree, &rel).map_err(|e| {
        RpcError::invalid_params(format!("[claude] settings {rel:?}: {}", e.message))
    })?;
    // Opened once, here, and every read after this is of *this handle*.
    // `fs::resolve` proved the path was inside the worktree, but the value it
    // returned used to be re-opened twice more — `is_file()` and then
    // `fs::copy`, which follows symlinks. An agent renaming that name back and
    // forth between a regular file and a link to, say, `~/.ssh/id_ed25519` wins
    // the window occasionally and has the link followed. One handle closes it.
    let mut src = open_no_follow(&from).map_err(|e| {
        RpcError::invalid_params(format!(
            "[claude] settings {rel:?}: no such readable file in the workspace ({e})"
        ))
    })?;
    if !src.metadata().map(|m| m.is_file()).unwrap_or(false) {
        return Err(RpcError::invalid_params(format!(
            "[claude] settings {rel:?}: no such file in the workspace"
        )));
    }
    let dir = home.join(".claude");
    let to = dir.join("settings.json");
    write_settings(&mut src, home, &dir, &to).map_err(|e| {
        RpcError::new(
            ErrorCode::IoError,
            format!("[claude] settings {rel:?}: copying it into the workspace home failed: {e}"),
        )
    })
}

/// Opens `path` for reading without following a final symlink where the
/// platform can say so.
///
/// On unix `O_NOFOLLOW` makes the rule the kernel's rather than a check this
/// code has to win a race against. Windows has no equivalent on `open`, and no
/// sandbox either — the caller's `metadata()` check on the returned handle is
/// what stands there.
fn open_no_follow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// Writes the already-open settings source into `<home>/.claude/settings.json`.
///
/// Both the directory and the file are de-symlinked. `home` is bind-mounted into
/// the sandbox read-write, so the agent can replace `.claude` with a link out of
/// the workspace between one `agent.start` and the next; `create_dir_all` would
/// follow it, and what this writes is *the agent's own bytes* — Claude Code
/// settings carry `hooks`, which are shell commands, so following that link is
/// code execution as the daemon user next time they run `claude` themselves.
fn write_settings(
    src: &mut std::fs::File,
    home: &std::path::Path,
    dir: &std::path::Path,
    to: &std::path::Path,
) -> std::io::Result<()> {
    super::credentials::ensure_real_dir(home)?;
    super::credentials::ensure_real_dir(dir)?;
    // Unlinked rather than written through, and then created with `create_new`,
    // so a link raced back into the destination between the two is refused
    // instead of followed.
    super::credentials::remove_any(to)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(src, &mut dst)?;
    Ok(())
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

/// The programs whose `--version` has already answered.
///
/// The probe is worth running once per program, not once per agent: it costs a
/// process start, and what it learns cannot change while the file does not. A
/// probe that *failed* is deliberately not in here — a half-installed CLI that
/// the user then repairs must be usable without restarting the daemon, which is
/// exactly what a `OnceCell` filled with the failure prevented.
static PROBED: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());

/// Runs `claude --version` before an agent is started, and warns when the
/// installed CLI is not the version this adapter was verified against.
///
/// Bounded, because this runs on the `agent.start` path: a CLI that never
/// answers — a binary on a filesystem that has gone away, one waiting on a
/// terminal that is not there — used to hang the request for ever with nothing
/// to show the user. Ten seconds is far longer than the real CLI takes and short
/// enough that the IDE can say what happened.
///
/// The version *comparison* is skipped when `BS_CLAUDE_BIN` is set: that hook
/// points at a stand-in whose version number says nothing about the protocol.
/// The probe itself still runs, because "does this program answer at all" is the
/// question, and the answer has to be about the program that will be spawned.
pub async fn probe_claude(backend: &str) -> Result<(), RpcError> {
    let bin = claude_bin(backend)?;
    if PROBED.lock().iter().any(|seen| seen == &bin) {
        return Ok(());
    }
    let mut cmd = tokio::process::Command::new(&bin[0]);
    cmd.args(&bin[1..])
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The timeout drops the future, which drops the child: without this a
        // probe that gave up would leave the hung process behind.
        .kill_on_drop(true);
    let out = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(out) => out,
        Err(_) => {
            warn!(bin = ?bin, "`claude --version` did not answer; giving up on this start");
            return Err(RpcError::new(
                ErrorCode::PrereqMissing,
                format!(
                    "{} did not answer `--version` within {} seconds; the install may be broken",
                    bin.join(" "),
                    PROBE_TIMEOUT.as_secs()
                ),
            )
            .with_data(serde_json::json!({ "reason": REASON_PROBE_TIMEOUT })));
        }
    };
    match out {
        Ok(out) => {
            let version = String::from_utf8_lossy(&out.stdout);
            let version = version.trim();
            if std::env::var_os("BS_CLAUDE_BIN").is_none()
                && !version.starts_with(TESTED_CLAUDE_VERSION)
            {
                warn!(
                    found = version,
                    tested = TESTED_CLAUDE_VERSION,
                    "claude version differs from the one this adapter was tested against"
                );
            }
            // The program answered, so there is nothing to learn by asking
            // again. A non-zero status counts: a stand-in that does not
            // understand `--version` still runs.
            PROBED.lock().push(bin);
        }
        // Not fatal here, and not remembered: the spawn that follows fails with
        // the real reason, which names the program and what the operating system
        // said about it.
        Err(e) => warn!(bin = ?bin, error = %e, "could not run `claude --version`"),
    }
    Ok(())
}

fn agent_error(msg: impl Into<String>) -> RpcError {
    RpcError::new(ErrorCode::AgentError, msg)
}

/// An `AgentError` a client can act on without reading the message.
fn agent_error_because(reason: &str, msg: impl Into<String>) -> RpcError {
    agent_error(msg).with_data(serde_json::json!({ "reason": reason }))
}

/// The last few stderr lines, kept so an exit can say why.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// Whether a line of the agent's output means the CLI has picked the
/// conversation back up, and the reader should say so.
///
/// This is the other half of "only the reader publishes state": with `send` and
/// `permission_reply` no longer announcing a `Working` they cannot know to be
/// true, the first thing the CLI says after being written to is what tells
/// everyone it is going again.
///
/// A line that carries a state of its own says it better -- `init` opens a turn,
/// a `result` ends one, a `can_use_tool` request stops it -- so those are left
/// alone. And a permission answered while a second question is still outstanding
/// is not a resumption at all: the CLI is still waiting.
fn resumed(state: AgentState, nothing_pending: bool, items: &[Parsed]) -> bool {
    if items.iter().any(|i| matches!(i, Parsed::State(..)))
        || !items.iter().any(|i| matches!(i, Parsed::Message(_)))
    {
        return false;
    }
    match state {
        AgentState::Idle | AgentState::Error => true,
        AgentState::WaitingPermission => nothing_pending,
        // Already working, or gone: a line from a process that has been
        // announced as exited must not bring it back.
        AgentState::Working | AgentState::Exited => false,
    }
}

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
    /// Resolves when the stderr reader has seen end of input, so both of the
    /// paths that build an exit detail can wait for the agent's last words
    /// instead of racing them.
    stderr_done: Shared<futures::future::BoxFuture<'static, ()>>,
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

    /// Whether the CLI can be given a new turn at all.
    ///
    /// Both refusals are about a process that will not read the line: one that
    /// is blocked on a permission question reads nothing until it is answered,
    /// and one that has exited reads nothing ever again. Written to anyway, the
    /// first turn vanishes without a trace and the second comes back as a
    /// broken pipe, which tells a client nothing it can act on.
    fn ready_for_a_turn(&self) -> Result<(), RpcError> {
        match self.sink.entry().state().0 {
            AgentState::WaitingPermission => Err(agent_error_because(
                REASON_WAITING_PERMISSION,
                "the agent is waiting for a permission answer; answer it before sending a turn",
            )),
            AgentState::Exited => Err(agent_error_because(
                REASON_AGENT_EXITED,
                "the agent has ended; start a new one with resume_session to continue it",
            )),
            _ => Ok(()),
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
        let (finished, finished_rx) = tokio::sync::oneshot::channel::<()>();
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
            // Dropped rather than sent on an abort, which the waiters read as
            // "finished" too — they are bounded anyway.
            let _ = finished.send(());
        });
        let stderr_done: Shared<futures::future::BoxFuture<'static, ()>> = async move {
            let _ = finished_rx.await;
        }
        .boxed()
        .shared();

        // stdout: the protocol.
        let sink = self.sink.clone();
        let pending = self.pending_request_ids.clone();
        let announced = self.exit_announced.clone();
        let tail = self.stderr_tail.clone();
        let exit_for_reader = exit.clone();
        let stderr_for_reader = stderr_done.clone();
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
                let items = parse_line(&line);
                // State belongs to this reader and to nothing else, so this is
                // where `Working` comes from for a turn nobody announced: the
                // prompt a client wrote to stdin, or the permission answer that
                // released the CLI. Before the line's own messages, so the
                // transcript entry and the state a client reads beside it
                // agree.
                if resumed(sink.entry().state().0, pending.lock().is_empty(), &items) {
                    sink.state(AgentState::Working, None).await;
                }
                for item in items {
                    match item {
                        Parsed::Message(body) => {
                            if let AgentMessageBody::PermissionRequest { request_id, .. } = &body {
                                pending.lock().insert(request_id.clone());
                            }
                            sink.message(body).await;
                        }
                        Parsed::State(state, detail) => sink.state(state, detail).await,
                        Parsed::SessionId(id) => sink.session_id(id),
                        Parsed::Nothing => {}
                    }
                }
            }
            // stdout is closed, so the process is on its way out.
            let code = exit_for_reader.await;
            // Before the detail is built out of it: the stderr tail is read by
            // a task of its own, and the agent's last words are still on their
            // way when the exit code lands -- the CLI hands its stderr to every
            // tool it runs, so something that outlives it by a moment is the
            // ordinary case. Without this wait the exit says "exit code 1" for
            // an agent that spent its last breath saying "Invalid API key".
            // Bounded for the reason `STDERR_DRAIN` gives: an exit nobody
            // announces is worse than one that cannot say why.
            let _ = tokio::time::timeout(STDERR_DRAIN, stderr_for_reader).await;
            // The process is gone whichever way the state went, so the record
            // is closed here rather than only on the `Exited` announcement: an
            // agent that died mid-turn stays in `Error` and never announces one.
            sink.ended();
            // An error result already said why the turn failed, and that is the
            // more useful of the two messages -- so it becomes the exit's
            // detail rather than replacing the exit. An agent left in `Error`
            // is one the IDE shows as a live tab for ever, and one whose next
            // turn comes back as a broken pipe instead of "this agent ended".
            let detail = match sink.entry().state() {
                (AgentState::Error, Some(why)) => why,
                _ => ClaudeAdapter::exit_detail(code, &tail),
            };
            // `stop` may be ending this same process; the flag makes one of the
            // two announce and the other stay quiet, so a process that exits on
            // its own and is then stopped still produces one `Exited`.
            if !announced.swap(true, Ordering::SeqCst) {
                sink.state(AgentState::Exited, Some(detail)).await;
            }
        });

        self.child = Some(Running {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            signal: Arc::new(child.signal),
            exit,
            reader_task,
            stderr_task,
            stderr_done,
        });
        Ok(())
    }

    /// The user's turn is recorded before it is sent, so it is in the
    /// transcript even if the write fails — but only once the agent is in a
    /// state that can take one at all.
    ///
    /// No state is published here. What the agent is doing is what its own
    /// output last said, and the reader is what says it: a `Working` written
    /// from this side is a guess about a process this code has not heard from.
    async fn send(&mut self, text: String) -> Result<(), RpcError> {
        // The state is asked first, so an agent that has already been stopped
        // answers with the reason a client can act on rather than with the
        // bare "not running" that having no process left would give.
        self.ready_for_a_turn()?;
        self.running()?;
        self.sink
            .message(AgentMessageBody::UserText { text: text.clone() })
            .await;
        self.write_line(user_line(&text)).await
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
        // And no state: the CLI may have a second question outstanding -- one
        // assistant message can propose two tools -- and publishing `Working`
        // here takes the bar down over a question nobody has answered, which
        // leaves the CLI waiting for an answer that can no longer be given.
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
        // The same wait the reader makes, for the same reason: this path builds
        // an exit detail too, and the tail is what makes it worth reading.
        let _ = tokio::time::timeout(STDERR_DRAIN, running.stderr_done.clone()).await;
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

    /// A worktree with `bondsymphonic.toml` holding `body`, and a home beside
    /// it. Answers `(worktree, home)`.
    fn worktree_with(dir: &std::path::Path, body: Option<&str>) -> (PathBuf, PathBuf) {
        let worktree = dir.join("worktree");
        let home = dir.join("home");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        if let Some(body) = body {
            std::fs::write(worktree.join("bondsymphonic.toml"), body).unwrap();
        }
        (worktree, home)
    }

    fn settings_of(home: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(home.join(".claude").join("settings.json")).ok()
    }

    /// A file symlink at `link`, or `false` where the platform refuses to make
    /// one (Windows without the symlink privilege).
    fn symlink_file(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    /// A directory symlink at `link`, or `false` where the platform refuses.
    /// Unix, which is where the sandbox and therefore the attack live, always
    /// makes it.
    fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    /// A repo that says nothing about Claude leaves the home exactly as the
    /// credential seeding left it.
    #[test]
    fn without_a_claude_section_nothing_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let bodies = [
            None,
            Some("[[run]]\nname = \"web\"\ncommand = \"x\"\nport = 1\n"),
        ];
        for (i, body) in bodies.into_iter().enumerate() {
            let (worktree, home) = worktree_with(&dir.path().join(format!("case{i}")), body);
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::write(home.join(".claude").join("settings.json"), "the user's").unwrap();
            apply_repo_settings(&worktree, &home).unwrap();
            assert_eq!(settings_of(&home).as_deref(), Some("the user's"));
        }
    }

    /// The repo's file wins over the daemon user's copy, which is the whole
    /// point: a repository pins the tools its agents may use.
    #[test]
    fn the_repos_settings_replace_the_users_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(
            dir.path(),
            Some("[claude]\nsettings = \"config/claude.json\"\n"),
        );
        std::fs::create_dir_all(worktree.join("config")).unwrap();
        std::fs::write(worktree.join("config").join("claude.json"), "the repo's").unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude").join("settings.json"), "the user's").unwrap();

        apply_repo_settings(&worktree, &home).unwrap();
        assert_eq!(settings_of(&home).as_deref(), Some("the repo's"));

        // And again, with the repo's file changed: every start re-applies it,
        // so an agent never runs under a settings file the branch has moved on
        // from.
        std::fs::write(worktree.join("config").join("claude.json"), "changed").unwrap();
        apply_repo_settings(&worktree, &home).unwrap();
        assert_eq!(settings_of(&home).as_deref(), Some("changed"));
    }

    /// The home does not have to exist yet: the first agent in a workspace whose
    /// user has never logged in has no `.claude` directory to write into.
    #[test]
    fn the_claude_directory_is_created_when_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(dir.path(), Some("[claude]\nsettings = \"s.json\"\n"));
        std::fs::write(worktree.join("s.json"), "{}").unwrap();
        apply_repo_settings(&worktree, &home).unwrap();
        assert_eq!(settings_of(&home).as_deref(), Some("{}"));
    }

    /// C1. The agent owns `$HOME` inside the sandbox: it can `rm -rf ~/.claude`
    /// and leave a symlink to the daemon user's own `~/.claude` in its place,
    /// then write a `bondsymphonic.toml` naming settings full of `hooks`. The
    /// next `agent.start` in that workspace used to follow the link and drop the
    /// agent's shell commands into the file the user's own `claude` reads.
    ///
    /// Nothing outside the workspace home may be read, written or removed.
    #[test]
    fn a_symlinked_claude_dir_does_not_carry_the_settings_out_of_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(
            dir.path(),
            Some(
                "[claude]
settings = \"evil.json\"
",
            ),
        );
        std::fs::write(worktree.join("evil.json"), "{\"hooks\":\"rm -rf /\"}").unwrap();

        // The daemon user's real home, outside the workspace, with the file the
        // attack is aimed at.
        let outside = dir.path().join("the-users-real-home").join(".claude");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("settings.json"), "the user's own settings").unwrap();

        if !symlink_dir(&outside, &home.join(".claude")) {
            eprintln!("SKIP: this host will not create directory symlinks");
            return;
        }

        apply_repo_settings(&worktree, &home).unwrap();

        assert_eq!(
            std::fs::read_to_string(outside.join("settings.json")).unwrap(),
            "the user's own settings",
            "the file outside the workspace home must be exactly as it was"
        );
        assert!(
            std::fs::symlink_metadata(home.join(".claude"))
                .unwrap()
                .is_dir(),
            "the link must have been replaced by a real directory"
        );
        assert_eq!(
            settings_of(&home).as_deref(),
            Some("{\"hooks\":\"rm -rf /\"}"),
            "and the repo's settings must have landed inside the home"
        );
    }

    /// The same guard without needing the symlink privilege, so it runs on
    /// Windows as well: anything that is not a real directory is replaced.
    #[test]
    fn a_file_where_the_claude_dir_goes_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(
            dir.path(),
            Some(
                "[claude]
settings = \"s.json\"
",
            ),
        );
        std::fs::write(worktree.join("s.json"), "{}").unwrap();
        std::fs::write(home.join(".claude"), "in the way").unwrap();

        apply_repo_settings(&worktree, &home).unwrap();
        assert!(home.join(".claude").is_dir());
        assert_eq!(settings_of(&home).as_deref(), Some("{}"));
    }

    /// A symlink left at the destination *file* is unlinked, not written
    /// through — the same rule one level down.
    #[test]
    fn a_symlinked_settings_file_is_unlinked_rather_than_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(
            dir.path(),
            Some(
                "[claude]
settings = \"s.json\"
",
            ),
        );
        std::fs::write(worktree.join("s.json"), "the repo's").unwrap();
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, "the user's").unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        if !symlink_file(&outside, &home.join(".claude").join("settings.json")) {
            eprintln!("SKIP: this host will not create file symlinks");
            return;
        }

        apply_repo_settings(&worktree, &home).unwrap();
        assert_eq!(settings_of(&home).as_deref(), Some("the repo's"));
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the user's",
            "the link's target must be untouched"
        );
    }

    /// I2. The source is opened once, and that open refuses a symlink.
    ///
    /// `fs::resolve` canonicalises, so the path it hands back has already had
    /// every link on it followed and checked for containment. The hole was that
    /// the *value* it returned was then re-opened twice more — `is_file()` and
    /// `fs::copy` — and an agent that turns that canonical name into a link to,
    /// say, `~/.ssh/id_ed25519` in between gets the link followed. One
    /// `O_NOFOLLOW` open is what closes it; the race itself cannot be staged
    /// deterministically, so this pins the mechanism.
    #[cfg(unix)]
    #[test]
    fn the_settings_source_is_opened_without_following_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        std::fs::write(&real, "{}").unwrap();
        assert!(open_no_follow(&real).is_ok(), "a regular file opens");

        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = open_no_follow(&link).unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(libc::ELOOP),
            "a symlink must be refused by the open itself: {e}"
        );
    }

    /// Every way the setting can be wrong is an `InvalidParams` on the start,
    /// not a warning: an agent must never run under settings nobody chose.
    #[test]
    fn a_settings_path_that_is_not_inside_the_worktree_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("outside.json"), "not yours").unwrap();
        let absolute = dir
            .path()
            .join("outside.json")
            .display()
            .to_string()
            .replace('\\', "/");
        let cases = [
            ("../outside.json", "climbing out"),
            (absolute.as_str(), "an absolute path"),
            ("config/absent.json", "a file that is not there"),
        ];
        for (rel, what) in cases {
            let (worktree, home) = worktree_with(
                &dir.path().join(what.replace(' ', "-")),
                Some(&format!("[claude]\nsettings = \"{rel}\"\n")),
            );
            let e = apply_repo_settings(&worktree, &home).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams, "{what}: {e:?}");
            assert!(
                e.message.contains("settings"),
                "{what}: the message must name the setting: {}",
                e.message
            );
            assert!(
                settings_of(&home).is_none(),
                "{what}: nothing must be written"
            );
        }
    }

    /// A config file that will not parse may be the one naming the settings, and
    /// there is no way to tell, so the agent does not start.
    #[test]
    fn an_unparseable_config_stops_the_start_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let (worktree, home) = worktree_with(dir.path(), Some("[claude]\nsettings = \n"));
        let e = apply_repo_settings(&worktree, &home).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");
        assert!(
            e.message.contains("bondsymphonic.toml"),
            "the message must name the file: {}",
            e.message
        );
    }

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
