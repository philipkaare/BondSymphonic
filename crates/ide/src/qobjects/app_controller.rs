use crate::client::router::EventRouter;
use crate::client::DaemonClient;
use crate::launcher::{self, LaunchSpec};
use crate::model::app_state::{compose_status, ConnectionState};
use crate::qobjects::settings::Settings;
use bondsymphonic_proto::*;
use std::sync::OnceLock;
use std::time::Duration;

/// One process-wide multi-threaded runtime drives the daemon connection. It is
/// intentionally never dropped: the Qt event loop owns the main thread, and the
/// daemon process handle parked on the controller outlives every task here.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio")
    })
}

/// The connection, plus the router fanning its events out. Published process-wide
/// once `start` has connected so that QObjects created later (terminal panes, the
/// file tree) can issue requests and subscribe to events without holding a
/// pointer to the controller.
#[derive(Clone)]
pub struct Shared {
    pub client: DaemonClient,
    pub router: EventRouter,
}

static SHARED: OnceLock<std::sync::Mutex<Option<Shared>>> = OnceLock::new();

fn shared_slot() -> &'static std::sync::Mutex<Option<Shared>> {
    SHARED.get_or_init(|| std::sync::Mutex::new(None))
}

/// The live daemon connection and event router, or `None` before `start` has
/// connected. The returned clone keeps working across reconnects only until the
/// next successful connect replaces it, so callers should fetch it per operation
/// rather than caching it.
pub fn shared() -> Option<Shared> {
    shared_slot()
        .lock()
        .expect("shared handle mutex poisoned")
        .clone()
}

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `status_message` is exposed as `getStatusMessage`/`statusMessageChanged`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qproperty(i32, connection_state)]
        #[qproperty(QString, status_message)]
        #[qproperty(QString, daemon_version)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn prereq_warning(self: Pin<&mut AppController>, message: QString);

        /// The daemon's full workspace list, as a JSON array of `WorkspaceInfo`,
        /// emitted once per successful connect.
        #[qsignal]
        fn workspaces_listed(self: Pin<&mut AppController>, json: QString);

        /// One workspace changed state: a JSON `WorkspaceInfo`.
        #[qsignal]
        fn workspace_changed(self: Pin<&mut AppController>, info_json: QString);

        /// A `create_workspace` call succeeded. `group`, `adapter` and `command`
        /// are echoed back from the call so the UI can place the new tab without
        /// tracking the in-flight request itself.
        #[qsignal]
        fn workspace_created(
            self: Pin<&mut AppController>,
            info_json: QString,
            group: QString,
            adapter: QString,
            command: QString,
        );

        /// A `destroy_workspace` call succeeded, for the workspace with this id.
        #[qsignal]
        fn workspace_destroyed(self: Pin<&mut AppController>, id: QString);

        /// An `inspect_repo` call succeeded: a JSON `RepoInfo` for `path`.
        #[qsignal]
        fn repo_inspected(self: Pin<&mut AppController>, path: QString, info_json: QString);

        /// The daemon dropped `count` events because a consumer fell behind.
        #[qsignal]
        fn output_dropped(self: Pin<&mut AppController>, count: i64);

        /// An asynchronous operation failed. `op` is the daemon method name.
        #[qsignal]
        fn operation_failed(self: Pin<&mut AppController>, op: QString, message: QString);

        /// Launch the daemon inside WSL and connect to it.
        #[qinvokable]
        fn start(self: Pin<&mut AppController>);

        /// Record the daemon version and recompose the status text. Callers use
        /// this rather than `set_daemon_version` so the status bar stays in sync.
        #[qinvokable]
        fn apply_daemon_version(self: Pin<&mut AppController>, version: QString);

        /// Create a workspace. Answers with `workspace_created` or
        /// `operation_failed`; `group`, `adapter` and `command` are not sent to
        /// the daemon, only echoed back to the caller.
        #[qinvokable]
        fn create_workspace(
            self: Pin<&mut AppController>,
            repo_path: QString,
            base_branch: QString,
            name: QString,
            group: QString,
            adapter: QString,
            command: QString,
        );

        /// Destroy a workspace. Answers with `workspace_destroyed` or
        /// `operation_failed`.
        #[qinvokable]
        fn destroy_workspace(self: Pin<&mut AppController>, id: QString, force: bool);

        /// Inspect a git repository. Answers with `repo_inspected` or
        /// `operation_failed`.
        #[qinvokable]
        fn inspect_repo(self: Pin<&mut AppController>, path: QString);

        /// Translates a Windows path to the WSL path the daemon expects, or
        /// returns an empty string if it is not a translatable path.
        #[qinvokable]
        fn wsl_path(self: &AppController, windows_path: QString) -> QString;
    }

    impl cxx_qt::Threading for AppController {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::AppController>;

/// Shared handle to the running daemon so a later "Exit" action can shut it down.
type ProcessHandle = std::sync::Arc<tokio::sync::Mutex<Option<launcher::DaemonProcess>>>;

/// Warn-level daemon logs starting with this are the event-queue overflow
/// notice, which the UI surfaces as a terminal banner rather than a log line.
const DROP_NOTICE_PREFIX: &str = "events dropped: ";

/// Reported when an invokable runs before `start` has connected.
const NOT_CONNECTED: &str = "not connected to the daemon";

pub struct AppControllerRust {
    connection_state: i32,
    status_message: QString,
    daemon_version: QString,
    client: Option<DaemonClient>,
    process: Option<ProcessHandle>,
}

impl Default for AppControllerRust {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected.as_i32(),
            status_message: QString::from(ConnectionState::Disconnected.label()),
            daemon_version: QString::from(""),
            client: None,
            process: None,
        }
    }
}

