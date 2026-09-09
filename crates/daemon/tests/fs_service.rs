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

#[test]
#[cfg(unix)]
fn list_dir_follows_symlinks_to_directories_and_handles_dangling_links() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt");
    std::fs::create_dir_all(root.join("real_dir")).unwrap();
    std::fs::write(root.join("z.txt"), "z").unwrap();
    std::os::unix::fs::symlink(root.join("real_dir"), root.join("link_dir")).unwrap();
    std::os::unix::fs::symlink(root.join("does_not_exist"), root.join("dangling")).unwrap();

    let l = fs::list_dir(&root, "").unwrap();

    let link = l.entries.iter().find(|e| e.name == "link_dir").unwrap();
    assert!(
        link.is_dir,
        "a symlink to a directory must be listed as a directory"
    );
    assert_eq!(link.size, 0);

    let dangling = l.entries.iter().find(|e| e.name == "dangling").unwrap();
    assert!(
        !dangling.is_dir,
        "a dangling symlink must still be listed, as a file"
    );
    assert_eq!(dangling.size, 0);

    // Directories sort before regular files: link_dir (a directory) must
    // come before z.txt (a plain file).
    let idx = |n: &str| l.entries.iter().position(|e| e.name == n).unwrap();
    assert!(idx("link_dir") < idx("z.txt"));
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
