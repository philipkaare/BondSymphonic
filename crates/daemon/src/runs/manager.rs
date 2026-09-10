//! The live runs: starting a workspace's web app inside its sandbox, telling
//! the IDE when it is ready and where to reach it, streaming its output, and
//! ending it.
//!
//! A run is one process started from a [`RunConfig`](bondsymphonic_proto::RunConfig)
//! — the repo's `bondsymphonic.toml`, or detection — inside the workspace's
//! sandbox, plus everything needed to reach and end it:
//!
//! * **A port on the host.** On bubblewrap the sandbox has a network namespace
//!   of its own, so the app's port is invisible from outside and a
//!   [`Bridge`](crate::net::bridge::Bridge) plus an in-sandbox forwarder
//!   ([`crate::net::forward`]) give it one on the host's loopback. On the
//!   no-sandbox backend there is nothing to bridge: the process is a plain
//!   child of the daemon and its port already is the host's.
//! * **Readiness.** A port that accepts is not proof the app is up, so a run
//!   stays `starting` until either an output line matches the config's
//!   `ready_regex` or the port answers — checked every [`PROBE_INTERVAL`],
//!   never at start.
//! * **Output.** Every line of stdout and stderr becomes a `run.output` event.
//!   The daemon keeps only the last [`TAIL`] lines, for the exit detail; the
//!   IDE keeps the log.
//!
//! Nothing here is proxied. The bridge carries raw bytes with no HTTP in them,
//! which is what lets a WebSocket or a dev server's hot-reload channel work
//! through the same port, and the forwarder connects to the sandbox's own
//! loopback, which `NO_PROXY=localhost,127.0.0.1` keeps out of the workspace
//! proxy in any case.

use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::sandbox::{ChildReader, SandboxCommand, SandboxHandle, Signaller};
use crate::server::broadcast::EventBus;
use bondsymphonic_proto::*;
use futures::future::Shared;
use futures::FutureExt;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;
use tracing::warn;

/// How often readiness is checked. The first check happens one interval in, so
/// a command that has not even been exec'd yet is never probed.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The most of one output line that reaches a client. A run is free to print a
/// megabyte with no newline in it; that must cost the daemon a bounded buffer
/// and the IDE a bounded event, so the rest of such a line is dropped.
const MAX_LINE: usize = 8 * 1024;

/// Output lines kept for the exit detail, so a run that dies can say what it
/// last said.
const TAIL: usize = 20;

/// How long `stop` waits after SIGTERM before SIGKILL.
const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a terminal state waits for the readers to finish, and how long
/// `stop` waits for an exit code after SIGKILL. A reader still blocked after
/// this is one whose pipe a grandchild the kill did not reach is holding open,
/// and waiting on it forever would keep the run from ever announcing its end.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// How long the in-sandbox forwarder gets to bind its socket before the command
/// is started anyway. Exceeded, the run still starts and simply may need an
/// extra probe or two to look ready.
#[cfg(unix)]
const FORWARDER_READY: std::time::Duration = std::time::Duration::from_secs(5);

/// SIGTERM and SIGKILL as plain numbers. `libc`, and signals at all, exist only
/// on Unix, while [`Signaller`] is cross-platform and the Windows
/// implementation recognises exactly these two, so spelling them out keeps this
/// one code path instead of two.
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// The backend whose sandbox has a network namespace of its own, and therefore
/// needs the bridge. Compared by name so this module is not conditional on the
/// target OS.
const BWRAP_BACKEND: &str = "linux_bwrap";

type Tail = Arc<Mutex<VecDeque<String>>>;
type Runs = Arc<Mutex<HashMap<RunId, Arc<Run>>>>;

/// One live run.
///
/// Every `Mutex` here is held for the moment it takes to read or replace what
/// it holds; nothing awaits while one is locked.
struct Run {
    id: RunId,
    workspace_id: WorkspaceId,
    config_name: String,
    /// The direct child, which on Windows is the shell rather than the server.
    /// See [`kill_tree`].
    pid: u32,
    host_port: u16,
    url: String,
    /// Insertion order, so `list` reports runs in the order they were started.
    ordinal: u64,
    state: Mutex<RunState>,
    signal: Arc<Signaller>,
    /// The exit code, awaitable more than once.
    exit: Shared<futures::future::BoxFuture<'static, i32>>,
    tail: Tail,
    /// Claimed by whichever of the supervisor and `stop` gets there first, so
    /// one run publishes exactly one terminal state.
    finished: Arc<AtomicBool>,
    /// The stdout and stderr readers, drained then abandoned by whichever path
    /// ends the run.
    readers: Mutex<Vec<JoinHandle<()>>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Everything the run holds outside its own process: the host bridge and
    /// the forwarder inside the sandbox. Taken by whoever tears the run down.
    plumbing: Mutex<Option<Plumbing>>,
}

