//! The branches a worktree workspace's agent works on: its own, under the
//! prefix of the branch the workspace was created with (`bs/<name>/`).
//!
//! An agent may create, switch between and delete those inside its sandbox:
//! their refs live in the one directory of the shared `.git` the sandbox can
//! write (see `Layout::rw_git_paths`). Everything the daemon publishes or
//! merges for the workspace -- a push, a pull request, Merge, Rebase, Squash
//! -- takes the branch the worktree is on now, when it is one of these, and
//! the workspace's original branch otherwise (a detached `HEAD`, or a branch
//! of the user's the agent switched to and cannot commit on).

use crate::daemon::Daemon;
use crate::git::Git;
use crate::workspace::lifecycle::layout_for;
use crate::workspace::Workspace;
use bondsymphonic_proto::{RpcError, WorkspaceKind};
use std::path::Path;

/// `bs/<name>/` for a worktree workspace whose branch is `bs/<name>/work`;
/// `None` for an in-place one, which has no branches of its own.
pub fn own_prefix(ws: &Workspace) -> Option<String> {
    if ws.kind != WorkspaceKind::Worktree {
        return None;
    }
    ws.branch
        .rsplit_once('/')
        .map(|(parent, _)| format!("{parent}/"))
}

/// Whether `branch` is one of `ws`'s own: under its prefix, with a name of its
/// own after it, and nothing in it that could step out of the namespace.
pub fn is_own(ws: &Workspace, branch: &str) -> bool {
    own_prefix(ws).is_some_and(|prefix| {
        branch.len() > prefix.len()
            && branch.starts_with(&prefix)
            && !branch.contains("..")
            && !branch.ends_with('/')
    })
}

/// The branch `ws`'s worktree is on, when it is one of the workspace's own;
/// the workspace's original branch otherwise. What a push, a pull request and
/// a merge act on.
pub async fn active_branch(d: &Daemon, ws: &Workspace) -> Result<String, RpcError> {
    if ws.kind != WorkspaceKind::Worktree {
        return Ok(ws.branch.clone());
    }
    let git = layout_for(d, ws).await?.worktree_git();
    let head = git
        .run(
            &ws.worktree_path,
            &["symbolic-ref", "--no-recurse", "-q", "HEAD"],
        )
        .await;
    Ok(match head {
        Ok(out) => out
            .stdout
            .trim()
            .strip_prefix("refs/heads/")
            .filter(|branch| is_own(ws, branch))
            .map_or_else(|| ws.branch.clone(), str::to_owned),
        Err(_) => ws.branch.clone(),
    })
}

/// `ws` as the branch it is on now: a copy whose `branch` is
/// [`active_branch`], for code that reads `ws.branch` as "the branch to
/// publish or merge".
pub async fn on_active_branch(d: &Daemon, ws: &Workspace) -> Result<Workspace, RpcError> {
    let mut ws = ws.clone();
    ws.branch = active_branch(d, &ws).await?;
    Ok(ws)
}

/// Every branch under `prefix` (`bs/<name>/`) in `repo`, by name.
pub async fn branches_under(git: &Git, repo: &Path, prefix: &str) -> Result<Vec<String>, RpcError> {
    let pattern = format!("refs/heads/{}", prefix.trim_end_matches('/'));
    let out = git
        .run(
            repo,
            &["for-each-ref", "--format=%(refname:short)", &pattern],
        )
        .await?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| name.starts_with(prefix))
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(kind: WorkspaceKind) -> Workspace {
        Workspace {
            id: bondsymphonic_proto::WorkspaceId("ws_1".into()),
            name: "agent-1".into(),
            repo_path: "/repo".into(),
            base_branch: "main".into(),
            branch: Workspace::branch_for("agent-1"),
            worktree_path: "/wt".into(),
            created_at: String::new(),
            allowlist: Vec::new(),
            blocklist: Vec::new(),
            state: bondsymphonic_proto::WorkspaceState::Ready,
            agents: Vec::new(),
            runs: Vec::new(),
            kind,
            task: None,
        }
    }

    #[test]
    fn only_names_under_the_workspace_prefix_are_its_own() {
        let w = ws(WorkspaceKind::Worktree);
        assert_eq!(own_prefix(&w).as_deref(), Some("bs/agent-1/"));
        assert!(is_own(&w, "bs/agent-1/work"));
        assert!(is_own(&w, "bs/agent-1/feature/x"));
        assert!(!is_own(&w, "bs/agent-1/"));
        assert!(!is_own(&w, "bs/agent-10/work"));
        assert!(!is_own(&w, "bs/agent-1/../main"));
        assert!(!is_own(&w, "main"));
        let in_place = ws(WorkspaceKind::InPlace);
        assert_eq!(own_prefix(&in_place), None);
        assert!(!is_own(&in_place, "bs/agent-1/work"));
    }
}
