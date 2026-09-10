mod common;

use bondsymphonic_daemon::git::{
    repo,
    worktree::{self, Layout},
    Git,
};
use bondsymphonic_proto::ErrorCode;

async fn layout_for(dir: &std::path::Path, repo_path: &std::path::Path, name: &str) -> Layout {
    let git = Git::new();
    let git_common = repo::common_dir(&git, repo_path).await.unwrap();
    Layout {
        repo: repo_path.to_path_buf(),
        git_common,
        name: name.into(),
        branch: format!("bs/{name}/work"),
        worktree_path: dir.join("worktrees").join("ws_00000001"),
        objects_dir: dir.join("objects").join("ws_00000001"),
        no_hooks_dir: dir.join("nohooks"),
    }
}

#[tokio::test]
async fn create_makes_branch_worktree_and_writable_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-1").await;

    worktree::create(&layout, "main").await.unwrap();

    assert!(layout.worktree_path.join("README.md").exists());
    assert!(
        layout.ref_dir().join("work").is_file(),
        "loose ref file must exist for the rw bind"
    );
    assert!(layout.reflog_dir().is_dir());
    assert!(layout.objects_dir.is_dir());
    assert!(layout.worktree_gitdir().join("HEAD").exists());
    assert!(repo::branch_exists(&git, &repo_path, "bs/agent-1/work")
        .await
        .unwrap());
    assert_eq!(layout.rw_git_paths().len(), 3);

    // Creating again is a Conflict.
    let err = worktree::create(&layout, "main").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test]
async fn commits_in_worktree_go_to_private_objects_and_daemon_can_read_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-2").await;
    worktree::create(&layout, "main").await.unwrap();

    let shared_objects_dir = layout.git_common.join("objects");
    let shared_count_before = walkdir_count(&shared_objects_dir);

    // Simulate the sandboxed agent: git with the sandbox env.
    let mut agent_git = Git::new()
        .with_env("GIT_AUTHOR_NAME", "a")
        .with_env("GIT_AUTHOR_EMAIL", "a@a")
        .with_env("GIT_COMMITTER_NAME", "a")
        .with_env("GIT_COMMITTER_EMAIL", "a@a");
    for (k, v) in layout.sandbox_git_env() {
        agent_git = agent_git.with_env(k, v);
    }
    std::fs::write(layout.worktree_path.join("new.txt"), "agent work\n").unwrap();
    agent_git
        .run(&layout.worktree_path, &["add", "new.txt"])
        .await
        .unwrap();
    agent_git
        .run(
            &layout.worktree_path,
            &["commit", "-q", "-m", "agent commit"],
        )
        .await
        .unwrap();

    // New objects landed in the private dir, not the shared store.
    let private_count = walkdir_count(&layout.objects_dir);
    assert!(
        private_count >= 3,
        "blob+tree+commit expected in private objects, got {private_count}"
    );
    // The shared store must not have grown: this proves exclusivity, not just presence.
    assert_eq!(
        walkdir_count(&shared_objects_dir),
        shared_count_before,
        "shared object store must not receive objects written by the sandboxed agent"
    );

    // The new commit's object file must exist under the private loose-object path
    // and must not exist under the shared loose-object path.
    let head_sha = agent_git
        .run(&layout.worktree_path, &["rev-parse", "HEAD"])
        .await
        .unwrap()
        .stdout
        .trim()
        .to_string();
    let (dir_part, file_part) = head_sha.split_at(2);
    assert!(
        layout.objects_dir.join(dir_part).join(file_part).is_file(),
        "commit object must be present in the private objects dir"
    );
    assert!(
        !shared_objects_dir.join(dir_part).join(file_part).exists(),
        "commit object must be absent from the shared objects dir"
    );

    // The daemon (outside the sandbox) can read the commit via alternates.
    let daemon_git = layout.daemon_git();
    let subject = daemon_git
        .run(&repo_path, &["log", "-1", "--format=%s", "bs/agent-2/work"])
        .await
        .unwrap();
    assert_eq!(subject.stdout.trim(), "agent commit");

    worktree::remove(&layout).await.unwrap();
    assert!(!layout.worktree_path.exists());
    assert!(!repo::branch_exists(&git, &repo_path, "bs/agent-2/work")
        .await
        .unwrap());
    worktree::remove(&layout).await.unwrap(); // idempotent
}

fn walkdir_count(p: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                n += walkdir_count(&path);
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Creating and removing a workspace worktree runs none of the repository's own
/// hooks.
///
/// `git worktree add` fires `post-checkout` and, with the branch it creates,
/// `reference-transaction`; `git branch -D` on the way out fires
/// `reference-transaction` again. Those hooks are arbitrary code out of the
/// user's repository, and neither call is the user typing a git command: they
/// happen when the IDE opens or closes a workspace. Both go through
/// `Layout::daemon_git`, which pins `core.hooksPath` at the daemon's own empty
/// directory.
#[tokio::test]
async fn create_and_remove_run_no_repository_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let marker = dir.path().join("hooks-fired.txt");
    for hook in [
        "post-checkout",
        "reference-transaction",
        "post-index-change",
    ] {
        common::install_hook(
            &repo_path,
            hook,
            &format!("echo {hook} >> '{}'", common::sh_path(&marker)),
        );
    }
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "hooked").await;
    std::fs::create_dir_all(&layout.no_hooks_dir).unwrap();

    worktree::create(&layout, "main").await.unwrap();
    assert!(
        !marker.exists(),
        "worktree create ran repository hooks: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );

    worktree::remove(&layout).await.unwrap();
    assert!(
        !marker.exists(),
        "worktree remove ran repository hooks: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
    assert!(!repo::branch_exists(&git, &repo_path, "bs/hooked/work")
        .await
        .unwrap());

    // The hooks themselves work: without this the assertions above would pass
    // just as well on a host where git never runs hooks at all.
    let plain = dir.path().join("plain");
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "control", &plain.to_string_lossy()])
        .arg("main")
        .current_dir(&repo_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fired = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        fired.contains("post-checkout"),
        "the control worktree must fire the hook, or this test proves nothing: {fired:?}"
    );
}
