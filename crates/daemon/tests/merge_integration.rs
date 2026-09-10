//! `workspace.merge` end to end over the noop backend: merge, rebase and
//! squash, the conflict abort, the dirty-base guard and the temporary base
//! worktree the daemon uses when the user's checkout is on another branch.
//!
//! Every assertion about the main repository is made with a plain `git`, with
//! none of the daemon's `GIT_ALTERNATE_OBJECT_DIRECTORIES` in the environment.
//! That is deliberate: a workspace's commits live in a private object
//! directory that goes away with the workspace, so a base branch that is only
//! readable *through* that directory is a base branch that breaks the moment
//! the workspace is destroyed. Reading it the way the user's own git would is
//! what proves the objects were copied into the shared store.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::workspace::lifecycle;
use bondsymphonic_proto::*;
use common::{commit_all, create_ws, init_repo, start_daemon, Client};
use std::path::Path;
use std::sync::Arc;

/// Runs git in `dir` and returns trimmed stdout, with stderr kept out of the
/// test's own output unless the command fails (Windows git narrates every
/// LF/CRLF rewrite there).
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ok(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

/// [`git_out`] with extra environment, for the workspace worktrees: their
/// commits live in a private object directory that a plain git cannot read.
fn git_out_env(dir: &Path, env: &[(String, String)], args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The environment a commit made *inside* the sandbox runs with: new objects go
/// to the workspace's private object directory.
async fn sandbox_env(d: &Arc<Daemon>, id: &WorkspaceId) -> Vec<(String, String)> {
    let ws = d.workspace(id).unwrap();
    lifecycle::layout_for(d, &ws)
        .await
        .unwrap()
        .sandbox_git_env()
}

/// Writes `files` into the workspace worktree and commits them the way the
/// agent in the sandbox would.
async fn commit_in_ws(d: &Arc<Daemon>, ws: &WorkspaceInfo, files: &[(&str, &str)], msg: &str) {
    let env = sandbox_env(d, &ws.id).await;
    let wt = Path::new(&ws.worktree_path);
    for (name, content) in files {
        std::fs::write(wt.join(name), content).unwrap();
    }
    commit_all(wt, &env, msg);
}

async fn merge(
    c: &mut Client,
    id: &WorkspaceId,
    mode: MergeMode,
    message: Option<&str>,
) -> Result<MergeResult, RpcError> {
    let v = c
        .call(Request::WorkspaceMerge(WorkspaceMergeParams {
            workspace_id: id.clone(),
            mode,
            message: message.map(str::to_owned),
        }))
        .await?;
    Ok(serde_json::from_value(v).unwrap())
}

#[tokio::test]
async fn merge_brings_the_workspace_commit_into_the_base_with_a_merge_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    commit_in_ws(
        &daemon,
        &ws,
        &[("README.md", "hello\nalpha\n"), ("alpha.txt", "a\n")],
        "alpha work",
    )
    .await;

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");
    assert!(res.conflicts.is_empty(), "{res:?}");
    assert_eq!(res.reason, None);

    // `--no-ff`, so the base gains a merge commit naming the branch rather than
    // fast-forwarding onto it.
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%p", "main"])
            .split_whitespace()
            .count(),
        2,
        "a --no-ff merge has two parents"
    );
    // The main repo was on the base, so the merge landed in the user's own
    // checkout and the new file is on disk there.
    assert!(
        repo.join("alpha.txt").is_file(),
        "alpha.txt in the checkout"
    );
    assert_eq!(git_out(&repo, &["show", "main:README.md"]), "hello\nalpha");
    // The workspace survives its own merge: destroying it is a separate,
    // explicit act.
    assert!(!git_out(
        &repo,
        &["rev-parse", "--verify", "refs/heads/bs/alpha/work"]
    )
    .is_empty());
    let info: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(info.state, WorkspaceState::Ready);

    cancel.cancel();
}

