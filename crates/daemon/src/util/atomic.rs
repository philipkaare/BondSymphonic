//! Replacing a file's whole content without ever leaving half of it on disk.
//!
//! Every small state file the daemon owns — the workspace registry, the agent
//! records — is rewritten in full on each change, from a process the IDE kills
//! by closing its stdin and the operating system kills on shutdown. The file
//! that survives that has to be either the old one or the new one.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes two temporaries created in the same millisecond by the same
/// process. The pid separates processes; this separates writers inside one.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The sibling temporary [`write_atomic`] will write `path` through.
///
/// Unique per call, and deliberately not `<name>.tmp`: a fixed name is shared
/// by every writer of that file and by every earlier run of the daemon, so two
/// saves at once can rename each other's half-written file into place, and a
/// leftover directory or a read-only file at that name wedges the writer for
/// good. A sibling rather than a temp directory, because `rename` is only
/// atomic within one filesystem.
pub fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = format!(".{name}.{}.{n}.tmp", std::process::id());
    match path.parent() {
        Some(dir) => dir.join(tmp),
        None => PathBuf::from(tmp),
    }
}

/// Writes `bytes` to `path`, atomically as far as the platform allows.
///
/// The content goes to a unique sibling temporary, is flushed all the way to
/// the device with `sync_all`, and only then replaces `path` with a rename. A
/// process that dies at any point leaves either the previous file or the new
/// one, never a truncated one and never a half-written one — which is what a
/// plain `File::create` + `write_all` cannot promise, since the truncation
/// happens first and the bytes land whenever the page cache gets to them.
///
/// The directory entry is fsynced afterwards on Unix, best effort: without it
/// the rename itself can be lost across a power cut even though the file's own
/// data was synced. Windows has no portable equivalent and `ReplaceFile`
/// semantics already order the metadata, so the step is simply absent there.
///
/// The temporary is removed when anything fails, so a failed write leaves the
/// directory exactly as it found it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = temp_path(path);
    match write_and_rename(&tmp, path, bytes) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        // Best effort: a filesystem that refuses to open a directory for
        // reading (or has no such concept) is not a reason to report a write
        // that did happen as a failure.
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

fn write_and_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    {
        let mut f = File::create(tmp)?;
        f.write_all(bytes)?;
        // Before the rename, not after: the rename is what publishes the file,
        // so the bytes have to be on the device by the time it happens.
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_atomic(&path, b"second, and longer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second, and longer");

        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["state.json".to_string()], "{left:?}");
    }

    #[test]
    fn the_parent_directory_is_created_if_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("state.json");
        write_atomic(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    /// Two writers of the same file must not share a temporary: with one fixed
    /// name each can rename the other's half-written content into place.
    #[test]
    fn every_call_picks_its_own_temporary_beside_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let a = temp_path(&path);
        let b = temp_path(&path);
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.parent(), path.parent(), "the temporary must be a sibling");
            assert_ne!(t, &path);
            assert!(
                t.to_string_lossy().ends_with(".tmp"),
                "{}",
                t.to_string_lossy()
            );
        }
    }

    /// A leftover at the name an older writer used is not this writer's problem.
    #[test]
    fn a_directory_at_the_old_fixed_temporary_name_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::create_dir(dir.path().join("state.json.tmp")).unwrap();
        write_atomic(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    /// A write that cannot land leaves both the old content and the directory
    /// as they were: no truncated target, no temporary left behind.
    #[test]
    fn a_failed_write_leaves_the_previous_file_and_no_debris() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: `File::create` on the temporary
        // succeeds and the rename over a non-empty directory is refused, which
        // is the failure that happens after bytes have been written.
        let path = dir.path().join("state.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"keep me").unwrap();

        assert!(write_atomic(&path, b"x").is_err());
        assert_eq!(std::fs::read(path.join("occupied")).unwrap(), b"keep me");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["state.json".to_string()], "{left:?}");
    }
}
