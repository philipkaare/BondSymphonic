//! In-place workspaces: an agent working directly in a repository's own
//! checkout rather than in a worktree of the daemon's (in-place workspaces
//! design, 2026-09-17).
//!
//! What such a workspace shares with the user is their `.git`, and a `.git` is
//! code: its config names programs, its hooks are programs, and git follows
//! `commondir` to a config somewhere else entirely. The agent gets the git
//! directory read-write, as a mount of its own so it cannot be swapped out,
//! with every one of those entries bound read-only on top. Everything here is
//! the daemon's half of that arrangement, kept in one place so no worktree-only
//! path (`ref_dir`, `worktree_gitdir`, a private object directory) can be
//! reached for an in-place workspace by accident.

use crate::git::repo::{self, RepoKind};
use crate::git::{path_arg, Git};
use bondsymphonic_proto::{ErrorCode, RpcError, WorkspaceId};
use std::path::{Path, PathBuf};

/// The `data.reason` a merge or pull request on an in-place workspace is
/// refused with.
pub const IN_PLACE_REASON: &str = "in_place";

/// What the daemon writes to `.git/commondir` before an agent may work in the
/// checkout.
///
/// Git reads `commondir` from *any* git directory, not only a linked
/// worktree's, and takes config, refs and objects from wherever it points. An
/// agent that could write it could make the user's next `git status` run a
/// `core.fsmonitor` of its own choosing. `.` points the common directory at the
/// git directory itself, which is what it is anyway, and the file is then bound
/// read-only. Measured with git 2.43 and Git for Windows 2.52: status, commit,
/// switch, stash, worktree add and remove, gc and fsck all behave as without it.
pub const COMMONDIR_GUARD: &str = ".\n";

/// The empty tree, which git knows without it being in any object store. What
/// changes are measured against in a repository with no commits yet.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The flag every daemon-side `git status` and `git diff` against a tree an
/// agent can write passes, for both workspace kinds.
///
/// An embedded repository is a directory with a `.git` of its own, and git
/// reads that repository's config whenever it looks inside it to report its
/// state. An agent that runs `git init` in `sub/`, sets `core.fsmonitor` in
/// `sub/.git/config` and commits the gitlink would have every status and diff
/// in the parent run its program -- the daemon's included. `-c
/// diff.ignoreSubmodules=all` does not stop it: an `ignore = none` in a
/// `.gitmodules` the agent writes takes precedence over that setting. The
/// command-line flag takes precedence over both, so git never looks inside the
/// embedded repository at all.
pub const IGNORE_SUBMODULES: &str = "--ignore-submodules=all";

/// The directories under `.git` that git makes only when it needs them, which
/// the daemon makes (and binds read-only) when they are missing, and which
/// Close takes back when the daemon made them and they are still empty.
pub const ON_DEMAND_DIRS: [&str; 3] = ["worktrees", "remotes", "branches"];

/// The paths of one in-place workspace.
#[derive(Debug, Clone)]
pub struct InPlaceLayout {
    /// The repository root, which is also the workspace's `worktree_path`.
    pub root: PathBuf,
    /// `<root>/.git`, always a directory: [`in_place_refusal`] refuses the rest.
    pub git_dir: PathBuf,
    /// An empty daemon-owned directory, used as `core.hooksPath`. See
    /// [`crate::workspace::DataDirs::no_hooks`].
    pub no_hooks_dir: PathBuf,
}

/// True for a directory that is one, not a symlink to one: a bind follows
/// symlinks, and a symlink an earlier session planted must not decide what the
/// next one binds.
fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn exit_code(e: &RpcError) -> Option<i64> {
    e.data.as_ref().and_then(|d| d["exit_code"].as_i64())
}

fn io_error(what: &Path, e: std::io::Error) -> RpcError {
    RpcError::new(
        ErrorCode::IoError,
        format!("cannot prepare {}: {e}", what.display()),
    )
}

