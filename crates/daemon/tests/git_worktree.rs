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

/// A new workspace worktree is locked, so a `git worktree prune` that cannot see
/// the directory keeps its registration.
///
/// That is exactly what a Windows git does to every workspace: the worktree
/// lives under the WSL user's home, which Windows git cannot see, so to it every
/// registration looks stale. Hiding the directory stands in for that here.
#[tokio::test]
async fn create_locks_the_worktree_so_a_prune_that_cannot_see_it_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "locked-on-create").await;
    worktree::create(&layout, "main").await.unwrap();

    let reason = std::fs::read_to_string(layout.worktree_gitdir().join("locked"))
        .expect("create must lock the worktree");
    assert_eq!(reason.trim_end(), worktree::LOCK_REASON);

    let hidden = dir.path().join("hidden");
    std::fs::rename(dir.path().join("worktrees"), &hidden).unwrap();
    git.run(&repo_path, &["worktree", "prune"]).await.unwrap();
    std::fs::rename(&hidden, dir.path().join("worktrees")).unwrap();
    assert!(
        layout.worktree_gitdir().join("HEAD").is_file(),
        "prune removed a locked registration"
    );

    worktree::remove(&layout).await.unwrap();
    assert!(!layout.worktree_gitdir().exists());
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

    // `create` locks every worktree with a reason of its own; this one is
    // locked the way a user would, which git only allows once it is unlocked.
    let _ = git
        .run(
            &repo_path,
            &[
                "worktree",
                "unlock",
                &layout.worktree_path.to_string_lossy(),
            ],
        )
        .await;
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

/// A handle held open inside the worktree, released however the test ends.
///
/// The handle is opened *without* the delete share Rust's own `File::open`
/// grants — the only kind that stops a delete, and what a handle from another
/// program looks like. The guard exists because the release must not be a
/// statement in the test body: an unexpected success or a failed assertion
/// would skip it, and the thread would hold the directory open into whatever
/// ran next. `Drop` runs on the panic path too.
#[cfg(windows)]
struct HeldHandle {
    release: std::sync::Arc<std::sync::atomic::AtomicBool>,
    holder: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl HeldHandle {
    /// The flag the holder watches, for a test that wants to let go early.
    fn release_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.release)
    }
}

#[cfg(windows)]
impl Drop for HeldHandle {
    fn drop(&mut self) {
        self.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.holder.take() {
            let _ = h.join();
        }
    }
}

/// Opens the handle and returns once it is really open, so nothing downstream
/// depends on a thread being scheduled promptly.
#[cfg(windows)]
async fn hold_a_handle(path: std::path::PathBuf) -> HeldHandle {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let opened = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (o, r) = (Arc::clone(&opened), Arc::clone(&release));
    let holder = std::thread::spawn(move || {
        // FILE_SHARE_READ only: no FILE_SHARE_DELETE, so the file cannot be
        // unlinked while this handle is open.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .expect("open the held file");
        o.store(true, Ordering::SeqCst);
        while !r.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(f);
    });
    while !opened.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    HeldHandle {
        release,
        holder: Some(holder),
    }
}

/// A handle that is released while the removal is still trying does not fail
/// the destroy.
///
/// Success is impossible before the release — the handle is held until this
/// test says so — so a green here is the retry doing its job and nothing else.
#[cfg(windows)]
#[tokio::test]
async fn remove_waits_out_a_handle_that_is_about_to_be_released() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "briefly-held").await;
    worktree::create(&layout, "main").await.unwrap();

    let held = hold_a_handle(layout.worktree_path.join("README.md")).await;
    let release = held.release_flag();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        release.store(true, Ordering::SeqCst);
    });

    worktree::remove(&layout).await.unwrap();

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
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "stuck").await;
    worktree::create(&layout, "main").await.unwrap();

    // Held for the whole call, and released by the guard's `Drop` whichever way
    // this test ends — including the panic an unexpected success would raise.
    let _held = hold_a_handle(layout.worktree_path.join("README.md")).await;

    let err = worktree::remove(&layout).await.unwrap_err();

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

