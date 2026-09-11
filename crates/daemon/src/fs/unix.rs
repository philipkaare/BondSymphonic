//! The handle-based service: every component is opened from the handle of
//! the one before it, with `O_NOFOLLOW`, so a symlink anywhere on the path is
//! refused by the kernel at the moment of the open. There is no window between
//! a check and a use because there is no check: the open *is* the check.
//!
//! The cost is that a symlink inside the worktree is refused too, even one
//! that points at another file inside it. Following it safely would mean
//! resolving its target through the same walk, which is what
//! `openat2(RESOLVE_BENEATH)` does in the kernel on Linux 5.6+; that is the
//! upgrade path if in-tree symlinks turn out to matter.

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

/// What a symlink anywhere on a path is refused with. Deliberately not
/// [`ESCAPES`]: the walk refuses every link, including one pointing
/// squarely inside the worktree, because it cannot tell the two apart
/// without following the link -- which is the thing it must not do. Telling
/// a user that their own in-tree `docs -> shared/docs` was trying to break
/// out of the worktree would be a false accusation, and it would spend the
/// containment message on the case that is not a containment failure.
const SYMLINK: &str = "symlinks are not followed";

fn not_followed() -> RpcError {
    RpcError::invalid_params(SYMLINK)
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
            Err(e) if is_symlink_refusal(e) => return Err(not_followed()),
            // A symlink and a plain file in the way both come back as
            // `ENOTDIR`; the one that was a link is named as one.
            Err(Errno::ENOTDIR) if is_symlink(parent, name) => return Err(not_followed()),
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
    Err(RpcError::io(&std::io::Error::other(format!(
        "{} kept changing shape under the write",
        name.to_string_lossy()
    ))))
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
        // Stat the entry itself, never what a link points at. Following
        // the link would list a linked directory as a directory, and the
        // client would then be told "symlinks are not followed" for every
        // call that tried to open the folder it had just been shown. A
        // link is listed -- it is really there, and hiding it would be its
        // own lie -- as a plain entry of no size, which is exactly what
        // this service can do with it. That covers a dangling link too,
        // with no second stat to fall back on.
        let Ok(st) = fstatat(
            Some(dirfd),
            os_name.as_os_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) else {
            // Gone between the `readdir` and the stat: not an entry any more.
            continue;
        };
        let kind = SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT;
        let is_dir = kind == SFlag::S_IFDIR;
        let size = if is_dir || kind == SFlag::S_IFLNK {
            0
        } else {
            st.st_size as u64
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
        Err(e) if is_symlink_refusal(e) => return Err(not_followed()),
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
                return Err(not_followed());
            }
            (kind == SFlag::S_IFREG).then(|| Mode::from_bits_truncate(st.st_mode & 0o7777))
        }
        Err(Errno::ENOENT) => None,
        Err(e) => return Err(io_err(e)),
    };

    let tmp = tmp_name(&name.to_string_lossy());
    let flags =
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let fd = openat(dirfd, tmp.as_str(), flags, Mode::from_bits_truncate(0o666)).map_err(io_err)?;
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