impl InPlaceLayout {
    pub fn new(root: &Path, no_hooks_dir: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            no_hooks_dir: no_hooks_dir.to_path_buf(),
        }
    }
    pub fn config(&self) -> PathBuf {
        self.git_dir.join("config")
    }
    pub fn commondir(&self) -> PathBuf {
        self.git_dir.join("commondir")
    }
    pub fn hooks(&self) -> PathBuf {
        self.git_dir.join("hooks")
    }
    pub fn info(&self) -> PathBuf {
        self.git_dir.join("info")
    }
    pub fn modules(&self) -> PathBuf {
        self.git_dir.join("modules")
    }
    pub fn worktrees(&self) -> PathBuf {
        self.git_dir.join("worktrees")
    }
    pub fn remotes(&self) -> PathBuf {
        self.git_dir.join("remotes")
    }
    pub fn branches(&self) -> PathBuf {
        self.git_dir.join("branches")
    }
    pub fn config_worktree(&self) -> PathBuf {
        self.git_dir.join("config.worktree")
    }

    /// Git for the daemon's own commands in the checkout.
    ///
    /// Discovery is taken out of the tree's hands the way
    /// [`crate::git::worktree::Layout::worktree_git`] does it, and
    /// `GIT_COMMON_DIR` is pinned as well, which is what stops a planted
    /// `commondir` (see [`COMMONDIR_GUARD`]). Hooks go to the empty directory.
    /// The repository's config is the user's own and read-only to the agent,
    /// so -- as for `daemon_git` -- the `NEUTRALISED_CONFIG` list is not
    /// applied: emptying `filter.lfs.clean` would corrupt the checkout.
    pub fn git(&self) -> Git {
        Git::new()
            .with_env("GIT_DIR", path_arg(&self.git_dir))
            .with_env("GIT_COMMON_DIR", path_arg(&self.git_dir))
            .with_env("GIT_WORK_TREE", path_arg(&self.root))
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir))
    }

    /// Whether the checkout is still there and still a repository, as the
    /// sentence the workspace's `Error` state carries when it is not.
    pub fn check_repository(&self) -> Result<(), RpcError> {
        if self.root.is_dir() && is_real_dir(&self.git_dir) && self.config().is_file() {
            return Ok(());
        }
        Err(RpcError::new(
            ErrorCode::IoError,
            format!(
                "The repository {} is missing or is no longer a git repository. Close the \
                 workspace, or restore the folder and press Retry.",
                self.root.display()
            ),
        ))
    }

    /// Every entry the sandbox gets read-only, with what it must be, in bind
    /// order. `modules` only when it is a real directory: bwrap would otherwise
    /// create it, and an agent that makes one gains nothing an embedded
    /// repository in the tree would not give it (plan R3).
    fn late_entries(&self) -> Vec<(PathBuf, EntryKind)> {
        let mut entries = vec![
            (self.config(), EntryKind::File),
            (self.commondir(), EntryKind::File),
            (self.hooks(), EntryKind::Dir),
            (self.info(), EntryKind::Dir),
            (self.worktrees(), EntryKind::Dir),
            (self.remotes(), EntryKind::Dir),
            (self.branches(), EntryKind::Dir),
        ];
        if is_real_dir(&self.modules()) {
            entries.push((self.modules(), EntryKind::Dir));
        }
        entries.push((self.config_worktree(), EntryKind::File));
        entries
    }

    /// Refuses the checkout when `.git` or an entry that is about to be bound
    /// read-only exists as something other than what git makes there.
    ///
    /// A bind follows a symlink, so a symlinked `config.worktree` would protect
    /// whatever it points at rather than the entry git reads, and writing a
    /// missing file through a dangling one would create its target wherever
    /// the daemon user can write.
    fn refuse_foreign_entries(&self) -> Result<(), RpcError> {
        let entries =
            std::iter::once((self.git_dir.clone(), EntryKind::Dir)).chain(self.late_entries());
        for (path, kind) in entries {
            match std::fs::symlink_metadata(&path) {
                Ok(m) if EntryKind::of(&m) == Some(kind) => {}
                Ok(_) => {
                    return Err(RpcError::invalid_params(format!(
                        "{} has a {} that is not {}; an agent cannot work in place in it",
                        self.root.display(),
                        self.name_of(&path),
                        kind.described()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error(&path, e)),
            }
        }
        Ok(())
    }

    /// Refuses a checkout where one of the files whose contents are protected
    /// has another name of its own.
    ///
    /// A read-only bind protects a path, not a file. A second hard link to
    /// `.git/config` is a second path to the same bytes on the same writable
    /// mount, so an agent that plants one -- during the window a Retry opens,
    /// or before the workspace was ever started -- writes the user's config
    /// through it with the bind standing and every identity check satisfied.
    /// Git never makes one, so a link count above one is refused rather than
    /// reasoned about.
    fn refuse_extra_links(&self) -> Result<(), RpcError> {
        for path in self.hard_link_checked() {
            if links(&path)? > 1 {
                return Err(RpcError::invalid_params(format!(
                    "{} has more than one name; an agent cannot work in place in it",
                    self.name_of(&path)
                )));
            }
        }
        Ok(())
    }

    /// The protected files a second hard link would defeat: the three git
    /// reads as configuration. The protected *directories* are not here --
    /// a directory has a link per subdirectory, and `.git/worktrees` gains one
    /// whenever the user adds a worktree.
    fn hard_link_checked(&self) -> [PathBuf; 3] {
        [self.config(), self.config_worktree(), self.commondir()]
    }

    /// Every refusal [`prepare`](Self::prepare) can make, without writing
    /// anything, so `workspace.create` can refuse a checkout rather than
    /// register a workspace that could never start. `Ok(true)` when the
    /// `commondir` guard is already in place, `Ok(false)` when it is missing.
    pub fn check_preparable(&self) -> Result<bool, RpcError> {
        self.refuse_foreign_entries()?;
        self.refuse_extra_links()?;
        let guard = self.commondir();
        match std::fs::read(&guard) {
            Ok(bytes) if bytes == COMMONDIR_GUARD.as_bytes() => Ok(true),
            Ok(_) => Err(RpcError::invalid_params(format!(
                "{} has a .git/commondir that BondSymphonic did not write; an agent cannot \
                 work in place in it",
                self.root.display()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error(&guard, e)),
        }
    }

    /// `path` as the root-relative name a person reads, `.git/config`.
    fn name_of(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Makes every read-only bind target exist, before every sandbox start.
    ///
    /// bwrap creates a missing bind target itself -- an empty directory, or an
    /// empty read-only file -- and here the underlying mount is the user's
    /// writable repository, so the daemon makes each one first and knows
    /// exactly what it made: the `commondir` guard, and `hooks`, `info`, the
    /// [`ON_DEMAND_DIRS`] and `config.worktree` when they are missing, which
    /// git would create itself and which change nothing empty.
    /// `config.worktree` is made and bound whatever `extensions.worktreeConfig`
    /// says: git ignores the file while the extension is off, but `git
    /// sparse-checkout init` turns the extension on and keeps whatever an agent
    /// left in the file.
    ///
    /// Which of the recordable entries this call is about to create is
    /// appended to `record`, a daemon-owned file outside every sandbox
    /// ([`crate::workspace::DataDirs::in_place_record`]), so Close can take
    /// back exactly those and never one the user had. The record is written
    /// before the entries are made, so a failure in between leaves a name
    /// recorded for an entry that may not exist, which Close skips, rather
    /// than an entry nobody remembers making.
    ///
    /// Last, once `.git/worktrees` is certain to exist, the persistent hold
    /// goes in: see [`crate::git::worktree::hold_worktrees_for_in_place`].
    pub fn prepare(&self, id: &WorkspaceId, record: &Path) -> Result<(), RpcError> {
        if !self.check_preparable()? {
            create_file(&self.commondir(), COMMONDIR_GUARD.as_bytes())?;
        }
        for dir in [self.hooks(), self.info()] {
            if std::fs::symlink_metadata(&dir).is_err() {
                std::fs::create_dir(&dir).map_err(|e| io_error(&dir, e))?;
            }
        }
        let missing: Vec<&str> = RECORDABLE
            .into_iter()
            .filter(|name| std::fs::symlink_metadata(self.git_dir.join(name)).is_err())
            .collect();
        if missing.is_empty() {
            crate::git::worktree::hold_worktrees_for_in_place(&self.git_dir, id);
            return Ok(());
        }
        {
            use std::io::Write;
            if let Some(parent) = record.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
            }
            let lines: String = missing.iter().map(|name| format!("{name}\n")).collect();
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(record)
                .and_then(|mut f| f.write_all(lines.as_bytes()))
                .map_err(|e| io_error(record, e))?;
        }
        for name in missing {
            let path = self.git_dir.join(name);
            if name == CONFIG_WORKTREE {
                // Git reads an empty file as no configuration.
                create_file(&path, b"")?;
            } else {
                std::fs::create_dir(&path).map_err(|e| io_error(&path, e))?;
            }
        }
        crate::git::worktree::hold_worktrees_for_in_place(&self.git_dir, id);
        Ok(())
    }

    /// Takes back what [`prepare`](Self::prepare) left that git would not have:
    /// the persistent hold, the guard, if it is still exactly the guard, and
    /// each entry `record` says the daemon created, if it is still empty. Then
    /// the record itself. Best effort: Close must not fail over any of it.
    ///
    /// The hold goes first, since it is what keeps `.git/worktrees` from being
    /// empty.
    pub fn release(&self, id: &WorkspaceId, record: &Path) {
        crate::git::worktree::release_in_place_hold(&self.git_dir, id);
        let guard = self.commondir();
        if std::fs::read(&guard).is_ok_and(|b| b == COMMONDIR_GUARD.as_bytes()) {
            let _ = std::fs::remove_file(&guard);
        }
        let recorded = std::fs::read_to_string(record).unwrap_or_default();
        for name in RECORDABLE {
            // Only names from the fixed list: the record is the daemon's, but
            // nothing read back from disk gets to name a path by itself.
            if !recorded.lines().any(|line| line == name) {
                continue;
            }
            let path = self.git_dir.join(name);
            if name == CONFIG_WORKTREE {
                let empty_file = std::fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.file_type().is_file() && m.len() == 0);
                if empty_file {
                    let _ = std::fs::remove_file(&path);
                }
            } else {
                // `remove_dir` only removes an empty directory, which is the
                // point: whatever git has put there since is the user's.
                let _ = std::fs::remove_dir(&path);
            }
        }
        let _ = std::fs::remove_file(record);
    }

    /// Bound read-write, in this order. `.git` is a mount of its own so it
    /// cannot be renamed, removed or replaced from inside (`EBUSY`). From
    /// outside it can, which is what [`ProtectedSnapshot`] watches for.
    pub fn rw_binds(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.git_dir.clone()]
    }

    /// Bound read-only after the read-write binds. `worktrees` is here because
    /// it holds the git directories of this repository's *worktree*
    /// workspaces, whose `config.worktree` the daemon's own status reads;
    /// `remotes` and `branches` because `git fetch <name>` and `git push
    /// <name>` read remote definitions from them. `config.worktree` is bound
    /// whatever `extensions.worktreeConfig` says; see [`prepare`](Self::prepare).
    pub fn late_ro_binds(&self) -> Vec<PathBuf> {
        self.late_entries().into_iter().map(|(p, _)| p).collect()
    }

    /// What the sandbox about to start is protected by, as it is now. Taken
    /// right after [`prepare`](Self::prepare), before the sandbox starts, and
    /// then [checked](ProtectedSnapshot::check) every [`PROTECTION_POLL`].
    ///
    /// A read-only bind holds only as long as the entry it was made on: a git
    /// *outside* the sandbox that replaces `.git/config` (every `git config`
    /// writes `config.lock` and renames it over) or removes `.git/worktrees`
    /// (the last `git worktree remove`) detaches the bind inside the sandbox,
    /// and the path falls through to the read-write `.git` below it. Nothing
    /// can prevent that from the sandbox's side, so the daemon notices and
    /// stops the sandbox instead.
    ///
    /// The sandbox must be given exactly
    /// [`ProtectedSnapshot::late_ro_binds`], not a list computed again: the
    /// list depends on whether `.git/modules` exists, and an entry that is
    /// bound but not watched is not protected.
    pub fn snapshot(&self) -> std::io::Result<ProtectedSnapshot> {
        let root = std::fs::canonicalize(&self.root)?;
        let mut entries = Vec::new();
        let single_link = self.hard_link_checked();
        let all =
            std::iter::once((self.git_dir.clone(), EntryKind::Dir)).chain(self.late_entries());
        for (path, kind) in all {
            let meta = std::fs::symlink_metadata(&path)?;
            if EntryKind::of(&meta) != Some(kind) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} is not {}", path.display(), kind.described()),
                ));
            }
            // A hard link planted before this start would otherwise be a name
            // for the user's config that no bind covers; see
            // `refuse_extra_links`.
            let single_link = single_link.contains(&path);
            if single_link && link_count(&meta) > 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} has more than one name", path.display()),
                ));
            }
            // bwrap makes the path of a bind target inside the sandbox from
            // the spelling it is given, so a symlink in the root's path can
            // show up in `mountinfo` either resolved or as written. Either
            // counts; both are fixed here, once.
            let mut mount_points = vec![path.clone()];
            if let Ok(rel) = path.strip_prefix(&self.root) {
                let canonical = root.join(rel);
                if canonical != path {
                    mount_points.push(canonical);
                }
            }
            entries.push(Protected {
                identity: identity(&meta),
                name: self.name_of(&path),
                path,
                mount_points,
                kind,
                single_link,
            });
        }
        let diff_files = [self.config(), self.config_worktree(), self.commondir()]
            .into_iter()
            .map(|path| {
                let text = read_protected(&path);
                (self.name_of(&path), path, text)
            })
            .collect();
        let registrations = registrations(&self.worktrees());
        let covered = self.scan_covered(&registrations);
        Ok(ProtectedSnapshot {
            layout: self.clone(),
            entries,
            diff_files,
            registrations,
            covered,
        })
    }

    /// Everything under `.git` that git reads as configuration or runs as a
    /// program, with its contents, in one deterministic order.
    ///
    /// This is the check that holds where a bind does not. A bind protects one
    /// Linux dentry; on the Windows drives this IDE's users work from
    /// (`/mnt/c/...`, 9p `drvfs`), Windows resolves `.git/CONFIG`, `.GIT/config`
    /// and the 8.3 short name `GIT~1/config` to the same file through dentries
    /// the mount does not cover, and a write through any of them reaches the
    /// user's config with the mount, the device and the inode all unchanged.
    /// Measured in the distro: each of those four spellings gets past every
    /// read-only bind. Contents are the only thing left that tells the
    /// difference, so they are read on every poll and compared whole.
    ///
    /// What is covered, and what deliberately is not:
    /// - `config`, `config.worktree` and `commondir`: every one names programs.
    /// - Everything under `hooks`, `info`, `remotes` and `branches`: hooks are
    ///   programs, `info/attributes` and `info/grafts` change what git does with
    ///   the tree and the history, and a legacy remote definition can hold an
    ///   `ext::` URL, which is a command. All four are small and git does not
    ///   write them by itself -- except `info/refs`, which every `git gc`
    ///   rewrites through `update-server-info` (measured) and which is a ref
    ///   listing for dumb HTTP clients rather than anything git executes.
    /// - Of `worktrees` and `modules`, the files a sibling worktree's or a
    ///   submodule's git reads as configuration. Not the rest: a registration
    ///   holds an index, a `HEAD` and reflogs that the daemon's own work in a
    ///   sibling worktree rewrites constantly, and a submodule's git directory
    ///   holds an object store.
    /// - Of `worktrees`, only the registrations `registrations` names, which is
    ///   the set that existed when the sandbox started. The daemon adds
    ///   registrations itself while an in-place sandbox runs -- that is what
    ///   `workspace.create` on a sibling worktree workspace does -- and an
    ///   in-place workspace must not be stopped by the daemon's own ordinary
    ///   work. A registration an agent invents instead gains it nothing: git
    ///   only reads a registration's `config.worktree` under that worktree's own
    ///   `GIT_DIR`, and `worktree list` skips an entry with no `gitdir`.
    fn scan_covered(&self, registrations: &[std::ffi::OsString]) -> Vec<(String, Content)> {
        let mut scan = Scan {
            root: &self.root,
            out: Vec::new(),
            bytes: 0,
        };
        for path in self.hard_link_checked() {
            scan.file(&path, Read::Contents);
        }
        scan.tree(&self.hooks(), &[]);
        scan.tree(&self.info(), &[INFO_REFS]);
        scan.tree(&self.remotes(), &[]);
        scan.tree(&self.branches(), &[]);
        let worktrees = self.worktrees();
        for name in registrations {
            let entry = worktrees.join(name);
            // `config.worktree` as "empty or not there at all": git reads an
            // empty file as no configuration, and the daemon writes this one
            // empty on every start of the worktree workspace it belongs to,
            // over a file git itself never made.
            scan.empty_or_missing(&entry.join("config.worktree"));
            scan.file(&entry.join("commondir"), Read::Contents);
        }
        scan.modules(&self.modules(), 0);
        scan.out
    }
}

