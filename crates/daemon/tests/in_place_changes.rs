//! What an in-place workspace reports as changed -- everything `git status`
//! would, measured against `HEAD` -- and what it refuses to do: there is no
//! branch of its own to merge or push. Also what `repo.inspect` tells the New
//! Agent dialog about working in place.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::git::{merge, pr, repo, Git};
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{changes, now_rfc3339, DataDirs, Workspace};
use bondsymphonic_proto::*;
use std::path::Path;
use std::sync::Arc;

/// A Ready in-place workspace on `root`, without a sandbox: changes, diff,
/// merge and PR never need one.
async fn in_place_on(dir: &Path, root: &Path, base_branch: &str) -> (Arc<Daemon>, WorkspaceId) {
    let d = Daemon::new(
        DataDirs::new(dir.join("data")),
        backend_for("noop"),
        EventBus::new(16),
    )
    .unwrap();
    let id = WorkspaceId("ws_inplace_changes".into());
    d.registry
        .insert(Workspace {
            id: id.clone(),
            name: "here".into(),
            repo_path: root.into(),
            base_branch: base_branch.into(),
            branch: base_branch.into(),
            worktree_path: root.into(),
            created_at: now_rfc3339(),
            allowlist: vec![],
            state: WorkspaceState::Ready,
            agents: vec![],
            runs: vec![],
            kind: WorkspaceKind::InPlace,
        })
        .await
        .unwrap();
    (d, id)
}

fn summary(r: &ChangesResult) -> Vec<(String, FileStatus)> {
    r.files.iter().map(|f| (f.path.clone(), f.status)).collect()
}

#[tokio::test]
async fn changes_are_what_git_status_says_even_after_the_branch_moved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // Recorded on `main`; the agent then switched and committed. Measured
    // against the merge-base with `main` that commit would be a change; against
    // `HEAD` it is not.
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    common::git_ok(&repo, &["switch", "-q", "-c", "agent"]);
    std::fs::write(repo.join("committed.txt"), "c\n").unwrap();
    common::commit_all(&repo, &[], "agent commit");
    std::fs::write(repo.join("README.md"), "hello\nmore\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "s\n").unwrap();
    common::git_ok(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("loose.txt"), "l\nl\n").unwrap();

    let r = changes::changes(&d, &id).await.unwrap();
    assert_eq!(
        summary(&r),
        [
            ("README.md".to_string(), FileStatus::Modified),
            ("loose.txt".to_string(), FileStatus::Untracked),
            ("staged.txt".to_string(), FileStatus::Added),
        ]
    );
    let readme = r.files.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!((readme.additions, readme.deletions), (1, 0));

    let diff = changes::diff(&d, &id, "README.md").await.unwrap();
    assert_eq!(diff.base_text, "hello\n");
    assert_eq!(diff.work_text, "hello\nmore\n");
    let diff = changes::diff(&d, &id, "committed.txt").await.unwrap();
    assert_eq!(
        diff.base_text, diff.work_text,
        "a committed file is unchanged"
    );
}

#[tokio::test]
async fn a_repository_with_no_commits_is_measured_against_the_empty_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("unborn");
    std::fs::create_dir_all(&root).unwrap();
    common::git_ok(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("first.txt"), "one\n").unwrap();
    common::git_ok(&root, &["add", "first.txt"]);
    let (d, id) = in_place_on(dir.path(), &root, "main").await;

    let r = changes::changes(&d, &id).await.unwrap();
    assert_eq!(summary(&r), [("first.txt".to_string(), FileStatus::Added)]);
    let diff = changes::diff(&d, &id, "first.txt").await.unwrap();
    assert_eq!(
        (diff.base_text.as_str(), diff.work_text.as_str()),
        ("", "one\n")
    );
}

#[tokio::test]
async fn merge_and_pull_request_are_refused_with_the_in_place_reason() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let refs_before = common::git_out(&repo, &["for-each-ref"]);
    for e in [
        merge::merge(&d, &id, MergeMode::Merge, None)
            .await
            .unwrap_err(),
        merge::merge(&d, &id, MergeMode::Squash, Some("s".into()))
            .await
            .unwrap_err(),
        pr::create_pr(&d, &id, "t", "b", false).await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert_eq!(e.data.as_ref().unwrap()["reason"], "in_place");
    }
    assert_eq!(common::git_out(&repo, &["for-each-ref"]), refs_before);
}

/// Plan R3, for the Changes panel: neither kind of workspace may run an
/// embedded repository's config when its changes are read.
#[cfg(unix)]
#[tokio::test]
async fn changes_do_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let marker = dir.path().join("PWNED-in-place");
    common::plant_embedded_repo(&repo, &marker, &[]);
    let _ = std::fs::remove_file(&marker);
    changes::changes(&d, &id).await.unwrap();
    changes::diff(&d, &id, "README.md").await.unwrap();
    assert!(
        !marker.exists(),
        "in place: changes ran the embedded repository's config"
    );

    // The worktree kind, created the ordinary way.
    let other = tempfile::tempdir().unwrap();
    let repo = common::init_repo(other.path());
    let (port, token, daemon, cancel) = common::start_daemon(&other.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = common::create_ws(&mut c, &repo, "wt").await;
    let w = daemon.registry.get(&ws.id).unwrap();
    let env = bondsymphonic_daemon::workspace::lifecycle::layout_for(&daemon, &w)
        .await
        .unwrap()
        .sandbox_git_env();
    let marker = other.path().join("PWNED-worktree");
    common::plant_embedded_repo(&w.worktree_path, &marker, &env);
    let _ = std::fs::remove_file(&marker);
    changes::changes(&daemon, &ws.id).await.unwrap();
    assert!(
        !marker.exists(),
        "worktree: changes ran the embedded repository's config"
    );
    cancel.cancel();
}