#[tokio::test]
async fn a_conflicting_merge_aborts_and_leaves_the_base_clean() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    // Both branch from the same commit, so the second one cannot fast-forward
    // over the first one's edit to the same file.
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let beta = create_ws(&mut c, &repo, "beta").await;

    commit_in_ws(&daemon, &alpha, &[("README.md", "hello\nalpha\n")], "alpha").await;
    commit_in_ws(&daemon, &beta, &[("README.md", "hello\nbeta\n")], "beta").await;

    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let beta_tip = git_out(&repo, &["rev-parse", "refs/heads/bs/beta/work"]);
    let main_tip = git_out(&repo, &["rev-parse", "main"]);

    let res = merge(&mut c, &beta.id, MergeMode::Merge, None)
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(res.conflicts, vec!["README.md".to_string()]);
    assert_eq!(res.reason.as_deref(), Some("conflict"));

    // Aborted, not left half-merged: no `MERGE_HEAD`, nothing staged or
    // modified, and neither branch moved.
    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "MERGE_HEAD left behind"
    );
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main_tip);
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/heads/bs/beta/work"]),
        beta_tip
    );

    cancel.cancel();
}

#[tokio::test]
async fn rebase_replays_the_workspace_onto_the_base_and_fast_forwards_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let gamma = create_ws(&mut c, &repo, "gamma").await;

    commit_in_ws(&daemon, &alpha, &[("alpha.txt", "a\n")], "alpha work").await;
    commit_in_ws(&daemon, &gamma, &[("gamma.txt", "g\n")], "gamma work").await;
    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let res = merge(&mut c, &gamma.id, MergeMode::Rebase, None)
        .await
        .unwrap();
    assert!(res.ok, "{res:?}");
    assert!(res.conflicts.is_empty(), "{res:?}");

    // Replayed onto the base and then fast-forwarded, so nothing on the branch
    // is missing from the base and the base has no merge commit for it.
    assert_eq!(
        git_out(&repo, &["rev-list", "main..refs/heads/bs/gamma/work"]),
        ""
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", "main"]),
        git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"])
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "gamma work"
    );
    assert!(repo.join("gamma.txt").is_file());
    assert!(repo.join("alpha.txt").is_file());
    // The workspace worktree is where the rebase ran; it must come out clean.
    assert_eq!(
        git_out(
            Path::new(&gamma.worktree_path),
            &["status", "--porcelain", "--untracked-files=no"]
        ),
        ""
    );

    cancel.cancel();
}

#[tokio::test]
async fn squash_uses_the_request_message_and_falls_back_to_the_last_commit_subject() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let delta = create_ws(&mut c, &repo, "delta").await;

    commit_in_ws(&daemon, &delta, &[("one.txt", "1\n")], "first delta commit").await;
    commit_in_ws(
        &daemon,
        &delta,
        &[("two.txt", "2\n")],
        "second delta commit",
    )
    .await;

    let before = git_out(&repo, &["rev-parse", "main"]);
    let res = merge(
        &mut c,
        &delta.id,
        MergeMode::Squash,
        Some("squashed feature"),
    )
    .await
    .unwrap();
    assert!(res.ok, "{res:?}");
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "delta: squashed feature"
    );
    // Squashed: the branch's two commits arrive as one, with a single parent.
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{before}..main")]),
        "1"
    );
    assert!(repo.join("one.txt").is_file() && repo.join("two.txt").is_file());

    // Without a message the subject is the workspace name and the *last* commit
    // on the branch.
    let eps = create_ws(&mut c, &repo, "epsilon").await;
    commit_in_ws(&daemon, &eps, &[("three.txt", "3\n")], "an earlier subject").await;
    commit_in_ws(&daemon, &eps, &[("four.txt", "4\n")], "the last subject").await;
    let res = merge(&mut c, &eps.id, MergeMode::Squash, None)
        .await
        .unwrap();
    assert!(res.ok, "{res:?}");
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "epsilon: the last subject"
    );

    cancel.cancel();
}