/// The bridge and the forwarder, which exist together or not at all.
struct Plumbing {
    #[cfg(unix)]
    bridge: Option<crate::net::bridge::Bridge>,
    /// Ends the forwarder inside the sandbox. Init takes it down with the
    /// sandbox in any case; this is for a run that stops while the workspace
    /// lives on.
    forwarder: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Plumbing {
    /// What a run with nothing between it and the host holds: nothing.
    fn none() -> Self {
        Self {
            #[cfg(unix)]
            bridge: None,
            forwarder: None,
        }
    }
}

impl Run {
    fn info(&self) -> RunInfo {
        RunInfo {
            run_id: self.id.clone(),
            config_name: self.config_name.clone(),
            state: *self.state.lock(),
            host_port: self.host_port,
            url: self.url.clone(),
        }
    }
}

/// Every run the daemon has started, and the requests that reach them.
///
/// The manager owns the list, not the workspace registry: a run is a live
/// process and the registry is a file that survives restarts.
/// [`runs_of`](RunManager::runs_of) is what puts them back into
/// [`WorkspaceInfo`].
pub struct RunManager {
    events: EventBus,
    runs: Runs,
    /// `(workspace, config)` pairs whose run is being started right now, so two
    /// `run.start` calls that arrive together cannot both pass the conflict
    /// check while neither is in `runs` yet.
    claims: Arc<Mutex<HashSet<(WorkspaceId, String)>>>,
    next_ordinal: AtomicU64,
}

/// Holds a `(workspace, config)` claim for as long as a start is in flight, and
/// releases it however that start ends.
struct Claim {
    claims: Arc<Mutex<HashSet<(WorkspaceId, String)>>>,
    key: (WorkspaceId, String),
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.claims.lock().remove(&self.key);
    }
}

