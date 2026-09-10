mod common;

use bondsymphonic_proto::*;
use common::{start_daemon, Client};

#[tokio::test]
async fn fs_list_read_write_over_the_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, _cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    let listing: ListDirResult = serde_json::from_value(
        c.call(Request::FsListDir(FsPathParams {
            workspace_id: ws.id.clone(),
            path: "".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(listing
        .entries
        .iter()
        .any(|e| e.name == "README.md" && !e.is_dir));

    c.call(Request::FsWriteFile(FsWriteParams {
        workspace_id: ws.id.clone(),
        path: "src/x.rs".into(),
        content: "fn main() {}".into(),
    }))
    .await
    .unwrap();

    let read: ReadFileResult = serde_json::from_value(
        c.call(Request::FsReadFile(FsPathParams {
            workspace_id: ws.id.clone(),
            path: "src/x.rs".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read.content, "fn main() {}");
    assert_eq!(read.encoding, "utf-8");
    assert!(!read.truncated);

    let err = c
        .call(Request::FsReadFile(FsPathParams {
            workspace_id: ws.id.clone(),
            path: "../secret".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
}
