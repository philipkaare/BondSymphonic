use super::{path_arg, repo, Git};
use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Layout {
    pub repo: PathBuf,
    pub git_common: PathBuf,
    pub name: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub objects_dir: PathBuf,
    /// An empty daemon-owned directory, used as `core.hooksPath`. See
    /// [`crate::workspace::DataDirs::no_hooks`].
    pub no_hooks_dir: PathBuf,
}

impl Layout {
    pub fn ref_dir(&self) -> PathBuf {
        self.git_common.join("refs/heads/bs").join(&self.name)
    }
    pub fn reflog_dir(&self) -> PathBuf {
        self.git_common.join("logs/refs/heads/bs").join(&self.name)
    }
    /// Relies on worktree directory basenames (the unique `ws_` ids) never colliding;
    /// git otherwise suffixes the per-worktree gitdir name (e.g. `ws_1`, `ws_11`), which
    /// would desync this from the actual `<git_common>/worktrees/<...>` directory.
    pub fn worktree_gitdir(&self) -> PathBuf {
        let base = self
            .worktree_path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        self.git_common.join("worktrees").join(base)
    }
    /// Subpaths of the main `.git` that must be bind-mounted read-write into the sandbox.
    pub fn rw_git_paths(&self) -> Vec<PathBuf> {
        vec![self.worktree_gitdir(), self.ref_dir(), self.reflog_dir()]
    }
    /// The one file inside the read-write worktree gitdir that the agent must
    /// not be able to write.
    ///
    /// On a repository with `extensions.worktreeConfig` enabled, git reads this
    /// as repository config for the worktree, and repository config is arbitrary
    /// code: a `filter.<anything>.clean` driver runs whenever `git status` has to
    /// re-hash a file the agent has touched. The daemon owns the file and the
    /// sandbox gets it read-only.
    pub fn config_worktree(&self) -> PathBuf {
        self.worktree_gitdir().join("config.worktree")
    }
    /// Env for git *inside* the sandbox: new objects go to the private dir.
    pub fn sandbox_git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_OBJECT_DIRECTORY".into(), path_arg(&self.objects_dir)),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
                path_arg(&self.git_common.join("objects")),
            ),
        ]
    }
    /// Git for the daemon side: main store primary, private objects as alternate.
    ///
    /// Only safe for commands that run in the *main* repository, which is never
    /// bind-mounted into a sandbox. Anything that touches the worktree must use
    /// [`Layout::worktree_git`].
    ///
    /// `core.hooksPath` is pinned at the empty daemon-owned directory, for the
    /// main repository and for the scratch worktree a merge checks out under it.
    /// A merge the daemon runs is not the user typing `git merge`: it happens on
    /// a schedule the user did not choose, over content an agent wrote, and a
    /// `post-merge` or `pre-commit` hook firing there would run repository code
    /// nobody in front of the screen asked for. Daemon design §5.4 states the
    /// rule and its one exception.
    ///
    /// The [`NEUTRALISED_CONFIG`] list is deliberately **not** applied here.
    /// Those keys name the user's own filter and merge drivers — `git-lfs`'s
    /// `filter.lfs.clean` above all — and the main repository's config is not
    /// agent-writable, so emptying them would not close a hole, it would corrupt
    /// the user's checkout by writing LFS pointers where their files should be.
    /// A `.gitattributes` in the merged tree still chooses which of those
    /// drivers run, which is the residual risk §5.4 records.
    pub fn daemon_git(&self) -> Git {
        Git::new()
            .with_env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                path_arg(&self.objects_dir),
            )
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir))
    }

    /// [`Layout::daemon_git`] with the repository's own hooks left in place, for
    /// `git push` and nothing else.
    ///
    /// `pre-push` is not decoration: it is how `git-lfs` uploads the large
    /// objects a push needs, and a push that skips it puts pointer files on the
    /// remote with nothing behind them. A push is also the one daemon-side git
    /// operation the user explicitly asked for by name, through **Create PR**.
    pub fn daemon_push_git(&self) -> Git {
        Git::new().with_env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            path_arg(&self.objects_dir),
        )
    }

    /// Git for daemon-side commands that run *against a workspace worktree*.
    ///
    /// Every file git would use to discover its repository from the working
    /// directory — `<worktree>/.git` and `<git_common>/worktrees/<id>/commondir`
    /// — is bind-mounted read-write into the sandbox, so an agent can point
    /// discovery at a gitdir of its own and have the daemon read that gitdir's
    /// `config`. Pinning all four paths explicitly takes discovery out of the
    /// agent's hands: the environment outranks anything in the worktree.
    pub fn worktree_git(&self) -> Git {
        let mut git = Git::new()
            .with_env("GIT_DIR", path_arg(&self.worktree_gitdir()))
            .with_env("GIT_COMMON_DIR", path_arg(&self.git_common))
            .with_env("GIT_WORK_TREE", path_arg(&self.worktree_path))
            .with_env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                path_arg(&self.objects_dir),
            )
            .with_config("extensions.worktreeConfig", "false")
            // Not in the list below: an empty `core.hooksPath` does not disable
            // hooks, it moves them to the filesystem root, so this one needs a
            // real directory that is always empty.
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir));
        for key in NEUTRALISED_CONFIG {
            git = git.with_config(key, "");
        }
        git
    }
}

