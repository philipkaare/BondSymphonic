//! The Qt face of [`crate::host_setup`]: runs the check-and-provision on a
//! blocking thread, reports each step and each line of output, and says
//! `ready` when the daemon can be launched.
//!
//! `app.cpp` starts the daemon connection on `ready`, so on a machine with
//! nothing set up the first thing the IDE does is set it up, in a dialog
//! (`HostSetupDialog`) that shows the work as it happens.

use crate::host_setup::{self, Outcome, Reporter};
use crate::qobjects::app_controller::runtime;
use crate::qobjects::settings::Settings;
use serde::Serialize;
use std::sync::{Arc, Mutex};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        type HostSetup = super::HostSetupRust;

        /// Something in `statusJson` changed.
        #[qsignal]
        fn status_changed(self: Pin<&mut HostSetup>);

        /// One line of a setup command's output.
        #[qsignal]
        fn output(self: Pin<&mut HostSetup>, line: QString);

        /// The daemon can be launched now. May be emitted more than once
        /// (a retry after a skip); `AppController::start` ignores repeats.
        #[qsignal]
        fn ready(self: Pin<&mut HostSetup>);

        /// Checks, and sets up whatever is missing. Also the Retry button.
        #[qinvokable]
        fn start(self: Pin<&mut HostSetup>);

        /// Windows only: installs WSL, elevated, then carries on if it works
        /// without a restart.
        #[qinvokable]
        fn install_wsl(self: Pin<&mut HostSetup>);

        /// Gives up on setting up and launches the daemon anyway, which then
        /// fails in the status bar the way it always has.
        #[qinvokable]
        fn skip(self: Pin<&mut HostSetup>);

        /// `phase` (`idle`, `checking`, `working`, `needs_wsl`,
        /// `needs_restart`, `failed`, `ready`) and `message`.
        #[qinvokable]
        fn status_json(self: &HostSetup) -> QString;
    }

    impl cxx_qt::Threading for HostSetup {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Idle,
    /// The quick check; nothing worth a dialog yet.
    Checking,
    /// Creating, starting or provisioning: shown.
    Working,
    NeedsWsl,
    NeedsRestart,
    Failed,
    Ready,
}

#[derive(Debug, Clone, Serialize)]
struct Status {
    phase: Phase,
    message: String,
}

struct Shared {
    status: Status,
    running: bool,
}

type SharedHandle = Arc<Mutex<Shared>>;
type QtHandle = cxx_qt::CxxQtThread<qobject::HostSetup>;

pub struct HostSetupRust {
    shared: SharedHandle,
}

impl Default for HostSetupRust {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                status: Status {
                    phase: Phase::Idle,
                    message: String::new(),
                },
                running: false,
            })),
        }
    }
}

fn publish(shared: &SharedHandle, qt: &QtHandle, phase: Phase, message: &str) {
    {
        let mut s = shared.lock().expect("host setup state");
        s.status = Status {
            phase,
            message: message.to_owned(),
        };
    }
    let ready = phase == Phase::Ready;
    let _ = qt.queue(move |mut q| {
        q.as_mut().status_changed();
        if ready {
            q.ready();
        }
    });
}

struct QtReporter {
    shared: SharedHandle,
    qt: QtHandle,
}

impl Reporter for QtReporter {
    fn step(&mut self, title: &str) {
        tracing::info!("host setup: {title}");
        publish(&self.shared, &self.qt, Phase::Working, title);
    }

    fn line(&mut self, line: &str) {
        tracing::info!(target: "host_setup", "{line}");
        let line = line.to_owned();
        let _ = self.qt.queue(move |q| q.output(QString::from(&line)));
    }
}

/// Whether this run should skip the setup altogether: the `BS_DAEMON_ADDR`
/// test hook connects to a fake daemon and never touches WSL or Lima, and
/// `BS_SKIP_HOST_SETUP` is the same switch for a person.
fn skipped() -> bool {
    crate::launcher::test_endpoint().is_some()
        || std::env::var_os("BS_SKIP_HOST_SETUP").is_some_and(|v| !v.is_empty())
}

/// Runs `work` on a blocking thread unless a run is already going.
fn run_exclusive(
    shared: SharedHandle,
    qt: QtHandle,
    work: impl FnOnce(&mut QtReporter) -> Outcome + Send + 'static,
) {
    {
        let mut s = shared.lock().expect("host setup state");
        if s.running {
            return;
        }
        s.running = true;
    }
    runtime().spawn_blocking(move || {
        let mut reporter = QtReporter {
            shared: shared.clone(),
            qt: qt.clone(),
        };
        let outcome = work(&mut reporter);
        shared.lock().expect("host setup state").running = false;
        match outcome {
            Outcome::Ready => publish(&shared, &qt, Phase::Ready, ""),
            Outcome::NeedsWsl => publish(
                &shared,
                &qt,
                Phase::NeedsWsl,
                "BondSymphonic runs its agents in Linux, through WSL, which is not installed on \
                 this computer yet. Installing it needs administrator permission and usually a \
                 restart of Windows.",
            ),
            Outcome::NeedsRestart => publish(
                &shared,
                &qt,
                Phase::NeedsRestart,
                "WSL is installed. Restart Windows to finish, then start BondSymphonic again; it \
                 carries on with the setup from there.",
            ),
            Outcome::Failed(why) => {
                tracing::warn!("host setup failed: {why}");
                publish(&shared, &qt, Phase::Failed, &why);
            }
        }
    });
}

fn ensure(reporter: &mut QtReporter) -> Outcome {
    let distro = Settings::load().distro;
    let scripts = std::env::current_exe()
        .ok()
        .and_then(|exe| host_setup::find_scripts(&exe));
    host_setup::ensure(&distro, scripts.as_ref(), reporter)
}

impl qobject::HostSetup {
    pub fn start(self: Pin<&mut Self>) {
        let shared = self.rust().shared.clone();
        let qt = self.qt_thread();
        if skipped() {
            publish(&shared, &qt, Phase::Ready, "");
            return;
        }
        publish(
            &shared,
            &qt,
            Phase::Checking,
            "Checking the Linux environment…",
        );
        run_exclusive(shared, qt, ensure);
    }

    pub fn install_wsl(self: Pin<&mut Self>) {
        let shared = self.rust().shared.clone();
        let qt = self.qt_thread();
        run_exclusive(shared, qt, |reporter| {
            if let Err(why) = host_setup::install_wsl(reporter) {
                return Outcome::Failed(format!("WSL was not installed: {why}"));
            }
            if host_setup::wsl_works() {
                ensure(reporter)
            } else {
                Outcome::NeedsRestart
            }
        });
    }

    pub fn skip(self: Pin<&mut Self>) {
        let shared = self.rust().shared.clone();
        publish(&shared, &self.qt_thread(), Phase::Ready, "");
    }

    pub fn status_json(&self) -> QString {
        let s = self.rust().shared.lock().expect("host setup state");
        QString::from(&serde_json::to_string(&s.status).unwrap_or_default())
    }
}