impl RunManager {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            runs: Arc::new(Mutex::new(HashMap::new())),
            claims: Arc::new(Mutex::new(HashSet::new())),
            next_ordinal: AtomicU64::new(0),
        }
    }

    /// Starts one of a ready workspace's run configurations inside its sandbox.
    pub async fn start(&self, d: &Daemon, p: RunStartParams) -> Result<RunStartResult, RpcError> {
        let ws = d.workspace(&p.workspace_id)?;
        if ws.state != WorkspaceState::Ready {
            return Err(RpcError::invalid_params(format!(
                "workspace {} is not ready",
                ws.id
            )));
        }
        // The worktree's own copy of `bondsymphonic.toml`, not the source
        // repo's: the workspace is a checkout of a branch of its own, and a run
        // config an agent added there is the one the user is looking at.
        let worktree = ws.worktree_path.clone();
        let configs =
            tokio::task::spawn_blocking(move || crate::runs::config::configs_for(&worktree))
                .await
                .map_err(|e| RpcError::internal(e.to_string()))?;
        let config = configs
            .into_iter()
            .find(|c| c.name == p.config_name)
            .ok_or_else(|| {
                RpcError::not_found(format!(
                    "run config {} in workspace {}",
                    p.config_name, ws.id
                ))
            })?;
        if let Some(reason) = config.disabled_reason.as_deref() {
            return Err(RpcError::invalid_params(format!(
                "run config {} cannot be started: {reason}",
                config.name
            )));
        }
        // Compiled before anything is spawned, so a typo in the repo's config is
        // an error on the call rather than a run that can never become ready.
        let ready_regex = match config.ready_regex.as_deref() {
            Some(pattern) => Some(regex::Regex::new(pattern).map_err(|e| {
                RpcError::invalid_params(format!(
                    "run config {}: ready_regex {pattern:?} does not compile: {e}",
                    config.name
                ))
            })?),
            None => None,
        };

        let _claim = self.claim(&ws.id, &config.name)?;
        let handle = d.sandbox(&ws.id)?;

        // On a sandbox with a network namespace of its own the app's port is
        // unreachable from here, so it gets a host port and a forwarder; on the
        // no-sandbox backend the process is a plain child of the daemon and its
        // port already is the host's.
        let (host_port, plumbing, readiness_source) = if d.backend.name() == BWRAP_BACKEND {
            bridge_into(d, &ws.id, &handle, config.port).await?
        } else {
            (config.port, Plumbing::none(), ReadinessSource::Port)
        };
        let url = format!("http://localhost:{host_port}");

        let mut env: Vec<(String, String)> = config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // After the config's own entries, so a repo cannot quietly redefine the
        // two variables the daemon promises every run.
        env.push(("PORT".into(), config.port.to_string()));
        env.push(("HOST".into(), "0.0.0.0".into()));
        let cwd = ws.worktree_path.join(config.cwd.as_deref().unwrap_or("."));

        let mut child = match handle
            .spawn(SandboxCommand {
                argv: shell_argv(&config.command),
                env,
                cwd: Some(cwd),
                pty: None,
            })
            .await
        {
            Ok(c) => c,
            Err(e) => {
                teardown(plumbing);
                return Err(e);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            // Nothing can watch this process, so it must not be left running.
            (child.killer)();
            teardown(plumbing);
            return Err(RpcError::internal("backend returned no pipes for the run"));
        };

        let id: RunId = new_id(RunId::PREFIX).as_str().into();
        let exit_rx = child.exit;
        let exit: Shared<futures::future::BoxFuture<'static, i32>> =
            async move { exit_rx.await.unwrap_or(-1) }.boxed().shared();
        let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
        let regex_hit = Arc::new(AtomicBool::new(false));
        let run = Arc::new(Run {
            id: id.clone(),
            workspace_id: ws.id.clone(),
            config_name: config.name.clone(),
            pid: child.pid,
            host_port,
            url: url.clone(),
            ordinal: self.next_ordinal.fetch_add(1, Ordering::SeqCst),
            state: Mutex::new(RunState::Starting),
            signal: Arc::new(child.signal),
            exit,
            tail: tail.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            readers: Mutex::new(Vec::new()),
            supervisor: Mutex::new(None),
            plumbing: Mutex::new(Some(plumbing)),
        });

        let readers = vec![
            self.reader(
                &run,
                stdout,
                tail.clone(),
                ready_regex.clone(),
                regex_hit.clone(),
            ),
            self.reader(&run, stderr, tail, ready_regex.clone(), regex_hit.clone()),
        ];
        run.readers.lock().extend(readers);

        // A configured `ready_regex` is the *only* thing that makes such a run
        // ready: a repo that says "wait for this line" is saying the port alone
        // is not good enough.
        let readiness = match ready_regex {
            Some(_) => ReadinessSource::Regex(regex_hit),
            None => readiness_source,
        };

        // Registered before the first event, so a client that reacts to
        // `starting` by calling `run.list` always finds the run there.
        self.runs.lock().insert(id.clone(), run.clone());
        publish(&self.events, &run, RunState::Starting, None, None);

        let supervisor = tokio::spawn(supervise(
            run.clone(),
            self.runs.clone(),
            self.events.clone(),
            readiness,
        ));
        *run.supervisor.lock() = Some(supervisor);

        Ok(RunStartResult {
            run_id: id,
            host_port,
            url,
        })
    }

    /// Ends a run: SIGTERM to its process group, SIGKILL after [`TERM_GRACE`],
    /// then the bridge and the forwarder.
    ///
    /// Idempotent. A run that has already ended — stopped by an earlier call,
    /// or gone on its own a moment before the user clicked Stop — is not an
    /// error: `stop` is a teardown verb, and the client has already been told
    /// the run is over by its terminal `run.state` event.
    pub async fn stop(&self, id: &RunId) -> Result<Empty, RpcError> {
        let Some(run) = self.runs.lock().remove(id) else {
            return Ok(Empty {});
        };
        stop_run(&run, &self.events).await;
        Ok(Empty {})
    }

    /// The runs belonging to `ws`, oldest first.
    pub fn list(&self, ws: &WorkspaceId) -> Vec<RunInfo> {
        let mut found: Vec<(u64, RunInfo)> = self
            .runs
            .lock()
            .values()
            .filter(|r| &r.workspace_id == ws)
            .map(|r| (r.ordinal, r.info()))
            .collect();
        found.sort_by_key(|(ordinal, _)| *ordinal);
        found.into_iter().map(|(_, info)| info).collect()
    }

    /// The run ids belonging to `ws`, oldest first. What fills
    /// [`WorkspaceInfo::runs`].
    pub fn runs_of(&self, ws: &WorkspaceId) -> Vec<RunId> {
        self.list(ws).into_iter().map(|r| r.run_id).collect()
    }

    /// Ends every run in `ws`. Called before the workspace's sandbox is torn
    /// down, so each run ends through its own stop path — signalled, unbridged,
    /// `stopped` announced — instead of vanishing with the sandbox and leaving
    /// a client showing a run that no longer exists.
    pub async fn stop_all_in(&self, ws: &WorkspaceId) {
        let victims: Vec<Arc<Run>> = {
            let mut runs = self.runs.lock();
            let ids: Vec<RunId> = runs
                .values()
                .filter(|r| &r.workspace_id == ws)
                .map(|r| r.id.clone())
                .collect();
            ids.into_iter().filter_map(|id| runs.remove(&id)).collect()
        };
        // Together rather than one after another: each one may wait out the
        // whole termination grace.
        futures::future::join_all(victims.iter().map(|run| stop_run(run, &self.events))).await;
    }

    /// Reserves `(workspace, config)` for the duration of a start, refusing a
    /// second run of the same configuration in the same workspace.
    fn claim(&self, ws: &WorkspaceId, config: &str) -> Result<Claim, RpcError> {
        let key = (ws.clone(), config.to_string());
        // One lock over both checks: a run that is already registered and one
        // that is still starting are the same conflict.
        let mut claims = self.claims.lock();
        let running = self
            .runs
            .lock()
            .values()
            .any(|r| r.workspace_id == *ws && r.config_name == config);
        if running || !claims.insert(key.clone()) {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                format!("run config {config} is already running in workspace {ws}"),
            ));
        }
        Ok(Claim {
            claims: self.claims.clone(),
            key,
        })
    }

    /// One reader task: lines out of a pipe, into `run.output` and the tail.
    fn reader(
        &self,
        run: &Arc<Run>,
        pipe: ChildReader,
        tail: Tail,
        ready_regex: Option<regex::Regex>,
        regex_hit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let events = self.events.clone();
        let run_id = run.id.clone();
        let ws = run.workspace_id.clone();
        tokio::spawn(async move {
            let mut lines = Lines::new(pipe);
            while let Some(line) = lines.next().await {
                if let Some(re) = &ready_regex {
                    if re.is_match(&line) {
                        regex_hit.store(true, Ordering::SeqCst);
                    }
                }
                {
                    let mut tail = tail.lock();
                    if tail.len() == TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
                events.publish(
                    Some(ws.clone()),
                    Event::RunOutput {
                        run_id: run_id.clone(),
                        line,
                    },
                );
            }
        })
    }
}

