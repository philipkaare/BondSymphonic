//! `fs.watch` end to end: enabling a watch turns worktree writes into a single
//! coalesced `fs.changed` with repo-relative paths, a burst of writes is one
//! event and not one per file, ignored directories stay silent, and disabling
//! the watch stops the events.

mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::time::{Duration, Instant};

/// Waits up to `limit` for an `fs.changed` for `ws`, pumping the connection so
/// buffered events reach the client: `Client` only surfaces events it read
/// while waiting for a response, so a cheap request is what delivers them.
async fn wait_for_fs_changed(
    c: &mut Client,
    ws: &WorkspaceId,
    limit: Duration,
) -> Option<Vec<String>> {
    let start = Instant::now();
    while start.elapsed() < limit {
        for (id, ev) in c.drain_events() {
            if let (Some(id), Event::FsChanged { paths }) = (id, ev) {
                if &id == ws {
                    return Some(paths);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = c.call(Request::WorkspaceList {}).await;
    }
    None
}

/// The headline behaviour: two files written inside one [`DEBOUNCE`] window
/// arrive as a single `fs.changed` carrying both paths sorted, not as one event
/// per file. A sliding debounce window, or a publish moved inside the receive
/// loop, would fail here and nowhere else in the suite.
#[tokio::test]
async fn a_burst_of_writes_is_one_event_with_sorted_paths() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "burst").await;

    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: true,
    }))
    .await
    .unwrap();

    // Written straight to the worktree rather than through `fs.write_file`: no
    // RPC round trip between the two writes, so the second one is comfortably
    // inside the 200 ms window the first one opens. Written in reverse
    // alphabetical order, so the assertion below tests the sort rather than
    // arrival order.
    let worktree = std::path::Path::new(&ws.worktree_path).to_path_buf();
    std::fs::write(worktree.join("zebra.txt"), "z\n").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(worktree.join("alpha.txt"), "a\n").unwrap();

    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("fs.changed arrives");
    assert_eq!(
        paths,
        vec!["alpha.txt".to_string(), "zebra.txt".to_string()],
        "one event for the whole burst, paths sorted"
    );
    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_millis(300))
            .await
            .is_none(),
        "the burst is published once, not once per file"
    );

    cancel.cancel();
}

#[tokio::test]
async fn watch_reports_relative_paths_debounced_and_ignores_target_dir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: true,
    }))
    .await
    .unwrap();
    // Idempotent: a second enable must not install a second watcher.
    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: true,
    }))
    .await
    .unwrap();

    c.call(Request::FsWriteFile(FsWriteParams {
        workspace_id: ws.id.clone(),
        path: "hello.txt".into(),
        content: "hi\n".into(),
    }))
    .await
    .unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("fs.changed arrives");
    assert_eq!(
        paths,
        vec!["hello.txt".to_string()],
        "relative path, no temp file"
    );

    let worktree = std::path::Path::new(&ws.worktree_path).to_path_buf();
    std::fs::create_dir_all(worktree.join("target")).unwrap();
    std::fs::write(worktree.join("target/out.bin"), b"x").unwrap();
    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1))
            .await
            .is_none(),
        "target/ is ignored"
    );

    // Ignored at any depth, not only at the worktree root: this is the monorepo
    // layout the filter mostly exists for. The `packages/` and `packages/web/`
    // parents are ordinary directories, so they are reported; waiting for that
    // event both clears it and confirms the nested path is otherwise watched.
    std::fs::create_dir_all(worktree.join("packages/web/node_modules/dep")).unwrap();
    wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("the non-ignored parent directories are reported");
    std::fs::write(
        worktree.join("packages/web/node_modules/dep/index.js"),
        b"x",
    )
    .unwrap();
    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1))
            .await
            .is_none(),
        "a nested node_modules/ is ignored too"
    );

    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: false,
    }))
    .await
    .unwrap();
    c.call(Request::FsWriteFile(FsWriteParams {
        workspace_id: ws.id.clone(),
        path: "after.txt".into(),
        content: "x\n".into(),
    }))
    .await
    .unwrap();
    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1))
            .await
            .is_none(),
        "disabled watch is silent"
    );

    cancel.cancel();
}
