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
        head_branch: None,
        in_place_refusal: None,
        hooks_path_in_tree: None,
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

/// What `path` is, as far as git is concerned.
///
/// One question, one answer, asked by everything that has to decide what may be
/// done with a directory. There used to be three: [`is_not_a_repository`]
/// reading git's stderr, a `--show-toplevel` comparison, and a ladder of its own
/// inside `workspace.create` — and they disagreed about the two cases that
/// matter most. A subdirectory of a repository was "not a repository" to one
/// and "a repository" to another, so `repo.inspect` offered to initialise a
/// folder that `workspace.create` then refused; a bare repository was a plain
/// folder to one of them, which is an invitation to run `git init` and a commit
/// inside somebody's remote.
///
/// Five answers, because five is what the callers need:
///
/// * `NotARepo` — a plain directory, or nothing at all. It may be initialised.
/// * `Root` — the top of an ordinary repository with a working tree.
/// * `InsideEnclosing` — a directory *inside* one. `rev-parse` searches
///   upwards, so this is the answer a naive check silently turns into `Root`,
///   and the enclosing root comes with it so the caller can name the repository
///   the user probably meant.
/// * `Bare` — a repository with no working tree. Neither usable as a workspace
///   source nor writable, so every caller turns it into a refusal by name.
/// * `Worktree` — a linked worktree of some repository. Its own checkout,
///   sharing the main store, so it is usable exactly like a `Root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    NotARepo,
    Root,
    InsideEnclosing { root: PathBuf },
    Bare,
    Worktree,
}

/// Asks git what `path` is. See [`RepoKind`].
///
/// A path that is not a directory is `NotARepo`: a file and a name with nothing
/// behind it are both "not a repository", and telling them apart is
/// [`exists_as_directory`]'s job, not this one's.
///
/// Every failure that is *not* one of git's two recognised refusals comes back
/// as an error and stays one. A timeout, a missing binary, a `safe.directory`
/// ownership refusal, an unreadable gitfile — each of those happens on a real
/// repository, and answering `NotARepo` to any of them would send a caller on
/// to initialise somebody's work.
pub async fn classify(git: &Git, path: &Path) -> Result<RepoKind, RpcError> {
    if !path.is_dir() {
        return Ok(RepoKind::NotARepo);
    }
    // One `rev-parse` for all three facts, so the answer cannot be assembled
    // out of two different moments. `--git-dir` against `--git-common-dir` is
    // what tells a linked worktree from the main one: they are the same
    // directory in the main worktree and differ in every linked one.
    let out = match git
        .run(
            path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ],
        )
        .await
    {
        Ok(o) => o,
        Err(e) if is_not_a_repository(&e) => return Ok(RepoKind::NotARepo),
        // `--show-toplevel` is the option that fails here, and it fails because
        // there is no working tree at all.
        Err(e) if is_bare_repository(&e) => return Ok(RepoKind::Bare),
        Err(e) => return Err(e),
    };
    let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let (Some(toplevel), Some(git_dir), Some(common_dir)) =
        (lines.next(), lines.next(), lines.next())
    else {
        return Err(RpcError::internal(format!(
            "git rev-parse did not answer all three questions about {}: {:?}",
            path.display(),
            out.stdout
        )));
    };
    if canonical_ish(Path::new(toplevel)) != canonical_ish(path) {
        // `rev-parse` searches upwards, so this is a folder somewhere inside a
        // repository. Adopting that repository would answer a question nobody
        // asked, so the root is handed back for the caller to name instead.
        return Ok(RepoKind::InsideEnclosing {
            root: PathBuf::from(toplevel),
        });
    }
    if canonical_ish(Path::new(git_dir)) == canonical_ish(Path::new(common_dir)) {
        Ok(RepoKind::Root)
    } else {
        Ok(RepoKind::Worktree)
    }
}

