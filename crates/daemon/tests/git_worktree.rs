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
    }
}

#[tokio::test]
async fn create_makes_branch_worktree_and_writable_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-1").await;

    worktree::create(&git, &layout, "main").await.unwrap();

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
    let err = worktree::create(&git, &layout, "main").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test]
async fn commits_in_worktree_go_to_private_objects_and_daemon_can_read_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-2").await;
    worktree::create(&git, &layout, "main").await.unwrap();

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

    // The daemon (outside the sandbox) can read the commit via alternates.
    let daemon_git = layout.daemon_git();
    let subject = daemon_git
        .run(&repo_path, &["log", "-1", "--format=%s", "bs/agent-2/work"])
        .await
        .unwrap();
    assert_eq!(subject.stdout.trim(), "agent commit");

    worktree::remove(&git, &layout).await.unwrap();
    assert!(!layout.worktree_path.exists());
    assert!(!repo::branch_exists(&git, &repo_path, "bs/agent-2/work")
        .await
        .unwrap());
    worktree::remove(&git, &layout).await.unwrap(); // idempotent
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