/// The names in `.git/worktrees`, sorted, or nothing when it cannot be read.
fn registrations(worktrees: &Path) -> Vec<std::ffi::OsString> {
    let Ok(read) = std::fs::read_dir(worktrees) else {
        return Vec::new();
    };
    let mut names: Vec<std::ffi::OsString> = read.flatten().map(|e| e.file_name()).collect();
    names.sort();
    names
}

/// The most paths one [`InPlaceLayout::scan_covered`] looks at, and the most
/// bytes it keeps across all of them.
///
/// Neither is reached by anything git makes: a fresh `.git/hooks` holds
/// fourteen samples of a few kilobytes each, and every other covered file is
/// one git wrote by hand. They are here so that a repository with hundreds of
/// submodules cannot make a poll that must finish inside
/// [`PROTECTION_POLL`] walk an unbounded tree. The order is fixed and sorted,
/// so a snapshot and a later check that both stop at the cap stop at the same
/// place -- and anything added ahead of the cap is a change in its own right,
/// which is what stops the sandbox.
const COVERED_CAP: usize = 512;
const COVERED_BYTES_CAP: u64 = 4 * 1024 * 1024;

/// What a covered path was when it was looked at. Compared whole.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Dir,
    /// A regular file: its length, and its first [`READ_CAP`] bytes. The length
    /// is kept as well as the bytes so that appending past the cap is a change
    /// too.
    File(u64, Vec<u8>),
    /// A symlink, which is neither.
    Other,
    /// There, and deliberately not read: its name and its type are all that
    /// matters about it. See [`Read`].
    Listed,
    /// Nothing is there.
    Absent,
    /// There, and the daemon could not read it.
    Unreadable(String),
}

