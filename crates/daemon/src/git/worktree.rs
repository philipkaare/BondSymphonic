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
    pub fn daemon_git(&self) -> Git {
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
            .with_config("extensions.worktreeConfig", "false");
        for key in NEUTRALISED_CONFIG {
            git = git.with_config(key, "");
        }
        git
    }
}

/// Config keys whose value git executes as a command, cleared on the command
/// line for every daemon-side worktree command.
///
/// Pinning the repository is not by itself enough. The per-worktree gitdir is
/// writable by the agent, and git reads `config.worktree` from it whenever the
/// repository has `extensions.worktreeConfig` enabled. `-c
/// extensions.worktreeConfig=false` does **not** turn that off — git reads the
/// extension out of the repository format during setup, before command-line
/// config exists — so the keys themselves are emptied instead, which does win
/// because `-c` outranks every config file. Measured against git 2.43 and 2.52:
/// `core.fsmonitor` is the one `git status` reaches, and the rest are here so a
/// future daemon-side worktree command does not quietly reopen the hole.
///
/// Any new key git learns to execute has to be added here; that is the cost of
/// leaving the per-worktree gitdir writable, which the agent needs for `HEAD`,
/// the index and its lock files.
const NEUTRALISED_CONFIG: &[&str] = &[
    "core.fsmonitor",
    "core.hooksPath",
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
