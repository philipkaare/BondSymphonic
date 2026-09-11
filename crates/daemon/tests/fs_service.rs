use bondsymphonic_daemon::fs;
use bondsymphonic_proto::ErrorCode;
use std::sync::Arc;

#[test]
fn resolve_contains_paths_and_rejects_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(root.join("src")).unwrap();
    assert!(
        fs::resolve(&root, "src/main.rs")
            .unwrap()
            .ends_with("src/main.rs")
            || fs::resolve(&root, "src/main.rs")
                .unwrap()
                .ends_with("src\\main.rs")
    );
    for bad in ["../x", "src/../../x", "/etc/passwd", "a\0b"] {
        assert_eq!(
            fs::resolve(&root, bad).unwrap_err().code,
            ErrorCode::InvalidParams,
            "{bad}"
        );
    }
    // A Windows drive-letter path is only absolute on Windows; on unix it is
    // just a relative filename that stays inside the root.
    #[cfg(windows)]
    assert_eq!(
        fs::resolve(&root, "C:\\Windows").unwrap_err().code,
        ErrorCode::InvalidParams
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path(), root.join("escape")).unwrap();
        assert_eq!(
            fs::resolve(&root, "escape/secret").unwrap_err().code,
            ErrorCode::InvalidParams
        );
    }
}

#[test]
fn list_read_write_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("b.txt"), "bee").unwrap();
    std::fs::write(root.join("a.bin"), [0u8, 159, 146, 150]).unwrap();
    let l = fs::list_dir(&root, "").unwrap();
    let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["src", "a.bin", "b.txt"]);
    assert!(l.entries[0].is_dir && !l.entries[2].is_dir && l.entries[2].size == 3);
    assert_eq!(fs::read_file(&root, "b.txt").unwrap().content, "bee");
    assert_eq!(fs::read_file(&root, "a.bin").unwrap().encoding, "binary");
    fs::write_file(&root, "src/new/deep.txt", "hi").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("src/new/deep.txt")).unwrap(),
        "hi"
    );
    assert_eq!(
        fs::read_file(&root, "missing").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        fs::list_dir(&root, "b.txt").unwrap_err().code,
        ErrorCode::InvalidParams
    );
}

/// A symlink is listed as what the service can actually do with it: nothing.
///
/// The unix service walks a path one component at a time and refuses to follow
/// a symlink at any of them, so a link to a directory is a folder that answers
/// every click with a refusal. Reporting it as a directory was the Explorer
/// advertising something it cannot open. It is listed, because it is really
/// there and hiding it would be its own lie, but as a plain entry of no size.
#[test]
#[cfg(unix)]
fn list_dir_reports_a_symlink_as_a_file_because_nothing_can_descend_into_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(root.join("real_dir")).unwrap();
    std::fs::write(root.join("z.txt"), "z").unwrap();
    std::os::unix::fs::symlink(root.join("real_dir"), root.join("link_dir")).unwrap();
    std::os::unix::fs::symlink(root.join("does_not_exist"), root.join("dangling")).unwrap();

    let l = fs::list_dir(&root, "").unwrap();

    let link = l.entries.iter().find(|e| e.name == "link_dir").unwrap();
    assert!(
        !link.is_dir,
        "a symlink must not be listed as a directory: nothing can descend into it"
    );
    assert_eq!(link.size, 0, "a symlink has no content to offer");

    let dangling = l.entries.iter().find(|e| e.name == "dangling").unwrap();
    assert!(
        !dangling.is_dir,
        "a dangling symlink must still be listed, as a file"
    );
    assert_eq!(dangling.size, 0);

    // The real directory still sorts before the plain files, and the link now
    // sorts among them rather than with the directories.
    let idx = |n: &str| l.entries.iter().position(|e| e.name == n).unwrap();
    assert!(idx("real_dir") < idx("z.txt"));
    assert!(idx("link_dir") > idx("real_dir"));
}

/// Refusing to follow a symlink and refusing to leave the worktree are two
/// different answers, and the service has to say which one it gave.
///
/// Every symlink is refused, including one that points squarely inside the
/// worktree, because the walk cannot tell the two apart without following the
/// link -- which is the thing it must not do. Answering an in-tree
/// `docs -> shared/docs` with "path escapes the worktree" told the user their
/// own repository was trying to break out. The containment message is reserved
/// for containment; a link gets a message about links.
#[test]
#[cfg(unix)]
fn a_symlink_is_refused_with_its_own_message_not_the_containment_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(root.join("shared/docs")).unwrap();
    std::fs::write(root.join("shared/docs/x.txt"), "x").unwrap();
    std::fs::write(root.join("real.txt"), "real").unwrap();
    // Both links point inside the worktree: nothing here is an escape attempt.
    std::os::unix::fs::symlink(root.join("shared/docs"), root.join("docs")).unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

    let refused = |what: &str, e: bondsymphonic_proto::RpcError| {
        assert_eq!(e.code, ErrorCode::InvalidParams, "{what}: {e:?}");
        assert!(
            e.message.contains("symlinks are not followed"),
            "{what}: expected the symlink message, got {:?}",
            e.message
        );
        assert!(
            !e.message.contains("escapes the worktree"),
            "{what}: an in-tree link must not be reported as an escape: {:?}",
            e.message
        );
    };

    // The link as an intermediate component of the path.
    refused(
        "read through a linked directory",
        fs::read_file(&root, "docs/x.txt").unwrap_err(),
    );
    refused(
        "write through a linked directory",
        fs::write_file(&root, "docs/y.txt", "y").unwrap_err(),
    );
    // The same links as the final component, which each call opens its own way.
    refused(
        "list a linked directory",
        fs::list_dir(&root, "docs").unwrap_err(),
    );
    refused(
        "read a linked file",
        fs::read_file(&root, "link.txt").unwrap_err(),
    );
    refused(
        "write a linked file",
        fs::write_file(&root, "link.txt", "y").unwrap_err(),
    );

    // The refusal is total: neither write reached the link's target.
    assert_eq!(
        std::fs::read_to_string(root.join("real.txt")).unwrap(),
        "real"
    );
    assert!(!root.join("shared/docs/y.txt").exists());
}