/// One walk of the covered set. See [`InPlaceLayout::scan_covered`].
struct Scan<'a> {
    root: &'a Path,
    out: Vec<(String, Content)>,
    bytes: u64,
}

impl Scan<'_> {
    fn full(&self) -> bool {
        self.out.len() >= COVERED_CAP || self.bytes >= COVERED_BYTES_CAP
    }

    fn name_of(&self, path: &Path) -> String {
        path.strip_prefix(self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Records one path, reading it when it is a regular file.
    ///
    /// Opened before it is looked at, rather than `lstat` and then open: on a
    /// Windows drive every one of those crosses into Windows, the poll has 250
    /// ms for the whole covered set, and this is the call the set is made of.
    /// `O_NOFOLLOW` so a symlink is seen as one rather than followed, and
    /// `O_NONBLOCK` so a FIFO nobody will ever write cannot hold the poll.
    fn file(&mut self, path: &Path, read: Read) {
        if self.full() {
            return;
        }
        let content = self.contents_of(path, read);
        self.out.push((self.name_of(path), content));
    }

    fn contents_of(&mut self, path: &Path, read: Read) -> Content {
        use std::io::Read as _;
        // Nothing to open, and nothing to `lstat` either: a listed entry is
        // recorded by its name and its type, and on a Windows drive every
        // syscall that is not made is a crossing into Windows that is not made.
        if read == Read::Listed {
            return match std::fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Content::Absent,
                Err(e) => Content::Unreadable(e.to_string()),
                Ok(m) if m.is_dir() => Content::Dir,
                Ok(m) if m.is_symlink() => Content::Other,
                Ok(_) => Content::Listed,
            };
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Content::Absent,
            // What `O_NOFOLLOW` answers for a symlink.
            #[cfg(unix)]
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Content::Other,
            Err(e) => return Content::Unreadable(e.to_string()),
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => return Content::Unreadable(e.to_string()),
        };
        if meta.is_dir() {
            return Content::Dir;
        }
        if !meta.is_file() || read == Read::Listed {
            // A `*.sample` hook is listed, not read: git runs a hook by name and
            // never one with that suffix, so what is *in* one changes nothing.
            // Renaming it to a name git does run is a new name in the listing,
            // which is a change like any other.
            return Content::Listed;
        }
        let mut bytes = Vec::new();
        match file.take(READ_CAP).read_to_end(&mut bytes) {
            Ok(_) => {
                self.bytes += bytes.len() as u64;
                Content::File(meta.len(), bytes)
            }
            Err(e) => Content::Unreadable(e.to_string()),
        }
    }

    /// [`Self::file`], with an empty regular file recorded as nothing being
    /// there: for a file whose emptiness is how git is told to ignore it, the
    /// two say the same thing.
    fn empty_or_missing(&mut self, path: &Path) {
        self.file(path, Read::Contents);
        if let Some((_, content)) = self.out.last_mut() {
            if *content == Content::File(0, Vec::new()) {
                *content = Content::Absent;
            }
        }
    }

    /// Records `dir` and everything under it, names sorted so two walks of the
    /// same tree agree. `skip` names entries of `dir` itself that are left out.
    fn tree(&mut self, dir: &Path, skip: &[&str]) {
        if self.full() {
            return;
        }
        self.file(dir, Read::Contents);
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        // The type from the directory entry, so nothing here needs an `lstat`
        // of its own. A filesystem that does not carry one -- a Windows drive
        // among them -- answers `None`, and then the open in `file` settles it.
        let mut entries: Vec<(std::ffi::OsString, Option<std::fs::FileType>)> = read
            .flatten()
            .map(|e| (e.file_name(), e.file_type().ok()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, file_type) in entries {
            if skip.iter().any(|s| std::ffi::OsStr::new(s) == name) {
                continue;
            }
            let path = dir.join(&name);
            let read = Read::of(&name);
            match file_type {
                Some(t) if t.is_dir() => self.tree(&path, &[]),
                // Nothing more to ask: a listed entry is its name and its
                // type, and the directory listing carried both.
                Some(t) if read == Read::Listed && t.is_file() => {
                    self.out.push((self.name_of(&path), Content::Listed))
                }
                Some(_) => self.file(&path, read),
                // A filesystem whose directory entries carry no type. The
                // `lstat` or the open in `file` settles it instead.
                None if path.is_dir() => self.tree(&path, &[]),
                None => self.file(&path, read),
            }
        }
    }

    /// The configuration and hooks of every submodule git directory under
    /// `modules`, and of every module nested in one of those.
    ///
    /// Only through path components named `modules`, which is how git nests
    /// them, so nothing here descends into an object store. The depth is
    /// bounded as well as the walk: a `modules/a/modules/a/...` chain an agent
    /// makes is not something to follow to the end.
    fn modules(&mut self, modules: &Path, depth: usize) {
        const MAX_DEPTH: usize = 8;
        if depth >= MAX_DEPTH || self.full() || !modules.is_dir() {
            return;
        }
        let Ok(read) = std::fs::read_dir(modules) else {
            return;
        };
        let mut names: Vec<std::ffi::OsString> = read.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            let module = modules.join(&name);
            if !std::fs::symlink_metadata(&module).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            for file in ["config", "config.worktree", "commondir"] {
                self.file(&module.join(file), Read::Contents);
            }
            self.tree(&module.join("hooks"), &[]);
            self.modules(&module.join("modules"), depth + 1);
        }
    }
}

