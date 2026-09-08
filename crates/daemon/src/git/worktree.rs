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
    pub fn daemon_git(&self) -> Git {
        Git::new().with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", s(&self.objects_dir))
    }
}

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
