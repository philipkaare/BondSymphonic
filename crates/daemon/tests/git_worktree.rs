mod common;

use bondsymphonic_daemon::git::{
    repo,
    worktree::{self, Layout},
    Git,
};
use bondsymphonic_proto::ErrorCode;

async fn layout_for(dir: &std::path::Path, repo_path: &std::path::Path, name: &str) -> Layout {
    let git = Git::new();
    let git_common = repo::common_dir(&git, repo_path).await.unwrap();
    Layout {
        repo: repo_path.to_path_buf(),
        git_common,
        name: name.into(),
        branch: format!("bs/{name}/work"),
        worktree_path: dir.join("worktrees").join("ws_00000001"),
        objects_dir: dir.join("objects").join("ws_00000001"),
        no_hooks_dir: dir.join("nohooks"),
    }
}

#[tokio::test]
async fn create_makes_branch_worktree_and_writable_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-1").await;

    worktree::create(&layout, "main").await.unwrap();

    assert!(layout.worktree_path.join("README.md").exists());
    assert!(
        layout.ref_dir().join("work").is_file(),
        "loose ref file must exist for the rw bind"
    );
    assert!(layout.reflog_dir().is_dir());
    assert!(layout.objects_dir.is_dir());
    assert!(layout.worktree_gitdir().join("HEAD").exists());
    assert!(repo::branch_exists(&git, &repo_path, "bs/agent-1/work")
        .await
        .unwrap());
    assert_eq!(layout.rw_git_paths().len(), 3);

    // Creating again is a Conflict.
    let err = worktree::create(&layout, "main").await.unwrap_err();
    assert_eq!(err.error.code, ErrorCode::Conflict);
    assert_eq!(
        err.left,
        worktree::Leftovers::Nothing,
        "a name that is already taken is refused before anything is made"
    );
}

#[tokio::test]
async fn commits_in_worktree_go_to_private_objects_and_daemon_can_read_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-2").await;
    worktree::create(&layout, "main").await.unwrap();

    let shared_objects_dir = layout.git_common.join("objects");
    let shared_count_before = walkdir_count(&shared_objects_dir);

    // Simulate the sandboxed agent: git with the sandbox env.
    let mut agent_git = Git::new()
        .with_env("GIT_AUTHOR_NAME", "a")
        .with_env("GIT_AUTHOR_EMAIL", "a@a")
        .with_env("GIT_COMMITTER_NAME", "a")
        .with_env("GIT_COMMITTER_EMAIL", "a@a");
    for (k, v) in layout.sandbox_git_env() {
        agent_git = agent_git.with_env(k, v);
    }
    std::fs::write(layout.worktree_path.join("new.txt"), "agent work\n").unwrap();
    agent_git
        .run(&layout.worktree_path, &["add", "new.txt"])
        .await
        .unwrap();
    agent_git
        .run(
            &layout.worktree_path,
            &["commit", "-q", "-m", "agent commit"],
        )
        .await
        .unwrap();

    // New objects landed in the private dir, not the shared store.
    let private_count = walkdir_count(&layout.objects_dir);
    assert!(
        private_count >= 3,
        "blob+tree+commit expected in private objects, got {private_count}"
    );
    // The shared store must not have grown: this proves exclusivity, not just presence.
    assert_eq!(
        walkdir_count(&shared_objects_dir),
        shared_count_before,
        "shared object store must not receive objects written by the sandboxed agent"
    );

    // The new commit's object file must exist under the private loose-object path
    // and must not exist under the shared loose-object path.
    let head_sha = agent_git
        .run(&layout.worktree_path, &["rev-parse", "HEAD"])
        .await
        .unwrap()
        .stdout
        .trim()
        .to_string();
    let (dir_part, file_part) = head_sha.split_at(2);
    assert!(
        layout.objects_dir.join(dir_part).join(file_part).is_file(),
        "commit object must be present in the private objects dir"
    );
    assert!(
        !shared_objects_dir.join(dir_part).join(file_part).exists(),
        "commit object must be absent from the shared objects dir"
    );

    // The daemon (outside the sandbox) can read the commit via alternates.
    let daemon_git = layout.daemon_git();
    let subject = daemon_git
        .run(&repo_path, &["log", "-1", "--format=%s", "bs/agent-2/work"])
        .await
        .unwrap();
    assert_eq!(subject.stdout.trim(), "agent commit");

    worktree::remove(&layout).await.unwrap();
    assert!(!layout.worktree_path.exists());
    assert!(!repo::branch_exists(&git, &repo_path, "bs/agent-2/work")
        .await
        .unwrap());
    worktree::remove(&layout).await.unwrap(); // idempotent
}

fn walkdir_count(p: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                n += walkdir_count(&path);
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Creating and removing a workspace worktree runs none of the repository's own
/// hooks.
///
/// `git worktree add` fires `post-checkout` and, with the branch it creates,
/// `reference-transaction`; `git branch -D` on the way out fires
/// `reference-transaction` again. Those hooks are arbitrary code out of the
/// user's repository, and neither call is the user typing a git command: they
/// happen when the IDE opens or closes a workspace. Both go through
/// `Layout::daemon_git`, which pins `core.hooksPath` at the daemon's own empty
/// directory.
#[tokio::test]
async fn create_and_remove_run_no_repository_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let marker = dir.path().join("hooks-fired.txt");
    for hook in [
        "post-checkout",
        "reference-transaction",
        "post-index-change",
    ] {
        common::install_hook(
            &repo_path,
            hook,
            &format!("echo {hook} >> '{}'", common::sh_path(&marker)),
        );
    }
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "hooked").await;
    std::fs::create_dir_all(&layout.no_hooks_dir).unwrap();

    worktree::create(&layout, "main").await.unwrap();
    assert!(
        !marker.exists(),
        "worktree create ran repository hooks: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );

    worktree::remove(&layout).await.unwrap();
    assert!(
        !marker.exists(),
        "worktree remove ran repository hooks: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
    assert!(!repo::branch_exists(&git, &repo_path, "bs/hooked/work")
        .await
        .unwrap());

    // The hooks themselves work: without this the assertions above would pass
    // just as well on a host where git never runs hooks at all.
    let plain = dir.path().join("plain");
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "control", &plain.to_string_lossy()])
        .arg("main")
        .current_dir(&repo_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fired = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        fired.contains("post-checkout"),
        "the control worktree must fire the hook, or this test proves nothing: {fired:?}"
    );
}

