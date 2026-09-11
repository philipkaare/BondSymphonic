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

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The temp file a write of `name` goes through: unique per call (pid + a
/// process-wide counter) so concurrent writers to the same path never share
/// one temp file and race on its rename.
fn tmp_name(name: &str) -> String {
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{name}.{}.{counter}.bs-tmp", std::process::id())
}

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
#[cfg(unix)]
pub fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
    unix::list_dir(root, rel)
}

/// Reads the file at `rel`. UTF-8 content above [`MAX_READ`] is truncated at
/// the last valid char boundary; non-UTF-8 content is reported as binary
/// with empty content.
#[cfg(unix)]
pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    unix::read_file(root, rel)
}

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed and preserving the existing file's mode.
#[cfg(unix)]
pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    unix::write_file(root, rel, content)
}

/// The handle-based service: every component is opened from the handle of
/// the one before it, with `O_NOFOLLOW`, so a symlink anywhere on the path is
/// refused by the kernel at the moment of the open. There is no window between
/// a check and a use because there is no check: the open *is* the check.
///
/// The cost is that a symlink inside the worktree is refused too, even one
/// that points at another file inside it. Following it safely would mean
/// resolving its target through the same walk, which is what
/// `openat2(RESOLVE_BENEATH)` does in the kernel on Linux 5.6+; that is the
/// upgrade path if in-tree symlinks turn out to matter.
#[cfg(unix)]
mod unix {
    use super::*;
    use nix::errno::Errno;
    use nix::fcntl::{openat, renameat, AtFlags, OFlag};
    use nix::sys::stat::{fchmod, fstatat, mkdirat, Mode, SFlag};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::ffi::{OsStr, OsString};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    fn io_err(e: Errno) -> RpcError {
        RpcError::io(&std::io::Error::from_raw_os_error(e as i32))
    }

    fn escapes() -> RpcError {
        RpcError::invalid_params(ESCAPES)
    }

    /// What the kernel answers when `O_NOFOLLOW` meets a symlink: `ELOOP` on
    /// Linux, `EMLINK` on some BSDs. With `O_DIRECTORY` as well, Linux says
    /// `ENOTDIR` instead, the same as for a plain file; see [`open_child_dir`].
    fn is_symlink_refusal(e: Errno) -> bool {
        matches!(e, Errno::ELOOP | Errno::EMLINK)
    }

    /// Whether `name` under `parent` is a symlink right now. Only ever used to
    /// word an error the kernel has already decided: the open was refused
    /// either way, so a stale answer here costs a message, not containment.
    fn is_symlink(parent: &OwnedFd, name: &OsStr) -> bool {
        fstatat(Some(parent.as_raw_fd()), name, AtFlags::AT_SYMLINK_NOFOLLOW)
            .is_ok_and(|st| SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFLNK)
    }

