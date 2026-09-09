//! Worktree file service: `fs.list_dir` / `fs.read_file` / `fs.write_file`,
//! with strict path containment so a workspace's client can never read or
//! write outside its worktree.

use bondsymphonic_proto::*;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_READ: usize = 4 * 1024 * 1024;

/// Resolves `rel` against `root`, rejecting absolute paths, `..` components
/// and NUL bytes, then canonicalizing the deepest existing ancestor to
/// defeat symlink escapes (the result must still start with the canonical
/// root).
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    if rel.contains('\0') {
        return Err(RpcError::invalid_params("path contains NUL"));
    }
    let p = Path::new(rel);
    if p.is_absolute()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || p.components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Err(RpcError::invalid_params(
            "path must be relative to the worktree",
        ));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(RpcError::invalid_params("path may not contain '..'"));
    }
    let joined = root.join(p);
    let root_c = root.canonicalize().map_err(|e| RpcError::io(&e))?;

    // Canonicalizing the deepest existing ancestor opens a handle on it, which races a
    // concurrent rename onto that same path (e.g. two `write_file` calls to the same
    // destination): on Windows, if the file we open a handle on is unlinked by another
    // thread's rename between our open and `GetFinalPathNameByHandleW`, NTFS resolves the
    // (now-orphaned, still-open) handle to a `$Extend\$Deleted\...` path instead of the
    // real one, and containment fails for a path that was never actually outside the
    // worktree. Retry briefly on Windows before treating that as a real escape; a genuine
    // symlink escape resolves the same way every time and still fails after the retries.
    let mut attempts = 0;
    loop {
        match resolve_existing_ancestor(&joined) {
            Ok(canon) if canon.starts_with(&root_c) => return Ok(canon),
            _ if cfg!(windows) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(_) => return Err(RpcError::invalid_params("path escapes the worktree")),
            Err(e) => return Err(RpcError::io(&e)),
        }
    }
}

/// Canonicalizes the deepest existing ancestor of `joined` (defeating symlink escapes)
/// and re-appends the remaining, not-yet-existing path components literally.
fn resolve_existing_ancestor(joined: &Path) -> std::io::Result<PathBuf> {
    let mut probe = joined.to_path_buf();
    let mut tail = Vec::new();
    while !probe.exists() {
        tail.push(
            probe
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_default(),
        );
        if !probe.pop() {
            break;
        }
    }
    let mut canon = probe.canonicalize()?;
    for t in tail.into_iter().rev() {
        canon.push(t);
    }
    Ok(canon)
}

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
pub fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
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
            // `DirEntry::metadata` does not follow symlinks (it is `lstat`
            // under the hood), so a symlinked directory would otherwise be
            // reported as a file sized by the link itself. Follow the link
            // with `std::fs::metadata`; if the target is missing (a
            // dangling symlink), fall back to the link's own metadata just
            // to confirm it exists, and list it as a zero-size file.
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
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(ListDirResult { entries })
}

/// Reads the file at `rel`. UTF-8 content above [`MAX_READ`] is truncated at
/// the last valid char boundary; non-UTF-8 content is reported as binary
/// with empty content.
///
/// At most `MAX_READ + 1` bytes ever reach memory: the extra byte is what
/// distinguishes a file that is exactly `MAX_READ` long from a longer one.
/// Reading the whole file first would let one multi-gigabyte build artifact or
/// core dump in a worktree take the daemon, and every other workspace with it.
pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    use std::io::Read;
    let path = resolve(root, rel)?;
    if !path.exists() {
        return Err(RpcError::not_found(rel));
    }
    let file = std::fs::File::open(&path).map_err(|e| RpcError::io(&e))?;
    let mut bytes = Vec::new();
    file.take(MAX_READ as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RpcError::io(&e))?;
    let truncated = bytes.len() > MAX_READ;
    let slice = if truncated {
        &bytes[..MAX_READ]
    } else {
        &bytes[..]
    };
    match std::str::from_utf8(slice) {
        Ok(s) => Ok(ReadFileResult {
            content: s.to_string(),
            encoding: "utf-8".into(),
            truncated,
        }),
        Err(e)
            if truncated
                && e.valid_up_to() > 0
                && slice[..e.valid_up_to()].len() + 4 >= slice.len() =>
        {
            Ok(ReadFileResult {
                content: std::str::from_utf8(&slice[..e.valid_up_to()])
                    .unwrap()
                    .to_string(),
                encoding: "utf-8".into(),
                truncated: true,
            })
        }
        Err(_) => Ok(ReadFileResult {
            content: String::new(),
            encoding: "binary".into(),
            truncated,
        }),
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed and preserving the existing file's mode on unix.
///
/// The temp file name is unique per call (pid + a process-wide counter) so
/// concurrent writers to the same path never share one temp file and race
/// on its rename; the temp file is removed if anything fails after it is
/// created.
pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    let path = resolve(root, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{name}.{}.{counter}.bs-tmp", std::process::id()));

    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RpcError::io(&e));
    }
    #[cfg(unix)]
    if let Ok(md) = std::fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &tmp,
            std::fs::Permissions::from_mode(md.permissions().mode()),
        );
    }
    if let Err(e) = rename_with_retry(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RpcError::io(&e));
    }
    Ok(Empty {})
}

/// Renames `from` to `to`, retrying briefly on Windows when the destination is
/// transiently locked (a sharing or lock violation, or the `PermissionDenied` that
/// wraps them) by another process or thread's own open handle on the same path -
/// most often another writer's concurrent temp-file rename to this same destination.
/// Up to 20 attempts, 5 ms apart, so a brief hold clears without surfacing an error to
/// the caller; any other error, or persistence past 20 attempts, is returned as-is.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if cfg!(windows) && attempts < 20 && is_sharing_violation(&e) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
        || matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5))
}