#[test]
fn concurrent_writes_to_the_same_path_never_interleave_and_leave_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = Arc::new(dir.path().join("wt"));
    std::fs::create_dir_all(&*root).unwrap();

    let contents: Vec<String> = (0..8)
        .map(|i| format!("payload-from-writer-{i}-").repeat(2000))
        .collect();
    let handles: Vec<_> = contents
        .iter()
        .cloned()
        .map(|content| {
            let root = Arc::clone(&root);
            std::thread::spawn(move || fs::write_file(&root, "shared.txt", &content).unwrap())
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let final_content = std::fs::read_to_string(root.join("shared.txt")).unwrap();
    assert!(
        contents.contains(&final_content),
        "final content (len {}) was not one of the writes, so writes interleaved",
        final_content.len()
    );

    let leftover: Vec<_> = std::fs::read_dir(&*root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".bs-tmp"))
        .collect();
    assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
}

/// A file longer than `MAX_READ` comes back truncated at the cap, and the
/// caller is told so.
#[test]
fn read_file_truncates_above_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("big.txt"), vec![b'x'; fs::MAX_READ + 10]).unwrap();
    let r = fs::read_file(&root, "big.txt").unwrap();
    assert!(r.truncated, "a file above the cap must report truncated");
    assert_eq!(r.encoding, "utf-8");
    assert_eq!(r.content.len(), fs::MAX_READ);
    // Exactly at the cap is not truncated.
    std::fs::write(root.join("edge.txt"), vec![b'x'; fs::MAX_READ]).unwrap();
    let r = fs::read_file(&root, "edge.txt").unwrap();
    assert!(!r.truncated);
    assert_eq!(r.content.len(), fs::MAX_READ);
}

/// `read_file` must never allocate the whole file. Ignored by default because
/// it needs an 8 GiB sparse file; run it with
/// `cargo test -p bondsymphonic-daemon --test fs_service -- --ignored`.
///
/// The time bound is the assertion that matters: reading the cap takes
/// milliseconds, while pulling 8 GiB through memory cannot.
#[test]
#[ignore = "creates an 8 GiB sparse file"]
fn read_file_does_not_load_a_huge_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(&root).unwrap();
    let f = std::fs::File::create(root.join("huge.bin")).unwrap();
    f.set_len(8 * 1024 * 1024 * 1024).unwrap();
    drop(f);
    let started = std::time::Instant::now();
    let r = fs::read_file(&root, "huge.bin").unwrap();
    let elapsed = started.elapsed();
    assert!(r.truncated);
    assert!(r.content.len() <= fs::MAX_READ);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "read_file took {elapsed:?}: it is still reading the whole file"
    );
}

#[cfg(windows)]
#[test]
fn write_file_retries_when_destination_is_briefly_locked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(&root).unwrap();
    fs::write_file(&root, "locked.txt", "v1").unwrap();
    // Hold the destination open with share-deny-none is not enough to block rename on Windows;
    // a mapping does: open + hold a std File and MapViewOfFile-like effect is not available in std,
    // so emulate contention with concurrent renames: many writers, plus a reader loop that keeps
    // reopening the file. Success criterion: no writer errors.
    let path = root.join("locked.txt");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let p = path.clone();
        let s = stop.clone();
        std::thread::spawn(move || {
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::read(&p);
            }
        })
    };
    let writers: Vec<_> = (0..8)
        .map(|i| {
            let r = root.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    fs::write_file(&r, "locked.txt", &format!("writer {i}"))
                        .expect("write_file must not fail under contention");
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .starts_with("writer "));
    assert!(std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().ends_with(".bs-tmp")));
}

/// An oversized save is an error the IDE can show, not a connection that dies.
///
/// The request carrying it is one JSON line, and a line past the connection's
/// 8 MiB frame cap is not read at all: the peer is disconnected with no reply,
/// which from the editor looks like the daemon crashing on save. Since the
/// service never hands out more than `MAX_READ` in the first place, anything
/// larger is refused here with an answer that says so.
#[test]
fn write_file_refuses_content_larger_than_it_would_ever_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(&root).unwrap();

    let err = fs::write_file(&root, "big.txt", &"x".repeat(fs::MAX_READ + 1)).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(
        err.message.contains(&fs::MAX_READ.to_string()),
        "the refusal names the limit: {}",
        err.message
    );
    assert!(
        !root.join("big.txt").exists(),
        "nothing was created for a save that was refused"
    );

    // Exactly at the cap is a file the service could have read out, so it goes
    // through: the refusal is for what is past the limit, not at it.
    fs::write_file(&root, "edge.txt", &"y".repeat(fs::MAX_READ)).unwrap();
    assert_eq!(
        std::fs::metadata(root.join("edge.txt")).unwrap().len(),
        fs::MAX_READ as u64
    );
}
