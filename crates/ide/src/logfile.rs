//! The IDE's log file: the lines that go to the console, also kept on disk.
//!
//! The IDE logs to the console and relays the daemon's whole stderr into the
//! same stream, and until this module nothing of it reached disk. Diagnosing a
//! launch-time hang therefore meant asking the user to capture the console
//! with `.\launch.ps1 *> file` -- which gave a UTF-16 file full of PowerShell
//! error-record wrapping, and then hung `launch.ps1` after the IDE had quit,
//! because the daemon's `wsl.exe` relay had inherited the redirected handle.
//! The next time something goes wrong at launch the file must already be
//! there: `%LOCALAPPDATA%\BondSymphonic\logs\ide.log`, with the launch before
//! it beside it as `ide.1.log`.
//!
//! The console output is left exactly as it was. The file gets the same lines
//! through the same filter, minus the ANSI colour.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// The log filter, in `tracing_subscriber::EnvFilter` syntax; `info` when
/// unset. One filter for the console and the file alike, so the file never
/// shows less than the console did and vice versa.
pub const FILTER_ENV: &str = "BS_LOG";

/// Set to `0` or empty, turns the file off and leaves the console as the only
/// output, which is what the IDE did before the file existed.
pub const FILE_ENV: &str = "BS_LOG_FILE";

/// This launch's log.
pub const CURRENT: &str = "ide.log";

/// The launch before it.
pub const PREVIOUS: &str = "ide.1.log";

/// Where the log files live: `<data_local_dir>\BondSymphonic\logs`, which on
/// Windows is `%LOCALAPPDATA%\BondSymphonic\logs`. Local rather than the
/// roaming `%APPDATA%` that `settings.json` uses: a log is diagnostic output
/// about this machine, and nothing is gained by syncing it to another.
pub fn log_dir(data_local_dir: &Path) -> PathBuf {
    data_local_dir.join("BondSymphonic").join("logs")
}

/// The path this launch writes to, under `dir`.
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join(CURRENT)
}

/// Whether [`FILE_ENV`] leaves the file on.
pub fn file_enabled() -> bool {
    match std::env::var_os(FILE_ENV) {
        None => true,
        Some(value) => !(value.is_empty() || value == "0"),
    }
}

/// Moves the previous launch's `ide.log` to `ide.1.log`, dropping whatever
/// `ide.1.log` held before. No `ide.log` is not an error: the first launch on
/// a machine has nothing to rotate.
///
/// Rotation is per launch, not by size, because of what the file is for. The
/// failure that motivated it happens at launch, and what a diagnosis reads is
/// the whole of one launch from its first line -- a size cap could cut exactly
/// that beginning away, and would add a length check to every line written.
/// Two launches keep the file set bounded without any of that: this run, and
/// the one before it that the user is usually reporting on.
pub fn rotate(dir: &Path) -> io::Result<()> {
    let current = log_path(dir);
    if !current.exists() {
        return Ok(());
    }
    // `rename` replaces an existing destination on Windows as on Unix, so a
    // stale `ide.1.log` needs no removal of its own.
    std::fs::rename(current, dir.join(PREVIOUS))
}

/// Creates `dir` if it is missing, rotates the previous launch out of the way
/// and opens a fresh `ide.log` there.
pub fn open(dir: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    rotate(dir)?;
    File::create(log_path(dir))
}

/// How the file side of [`subscriber`] came out, reported once the subscriber
/// is installed so the report itself goes through it.
pub enum FileOutcome {
    /// Off by [`FILE_ENV`].
    Disabled,
    /// Open and receiving lines.
    Open(PathBuf),
    /// Could not be opened; the console is the only output.
    Failed { path: PathBuf, error: io::Error },
}

/// Builds the subscriber: `filter` in front of a console layer that behaves
/// exactly as the pre-file `tracing_subscriber::fmt().init()` did, and, when
/// `dir` is given and the file opens, a second layer writing the same lines to
/// `ide.log` in `dir` without colour. A file that fails to open costs the file
/// layer and nothing else.
pub fn subscriber(
    filter: EnvFilter,
    dir: Option<&Path>,
) -> (impl Subscriber + Send + Sync, FileOutcome) {
    let (file, outcome) = match dir {
        None => (None, FileOutcome::Disabled),
        Some(dir) => match open(dir) {
            Ok(file) => (Some(file), FileOutcome::Open(log_path(dir))),
            Err(error) => (
                None,
                FileOutcome::Failed {
                    path: log_path(dir),
                    error,
                },
            ),
        },
    };
    // The file is written line by line through a plain `Mutex<File>`, with no
    // buffer and no background writer thread on purpose: when the IDE hangs at
    // launch, the line that matters is the last one before the hang, and it
    // has to be on disk already rather than in a buffer the hung process will
    // never flush.
    let file_layer = file.map(|file| fmt::layer().with_ansi(false).with_writer(Mutex::new(file)));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer);
    (subscriber, outcome)
}