    /// The worktree root itself, opened as a directory. This is the one open
    /// that follows symlinks: the root is the daemon's own path, not one the
    /// agent chose.
    fn open_root(root: &Path) -> Result<OwnedFd, RpcError> {
        let fd = nix::fcntl::open(
            root,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_err)?;
        // SAFETY: `open` just returned this descriptor and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Opens the directory `name` directly under `parent`, refusing a symlink
    /// in its place. With `create`, a missing directory is made first; the
    /// open is then retried rather than trusted, because the agent can replace
    /// what was just made before it is opened.
    fn open_child_dir(parent: &OwnedFd, name: &OsStr, create: bool) -> Result<OwnedFd, RpcError> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        // Making a directory and opening it are two steps, and the agent owns
        // the tree between them: it can delete what was just made, and the open
        // is then back where it started. Retrying is right, retrying forever is
        // not -- an agent deleting in a loop would pin a request task on a core
        // for as long as it cared to. A tree that has changed shape this many
        // times under one request is not a tree a save can be completed in.
        const ATTEMPTS: u32 = 32;
        for _ in 0..ATTEMPTS {
            match openat(Some(parent.as_raw_fd()), name, flags, Mode::empty()) {
                // SAFETY: `openat` just returned this descriptor and nothing else owns it.
                Ok(fd) => return Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
                Err(e) if is_symlink_refusal(e) => return Err(escapes()),
                // A symlink and a plain file in the way both come back as
                // `ENOTDIR`; the one that was an escape attempt is named as one.
                Err(Errno::ENOTDIR) if is_symlink(parent, name) => return Err(escapes()),
                Err(Errno::ENOTDIR) => return Err(RpcError::invalid_params("not a directory")),
                Err(Errno::ENOENT) if create => {
                    match mkdirat(
                        Some(parent.as_raw_fd()),
                        name,
                        Mode::from_bits_truncate(0o777),
                    ) {
                        Ok(()) | Err(Errno::EEXIST) => continue,
                        Err(e) => return Err(io_err(e)),
                    }
                }
                Err(Errno::ENOENT) => {
                    return Err(RpcError::not_found(name.to_string_lossy().into_owned()))
                }
                Err(e) => return Err(io_err(e)),
            }
        }
        Err(RpcError::io(&std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "{} kept changing shape under the write",
                name.to_string_lossy()
            ),
        )))
    }

    /// Walks from `root` to the directory that holds the last component of
    /// `rel`, and returns that directory's handle together with the last
    /// component's name (`None` when `rel` names the root itself). With
    /// `create`, directories missing along the way are made.
    fn open_parent(
        root: &Path,
        rel: &str,
        create: bool,
    ) -> Result<(OwnedFd, Option<OsString>), RpcError> {
        let p = check_relative(rel)?;
        let mut names: Vec<&OsStr> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n),
                _ => None,
            })
            .collect();
        let last = names.pop().map(OsStr::to_os_string);
        let mut dir = open_root(root)?;
        for name in names {
            dir = open_child_dir(&dir, name, create).map_err(|e| {
                // A missing intermediate directory is the whole path missing.
                if e.code == ErrorCode::NotFound {
                    RpcError::not_found(rel)
                } else {
                    e
                }
            })?;
        }
        Ok((dir, last))
    }

    /// An open directory stream over a handle, closed on drop. `libc` rather
    /// than a path-based `read_dir`, because a path is exactly what cannot be
    /// trusted here; the handle already is the directory that was checked.
    struct DirStream(*mut libc::DIR);

    impl DirStream {
        /// Takes ownership of `fd`: `fdopendir` adopts the descriptor and
        /// `closedir` releases it.
        fn open(fd: OwnedFd) -> Result<Self, RpcError> {
            use std::os::fd::IntoRawFd;
            let raw = fd.into_raw_fd();
            // SAFETY: `raw` is a valid, open directory descriptor we now own.
            let dirp = unsafe { libc::fdopendir(raw) };
            if dirp.is_null() {
                let e = std::io::Error::last_os_error();
                // SAFETY: `fdopendir` failed and left the descriptor with us.
                unsafe { libc::close(raw) };
                return Err(RpcError::io(&e));
            }
            Ok(Self(dirp))
        }

        fn fd(&self) -> std::os::fd::RawFd {
            // SAFETY: `self.0` is a live stream from `fdopendir`.
            unsafe { libc::dirfd(self.0) }
        }

        /// The next entry's name, or `None` at the end.
        fn next_name(&mut self) -> Option<OsString> {
            loop {
                // SAFETY: `self.0` is a live stream; `readdir` returns either
                // null or a pointer valid until the next call on this stream.
                let ent = unsafe { libc::readdir(self.0) };
                if ent.is_null() {
                    return None;
                }
                // SAFETY: `d_name` is a NUL-terminated array inside `*ent`.
                let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
                let bytes = name.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                return Some(OsStr::from_bytes(bytes).to_os_string());
            }
        }
    }

    impl Drop for DirStream {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from `fdopendir` and is closed exactly once.
            unsafe { libc::closedir(self.0) };
        }
    }

    pub(super) fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
        let (parent, name) = open_parent(root, rel, false)?;
        let dir = match name {
            None => parent,
            Some(name) => open_child_dir(&parent, &name, false).map_err(|e| {
                if e.code == ErrorCode::NotFound {
                    RpcError::not_found(rel)
                } else {
                    e
                }
            })?,
        };
        let mut stream = DirStream::open(dir)?;
        let dirfd = stream.fd();
        let mut entries = Vec::new();
        while let Some(os_name) = stream.next_name() {
            if os_name.as_bytes() == b".git" {
                continue;
            }
            // Stat through the handle, following a symlink so a linked
            // directory is listed as one rather than as a file the size of the
            // link; a dangling link falls back to the link's own metadata just
            // to confirm it exists, and is listed as a zero-size file.
            let (is_dir, size) = match fstatat(Some(dirfd), os_name.as_os_str(), AtFlags::empty()) {
                Ok(st) => {
                    let is_dir = SFlag::from_bits_truncate(st.st_mode).contains(SFlag::S_IFDIR);
                    (is_dir, if is_dir { 0 } else { st.st_size as u64 })
                }
                Err(_) => {
                    if fstatat(
                        Some(dirfd),
                        os_name.as_os_str(),
                        AtFlags::AT_SYMLINK_NOFOLLOW,
                    )
                    .is_err()
                    {
                        continue;
                    }
                    (false, 0)
                }
            };
            entries.push(FileEntry {
                name: os_name.to_string_lossy().into_owned(),
                is_dir,
                size,
                status: FileStatus::Unchanged,
            });
        }
        sort_entries(&mut entries);
        Ok(ListDirResult { entries })
    }

    pub(super) fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
        let (dir, name) = open_parent(root, rel, false)?;
        let Some(name) = name else {
            return Err(RpcError::invalid_params("not a file"));
        };
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let fd = match openat(
            Some(dir.as_raw_fd()),
            name.as_os_str(),
            flags,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(e) if is_symlink_refusal(e) => return Err(escapes()),
            Err(Errno::ENOENT) => return Err(RpcError::not_found(rel)),
            Err(e) => return Err(io_err(e)),
        };
        // SAFETY: `openat` just returned this descriptor and nothing else owns it.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        read_capped(file)
    }

    pub(super) fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
        use std::io::Write;
        let (dir, name) = open_parent(root, rel, true)?;
        let Some(name) = name else {
            return Err(RpcError::invalid_params("not a file"));
        };
        let dirfd = Some(dir.as_raw_fd());

        // The mode to carry over from the file being replaced. A symlink in
        // its place is refused like one anywhere else on the path; should one
        // appear between this stat and the rename below, the rename replaces
        // the link itself and the file still lands here, inside the worktree.
        let existing_mode = match fstatat(dirfd, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) => {
                let kind = SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT;
                if kind == SFlag::S_IFLNK {
                    return Err(escapes());
                }
                (kind == SFlag::S_IFREG).then(|| Mode::from_bits_truncate(st.st_mode & 0o7777))
            }
            Err(Errno::ENOENT) => None,
            Err(e) => return Err(io_err(e)),
        };

        let tmp = tmp_name(&name.to_string_lossy());
        let flags =
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let fd =
            openat(dirfd, tmp.as_str(), flags, Mode::from_bits_truncate(0o666)).map_err(io_err)?;
        // SAFETY: `openat` just returned this descriptor and nothing else owns it.
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let discard = || {
            let _ = unlinkat(dirfd, tmp.as_str(), UnlinkatFlags::NoRemoveDir);
        };

        if let Err(e) = file.write_all(content.as_bytes()) {
            discard();
            return Err(RpcError::io(&e));
        }
        if let Some(mode) = existing_mode {
            let _ = fchmod(file.as_raw_fd(), mode);
        }
        drop(file);
        if let Err(e) = renameat(dirfd, tmp.as_str(), dirfd, name.as_os_str()) {
            discard();
            return Err(io_err(e));
        }
        Ok(Empty {})
    }
}

/// Lists the directory at `rel`, directories first, then case-insensitive by
/// name. Skips `.git`. `status` is always `Unchanged` at this milestone.
#[cfg(windows)]
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
#[cfg(windows)]
pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    let path = resolve(root, rel)?;
    if !path.exists() {
        return Err(RpcError::not_found(rel));
    }
    let file = std::fs::File::open(&path).map_err(|e| RpcError::io(&e))?;
    read_capped(file)
}

/// Writes `content` to `rel` via a temp file + rename, creating parent
/// directories as needed.
///
/// The temp file is removed if anything fails after it is created.
#[cfg(windows)]
pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
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
/// transiently locked (a sharing or lock violation, or the `PermissionDenied` that
/// wraps them) by another process or thread's own open handle on the same path -
/// most often another writer's concurrent temp-file rename to this same destination.
/// Up to 20 attempts, 5 ms apart, so a brief hold clears without surfacing an error to
/// the caller; any other error, or persistence past 20 attempts, is returned as-is.
#[cfg(windows)]
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

#[cfg(windows)]
fn is_sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
        || matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5))
}