/// When more than one step of a removal fails, the error says so.
///
/// The three steps run independently now, so two of them can fail for two
/// unrelated reasons. Returning only the first would park the workspace in
/// `Error(...)` with half the story, and the half it drops is the half nobody
/// can go back and ask about: the daemon has already finished the destroy.
#[tokio::test]
async fn a_removal_that_fails_twice_names_both_failures() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let layout = layout_for(dir.path(), &repo_path, "doomed").await;
    worktree::create(&layout, "main").await.unwrap();

    // The repository itself goes. The worktree directory is an ordinary
    // directory and still removes cleanly, so the first step succeeds — and
    // neither `worktree prune` nor the branch step has a repository left to run
    // in, so both of those fail, for the same underlying reason but as two
    // separate commands.
    std::fs::remove_dir_all(repo_path.join(".git")).unwrap();

    let err = worktree::remove(&layout).await.unwrap_err();

    assert!(
        err.message.contains("worktree prune"),
        "the first failure is the prune: {}",
        err.message
    );
    assert!(
        err.message.contains("also:"),
        "the second failure has to be in the message too: {}",
        err.message
    );
    assert!(
        err.message.contains("deleting the branch"),
        "the second failure has to say which step it was: {}",
        err.message
    );
}

// ---------------------------------------------------------------------------
// A registration a Windows git pruned.
//
// Windows git cannot see a worktree under the WSL user's home, so its
// `git worktree prune` deletes `<git_common>/worktrees/<id>` and leaves the
// directory and the branch where they are. `ensure_registered` puts the
// registration back when that is all that happened, and says why not
// otherwise.
// ---------------------------------------------------------------------------

/// What a Windows `git worktree prune` does to an unlocked registration.
fn prune_like_windows_git(layout: &Layout) {
    std::fs::remove_dir_all(layout.worktree_gitdir()).unwrap();
}

/// A workspace with a commit of the agent's in its private objects, a
/// modified tracked file and an untracked one: everything a repair must keep.
async fn workspace_with_work(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, Layout) {
    let repo_path = common::init_repo(dir);
    let layout = layout_for(dir, &repo_path, name).await;
    worktree::create(&layout, "main").await.unwrap();
    std::fs::write(layout.worktree_path.join("committed.txt"), "c\n").unwrap();
    common::commit_all(
        &layout.worktree_path,
        &layout.sandbox_git_env(),
        "agent commit",
    );
    std::fs::write(layout.worktree_path.join("README.md"), "changed\n").unwrap();
    std::fs::write(layout.worktree_path.join("wip.txt"), "wip\n").unwrap();
    (repo_path, layout)
}

#[tokio::test]
async fn ensure_registered_locks_an_intact_registration_that_is_not_locked() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let layout = layout_for(dir.path(), &repo_path, "older").await;
    worktree::create(&layout, "main").await.unwrap();
    // A workspace made before worktrees were locked.
    Git::new()
        .run(
            &repo_path,
            &[
                "worktree",
                "unlock",
                &layout.worktree_path.to_string_lossy(),
            ],
        )
        .await
        .unwrap();

    let got = worktree::ensure_registered(&layout).await.unwrap();

    assert_eq!(got, worktree::Registration::Intact);
    let reason = std::fs::read_to_string(layout.worktree_gitdir().join("locked")).unwrap();
    assert_eq!(reason.trim_end(), worktree::LOCK_REASON);
}

#[tokio::test]
async fn ensure_registered_leaves_an_existing_lock_alone() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let layout = layout_for(dir.path(), &repo_path, "user-locked").await;
    worktree::create(&layout, "main").await.unwrap();
    let locked = layout.worktree_gitdir().join("locked");
    std::fs::write(&locked, "the user's own reason\n").unwrap();

    let got = worktree::ensure_registered(&layout).await.unwrap();

    assert_eq!(got, worktree::Registration::Intact);
    assert_eq!(
        std::fs::read_to_string(&locked).unwrap(),
        "the user's own reason\n"
    );
}

