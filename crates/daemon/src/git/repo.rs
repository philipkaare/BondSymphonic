use super::Git;
use bondsymphonic_proto::{ErrorCode, RepoInfo, RpcError};
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

/// The branch a folder this daemon initialises ends up on, and therefore the
/// `default_branch` reported for a folder that is not a repository yet: the
/// answer names the branch the user is about to get, not a guess about one that
/// does not exist.
pub const INITIAL_BRANCH: &str = "main";

/// The answer for a path that git does not recognise as a repository.
fn not_a_repo(exists: bool) -> RepoInfo {
    RepoInfo {
        default_branch: INITIAL_BRANCH.to_string(),
        branches: Vec::new(),
        is_dirty: false,
        remotes: Vec::new(),
        is_repo: false,
        exists,
    }
}

/// Whether the directory itself can be reported on, or the call has to fail.
///
/// Three shapes, and only one of them is an error:
///
/// * a directory — inspect it;
/// * nothing there, but its parent is a directory — it can be created, so the
///   answer is `exists: false` rather than a failure;
/// * anything else — a file where a folder was named, or a path whose parent
///   directory is itself missing. Neither can become a repository, and
///   answering "it will be created" to a mistyped path would have the daemon
///   build a directory tree nobody meant to name.
fn directory_state(repo: &Path) -> Result<bool, RpcError> {
    match std::fs::symlink_metadata(repo) {
        // A symlink to a directory is followed here on purpose: this is a path
        // the *user* typed on their own machine, outside every sandbox.
        Ok(_) if repo.is_dir() => Ok(true),
        Ok(_) => Err(RpcError::invalid_params(format!(
            "{} is not a directory",
            repo.display()
        ))),
        Err(_) => match repo.parent() {
            Some(parent) if parent.as_os_str().is_empty() || parent.is_dir() => Ok(false),
            Some(parent) => Err(RpcError::invalid_params(format!(
                "{} does not exist, and neither does the directory it would go in ({})",
                repo.display(),
                parent.display()
            ))),
            None => Err(RpcError::invalid_params(format!(
                "{} does not exist",
                repo.display()
            ))),
        },
    }
}

/// True when the error is git saying "this is not a repository" rather than git
/// failing to run at all.
///
/// An exit code means git started, read the path and answered; no exit code
/// means the process could not be spawned or timed out, which is a real failure
/// and must not be reported to the user as "not a repository yet".
fn git_ran_and_said_no(e: &RpcError) -> bool {
    e.data
        .as_ref()
        .and_then(|d| d["exit_code"].as_i64())
        .is_some()
}

pub async fn inspect(git: &Git, repo: &Path) -> Result<RepoInfo, RpcError> {
    let exists = directory_state(repo)?;
    if !exists {
        return Ok(not_a_repo(false));
    }
    // Fails with `GitError` when the directory is not a repository, which is an
    // answer rather than a failure: the New Agent dialog asks about a folder the
    // user has just picked and offers to initialise it.
    if let Err(e) = common_dir(git, repo).await {
        if git_ran_and_said_no(&e) {
            return Ok(not_a_repo(true));
        }
        return Err(e);
    }
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
        is_repo: true,
        exists: true,
    })
}

/// Makes `path` a git repository with one empty commit on `main`, creating the
/// directory if it is missing.
///
/// This is what `workspace.create` does for a folder the user picked that is not
/// a repository yet (daemon design §4). The empty commit is not decoration: a
/// repository with no commits has no branch for `git worktree add` to branch
/// from, so a workspace could not be created in it at all.
///
/// **Already a repository is success, not an error.** The call sits in front of
/// `workspace.create`, and a repository that exists is exactly the state the
/// caller wanted; adding a second "Initial commit" to somebody's repository
/// because two workspaces were created at once would be the worst outcome here.
///
/// `git` must arrive with `core.hooksPath` pinned at an empty directory, the way
/// [`crate::git::worktree::Layout::daemon_git`] does it: `git init` copies
/// `init.templateDir` — hooks and all — into the new repository, and the commit
/// that follows would run them. Initialising a folder because a dialog offered
/// to is not the user typing `git commit`.
pub async fn init_repo(git: &Git, path: &Path) -> Result<(), RpcError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) if !path.is_dir() => {
            return Err(RpcError::invalid_params(format!(
                "{} is not a directory",
                path.display()
            )))
        }
        Ok(_) => {}
        Err(_) => std::fs::create_dir_all(path).map_err(|e| {
            RpcError::new(
                ErrorCode::IoError,
                format!("cannot create {}: {e}", path.display()),
            )
        })?,
    }
    if common_dir(git, path).await.is_ok() {
        return Ok(());
    }
    git.run(path, &["init", "-b", INITIAL_BRANCH]).await?;
    // Asked *after* the init, so the new repository's own config counts too, and
    // asked at all because `commit` fails outright without an identity — which
    // is the state of a machine that has never had git configured, and exactly
    // the machine most likely to be starting from a folder that is not a
    // repository yet. Where there is an identity it is left alone: this commit
    // belongs to the user, not to the daemon.
    let has_identity = git
        .run(path, &["config", "--get", "user.email"])
        .await
        .map(|o| !o.stdout.trim().is_empty())
        .unwrap_or(false);
    let mut commit = git.clone();
    if !has_identity {
        commit = commit
            .with_config("user.name", "BondSymphonic")
            .with_config("user.email", "bondsymphonic@localhost");
    }
    // Signing is off for this one commit whatever the config says. It is an
    // empty commit the daemon makes on the user's behalf while a dialog waits,
    // and a `gpg.program` that wants a passphrase would hang it until the 60s
    // git timeout and then fail the workspace.
    commit = commit.with_config("commit.gpgsign", "false");
    commit
        .run(path, &["commit", "--allow-empty", "-m", "Initial commit"])
        .await?;
    Ok(())
}
