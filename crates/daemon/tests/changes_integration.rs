mod common;

use bondsymphonic_daemon::fs::MAX_READ;
use bondsymphonic_daemon::workspace::lifecycle;
use bondsymphonic_proto::*;
use common::{commit_all, create_ws, init_repo, start_daemon, Client};

/// Not valid UTF-8 and NUL-bearing, so both git and `fs::read_file` call it binary.
const BINARY: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe, 0x00, 0x01,
];
const BINARY_EDITED: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe, 0x00, 0x02, 0x03,
];

#[tokio::test]
async fn changes_and_diff_report_committed_uncommitted_and_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    // Files that exist at the merge-base, so a rename and a delete in the
    // workspace are visible to git as a rename and a delete rather than as an
    // add of something it has never seen.
    std::fs::write(repo.join("old.rs"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(repo.join("doomed.txt"), "x\ny\n").unwrap();
    std::fs::write(repo.join("img.png"), BINARY).unwrap();
    std::fs::write(repo.join("crlf.txt"), "a\nb\n").unwrap();
    std::fs::create_dir(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/deep.txt"), "deep\n").unwrap();
    // One byte over the daemon's 4 MiB read cap, committed so both sides of its
    // diff — the blob `git show` streams and the working file — are cut. Built
    // once and left untouched in the workspace: what is being tested is the cap,
    // not a change, and a second 4 MiB copy would only cost time.
    std::fs::write(repo.join("big.txt"), big_text()).unwrap();
    commit_all(&repo, &[], "base files");

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let wt = std::path::Path::new(&ws.worktree_path).to_path_buf();
    let w = daemon.workspace(&ws.id).unwrap();
    let env = lifecycle::layout_for(&daemon, &w)
        .await
        .unwrap()
        .sandbox_git_env();
    let git = |args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(&wt);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        assert!(cmd.status().unwrap().success(), "git {args:?}");
    };

    // A committed edit and a committed rename, then uncommitted work on top: an
    // edit, a new file, a deletion, a binary edit, and a CRLF rewrite.
    std::fs::write(wt.join("README.md"), "hello\nworld\n").unwrap();
    git(&["mv", "old.rs", "new.rs"]);
    commit_all(&wt, &env, "work");
    std::fs::write(wt.join("README.md"), "hello\nworld\nagain\n").unwrap();
    std::fs::write(wt.join("notes.txt"), "a\nb\nc\n").unwrap();
    std::fs::remove_file(wt.join("doomed.txt")).unwrap();
    std::fs::write(wt.join("img.png"), BINARY_EDITED).unwrap();
    std::fs::write(wt.join("crlf.txt"), "a\r\nb\r\nc\r\n").unwrap();

    let v = c
        .call(Request::WorkspaceChanges(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let res: ChangesResult = serde_json::from_value(v).unwrap();
    let file = |p: &str| {
        res.files
            .iter()
            .find(|f| f.path == p)
            .unwrap_or_else(|| panic!("{p} listed, got {:?}", res.files))
    };

    let readme = file("README.md");
    assert_eq!(readme.status, FileStatus::Modified);
    assert_eq!((readme.additions, readme.deletions), (2, 0));

    let notes = file("notes.txt");
    assert_eq!(notes.status, FileStatus::Untracked);
    assert_eq!((notes.additions, notes.deletions), (3, 0));

    // A real `git mv`, reported against the new path only, with no phantom
    // untracked entry for the old one.
    let renamed = file("new.rs");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!((renamed.additions, renamed.deletions), (0, 0));
    assert!(
        !res.files.iter().any(|f| f.path == "old.rs"),
        "the old path is gone, got {:?}",
        res.files
    );

    // A real delete of a tracked file, counted from the merge-base version.
    let doomed = file("doomed.txt");
    assert_eq!(doomed.status, FileStatus::Deleted);
    assert_eq!((doomed.additions, doomed.deletions), (0, 2));

    // Binary: git reports `-\t-`, which is neither an addition nor a deletion.
    let img = file("img.png");
    assert_eq!(img.status, FileStatus::Modified);
    assert_eq!((img.additions, img.deletions), (0, 0));

    let d = diff(&mut c, &ws.id, "README.md").await;
    assert_eq!(d.base_text, "hello\n");
    assert_eq!(d.work_text, "hello\nworld\nagain\n");
    assert!(!d.truncated, "a two-line file is not truncated");

    // A file that does not exist at the merge-base has an empty base side.
    let d = diff(&mut c, &ws.id, "notes.txt").await;
    assert_eq!(d.base_text, "");
    assert_eq!(d.work_text, "a\nb\nc\n");

    // A deleted file keeps its base side and loses its work side.
    let d = diff(&mut c, &ws.id, "doomed.txt").await;
    assert_eq!(d.base_text, "x\ny\n");
    assert_eq!(d.work_text, "");

    // Binary on both sides: the base blob goes through the same classification
    // as the working file, so neither side is mojibake.
    let d = diff(&mut c, &ws.id, "img.png").await;
    assert_eq!(d.base_text, "");
    assert_eq!(d.work_text, "");

    // Line endings are normalised on both sides, so a CRLF checkout does not
    // render as a whole-file change.
    let d = diff(&mut c, &ws.id, "crlf.txt").await;
    assert_eq!(d.base_text, "a\nb\n");
    assert_eq!(d.work_text, "a\nb\nc\n");

    // Over the read cap: both sides stop at 4 MiB and the reply says so, which
    // is a different claim from the IDE's alignment budget running out.
    let d = diff(&mut c, &ws.id, "big.txt").await;
    assert!(
        d.truncated,
        "a file over the 4 MiB cap is reported truncated"
    );
    // The base side is the blob as git stores it, so it is cut at exactly the cap.
    assert_eq!(d.base_text.len(), MAX_READ);
    // The work side is the checkout, which on a machine with `core.autocrlf` on
    // holds CRLF: still cut at the cap, then shortened again by the normalise.
    assert!(
        !d.work_text.is_empty() && d.work_text.len() <= MAX_READ,
        "the work side is cut at the cap, got {} bytes",
        d.work_text.len()
    );

    // A path in a subdirectory, in the separator git speaks.
    let d = diff(&mut c, &ws.id, "sub/deep.txt").await;
    assert_eq!(d.base_text, "deep\n");
    assert_eq!(d.work_text, "deep\n");

    // ...and, on Windows only, in the separator the platform speaks. On unix a
    // backslash is an ordinary filename character and must not be rewritten.
    if cfg!(windows) {
        let d = diff(&mut c, &ws.id, "sub\\deep.txt").await;
        assert_eq!(d.base_text, "deep\n");
        assert_eq!(d.work_text, "deep\n");
    }

    let err = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.id.clone(),
            path: "../outside".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);

    // A directory is not a side of a diff.
    let err = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.id.clone(),
            path: "sub".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);

    cancel.cancel();
}

/// `Git::run_bytes` exists for the base side of `workspace.diff`, so its
/// behaviour is pinned here alongside the caller that needs it.
///
/// The blob is deliberately larger than a pipe buffer, so git is still writing
/// when the cap is reached. That is the case a reader can hang on, and the first
/// version of this one did.
#[tokio::test]
async fn run_bytes_caps_the_stream_without_calling_the_cut_off_a_failure() {
    use bondsymphonic_daemon::git::Git;

    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let big = "x".repeat(256 * 1024);
    std::fs::write(repo.join("big.txt"), &big).unwrap();
    commit_all(&repo, &[], "big");

    let git = Git::new();
    let head = git.run(&repo, &["rev-parse", "HEAD"]).await.unwrap();
    let spec = format!("{}:big.txt", head.stdout.trim());

    // Capped: cap + 1 bytes, the extra byte being what says "there is more".
    let out = git.run_bytes(&repo, &["show", &spec], 4).await.unwrap();
    assert_eq!(out.stdout, b"xxxxx");

    // Under the cap: the whole blob, and the exit status is still checked.
    let out = git
        .run_bytes(&repo, &["show", &spec], big.len() * 2)
        .await
        .unwrap();
    assert_eq!(out.stdout.len(), big.len());

    // A path that is not in the tree is still an error, which is what lets
    // `diff` tell "absent at the merge-base" from "here is the content".
    let err = git
        .run_bytes(
            &repo,
            &["show", &format!("{}:nope.txt", head.stdout.trim())],
            64,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError);
}

/// The base side of `workspace.diff` decides "absent at the merge-base" from
/// what git says, because the exit code cannot tell absence from a broken
/// object store — both are `fatal:`, exit 128. So what git says for each case
/// is part of the contract and is pinned here.
#[tokio::test]
async fn cat_file_tells_an_absent_path_from_an_invalid_revision() {
    use bondsymphonic_daemon::git::Git;

    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    commit_all(&repo, &[], "one file");

    let git = Git::new();
    let head = git
        .run(&repo, &["rev-parse", "HEAD"])
        .await
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    let stderr_of = |e: &RpcError| {
        e.data
            .as_ref()
            .and_then(|d| d.get("stderr"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_owned()
    };

    // Present at the revision: exit 0, so the read that follows is a real read.
    git.run(&repo, &["cat-file", "-e", &format!("{head}:a.txt")])
        .await
        .unwrap();

    // Absent: the message the daemon reads as "this file is new".
    let err = git
        .run(&repo, &["cat-file", "-e", &format!("{head}:gone.txt")])
        .await
        .unwrap_err();
    assert!(
        stderr_of(&err).contains("does not exist in"),
        "git changed its absence message: {err:?}"
    );

    // An invalid revision says something else entirely, so it surfaces as an
    // error instead of being reported as a file with no base side.
    let err = git
        .run(&repo, &["cat-file", "-e", "not-a-rev:a.txt"])
        .await
        .unwrap_err();
    let stderr = stderr_of(&err);
    assert_eq!(err.code, ErrorCode::GitError);
    assert!(
        !stderr.contains("does not exist in") && !stderr.contains("exists on disk, but not in"),
        "an invalid revision must not look like absence: {stderr:?}"
    );
}

/// 4 MiB + 1 byte of `a\n` lines: one byte over the daemon's read cap, which is
/// exactly what makes `classify` cut the content and set `truncated`.
fn big_text() -> String {
    let mut s = "a\n".repeat(MAX_READ / 2);
    s.push('a');
    s
}

/// One `workspace.diff` round trip.
async fn diff(c: &mut Client, ws: &WorkspaceId, path: &str) -> DiffResult {
    let v = c
        .call(Request::WorkspaceDiff(WorkspaceDiffParams {
            workspace_id: ws.clone(),
            path: path.into(),
        }))
        .await
        .unwrap();
    serde_json::from_value(v).unwrap()
}

/// Paths above ASCII survive `workspace.changes` and `workspace.diff` intact.
///
/// Every git command that prints a path obeys `core.quotePath`, which is on by
/// default and renders each non-ASCII byte as an octal escape inside double
/// quotes. The IDE opens what this list gives it, so a quoted name is a file it
/// cannot find; the daemon turns the option off for every path-listing call.
/// Both a committed change and an untracked file are checked, because they come
/// from two different commands (`git diff` and `git status`).
#[tokio::test]
async fn non_ascii_paths_are_reported_the_way_they_are_spelled_on_disk() {
    const TRACKED: &str = "håndbog.md";
    const UNTRACKED: &str = "æøå-notat.txt";
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    std::fs::write(repo.join(TRACKED), "fælles\n").unwrap();
    commit_all(&repo, &[], "base");

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "utf8").await;
    let wt = std::path::Path::new(&ws.worktree_path).to_path_buf();
    let env = lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap())
        .await
        .unwrap()
        .sandbox_git_env();

    std::fs::write(wt.join(TRACKED), "fælles\nnyt\n").unwrap();
    commit_all(&wt, &env, "edit");
    std::fs::write(wt.join(UNTRACKED), "hej\n").unwrap();

    let res: ChangesResult = serde_json::from_value(
        c.call(Request::WorkspaceChanges(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let paths: Vec<&str> = res.files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.contains(&TRACKED),
        "the committed change must keep its name: {paths:?}"
    );
    assert!(
        paths.contains(&UNTRACKED),
        "the untracked file must keep its name: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with('"')),
        "no path may come back quoted and escaped: {paths:?}"
    );

    // And the name the list gives is a name `workspace.diff` accepts.
    let d = diff(&mut c, &ws.id, TRACKED).await;
    assert_eq!(d.base_text, "fælles\n");
    assert_eq!(d.work_text, "fælles\nnyt\n");

    cancel.cancel();
}
