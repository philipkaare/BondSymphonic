//! `workspace.merge`: bringing a workspace's branch back into its base branch
//! as a merge, a rebase or a squash.
//!
//! All of it runs on the host, as the daemon, never inside a sandbox. The
//! branch being merged was written by an agent, so nothing here reads
//! configuration or hooks the agent could have put in the worktree: the base
//! side goes through [`Layout::daemon_git`] against the main repository, and
//! the one step that has to happen *in* the workspace worktree — the rebase —
//! goes through [`Layout::worktree_git`], which pins git's discovery and
//! neutralises the config keys git would otherwise execute.

use crate::daemon::Daemon;
use crate::git::worktree::Layout;
use crate::git::{path_arg, repo, Git};
use crate::workspace::lifecycle::layout_for;
use crate::workspace::Workspace;
use bondsymphonic_proto::{
    ErrorCode, MergeMode, MergeResult, RpcError, WorkspaceId, WorkspaceState,
};
use std::path::{Path, PathBuf};

/// A path in the form the filesystem itself uses, for comparing two paths that
/// were spelled differently.
///
/// `git worktree list` prints forward slashes on Windows and the data root is
/// built with backslashes; canonicalising both settles that, along with case
/// and any symlink on the way. Falls back to the path as given when it does not
/// resolve — a registration whose directory is already gone, say — which is
/// safe here because the comparison is only ever used to *narrow* what gets
/// removed.
fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn merged() -> MergeResult {
    MergeResult {
        ok: true,
        conflicts: Vec::new(),
        reason: None,
    }
}

/// Merges, rebases or squashes `id`'s branch into its base branch.
///
/// A conflict is not an error: it comes back as `Ok(MergeResult { ok: false })`
/// with the paths git could not reconcile, because the caller asked a question
/// ("can this go in?") that has been answered. Only the refusals — a base
/// checkout with uncommitted work, a workspace that is not up — and genuine git
/// failures are `Err`.
pub async fn merge(
    d: &Daemon,
    id: &WorkspaceId,
    mode: MergeMode,
    message: Option<String>,
) -> Result<MergeResult, RpcError> {
    let ws = d.workspace(id)?;
    if ws.state != WorkspaceState::Ready {
        return Err(RpcError::invalid_params(format!(
            "workspace {} is not ready",
            ws.id
        )));
    }
    let layout = layout_for(d, &ws).await?;
    let git = layout.daemon_git();

    // One merge at a time per repository. Two merges of two workspaces of the
    // same repository both want to move the base branch and both write the
    // shared index and object store, and nothing else keeps them apart. Taken
    // before the reaper below, which is what makes "anything still there is
    // stale" true.
    let lock = crate::git::repo_lock(&ws.repo_path);
    let _guard = lock.lock().await;

    // Scratch worktrees a killed daemon left behind. Each one keeps the base
    // branch checked out, so `git worktree add <new> <base>` fails with
    // "already used by worktree" until a human deletes it — permanently, since
    // `worktree prune` only forgets registrations whose directory is *gone*.
    // With the lock held no live merge owns one, so every one of them is stale.
    reap_scratch_worktrees(&git, &ws.repo_path, &d.dirs.root).await;

    // Where the base branch gets to move. If the user happens to be sitting on
    // it, that is their checkout and the merge shows up in it. If they are
    // somewhere else, the base branch is not checked out anywhere and git will
    // not let it be advanced from a worktree that is on another branch, so the
    // merge gets a scratch checkout of its own and the user's stays untouched.
    let on_base = current_branch(&git, &ws.repo_path).await.as_deref() == Some(&*ws.base_branch);
    let scratch: Option<PathBuf> = if on_base {
        None
    } else {
        Some(d.dirs.merge_worktree(id))
    };

    // Only when the merge is going to land in the user's own checkout. Merging
    // over uncommitted work either loses it or wedges the checkout half-way
    // through a merge the user never asked for, so that is a refusal before
    // anything moves. On the scratch path their checkout is not involved at
    // all — `worktree add` makes its own clean one — and refusing a merge
    // because a developer happens to have edits open on some other branch would
    // be refusing the ordinary case.
    //
    // `--untracked-files=no`, because an untracked file is neither of the two
    // things this guard is about. Git will not overwrite one — a merge that
    // would lands its own refusal, naming the file — and a checkout with a log,
    // an unignored build output or a scratch note lying in it is the ordinary
    // state of a working directory, not a reason to refuse every merge into it.
    // Counting them made the guard fire on repositories where nothing was at
    // risk, and the only way out was to delete files the user had put there.
    if on_base
        && !git
            .run(
                &ws.repo_path,
                &["status", "--porcelain", "--untracked-files=no"],
            )
            .await?
            .stdout
            .trim()
            .is_empty()
    {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!(
                "{} has uncommitted changes on {}; commit or stash them before merging",
                ws.repo_path.display(),
                ws.base_branch
            ),
        )
        .with_data(serde_json::json!({ "reason": "base_dirty" })));
    }

    // Where the base branch stood before any of this, so the objects it gains
    // can be copied out of the workspace afterwards. Read as a ref rather than
    // from the checkout: with a scratch worktree the base is not checked out
    // anywhere yet.
    let base_ref = format!("refs/heads/{}", ws.base_branch);
    let before = repo::head_commit(&git, &ws.repo_path, &base_ref).await?;

    let outcome = async {
        if let Some(path) = &scratch {
            add_scratch(&git, &ws.repo_path, &ws.base_branch, path).await?;
        }
        let base_dir: &Path = scratch.as_deref().unwrap_or(&ws.repo_path);
        run(&ws, &layout, base_dir, mode, message.as_deref()).await
    }
    .await;

    // Every exit path, the failures included: a scratch worktree left behind
    // keeps the base branch checked out, which makes the *next* merge fail with
    // "already used by worktree".
    if let Some(path) = &scratch {
        remove_scratch(&git, &ws.repo_path, path).await;
    }
    if matches!(&outcome, Ok(r) if r.ok) {
        // Neither of these may be swallowed. The base branch now points at
        // commits that live in the workspace's private object directory, and if
        // they cannot be copied out, reporting success would leave the user one
        // `workspace.destroy` away from a `main` that no longer resolves.
        let after = repo::head_commit(&git, &ws.repo_path, &base_ref)
            .await
            .map_err(|e| crate::git::objects_stranded("merge", "merged", &e.message))?;
        crate::git::absorb_objects(&git, &ws.repo_path, &layout.git_common, &after, &before)
            .await
            .map_err(|e| crate::git::objects_stranded("merge", "merged", &e.message))?;
    }
    outcome
}

