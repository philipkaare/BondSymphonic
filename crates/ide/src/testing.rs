//! The one place the integration tests decide what to do without a Qt runtime.
//!
//! Three suites (`smoke`, `reconnect_tests`, `restore_tests`) launch the IDE
//! binary, so they need the Qt libraries on `PATH` and there is nothing for
//! them to run against without one. Off a developer machine that has run
//! `scripts\env.ps1` there is no runtime to launch, and a hard failure there
//! says nothing about the code.
//!
//! So they skip -- loudly. The skip prints `SKIP: <reason>` on stderr rather
//! than passing silently, and under the `require-qt` cargo feature it is a
//! panic instead. CI's Windows job turns that feature on (Milestone 7 Task 5),
//! which is what stops a machine that lost its Qt installation from reporting a
//! green run in which nothing was actually exercised.
//!
//! The pure-Rust suites -- `qobject_smoke` among them -- deliberately have no
//! guard: they call library functions that need no Qt runtime, and skipping
//! them would delete real coverage rather than protect anything. See IDE design
//! §14.

use std::ffi::OsStr;

/// Whether the caller should return without doing anything, because the Qt
/// runtime the IDE needs cannot be found.
///
/// `what` names the suite, so a run with several skips says which. Prints
/// `SKIP: <what>: <reason>` on stderr and answers `true`; with `require-qt` on
/// it panics with the same reason and never answers at all.
#[must_use]
pub fn skip_without_qt(what: &str) -> bool {
    if qt_is_available() {
        return false;
    }
    let reason = format!(
        "{what}: neither QMAKE nor a qmake on PATH, so the Qt runtime the IDE needs is not \
         reachable. Dot-source scripts\\env.ps1 and run again."
    );
    if cfg!(feature = "require-qt") {
        panic!("{reason}");
    }
    // stderr, with the `SKIP:` prefix, which is how every other skip in this
    // repository reports itself (the daemon's `bwrap` guards included). Cargo
    // captures it unless the run asks for `--nocapture`.
    eprintln!("SKIP: {reason}");
    true
}

/// Whether a Qt installation can be reached at all.
///
/// `QMAKE` first, because `scripts\env.ps1` is what sets it and it travels
/// alongside `<Qt>\bin` going on `PATH`; the IDE's own `build.rs` reads the
/// same variable through `cxx-qt-build`.
///
/// `qmake` on `PATH` second, because `cxx-qt-build` accepts that too. A host
/// that builds and runs the suite perfectly well, but reached its Qt some other
/// way than `env.ps1`, should not have real coverage skipped out from under it
/// -- and under `require-qt` should not fail a job that would otherwise be
/// green. Finding `qmake` on `PATH` also means `<Qt>\bin` is on `PATH`, which
/// is where the runtime libraries the launched binary needs actually live, so
/// this asks about the thing rather than about a habit.
fn qt_is_available() -> bool {
    std::env::var_os("QMAKE").is_some() || path_has_qmake(std::env::var_os("PATH").as_deref())
}

/// Whether any directory in a `PATH`-shaped string holds a `qmake`.
///
/// Takes the value rather than reading the environment, so the search itself is
/// testable: the process's own `PATH` cannot be changed from a test without
/// racing every other thread in the binary, and on Windows the search is
/// otherwise unreachable -- a host with no `qmake` also has no Qt DLLs, so the
/// test binary would not load far enough to run this.
///
/// `None`, and a `PATH` with nothing on it, are both "no Qt here".
pub fn path_has_qmake(path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| {
        // Both spellings on both platforms rather than `cfg!(windows)`: an
        // extensionless `qmake` beside `qmake.exe` costs one `is_file` and
        // removes a way for this to be wrong under WSL or MSYS.
        ["qmake.exe", "qmake"]
            .iter()
            .any(|name| dir.join(name).is_file())
    })
}