/// Whether a covered file's contents are worth reading, or whether its name,
/// type and length say everything there is to say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    Contents,
    Listed,
}

impl Read {
    /// Git runs a hook by name, and never one named `<hook>.sample` -- those
    /// are the samples `git init` copies out of its template. Reading a
    /// kilobyte out of each of the fourteen of them on every poll, on a
    /// filesystem where every open crosses into Windows, is most of what a
    /// poll would cost, and it buys nothing: to make one run, an agent has to
    /// give it a name git knows, which is a new name in the listing.
    fn of(name: &std::ffi::OsStr) -> Self {
        match Path::new(name).extension() {
            Some(ext) if ext == "sample" => Self::Listed,
            _ => Self::Contents,
        }
    }
}

/// What a protected entry must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
}

impl EntryKind {
    /// Without following a symlink: a symlink is neither.
    fn of(meta: &std::fs::Metadata) -> Option<Self> {
        let t = meta.file_type();
        if t.is_file() {
            Some(Self::File)
        } else if t.is_dir() {
            Some(Self::Dir)
        } else {
            None
        }
    }

    fn described(self) -> &'static str {
        match self {
            Self::File => "a regular file",
            Self::Dir => "a directory",
        }
    }
}

/// The name of the one recordable entry that is a file.
const CONFIG_WORKTREE: &str = "config.worktree";