/// Queues an `operation_failed` for `op` back onto the Qt thread.
fn report_failure(qt: &QtHandle, op: &'static str, message: String) {
    tracing::warn!("{op} failed: {message}");
    let _ = qt.queue(move |q| q.operation_failed(QString::from(op), QString::from(&message)));
}

/// Drains the connection's event stream for the life of the connection. Runs on
/// its own task, started before any request is issued, so nothing the controller
/// asks the daemon for can be starved by events the daemon is already sending.
async fn drain_events(mut events: crate::client::EventStream, router: EventRouter, qt: QtHandle) {
    while let Some((ws, ev)) = events.recv().await {
        // Every consumer sees the event before the controller acts on it, so a
        // terminal's output is never delayed behind UI work.
        router.dispatch(ws, ev.clone());
        match ev {
            Event::WorkspaceStateChanged { info } => {
                let json = serde_json::to_string(&info).unwrap_or_default();
                let _ = qt.queue(move |q| q.workspace_changed(QString::from(&json)));
            }
            Event::DaemonLog {
                level, ref message, ..
            } if level == LogLevel::Warn && message.starts_with(DROP_NOTICE_PREFIX) => {
                let count = message[DROP_NOTICE_PREFIX.len()..]
                    .trim()
                    .parse::<i64>()
                    .unwrap_or(0);
                let _ = qt.queue(move |q| q.output_dropped(count));
            }
            Event::DaemonLog { level, message, .. } => tracing::info!(?level, "{message}"),
            _ => {}
        }
    }
    // The stream only ends when the connection does.
    let _ = qt.queue(|q| q.set_state(ConnectionState::Reconnecting));
}

/// How long to wait before the single `workspace.list` retry.
const LIST_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Fetches the daemon's workspace list and queues `workspaces_listed`. The
/// daemon is authoritative about which workspaces exist and the UI reconciles
/// its restored tabs against this list, so a single lost reply would leave the
/// session with no list at all: one retry, and `operation_failed` only if that
/// also fails.
async fn load_workspace_list(client: DaemonClient, qt: QtHandle) {
    let mut last_error = String::new();
    for attempt in 1..=2 {
        match client
            .request::<WorkspaceListResult>(Request::WorkspaceList {})
            .await
        {
            Ok(res) => {
                let json = serde_json::to_string(&res.workspaces).unwrap_or_else(|_| "[]".into());
                tracing::info!(count = res.workspaces.len(), attempt, "workspaces_listed");
                let _ = qt.queue(move |q| q.workspaces_listed(QString::from(&json)));
                return;
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == 1 {
                    tracing::warn!("workspace.list failed ({last_error}); retrying");
                    tokio::time::sleep(LIST_RETRY_DELAY).await;
                }
            }
        }
    }
    report_failure(&qt, "workspace.list", last_error);
}

