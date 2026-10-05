//! `workspace.create_pr`: push the workspace branch to `origin` and open a
//! pull request with the GitHub CLI.
//!
//! Both halves run on the host as the daemon user, never in a sandbox: `gh`
//! needs the user's GitHub credentials and the push needs the credential helper
//! and SSH agent that go with them, none of which a sandboxed agent may reach.

use crate::daemon::Daemon;
use crate::git::git_error;
use crate::workspace::lifecycle::layout_for;
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
    let layout = layout_for(d, &ws).await?;
    // `daemon_push_git`, not `daemon_git`: the push must still run the
    // repository's `pre-push` hook, because that is how `git-lfs` uploads the
    // objects the pushed commits point at. See [`Layout::daemon_push_git`].
    let git = layout.daemon_push_git();
    {
        // The push and the object copy that follows both write the repository's
        // shared object store, so they take the same per-repository lock a
        // merge does. Released before `gh` runs: opening a pull request is a
        // network round trip that touches nothing local, and holding a
        // repository's merges behind it for two minutes would be its own bug.
        let lock = crate::git::repo_lock(&ws.repo_path);
        let _guard = lock.lock().await;
        // Without `-u`: setting an upstream writes `branch.<name>.remote` and
        // `branch.<name>.merge` into `.git/config`, and every write of that file
        // replaces it, which stops any in-place workspace of the same repository
        // (see [`crate::workspace::in_place::ProtectedSnapshot`]). Nothing here
        // needs the tracking: `gh pr create` is given `--head` explicitly, and a
        // later push from the user's own shell is theirs to set up. The objects
        // being pushed live in the workspace's private object directory;
        // `daemon_git` is what lets git read them.
        git.run(&ws.repo_path, &["push", "origin", &ws.branch])
            .await?;
        // Daemon design §5.2: the borrowed objects are copied into the shared
        // store after the push. A push updates `refs/remotes/origin/<branch>`
        // with or without `-u`, and unlike the local branch that ref is *not*
        // deleted when the workspace is destroyed — it would be left pointing
        // into a directory that no longer exists. Everything the branch adds to
        // the base is what has to be copied, and a failure here is reported
        // rather than logged: the push has happened, so the caller has to know
        // the workspace is now holding objects the repository needs.
        crate::git::absorb_objects(
            &git,
            &ws.repo_path,
            &layout.git_common,
            &ws.branch,
            &ws.base_branch,
        )
        .await
        .map_err(|e| crate::git::objects_stranded("push", "pushed", &e.message))?;
    }

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
