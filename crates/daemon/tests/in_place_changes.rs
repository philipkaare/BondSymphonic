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
            blocklist: vec![],
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
async fn inspect_reports_a_hooks_path_the_agent_could_write() {
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
        ("../outside".to_string(), None),
        // Only the entries the sandbox binds read-only are out of an agent's
        // reach. The rest of `.git` is read-write to it, so a hooks path there
        // names programs it can write, and the dialog has to say so.
        (".git/hooks".to_string(), None),
        (".git/info".to_string(), None),
        (".git/modules".to_string(), None),
        (".git/worktrees".to_string(), None),
        (".git/remotes".to_string(), None),
        (".git/branches".to_string(), None),
        (".git/hooks/nested".to_string(), None),
        (".git/my-hooks".to_string(), Some(".git/my-hooks")),
        // The same place, spelled to look as though it were exempt.
        (".git/hooks/../my-hooks".to_string(), Some(".git/my-hooks")),
        (".git".to_string(), Some(".git")),
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

    let result = merge::merge(&daemon, &ws.id, bondsymphonic_proto::MergeMode::Merge, None)
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

/// Plan I5: an agent can write `refs/replace/<oid>` -- the shared ref store is
/// read-write in a worktree workspace and the whole `.git` is in an in-place
/// one -- and git reads one commit where another is named wherever such a ref
/// exists. That would let an agent choose what the user is shown before they
/// press Merge, and what the merge then merges.
#[tokio::test]
async fn the_daemons_git_does_not_honour_a_planted_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    std::fs::write(repo.join("README.md"), "real\n").unwrap();
    common::commit_all(&repo, &[], "real");
    let real = common::git_out(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("README.md"), "decoy\n").unwrap();
    common::commit_all(&repo, &[], "decoy");
    let decoy = common::git_out(&repo, &["rev-parse", "HEAD"]);
    common::git_ok(
        &repo,
        &["update-ref", &format!("refs/replace/{real}"), &decoy],
    );
    // The agent's own git, and the user's, do honour it.
    assert_eq!(
        common::git_out(&repo, &["log", "-1", "--format=%s", &real]),
        "decoy"
    );

    let git = Git::new();
    assert_eq!(
        git.run(&repo, &["log", "-1", "--format=%s", &real])
            .await
            .unwrap()
            .stdout
            .trim(),
        "real",
        "the daemon's git followed a planted refs/replace"
    );
}

