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

/// Whether the directory is there, or the call has to fail on the path itself.
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
pub fn exists_as_directory(repo: &Path) -> Result<bool, RpcError> {
    match std::fs::symlink_metadata(repo) {
        // A symlink to a directory is followed here on purpose: this is a path
        // the *user* typed on their own machine, outside every sandbox.
        Ok(_) if repo.is_dir() => Ok(true),
        Ok(_) => Err(RpcError::invalid_params(format!(
            "{} is not a directory",
            repo.display()
        ))),
        // Only "there is nothing here" means the folder can be created. Anything
        // else — a permission error on the parent above all — would otherwise be
        // reported as `exists: false`, and the dialog would offer to create a
        // directory that is already there.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(RpcError::new(
            ErrorCode::IoError,
            format!("cannot read {}: {e}", repo.display()),
        )),
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

/// True when the error is git saying "this is not a repository", and nothing
/// else.
///
/// The distinction decides whether the daemon may *write*: "not a repository
/// yet" is what `init_if_missing` acts on, and every other failure has to stay a
/// failure. Two conditions, because neither is sufficient alone:
///
/// * an exit code at all — without one the process could not be spawned or timed
///   out, and a 60-second timeout on a large repository is the exact failure this
///   pass was written after;
/// * git's own wording. Exit 128 is git's general fatal, shared with a
///   `safe.directory` ownership refusal and an unreadable gitfile — both of which
///   happen *on a real repository*, where initialising would put an empty commit
///   into somebody's work.
pub fn is_not_a_repository(e: &RpcError) -> bool {
    let data = e.data.as_ref();
    data.and_then(|d| d["exit_code"].as_i64()) == Some(128)
        && data
            .and_then(|d| d["stderr"].as_str())
            .is_some_and(|s| s.contains("not a git repository"))
}

/// `path` with as much of it canonicalised as exists, so two spellings of the
/// same directory compare equal even when the last component is not there yet.
pub fn canonical_ish(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical_ish(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether `path` is a repository **of its own**, rather than a directory
/// somewhere inside one.
///
/// `rev-parse` searches upwards, so every question asked with a bare
/// `--git-common-dir` is really a question about the nearest *enclosing*
/// repository. That is the wrong question here twice over: `repo.inspect` would
/// answer a plain folder with its parent repository's branches, dirty state and
/// remotes, and `workspace.create` would quietly make the folder a worktree of
/// that parent — the opposite of the "this folder will be initialised" the
/// dialog just showed. Comparing `--show-toplevel` with the path asks about this
/// directory and no other.
///
/// A path that is not a directory is not a repository; a bare repository is not
/// one either, as far as this daemon is concerned, and says so through the error
/// `--show-toplevel` raises outside a working tree.
pub async fn is_repo_root(git: &Git, path: &Path) -> Result<bool, RpcError> {
    if !path.is_dir() {
        return Ok(false);
    }
    match git
        .run(
            path,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )
        .await
    {
        Ok(o) => Ok(canonical_ish(Path::new(o.stdout.trim())) == canonical_ish(path)),
        Err(e) if is_not_a_repository(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

pub async fn inspect(git: &Git, repo: &Path) -> Result<RepoInfo, RpcError> {
    let exists = exists_as_directory(repo)?;
    // A folder that is not a repository — or is one only by way of a parent — is
    // an answer rather than a failure: the New Agent dialog asks about a folder
    // the user has just picked and offers to initialise it.
    if !exists || !is_repo_root(git, repo).await? {
        return Ok(not_a_repo(exists));
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

/// The daemon user's home, or `None` where the platform will not name one.
///
/// The same lookup [`crate::agents::credentials`] seeds from; it is two lines
/// rather than a shared import so that the git layer does not depend on the
/// agent layer for a question about paths.
fn daemon_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Why `path` must never be turned into a repository, or `None`.
///
/// `init_if_missing` is a write the user asked for through a dialog, and a
/// dialog is where a stale default or a typo is ordinary. These two targets are
/// the ones where `git init` plus a commit would be a mess nobody would connect
/// to what they clicked: a filesystem root, and the home directory of the user
/// the daemon runs as — the home whose `~/.claude.json` and `~/.gitconfig` the
/// daemon reads, and which would become a repository holding everything under it.
///
/// A *directory inside* the home is deliberately allowed: that is where people
/// keep their code. So is a folder that already has files in it, which is the
/// ordinary "I have some code, make it a project" case; the empty commit adds
/// nothing to the index, so those files stay untracked and nothing is lost.
/// [`crate::workspace::lifecycle::create`] adds the one refusal that needs the
/// daemon's own layout: its data directory.
pub fn init_target_refusal(path: &Path, home: Option<&Path>) -> Option<String> {
    let canon = canonical_ish(path);
    if canon.parent().is_none() {
        return Some("it is a filesystem root".into());
    }
    if home.is_some_and(|h| canonical_ish(h) == canon) {
        return Some("it is the home directory of the user the daemon runs as".into());
    }
    None
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
    if let Some(why) = init_target_refusal(path, daemon_home().as_deref()) {
        return Err(RpcError::invalid_params(format!(
            "refusing to initialise a git repository at {}: {why}",
            path.display()
        )));
    }
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
    // This directory, not an enclosing one: a new folder inside somebody else's
    // repository becomes a repository of its own, which is what the dialog
    // offered. `git init` inside a working tree is allowed and makes a nested
    // repository; the enclosing repository is not touched.
    if is_repo_root(git, path).await? {
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
