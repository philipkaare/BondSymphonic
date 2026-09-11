//! `fs.watch` end to end: enabling a watch turns worktree writes into a single
//! coalesced `fs.changed` with repo-relative paths, reading the worktree
//! reports nothing at all, a burst of writes is one event and not one per file,
//! ignored directories stay silent, and disabling the watch stops the events.

mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::process::Command;
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

/// Reading the worktree must not publish `fs.changed`. inotify reports reads as
/// `Access` events, so without a kind filter a client that answers `fs.changed`
/// by asking for `workspace.changes` makes the daemon run git, git reads every
/// file in the worktree, and those reads publish again: an idle workspace that
/// refreshes itself forever. On Windows this passes trivially, since
/// `ReadDirectoryChangesW` does not report reads at all; the WSL run is the one
/// that exercises the fix.
#[tokio::test]
async fn reading_the_worktree_is_not_a_change_but_writing_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "reads").await;
    let worktree = std::path::Path::new(&ws.worktree_path).to_path_buf();

    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: true,
    }))
    .await
    .unwrap();

    // A plain read, then the two ways git reads the whole worktree: directly,
    // and through the `workspace.changes` request that closed the loop.
    std::fs::read(worktree.join("README.md")).unwrap();
    let st = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&worktree)
        .status()
        .unwrap();
    assert!(st.success(), "git status runs inside the worktree");
    c.call(Request::WorkspaceChanges(WorkspaceIdParams {
        workspace_id: ws.id.clone(),
    }))
    .await
    .unwrap();

    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1))
            .await
            .is_none(),
        "reading the worktree is not a change"
    );

    // The watch is still armed, so this is a real filter and not a dead watcher.
    std::fs::write(worktree.join("touched.txt"), "x\n").unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("a write is still reported");
    assert_eq!(paths, vec!["touched.txt".to_string()]);

    cancel.cancel();
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
    tokio::time::sleep(Duration::from_millis(20)).await;
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

/// Serialises the two tests that either measure this process's inotify watches
/// or install thousands of them.
///
/// The count below is per *process*, not per watcher, and the harness runs the
/// whole file on threads of its own. `enabling_a_big_tree_does_not_block_other_workspaces_watches`
/// installs about 5,000 watches; overlapping the measurement window it would
/// inflate the reading into a false "node_modules/ is being watched", or, if
/// its watcher were dropped inside the window, push the subtraction below zero
/// and panic. Neither test is slow, so taking turns costs nothing.
///
/// A `tokio::sync::Mutex` rather than a `std` one: the guard is held across
/// `.await` in both tests, and this one has no poisoning, so a failure in one
/// test reports itself rather than reappearing as a poisoned-lock panic in the
/// other.
static WATCH_COUNT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How many inotify watches this process holds, summed over every inotify
/// descriptor it has open. The kernel lists them in `/proc/self/fdinfo/<fd>`,
/// one `inotify wd:` line per watch, so this is the watcher's real footprint
/// against `max_user_watches` rather than anything the daemon claims.
///
/// Only ever read while [`WATCH_COUNT`] is held.
#[cfg(target_os = "linux")]
fn inotify_watch_count() -> usize {
    std::fs::read_dir("/proc/self/fdinfo")
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .map(|info| {
            info.lines()
                .filter(|l| l.starts_with("inotify wd:"))
                .count()
        })
        .sum()
}