/// True when git refused because there is no working tree — a bare repository.
///
/// Its own wording, not an exit code: 128 is git's general fatal. Told apart
/// from [`is_not_a_repository`] because the two lead opposite ways, one to a
/// folder the daemon may initialise and one to a refusal.
fn is_bare_repository(e: &RpcError) -> bool {
    e.data
        .as_ref()
        .and_then(|d| d["stderr"].as_str())
        .is_some_and(|s| s.contains("must be run in a work tree"))
}

/// The refusal every caller gives for a [`RepoKind::Bare`] path.
///
/// A bare repository has no working tree, so it is neither something a
/// workspace can be made from nor a folder the daemon may write into. Both
/// mistakes are available and both are bad: read as "not a repository" it gets
/// a `git init` and a commit *inside somebody's remote*, and read as a
/// repository it gets a workspace whose worktree cannot be checked out. It is
/// named instead, in a sentence that says what to do about it rather than
/// quoting git.
pub fn bare_repository_error(path: &Path) -> RpcError {
    RpcError::invalid_params(format!(
        "{} is a bare repository; BondSymphonic needs a checkout (clone it first)",
        path.display()
    ))
}

/// The refusal `workspace.create` gives for a [`RepoKind::NotARepo`] path when
/// the client did not ask for it to be initialised.
///
/// [`ErrorCode::GitError`] rather than `InvalidParams`, and "not a git
/// repository" in the message, because that is the answer clients have always
/// had here: it used to be git's own error, passed through. A client that
/// predates `init_if_missing` branches on it.
pub fn not_a_repository_error(path: &Path) -> RpcError {
    RpcError::new(
        ErrorCode::GitError,
        format!("{} is not a git repository", path.display()),
    )
}

/// Every local branch, in git's own order.
async fn local_branches(git: &Git, repo: &Path) -> Result<Vec<String>, RpcError> {
    Ok(git
        .run(
            repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        )
        .await?
        .stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// The branch the repository itself calls its default, or `None` when it does
/// not say.
///
/// `refs/remotes/origin/HEAD` first, because that is the remote's answer and
/// the one a clone was made against; the local `HEAD` second, for a repository
/// with no remote. Neither failing is an error — a repository can have neither
/// — so the caller falls back to the branch list instead.
async fn configured_head(git: &Git, repo: &Path) -> Result<Option<String>, RpcError> {
    if let Ok(o) = git
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
        return Ok(Some(
            o.stdout.trim().trim_start_matches("origin/").to_string(),
        ));
    }
    Ok(git
        .run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string()))
}

/// Whether the checkout has uncommitted changes.
///
/// `--untracked-files=no`, the same question `workspace.merge`'s guard asks of
/// the same checkout. An untracked file is not a change a merge can destroy —
/// git refuses by name rather than overwriting one — and a log, a build output
/// or a scratch note lying in a working directory is its ordinary state, not
/// something to announce as uncommitted work. With the two disagreeing, the New
/// Agent dialog said "Repository has uncommitted changes" about a repository
/// the daemon would merge into without a murmur.
async fn is_dirty(git: &Git, repo: &Path) -> Result<bool, RpcError> {
    Ok(!git
        .run(repo, &["status", "--porcelain", "--untracked-files=no"])
        .await?
        .stdout
        .trim()
        .is_empty())
}

