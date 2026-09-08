use crate::client::DaemonClient;
use crate::launcher::{self, LaunchSpec};
use crate::model::app_state::{compose_status, ConnectionState};
use crate::qobjects::settings::Settings;
use bondsymphonic_proto::*;
use std::sync::OnceLock;

/// One process-wide multi-threaded runtime drives the daemon connection. It is
/// intentionally never dropped: the Qt event loop owns the main thread, and the
/// daemon process handle parked on the controller outlives every task here.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio")
    })
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

        /// Launch the daemon inside WSL and connect to it.
        #[qinvokable]
        fn start(self: Pin<&mut AppController>);

        /// Record the daemon version and recompose the status text. Callers use
        /// this rather than `set_daemon_version` so the status bar stays in sync.
        #[qinvokable]
        fn apply_daemon_version(self: Pin<&mut AppController>, version: QString);
    }

    impl cxx_qt::Threading for AppController {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

/// Shared handle to the running daemon so a later "Exit" action can shut it down.
type ProcessHandle = std::sync::Arc<tokio::sync::Mutex<Option<launcher::DaemonProcess>>>;

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
            let proc = match launcher::launch(&spec).await {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("daemon: launch failed: {e:#}");
                    tracing::error!("{msg}");
                    let _ = qt.queue(move |mut q| {
                        q.as_mut().set_state(ConnectionState::Error);
                        q.set_status_message(QString::from(msg.as_str()));
                    });
                    return;
                }
            };
            let _ = qt.queue(|q| q.set_state(ConnectionState::Connecting));

            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], proc.port));
            let (client, hello, mut events) =
                match DaemonClient::connect(addr, &proc.token, env!("CARGO_PKG_VERSION")).await {
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

            let version = hello.daemon_version.clone();
            let handle: ProcessHandle = std::sync::Arc::new(tokio::sync::Mutex::new(Some(proc)));
            let c2 = client.clone();
            let _ = qt.queue(move |mut q| {
                q.as_mut().rust_mut().client = Some(c2);
                q.as_mut().rust_mut().process = Some(handle);
                // State first, then the version, so `apply_daemon_version`
                // recomposes the text as "daemon: connected v<version>".
                q.as_mut().set_state(ConnectionState::Connected);
                q.apply_daemon_version(QString::from(version.as_str()));
            });

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

            while let Some((_ws, ev)) = events.recv().await {
                if let Event::DaemonLog { level, message, .. } = ev {
                    tracing::info!(?level, "{message}");
                }
                // Later milestones route events to their QObjects here.
            }
            let _ = qt.queue(|q| q.set_state(ConnectionState::Reconnecting));
        });
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