/// A locked worktree is removed like any other.
///
/// `git worktree lock` is what a user reaches for when a checkout lives on a
/// removable disk, and git then refuses `worktree remove --force` outright —
/// with a wording that is none of the three the removal used to tolerate, so
/// `workspace.destroy` failed and the workspace could never be got rid of. The
/// removal now unlocks first and does not read git's prose at all: whatever the
/// registration says, the directory goes and `worktree prune` forgets it.
#[tokio::test]
async fn remove_takes_a_locked_worktree_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "locked").await;
    worktree::create(&layout, "main").await.unwrap();

    git.run(
        &repo_path,
        &[
            "worktree",
            "lock",
            &layout.worktree_path.to_string_lossy(),
            "--reason",
            "on a removable disk",
        ],
    )
    .await
    .unwrap();

    worktree::remove(&layout).await.unwrap();

    assert!(
        !layout.worktree_path.exists(),
        "the worktree directory is still there"
    );
    assert!(!repo::branch_exists(&git, &repo_path, "bs/locked/work")
        .await
        .unwrap());
    let listed = git
        .run(&repo_path, &["worktree", "list", "--porcelain"])
        .await
        .unwrap()
        .stdout;
    assert!(
        !listed.contains("ws_00000001"),
        "the registration outlived the worktree: {listed}"
    );
}

// ---------------------------------------------------------------------------
// A worktree directory that will not go away.
//
// `workspace.destroy` stops the sandbox and its agent processes a few lines
// before it removes the worktree, and on Windows a just-killed process can hold
// handles inside that directory for a moment afterwards. The daemon opens that
// window itself, so both shapes of it are pinned here: one that clears, and one
// that does not.
// ---------------------------------------------------------------------------

/// Holds an open handle on `path` until `release` is set, *without* the delete
/// share Rust's own `File::open` grants — which is what a handle from another
/// program looks like, and the only kind that stops a delete.
///
/// Announces through `opened` that the handle is real before the test goes on,
/// so nothing here depends on a thread being scheduled promptly.
#[cfg(windows)]
fn hold_a_handle(
    path: std::path::PathBuf,
    opened: std::sync::Arc<std::sync::atomic::AtomicBool>,
    release: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        // FILE_SHARE_READ only: no FILE_SHARE_DELETE, so the file cannot be
        // unlinked while this handle is open.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .expect("open the held file");
        opened.store(true, Ordering::SeqCst);
        while !release.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(f);
    })
}

/// A handle that is released while the removal is still trying does not fail
/// the destroy.
///
/// Success is impossible before the release — the handle is held until this
/// test says so — so a green here is the retry doing its job and nothing else.
#[cfg(windows)]
#[tokio::test]
async fn remove_waits_out_a_handle_that_is_about_to_be_released() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "briefly-held").await;
    worktree::create(&layout, "main").await.unwrap();

    let (opened, release) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let holder = hold_a_handle(
        layout.worktree_path.join("README.md"),
        opened.clone(),
        release.clone(),
    );
    while !opened.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let held = Arc::clone(&release);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        held.store(true, Ordering::SeqCst);
    });
    worktree::remove(&layout).await.unwrap();
    holder.join().unwrap();

    assert!(!layout.worktree_path.exists());
    assert!(
        !repo::branch_exists(&git, &repo_path, "bs/briefly-held/work")
            .await
            .unwrap()
    );
}

/// A handle that is *never* released fails the destroy — and the failure costs
/// the directory only.
///
/// The registration and the branch go regardless. A registration that outlives
/// its directory keeps the branch checked out and makes `worktree add` refuse
/// until a human intervenes, so leaving one behind on the way out of a failure
/// is worse than the failure: it turns a transient lock into a repository the
/// user has to repair by hand.
#[cfg(windows)]
#[tokio::test]
async fn a_directory_that_will_not_go_still_costs_no_registration_and_no_branch() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "stuck").await;
    worktree::create(&layout, "main").await.unwrap();

    let (opened, release) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let holder = hold_a_handle(
        layout.worktree_path.join("README.md"),
        opened.clone(),
        release.clone(),
    );
    while !opened.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let err = worktree::remove(&layout).await.unwrap_err();
    release.store(true, Ordering::SeqCst);
    holder.join().unwrap();

    assert!(
        err.message.contains("worktree directory"),
        "the error has to name what would not go: {}",
        err.message
    );
    let listed = git
        .run(&repo_path, &["worktree", "list", "--porcelain"])
        .await
        .unwrap()
        .stdout;
    assert!(
        !listed.contains("ws_00000001"),
        "the registration outlived a failed removal: {listed}"
    );
    assert!(
        !repo::branch_exists(&git, &repo_path, "bs/stuck/work")
            .await
            .unwrap(),
        "the branch outlived a failed removal"
    );
}