/// Installs the global subscriber for this process: [`FILTER_ENV`] as the
/// filter, the console, and unless [`FILE_ENV`] turns it off, the file under
/// the per-user local data directory. A file that cannot be opened is one
/// warning on the console and otherwise ignored; the IDE never fails to start
/// over its own log.
pub fn init() {
    let filter = EnvFilter::new(std::env::var(FILTER_ENV).unwrap_or_else(|_| "info".into()));
    let dir = file_enabled()
        .then(|| directories::BaseDirs::new().map(|d| log_dir(d.data_local_dir())))
        .flatten();
    let (subscriber, outcome) = subscriber(filter, dir.as_deref());
    subscriber.init();
    match outcome {
        FileOutcome::Disabled => {}
        FileOutcome::Open(path) => tracing::info!(path = %path.display(), "log file"),
        FileOutcome::Failed { path, error } => tracing::warn!(
            path = %path.display(),
            %error,
            "log file could not be opened; logging to the console only"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh directory under the system temp dir. The crate has no
    /// `tempfile`; a pid plus a counter is unique enough for tests that run in
    /// one process.
    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bs-logfile-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn log_dir_is_the_app_logs_folder_under_the_local_data_dir() {
        let base = Path::new(r"C:\Users\someone\AppData\Local");
        assert_eq!(
            log_dir(base),
            PathBuf::from(r"C:\Users\someone\AppData\Local\BondSymphonic\logs")
        );
        assert_eq!(log_path(&log_dir(base)), log_dir(base).join("ide.log"));
    }

    #[test]
    fn rotate_moves_the_previous_launch_aside_and_drops_the_one_before() {
        let dir = temp_dir("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CURRENT), "this launch\n").unwrap();
        std::fs::write(dir.join(PREVIOUS), "the launch before\n").unwrap();

        rotate(&dir).unwrap();

        assert!(
            !dir.join(CURRENT).exists(),
            "ide.log is gone until the next open"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(PREVIOUS)).unwrap(),
            "this launch\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_with_nothing_to_rotate_is_not_an_error() {
        let dir = temp_dir("empty");
        std::fs::create_dir_all(&dir).unwrap();
        rotate(&dir).unwrap();
        assert!(!dir.join(PREVIOUS).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_creates_the_directory_and_starts_fresh() {
        let dir = temp_dir("open").join("nested");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CURRENT), "old\n").unwrap();
        // A missing parent chain is the first launch on a machine.
        let deeper = dir.join("also").join("missing");

        drop(open(&dir).unwrap());
        drop(open(&deeper).unwrap());

        assert_eq!(std::fs::read_to_string(dir.join(CURRENT)).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(dir.join(PREVIOUS)).unwrap(),
            "old\n"
        );
        assert_eq!(std::fs::read_to_string(deeper.join(CURRENT)).unwrap(), "");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn file_layer_writes_the_line_plain_and_filtered() {
        let dir = temp_dir("layer");
        let (subscriber, outcome) = subscriber(EnvFilter::new("info"), Some(&dir));
        assert!(matches!(outcome, FileOutcome::Open(ref p) if *p == log_path(&dir)));
        // Scoped rather than global, so the test does not fight the other
        // tests in this process for the one global subscriber.
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kept by the filter");
            tracing::debug!("dropped by the filter");
        });

        let text = std::fs::read_to_string(log_path(&dir)).unwrap();
        assert!(text.contains("kept by the filter"), "{text:?}");
        assert!(!text.contains("dropped by the filter"), "{text:?}");
        assert!(
            !text.contains('\x1b'),
            "no ANSI colour in the file: {text:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_open_costs_only_the_file() {
        let dir = temp_dir("unopenable");
        // A file where the directory should be: `create_dir_all` fails on it.
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "not a directory").unwrap();

        let (subscriber, outcome) = subscriber(EnvFilter::new("info"), Some(&dir));
        assert!(matches!(outcome, FileOutcome::Failed { .. }));
        tracing::subscriber::with_default(subscriber, || tracing::info!("console still works"));
        std::fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn no_dir_means_the_file_is_off() {
        let (_, outcome) = subscriber(EnvFilter::new("info"), None);
        assert!(matches!(outcome, FileOutcome::Disabled));
    }
}