/// Config keys whose value git executes as a command, cleared on the command
/// line for every daemon-side worktree command.
///
/// **Defence in depth, not the primary control.** The primary control is the
/// read-only bind of [`Layout::config_worktree`], because a key list cannot
/// close this class of hole: `filter.<any name>.clean` is a command git runs
/// whenever it re-hashes a file, and the driver name is chosen by whoever wrote
/// the config. This list only shortens the reach of the keys with fixed names,
/// for a repository whose `config.worktree` somehow became writable anyway and
/// for the `noop` backend, which has no mounts and therefore no protection at
/// all — it is a development and test backend and does not sandbox anything.
///
/// Pinning the repository is not enough by itself either. The per-worktree
/// gitdir is writable by the agent (it needs `HEAD`, the index and their lock
/// files), and git reads `config.worktree` from it whenever the repository has
/// `extensions.worktreeConfig` enabled. `-c extensions.worktreeConfig=false`
/// does **not** turn that off — git takes the extension from the repository
/// format during setup, before command-line config exists. Measured against git
/// 2.43 and 2.52: `core.fsmonitor` is the one `git status` reaches by name.
///
/// Every key here is emptied, which disables it. `core.hooksPath` is *not* here
/// for exactly that reason — emptying it relocates hooks to `/` instead of
/// disabling them — so it is set to an empty daemon-owned directory in
/// [`Layout::worktree_git`]. Any key added here has to be checked the same way:
/// an empty value has to mean "off", not "somewhere else".
const NEUTRALISED_CONFIG: &[&str] = &[
    "core.fsmonitor",
    "core.sshCommand",
    "core.gitProxy",
    "core.askPass",
    "core.editor",
    "core.pager",
    "core.alternateRefsCommand",
    "sequence.editor",
    "diff.external",
    "credential.helper",
    "gpg.program",
    "uploadpack.packObjectsHook",
];

/// Whether a [`remove`] may delete the workspace's branch.
///
/// The branch is the one thing in a `Layout` that is **not** private to the
/// workspace being removed. Its name comes from the workspace *name*, and so do
/// `refs/heads/bs/<name>/` and that directory's reflog; the worktree and the
/// object directory are named after the workspace id and belong to one
/// workspace alone. So a cleanup that deletes the branch unconditionally is a
/// cleanup that can delete somebody else's work: two creates of one name, one
/// of them failing, and `git branch -D` lands on the branch the other one is
/// checking out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveBranch {
    /// Delete it. The workspace owns the branch, which is `workspace.destroy`
    /// and the unwinding of a creation that got as far as making it.
    Always,
    /// Leave it, and leave the name-shared ref and reflog directories with it.
    /// For a cleanup that cannot show the branch is its own.
    Never,
}