/// Moves a run's state and announces it. The run's own record is updated first,
/// so a client that reacts by calling `run.list` never sees the older value.
fn publish(
    events: &EventBus,
    run: &Run,
    state: RunState,
    url: Option<String>,
    detail: Option<String>,
) {
    *run.state.lock() = state;
    events.publish(
        Some(run.workspace_id.clone()),
        Event::RunStateChanged {
            run_id: run.id.clone(),
            state,
            url,
            detail,
        },
    );
}

/// Where a run's readiness comes from.
enum ReadinessSource {
    /// A line matched the config's `ready_regex`; the readers set the flag.
    Regex(Arc<AtomicBool>),
    /// The service answers on the host's own loopback, which is where the
    /// no-sandbox backend leaves it.
    Port,
    /// The forwarder at this socket says the service inside the sandbox
    /// accepts.
    #[cfg(unix)]
    Bridge(std::path::PathBuf),
}

/// Watches one run from `starting` to its terminal state.
///
/// Readiness is polled rather than pushed because both sources are polled
/// things: a port either answers now or it does not, and a regex hit is a flag
/// the readers set as output arrives.
async fn supervise(run: Arc<Run>, runs: Runs, events: EventBus, readiness: ReadinessSource) {
    let mut exit = run.exit.clone();
    loop {
        tokio::select! {
            code = &mut exit => {
                // Gone before it ever answered: that is a failure, and the exit
                // code with the last of its output is the only explanation
                // anyone gets.
                finish(
                    &run,
                    &runs,
                    &events,
                    RunState::Failed,
                    Some(exit_detail(code, &run.tail)),
                )
                .await;
                return;
            }
            _ = tokio::time::sleep(PROBE_INTERVAL) => {
                if is_ready(&run, &readiness).await {
                    break;
                }
            }
        }
    }
    publish(&events, &run, RunState::Ready, Some(run.url.clone()), None);
    // Ready, so whatever ends it from here is an ending rather than a failure.
    // `stop` claims `finished` before this ever sees the exit, and `finish`
    // then keeps quiet.
    let code = exit.await;
    finish(
        &run,
        &runs,
        &events,
        RunState::Stopped,
        Some(exit_detail(code, &run.tail)),
    )
    .await;
}