async fn remotes(git: &Git, repo: &Path) -> Result<Vec<String>, RpcError> {
    Ok(git
        .run(repo, &["remote"])
        .await?
        .stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// The branch checked out, or `None` for a detached `HEAD`.
async fn head_branch(git: &Git, repo: &Path) -> Result<Option<String>, RpcError> {
    match git
        .run(repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
    {
        Ok(o) => Ok(Some(o.stdout.trim().to_string())),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `core.hooksPath` as the repository's config sets it, or `None`.
async fn configured_hooks_path(git: &Git, repo: &Path) -> Result<Option<String>, RpcError> {
    match git.run(repo, &["config", "--get", "core.hooksPath"]).await {
        Ok(o) => Ok(Some(o.stdout.trim().to_string())),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `configured` (a `core.hooksPath` value) relative to `root`, with `/`
/// separators, when it resolves inside the working tree -- `.` for the root
/// itself -- and `None` when it resolves outside it or inside `.git`, which an
/// in-place agent cannot write. A relative value is relative to the root,
/// which is where git runs hooks from in a repository with a working tree.
pub fn hooks_path_in_tree(root: &Path, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    let expanded = match configured.strip_prefix("~/") {
        Some(rest) => daemon_home()?.join(rest),
        None => PathBuf::from(configured),
    };
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    };
    let resolved = canonical_ish(&normalise(&resolved));
    let root = canonical_ish(root);
    let rel = resolved.strip_prefix(&root).ok()?;
    if rel.starts_with(".git") {
        return None;
    }
    if rel.as_os_str().is_empty() {
        return Some(".".into());
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// `..` and `.` taken out lexically, so a hooks path that climbs out of the
/// root is seen to, even when the directory it names does not exist.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub async fn inspect(git: &Git, repo: &Path) -> Result<RepoInfo, RpcError> {
    // A folder that is not a repository — or is one only by way of a parent — is
    // an answer rather than a failure: the New Agent dialog asks about a folder
    // the user has just picked and offers to initialise it. A bare repository is
    // the one shape that stays a refusal, because neither answer is true of it.
    if !exists_as_directory(repo)? {
        return Ok(not_a_repo(false));
    }
    let kind = classify(git, repo).await?;
    match &kind {
        RepoKind::Root | RepoKind::Worktree => {}
        RepoKind::NotARepo => return Ok(not_a_repo(true)),
        RepoKind::InsideEnclosing { root } => {
            // Reported as "not a repository", which is what the folder is, and
            // logged with the repository it sits in so the daemon log says which
            // one a client was really looking at. `RepoInfo` has no field for
            // it; `workspace.create` is where the user is told the name.
            tracing::debug!(
                path = %repo.display(),
                root = %root.display(),
                "a folder inside a repository is not a repository itself"
            );
            return Ok(not_a_repo(true));
        }
        RepoKind::Bare => return Err(bare_repository_error(repo)),
    }
    // Six independent questions, asked at once. `repo.inspect` is what the New
    // Agent dialog waits on while the user looks at an empty panel, and on a
    // large repository under `/mnt/c` the answers took long enough in
    // sequence to reach the IDE's 30 s timeout. None of them reads what another
    // wrote, so nothing here depends on the order they come back in; the branch
    // list is only *consulted* by the default-branch fallback, which is done
    // below once both have arrived.
    //
    // `try_join!` rather than spawned tasks: these are child processes, so
    // there is nothing for a thread to do but wait, and the first failure still
    // ends the call with that failure the way the sequence did.
    let (branches, configured_head, is_dirty, remotes, head_branch, hooks_path) = tokio::try_join!(
        local_branches(git, repo),
        configured_head(git, repo),
        is_dirty(git, repo),
        remotes(git, repo),
        head_branch(git, repo),
        configured_hooks_path(git, repo),
    )?;
    let default_branch = configured_head.unwrap_or_else(|| {
        branches
            .iter()
            .find(|b| *b == "main" || *b == "master")
            .cloned()
            .or_else(|| branches.first().cloned())
            .unwrap_or_default()
    });
    Ok(RepoInfo {
        default_branch,
        branches,
        is_dirty,
        remotes,
        is_repo: true,
        exists: true,
        head_branch,
        in_place_refusal: crate::workspace::in_place::in_place_refusal(&kind, repo),
        hooks_path_in_tree: hooks_path.and_then(|p| hooks_path_in_tree(repo, &p)),
    })
}

/// The daemon user's home, or `None` where the platform will not name one.
///
/// The same lookup [`crate::agents::credentials`] seeds from; it is two lines
/// rather than a shared import so that the git layer does not depend on the
/// agent layer for a question about paths.
pub(crate) fn daemon_home() -> Option<PathBuf> {
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
    match classify(git, path).await? {
        // Already a repository is success, not an error — see above.
        RepoKind::Root | RepoKind::Worktree => return Ok(()),
        RepoKind::Bare => return Err(bare_repository_error(path)),
        RepoKind::NotARepo | RepoKind::InsideEnclosing { .. } => {}
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