#[tokio::test]
async fn inspect_names_the_checked_out_branch_and_whether_it_can_be_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    common::git_ok(&repo, &["switch", "-q", "-c", "feature"]);
    let git = Git::new();
    let info = repo::inspect(&git, &repo).await.unwrap();
    assert_eq!(info.head_branch.as_deref(), Some("feature"));
    assert_eq!(info.in_place_refusal, None);
    assert_eq!(info.hooks_path_in_tree, None);

    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    assert_eq!(repo::inspect(&git, &repo).await.unwrap().head_branch, None);

    let linked = dir.path().join("linked");
    common::git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &linked.to_string_lossy(),
        ],
    );
    let why = repo::inspect(&git, &linked)
        .await
        .unwrap()
        .in_place_refusal
        .unwrap();
    assert!(why.contains("linked worktree"), "{why}");

    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let info = repo::inspect(&git, &plain).await.unwrap();
    assert!(!info.is_repo);
    assert_eq!((info.head_branch, info.hooks_path_in_tree), (None, None));
}

#[tokio::test]
async fn inspect_reports_a_hooks_path_inside_the_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let git = Git::new();
    for (configured, expected) in [
        (".husky/_".to_string(), Some(".husky/_")),
        (
            repo.join("tools/hooks").to_string_lossy().into_owned(),
            Some("tools/hooks"),
        ),
        (
            dir.path().join("elsewhere").to_string_lossy().into_owned(),
            None,
        ),
        (".git/hooks".to_string(), None),
        ("../outside".to_string(), None),
    ] {
        common::git_ok(&repo, &["config", "core.hooksPath", &configured]);
        let info = repo::inspect(&git, &repo).await.unwrap();
        assert_eq!(info.hooks_path_in_tree.as_deref(), expected, "{configured}");
    }
    assert_eq!(repo::hooks_path_in_tree(&repo, "."), Some(".".into()));
    assert_eq!(repo::hooks_path_in_tree(&repo, ""), None);
}

/// Plan R3 again, for the New Agent dialog: `repo.inspect` asks the user's own
/// checkout whether it is dirty, and with an in-place workspace in that
/// checkout the tree it asks about is one an agent writes. Opening the dialog
/// must not be enough to run a program the agent committed.
#[cfg(unix)]
#[tokio::test]
async fn inspect_does_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let marker = dir.path().join("PWNED-inspect");
    common::plant_embedded_repo(&repo, &marker, &[]);
    let _ = std::fs::remove_file(&marker);

    let info = repo::inspect(&Git::new(), &repo).await.unwrap();
    assert!(info.is_repo);
    assert!(
        !marker.exists(),
        "repo.inspect ran the embedded repository's config"
    );
    // And the answer is still right: the dirty file is inside `sub`, which a
    // status that ignores submodules does not count.
    assert!(!info.is_dirty);
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    assert!(repo::inspect(&Git::new(), &repo).await.unwrap().is_dirty);
    assert!(!marker.exists());
}

/// The merge guard that reads the user's checkout, and the conflict listing
/// that runs in it afterwards. Both used to look inside every embedded
/// repository on the way.
#[cfg(unix)]
#[tokio::test]
async fn merging_into_a_checkout_does_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = common::create_ws(&mut c, &repo, "wt").await;
    let w = daemon.registry.get(&ws.id).unwrap();

    // The agent's side of the conflict, in its own worktree.
    let env = bondsymphonic_daemon::workspace::lifecycle::layout_for(&daemon, &w)
        .await
        .unwrap()
        .sandbox_git_env();
    std::fs::write(w.worktree_path.join("README.md"), "theirs\n").unwrap();
    common::commit_all(&w.worktree_path, &env, "theirs");

    // The user's side, in their own checkout, which is on the base branch --
    // so the base-dirty guard runs there -- and which holds an embedded
    // repository whose config names a program.
    let marker = dir.path().join("PWNED-merge");
    common::plant_embedded_repo(&repo, &marker, &[]);
    std::fs::write(repo.join("README.md"), "ours\n").unwrap();
    common::commit_all(&repo, &[], "ours");
    let _ = std::fs::remove_file(&marker);

    let result = merge::merge(
        &daemon,
        &ws.id,
        bondsymphonic_proto::MergeMode::Merge,
        None,
    )
    .await
    .unwrap();
    assert!(!result.ok, "the merge should have conflicted");
    assert_eq!(result.conflicts, ["README.md"]);
    assert!(
        !marker.exists(),
        "the merge ran the embedded repository's config"
    );
    cancel.cancel();
}