/// Removes every scratch worktree this repository still has registered.
///
/// Called with the repository lock held, so nothing here can be in use: a live
/// merge would be holding the lock. Best effort throughout — a scratch worktree
/// that will not go away is a reason to warn, and the `worktree add` that
/// follows will produce the real error if it still matters.
///
/// A candidate has to be both named `merge-*` **and** sitting directly in
/// `scratch_root`, the one directory [`crate::workspace::DataDirs::merge_worktree`]
/// ever puts one in. The name alone is not a claim of ownership: a developer's
/// own `git worktree add ../merge-upstream` is registered in the same
/// repository, and reaping it would take their uncommitted work with it.
async fn reap_scratch_worktrees(git: &Git, repo: &Path, scratch_root: &Path) {
    let listing = match git.run(repo, &["worktree", "list", "--porcelain"]).await {
        Ok(o) => o.stdout,
        Err(e) => {
            tracing::warn!(repo = %repo.display(), "cannot list worktrees: {}", e.message);
            return;
        }
    };
    let root = real(scratch_root);
    let stale: Vec<PathBuf> = listing
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| {
            let named = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("merge-"));
            // The name is not enough on its own. A developer's own
            // `git worktree add ../merge-upstream` in this repository is
            // registered here too, and force-removing it — along with whatever
            // uncommitted work is in it — would be this daemon destroying data
            // it was never asked to touch. Only what this daemon creates, in the
            // one directory it creates it in, is a candidate.
            let ours = p.parent().is_some_and(|parent| real(parent) == root);
            if named && !ours {
                tracing::debug!(path = %p.display(), "not a scratch worktree of this daemon; left alone");
            }
            named && ours
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    for path in &stale {
        tracing::warn!(path = %path.display(), "reaping a merge worktree left by an earlier run");
        if let Err(e) = git
            .run(repo, &["worktree", "remove", "--force", &path_arg(path)])
            .await
        {
            tracing::warn!(path = %path.display(), "removing it failed: {}", e.message);
        }
        let _ = std::fs::remove_dir_all(path);
    }
    if let Err(e) = git.run(repo, &["worktree", "prune"]).await {
        tracing::warn!(repo = %repo.display(), "pruning worktrees failed: {}", e.message);
    }
}

/// The branch the main repository is checked out on, or `None` if it is not on
/// one (a detached HEAD), which is as good a reason to use a scratch worktree.
async fn current_branch(git: &Git, repo: &Path) -> Option<String> {
    git.run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|b| !b.is_empty())
}

async fn add_scratch(git: &Git, repo: &Path, base: &str, path: &Path) -> Result<(), RpcError> {
    // A daemon killed mid-merge leaves both a directory and a registration
    // behind, and either one on its own makes `worktree add` refuse. Clearing
    // both is what keeps one crash from breaking every later merge.
    let _ = git
        .run(repo, &["worktree", "remove", "--force", &path_arg(path)])
        .await;
    let _ = std::fs::remove_dir_all(path);
    let _ = git.run(repo, &["worktree", "prune"]).await;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    git.run(repo, &["worktree", "add", &path_arg(path), base])
        .await?;
    Ok(())
}

