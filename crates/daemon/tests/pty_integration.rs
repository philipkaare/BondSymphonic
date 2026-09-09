//! `pty.*` over the wire: open a terminal in a workspace sandbox, type into it,
//! resize it, and see `pty.output` / `pty.exit` events arrive on the connection.

mod common;

use base64::Engine;
use bondsymphonic_proto::*;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

#[tokio::test]
async fn pty_open_echo_resize_close_over_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "p".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    let command = if cfg!(windows) {
        Some("cmd".to_string())
    } else {
        Some("sh".to_string())
    };
    let opened: PtyOpenResult = serde_json::from_value(
        c.call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let marker = "bs-marker-4242";
    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: opened.pty_id.clone(),
        data_b64: b64(&format!("echo {marker}\r\n")),
    }))
    .await
    .unwrap();
    c.call(Request::PtyResize(PtyResizeParams {
        pty_id: opened.pty_id.clone(),
        cols: 120,
        rows: 40,
    }))
    .await
    .unwrap();

    // The marker shows up twice: once as the terminal's echo of the typed line,
    // once as the shell's output. Waiting for both proves the shell really ran
    // the command rather than only echoing it back.
    //
    // A fast shell can produce both before the `pty.resize` reply above, so the
    // events those calls buffered are drained here as well; harvesting only the
    // ones that ride along with the poll below would lose them.
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && collected.matches(marker).count() < 2 {
        let mut events = c.drain_events();
        // Any request will do; the events queued for this connection ride along.
        let id = c
            .send(Request::WorkspaceGet(WorkspaceIdParams {
                workspace_id: ws.id.clone(),
            }))
            .await;
        c.recv_response(id, &mut events).await.unwrap();
        for (_, e) in events {
            if let Event::PtyOutput { data_b64, .. } = e {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap();
                collected.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(collected.contains(marker), "collected: {collected}");

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: opened.pty_id.clone(),
    }))
    .await
    .unwrap();
    let mut saw_exit = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_exit {
        // The exit can beat the `pty.close` reply, so its call's events count too.
        let mut events = c.drain_events();
        let id = c.send(Request::WorkspaceList {}).await;
        c.recv_response(id, &mut events).await.unwrap();
        saw_exit = events
            .iter()
            .any(|(_, e)| matches!(e, Event::PtyExit { pty_id, .. } if *pty_id == opened.pty_id));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saw_exit, "pty.exit event expected after close");

    // The session is gone once it has exited, so later calls fault cleanly.
    let err = c
        .call(Request::PtyWrite(PtyWriteParams {
            pty_id: opened.pty_id.clone(),
            data_b64: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

/// An unparseable `command` is the caller's mistake, not a sandbox failure.
#[tokio::test]
async fn pty_open_rejects_an_unbalanced_command() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "q".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let err = c
        .call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command: Some("echo \"unterminated".into()),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    cancel.cancel();
}

/// Destroying a workspace has to take its terminals down with it.
#[tokio::test]
async fn destroying_a_workspace_closes_its_ptys() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "r".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let command = if cfg!(windows) {
        Some("cmd".to_string())
    } else {
        Some("sh".to_string())
    };
    let opened: PtyOpenResult = serde_json::from_value(
        c.call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut gone = false;
    while tokio::time::Instant::now() < deadline && !gone {
        gone = matches!(
            c.call(Request::PtyWrite(PtyWriteParams {
                pty_id: opened.pty_id.clone(),
                data_b64: String::new(),
            }))
            .await,
            Err(e) if e.code == ErrorCode::NotFound
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(gone, "the workspace's pty should be closed and forgotten");
    cancel.cancel();
}
