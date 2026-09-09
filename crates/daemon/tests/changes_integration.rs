mod common;

use bondsymphonic_daemon::workspace::lifecycle;
use bondsymphonic_proto::*;
use common::{commit_all, create_ws, init_repo, start_daemon, Client};

#[tokio::test]
async fn changes_and_diff_report_committed_uncommitted_and_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let wt = std::path::Path::new(&ws.worktree_path).to_path_buf();
    let w = daemon.workspace(&ws.id).unwrap();
    let env = lifecycle::layout_for(&daemon, &w)
        .await
        .unwrap()
        .sandbox_git_env();

    // A committed change inside the worktree, an uncommitted edit on top of it,
    // and a new file git has never seen.
    std::fs::write(wt.join("README.md"), "hello\nworld\n").unwrap();
    commit_all(&wt, &env, "work");
    std::fs::write(wt.join("README.md"), "hello\nworld\nagain\n").unwrap();
    std::fs::write(wt.join("notes.txt"), "a\nb\nc\n").unwrap();

    let v = c
        .call(Request::WorkspaceChanges(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let res: ChangesResult = serde_json::from_value(v).unwrap();
    let readme = res
        .files
        .iter()
        .find(|f| f.path == "README.md")
        .expect("README listed");
    assert_eq!(readme.status, FileStatus::Modified);
    assert_eq!((readme.additions, readme.deletions), (2, 0));
    let notes = res
        .files
        .iter()
        .find(|f| f.path == "notes.txt")
        .expect("notes listed");
    assert_eq!(notes.status, FileStatus::Untracked);
    assert_eq!((notes.additions, notes.deletions), (3, 0));

    let v = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.id.clone(),
            path: "README.md".into(),
        }))
        .await
        .unwrap();
    let d: DiffResult = serde_json::from_value(v).unwrap();
    assert_eq!(d.base_text, "hello\n");
    assert_eq!(d.work_text, "hello\nworld\nagain\n");

    // A file that does not exist at the merge-base has an empty base side.
    let v = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.id.clone(),
            path: "notes.txt".into(),
        }))
        .await
        .unwrap();
    let d: DiffResult = serde_json::from_value(v).unwrap();
    assert_eq!(d.base_text, "");
    assert_eq!(d.work_text, "a\nb\nc\n");

    let err = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.id.clone(),
            path: "../outside".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);

    cancel.cancel();
}
