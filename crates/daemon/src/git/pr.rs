//! `workspace.create_pr`: push the workspace branch to `origin` and open a
//! pull request with the GitHub CLI.
//!
//! Both halves run on the host as the daemon user, never in a sandbox: `gh`
//! needs the user's GitHub credentials and the push needs the credential helper
//! and SSH agent that go with them, none of which a sandboxed agent may reach.

use crate::daemon::Daemon;
use crate::git::git_error;
use crate::workspace::lifecycle::layout_for;
use crate::workspace::Workspace;
use bondsymphonic_proto::{CreatePrResult, RpcError, WorkspaceId, WorkspaceState};

/// Pushes `id`'s branch to `origin` and opens a pull request for it.
pub async fn create_pr(
    d: &Daemon,
    id: &WorkspaceId,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<CreatePrResult, RpcError> {
    let ws = d.workspace(id)?;
    push_branch(d, &ws, false).await?;

    let mut args: Vec<String> = [
        "pr",
        "create",
        "--title",
        title,
        "--body",
        body,
        "--head",
        &ws.branch,
        "--base",
        &ws.base_branch,
    ]
    .map(str::to_owned)
    .to_vec();
    if draft {
        args.push("--draft".into());
    }
    // What goes in the error, and therefore on the user's screen. The title and
    // the body are deliberately left out: they are the user's own prose, they
    // can be arbitrarily long, and neither tells anyone anything about why `gh`
    // failed.
    let program = crate::git::gh::gh_argv()?.remove(0);
    let command = {
        let mut c = format!(
            "{program} pr create --head {} --base {}",
            ws.branch, ws.base_branch
        );
        if draft {
            c.push_str(" --draft");
        }
        c
    };
    let out = crate::git::gh::run_gh(&ws.repo_path, &args, &command).await?;
    let stdout = out.stdout;
    // `gh pr create` prints the pull request's URL on its own line. It also
    // prints notices around it ("Creating draft pull request…"), so the URL is
    // found rather than assumed to be the whole of stdout.
    let url = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://"))
        .ok_or_else(|| {
            git_error(
                &command,
                None,
                "gh printed no pull request URL, so there is nothing to open",
            )
        })?;
    Ok(CreatePrResult {
        url: url.to_owned(),
    })
}

/// Pushes the workspace's own branch to `origin`, under the repository lock,
/// and copies the objects it carries into the shared store.
///
/// The one push the daemon makes, for `workspace.create_pr` and for an agent's
/// `git_push` tool alike: the branch is always `ws.branch`, never one the
/// caller names. `force` is `--force-with-lease`, for an agent that rebased its
/// branch; it still refuses to overwrite commits it has not seen.
///
/// **The branch ref is agent-writable**, and the push is built so that does
/// not matter. `refs/heads/bs/<name>/` is bind-mounted read-write into the
/// sandbox (see [`crate::git::worktree::Layout::rw_git_paths`]), so the agent
/// can make `ws.branch` a symbolic ref to `refs/heads/main`. A bare
/// `git push origin <branch>` resolves a symref source and takes its *target*
/// as the destination — it would publish the user's local `main` to origin's
/// `main`, and with force rewind it. So: a symbolic branch is refused outright,
/// the commit is resolved here on the host, and the push is an explicit
/// `<sha>:refs/heads/<branch>` whose destination is fixed whatever the ref
/// says by the time git reads it. The lease names its expected value
/// explicitly for the same reason.
pub async fn push_branch(d: &Daemon, ws: &Workspace, force: bool) -> Result<(), RpcError> {
    // Before the state check: an in-place workspace has nothing to merge in
    // any state, and the IDE branches on this reason rather than the prose.
    if ws.kind == bondsymphonic_proto::WorkspaceKind::InPlace {
        return Err(crate::workspace::in_place::nothing_to_merge());
    }
    // The same gate `merge` applies. Pushing out of a workspace that is still
    // `Creating`, or one that failed, would publish a half-formed branch.
    if ws.state != WorkspaceState::Ready {
        return Err(RpcError::invalid_params(format!(
            "workspace {} is not ready",
            ws.id
        )));
    }
    let layout = layout_for(d, ws).await?;
    // `daemon_push_git`, not `daemon_git`: the push must still run the
    // repository's `pre-push` hook, because that is how `git-lfs` uploads the
    // objects the pushed commits point at. See [`Layout::daemon_push_git`].
    let git = layout.daemon_push_git();
    // The probes below run no hooks either way; `daemon_git` because it can
    // read the workspace's private objects and runs nothing of the repository.
    let probe = layout.daemon_git();
    let local = format!("refs/heads/{}", ws.branch);
    {
        // The push and the object copy that follows both write the repository's
        // shared object store, so they take the same per-repository lock a
        // merge does, and a destroy waits on it before deleting the workspace's
        // objects. Callers that go on to talk to GitHub (`create_pr`,
        // `pr_create`) do that after this returns, with the lock released.
        let lock = crate::git::repo_lock(&ws.repo_path);
        let _guard = lock.lock().await;
        // `symbolic-ref -q` exits 0 only for a symbolic ref; 1 is "a plain
        // ref", the normal case. Anything else (a broken repository) fails the
        // `rev-parse` below with git's own message.
        if probe
            .run(&ws.repo_path, &["symbolic-ref", "-q", &local])
            .await
            .is_ok()
        {
            return Err(RpcError::invalid_params(format!(
                "the workspace branch {} is a symbolic ref and will not be pushed; \
                 make it an ordinary branch again (git branch -f {} <commit>)",
                ws.branch, ws.branch
            )));
        }
        let sha = probe
            .run(
                &ws.repo_path,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{local}^{{commit}}"),
                ],
            )
            .await?
            .stdout
            .trim()
            .to_owned();
        let refspec = format!("{sha}:{local}");
        // The lease's expected value is what this repository last saw of the
        // branch on origin; with none, the branch must not exist there yet.
        let lease = if force {
            let tracking = format!("refs/remotes/origin/{}", ws.branch);
            let expected = probe
                .run(&ws.repo_path, &["rev-parse", "--verify", "-q", &tracking])
                .await
                .map(|o| o.stdout.trim().to_owned())
                .unwrap_or_default();
            Some(format!("--force-with-lease={local}:{expected}"))
        } else {
            None
        };
        // Without `-u`: setting an upstream writes `branch.<name>.remote` and
        // `branch.<name>.merge` into `.git/config`, and every write of that file
        // replaces it, which stops any in-place workspace of the same repository
        // (see [`crate::workspace::in_place::ProtectedSnapshot`]). Nothing here
        // needs the tracking: `gh pr create` is given `--head` explicitly, and a
        // later push from the user's own shell is theirs to set up. The objects
        // being pushed live in the workspace's private object directory;
        // `daemon_push_git` carries the alternate that lets git read them.
        let mut args = vec!["push"];
        if let Some(l) = &lease {
            args.push(l);
        }
        args.extend(["origin", refspec.as_str()]);
        git.run(&ws.repo_path, &args).await?;
        // Daemon design §5.2: the borrowed objects are copied into the shared
        // store after the push. A push updates `refs/remotes/origin/<branch>`
        // with or without `-u` (for an explicit `<sha>:<dst>` refspec too: git
        // maps the destination through the remote's fetch refspec), and unlike
        // the local branch that ref is *not* deleted when the workspace is
        // destroyed — it would be left pointing into a directory that no longer
        // exists. Everything the pushed commit adds to the base is what has to
        // be copied — the commit, not the branch name, which the agent may have
        // moved since — and a failure here is reported rather than logged: the
        // push has happened, so the caller has to know the workspace is now
        // holding objects the repository needs.
        crate::git::absorb_objects(
            &git,
            &ws.repo_path,
            &layout.git_common,
            &sha,
            &ws.base_branch,
        )
        .await
        .map_err(|e| crate::git::objects_stranded("push", "pushed", &e.message))?;
    }
    Ok(())
}