#[tokio::test]
async fn ensure_registered_rebuilds_a_pruned_registration_and_keeps_the_work() {
    let dir = tempfile::tempdir().unwrap();
    let (repo_path, layout) = workspace_with_work(dir.path(), "pruned").await;
    let head_before = layout
        .daemon_git()
        .run(&repo_path, &["rev-parse", "bs/pruned/work"])
        .await
        .unwrap()
        .stdout;
    prune_like_windows_git(&layout);

    let got = worktree::ensure_registered(&layout).await.unwrap();

    assert_eq!(got, worktree::Registration::Repaired);
    let gitdir = layout.worktree_gitdir();
    assert_eq!(
        std::fs::read_to_string(gitdir.join("HEAD")).unwrap(),
        "ref: refs/heads/bs/pruned/work\n"
    );
    assert_eq!(
        std::fs::read_to_string(gitdir.join("locked"))
            .unwrap()
            .trim_end(),
        worktree::LOCK_REASON
    );
    // The branch has not moved, and the worktree still has the agent's work:
    // the commit, the modified file and the untracked one, none of them staged.
    let git = layout.worktree_git();
    let head = git
        .run(&layout.worktree_path, &["rev-parse", "HEAD"])
        .await
        .unwrap()
        .stdout;
    assert_eq!(head, head_before);
    let status = git
        .run(&layout.worktree_path, &["status", "--porcelain"])
        .await
        .unwrap()
        .stdout;
    assert_eq!(status, " M README.md\n?? wip.txt\n");
    assert_eq!(
        std::fs::read_to_string(layout.worktree_path.join("README.md")).unwrap(),
        "changed\n"
    );
    // Plain git, discovering the repository from the worktree the way a user's
    // shell would, sees the same thing.
    let plain = common::git_out(&layout.worktree_path, &["worktree", "list", "--porcelain"]);
    assert!(
        plain.contains("branch refs/heads/bs/pruned/work"),
        "{plain}"
    );
    assert!(plain.contains("locked"), "{plain}");

    // And a prune that cannot see the directory no longer takes it.
    let hidden = dir.path().join("hidden");
    std::fs::rename(dir.path().join("worktrees"), &hidden).unwrap();
    common::git_ok(&repo_path, &["worktree", "prune"]);
    std::fs::rename(&hidden, dir.path().join("worktrees")).unwrap();
    assert!(gitdir.join("HEAD").is_file());
}

#[tokio::test]
async fn ensure_registered_refuses_when_the_branch_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (repo_path, layout) = workspace_with_work(dir.path(), "no-branch").await;
    prune_like_windows_git(&layout);
    common::git_ok(&repo_path, &["branch", "-D", "bs/no-branch/work"]);

    let err = worktree::ensure_registered(&layout).await.unwrap_err();

    assert!(err.message.contains("bs/no-branch/work"), "{}", err.message);
    assert!(err.message.contains("no longer exists"), "{}", err.message);
    assert!(
        !layout.worktree_gitdir().exists(),
        "nothing half-built is left"
    );
}

#[tokio::test]
async fn ensure_registered_refuses_when_the_branch_is_checked_out_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let (repo_path, layout) = workspace_with_work(dir.path(), "taken").await;
    prune_like_windows_git(&layout);
    let other = dir.path().join("other");
    // Through `daemon_git`: the branch tip is in the private object directory.
    layout
        .daemon_git()
        .run(
            &repo_path,
            &["worktree", "add", &other.to_string_lossy(), "bs/taken/work"],
        )
        .await
        .unwrap();

    let err = worktree::ensure_registered(&layout).await.unwrap_err();

    assert!(err.message.contains("checked out"), "{}", err.message);
    assert!(!layout.worktree_gitdir().exists());
}

#[tokio::test]
async fn ensure_registered_refuses_a_gitfile_that_points_somewhere_else() {
    let dir = tempfile::tempdir().unwrap();
    let (_repo_path, layout) = workspace_with_work(dir.path(), "elsewhere").await;
    prune_like_windows_git(&layout);
    std::fs::write(
        layout.worktree_path.join(".git"),
        "gitdir: /somewhere/else/.git/worktrees/x\n",
    )
    .unwrap();

    let err = worktree::ensure_registered(&layout).await.unwrap_err();

    assert!(err.message.contains(".git"), "{}", err.message);
    assert!(!layout.worktree_gitdir().exists());
}

#[tokio::test]
async fn ensure_registered_refuses_when_the_worktree_directory_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (_repo_path, layout) = workspace_with_work(dir.path(), "gone").await;
    prune_like_windows_git(&layout);
    std::fs::remove_dir_all(&layout.worktree_path).unwrap();

    let err = worktree::ensure_registered(&layout).await.unwrap_err();

    assert!(err.message.contains("directory"), "{}", err.message);
    assert!(!layout.worktree_gitdir().exists());
}