/// Publishes a run's terminal state, once, and forgets it.
async fn finish(
    run: &Arc<Run>,
    runs: &Runs,
    events: &EventBus,
    state: RunState,
    detail: Option<String>,
) {
    // What the process wrote before it went is still worth sending, so the
    // readers get a moment to finish ahead of the terminal state.
    drain_readers(run).await;
    // `stop` may be ending this same run; the flag makes one of the two
    // announce and the other stay quiet.
    if run.finished.swap(true, Ordering::SeqCst) {
        return;
    }
    runs.lock().remove(&run.id);
    teardown_run(run);
    publish(events, run, state, None, detail);
}

/// Lets the reader tasks finish, then abandons whichever did not.
async fn drain_readers(run: &Arc<Run>) {
    let mut readers: Vec<JoinHandle<()>> = std::mem::take(&mut *run.readers.lock());
    if readers.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(DRAIN, futures::future::join_all(readers.iter_mut())).await;
    for r in readers {
        r.abort();
    }
}

/// Takes the bridge and the forwarder down, once.
fn teardown_run(run: &Run) {
    if let Some(plumbing) = run.plumbing.lock().take() {
        teardown(plumbing);
    }
}

fn teardown(plumbing: Plumbing) {
    if let Some(kill) = plumbing.forwarder {
        kill();
    }
    #[cfg(unix)]
    if let Some(bridge) = plumbing.bridge {
        bridge.stop();
    }
}

/// Ends one run's process and its plumbing, and announces `stopped` unless the
/// supervisor got there first.
async fn stop_run(run: &Arc<Run>, events: &EventBus) {
    // Before the signal, while the shell is still there to be walked: on
    // Windows the tree is what has to go, and the signal would take the shell
    // out from under it. A no-op everywhere else.
    kill_tree(run).await;
    (run.signal)(SIGTERM);
    let mut exit = run.exit.clone();
    let mut ended = tokio::time::timeout(TERM_GRACE, &mut exit).await.is_ok();
    if !ended {
        warn!(run = %run.id, "run did not stop; killing it");
        (run.signal)(SIGKILL);
        ended = tokio::time::timeout(DRAIN, &mut exit).await.is_ok();
    }
    if !ended {
        warn!(run = %run.id, pid = run.pid, "run did not report an exit code");
    }
    drain_readers(run).await;
    teardown_run(run);
    if let Some(supervisor) = run.supervisor.lock().take() {
        supervisor.abort();
    }
    if !run.finished.swap(true, Ordering::SeqCst) {
        // No detail: a run the user stopped needs no explanation, and the exit
        // status of a process that was signalled says nothing useful.
        publish(events, run, RunState::Stopped, None, None);
    }
}

/// Ends the whole process tree on Windows, where there are no process groups.
///
/// The no-sandbox backend terminates only the direct child there, which for a
/// run is `cmd /C <command>` — the server itself is a grandchild, and it would
/// go on holding the port after the run claimed to have stopped. `taskkill /T`
/// is no less abrupt than what the backend already does on that platform, which
/// has no gentle termination to offer in the first place.
///
/// On Unix the backend puts every child in a process group of its own and one
/// signal reaches the whole tree, so there is nothing to do.
async fn kill_tree(run: &Run) {
    #[cfg(windows)]
    {
        if run.pid == 0 {
            return;
        }
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &run.pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(windows))]
    let _ = run;
}

