mod common;

use bondsymphonic_daemon::git::{repo, Git};
use bondsymphonic_proto::ErrorCode;
use common::init_repo;

#[tokio::test]
async fn inspect_reports_default_branch_and_clean_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = init_repo(dir.path());
    let git = Git::new();
    let info = repo::inspect(&git, &repo_path).await.unwrap();
    assert_eq!(info.default_branch, "main");
    assert!(info.branches.contains(&"main".to_string()));
    assert!(!info.is_dirty);
    assert!(info.remotes.is_empty());
    std::fs::write(repo_path.join("dirty.txt"), "x").unwrap();
    assert!(repo::inspect(&git, &repo_path).await.unwrap().is_dirty);
    assert!(repo::branch_exists(&git, &repo_path, "main").await.unwrap());
    assert!(!repo::branch_exists(&git, &repo_path, "nope").await.unwrap());
    assert_eq!(
        repo::head_commit(&git, &repo_path, "main")
            .await
            .unwrap()
            .len(),
        40
    );
}

#[tokio::test]
async fn failures_map_to_git_error_with_details() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new();
    let err = git
        .run(dir.path(), &["rev-parse", "--git-common-dir"])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError);
    let data = err.data.unwrap();
    assert!(data["command"]
        .as_str()
        .unwrap()
        .starts_with("git rev-parse"));
    assert!(data["exit_code"].as_i64().unwrap() != 0);
    assert!(data["stderr"]
        .as_str()
        .unwrap()
        .contains("not a git repository"));
}