/// Everything [`InPlaceLayout::prepare`] records when it creates it, and
/// [`InPlaceLayout::release`] may take back.
const RECORDABLE: [&str; 4] = [
    ON_DEMAND_DIRS[0],
    ON_DEMAND_DIRS[1],
    ON_DEMAND_DIRS[2],
    CONFIG_WORKTREE,
];

/// Creates `path` with `bytes`, failing if anything -- a dangling symlink
/// included -- is already there.
fn create_file(path: &Path, bytes: &[u8]) -> Result<(), RpcError> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| io_error(path, e))
}

/// The most of a protected file [`ProtectedSnapshot::check`] reads.
const READ_CAP: u64 = 256 * 1024;

/// The most lines a breach's diff carries.
const DIFF_LINES: usize = 200;

/// Reads a protected file for the breach diff, or says why it will not.
///
/// By the time this runs after a breach the agent may have had write access
/// to the entry, so it may be a FIFO that blocks an `open` forever, a symlink
/// to `/dev/zero`, or a sparse file of any size. The open neither follows a
/// symlink nor waits, only a regular file is read, and only up to
/// [`READ_CAP`]. Control characters come back escaped, since the text goes
/// to the daemon log.
fn read_protected(path: &Path) -> Result<String, String> {
    use std::io::Read;
    const NOT_REGULAR: &str = "is no longer a regular file";
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("is missing".into()),
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(NOT_REGULAR.into()),
        Err(e) => return Err(format!("cannot be read: {e}")),
    };
    match file.metadata() {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(NOT_REGULAR.into()),
        Err(e) => return Err(format!("cannot be read: {e}")),
    }
    let mut bytes = Vec::new();
    if let Err(e) = file.take(READ_CAP + 1).read_to_end(&mut bytes) {
        return Err(format!("cannot be read: {e}"));
    }
    if bytes.len() as u64 > READ_CAP {
        return Err(format!("is larger than {} KiB; not shown", READ_CAP / 1024));
    }
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect())
}

/// Which file an entry is. Device and inode, never size or time: the user
/// editing `.git/config` in place leaves the bind standing, and only an entry
/// that was replaced or removed has lost it.
#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// No sandbox binds anything on a platform without inodes.
#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// How many names this file has. Git makes one of each, so anything above one
/// is a name the read-only binds do not cover; see
/// [`InPlaceLayout::refuse_extra_links`].
#[cfg(unix)]
fn link_count(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn link_count(_meta: &std::fs::Metadata) -> u64 {
    1
}

/// The link count of `path`, counting a path that is not there as one: a file
/// that does not exist has no second name either, and whether it may be missing
/// is [`InPlaceLayout::refuse_foreign_entries`]'s question, not this one.
fn links(path: &Path) -> Result<u64, RpcError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => Ok(link_count(&m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(1),
        Err(e) => Err(io_error(path, e)),
    }
}

/// The one name under `.git/info` the content check leaves out. Every `git gc`
/// rewrites it through `update-server-info` (measured with git 2.43), and it is
/// a ref listing for dumb HTTP clients, not anything git executes.
const INFO_REFS: &str = "refs";

/// How often the lifecycle re-checks an in-place sandbox's protected entries.
pub const PROTECTION_POLL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug)]
struct Protected {
    /// Root-relative, for the sentence and the log.
    name: String,
    /// Where the daemon looks at it.
    path: PathBuf,
    /// The spellings under which the sandbox's `mountinfo` may list it.
    mount_points: Vec<PathBuf>,
    kind: EntryKind,
    identity: (u64, u64),
    /// Whether a second hard link to this entry is a breach in itself.
    single_link: bool,
}

