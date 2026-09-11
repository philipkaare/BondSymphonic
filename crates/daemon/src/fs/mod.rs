//! Worktree file service: `fs.list_dir` / `fs.read_file` / `fs.write_file`,
//! with strict path containment so a workspace's client can never read or
//! write outside its worktree.
//!
//! The worktree is read-write for the sandboxed agent, so the containment has
//! to hold against an adversary that is changing the tree *while* a request
//! runs. On unix that rules out any check-then-open: a directory can be
//! swapped for a symlink between the two. Instead the path is walked one
//! component at a time from an open handle on the root, each step an `openat`
//! that the kernel refuses to follow through a symlink, so what is opened is
//! exactly what was checked. On Windows, where the daemon exists only for the
//! test suite, the path is canonicalised and checked against the root.
//!
//! A symlink refused on the way and a path that left the worktree are two
//! different answers and are worded as two: the walk refuses every link,
//! in-tree ones included, so most refusals are not escapes and must not be
//! reported as one. `list_dir` matches, listing a link as a plain entry rather
//! than as a folder nothing can then open.
//!
//! What the walk pins is the *inode*, not the path. A directory renamed out of
//! the worktree between two steps of the walk is still the directory the
//! daemon holds a handle to, so a save that was checked as `wt/a/f` can land in
//! `/outside/a/f` if the agent moves `wt/a` there while the request is running.
//! That is not the containment failure the symlink case is: only something
//! already inside the worktree, with write access to it, can arrange it, and it
//! could have copied the file out itself instead. `openat2(RESOLVE_BENEATH)`
//! behaves the same way, so there is nothing to upgrade to here. Read "what is
//! opened is exactly what was checked" with that in mind: it is a statement
//! about the object, not about where the object still lives.

use bondsymphonic_proto::*;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_READ: usize = 4 * 1024 * 1024;

/// The one thing the containment error ever says, so a client can tell an
/// escape from a missing file without parsing prose.
const ESCAPES: &str = "path escapes the worktree";

/// Checks the shape of `rel` without touching the filesystem: no NUL bytes,
/// not absolute, no `..`. Everything the service resolves starts here.
fn check_relative(rel: &str) -> Result<&Path, RpcError> {
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
    Ok(p)
}

/// Resolves `rel` against `root`, rejecting absolute paths, `..` components
/// and NUL bytes, then canonicalizing the deepest existing ancestor to
/// defeat symlink escapes (the result must still start with the canonical
/// root).
///
/// This is a *check*, and the path it returns can be swapped for a symlink
/// before a caller opens it, so the file service itself does not use it on
/// unix (see the module documentation). It remains the answer for callers that
/// need a path rather than a handle — a `git` invocation, say — and for the
/// Windows service, where the tree has no adversary.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let p = check_relative(rel)?;
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
            Ok(_) => return Err(RpcError::invalid_params(ESCAPES)),
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

/// Directories first, then case-insensitive by name.
fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Applies [`read_file`]'s content policy to bytes that came from somewhere else
/// — a git blob, say — so every text the daemon hands a client is classified the
/// same way, whichever side of a diff it came from.
///
/// `bytes` is expected to hold at most `MAX_READ + 1` bytes, the extra byte being
/// what distinguishes content that is exactly `MAX_READ` long from longer content;
/// anything past the cap is dropped here rather than sent.
pub fn classify(bytes: &[u8]) -> ReadFileResult {
    let truncated = bytes.len() > MAX_READ;
    let slice = if truncated { &bytes[..MAX_READ] } else { bytes };
    match std::str::from_utf8(slice) {
        Ok(s) => ReadFileResult {
            content: s.to_string(),
            encoding: "utf-8".into(),
            truncated,
        },
        Err(e)
            if truncated
                && e.valid_up_to() > 0
                && slice[..e.valid_up_to()].len() + 4 >= slice.len() =>
        {
            ReadFileResult {
                content: std::str::from_utf8(&slice[..e.valid_up_to()])
                    .unwrap()
                    .to_string(),
                encoding: "utf-8".into(),
                truncated: true,
            }
        }
        Err(_) => ReadFileResult {
            content: String::new(),
            encoding: "binary".into(),
            truncated,
        },
    }
}

/// Reads at most `MAX_READ + 1` bytes from an open file and classifies them.
///
/// The extra byte is what distinguishes a file that is exactly `MAX_READ` long
/// from a longer one. Reading the whole file first would let one
/// multi-gigabyte build artifact or core dump in a worktree take the daemon,
/// and every other workspace with it.
fn read_capped(file: std::fs::File) -> Result<ReadFileResult, RpcError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(MAX_READ as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RpcError::io(&e))?;
    Ok(classify(&bytes))
}

/// Refuses content no `fs.write_file` should be carrying, before anything is
/// opened.
///
/// The service reads at most [`MAX_READ`], so a save larger than that is a save
/// of something this daemon never handed out. Left uncapped it is not merely a
/// large write: the request line carrying it is JSON, where every control
/// character costs six bytes, so content past a few megabytes overruns
/// [`crate::server::connection::MAX_FRAME`] and the connection is dropped with
/// no reply at all. An error naming the limit is the difference between a save
/// the user can fix and an editor that silently loses its daemon.
fn check_write_size(content: &str) -> Result<(), RpcError> {
    if content.len() > MAX_READ {
        return Err(RpcError::invalid_params(format!(
            "content is {} bytes; a file this service writes may be at most {MAX_READ}",
            content.len()
        )));
    }
    Ok(())
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The temp file a write of `name` goes through: unique per call (pid + a
/// process-wide counter) so concurrent writers to the same path never share
/// one temp file and race on its rename.
fn tmp_name(name: &str) -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{name}.{}.{counter}.bs-tmp", std::process::id())
}

/// The service itself: the same three calls on either platform, with the
/// `#[cfg]` boundary at the file boundary rather than scattered through one.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix as imp;
#[cfg(windows)]
use self::windows as imp;

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
pub fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
    imp::list_dir(root, rel)
}

/// Reads the file at `rel`. UTF-8 content above [`MAX_READ`] is truncated at
/// the last valid char boundary; non-UTF-8 content is reported as binary
/// with empty content.
pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    imp::read_file(root, rel)
}

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed and, on unix, preserving the existing file's mode.
/// Content above [`MAX_READ`] is refused; see [`check_write_size`].
pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    check_write_size(content)?;
    imp::write_file(root, rel, content)
}