impl qobject::AppController {
    pub fn start(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        let settings = Settings::load();
        let spec = LaunchSpec {
            distro: settings.distro.clone(),
            daemon_path_in_wsl: settings.daemon_path.clone(),
            local_daemon_binary: Settings::local_daemon_binary(),
            log_level: settings.log_level.clone(),
        };
        let _ = qt.queue(|q| q.set_state(ConnectionState::Launching));
        runtime().spawn(async move {
            // `test_endpoint` is the `BS_DAEMON_ADDR` test hook and is `None` in
            // every ordinary run, which then launches the daemon inside WSL.
            let (addr, token, proc) = match launcher::test_endpoint() {
                Some((addr, token)) => {
                    tracing::warn!(
                        "{} is set: connecting to {addr} instead of launching a daemon",
                        launcher::TEST_ADDR_ENV
                    );
                    (addr, token, None)
                }
                None => match launcher::launch(&spec).await {
                    Ok(p) => (
                        std::net::SocketAddr::from(([127, 0, 0, 1], p.port)),
                        p.token.clone(),
                        Some(p),
                    ),
                    Err(e) => {
                        let msg = format!("daemon: launch failed: {e:#}");
                        tracing::error!("{msg}");
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().set_state(ConnectionState::Error);
                            q.set_status_message(QString::from(msg.as_str()));
                        });
                        return;
                    }
                },
            };
            let _ = qt.queue(|q| q.set_state(ConnectionState::Connecting));

            let (client, hello, events) =
                match DaemonClient::connect(addr, &token, env!("CARGO_PKG_VERSION")).await {
                    Ok(x) => x,
                    Err(e) => {
                        let msg = format!("daemon: connect failed: {e}");
                        tracing::error!("{msg}");
                        let _ = qt.queue(move |mut q| {
                            q.as_mut().set_state(ConnectionState::Error);
                            q.set_status_message(QString::from(msg.as_str()));
                        });
                        return;
                    }
                };

            // Published before any event is dispatched, so a QObject woken by the
            // first `workspaces_listed` can already reach the daemon.
            let router = EventRouter::new();
            *shared_slot().lock().expect("shared handle mutex poisoned") = Some(Shared {
                client: client.clone(),
                router: router.clone(),
            });

            // Draining starts before any request goes out. The client's event
            // channel is bounded, and its socket reader pushes into it with
            // backpressure, so a daemon that is already streaming (a reconnect to
            // one with live PTYs) would otherwise fill the channel, stall the
            // reader, and starve every reply below until the request timeout.
            runtime().spawn(drain_events(events, router.clone(), qt.clone()));

            let version = hello.daemon_version.clone();
            // `None` under the test hook: there is no daemon process to own.
            let handle: Option<ProcessHandle> =
                proc.map(|p| std::sync::Arc::new(tokio::sync::Mutex::new(Some(p))));
            let c2 = client.clone();
            let _ = qt.queue(move |mut q| {
                q.as_mut().rust_mut().client = Some(c2);
                q.as_mut().rust_mut().process = handle;
                // State first, then the version, so `apply_daemon_version`
                // recomposes the text as "daemon: connected v<version>".
                q.as_mut().set_state(ConnectionState::Connected);
                q.apply_daemon_version(QString::from(version.as_str()));
            });

            // Its own task, so the list does not queue behind the prereq check.
            runtime().spawn(load_workspace_list(client.clone(), qt.clone()));

            // The `BS_SMOKE_SCRIPT` test hook; `None` in every ordinary run.
            if let Some(steps) = smoke::script() {
                runtime().spawn(smoke::run(steps, client.clone(), qt.clone()));
            }

            if let Ok(res) = client
                .request::<CheckPrereqsResult>(Request::SystemCheckPrereqs {})
                .await
            {
                let bad: Vec<String> = res
                    .items
                    .iter()
                    .filter(|i| !i.ok)
                    .map(|i| {
                        let fix = i
                            .fix_hint
                            .as_ref()
                            .map(|f| format!(" (fix: {f})"))
                            .unwrap_or_default();
                        format!("{}: {}{}", i.name, i.detail, fix)
                    })
                    .collect();
                if !bad.is_empty() {
                    let msg = bad.join("\n");
                    let _ = qt.queue(move |q| q.prereq_warning(QString::from(msg.as_str())));
                }
            }
        });
    }

    pub fn create_workspace(
        self: Pin<&mut Self>,
        repo_path: QString,
        base_branch: QString,
        name: QString,
        group: QString,
        adapter: QString,
        command: QString,
    ) {
        let qt = self.qt_thread();
        let params = WorkspaceCreateParams {
            repo_path: repo_path.to_string(),
            base_branch: base_branch.to_string(),
            name: name.to_string(),
        };
        // Echoed straight back on success: the controller keeps no tab state.
        let (group, adapter, command) =
            (group.to_string(), adapter.to_string(), command.to_string());
        let Some(shared) = shared() else {
            report_failure(&qt, "workspace.create", NOT_CONNECTED.to_owned());
            return;
        };
        runtime().spawn(async move {
            match shared
                .client
                .request::<WorkspaceInfo>(Request::WorkspaceCreate(params))
                .await
            {
                Ok(info) => {
                    let json = serde_json::to_string(&info).unwrap_or_default();
                    let _ = qt.queue(move |q| {
                        q.workspace_created(
                            QString::from(&json),
                            QString::from(&group),
                            QString::from(&adapter),
                            QString::from(&command),
                        )
                    });
                }
                Err(e) => report_failure(&qt, "workspace.create", e.to_string()),
            }
        });
    }

    pub fn destroy_workspace(self: Pin<&mut Self>, id: QString, force: bool) {
        let qt = self.qt_thread();
        let id = id.to_string();
        let Some(shared) = shared() else {
            report_failure(&qt, "workspace.destroy", NOT_CONNECTED.to_owned());
            return;
        };
        runtime().spawn(async move {
            let params = WorkspaceDestroyParams {
                workspace_id: WorkspaceId(id.clone()),
                force,
            };
            match shared
                .client
                .request_raw(Request::WorkspaceDestroy(params))
                .await
            {
                Ok(_) => {
                    let _ = qt.queue(move |q| q.workspace_destroyed(QString::from(&id)));
                }
                Err(e) => report_failure(&qt, "workspace.destroy", e.to_string()),
            }
        });
    }

    pub fn inspect_repo(self: Pin<&mut Self>, path: QString) {
        let qt = self.qt_thread();
        let path = path.to_string();
        let Some(shared) = shared() else {
            report_failure(&qt, "repo.inspect", NOT_CONNECTED.to_owned());
            return;
        };
        runtime().spawn(async move {
            let params = RepoPathParams { path: path.clone() };
            match shared
                .client
                .request::<RepoInfo>(Request::RepoInspect(params))
                .await
            {
                Ok(info) => {
                    let json = serde_json::to_string(&info).unwrap_or_default();
                    let _ = qt.queue(move |q| {
                        q.repo_inspected(QString::from(&path), QString::from(&json))
                    });
                }
                Err(e) => report_failure(&qt, "repo.inspect", e.to_string()),
            }
        });
    }

    pub fn wsl_path(&self, windows_path: QString) -> QString {
        let path = windows_path.to_string();
        match launcher::windows_path_to_wsl(std::path::Path::new(&path)) {
            Some(p) => QString::from(&p),
            None => QString::from(""),
        }
    }

    pub fn set_state(mut self: Pin<&mut Self>, state: ConnectionState) {
        self.as_mut().set_connection_state(state.as_i32());
        self.refresh_status(state);
    }

    pub fn apply_daemon_version(mut self: Pin<&mut Self>, version: QString) {
        self.as_mut().set_daemon_version(version);
        let state = ConnectionState::from_i32(*self.connection_state());
        self.refresh_status(state);
    }

    /// Recomposes `status_message` from `state` and the current `daemon_version`.
    fn refresh_status(mut self: Pin<&mut Self>, state: ConnectionState) {
        let version = self.daemon_version().to_string();
        let text = compose_status(state.label(), &version);
        self.as_mut().set_status_message(QString::from(&text));
    }
}

