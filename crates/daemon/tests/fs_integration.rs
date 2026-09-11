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

/// The file service against an adversary inside the worktree.
///
/// The worktree is read-write for the sandboxed agent, so anything the agent
/// can do to it can happen between the moment the daemon decides a path is
/// inside the worktree and the moment it opens that path. Swapping a directory
/// for a symlink in that window is the classic way to redirect a write, and a
/// path check followed by a path open, however careful the check, is exactly
/// that window. Each test here runs the swap in a tight loop on one thread
/// while the service is called on another, and asserts what must hold whatever
/// the interleaving: nothing lands outside, nothing outside is read, and every
/// call is either a success or a refusal.
#[cfg(unix)]
mod symlink_races {
    use bondsymphonic_daemon::fs;
    use bondsymphonic_proto::ErrorCode;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const ROUNDS: usize = 2000;

    /// Alternates `wt/notes` between a real directory and a symlink to
    /// `outside`, as fast as the filesystem allows, until told to stop.
    fn swapper(wt: &Path, outside: &Path, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        let notes = wt.join("notes");
        let outside = outside.to_path_buf();
        std::thread::spawn(move || {
            let mut link = false;
            while !stop.load(Ordering::Relaxed) {
                // Whatever is there now goes, then the other shape takes its place.
                match std::fs::symlink_metadata(&notes) {
                    Ok(md) if md.file_type().is_symlink() => {
                        let _ = std::fs::remove_file(&notes);
                    }
                    Ok(_) => {
                        let _ = std::fs::remove_dir_all(&notes);
                    }
                    Err(_) => {}
                }
                if link {
                    let _ = std::os::unix::fs::symlink(&outside, &notes);
                } else {
                    let _ = std::fs::create_dir(&notes);
                }
                link = !link;
            }
        })
    }

    /// What a call may come back with while the tree is being torn up under
    /// it: success, the containment error, or a refusal for a directory that
    /// vanished or changed shape mid-call (the swapper deletes `notes` between
    /// shapes, and nothing can be written into a directory that is not there).
    /// What it may never come back with is a success that landed outside, and
    /// that is asserted on the filesystem, not on the error.
    fn is_ok_or_refused<T: std::fmt::Debug>(r: &Result<T, bondsymphonic_proto::RpcError>) -> bool {
        match r {
            Ok(_) => true,
            Err(e) => matches!(
                e.code,
                ErrorCode::InvalidParams | ErrorCode::IoError | ErrorCode::NotFound
            ),
        }
    }

    /// The refusal that says the call met the swapper's symlink, rather than
    /// finding the directory simply missing that round. The service refuses a
    /// link without following it, so it never learns that this particular one
    /// pointed outside, and says so in the link's own words rather than in the
    /// containment message.
    fn is_symlink_refusal(e: &bondsymphonic_proto::RpcError) -> bool {
        e.code == ErrorCode::InvalidParams && e.message.contains("symlinks are not followed")
    }

    #[test]
    fn a_write_never_lands_outside_the_worktree_while_a_directory_is_swapped_for_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(wt.join("notes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swap = swapper(&wt, &outside, stop.clone());
        let mut outcomes = Vec::new();
        let mut links_refused = 0;
        for i in 0..ROUNDS {
            let r = fs::write_file(&wt, "notes/config", &format!("round {i}"));
            assert!(
                is_ok_or_refused(&r),
                "round {i}: a write may succeed or be refused, not {r:?}"
            );
            if r.as_ref().is_err_and(is_symlink_refusal) {
                links_refused += 1;
            }
            outcomes.push(r.is_ok());
            let stray: Vec<_> = std::fs::read_dir(&outside)
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert!(
                stray.is_empty(),
                "round {i}: the save was redirected outside the worktree: {stray:?}"
            );
        }
        stop.store(true, Ordering::Relaxed);
        swap.join().unwrap();
        // The swapper was really racing, or the test proved nothing: some
        // writes went through, and some met the symlink and were refused for it.
        assert!(
            outcomes.iter().any(|ok| *ok) && links_refused > 0,
            "the race never bit: {} of {ROUNDS} writes succeeded, {links_refused} refused as links",
            outcomes.iter().filter(|ok| **ok).count()
        );
    }

    #[test]
    fn a_read_never_returns_a_file_from_outside_the_worktree_while_a_directory_is_swapped() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(wt.join("notes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "the secret").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swap = swapper(&wt, &outside, stop.clone());
        let mut links_refused = 0;
        for i in 0..ROUNDS {
            match fs::read_file(&wt, "notes/secret") {
                Ok(r) => panic!("round {i}: read a file from outside the worktree: {r:?}"),
                Err(e) => {
                    assert!(
                        is_ok_or_refused::<()>(&Err(e.clone())),
                        "round {i}: unexpected error {e:?}"
                    );
                    if is_symlink_refusal(&e) {
                        links_refused += 1;
                    }
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        swap.join().unwrap();
        assert!(
            links_refused > 0,
            "the race never bit: no read met the symlink"
        );
    }
}
