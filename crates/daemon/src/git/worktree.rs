use super::{repo, Git};
use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::{Path, PathBuf};

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
            ("GIT_OBJECT_DIRECTORY".into(), s(&self.objects_dir)),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
                s(&self.git_common.join("objects")),
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
            .with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", s(&self.objects_dir))
            .with_config("core.hooksPath", &s(&self.no_hooks_dir))
    }

    /// [`Layout::daemon_git`] with the repository's own hooks left in place, for
    /// `git push` and nothing else.
    ///
    /// `pre-push` is not decoration: it is how `git-lfs` uploads the large
    /// objects a push needs, and a push that skips it puts pointer files on the
    /// remote with nothing behind them. A push is also the one daemon-side git
    /// operation the user explicitly asked for by name, through **Create PR**.
    pub fn daemon_push_git(&self) -> Git {
        Git::new().with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", s(&self.objects_dir))
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
            .with_env("GIT_DIR", s(&self.worktree_gitdir()))
            .with_env("GIT_COMMON_DIR", s(&self.git_common))
            .with_env("GIT_WORK_TREE", s(&self.worktree_path))
            .with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", s(&self.objects_dir))
            .with_config("extensions.worktreeConfig", "false")
            // Not in the list below: an empty `core.hooksPath` does not disable
            // hooks, it moves them to the filesystem root, so this one needs a
            // real directory that is always empty.
            .with_config("core.hooksPath", &s(&self.no_hooks_dir));
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

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub async fn create(git: &Git, layout: &Layout, base_branch: &str) -> Result<(), RpcError> {
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!("branch {} already exists", layout.branch),
        ));
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
    git.run(
        &layout.repo,
        &[
            "worktree",
            "add",
            "-b",
            &layout.branch,
            &s(&layout.worktree_path),
            base_branch,
        ],
    )
    .await?;
    if !layout.ref_dir().join("work").is_file() {
        return Err(RpcError::internal(format!(
            "expected loose ref at {}",
            layout.ref_dir().join("work").display()
        )));
    }
    Ok(())
}

pub async fn remove(git: &Git, layout: &Layout) -> Result<(), RpcError> {
    let ignore_missing = |r: Result<super::GitOutput, RpcError>| match r {
        Ok(_) => Ok(()),
        Err(e) => {
            let stderr = e
                .data
                .as_ref()
                .and_then(|d| d["stderr"].as_str())
                .unwrap_or("");
            if stderr.contains("is not a working tree")
                || stderr.contains("not found")
                || stderr.contains("No such file")
            {
                Ok(())
            } else {
                Err(e)
            }
        }
    };
    if layout.worktree_path.exists() {
        ignore_missing(
            git.run(
                &layout.repo,
                &["worktree", "remove", "--force", &s(&layout.worktree_path)],
            )
            .await,
        )?;
    }
    let _ = std::fs::remove_dir_all(&layout.worktree_path);
    git.run(&layout.repo, &["worktree", "prune"]).await?;
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        git.run(&layout.repo, &["branch", "-D", &layout.branch])
            .await?;
    }
    let _ = std::fs::remove_dir(layout.ref_dir());
    let _ = std::fs::remove_dir_all(layout.reflog_dir());
    Ok(())
}