#[tokio::test]
async fn merge_refuses_while_the_base_repo_has_uncommitted_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    // A tracked file edited in the user's own checkout: merging over it would
    // either lose the edit or wedge the checkout.
    std::fs::write(repo.join("README.md"), "hello\nlocal edit\n").unwrap();
    let main_tip = git_out(&repo, &["rev-parse", "main"]);

    let err = c
        .call(Request::WorkspaceMerge(WorkspaceMergeParams {
            workspace_id: ws.id.clone(),
            mode: MergeMode::Merge,
            message: None,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("base_dirty"),
        "{err:?}"
    );
    // Nothing happened: the edit is still there and the base did not move.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main_tip);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "hello\nlocal edit\n"
    );

    // The same uncommitted edit, carried onto another branch. Now the merge
    // goes to a scratch worktree that cannot see this checkout at all, so the
    // guard does not apply: refusing here would refuse the ordinary state of a
    // working developer.
    git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    assert_ne!(git_out(&repo, &["status", "--porcelain"]), "");

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    // Their edit was never touched, and they are still where they were.
    assert_eq!(
        git_out(&repo, &["symbolic-ref", "--short", "HEAD"]),
        "elsewhere"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "hello\nlocal edit\n"
    );

    cancel.cancel();
}

#[tokio::test]
async fn merge_uses_a_temporary_worktree_when_the_repo_is_on_another_branch() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    // The user is working somewhere else entirely; the merge must not move
    // their HEAD or touch their working tree.
    git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");

    assert_eq!(
        git_out(&repo, &["symbolic-ref", "--short", "HEAD"]),
        "elsewhere"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    // The merge landed on the base branch, not in the user's checkout.
    assert!(!repo.join("alpha.txt").exists(), "elsewhere was left alone");
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");

    // The scratch worktree is gone, from disk and from git's own list.
    let scratch = dir.path().join("data").join(format!("merge-{}", ws.id));
    assert!(!scratch.exists(), "{} still on disk", scratch.display());
    let worktrees = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains("merge-"),
        "scratch worktree still registered: {worktrees}"
    );

    cancel.cancel();
}

/// The reason the merge copies objects into the shared store at all.
///
/// A workspace's commits live in a private object directory that `destroy`
/// deletes. Without the copy the base branch would be left pointing at objects
/// that are simply gone: `git log` on the user's own `main` would fail, in the
/// user's own repository, after a merge the daemon reported as successful.
///
/// A second, unmerged workspace is present on purpose. Its commits are in a
/// *different* private directory, which is what makes a repository-wide
/// `git repack -a -d` — daemon design §5.2's suggestion — fail here, and fail
/// destructively.
#[tokio::test]
async fn the_base_still_reads_after_the_merged_workspace_is_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let beta = create_ws(&mut c, &repo, "beta").await;

    commit_in_ws(&daemon, &alpha, &[("alpha.txt", "a\n")], "alpha work").await;
    // Never merged: its objects stay private for the whole of this test.
    commit_in_ws(&daemon, &beta, &[("beta.txt", "b\n")], "beta work").await;

    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let objects = dir
        .path()
        .join("data")
        .join("objects")
        .join(alpha.id.as_str());
    assert!(objects.is_dir(), "the private object directory exists");
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: alpha.id.clone(),
        force: false,
    }))
    .await
    .unwrap();
    assert!(
        !objects.exists(),
        "destroy took the private objects with it"
    );

    // Plain git, as the user would run it, with nothing borrowed. Sorted,
    // because `git log` orders by commit date and all three land in the same
    // second, so their order varies from host to host.
    let mut subjects: Vec<String> = git_out(&repo, &["log", "--format=%s", "main"])
        .lines()
        .map(str::to_owned)
        .collect();
    subjects.sort();
    assert_eq!(subjects, ["Merge bs/alpha/work", "alpha work", "init"]);
    assert_eq!(git_out(&repo, &["show", "main:alpha.txt"]), "a");
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");

    cancel.cancel();
}

