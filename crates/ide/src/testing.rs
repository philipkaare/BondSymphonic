//! The one place the integration tests decide what to do without a Qt runtime.
//!
//! Four suites (`smoke`, `reconnect_tests`, `restore_tests`, `qobject_smoke`)
//! either launch the IDE binary or need the Qt libraries the crate links
//! against. Off a developer machine that has run `scripts\env.ps1` there is no
//! runtime to launch them against, and a hard failure there says nothing about
//! the code.
//!
//! So they skip -- loudly. The skip prints `SKIP: <reason>` on stdout rather
//! than passing silently, and under the `require-qt` cargo feature it is a
//! panic instead. CI's Windows job turns that feature on (Milestone 7 Task 5),
//! which is what stops a machine that lost its Qt installation from reporting a
//! green run in which nothing was actually exercised.

/// Whether the caller should return without doing anything, because the Qt
/// runtime the IDE needs is not on `PATH`.
///
/// `what` names the suite, so a run with several skips says which. Prints
/// `SKIP: <what>: <reason>` and answers `true`; with `require-qt` on it panics
/// with the same reason and never answers at all.
///
/// `QMAKE` is the probe because `scripts\env.ps1` is what sets it, alongside
/// putting `<Qt>\bin` on `PATH`: the two travel together, and the IDE's own
/// `build.rs` reads the same variable.
#[must_use]
pub fn skip_without_qt(what: &str) -> bool {
    if std::env::var_os("QMAKE").is_some() {
        return false;
    }
    let reason = format!(
        "{what}: QMAKE is unset, so the Qt runtime the IDE needs is not on PATH. Dot-source \
         scripts\\env.ps1 and run again."
    );
    if cfg!(feature = "require-qt") {
        panic!("{reason}");
    }
    // stdout, and with the `SKIP:` prefix the milestone's CI greps for. Cargo
    // captures it unless the run asks for `--nocapture`, which is how the
    // Windows job is expected to run when the feature is off.
    println!("SKIP: {reason}");
    true
}
