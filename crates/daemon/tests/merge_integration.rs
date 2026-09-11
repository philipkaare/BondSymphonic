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

/// [`common::git_out`] with extra environment, for the workspace worktrees: their
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
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%p", "main"])
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
    assert_eq!(
        common::git_out(&repo, &["show", "main:README.md"]),
        "hello\nalpha"
    );
    // The workspace survives its own merge: destroying it is a separate,
    // explicit act.
    assert!(!common::git_out(
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

    let beta_tip = common::git_out(&repo, &["rev-parse", "refs/heads/bs/beta/work"]);
    let main_tip = common::git_out(&repo, &["rev-parse", "main"]);

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
    assert_eq!(common::git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(common::git_out(&repo, &["rev-parse", "main"]), main_tip);
    assert_eq!(
        common::git_out(&repo, &["rev-parse", "refs/heads/bs/beta/work"]),
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
        common::git_out(&repo, &["rev-list", "main..refs/heads/bs/gamma/work"]),
        ""
    );
    assert_eq!(
        common::git_out(&repo, &["rev-parse", "main"]),
        common::git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"])
    );
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "gamma work"
    );
    assert!(repo.join("gamma.txt").is_file());
    assert!(repo.join("alpha.txt").is_file());
    // The workspace worktree is where the rebase ran; it must come out clean.
    assert_eq!(
        common::git_out(
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

    let before = common::git_out(&repo, &["rev-parse", "main"]);
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
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "delta: squashed feature"
    );
    // Squashed: the branch's two commits arrive as one, with a single parent.
    assert_eq!(
        common::git_out(&repo, &["rev-list", "--count", &format!("{before}..main")]),
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
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
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
    let main_tip = common::git_out(&repo, &["rev-parse", "main"]);

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
    assert_eq!(common::git_out(&repo, &["rev-parse", "main"]), main_tip);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "hello\nlocal edit\n"
    );

    // The same uncommitted edit, carried onto another branch. Now the merge
    // goes to a scratch worktree that cannot see this checkout at all, so the
    // guard does not apply: refusing here would refuse the ordinary state of a
    // working developer.
    common::git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    assert_ne!(common::git_out(&repo, &["status", "--porcelain"]), "");

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    // Their edit was never touched, and they are still where they were.
    assert_eq!(
        common::git_out(&repo, &["symbolic-ref", "--short", "HEAD"]),
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
    common::git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");

    assert_eq!(
        common::git_out(&repo, &["symbolic-ref", "--short", "HEAD"]),
        "elsewhere"
    );
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    // The merge landed on the base branch, not in the user's checkout.
    assert!(!repo.join("alpha.txt").exists(), "elsewhere was left alone");
    assert_eq!(common::git_out(&repo, &["status", "--porcelain"]), "");

    // The scratch worktree is gone, from disk and from git's own list.
    let scratch = dir.path().join("data").join(format!("merge-{}", ws.id));
    assert!(!scratch.exists(), "{} still on disk", scratch.display());
    let worktrees = common::git_out(&repo, &["worktree", "list", "--porcelain"]);
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
    let mut subjects: Vec<String> = common::git_out(&repo, &["log", "--format=%s", "main"])
        .lines()
        .map(str::to_owned)
        .collect();
    subjects.sort();
    assert_eq!(subjects, ["Merge bs/alpha/work", "alpha work", "init"]);
    assert_eq!(common::git_out(&repo, &["show", "main:alpha.txt"]), "a");
    assert_eq!(common::git_out(&repo, &["status", "--porcelain"]), "");

    cancel.cancel();
}

/// The same proof for a squash and a rebase. Both leave the base pointing at
/// blobs written into the workspace's private object directory, so a missed
/// `absorb_objects` on either would break the user's repository exactly the way
/// a missed one on a merge would -- and would otherwise pass this suite.
#[tokio::test]
async fn the_base_still_reads_after_a_squashed_or_rebased_workspace_is_destroyed() {
    // A squash makes a *new* commit on the base, so the workspace branch still
    // has commits the base does not contain and an unforced destroy refuses it.
    // A rebase fast-forwards the base onto the branch, so it does not.
    for (mode, name, subject, force) in [
        (MergeMode::Squash, "sq", "sq: sq work", true),
        (MergeMode::Rebase, "rb", "rb work", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
        let mut c = Client::connect(port, &token).await;
        let ws = create_ws(&mut c, &repo, name).await;
        // A second workspace that is never merged, so its objects stay in a
        // *different* private directory for the whole of this case: that is what
        // makes a repository-wide `git repack -a -d` the wrong way to do this.
        let other = create_ws(&mut c, &repo, "kept").await;

        let file = format!("{name}.txt");
        commit_in_ws(
            &daemon,
            &ws,
            &[(file.as_str(), "x\n")],
            &format!("{name} work"),
        )
        .await;
        commit_in_ws(&daemon, &other, &[("kept.txt", "k\n")], "kept work").await;

        let res = merge(&mut c, &ws.id, mode, None).await.unwrap();
        assert!(res.ok, "{name}: {res:?}");

        let objects = dir.path().join("data").join("objects").join(ws.id.as_str());
        assert!(
            objects.is_dir(),
            "{name}: the private object directory exists"
        );
        c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force,
        }))
        .await
        .unwrap();
        assert!(
            !objects.exists(),
            "{name}: destroy took the private objects"
        );

        // Plain git, as the user would run it, borrowing nothing.
        assert_eq!(
            common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
            subject,
            "{name}: the base must read without the destroyed workspace"
        );
        assert_eq!(
            common::git_out(&repo, &["show", &format!("main:{file}")]),
            "x"
        );
        // Walks every object reachable from the base and fails on a missing
        // one. `git fsck` is the wrong tool here: the second, unmerged
        // workspace legitimately keeps its objects private, and a repository-wide
        // check would report those as broken.
        common::git_ok(&repo, &["rev-list", "--objects", "main"]);

        cancel.cancel();
    }
}

/// C2. `destroy` deletes the workspace's private object directory; a merge or a
/// push of the same repository is at that moment copying that directory's
/// contents into the shared store under the per-repository lock. Without the
/// same lock on the teardown the destroy wins, and the base branch is left
/// pointing at objects that are gone: `git log main` and `git fsck` both fail,
/// in the user's own repository, after a merge the daemon reported as
/// successful.
///
/// The lock is held by the test rather than by an artificially slow merge. From
/// `destroy`'s side that is the same thing, and it does not turn the assertion
/// into a race against how long `pack-objects` happens to take.
#[tokio::test]
async fn destroy_waits_for_whoever_holds_the_repository_lock() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;
    assert!(
        merge(&mut c, &ws.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let objects = dir.path().join("data").join("objects").join(ws.id.as_str());
    assert!(objects.is_dir());

    // Standing in for the tail of a merge: the repository lock, held across the
    // `absorb_objects` that copies the merged range out of `objects/<id>`.
    let lock = bondsymphonic_daemon::git::repo_lock(&repo);
    let guard = lock.lock().await;

    // A second connection, because the destroy has to be in flight while this
    // task sits on the lock.
    let mut c2 = Client::connect(port, &token).await;
    let id = ws.id.clone();
    let destroying = tokio::spawn(async move {
        c2.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: id,
            force: false,
        }))
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    assert!(
        !destroying.is_finished(),
        "destroy must wait for the repository lock, not race the absorb"
    );
    assert!(
        objects.is_dir(),
        "and must not have deleted the private objects while the lock is held"
    );

    drop(guard);
    destroying.await.unwrap().unwrap();
    assert!(!objects.exists(), "and finishes once the lock is free");

    let mut subjects: Vec<String> = common::git_out(&repo, &["log", "--format=%s", "main"])
        .lines()
        .map(str::to_owned)
        .collect();
    subjects.sort();
    assert_eq!(subjects, ["Merge bs/alpha/work", "alpha work", "init"]);
    // Every object the base branch reaches, not just the commits.
    common::git_ok(&repo, &["rev-list", "--objects", "main"]);

    cancel.cancel();
}

/// An always-succeeding `<name>` hook that leaves a file behind in `fired` when
/// git runs it.
///
/// `exit 0` throughout: a hook that failed would fail the merge, and these tests
/// have to tell "the hook did not run" apart from "the hook ran and broke
/// something".
fn install_marker_hook(repo: &Path, name: &str, fired: &Path) {
    let marker = common::sh_path(&fired.join(name));
    common::install_hook(repo, name, &format!(": > \"{marker}\"\nexit 0"));
}

/// I5. A merge the daemon performs runs none of the repository's hooks.
///
/// It is not the user typing `git merge`: it happens when they click a button in
/// another window, over content an agent wrote, and `post-merge` / `commit-msg`
/// are repository-supplied shell commands. `daemon_git` pins `core.hooksPath` at
/// the empty daemon-owned directory for the main repository and for the scratch
/// worktree, which is the half `worktree_git` already did for the workspace
/// side. Daemon design 5.4 records the rule, and that filter and merge drivers
/// are deliberately *not* neutralised alongside it.
#[tokio::test]
async fn a_daemon_merge_runs_none_of_the_repositorys_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let fired = dir.path().join("fired");
    std::fs::create_dir_all(&fired).unwrap();

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let beta = create_ws(&mut c, &repo, "beta").await;
    commit_in_ws(&daemon, &alpha, &[("alpha.txt", "a\n")], "alpha work").await;
    commit_in_ws(&daemon, &beta, &[("beta.txt", "b\n")], "beta work").await;

    // Installed only now: the commits above are made by this test with a plain
    // git, which shares this repository's hooks and would fire them itself.
    // Every hook a merge, a squash's commit or the scratch checkout can reach.
    const HOOKS: [&str; 7] = [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "post-merge",
        "post-checkout",
        "reference-transaction",
    ];
    for hook in HOOKS {
        install_marker_hook(&repo, hook, &fired);
    }

    // In the user's own checkout, which is on the base branch.
    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );
    assert!(
        fired_hooks(&fired).is_empty(),
        "a merge in the user's checkout ran hooks: {:?}",
        fired_hooks(&fired)
    );

    // The positive control, and the setup for the scratch-worktree half: the
    // same hooks, in the same repository, do run for a plain `git checkout`.
    // Without it a hook this host declined to execute would make both
    // assertions pass for the wrong reason.
    common::git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    assert!(
        fired_hooks(&fired).contains(&"post-checkout".to_string()),
        "the hooks are installed and this host does run them: {:?}",
        fired_hooks(&fired)
    );
    for name in fired_hooks(&fired) {
        std::fs::remove_file(fired.join(name)).unwrap();
    }

    // The other half of I5: the user is elsewhere, so the daemon checks the base
    // out in a scratch worktree under its own data root and commits there.
    assert!(
        merge(&mut c, &beta.id, MergeMode::Squash, None)
            .await
            .unwrap()
            .ok
    );
    assert!(
        fired_hooks(&fired).is_empty(),
        "a squash in the scratch worktree ran hooks: {:?}",
        fired_hooks(&fired)
    );

    cancel.cancel();
}

