//! The agent tools served on each sandbox's `mcp.sock`, end to end.
#![cfg(unix)]
mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo_with_origin, start_daemon, Client};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn rpc(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: &mut tokio::net::unix::OwnedWriteHalf,
    msg: Value,
) -> Value {
    w.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn a_workspace_serves_its_tools_on_its_run_dir_socket_until_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "sock").await;
    let socket = d
        .dirs
        .run(&ws.id)
        .join(bondsymphonic_daemon::mcp::SOCKET_FILE);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let (r, mut w) = tokio::net::UnixStream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut lines = BufReader::new(r).lines();
    let init = rpc(
        &mut lines,
        &mut w,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    )
    .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "bondsymphonic");
    let list = rpc(
        &mut lines,
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 8);
    let fetched = rpc(
        &mut lines,
        &mut w,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"git_fetch","arguments":{}}}),
    )
    .await;
    assert_eq!(fetched["result"]["isError"], false, "{fetched}");

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    assert!(!socket.exists(), "destroy removes the socket");
    // The socket file going says little: it goes with the run directory
    // anyway. What only `stop_workspace` does is close the connection the
    // agent already holds, so a tool call cannot reach a destroyed workspace
    // through it.
    let eof = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
        .await
        .expect("destroy must close the open connection, not leave it hanging");
    assert!(
        matches!(eof, Ok(None) | Err(_)),
        "expected end of stream, got {eof:?}"
    );
    drop(w);
    cancel.cancel();
}

#[tokio::test]
async fn a_restart_serves_a_fresh_listener() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "again").await;
    c.call(Request::WorkspaceRestart(WorkspaceIdParams {
        workspace_id: ws.id.clone(),
    }))
    .await
    .unwrap();
    let socket = d
        .dirs
        .run(&ws.id)
        .join(bondsymphonic_daemon::mcp::SOCKET_FILE);
    let (r, mut w) = tokio::net::UnixStream::connect(&socket)
        .await
        .unwrap()
        .into_split();
    let mut lines = BufReader::new(r).lines();
    let v = rpc(
        &mut lines,
        &mut w,
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    assert_eq!(v["result"], json!({}));
    cancel.cancel();
}
