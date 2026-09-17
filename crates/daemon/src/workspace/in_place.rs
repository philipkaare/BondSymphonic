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
use bondsymphonic_proto::{ErrorCode, RpcError};
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

    /// Whether git reads `config.worktree` in this repository.
    pub async fn worktree_config_enabled(&self) -> Result<bool, RpcError> {
        match self
            .git()
            .run(
                &self.root,
                &[
                    "config",
                    "--local",
                    "--bool",
                    "--get",
                    "extensions.worktreeConfig",
                ],
            )
            .await
        {
            Ok(o) => Ok(o.stdout.trim() == "true"),
            // Exit 1 is "not set", which is off.
            Err(e) if exit_code(&e) == Some(1) => Ok(false),
            Err(e) => Err(e),
        }
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
    /// says, so `worktree_config` no longer changes anything: git ignores the
    /// file while the extension is off, but `git sparse-checkout init` turns
    /// the extension on and keeps whatever an agent left in the file.
    ///
    /// Which of the recordable entries this call is about to create is
    /// appended to `record`, a daemon-owned file outside every sandbox
    /// ([`crate::workspace::DataDirs::in_place_record`]), so Close can take
    /// back exactly those and never one the user had. The record is written
    /// before the entries are made, so a failure in between leaves a name
    /// recorded for an entry that may not exist, which Close skips, rather
    /// than an entry nobody remembers making.
    pub fn prepare(&self, _worktree_config: bool, record: &Path) -> Result<(), RpcError> {
        self.refuse_foreign_entries()?;
        let guard = self.commondir();
        match std::fs::read(&guard) {
            Ok(bytes) if bytes == COMMONDIR_GUARD.as_bytes() => {}
            Ok(_) => {
                return Err(RpcError::invalid_params(format!(
                    "{} has a .git/commondir that BondSymphonic did not write; an agent cannot \
                     work in place in it",
                    self.root.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_file(&guard, COMMONDIR_GUARD.as_bytes())?
            }
            Err(e) => return Err(io_error(&guard, e)),
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
        Ok(())
    }

    /// Takes back what [`prepare`](Self::prepare) left that git would not have:
    /// the guard, if it is still exactly the guard, and each entry `record`
    /// says the daemon created, if it is still empty. Then the record itself.
    /// Best effort: Close must not fail over any of it.
    pub fn release(&self, record: &Path) {
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
    /// whatever `worktree_config` says; see [`prepare`](Self::prepare).
    pub fn late_ro_binds(&self, _worktree_config: bool) -> Vec<PathBuf> {
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
    pub fn snapshot(&self) -> std::io::Result<ProtectedSnapshot> {
        let root = std::fs::canonicalize(&self.root)?;
        let mut entries = Vec::new();
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
            let name = self.name_of(&path);
            let mount_point = match path.strip_prefix(&self.root) {
                Ok(rel) => root.join(rel),
                Err(_) => path.clone(),
            };
            entries.push(Protected {
                identity: identity(&meta),
                name,
                path,
                mount_point,
                kind,
            });
        }
        let contents = [self.config(), self.config_worktree(), self.commondir()]
            .into_iter()
            .map(|path| {
                let text = read_lossy(&path);
                (self.name_of(&path), path, text)
            })
            .collect();
        Ok(ProtectedSnapshot { entries, contents })
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

fn read_lossy(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
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

/// How often the lifecycle re-checks an in-place sandbox's protected entries.
pub const PROTECTION_POLL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug)]
struct Protected {
    /// Root-relative, for the sentence and the log.
    name: String,
    /// Where the daemon looks at it.
    path: PathBuf,
    /// Where the sandbox has it mounted, as its `mountinfo` spells it: the
    /// canonical path, because a bind lands where the path resolves to.
    mount_point: PathBuf,
    kind: EntryKind,
    identity: (u64, u64),
}

/// The entries an in-place sandbox was started with, and the contents of the
/// files among them that name programs. See [`InPlaceLayout::snapshot`].
#[derive(Debug)]
pub struct ProtectedSnapshot {
    entries: Vec<Protected>,
    /// `(name, path, contents)` of `config`, `config.worktree` and `commondir`.
    contents: Vec<(String, PathBuf, String)>,
}

impl ProtectedSnapshot {
    /// `None` while every protected entry is still the one that was bound
    /// (same device, inode and type, still present) and -- when `sandbox_pid`
    /// is given and its `mountinfo` readable -- still a mount point in the
    /// sandbox. Otherwise the breach, with what changed in the files that name
    /// programs.
    ///
    /// The mount check catches what identity alone cannot: a directory
    /// removed and made again may get its old inode number back. It is a
    /// handful of `lstat` calls and one small read, cheap enough for
    /// [`PROTECTION_POLL`]. Looking from the host side also makes a DrvFs
    /// mount revalidate a name Windows renamed, which inotify never reports.
    pub fn check(&self, sandbox_pid: Option<u32>) -> Option<ProtectionBreach> {
        let mounted = sandbox_pid
            .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).ok())
            .map(|text| mount_points(&text));
        let entries: Vec<String> = self
            .entries
            .iter()
            .filter(|e| {
                let same = std::fs::symlink_metadata(&e.path)
                    .is_ok_and(|m| EntryKind::of(&m) == Some(e.kind) && identity(&m) == e.identity);
                let still_bound = mounted
                    .as_ref()
                    .is_none_or(|points| points.contains(&e.mount_point));
                !(same && still_bound)
            })
            .map(|e| e.name.clone())
            .collect();
        if entries.is_empty() {
            return None;
        }
        let config_diff = self
            .contents
            .iter()
            .map(|(name, path, before)| line_diff(name, before, &read_lossy(path)))
            .collect();
        Some(ProtectionBreach {
            entries,
            config_diff,
        })
    }
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

/// A whole-file line diff of `name` in unified style, or `""` when nothing
/// changed. These files are a few dozen lines, so every line is shown rather
/// than hunks; past a size where the comparison would be slow the old and new
/// contents are listed in full instead.
fn line_diff(name: &str, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let mut out = format!("--- a/{name}\n+++ b/{name}\n");
    if a.len().saturating_mul(b.len()) > 1_000_000 {
        a.iter().for_each(|l| out.push_str(&format!("-{l}\n")));
        b.iter().for_each(|l| out.push_str(&format!("+{l}\n")));
        return out;
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
            out.push_str(&format!(" {}\n", a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push_str(&format!("+{}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{}\n", a[i]));
            i += 1;
        }
    }
    out
}

/// Protected entries of a running in-place sandbox were replaced, removed or
/// unmounted from outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionBreach {
    /// Root-relative names of the entries that were replaced, removed or
    /// unmounted, e.g. `[".git/config"]`.
    pub entries: Vec<String>,
    /// A unified-style line diff of `.git/config` (and the other snapshotted
    /// files) between the snapshot and now; empty when unchanged.
    pub config_diff: String,
}

impl ProtectionBreach {
    /// The sentence the workspace's `Error` state carries.
    pub fn sentence(&self) -> String {
        format!(
            "Git files this workspace protects were replaced while the agent was running ({}), \
             so its sandbox was stopped. Check .git/config for settings you did not make — \
             the daemon log shows what changed — then press Retry.",
            self.entries.join(", ")
        )
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