async fn is_ready(run: &Arc<Run>, readiness: &ReadinessSource) -> bool {
    match readiness {
        ReadinessSource::Regex(hit) => hit.load(Ordering::SeqCst),
        ReadinessSource::Port => {
            // No sandbox between here and the process: the port it bound is the
            // host's. Connect and close, with nothing sent, so probing a server
            // that logs its requests does not fill the run's output with them.
            tokio::time::timeout(
                PROBE_INTERVAL,
                tokio::net::TcpStream::connect(("127.0.0.1", run.host_port)),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        }
        #[cfg(unix)]
        ReadinessSource::Bridge(socket) => crate::net::bridge::probe(socket).await,
    }
}

/// Starts the host bridge and the in-sandbox forwarder for `port`.
#[cfg(unix)]
async fn bridge_into(
    d: &Daemon,
    ws: &WorkspaceId,
    handle: &Arc<dyn SandboxHandle>,
    port: u16,
) -> Result<(u16, Plumbing, ReadinessSource), RpcError> {
    let socket = d.dirs.run(ws).join(format!("fwd-{port}.sock"));
    // A file left by a run that did not shut down cleanly would make the
    // forwarder's bind fail inside the sandbox, where nothing can say so.
    let _ = std::fs::remove_file(&socket);
    let bridge = crate::net::bridge::Bridge::start(socket.clone())
        .await
        .map_err(|e| RpcError::io(&e))?;
    let host_port = bridge.host_port;
    let forwarder = start_forwarder(ws, handle, port).await;
    Ok((
        host_port,
        Plumbing {
            bridge: Some(bridge),
            forwarder,
        },
        ReadinessSource::Bridge(socket),
    ))
}

/// A host without Unix sockets has no bubblewrap either, so this is never
/// reached; it exists so the manager compiles on Windows.
#[cfg(not(unix))]
async fn bridge_into(
    _d: &Daemon,
    _ws: &WorkspaceId,
    _handle: &Arc<dyn SandboxHandle>,
    _port: u16,
) -> Result<(u16, Plumbing, ReadinessSource), RpcError> {
    Err(RpcError::internal("the port bridge needs unix sockets"))
}

/// Starts the forwarder inside the sandbox and waits for it to bind.
///
/// A failure is logged rather than returned: readiness simply never arrives
/// through a bridge with no far end, which the IDE shows as a run stuck in
/// `starting` rather than as a URL that does not work.
#[cfg(unix)]
async fn start_forwarder(
    ws: &WorkspaceId,
    handle: &Arc<dyn SandboxHandle>,
    port: u16,
) -> Option<Box<dyn Fn() + Send + Sync>> {
    let argv = vec![
        crate::workspace::lifecycle::INIT_EXE_IN_SANDBOX.to_string(),
        "forward".to_string(),
        "--socket".to_string(),
        format!("/run/bs/fwd-{port}.sock"),
        "--port".to_string(),
        port.to_string(),
    ];
    let mut child = match handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(ws = %ws, port, "port forwarder did not start: {e}; this run will not be reachable");
            return None;
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        warn!(ws = %ws, port, "port forwarder started without pipes");
        (child.killer)();
        return None;
    };
    let killer = child.killer;
    let mut exit = child.exit;
    let mut out = tokio::io::BufReader::new(stdout).lines();
    let mut err = tokio::io::BufReader::new(stderr).lines();
    // The forwarder announces its bind, so the first probe after the command
    // starts is not answered by an empty run directory.
    let ready = tokio::time::timeout(FORWARDER_READY, async {
        while let Ok(Some(line)) = out.next_line().await {
            if line.starts_with(crate::net::forward::READY_LINE) {
                return true;
            }
            tracing::debug!(ws = %ws, "forward: {line}");
        }
        false
    })
    .await
    .unwrap_or(false);
    if !ready {
        warn!(ws = %ws, port, "port forwarder never reported listening");
    }
    let id = ws.clone();
    tokio::spawn(async move {
        tokio::join!(
            async {
                while let Ok(Some(line)) = out.next_line().await {
                    tracing::debug!(ws = %id, "forward: {line}");
                }
            },
            async {
                while let Ok(Some(line)) = err.next_line().await {
                    warn!(ws = %id, "forward: {line}");
                }
            }
        );
        let code = (&mut exit).await.ok();
        tracing::debug!(ws = %id, ?code, "port forwarder exited");
    });
    Some(killer)
}