/// True when `git worktree add -b` refused because the branch is already there.
///
/// Three wordings, because git has three, and the third is the one this is
/// really for:
///
/// * `a branch named <b> already exists` — git checked before it started, so
///   the branch was there all along;
/// * `<b> is already used by worktree at <path>` — it is there and checked out;
/// * `cannot lock ref refs/heads/<b>: reference already exists` — git got past
///   its own check and lost the ref transaction. That is exactly the two-creates
///   race: both calls looked, neither saw the branch, and one of them wrote it
///   first.
///
/// Matched on git's wording and not on the exit code, which is its general
/// fatal. None of the three can be confused with the worktree *directory*
/// already existing, which git reports as `<path> already exists` — no ref, no
/// branch, no lock.
fn branch_is_already_there(e: &RpcError) -> bool {
    e.data
        .as_ref()
        .and_then(|d| d["stderr"].as_str())
        .is_some_and(|s| {
            s.contains("a branch named")
                || s.contains("already used by worktree")
                || s.contains("reference already exists")
        })
}

/// The one answer for "that branch is somebody else's", whichever step found
/// out. `workspace.create` turns it into the `Conflict` a client sees when a
/// name is taken.
fn branch_conflict(branch: &str) -> RpcError {
    RpcError::new(
        ErrorCode::Conflict,
        format!("branch {branch} already exists"),
    )
}

/// Creates the workspace's branch and worktree.
///
/// Through [`Layout::daemon_git`], so none of the repository's own hooks run.
/// `git worktree add` fires `post-checkout` and, for the branch it creates,
/// `reference-transaction`; neither is the user typing a git command. Opening a
/// workspace in the IDE must not execute code out of the repository being
/// opened, on a schedule nobody chose and with no way to see it happen.
///
/// **A failure leaves nothing of this call behind, and nothing anybody else can
/// be shown to own touched.** Whoever created the branch is the only one who
/// may delete it, and the two failures are told apart rather than guessed at:
/// git saying the branch is already there means the branch is not ours, so the
/// cleanup keeps its hands off it and the caller gets a `Conflict` — the same
/// answer as a branch that was already there when the call started.
///
/// Every *other* failure unwinds with [`RemoveBranch::Always`], and that rests
/// on an inference rather than on a fact: the pre-check above saw no branch of
/// this name, so this call is the only candidate for the one that is there now.
/// The inference holds for every wording of "already there" git has, because
/// those take the `Never` path — but a future git that refuses a create for
/// that reason in words none of the three match would send the unwind down this
/// branch instead, and `git branch -D` would land on somebody else's work. The
/// repository lock `workspace.create` holds is what keeps a second create of
/// the same name out of the window in practice; [`branch_is_already_there`] is
/// the part that has to stay current with git.
pub async fn create(layout: &Layout, base_branch: &str) -> Result<(), RpcError> {
    let git = &layout.daemon_git();
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        // Nothing of ours exists yet, so nothing is cleaned up here: the
        // directories this `Layout` names are shared with whichever workspace
        // that branch belongs to.
        return Err(branch_conflict(&layout.branch));
    }
    if !repo::branch_exists(git, &layout.repo, base_branch).await? {
        return Err(RpcError::invalid_params(format!(
            "base branch {base_branch} does not exist"
        )));
    }
    for d in [
        layout.ref_dir(),
        layout.reflog_dir(),
        layout.objects_dir.clone(),
    ] {
        std::fs::create_dir_all(&d).map_err(|e| RpcError::io(&e))?;
    }
    if let Some(parent) = layout.worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    if let Err(e) = git
        .run(
            &layout.repo,
            &[
                "worktree",
                "add",
                "-b",
                &layout.branch,
                &path_arg(&layout.worktree_path),
                base_branch,
            ],
        )
        .await
    {
        // Somebody put the branch there between the check above and now — the
        // same name created twice at once. Their branch and their checkout, so
        // only this call's own directories go.
        if branch_is_already_there(&e) {
            let _ = remove_with(layout, RemoveBranch::Never).await;
            return Err(branch_conflict(&layout.branch));
        }
        let _ = remove_with(layout, RemoveBranch::Always).await;
        return Err(e);
    }
    if !layout.ref_dir().join("work").is_file() {
        let _ = remove_with(layout, RemoveBranch::Always).await;
        return Err(RpcError::internal(format!(
            "expected loose ref at {}",
            layout.ref_dir().join("work").display()
        )));
    }
    Ok(())
}

