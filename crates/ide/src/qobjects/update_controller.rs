//! The IDE's side of self-update: when to check, where downloads go, what the
//! Settings section and the status bar are told, and the hand-over to the
//! installer when the IDE exits.
//!
//! The deciding is all in `bondsymphonic-updater`; this file is scheduling and
//! plumbing. One check runs at a time, on a blocking thread of the shared
//! runtime, and every change of state is announced with `statusChanged` --
//! C++ then reads the whole picture back through `statusJson`.

use crate::qobjects::app_controller::runtime;
use crate::qobjects::settings::Settings;
use bondsymphonic_updater::{
    self as updater, Check, InstallKind, Prepared, Source, UpdateError, Updater, Version,
};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        type UpdateController = super::UpdateControllerRust;

        /// Something in `statusJson` changed.
        #[qsignal]
        fn status_changed(self: Pin<&mut UpdateController>);

        /// Starts the automatic schedule. Called once, after the window is up.
        #[qinvokable]
        fn start(self: Pin<&mut UpdateController>);

        /// The whole state, as JSON: `phase` (`idle`, `checking`,
        /// `downloading`, `ready`, `up_to_date`, `available`, `failed`,
        /// `unavailable`), `current`, `latest`, `progress` (0..1),
        /// `message`, `last_checked` (Unix seconds, 0 for never),
        /// `installable` and `notes_url`.
        #[qinvokable]
        fn status_json(self: &UpdateController) -> QString;

        #[qinvokable]
        fn automatic(self: &UpdateController) -> bool;

        #[qinvokable]
        fn set_automatic(self: Pin<&mut UpdateController>, automatic: bool);

        /// Checks now and, if there is a newer release, downloads it --
        /// whatever the automatic setting says, since the user asked.
        #[qinvokable]
        fn check_now(self: Pin<&mut UpdateController>);

        /// The version a downloaded, verified update would install, or empty.
        #[qinvokable]
        fn ready_version(self: &UpdateController) -> QString;

        /// Hands a ready update to the installer helper, which waits for this
        /// process to exit. `relaunch` starts the new version afterwards.
        /// Returns whether a helper was started. Call on the way out only.
        #[qinvokable]
        fn install_on_exit(self: &UpdateController, relaunch: bool) -> bool;

        /// Asks the window to close and restart into the ready update. The
        /// window owns closing (it asks about unsaved editors first), so this
        /// only says so.
        #[qinvokable]
        fn request_restart(self: Pin<&mut UpdateController>);

        #[qsignal]
        fn restart_requested(self: Pin<&mut UpdateController>);
    }

    impl cxx_qt::Threading for UpdateController {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

/// The first automatic check waits this long, so it never competes with the
/// daemon launch for the first seconds of a start.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
/// Then one check every this often, for as long as the IDE runs.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Idle,
    Checking,
    Downloading,
    /// Downloaded, verified and prepared: installs on exit.
    Ready,
    UpToDate,
    /// Newer release exists but this copy cannot install it itself.
    Available,
    Failed,
    /// This build cannot update at all (no key, unsupported platform).
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
struct Status {
    phase: Phase,
    current: String,
    latest: String,
    progress: f64,
    message: String,
    last_checked: u64,
    installable: bool,
    notes_url: String,
}

struct Shared {
    status: Status,
    /// A ready update and the version it installs.
    prepared: Option<(Version, Prepared)>,
    busy: bool,
}

type SharedHandle = Arc<Mutex<Shared>>;
type QtHandle = cxx_qt::CxxQtThread<qobject::UpdateController>;

pub struct UpdateControllerRust {
    shared: SharedHandle,
    kind: InstallKind,
    started: bool,
}

impl Default for UpdateControllerRust {
    fn default() -> Self {
        let kind = std::env::current_exe()
            .map(|exe| InstallKind::of_exe(&exe))
            .unwrap_or(InstallKind::NotInstalled);
        let status = Status {
            phase: Phase::Idle,
            current: env!("CARGO_PKG_VERSION").to_owned(),
            latest: String::new(),
            progress: 0.0,
            message: String::new(),
            last_checked: 0,
            installable: kind.can_install(),
            notes_url: String::new(),
        };
        Self {
            shared: Arc::new(Mutex::new(Shared {
                status,
                prepared: None,
                busy: false,
            })),
            kind,
            started: false,
        }
    }
}

/// `<cache>/BondSymphonic`: `%LOCALAPPDATA%` on Windows, `~/Library/Caches`
/// on macOS.
fn cache_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.cache_dir().join("BondSymphonic"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn notify(qt: &QtHandle) {
    let _ = qt.queue(|q| q.status_changed());
}

fn set(shared: &SharedHandle, qt: &QtHandle, change: impl FnOnce(&mut Status)) {
    change(&mut shared.lock().expect("update state").status);
    notify(qt);
}

/// One check, and the download and preparation that follow it. Blocking:
/// run on a blocking thread.
fn run_check(shared: SharedHandle, qt: QtHandle, kind: InstallKind) {
    {
        let mut s = shared.lock().expect("update state");
        if s.busy {
            return;
        }
        s.busy = true;
    }
    let outcome = check_and_fetch(&shared, &qt, &kind);
    let mut s = shared.lock().expect("update state");
    s.busy = false;
    s.status.last_checked = now_unix();
    if let Err(e) = outcome {
        tracing::warn!("update check: {e}");
        s.status.phase = match &e {
            UpdateError::Unsupported(_) | UpdateError::Manifest(updater::ManifestError::NoKey) => {
                Phase::Unavailable
            }
            _ => Phase::Failed,
        };
        s.status.message = e.to_string();
        // A failed check says nothing about a download already made: keep it.
        let ready = s.prepared.as_ref().map(|(version, _)| version.to_string());
        if let Some(version) = ready {
            s.status.phase = Phase::Ready;
            s.status.latest = version;
        }
    }
    drop(s);
    notify(&qt);
}

fn check_and_fetch(
    shared: &SharedHandle,
    qt: &QtHandle,
    kind: &InstallKind,
) -> Result<(), UpdateError> {
    set(shared, qt, |s| {
        s.phase = Phase::Checking;
        s.message.clear();
    });
    let updater = Updater::new(
        env!("CARGO_PKG_VERSION"),
        Source::github(updater::REPOSITORY),
    )?;
    let release = match updater.check()? {
        Check::UpToDate { latest } => {
            set(shared, qt, |s| {
                s.phase = Phase::UpToDate;
                s.latest = latest.to_string();
            });
            return Ok(());
        }
        Check::Available(release) => release,
    };
    let version = release.version;
    if matches!(&shared.lock().expect("update state").prepared, Some((v, _)) if *v == version) {
        set(shared, qt, |s| s.phase = Phase::Ready);
        return Ok(());
    }
    set(shared, qt, |s| {
        s.latest = version.to_string();
        s.notes_url = release.notes_url.clone();
    });
    if !kind.can_install() {
        set(shared, qt, |s| {
            s.phase = Phase::Available;
            s.message = "This copy was not installed with the BondSymphonic installer, so it \
                         cannot update itself. Download the new version from the releases page."
                .into();
        });
        return Ok(());
    }

    let root = cache_root().ok_or_else(|| {
        UpdateError::Unsupported("there is no cache folder to download into".into())
    })?;
    let dir = root.join("updates");
    set(shared, qt, |s| {
        s.phase = Phase::Downloading;
        s.progress = 0.0;
    });
    let mut last_percent = 0u64;
    let cancel = AtomicBool::new(false);
    let download = updater.download(&release, &dir, &cancel, &mut |done, total| {
        // One signal per percent, not per 256 KB chunk.
        let percent = done * 100 / total.max(1);
        if percent != last_percent {
            last_percent = percent;
            set(shared, qt, |s| {
                s.progress = done as f64 / total.max(1) as f64
            });
        }
    })?;
    let work = dir.join(format!("{version}-extracted"));
    let prepared = updater::install::prepare(kind, &download, &work)?;
    updater::prune(&dir, &[download.as_path(), work.as_path()]);
    tracing::info!(%version, path = %prepared.path.display(), "update ready to install on exit");
    let mut s = shared.lock().expect("update state");
    s.prepared = Some((version, prepared));
    s.status.phase = Phase::Ready;
    s.status.progress = 1.0;
    s.status.message.clear();
    Ok(())
}

fn spawn_check(shared: SharedHandle, qt: QtHandle, kind: InstallKind) {
    runtime().spawn_blocking(move || run_check(shared, qt, kind));
}

impl qobject::UpdateController {
    pub fn start(mut self: Pin<&mut Self>) {
        if self.rust().started {
            return;
        }
        self.as_mut().rust_mut().started = true;
        // The automated runs (`BS_SMOKE_SCRIPT`, the `BS_DAEMON_ADDR` fake
        // daemon) must not reach out to GitHub on a timer of their own.
        if std::env::var_os("BS_SMOKE_SCRIPT").is_some()
            || crate::launcher::test_endpoint().is_some()
        {
            return;
        }
        let qt = self.qt_thread();
        let shared = self.rust().shared.clone();
        let kind = self.rust().kind.clone();
        // A download an earlier run made and never installed is reused when
        // the check finds the same release (see `Updater::download`).
        runtime().spawn(async move {
            tokio::time::sleep(FIRST_CHECK_DELAY).await;
            loop {
                if crate::qobjects::app_controller::quitting() {
                    return;
                }
                if Settings::load().updates_automatic {
                    let (shared, qt, kind) = (shared.clone(), qt.clone(), kind.clone());
                    let _ = tokio::task::spawn_blocking(move || run_check(shared, qt, kind)).await;
                }
                tokio::time::sleep(CHECK_INTERVAL).await;
            }
        });
    }

    pub fn status_json(&self) -> QString {
        let s = self.rust().shared.lock().expect("update state");
        QString::from(&serde_json::to_string(&s.status).unwrap_or_default())
    }

    pub fn automatic(&self) -> bool {
        Settings::load().updates_automatic
    }

    pub fn set_automatic(self: Pin<&mut Self>, automatic: bool) {
        let mut settings = match Settings::try_load() {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(
                    "settings: not saving the update choice over an unreadable file: {e}"
                );
                return;
            }
        };
        if settings.updates_automatic == automatic {
            return;
        }
        settings.updates_automatic = automatic;
        if let Err(e) = settings.save() {
            tracing::warn!("settings: saving the update choice: {e}");
            return;
        }
        // Switching automatic on is a good moment to find out.
        if automatic && self.rust().started {
            spawn_check(
                self.rust().shared.clone(),
                self.qt_thread(),
                self.rust().kind.clone(),
            );
        }
    }

    pub fn check_now(self: Pin<&mut Self>) {
        spawn_check(
            self.rust().shared.clone(),
            self.qt_thread(),
            self.rust().kind.clone(),
        );
    }

    pub fn ready_version(&self) -> QString {
        let s = self.rust().shared.lock().expect("update state");
        QString::from(
            &s.prepared
                .as_ref()
                .map(|(v, _)| v.to_string())
                .unwrap_or_default(),
        )
    }

    pub fn install_on_exit(&self, relaunch: bool) -> bool {
        let s = self.rust().shared.lock().expect("update state");
        let Some((version, prepared)) = &s.prepared else {
            return false;
        };
        let log = cache_root()
            .unwrap_or_else(std::env::temp_dir)
            .join("update-install.log");
        match updater::install::install_after_exit(
            &self.rust().kind,
            prepared,
            std::process::id(),
            relaunch,
            &log,
        ) {
            Ok(()) => {
                tracing::info!(%version, relaunch, log = %log.display(), "installing the update once the IDE exits");
                true
            }
            Err(e) => {
                tracing::warn!("could not start the update installer: {e}");
                false
            }
        }
    }

    pub fn request_restart(self: Pin<&mut Self>) {
        self.restart_requested();
    }
}