// ---------------------------------------------------------------------------
// Test hook: BS_SMOKE_SCRIPT
// ---------------------------------------------------------------------------

/// Drives a connected IDE with nobody at the keyboard. **Test-only**: with
/// `BS_SMOKE_SCRIPT` unset, which is every ordinary run, [`script`] returns
/// `None`, no task is spawned, and nothing in this module executes.
///
/// The variable holds a comma-separated step list, run in order once the daemon
/// connection is up. `tests/smoke.rs` runs the real IDE binary offscreen against
/// an in-process fake daemon with `create,open,tree,quit`. The steps are:
///
/// * `create` — create a workspace over the repository in `BS_SMOKE_REPO` and
///   emit `workspace_created`, so the window builds its tab, its terminal pane
///   and its file tree from the same signal a user's New Agent would produce.
/// * `open` — open a PTY in the workspace the last `create` made.
/// * `tree` — list that workspace's root directory.
/// * `quit` — let the window settle, then end the process with status 0.
///
/// A failing step logs and stops the script *without* quitting, so a broken run
/// is a process that never exits rather than a green exit code.
mod smoke {
    use super::QtHandle;
    use crate::client::DaemonClient;
    use bondsymphonic_proto::*;
    use cxx_qt_lib::QString;
    use std::time::Duration;

    /// The comma-separated step list. Unset in every ordinary run.
    const SCRIPT_ENV: &str = "BS_SMOKE_SCRIPT";
    /// The repository `create` builds its workspace from. A fake daemon ignores
    /// it, so the default only has to be a path shape.
    const REPO_ENV: &str = "BS_SMOKE_REPO";
    const DEFAULT_REPO: &str = "/smoke/repo";
    const BASE_BRANCH: &str = "main";
    /// The group the created tabs are filed under.
    const GROUP: &str = "Default";
    /// How long `quit` leaves the window running before ending the process.
    const QUIT_DELAY: Duration = Duration::from_secs(2);
    const PTY_COLS: u16 = 80;
    const PTY_ROWS: u16 = 24;