/// Removes the worktree, its registration and its branch.
///
/// [`remove_with`] with [`RemoveBranch::Always`]: what `workspace.destroy`
/// wants, where the workspace owns everything the layout names.
pub async fn remove(layout: &Layout) -> Result<(), RpcError> {
    remove_with(layout, RemoveBranch::Always).await
}

/// Removes the worktree and its registration, and the branch if `branch` says
/// so.
///
/// Through [`Layout::daemon_git`] for the same reason as [`create`]: the
/// `branch -D` at the end fires `reference-transaction`, and a destroy is the
/// last moment at which running the repository's code would be welcome.
///
/// With [`RemoveBranch::Never`] the branch stays, and so do
/// `refs/heads/bs/<name>/` and its reflog directory — those are named after the
/// workspace *name*, not its id, so they are shared with any other workspace of
/// the same name and are not this call's to delete either. What always goes is
/// the worktree directory and its registration, which belong to one workspace.
pub async fn remove_with(layout: &Layout, branch: RemoveBranch) -> Result<(), RpcError> {
    let git = &layout.daemon_git();
    // `worktree unlock` first, and its failure is not news: it fails for a
    // worktree that was never locked, which is almost all of them. When one
    // *is* locked — a checkout the user parked on a removable disk, say — git
    // refuses `worktree remove --force` outright *and* skips the registration
    // during `prune`, so without this the workspace could not be destroyed at
    // all.
    let _ = git
        .run(
            &layout.repo,
            &["worktree", "unlock", &path_arg(&layout.worktree_path)],
        )
        .await;
    // Git's own removal, for the registration and the directory in one step,
    // and equally not worth reading the outcome of: a worktree that is not
    // registered, a directory that is already gone and a gitfile git will not
    // parse all end here, and all three are states the two steps below reach
    // anyway. Reading git's *prose* to decide which failures were survivable is
    // what this used to do, and it hard-failed on every wording nobody had
    // thought of — the locked one above being exactly that.
    let _ = git
        .run(
            &layout.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &path_arg(&layout.worktree_path),
            ],
        )
        .await;
    // Whatever git left. Not being there is the ordinary case — git usually did
    // the job — but anything else is a directory still standing where the
    // daemon has just told a client the workspace is gone, so it is reported
    // rather than swallowed.
    match std::fs::remove_dir_all(&layout.worktree_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(RpcError::new(
                ErrorCode::IoError,
                format!(
                    "cannot remove the worktree directory {}: {e}",
                    layout.worktree_path.display()
                ),
            ))
        }
    }
    // With the directory gone and the lock off, this is what forgets the
    // registration, whatever state the steps above left it in. The one command
    // here whose failure is a failure: a registration that outlives its
    // directory keeps the branch checked out, and `worktree add` refuses it
    // until a human intervenes.
    git.run(&layout.repo, &["worktree", "prune"]).await?;
    if branch == RemoveBranch::Never {
        return Ok(());
    }
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        git.run(&layout.repo, &["branch", "-D", &layout.branch])
            .await?;
    }
    let _ = std::fs::remove_dir(layout.ref_dir());
    let _ = std::fs::remove_dir_all(layout.reflog_dir());
    Ok(())
}