#[tokio::test]
async fn selected_commit_keeps_unrelated_staging_and_treats_paths_literally() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    std::fs::write(repo.join("unrelated.txt"), "staged version\n").unwrap();
    common::git_ok(&repo, &["add", "unrelated.txt"]);
    std::fs::write(repo.join("unrelated.txt"), "working version\n").unwrap();
    std::fs::write(repo.join("[chosen]*.txt"), "literal\n").unwrap();
    std::fs::remove_file(repo.join("README.md")).unwrap();
    let before = common::git_out(&repo, &["ls-files", "--stage", "unrelated.txt"]);
    let prepared = editing::prepare_commit(&d, &id).await.unwrap();
    assert_eq!(prepared.ahead, 0);
    assert_eq!(prepared.files.len(), 3);
    let committed = editing::commit(
        &d,
        &WorkspaceCommitParams {
            workspace_id: id.clone(),
            paths: vec!["[chosen]*.txt".into(), "README.md".into()],
            message: "Selected change".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        committed.commit,
        common::git_out(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        before,
        common::git_out(&repo, &["ls-files", "--stage", "unrelated.txt"])
    );
    assert_eq!(
        common::git_out(&repo, &["show", "HEAD:[chosen]*.txt"]),
        "literal"
    );
    assert!(Git::new()
        .run(&repo, &["cat-file", "-e", "HEAD:README.md"])
        .await
        .is_err());
    assert!(Git::new()
        .run(&repo, &["cat-file", "-e", "HEAD:unrelated.txt"])
        .await
        .is_err());
}

#[tokio::test]
async fn ignore_preview_matches_append_and_keeps_tracked_files() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    std::fs::create_dir(repo.join("a [x]")).unwrap();
    std::fs::write(repo.join("a [x]/draft.tmp"), "draft").unwrap();
    std::fs::write(repo.join(".gitignore"), b"# preserve\r\n").unwrap();
    let options = editing::ignore_options(
        &d,
        &WorkspaceIgnoreOptionsParams {
            workspace_id: id.clone(),
            path: "a [x]/draft.tmp".into(),
            directory: false,
        },
    )
    .await
    .unwrap()
    .options;
    assert_eq!(options.len(), 4);
    assert!(options
        .iter()
        .any(|o| o.path == "." && o.pattern == "/*\n!/.gitignore"));
    for option in [&options[0], &options[3]] {
        let p = WorkspaceIgnoreParams {
            workspace_id: id.clone(),
            path: option.path.clone(),
            target: option.target,
        };
        assert_eq!(
            editing::ignore(&d, &p).await.unwrap().pattern,
            option.pattern
        );
        let once = std::fs::read(repo.join(".gitignore")).unwrap();
        editing::ignore(&d, &p).await.unwrap();
        assert_eq!(std::fs::read(repo.join(".gitignore")).unwrap(), once);
    }
    assert!(common::git_out(&repo, &["check-ignore", "a [x]/draft.tmp"]).contains("draft.tmp"));
    editing::ignore(
        &d,
        &WorkspaceIgnoreParams {
            workspace_id: id.clone(),
            path: ".".into(),
            target: IgnoreTarget::Directory,
        },
    )
    .await
    .unwrap();
    assert!(Git::new()
        .run(&repo, &["check-ignore", ".gitignore"])
        .await
        .is_err());
    assert_eq!(
        common::git_out(&repo, &["ls-files", "README.md"]),
        "README.md"
    );
    assert!(std::fs::read(repo.join(".gitignore"))
        .unwrap()
        .starts_with(b"# preserve\r\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn ignore_refuses_symlink_output_and_commit_runs_no_hooks() {
    use bondsymphonic_daemon::workspace::editing;
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let outside = dir.path().join("outside");
    std::fs::write(&outside, "keep").unwrap();
    symlink(&outside, repo.join(".gitignore")).unwrap();
    assert!(editing::ignore(
        &d,
        &WorkspaceIgnoreParams {
            workspace_id: id.clone(),
            path: "README.md".into(),
            target: IgnoreTarget::File
        }
    )
    .await
    .is_err());
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'hook refused' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join("README.md"), "changed").unwrap();
    let before = common::git_out(&repo, &["rev-parse", "HEAD"]);
    // The hook would refuse; the commit succeeding proves it never ran. An
    // agent can write whatever a hook executes, and the daemon is unsandboxed.
    editing::commit(
        &d,
        &WorkspaceCommitParams {
            workspace_id: id,
            paths: vec!["README.md".into()],
            message: "Attempt".into(),
        },
    )
    .await
    .expect("a pre-commit hook must not run for a daemon commit");
    assert_ne!(common::git_out(&repo, &["rev-parse", "HEAD"]), before);
}

#[tokio::test]
async fn selected_staged_rename_commits_both_paths_and_preserves_other_staging() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    std::fs::create_dir(repo.join("destination")).unwrap();
    common::git_ok(&repo, &["mv", "README.md", "destination/moved.md"]);
    std::fs::write(repo.join("other.txt"), "other").unwrap();
    common::git_ok(&repo, &["add", "other.txt"]);
    let before = common::git_out(&repo, &["ls-files", "--stage", "other.txt"]);
    editing::commit(
        &d,
        &WorkspaceCommitParams {
            workspace_id: id,
            paths: vec!["destination/moved.md".into()],
            message: "Rename selected".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        common::git_out(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "destination/moved.md"
    );
    assert_eq!(
        before,
        common::git_out(&repo, &["ls-files", "--stage", "other.txt"])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ignore_refuses_fifo_without_waiting_for_a_writer() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let path =
        std::ffi::CString::new(repo.join(".gitignore").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(editing::ignore(
        &d,
        &WorkspaceIgnoreParams {
            workspace_id: id,
            path: "README.md".into(),
            target: IgnoreTarget::File
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn prepare_commit_reports_staged_gitlinks_without_entering_submodules() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let sha = common::git_out(&repo, &["rev-parse", "HEAD"]);
    common::git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sha},embedded"),
        ],
    );
    // No checkout or child repository exists: this is an index-only gitlink.
    let prepared = editing::prepare_commit(&d, &id).await.unwrap();
    assert!(prepared
        .files
        .iter()
        .any(|f| f.path == "embedded" && f.staged && f.status == FileStatus::Added));
}

#[tokio::test]
async fn partial_commit_refuses_in_progress_operations_without_changing_index_or_head() {
    use bondsymphonic_daemon::workspace::editing;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    std::fs::write(repo.join("README.md"), "selected").unwrap();
    std::fs::write(repo.join("resolved.txt"), "resolution").unwrap();
    common::git_ok(&repo, &["add", "resolved.txt"]);
    let before = common::git_out(&repo, &["rev-parse", "HEAD"]);
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        let path = repo.join(".git").join(marker);
        if marker.contains("rebase-") || marker == "sequencer" {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, format!("{before}\n")).unwrap();
        }
        let error = editing::commit(
            &d,
            &WorkspaceCommitParams {
                workspace_id: id.clone(),
                paths: vec!["README.md".into()],
                message: "Must preserve operation".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            error.message.contains("in progress"),
            "{marker}: {}",
            error.message
        );
        assert_eq!(common::git_out(&repo, &["rev-parse", "HEAD"]), before);
        assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
        assert!(path.exists());
        if path.is_dir() {
            std::fs::remove_dir(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
}
