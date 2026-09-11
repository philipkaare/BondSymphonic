//! Filesystem operations that have to wait for Windows to let go.
//!
//! On Windows a file cannot be unlinked while somebody holds a handle on it
//! without the delete share, and a process that has just been killed keeps its
//! handles until the kernel reaps them. That is a window measured in
//! milliseconds and it clears on its own, so the answer is to try again rather
//! than to report a failure the user can do nothing about. Unix has no such
//! state: an open file is unlinked immediately and the handle simply outlives
//! the name.

use std::path::Path;
use std::time::Duration;

/// True when an I/O error is Windows saying somebody else still has the file
/// open, rather than saying the operation itself was wrong.
///
/// `ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33) say it
/// outright; `ERROR_ACCESS_DENIED` (5) is what a delete gets when the file is
/// already marked for deletion by somebody else's handle, and
/// `PermissionDenied` is the `ErrorKind` the standard library maps these to on
/// the paths where it does not keep the raw code.
///
/// Always false off Windows, which is what collapses every retry below to a
/// single attempt there.
#[cfg(windows)]
pub fn is_sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
        || matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5))
}

#[cfg(not(windows))]
pub fn is_sharing_violation(_: &std::io::Error) -> bool {
    false
}

/// How long a delete keeps trying before it agrees the directory is not going
/// anywhere.
///
/// Longer than the 5 ms `crate::fs`'s rename retry uses, because the two wait
/// for different things. That one races another writer's own temp-file rename
/// onto the same name, which is over in well under a millisecond. This one
/// waits for the operating system to reap the handles of a process the daemon
/// killed moments earlier — `workspace.destroy` stops the sandbox and its
/// agents a few lines before it removes the worktree — and that is slower and
/// less predictable. A second is the whole budget, and it is spent only in the
/// case where the alternative is failing the destroy outright.
const ATTEMPTS: u32 = 40;
const PAUSE: Duration = Duration::from_millis(25);

/// Deletes `path` and everything under it.
///
/// "It is not there" is success: every caller wants the directory gone, and a
/// caller that also wants to know whether it had been there asks beforehand.
/// A [`is_sharing_violation`] is waited out; anything else is returned at once,
/// because retrying a permission error or a path that is not a directory only
/// delays the report.
///
/// Async because the pause between attempts must not block a runtime worker.
/// The delete itself is a blocking syscall, like every other `std::fs` call in
/// the removal path it serves.
pub async fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempts < ATTEMPTS && is_sharing_violation(&e) => {
                attempts += 1;
                tokio::time::sleep(PAUSE).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that is not there is already in the state the caller wanted.
    #[tokio::test]
    async fn a_directory_that_is_not_there_is_success() {
        let dir = tempfile::tempdir().unwrap();
        remove_dir_all(&dir.path().join("never-existed"))
            .await
            .unwrap();
    }

    /// And one that is there goes, with everything under it.
    #[tokio::test]
    async fn a_directory_that_is_there_goes_with_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree").join("deep");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("file.txt"), "x").unwrap();

        remove_dir_all(&dir.path().join("tree")).await.unwrap();

        assert!(!dir.path().join("tree").exists());
    }

    /// A failure that is not somebody else's handle comes back at once rather
    /// than after the whole budget: retrying it would only delay the report.
    #[tokio::test]
    async fn a_path_that_is_a_file_fails_without_burning_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").unwrap();

        let started = std::time::Instant::now();
        let err = remove_dir_all(&file).await.unwrap_err();

        assert!(
            started.elapsed() < ATTEMPTS * PAUSE,
            "a file where a directory was named waited out the retry budget: {err}"
        );
    }
}
