//! The Windows file service: resolve the path against the root, then use it.
//!
//! The daemon runs on Windows only for the test suite, against a worktree no
//! sandboxed agent is writing to, so there is no adversary to race and
//! [`super::resolve`]'s check-then-use is enough. The unix service beside this
//! one cannot make that assumption and does not; see [`super::unix`].

use super::*;
use crate::util::fs_retry::is_sharing_violation;

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
pub(super) fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
    let dir = resolve(root, rel)?;
    if !dir.is_dir() {
        return Err(RpcError::invalid_params("not a directory"));
    }
    let mut entries: Vec<FileEntry> = std::fs::read_dir(&dir)
        .map_err(|e| RpcError::io(&e))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                return None;
            }
            // `DirEntry::metadata` does not follow symlinks, so a symlinked
            // directory would otherwise be reported as a file sized by the
            // link itself. Follow the link with `std::fs::metadata`; if the
            // target is missing (a dangling symlink), fall back to the link's
            // own metadata just to confirm it exists, and list it as a
            // zero-size file.
            let (is_dir, size) = match std::fs::metadata(e.path()) {
                Ok(md) => (md.is_dir(), if md.is_dir() { 0 } else { md.len() }),
                Err(_) => {
                    e.metadata().ok()?;
                    (false, 0)
                }
            };
            Some(FileEntry {
                name,
                is_dir,
                size,
                status: FileStatus::Unchanged,
            })
        })
        .collect();
    sort_entries(&mut entries);
    Ok(ListDirResult { entries })
}

/// Reads the file at `rel`. UTF-8 content above [`MAX_READ`] is truncated at
/// the last valid char boundary; non-UTF-8 content is reported as binary
/// with empty content.
pub(super) fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    let path = resolve(root, rel)?;
    if !path.exists() {
        return Err(RpcError::not_found(rel));
    }
    let file = std::fs::File::open(&path).map_err(|e| RpcError::io(&e))?;
    read_capped(file)
}

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed. Content above [`MAX_READ`] is refused; see
/// [`check_write_size`].
///
/// The temp file is removed if anything fails after it is created.
pub(super) fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    let path = resolve(root, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(tmp_name(name));

    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RpcError::io(&e));
    }
    if let Err(e) = rename_with_retry(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RpcError::io(&e));
    }
    Ok(Empty {})
}

/// Renames `from` to `to`, retrying briefly when the destination is
/// transiently locked by another process or thread's own open handle on the
/// same path - most often another writer's concurrent temp-file rename to this
/// same destination. Any other error, or persistence past the budget, is
/// returned as-is.
///
/// What counts as "somebody still has it open" is
/// [`crate::util::fs_retry::is_sharing_violation`], shared with the worktree
/// removal that waits for the same Windows behaviour. The budget is not
/// shared, and deliberately: 20 attempts 5 ms apart, because what this waits
/// for is another writer's rename onto this name, which is over in well under
/// a millisecond. The removal waits for the kernel to reap a killed process's
/// handles, which is slower and gets a second; `fs_retry` says why.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempts < 20 && is_sharing_violation(&e) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}
