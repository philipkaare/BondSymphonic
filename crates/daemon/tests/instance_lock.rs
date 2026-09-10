//! One daemon per data directory.
//!
//! Two daemons over one data directory would each rewrite the other's registry
//! and agent records, restore the other's workspaces into sandboxes of their
//! own, and race each other's merges and destroys over the same object stores.
//! The second one has to refuse to start, say so, and say so in a way the IDE's
//! launcher can tell from an ordinary crash — because the launcher restarts a
//! daemon that exits, and restarting cannot fix this.
//!
//! Everything here runs against a temporary data directory of the test's own,
//! with `--no-sandbox`, so no real daemon, workspace or sandbox is involved.

use bondsymphonic_daemon::daemon::{InstanceLock, InstanceLockError};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The daemon binary cargo just built for this test.
const DAEMON: &str = env!("CARGO_BIN_EXE_bondsymphonic-daemon");

/// A daemon child that is killed however the test ends.
///
/// The daemon exits when its stdin closes, but a test that panics unwinds
/// before the drop of `Child` (which does not kill), so without this a failed
/// assertion would leave a daemon holding the lock and every later run of this
/// test would fail for the wrong reason.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(data_dir: &Path) -> Daemon {
    Daemon(
        Command::new(DAEMON)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--no-sandbox")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

/// Blocks until the daemon prints its one stdout line, which is the point at
/// which it is up and holding whatever it holds.
fn wait_until_serving(d: &mut Daemon) -> String {
    let stdout = d.0.stdout.take().expect("stdout is piped");
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    assert!(
        line.contains("\"port\""),
        "the daemon must announce its port: {line:?}"
    );
    line
}

/// Runs a second daemon on the same data directory to completion, answering its
/// exit code and stderr.
fn start_and_wait(data_dir: &Path) -> (Option<i32>, String) {
    let out = Command::new(DAEMON)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--no-sandbox")
        // Closed at once, which is the daemon's own "the IDE is gone" signal:
        // a daemon that *did* take the lock exits 0 promptly instead of hanging
        // this test until a timeout.
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_second_daemon_on_one_data_dir_refuses_to_start_and_says_who_has_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");

    let mut first = spawn(&data);
    wait_until_serving(&mut first);
    assert!(
        InstanceLock::path_in(&data).exists(),
        "the lock file belongs beside the registry"
    );

    let (code, stderr) = start_and_wait(&data);
    assert_eq!(
        code,
        Some(2),
        "the second daemon must exit 2, not crash and not serve: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "another bondsymphonic-daemon owns {}",
            data.display()
        )),
        "the reason must name the data directory: {stderr:?}"
    );

    // The claim is the running process, not the file: once the first daemon is
    // gone the directory is free again, with nothing to clean up by hand.
    drop(first);
    let free = Instant::now();
    loop {
        match InstanceLock::acquire(&data) {
            Ok(_) => break,
            Err(e) => {
                assert!(
                    free.elapsed() < Duration::from_secs(30),
                    "the lock was never released: {e}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    // And a daemon started now gets as far as serving.
    let mut third = spawn(&data);
    wait_until_serving(&mut third);
}

/// The other half of the same rule, without a second process: a data directory
/// this test holds is one the daemon binary will not take.
///
/// Kept separate because it pins the *message and the code* against a lock held
/// by something that is demonstrably not a daemon, so a failure here is about
/// the refusal path alone and not about the first daemon's startup.
#[test]
fn a_daemon_refuses_a_data_dir_locked_by_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let held = InstanceLock::acquire(&data).expect("a fresh directory is free");

    let (code, stderr) = start_and_wait(&data);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("another bondsymphonic-daemon owns"),
        "{stderr:?}"
    );

    // A second `acquire` in this process is refused for the same reason, and
    // says so as `Busy` rather than as an io failure.
    match InstanceLock::acquire(&data) {
        Err(InstanceLockError::Busy(d)) => assert_eq!(d, data),
        Err(e) => panic!("expected Busy, got {e:?}"),
        Ok(_) => panic!("expected Busy, got the lock"),
    }

    drop(held);
    InstanceLock::acquire(&data).expect("released when the handle goes");
}