/// The entries an in-place sandbox was started with, and the contents of
/// everything under `.git` that git reads as configuration or runs as a
/// program. See [`InPlaceLayout::snapshot`].
#[derive(Debug)]
pub struct ProtectedSnapshot {
    /// What the covered set is scanned from again on every poll.
    layout: InPlaceLayout,
    /// `.git` first, then the late read-only binds in bind order.
    entries: Vec<Protected>,
    /// `(name, path, contents)` of `config`, `config.worktree` and `commondir`,
    /// as the text the breach diff is made of.
    diff_files: Vec<(String, PathBuf, Result<String, String>)>,
    /// The names in `.git/worktrees` the covered set was taken over.
    registrations: Vec<std::ffi::OsString>,
    /// `(name, contents)` of every covered path, in scan order.
    covered: Vec<(String, Content)>,
}

impl ProtectedSnapshot {
    /// The read-only binds this snapshot watches, in bind order: what the
    /// sandbox's spec must carry.
    pub fn late_ro_binds(&self) -> Vec<PathBuf> {
        self.entries[1..].iter().map(|e| e.path.clone()).collect()
    }

    /// `None` while every protected entry is still the one that was bound
    /// (same device, inode, type and number of names, still present),
    /// everything under `.git` that git runs or reads as configuration still
    /// holds exactly what it did ([`InPlaceLayout::scan_covered`]) and -- when
    /// `sandbox_pid` is given -- every entry is still a mount point in
    /// `/proc/<pid>/mountinfo`. Otherwise the breach, with what changed in the
    /// files that name programs.
    ///
    /// A running sandbox must be checked with a pid. Identity alone misses an
    /// entry replaced by one with the same inode number, and git's
    /// lock-and-rename gets the freed number back routinely on ext4: two
    /// config writes in a row (`git branch -u`) leave `.git/config` with its
    /// old inode and no bind. `None` is for a backend without mounts. With a
    /// pid, a `mountinfo` that cannot be read counts as nothing mounted, so
    /// the answer is a breach rather than a guess.
    ///
    /// A few dozen `lstat` calls and the reads of the covered set, which is a
    /// few dozen kilobytes of small files. Measured on the largest `/mnt/c`
    /// repository to hand (BondSymphonic itself, fourteen hooks and two
    /// worktree registrations): about 56 ms a poll, against a
    /// [`PROTECTION_POLL`] of 250 ms, and the caps
    /// ([`COVERED_CAP`]) keep a pathological repository from making it
    /// unbounded. Looking from the host side also makes a DrvFs mount
    /// revalidate a name Windows renamed, which inotify never reports. The
    /// entries are settled before any protected file is read, and those reads
    /// never block and are bounded, so a breach is always answered promptly.
    pub fn check(&self, sandbox_pid: Option<u32>) -> Option<ProtectionBreach> {
        let mounted = sandbox_pid.map(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
                .map(|text| mount_points(&text))
                .unwrap_or_default()
        });
        let mut entries = Vec::new();
        let mut removed = Vec::new();
        for e in &self.entries {
            let now = std::fs::symlink_metadata(&e.path);
            let same = now.as_ref().is_ok_and(|m| {
                EntryKind::of(m) == Some(e.kind)
                    && identity(m) == e.identity
                    && (!e.single_link || link_count(m) == 1)
            });
            let still_bound = mounted
                .as_ref()
                .is_none_or(|points| e.mount_points.iter().any(|p| points.contains(p)));
            if same && still_bound {
                continue;
            }
            entries.push(e.name.clone());
            if now.is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
                removed.push(e.name.clone());
            }
        }
        // The check the read-only binds cannot make for themselves. Every
        // difference is named, whether the entry moved or only its contents
        // did, so the sentence and the log say which file to look at.
        let now = self.layout.scan_covered(&self.registrations);
        for name in changed(&self.covered, &now) {
            if !entries.contains(&name) {
                entries.push(name);
            }
        }
        if entries.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for (name, path, before) in &self.diff_files {
            line_diff(name, before, &read_protected(path), &mut lines);
        }
        if lines.len() > DIFF_LINES {
            let more = lines.len() - DIFF_LINES;
            lines.truncate(DIFF_LINES);
            lines.push(format!("... {more} more lines not shown"));
        }
        let config_diff = lines.iter().map(|l| format!("{l}\n")).collect();
        Some(ProtectionBreach {
            entries,
            removed,
            config_diff,
        })
    }
}

/// The covered names that are not what they were: one that has appeared or
/// gone, and one whose contents differ. Both lists are scanned the same way
/// from the same roots, so a name that is in both is the same path.
fn changed(before: &[(String, Content)], now: &[(String, Content)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, was) in before {
        if now.iter().find(|(n, _)| n == name).map(|(_, c)| c) != Some(was) {
            out.push(name.clone());
        }
    }
    for (name, _) in now {
        if !before.iter().any(|(n, _)| n == name) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

/// The mount points in a `/proc/<pid>/mountinfo`, unescaped.
fn mount_points(mountinfo: &str) -> std::collections::HashSet<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mount_field(field)))
        .collect()
}

/// The kernel writes space, tab, newline and backslash in a mount point as
/// `\ooo` octal escapes.
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Appends a whole-file line diff of `name` in unified style to `out`, or
/// nothing when it did not change. These files are a few dozen lines, so
/// every line is shown rather than hunks. A file that could not be read is
/// one line saying why; past a size where the comparison would be slow, the
/// old and new lines are listed as they are. The caller caps the total.
fn line_diff(
    name: &str,
    before: &Result<String, String>,
    after: &Result<String, String>,
    out: &mut Vec<String>,
) {
    let (before, after) = match (before, after) {
        (Ok(a), Ok(b)) if a == b => return,
        (Ok(a), Ok(b)) => (a, b),
        (_, Err(why)) => return out.push(format!("{name} {why}")),
        (Err(why), Ok(_)) => {
            return out.push(format!("{name} {why} at the start; not compared"));
        }
    };
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    out.push(format!("--- a/{name}"));
    out.push(format!("+++ b/{name}"));
    if a.len().saturating_mul(b.len()) > 250_000 {
        out.extend(a.iter().map(|l| format!("-{l}")));
        out.extend(b.iter().map(|l| format!("+{l}")));
        return;
    }
    // Longest common subsequence lengths of every pair of suffixes.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(format!(" {}", a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(format!("+{}", b[j]));
            j += 1;
        } else {
            out.push(format!("-{}", a[i]));
            i += 1;
        }
    }
}

