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
                &["config", "--bool", "--get", "extensions.worktreeConfig"],
            )
            .await
        {
            Ok(o) => Ok(o.stdout.trim() == "true"),
            // Exit 1 is "not set", which is off.
            Err(e) if exit_code(&e) == Some(1) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Makes every read-only bind target exist, before every sandbox start.
    ///
    /// bwrap creates a missing bind target itself -- an empty directory, or an
    /// empty read-only file -- and here the underlying mount is the user's
    /// writable repository, so the daemon makes each one first and knows
    /// exactly what it made: the `commondir` guard, and `hooks`, `info` and the
    /// [`ON_DEMAND_DIRS`] when they are missing, which git would create itself
    /// and which change nothing empty. `config.worktree` only matters with the
    /// extension on, and a file the user already has is theirs and is left as
    /// it is.
    ///
    /// Which on-demand directories this call created is appended to `record`,
    /// a daemon-owned file outside every sandbox
    /// ([`crate::workspace::DataDirs::in_place_record`]), so Close can take
    /// back exactly those and never one the user had.
    pub fn prepare(&self, worktree_config: bool, record: &Path) -> Result<(), RpcError> {
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
                std::fs::write(&guard, COMMONDIR_GUARD).map_err(|e| io_error(&guard, e))?
            }
            Err(e) => return Err(io_error(&guard, e)),
        }
        for dir in [self.hooks(), self.info()] {
            std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        }
        let mut created = String::new();
        for name in ON_DEMAND_DIRS {
            let dir = self.git_dir.join(name);
            if std::fs::symlink_metadata(&dir).is_err() {
                std::fs::create_dir(&dir).map_err(|e| io_error(&dir, e))?;
                created.push_str(name);
                created.push('\n');
            }
        }
        if !created.is_empty() {
            // Written before the sandbox exists, so a failure here stops the
            // start rather than leaving a directory nobody remembers making.
            use std::io::Write;
            if let Some(parent) = record.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(record)
                .and_then(|mut f| f.write_all(created.as_bytes()))
                .map_err(|e| io_error(record, e))?;
        }
        let wt = self.config_worktree();
        if worktree_config && !wt.exists() {
            // Git reads an empty file as no configuration.
            std::fs::write(&wt, b"").map_err(|e| io_error(&wt, e))?;
        }
        Ok(())
    }

    /// Takes back what [`prepare`](Self::prepare) left that git would not have:
    /// the guard, if it is still exactly the guard, and each on-demand
    /// directory `record` says the daemon created, if it is still empty. Then
    /// the record itself. Best effort: Close must not fail over any of it.
    pub fn release(&self, record: &Path) {
        let guard = self.commondir();
        if std::fs::read(&guard).is_ok_and(|b| b == COMMONDIR_GUARD.as_bytes()) {
            let _ = std::fs::remove_file(&guard);
        }
        let recorded = std::fs::read_to_string(record).unwrap_or_default();
        for name in ON_DEMAND_DIRS {
            // Only names from the fixed list: the record is the daemon's, but
            // nothing read back from disk gets to name a path by itself.
            if recorded.lines().any(|line| line == name) {
                // `remove_dir` only removes an empty directory, which is the
                // point: whatever git has put there since is the user's.
                let _ = std::fs::remove_dir(self.git_dir.join(name));
            }
        }
        let _ = std::fs::remove_file(record);
    }

    /// Bound read-write, in this order. `.git` is a mount of its own so it
    /// cannot be renamed, removed or replaced from inside (`EBUSY`).
    pub fn rw_binds(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.git_dir.clone()]
    }

    /// Bound read-only after the read-write binds. `worktrees` is here because
    /// it holds the git directories of this repository's *worktree*
    /// workspaces, whose `config.worktree` the daemon's own status reads;
    /// `remotes` and `branches` because `git fetch <name>` and `git push
    /// <name>` read remote definitions from them. `modules` only when it is a
    /// real directory: bwrap would otherwise create it, and an agent that makes
    /// one gains nothing an embedded repository in the tree would not give it
    /// (plan R3).
    pub fn late_ro_binds(&self, worktree_config: bool) -> Vec<PathBuf> {
        let mut paths = vec![
            self.config(),
            self.commondir(),
            self.hooks(),
            self.info(),
            self.worktrees(),
            self.remotes(),
            self.branches(),
        ];
        if is_real_dir(&self.modules()) {
            paths.push(self.modules());
        }
        if worktree_config {
            paths.push(self.config_worktree());
        }
        paths
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