/// The rebase abort, which unwinds inside the *workspace* worktree rather than
/// in the base checkout. A rebase left in progress there would leave the agent
/// on a detached HEAD in a half-replayed branch.
#[tokio::test]
async fn a_conflicting_rebase_aborts_and_leaves_the_workspace_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let gamma = create_ws(&mut c, &repo, "gamma").await;

    commit_in_ws(&daemon, &alpha, &[("README.md", "hello\nalpha\n")], "alpha").await;
    commit_in_ws(&daemon, &gamma, &[("README.md", "hello\ngamma\n")], "gamma").await;
    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let gamma_tip = git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"]);
    let main_tip = git_out(&repo, &["rev-parse", "main"]);

    let res = merge(&mut c, &gamma.id, MergeMode::Rebase, None)
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(res.conflicts, vec!["README.md".to_string()]);
    assert_eq!(res.reason.as_deref(), Some("conflict"));

    // The branch was not rewritten and the base did not move.
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"]),
        gamma_tip
    );
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main_tip);

    // No rebase left in progress, and the worktree is back on its own branch
    // with a clean tree. Read with the sandbox environment, since the
    // workspace's own commits live in its private object directory.
    let gitdir = repo.join(".git/worktrees").join(gamma.id.as_str());
    assert!(!gitdir.join("rebase-merge").exists(), "rebase-merge left");
    assert!(!gitdir.join("rebase-apply").exists(), "rebase-apply left");
    let env = sandbox_env(&daemon, &gamma.id).await;
    let wt = Path::new(&gamma.worktree_path);
    assert_eq!(git_out_env(wt, &env, &["status", "--porcelain"]), "");
    assert_eq!(
        git_out_env(wt, &env, &["symbolic-ref", "--short", "HEAD"]),
        "bs/gamma/work"
    );

    cancel.cancel();
}

/// The squash abort. `merge --squash` records no `MERGE_HEAD`, so `merge
/// --abort` has nothing to work with and `reset --merge` is what unwinds it.
#[tokio::test]
async fn a_conflicting_squash_aborts_and_leaves_the_base_clean() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let delta = create_ws(&mut c, &repo, "delta").await;

    commit_in_ws(&daemon, &alpha, &[("README.md", "hello\nalpha\n")], "alpha").await;
    commit_in_ws(&daemon, &delta, &[("README.md", "hello\ndelta\n")], "delta").await;
    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let main_tip = git_out(&repo, &["rev-parse", "main"]);
    let res = merge(&mut c, &delta.id, MergeMode::Squash, Some("squashed"))
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(res.conflicts, vec!["README.md".to_string()]);
    assert_eq!(res.reason.as_deref(), Some("conflict"));

    // Nothing staged, nothing modified, nothing committed.
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main_tip);
    assert!(!repo.join(".git/MERGE_HEAD").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md"))
            .unwrap()
            .replace("\r\n", "\n"),
        "hello\nalpha\n"
    );

    cancel.cancel();
}

/// The merge landed but its objects could not be copied out of the workspace.
///
/// This must never be reported as success. The base branch points at commits
/// that live in the workspace's private object directory, so a client that
/// believed the merge and went on to destroy the workspace would be left with a
/// `main` that no longer resolves.
///
/// `pack.threads` is set to something that is not a number, which makes
/// `git pack-objects` refuse. `git merge` does not read that key, so the merge
/// itself still succeeds and the failure lands exactly where it is wanted.
#[tokio::test]
async fn a_merge_whose_objects_cannot_be_copied_out_is_an_error_not_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    let before = git_out(&repo, &["rev-parse", "main"]);
    git_ok(&repo, &["config", "pack.threads", "not-a-number"]);

    let err = c
        .call(Request::WorkspaceMerge(WorkspaceMergeParams {
            workspace_id: ws.id.clone(),
            mode: MergeMode::Merge,
            message: None,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal, "{err:?}");
    // The flag says the work landed even though the call failed, so the IDE can
    // say so rather than inviting a retry.
    assert_eq!(
        err.data.as_ref().and_then(|d| d["merged"].as_bool()),
        Some(true),
        "{err:?}"
    );
    // The same machine-readable field `base_dirty` and `conflict` use, so the
    // IDE branches on one thing across every merge outcome.
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("objects_stranded"),
        "{err:?}"
    );
    assert!(
        err.message.contains("Do not destroy this workspace"),
        "{err:?}"
    );

    // The merge really did happen; only the copy failed.
    assert_ne!(git_out(&repo, &["rev-parse", "main"]), before);

    cancel.cancel();
}