/// Protected entries of a running in-place sandbox were replaced, removed,
/// unmounted or written from outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionBreach {
    /// Root-relative names of the entries that were replaced, removed,
    /// unmounted or written, e.g. `[".git/config"]`.
    pub entries: Vec<String>,
    /// Those of `entries` that are not there at all any more, rather than
    /// replaced by something else.
    pub removed: Vec<String>,
    /// A unified-style line diff of `.git/config` (and the other snapshotted
    /// files) between the snapshot and now; empty when unchanged.
    pub config_diff: String,
}

impl ProtectionBreach {
    /// The sentence the workspace's `Error` state carries.
    ///
    /// Git removes `.git/worktrees` itself when the repository's last linked
    /// worktree goes, which is the user's own ordinary work and no sign of
    /// tampering, so that one case is told as what it is.
    pub fn sentence(&self) -> String {
        let worktrees = [".git/worktrees".to_string()];
        if self.entries == worktrees && self.removed == worktrees {
            return "This repository's last worktree was removed, which also removed a \
                    directory the sandbox keeps read-only, so the sandbox was stopped. Nothing \
                    needs checking; press Retry."
                .to_string();
        }
        format!(
            "Git files this workspace protects changed while the agent was running ({}), so its \
             sandbox was stopped. Check .git/config for settings you did not make — the daemon \
             log shows what changed — then press Retry.",
            self.named()
        )
    }

    /// The changed entries for the sentence. A write through a name a Windows
    /// drive resolves to the same file can change a whole directory at once, so
    /// the list is one a person can read rather than all of it.
    fn named(&self) -> String {
        const SHOWN: usize = 6;
        let shown = self.entries.iter().take(SHOWN).cloned().collect::<Vec<_>>();
        match self.entries.len().checked_sub(SHOWN) {
            Some(more) if more > 0 => format!("{}, and {more} more", shown.join(", ")),
            _ => shown.join(", "),
        }
    }
}

/// The branch checked out at `root`, or `""` when `HEAD` is detached. An
/// unborn branch is still a branch and is named.
pub async fn head_branch(git: &Git, root: &Path) -> Result<String, RpcError> {
    match git
        .run(root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
    {
        Ok(o) => Ok(o.stdout.trim().to_string()),
        Err(e) if exit_code(&e) == Some(1) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// What an in-place workspace's changes are measured against: `HEAD`, or the
/// empty tree in a repository with no commits yet.
pub async fn diff_base(git: &Git, root: &Path) -> Result<String, RpcError> {
    match git
        .run(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
    {
        Ok(o) => Ok(o.stdout.trim().to_string()),
        Err(e) if exit_code(&e) == Some(1) => Ok(EMPTY_TREE.to_string()),
        Err(e) => Err(e),
    }
}

/// Why `root`, which git classified as `kind`, cannot be worked on in place.
/// One sentence, shared by `repo.inspect` (which the dialog shows) and
/// `workspace.create` (which refuses with it).
pub fn in_place_refusal(kind: &RepoKind, root: &Path) -> Option<String> {
    match kind {
        RepoKind::Root if is_real_dir(&root.join(".git")) => None,
        RepoKind::Root => Some(format!(
            "{} keeps its git directory elsewhere (its .git is a file), so an agent cannot \
             work in it in place",
            root.display()
        )),
        RepoKind::Worktree => Some(format!(
            "{} is a linked worktree of another repository; pick that repository's main \
             checkout to work in place",
            root.display()
        )),
        RepoKind::Bare => Some(repo::bare_repository_error(root).message),
        RepoKind::InsideEnclosing { root: enclosing } => Some(format!(
            "{} is inside the git repository {}; pick the repository itself to work in place",
            root.display(),
            repo::canonical_ish(enclosing).display()
        )),
        RepoKind::NotARepo => Some(format!("{} is not a git repository", root.display())),
    }
}

/// Why the daemon must never bind `root` read-write into a sandbox: the
/// filesystem root, the daemon user's home, anything inside the daemon's data
/// directory -- and anything that *contains* it, because the bind would bring
/// every other workspace's home and exec socket back over the tmpfs that masks
/// them.
pub fn target_refusal(root: &Path, data_root: &Path) -> Option<String> {
    if let Some(why) = repo::init_target_refusal(root, repo::daemon_home().as_deref()) {
        return Some(why);
    }
    let root = repo::canonical_ish(root);
    let data = repo::canonical_ish(data_root);
    if root.starts_with(&data) {
        return Some("it is inside the daemon's own data directory".into());
    }
    if data.starts_with(&root) {
        return Some("it contains the daemon's own data directory".into());
    }
    None
}

/// The answer `workspace.merge` and `workspace.create_pr` give an in-place
/// workspace.
pub fn nothing_to_merge() -> RpcError {
    RpcError::invalid_params(
        "an in-place workspace has nothing to merge; commit and push from the checkout",
    )
    .with_data(serde_json::json!({ "reason": IN_PLACE_REASON }))
}
