mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{start_daemon, Client};

#[tokio::test]
async fn create_list_get_status_destroy_roundtrip_with_events() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let info: RepoInfo = serde_json::from_value(
        c.call(Request::RepoInspect(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(info.default_branch, "main");

    let id = c
        .send(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await;
    let mut events = Vec::new();
    let ws: WorkspaceInfo =
        serde_json::from_value(c.recv_response(id, &mut events).await.unwrap()).unwrap();
    assert_eq!(ws.branch, "bs/agent-1/work");
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(std::path::Path::new(&ws.worktree_path)
        .join("README.md")
        .exists());
    let states: Vec<String> = events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } => Some(format!("{:?}", info.state)),
            _ => None,
        })
        .collect();
    assert_eq!(states, vec!["Creating", "Ready"]);

    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(list.workspaces.len(), 1);
    let got: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(got.name, "agent-1");

    // Duplicate name in the same repo is a Conflict.
    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // Status sees an untracked file, and destroy without force refuses while dirty.
    std::fs::write(std::path::Path::new(&ws.worktree_path).join("wip.txt"), "x").unwrap();
    let st: WorkspaceStatusResult = serde_json::from_value(
        c.call(Request::WorkspaceStatus(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(st
        .entries
        .iter()
        .any(|e| e.path == "wip.txt" && e.status == FileStatus::Untracked));
    let err = c
        .call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(!std::path::Path::new(&ws.worktree_path).exists());
    assert!(daemon.registry.list().is_empty());
    let err = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

#[tokio::test]
async fn restore_marks_missing_worktree_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, _daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "a".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    cancel.cancel();
    std::fs::remove_dir_all(&ws.worktree_path).unwrap();

    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(&data),
        backend_for("noop"),
        server.event_bus(),
    )
    .unwrap();
    daemon.restore().await;
    let w = daemon.registry.get(&ws.id).unwrap();
    assert!(
        matches!(w.state, WorkspaceState::Error(_)),
        "got {:?}",
        w.state
    );
}

/// Commits inside a workspace worktree the way the sandbox does: new objects land in
/// the workspace's private object directory, so the commit is only reachable from the
/// main store through that directory.
fn commit_in_worktree(worktree: &std::path::Path, env: &[(String, String)], file: &str) {
    std::fs::write(worktree.join(file), "x\n").unwrap();
    for args in [
        ["add", "-A"].as_slice(),
        ["commit", "-q", "-m", "work"].as_slice(),
    ] {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(worktree)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        for (k, v) in env {
            cmd.env(k, v);
        }
        assert!(cmd.status().unwrap().success(), "git {args:?}");
    }
}

fn state_name(s: &WorkspaceState) -> &'static str {
    match s {
        WorkspaceState::Creating => "Creating",
        WorkspaceState::Ready => "Ready",
        WorkspaceState::SandboxDown => "SandboxDown",
        WorkspaceState::Error(_) => "Error",
        WorkspaceState::Destroying => "Destroying",
    }
}

fn state_names(events: &[(Option<WorkspaceId>, Event)]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } => Some(state_name(&info.state)),
            _ => None,
        })
        .collect()
}

async fn create_ws(c: &mut Client, repo: &std::path::Path, name: &str) -> WorkspaceInfo {
    serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: name.into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap()
}

/// A worktree directory that has gone missing (the state `restore` records as
/// `Error("worktree directory is missing")`) must not turn `destroy` into a silent
/// `git branch -D` of commits that live only in the workspace's private object dir.
#[tokio::test]
async fn destroy_refuses_unmerged_commits_when_the_worktree_directory_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;

    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    commit_in_worktree(&w.worktree_path, &layout.sandbox_git_env(), "work.txt");

    cancel.cancel();
    std::fs::remove_dir_all(&w.worktree_path).unwrap();
    daemon.restore().await;
    assert!(
        matches!(
            daemon.registry.get(&ws.id).unwrap().state,
            WorkspaceState::Error(_)
        ),
        "restore should mark the missing worktree as Error"
    );

    let err = lifecycle::destroy(&daemon, &ws.id, false)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    let data = err.data.expect("conflict carries dirty/unmerged");
    assert_eq!(data["unmerged"], serde_json::json!(true));
    assert_eq!(data["dirty"], serde_json::json!(false));
    // The commit is still there: nothing was deleted by the refused destroy.
    assert!(daemon.registry.get(&ws.id).is_some());

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    assert!(daemon.registry.get(&ws.id).is_none());
}

/// A `destroy` that fails after the sandbox is gone must leave the workspace in
/// `Error`, not stranded in `Destroying` forever. The branch delete is made to fail
/// by planting the ref lock file git takes before rewriting the ref.
#[tokio::test]
async fn a_failed_destroy_leaves_the_workspace_in_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;

    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    let lock = layout.ref_dir().join("work.lock");
    std::fs::write(&lock, "").unwrap();

    let err = lifecycle::destroy(&daemon, &ws.id, true).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError, "got {err:?}");
    let left = daemon
        .registry
        .get(&ws.id)
        .expect("entry is kept on failure");
    match &left.state {
        WorkspaceState::Error(m) => assert!(m.contains("destroy failed"), "got {m}"),
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_file(&lock);
    cancel.cancel();
}

/// A create that fails after the registry entry exists must tell the client the
/// workspace went away: `Error` with the reason, then the same terminal `Destroying`
/// event a real destroy emits.
#[tokio::test]
async fn a_failed_create_emits_error_then_destroying() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let id = c
        .send(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "no-such-branch".into(),
            name: "a".into(),
        }))
        .await;
    let mut events = Vec::new();
    let err = c.recv_response(id, &mut events).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert_eq!(state_names(&events), ["Creating", "Error", "Destroying"]);

    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(daemon.registry.list().is_empty());
    cancel.cancel();
}