    /// The steps in `BS_SMOKE_SCRIPT`, or `None` when it is unset or empty.
    pub(super) fn script() -> Option<Vec<String>> {
        let raw = std::env::var(SCRIPT_ENV).ok()?;
        let steps: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        (!steps.is_empty()).then_some(steps)
    }

    /// Runs `steps` in order against the connected daemon.
    pub(super) async fn run(steps: Vec<String>, client: DaemonClient, qt: QtHandle) {
        let repo = std::env::var(REPO_ENV).unwrap_or_else(|_| DEFAULT_REPO.to_owned());
        let mut workspace: Option<WorkspaceId> = None;
        let mut created = 0usize;
        for step in steps {
            tracing::info!(target: "smoke", "step: {step}");
            let outcome = match step.as_str() {
                "create" => {
                    created += 1;
                    create(&client, &qt, &repo, created)
                        .await
                        .map(|id| workspace = Some(id))
                }
                "open" => open(&client, workspace.as_ref()).await,
                "tree" => tree(&client, workspace.as_ref()).await,
                "quit" => quit(&qt).await,
                other => Err(format!("unknown step {other:?}")),
            };
            if let Err(e) = outcome {
                tracing::error!(target: "smoke", "step {step:?} failed: {e}");
                return;
            }
        }
    }

    /// Creates a workspace and announces it with the signal the
    /// `create_workspace` invokable emits, which is what the window listens for.
    async fn create(
        client: &DaemonClient,
        qt: &QtHandle,
        repo: &str,
        nth: usize,
    ) -> Result<WorkspaceId, String> {
        let name = if nth == 1 {
            "smoke".to_owned()
        } else {
            format!("smoke{nth}")
        };
        let info = client
            .request::<WorkspaceInfo>(Request::WorkspaceCreate(WorkspaceCreateParams {
                repo_path: repo.to_owned(),
                base_branch: BASE_BRANCH.to_owned(),
                name,
            }))
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(target: "smoke", "created {:?}", info.id);
        let json = serde_json::to_string(&info).unwrap_or_default();
        let _ = qt.queue(move |q| {
            q.workspace_created(
                QString::from(&json),
                QString::from(GROUP),
                QString::from("terminal"),
                QString::from(""),
            )
        });
        Ok(info.id)
    }

    async fn open(client: &DaemonClient, workspace: Option<&WorkspaceId>) -> Result<(), String> {
        let workspace_id = need(workspace, "open")?;
        let res = client
            .request::<PtyOpenResult>(Request::PtyOpen(PtyOpenParams {
                workspace_id,
                cols: PTY_COLS,
                rows: PTY_ROWS,
                command: None,
            }))
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(target: "smoke", "opened {:?}", res.pty_id);
        Ok(())
    }

    async fn tree(client: &DaemonClient, workspace: Option<&WorkspaceId>) -> Result<(), String> {
        let workspace_id = need(workspace, "tree")?;
        let res = client
            .request::<ListDirResult>(Request::FsListDir(FsPathParams {
                workspace_id,
                path: String::new(),
            }))
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(target: "smoke", "root listed: {} entries", res.entries.len());
        Ok(())
    }

    /// Ends the run from the Qt thread, so an event loop that has wedged never
    /// reaches it and the test times out rather than passing.
    ///
    /// This is `std::process::exit`, not `QApplication::quit()`. `quit` is a
    /// static member, which cxx cannot bind, and cxx-qt-lib exposes neither it
    /// nor `QCoreApplication::instance()`, so leaving the event loop properly
    /// would need a C++ shim in `cpp/app.*`. The cost of exiting instead is that
    /// destructors do not run, so a crash on the way out would go unseen.
    async fn quit(qt: &QtHandle) -> Result<(), String> {
        tokio::time::sleep(QUIT_DELAY).await;
        qt.queue(|_| {
            tracing::info!(target: "smoke", "quit");
            std::process::exit(0)
        })
        .map_err(|_| "the Qt thread is gone".to_owned())
    }

    /// The workspace a step operates on, or an error naming what is missing.
    fn need(workspace: Option<&WorkspaceId>, step: &str) -> Result<WorkspaceId, String> {
        workspace
            .cloned()
            .ok_or_else(|| format!("`{step}` needs a `create` before it"))
    }
}
