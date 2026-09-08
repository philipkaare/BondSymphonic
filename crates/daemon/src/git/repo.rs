use super::Git;
use bondsymphonic_proto::{RepoInfo, RpcError};
use std::path::{Path, PathBuf};

pub async fn common_dir(git: &Git, repo: &Path) -> Result<PathBuf, RpcError> {
    let out = git
        .run(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
    Ok(PathBuf::from(out.stdout.trim()))
}

pub async fn branch_exists(git: &Git, repo: &Path, branch: &str) -> Result<bool, RpcError> {
    match git
        .run(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(false),
        Err(e) => Err(e),
    }
}

pub async fn head_commit(git: &Git, repo: &Path, rev: &str) -> Result<String, RpcError> {
    Ok(git
        .run(repo, &["rev-parse", rev])
        .await?
        .stdout
        .trim()
        .to_string())
}

pub async fn inspect(git: &Git, repo: &Path) -> Result<RepoInfo, RpcError> {
    common_dir(git, repo).await?; // fails with GitError if not a repo
    let branches: Vec<String> = git
        .run(
            repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        )
        .await?
        .stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let default_branch = match git
        .run(
            repo,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
        )
        .await
    {
        Ok(o) => o.stdout.trim().trim_start_matches("origin/").to_string(),
        Err(_) => match git
            .run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
        {
            Ok(o) => o.stdout.trim().to_string(),
            Err(_) => branches
                .iter()
                .find(|b| *b == "main" || *b == "master")
                .cloned()
                .or_else(|| branches.first().cloned())
                .unwrap_or_default(),
        },
    };
    let is_dirty = !git
        .run(repo, &["status", "--porcelain"])
        .await?
        .stdout
        .trim()
        .is_empty();
    let remotes: Vec<String> = git
        .run(repo, &["remote"])
        .await?
        .stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(RepoInfo {
        default_branch,
        branches,
        is_dirty,
        remotes,
    })
}