/// The hooks that have left a marker in `fired`, sorted.
fn fired_hooks(fired: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fired)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
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

    let gamma_tip = common::git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"]);
    let main_tip = common::git_out(&repo, &["rev-parse", "main"]);

    let res = merge(&mut c, &gamma.id, MergeMode::Rebase, None)
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(res.conflicts, vec!["README.md".to_string()]);
    assert_eq!(res.reason.as_deref(), Some("conflict"));

    // The branch was not rewritten and the base did not move.
    assert_eq!(
        common::git_out(&repo, &["rev-parse", "refs/heads/bs/gamma/work"]),
        gamma_tip
    );
    assert_eq!(common::git_out(&repo, &["rev-parse", "main"]), main_tip);

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

    let main_tip = common::git_out(&repo, &["rev-parse", "main"]);
    let res = merge(&mut c, &delta.id, MergeMode::Squash, Some("squashed"))
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(res.conflicts, vec!["README.md".to_string()]);
    assert_eq!(res.reason.as_deref(), Some("conflict"));

    // Nothing staged, nothing modified, nothing committed.
    assert_eq!(common::git_out(&repo, &["status", "--porcelain"]), "");
    assert_eq!(common::git_out(&repo, &["rev-parse", "main"]), main_tip);
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

    let before = common::git_out(&repo, &["rev-parse", "main"]);
    common::git_ok(&repo, &["config", "pack.threads", "not-a-number"]);

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
    assert_ne!(common::git_out(&repo, &["rev-parse", "main"]), before);

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
    common::git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    // And an earlier one is still sitting on the base branch.
    let stale = data.join("merge-ws_killed");
    common::git_ok(
        &repo,
        &["worktree", "add", &stale.to_string_lossy(), "main"],
    );
    assert!(stale.is_dir());

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");

    assert!(!stale.exists(), "the stale worktree was not reaped");
    let worktrees = common::git_out(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains("merge-"),
        "a merge worktree is still registered: {worktrees}"
    );
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
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
        common::git_out(&repo, &["rev-list", "main..refs/heads/bs/alpha/work"]),
        ""
    );
    assert_eq!(
        common::git_out(&repo, &["rev-list", "main..refs/heads/bs/beta/work"]),
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
    common::git_ok(
        &repo,
        &["worktree", "add", "-b", "upstream", &mine.to_string_lossy()],
    );
    std::fs::write(mine.join("notes.txt"), "hours of work\n").unwrap();

    // And a scratch worktree of the daemon's own, left by an earlier run.
    let stale = data.join("merge-ws_killed");
    common::git_ok(&repo, &["checkout", "-q", "-b", "elsewhere"]);
    common::git_ok(
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
    let worktrees = common::git_out(&repo, &["worktree", "list", "--porcelain"]);
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

/// A conflicting path whose name is not ASCII comes back spelled the way it is
/// spelled on disk.
///
/// `git diff --name-only` is one of the commands that obeys `core.quotePath`,
/// which defaults to *on*: every byte above 0x7f is rendered as a C-style octal
/// escape and the whole name is wrapped in double quotes. A conflict list in
/// that form is useless to the IDE — it cannot open the file, and the user is
/// shown `"h\303\245ndbog.md"` where their file manager shows `håndbog.md`. The
/// daemon turns the option off for every path-listing call.
#[tokio::test]
async fn a_conflict_on_a_non_ascii_path_is_reported_unescaped() {
    const NAME: &str = "håndbog.md";
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    std::fs::write(repo.join(NAME), "fælles\n").unwrap();
    commit_all(&repo, &[], "base");
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let alpha = create_ws(&mut c, &repo, "alpha").await;
    let beta = create_ws(&mut c, &repo, "beta").await;

    commit_in_ws(&daemon, &alpha, &[(NAME, "alfa\n")], "alpha").await;
    commit_in_ws(&daemon, &beta, &[(NAME, "beta\n")], "beta").await;
    assert!(
        merge(&mut c, &alpha.id, MergeMode::Merge, None)
            .await
            .unwrap()
            .ok
    );

    let res = merge(&mut c, &beta.id, MergeMode::Merge, None)
        .await
        .unwrap();
    assert!(!res.ok, "{res:?}");
    assert_eq!(
        res.conflicts,
        vec![NAME.to_string()],
        "the conflict must name the file, not its octal escape"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// Task 4: what counts as a dirty base checkout.
// ---------------------------------------------------------------------------

/// RN5. The dirty-base guard exists because merging over *uncommitted edits to
/// tracked files* either loses them or wedges the checkout half-way through a
/// merge nobody asked for. An untracked file is neither: git will not overwrite
/// one, and a checkout with a scratch file in it — a log, a build output that
/// is not ignored, a note — is the ordinary state of a working directory.
///
/// So the guard asks `git status --porcelain --untracked-files=no`. Both halves
/// are asserted here together, because the value of the change is exactly that
/// it moved one case and not the other.
#[tokio::test]
async fn an_untracked_file_does_not_block_an_in_place_merge_but_an_edit_still_does() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, &[("alpha.txt", "a\n")], "alpha work").await;

    // The user is sitting on the base branch, so the merge lands in their own
    // checkout — the only situation the guard applies to at all.
    assert_eq!(
        common::git_out(&repo, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );

    // A tracked file with an uncommitted edit: still refused, and nothing moves.
    std::fs::write(repo.join("README.md"), "hello\nlocal edit\n").unwrap();
    let main_tip = common::git_out(&repo, &["rev-parse", "main"]);
    let err = merge(&mut c, &ws.id, MergeMode::Merge, None)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("base_dirty"),
        "{err:?}"
    );
    assert_eq!(common::git_out(&repo, &["rev-parse", "main"]), main_tip);

    // Put the tracked file back and leave only an untracked, non-ignored file.
    // Its name is not one the merge brings in, so git has nothing to overwrite.
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("scratch-notes.txt"), "mine\n").unwrap();
    assert_eq!(
        common::git_out(&repo, &["status", "--porcelain"]),
        "?? scratch-notes.txt",
        "the file has to be untracked and not ignored, or this test proves nothing"
    );

    let res = merge(&mut c, &ws.id, MergeMode::Merge, None).await.unwrap();
    assert!(res.ok, "{res:?}");
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
        "Merge bs/alpha/work"
    );
    assert!(
        repo.join("alpha.txt").is_file(),
        "the merge landed in the user's own checkout"
    );
    // And their scratch file is exactly where they left it.
    assert_eq!(
        std::fs::read_to_string(repo.join("scratch-notes.txt")).unwrap(),
        "mine\n"
    );

    cancel.cancel();
}
