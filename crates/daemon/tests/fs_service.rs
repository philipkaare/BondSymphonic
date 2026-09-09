use bondsymphonic_daemon::fs;
use bondsymphonic_proto::ErrorCode;

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