/// An ignored directory is not merely silenced: it is never watched at all.
///
/// Filtering `node_modules/**` paths in the callback keeps the events quiet,
/// but a recursive watch still installs one inotify watch per directory under
/// it, and a JS worktree's `node_modules` alone runs to tens of thousands. That
/// counts against the user's `max_user_watches`, and once it is exhausted the
/// next `fs.watch` for any workspace fails outright. Linux only: the count is
/// read from the kernel, and inotify is the backend the limit applies to.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn ignored_directories_are_not_watched_and_new_directories_are() {
    let _serial = WATCH_COUNT.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "nm").await;
    let worktree = std::path::Path::new(&ws.worktree_path).to_path_buf();

    const SUBDIRS: usize = 200;
    for i in 0..SUBDIRS {
        std::fs::create_dir_all(worktree.join("node_modules").join(format!("pkg{i}"))).unwrap();
    }
    std::fs::create_dir_all(worktree.join("src")).unwrap();

    let before = inotify_watch_count();
    c.call(Request::FsWatch(FsWatchParams {
        workspace_id: ws.id.clone(),
        enable: true,
    }))
    .await
    .unwrap();
    let installed = inotify_watch_count() - before;
    assert!(
        installed < SUBDIRS,
        "enabling the watch installed {installed} inotify watches: node_modules/ is being watched"
    );

    // The filter still holds, and it is a live watch: src/ is heard, node_modules/ is not.
    std::fs::write(worktree.join("node_modules/pkg7/index.js"), "x").unwrap();
    assert!(
        wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1))
            .await
            .is_none(),
        "node_modules/ is silent"
    );
    std::fs::write(worktree.join("src/main.rs"), "fn main() {}").unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("src/ is watched");
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    // A directory created after the watch was enabled is picked up: first the
    // directory itself is reported, then a file written inside it.
    std::fs::create_dir(worktree.join("src/new")).unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("the new directory is reported");
    assert!(paths.contains(&"src/new".to_string()), "{paths:?}");
    std::fs::write(worktree.join("src/new/a.rs"), "x").unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("a file inside the new directory is reported");
    assert!(paths.contains(&"src/new/a.rs".to_string()), "{paths:?}");

    // And a new ignored directory stays unwatched, however many children it grows.
    let before = inotify_watch_count();
    for i in 0..SUBDIRS {
        std::fs::create_dir_all(
            worktree
                .join("packages/web/node_modules")
                .join(format!("d{i}")),
        )
        .unwrap();
    }
    wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5))
        .await
        .expect("packages/ and packages/web/ are reported");
    let grown = inotify_watch_count().saturating_sub(before);
    assert!(
        grown < SUBDIRS,
        "a nested node_modules/ created later grew the watch set by {grown}"
    );

    cancel.cancel();
}

/// Enabling a watch on a big tree walks that tree, and the walk must not be
/// done under the lock every other `fs.watch` needs: a `disable` for another
/// workspace, issued while a 5,000-directory tree is being walked, returns
/// at once rather than after the walk.
///
/// Both watchers are driven directly rather than over the protocol, so the
/// timing measured is the lock's and not the connection's.
#[tokio::test]
async fn enabling_a_big_tree_does_not_block_other_workspaces_watches() {
    // Held for the whole test: the 5,000 watches this installs must not be in
    // place while the test above is counting this process's watches.
    let _serial = WATCH_COUNT.lock().await;
    use bondsymphonic_daemon::fs_watch::Watchers;
    use bondsymphonic_daemon::server::broadcast::{EventBus, EVENT_BUS_CAPACITY};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big");
    for i in 0..50 {
        for j in 0..100 {
            std::fs::create_dir_all(big.join(format!("d{i}/e{j}"))).unwrap();
        }
    }
    let small = dir.path().join("small");
    std::fs::create_dir_all(&small).unwrap();

    let watchers = Arc::new(Watchers::default());
    let events = EventBus::new(EVENT_BUS_CAPACITY);
    let small_id: WorkspaceId = "ws_small".into();
    watchers
        .enable(small_id.clone(), small.clone(), events.clone())
        .unwrap();

    // The walk runs on a blocking thread; the disable is timed from the
    // runtime once the walk is known to be under way.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let big_id: WorkspaceId = "ws_big".into();
    let walk = {
        let (w, s, ev) = (watchers.clone(), started.clone(), events.clone());
        tokio::task::spawn_blocking(move || {
            s.store(true, std::sync::atomic::Ordering::SeqCst);
            let t = Instant::now();
            w.enable(big_id, big, ev).unwrap();
            t.elapsed()
        })
    };
    while !started.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let t = Instant::now();
    tokio::task::spawn_blocking({
        let w = watchers.clone();
        move || w.disable(&small_id)
    })
    .await
    .unwrap();
    let disable_took = t.elapsed();
    let enable_took = walk.await.unwrap();
    eprintln!(
        "enable of 5,000 dirs took {enable_took:?}; concurrent disable took {disable_took:?}"
    );
    assert!(
        disable_took < Duration::from_millis(100),
        "disable waited {disable_took:?} behind a {enable_took:?} walk: the lock is held across it"
    );
}