/// Best effort, and loud when it fails: the merge itself has already happened
/// or already been aborted, so a scratch worktree that will not go away is a
/// thing to report rather than a reason to fail the call.
async fn remove_scratch(git: &Git, repo: &Path, path: &Path) {
    // Only when there is something to remove: `add_scratch` can fail before it
    // creates anything, and this runs on that path too. `prune` below is what
    // clears a registration without a directory.
    if path.exists() {
        if let Err(e) = git
            .run(repo, &["worktree", "remove", "--force", &path_arg(path)])
            .await
        {
            tracing::warn!(path = %path.display(), "removing the merge worktree failed: {}", e.message);
        }
    }
    let _ = std::fs::remove_dir_all(path);
    if let Err(e) = git.run(repo, &["worktree", "prune"]).await {
        tracing::warn!(repo = %repo.display(), "pruning worktrees after the merge failed: {}", e.message);
    }
}

async fn run(
    ws: &Workspace,
    layout: &Layout,
    base_dir: &Path,
    mode: MergeMode,
    message: Option<&str>,
) -> Result<MergeResult, RpcError> {
    let git = layout.daemon_git();
    match mode {
        MergeMode::Merge => {
            // `--no-ff` so the base keeps a commit that says where this came
            // from even when the branch could have fast-forwarded.
            let msg = format!("Merge {}", ws.branch);
            match git
                .run(base_dir, &["merge", "--no-ff", "-m", &msg, &ws.branch])
                .await
            {
                Ok(_) => Ok(merged()),
                Err(e) => conflict_or_error(&git, base_dir, e, &["merge", "--abort"]).await,
            }
        }
        MergeMode::Squash => squash(&git, ws, base_dir, message).await,
        MergeMode::Rebase => {
            let wt_git = layout.worktree_git();
            let wt = &ws.worktree_path;
            // In the workspace worktree, because that is where the branch is
            // checked out; the base branch is only read.
            if let Err(e) = wt_git
                .run(wt, &["rebase", &ws.base_branch, &ws.branch])
                .await
            {
                return conflict_or_error(&wt_git, wt, e, &["rebase", "--abort"]).await;
            }
            // The branch now sits directly on the base, so the base moves onto
            // it with no merge commit. `--ff-only` is the assertion that this
            // is really so: if the base moved while the rebase ran, that has to
            // surface rather than turn into a merge nobody asked for.
            match git.run(base_dir, &["merge", "--ff-only", &ws.branch]).await {
                Ok(_) => Ok(merged()),
                Err(e) => conflict_or_error(&git, base_dir, e, &["merge", "--abort"]).await,
            }
        }
    }
}

async fn squash(
    git: &Git,
    ws: &Workspace,
    base_dir: &Path,
    message: Option<&str>,
) -> Result<MergeResult, RpcError> {
    if let Err(e) = git.run(base_dir, &["merge", "--squash", &ws.branch]).await {
        // Not `merge --abort`: `--squash` never records a `MERGE_HEAD`, so git
        // has no merge to abort and would refuse. `reset --merge` is what puts
        // the index and the working tree back where they were.
        return conflict_or_error(git, base_dir, e, &["reset", "--merge"]).await;
    }
    // `merge --squash` stages the result and stops without committing. Nothing
    // staged means the branch held nothing the base did not already have, which
    // is a merge that succeeded with no work to do — `git commit` would call it
    // an empty commit and fail.
    if git
        .run(base_dir, &["diff", "--cached", "--name-only"])
        .await?
        .stdout
        .trim()
        .is_empty()
    {
        return Ok(merged());
    }
    let summary = match message.map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => git
            .run(base_dir, &["log", "-1", "--format=%s", &ws.branch])
            .await?
            .stdout
            .trim()
            .to_string(),
    };
    let msg = format!("{}: {summary}", ws.name);
    if let Err(e) = git.run(base_dir, &["commit", "-m", &msg]).await {
        // The squash is staged at this point. Leaving it there would hand the
        // user a checkout full of someone else's staged changes.
        let _ = git.run(base_dir, &["reset", "--merge"]).await;
        return Err(e);
    }
    Ok(merged())
}

/// Reads a failed merge/rebase command as either a conflict or a real failure,
/// and unwinds it either way.
///
/// The exit status cannot tell the two apart — git exits 1 for a conflict and
/// for "your local changes would be overwritten" alike — so the index is what
/// decides: unmerged entries mean a conflict. The abort runs before the answer
/// is returned and regardless of which answer it is, because a command that
/// failed for some other reason can still have left an operation in progress.
async fn conflict_or_error(
    git: &Git,
    dir: &Path,
    err: RpcError,
    abort: &[&str],
) -> Result<MergeResult, RpcError> {
    let conflicts = unmerged_paths(git, dir).await;
    let _ = git.run(dir, abort).await;
    if conflicts.is_empty() {
        return Err(err);
    }
    Ok(MergeResult {
        ok: false,
        conflicts,
        reason: Some("conflict".into()),
    })
}

/// Repo-relative paths with conflict markers, read before anything is aborted.
/// Empty when the command failed for a reason other than a conflict — and also
/// if this read itself fails, which is why the caller treats "no paths" as "not
/// a conflict" and re-raises the original error rather than inventing one.
async fn unmerged_paths(git: &Git, dir: &Path) -> Vec<String> {
    git.run(dir, &["diff", "--name-only", "--diff-filter=U"])
        .await
        .map(|o| {
            o.stdout
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