/// A scratch worktree from a daemon that was killed mid-merge keeps the base
/// branch checked out, and `git worktree prune` cannot clear it because the
/// directory is still there. Left alone it would fail every later merge that
/// needs a scratch worktree, forever.
#[tokio::test]
async fn a_scratch_worktree_left_by_an_earlier_run_does_not_block_a_merge() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    // The user is elsewhere, so this merge needs a scratch worktree of its own.
    git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    // And an earlier one is still sitting on the base branch.
    let stale = data.join("merge-ws_killed");
    git_ok(
        &repo,
        &["worktree", "add", &stale.to_string_lossy(), "main"],
    );
    assert!(stale.is_dir());

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");

    assert!(!stale.exists(), "the stale worktree was not reaped");
    let worktrees = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains("merge-"),
        "a merge worktree is still registered: {worktrees}"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );

    cancel.cancel();
}

/// Two workspaces of one repository merged at the same time. Both move the base
/// branch and both write the shared index and object store, so without the
/// per-repository lock one of them loses on `index.lock`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_merges_of_one_repository_both_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let beta = create_ws(&mut c, &repo, "beta").await;
    // Different files, so anything that goes wrong is a race and not a conflict.
    commit_in_ws(&daemon, &alpha, &[("alpha.txt", "a\n")], "alpha work").await;
    commit_in_ws(&daemon, &beta, &[("beta.txt", "b\n")], "beta work").await;

    // A connection each: the two requests have to be in flight together.
    let mut c1 = Client::connect(port, &token).await;
    let mut c2 = Client::connect(port, &token).await;
    let (ra, rb) = tokio::join!(
        merge(&mut c1, &alpha.id, MergeMode::Merge, None),
        merge(&mut c2, &beta.id, MergeMode::Merge, None),
    );
    let ra = ra.expect("alpha merged");
    let rb = rb.expect("beta merged");
    assert!(ra.ok, "{ra:?}");
    assert!(rb.ok, "{rb:?}");

    // Both landed, in whichever order the lock granted.
    assert!(repo.join("alpha.txt").is_file());
    assert!(repo.join("beta.txt").is_file());
    assert_eq!(
        git_out(&repo, &["rev-list", "main..refs/heads/bs/alpha/work"]),
        ""
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "main..refs/heads/bs/beta/work"]),
        ""
    );

    cancel.cancel();
}

/// The reaper's blast radius. It force-removes worktrees, so what counts as
/// "one of ours" has to be ownership and not a name: a developer's own
/// `git worktree add ../merge-upstream` is registered in the same repository,
/// and reaping it would take their uncommitted work with it.
#[tokio::test]
async fn reaping_scratch_worktrees_leaves_the_users_own_merge_named_worktree_alone() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    // The user's own worktree, named `merge-*` but nowhere near the daemon's
    // data root, with work in it that exists only there.
    let mine = dir.path().join("merge-mine");
    git_ok(
        &repo,
        &["worktree", "add", "-b", "upstream", &mine.to_string_lossy()],
    );
    std::fs::write(mine.join("notes.txt"), "hours of work\n").unwrap();

    // And a scratch worktree of the daemon's own, left by an earlier run.
    let stale = data.join("merge-ws_killed");
    git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    git_ok(
        &repo,
        &["worktree", "add", &stale.to_string_lossy(), "main"],
    );

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");

    // Theirs is untouched, down to the uncommitted file and the registration.
    assert!(mine.is_dir(), "the user's worktree was removed");
    assert_eq!(
        std::fs::read_to_string(mine.join("notes.txt")).unwrap(),
        "hours of work\n"
    );
    let worktrees = git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains("merge-mine"),
        "the user's worktree was unregistered: {worktrees}"
    );
    // The daemon's own was reaped, and so was the one this merge made.
    assert!(!stale.exists(), "the stale scratch worktree was not reaped");
    assert!(!worktrees.contains("merge-ws_killed"), "{worktrees}");
    assert!(
        !worktrees.contains(&format!("merge-{}", ws.id)),
        "{worktrees}"
    );

    cancel.cancel();
}
