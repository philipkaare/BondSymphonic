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
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// How long `gh` gets. Longer than [`crate::git::GIT_TIMEOUT`] because opening
/// a pull request is a round trip to github.com, which a slow link or a
/// throttled API can make genuinely slow — but still bounded, so a `gh` that
/// sits waiting on something does not hold the request open forever.
const GH_TIMEOUT: Duration = Duration::from_secs(120);

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

    let mut argv = gh_argv()?;
    let program = argv.remove(0);
    argv.extend(
        [
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
        .map(str::to_owned),
    );
    if draft {
        argv.push("--draft".into());
    }
    // What goes in the error, and therefore on the user's screen. The title and
    // the body are deliberately left out: they are the user's own prose, they
    // can be arbitrarily long, and neither tells anyone anything about why `gh`
    // failed.
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

    let mut cmd = Command::new(&program);
    cmd.args(&argv)
        .current_dir(&ws.repo_path)
        // Nothing here may stop and ask. `gh` with no terminal and no answer
        // would otherwise hold the request open until the timeout.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(git_error(&command, None, &e.to_string())),
        Err(_) => {
            return Err(git_error(
                &command,
                None,
                &format!("timed out after {}s", GH_TIMEOUT.as_secs()),
            ))
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(git_error(&command, out.status.code(), stderr.trim()));
    }
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
                out.status.code(),
                "gh printed no pull request URL, so there is nothing to open",
            )
        })?;
    Ok(CreatePrResult {
        url: url.to_owned(),
    })
}

/// The command to run instead of `gh`, as argv.
///
/// `BS_GH_BIN` is a test and development hook, read from the *daemon's* own
/// environment and never from a request. It is parsed the way a shell would
/// (`"python" "gh_stub.py"` becomes two argv entries) so a stand-in can be an
/// interpreter plus a script — parsed, not run through a shell, so nothing it
/// names is interpreted as shell syntax.
fn gh_argv() -> Result<Vec<String>, RpcError> {
    let Ok(raw) = std::env::var("BS_GH_BIN") else {
        return Ok(vec!["gh".to_owned()]);
    };
    crate::util::argv::split(&raw, "BS_GH_BIN").map_err(RpcError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BS_GH_BIN` is process-wide, so every case that touches it lives in one
    /// test rather than racing the others.
    #[test]
    fn gh_argv_reads_the_hook_and_falls_back_to_gh() {
        std::env::remove_var("BS_GH_BIN");
        assert_eq!(gh_argv().unwrap(), vec!["gh".to_string()]);

        std::env::set_var("BS_GH_BIN", "\"python\" \"/tmp/gh stub.py\"");
        assert_eq!(
            gh_argv().unwrap(),
            vec!["python".to_string(), "/tmp/gh stub.py".to_string()]
        );

        // Set but empty: a hook that names nothing is a misconfiguration, not a
        // silent fall back to the real `gh` the tests must never run.
        std::env::set_var("BS_GH_BIN", "   ");
        assert!(gh_argv().is_err());

        std::env::remove_var("BS_GH_BIN");
    }
}