/// `command` as a process, run through a shell so the shell features a config
/// is free to use (`&&`, `--`, quoting) mean what they say.
fn shell_argv(command: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), command.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), command.into()]
    }
}

fn exit_detail(code: i32, tail: &Tail) -> String {
    let lines = tail.lock();
    if lines.is_empty() {
        return format!("exit code {code}");
    }
    format!(
        "exit code {code}\n{}",
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    )
}

/// Lines out of a child's pipe, with a bound on how long one line may be.
///
/// `AsyncBufReadExt::lines` would happily buffer a gigabyte for a process that
/// never writes a newline. This keeps at most [`MAX_LINE`] bytes and throws the
/// rest of an over-long line away, so what a run prints can cost the daemon
/// output bandwidth but never its memory.
struct Lines {
    reader: tokio::io::BufReader<ChildReader>,
    buf: Vec<u8>,
    truncated: bool,
}

impl Lines {
    fn new(pipe: ChildReader) -> Self {
        Self {
            reader: tokio::io::BufReader::new(pipe),
            buf: Vec::new(),
            truncated: false,
        }
    }

    async fn next(&mut self) -> Option<String> {
        loop {
            let (consume, complete) = {
                let available = match self.reader.fill_buf().await {
                    Ok(b) => b,
                    Err(_) => return self.take(),
                };
                if available.is_empty() {
                    return self.take();
                }
                match available.iter().position(|b| *b == b'\n') {
                    Some(i) => {
                        push_capped(&mut self.buf, &available[..i], &mut self.truncated);
                        (i + 1, true)
                    }
                    None => {
                        let n = available.len();
                        push_capped(&mut self.buf, available, &mut self.truncated);
                        (n, false)
                    }
                }
            };
            self.reader.consume(consume);
            if complete {
                return self.take();
            }
        }
    }

    /// The line built so far, or `None` at a clean end of stream.
    fn take(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        if self.truncated {
            tracing::debug!("a run printed a line longer than {MAX_LINE} bytes; it was cut");
            self.truncated = false;
        }
        let bytes = std::mem::take(&mut self.buf);
        // A Windows child writes CRLF, and the carriage return is not part of
        // the line anyone wants to read or match a regex against.
        let end = bytes.len() - usize::from(bytes.last() == Some(&b'\r'));
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

/// Appends what still fits, dropping the rest and saying so.
fn push_capped(buf: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    let room = MAX_LINE.saturating_sub(buf.len());
    if bytes.len() > room {
        *truncated = true;
    }
    buf.extend_from_slice(&bytes[..room.min(bytes.len())]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_runs_through_a_shell() {
        let argv = shell_argv("npm run dev -- --port 3000");
        if cfg!(windows) {
            assert_eq!((argv[0].as_str(), argv[1].as_str()), ("cmd", "/C"));
        } else {
            assert_eq!((argv[0].as_str(), argv[1].as_str()), ("/bin/sh", "-c"));
        }
        assert_eq!(argv[2], "npm run dev -- --port 3000");
    }

    #[tokio::test]
    async fn lines_are_split_stripped_of_carriage_returns_and_capped() {
        let long = "x".repeat(MAX_LINE * 2);
        let input = format!("one\r\ntwo\n{long}\nlast");
        let pipe: ChildReader = Box::pin(std::io::Cursor::new(input.into_bytes()));
        let mut lines = Lines::new(pipe);
        assert_eq!(lines.next().await.as_deref(), Some("one"));
        assert_eq!(lines.next().await.as_deref(), Some("two"));
        let capped = lines.next().await.unwrap();
        assert_eq!(capped.len(), MAX_LINE, "an over-long line is cut, not kept");
        // The rest of that line is dropped rather than becoming one of its own.
        assert_eq!(lines.next().await.as_deref(), Some("last"));
        assert_eq!(lines.next().await, None);
    }

    #[test]
    fn the_exit_detail_carries_the_code_and_the_last_lines() {
        let tail: Tail = Arc::new(Mutex::new(VecDeque::from(vec![
            "starting".to_string(),
            "boom".to_string(),
        ])));
        let detail = exit_detail(3, &tail);
        assert!(detail.starts_with("exit code 3"), "{detail}");
        assert!(detail.contains("boom"), "{detail}");
        assert_eq!(
            exit_detail(1, &Arc::new(Mutex::new(VecDeque::new()))),
            "exit code 1"
        );
    }
}
