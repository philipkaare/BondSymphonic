//! The building blocks of an in-place workspace: what the daemon does to a
//! repository's `.git` before an agent may work in it, the git it runs there
//! itself, and -- in a real bubblewrap sandbox -- that git still works for the
//! agent while everything a git outside the sandbox would execute stays out of
//! its reach.

mod common;

use bondsymphonic_daemon::git::repo::RepoKind;
use bondsymphonic_daemon::workspace::in_place::{self, InPlaceLayout};
use bondsymphonic_proto::ErrorCode;
use std::path::{Path, PathBuf};

fn layout(dir: &Path, repo: &Path) -> InPlaceLayout {
    let no_hooks = dir.join("nohooks");
    std::fs::create_dir_all(&no_hooks).unwrap();
    InPlaceLayout::new(repo, &no_hooks)
}

/// Where the daemon would record what it created for this test's workspace:
/// under the data directory, never in the repository.
fn record(dir: &Path) -> PathBuf {
    dir.join("data/in-place/ws_test.created")
}

/// The top-level names in `.git`, sorted.
fn git_dir_entries(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn prepare_makes_only_the_documented_entries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // Whether `git init` makes `branches` depends on its template; take the
    // question away so the expected list does not.
    for name in ["hooks", "info", "branches"] {
        let _ = std::fs::remove_dir_all(repo.join(".git").join(name));
    }
    let before = git_dir_entries(&repo);
    let l = layout(dir.path(), &repo);

    l.prepare(false, &record(dir.path())).unwrap();

    let mut added: Vec<String> = git_dir_entries(&repo)
        .into_iter()
        .filter(|n| !before.contains(n))
        .collect();
    added.sort();
    assert_eq!(
        added,
        [
            "branches",
            "commondir",
            "hooks",
            "info",
            "remotes",
            "worktrees"
        ]
    );
    assert_eq!(std::fs::read(l.commondir()).unwrap(), b".\n");
    // Only the on-demand directories are recorded, and only once each.
    let recorded = std::fs::read_to_string(record(dir.path())).unwrap();
    let mut lines: Vec<&str> = recorded.lines().collect();
    lines.sort();
    assert_eq!(lines, ["branches", "remotes", "worktrees"]);
    // Idempotent: a restart prepares the same repository again, and what the
    // first start created is still remembered as the daemon's.
    l.prepare(false, &record(dir.path())).unwrap();
    assert_eq!(
        std::fs::read_to_string(record(dir.path())).unwrap(),
        recorded
    );
    // git still reads the repository as its own common dir.
    assert_eq!(
        common::git_out(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        ),
        common::git_out(&repo, &["rev-parse", "--path-format=absolute", "--git-dir"])
    );
    common::git_ok(&repo, &["status", "--porcelain"]);
}

#[test]
fn prepare_creates_config_worktree_only_with_the_extension_and_keeps_its_content() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(false, &record(dir.path())).unwrap();
    assert!(!l.config_worktree().exists());
    l.prepare(true, &record(dir.path())).unwrap();
    assert_eq!(std::fs::read(l.config_worktree()).unwrap(), b"");
    // The user's own file is theirs: never truncated.
    std::fs::write(l.config_worktree(), "[core]\n\tsparseCheckout = false\n").unwrap();
    l.prepare(true, &record(dir.path())).unwrap();
    assert!(std::fs::read_to_string(l.config_worktree())
        .unwrap()
        .contains("sparseCheckout"));
}

#[test]
fn prepare_refuses_a_commondir_it_did_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    std::fs::write(repo.join(".git/commondir"), "/somewhere/else\n").unwrap();
    let err = layout(dir.path(), &repo)
        .prepare(false, &record(dir.path()))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("commondir"), "{}", err.message);
    assert_eq!(
        std::fs::read_to_string(repo.join(".git/commondir")).unwrap(),
        "/somewhere/else\n",
        "the refusal must not touch the file"
    );
}

#[test]
fn release_takes_back_only_what_the_daemon_made_and_left_empty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let before = git_dir_entries(&repo);
    let l = layout(dir.path(), &repo);
    l.prepare(false, &record(dir.path())).unwrap();
    l.release(&record(dir.path()));
    assert_eq!(git_dir_entries(&repo), before);
    assert!(
        !record(dir.path()).exists(),
        "the record goes with the release"
    );

    // A worktrees dir with a registration in it is git's now, and stays.
    l.prepare(false, &record(dir.path())).unwrap();
    common::git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &dir.path().join("side").to_string_lossy(),
        ],
    );
    l.release(&record(dir.path()));
    assert!(l.worktrees().is_dir());
    assert!(!l.commondir().exists());

    // An empty `remotes` the user already had is theirs: not recorded, not
    // removed.
    let other = tempfile::tempdir().unwrap();
    let repo = common::init_repo(other.path());
    std::fs::create_dir_all(repo.join(".git/remotes")).unwrap();
    let l = layout(other.path(), &repo);
    l.prepare(false, &record(other.path())).unwrap();
    l.release(&record(other.path()));
    assert!(l.remotes().is_dir());

    // A record naming anything but the three on-demand directories is ignored.
    std::fs::create_dir_all(record(other.path()).parent().unwrap()).unwrap();
    std::fs::write(record(other.path()), "hooks\n../..\nobjects\n").unwrap();
    l.release(&record(other.path()));
    assert!(l.hooks().is_dir() && repo.join(".git/objects").is_dir());
}

#[test]
fn late_binds_follow_what_the_repository_has() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    let names = |v: Vec<PathBuf>| -> Vec<String> {
        v.iter()
            .map(|p| {
                p.strip_prefix(&repo)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    };
    assert_eq!(names(l.rw_binds()), ["", ".git"]);
    assert_eq!(
        names(l.late_ro_binds(false)),
        [
            ".git/config",
            ".git/commondir",
            ".git/hooks",
            ".git/info",
            ".git/worktrees",
            ".git/remotes",
            ".git/branches"
        ]
    );
    std::fs::create_dir_all(l.modules()).unwrap();
    assert_eq!(
        names(l.late_ro_binds(true)),
        [
            ".git/config",
            ".git/commondir",
            ".git/hooks",
            ".git/info",
            ".git/worktrees",
            ".git/remotes",
            ".git/branches",
            ".git/modules",
            ".git/config.worktree"
        ]
    );
}

#[test]
fn check_repository_names_the_folder_and_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.check_repository().unwrap();
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    let err = l.check_repository().unwrap_err();
    assert_eq!(
        err.message,
        format!(
            "The repository {} is missing or is no longer a git repository. Close the \
             workspace, or restore the folder and press Retry.",
            repo.display()
        )
    );
}

#[tokio::test]
async fn head_branch_and_diff_base_cover_detached_and_unborn() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    assert_eq!(
        in_place::head_branch(&l.git(), &repo).await.unwrap(),
        "main"
    );
    let head = common::git_out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(in_place::diff_base(&l.git(), &repo).await.unwrap(), head);
    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    assert_eq!(in_place::head_branch(&l.git(), &repo).await.unwrap(), "");

    let unborn = dir.path().join("unborn");
    std::fs::create_dir_all(&unborn).unwrap();
    common::git_ok(&unborn, &["init", "-q", "-b", "trunk"]);
    let u = layout(dir.path(), &unborn);
    assert_eq!(
        in_place::head_branch(&u.git(), &unborn).await.unwrap(),
        "trunk"
    );
    assert_eq!(
        in_place::diff_base(&u.git(), &unborn).await.unwrap(),
        in_place::EMPTY_TREE
    );
}

#[test]
fn only_a_root_with_its_own_git_directory_can_be_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    assert_eq!(in_place::in_place_refusal(&RepoKind::Root, &repo), None);
    let why = in_place::in_place_refusal(&RepoKind::Worktree, &repo).unwrap();
    assert!(why.contains("linked worktree"), "{why}");

    let separate = dir.path().join("separate");
    std::fs::create_dir_all(&separate).unwrap();
    common::git_ok(
        &separate,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            &dir.path().join("sep.git").to_string_lossy(),
        ],
    );
    let why = in_place::in_place_refusal(&RepoKind::Root, &separate).unwrap();
    assert!(why.contains(".git is a file"), "{why}");
    assert!(in_place::in_place_refusal(&RepoKind::Bare, &repo).is_some());
    assert!(in_place::in_place_refusal(&RepoKind::NotARepo, &repo).is_some());
}

#[test]
fn the_daemon_directory_and_the_filesystem_root_are_refused_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let data_inside = repo.join("data");
    std::fs::create_dir_all(&data_inside).unwrap();
    let why = in_place::target_refusal(&repo, &data_inside).unwrap();
    assert!(
        why.contains("contains the daemon's own data directory"),
        "{why}"
    );
    let why = in_place::target_refusal(&data_inside.join("x"), &data_inside).unwrap();
    assert!(
        why.contains("inside the daemon's own data directory"),
        "{why}"
    );
    assert!(in_place::target_refusal(Path::new("/"), &dir.path().join("d")).is_some());
    assert_eq!(
        in_place::target_refusal(&repo, &dir.path().join("elsewhere")),
        None
    );
}

#[test]
fn a_merge_refusal_carries_the_in_place_reason() {
    let e = in_place::nothing_to_merge();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert_eq!(e.data.unwrap()["reason"], "in_place");
    assert_eq!(
        e.message,
        "an in-place workspace has nothing to merge; commit and push from the checkout"
    );
}

/// The escape R1 in the plan closes, from the daemon's side: a planted
/// `commondir` with a config of its own must not reach the pinned git.
#[cfg(unix)]
#[tokio::test]
async fn the_pinned_git_ignores_a_planted_commondir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let evil = repo.join(".git/evil");
    std::fs::create_dir_all(&evil).unwrap();
    // A complete common dir of the agent's own: git refuses one without
    // objects and refs, and then the attack proves nothing.
    let git_dir = repo.join(".git");
    assert!(std::process::Command::new("cp")
        .arg("-r")
        .arg(git_dir.join("objects"))
        .arg(git_dir.join("refs"))
        .arg(&evil)
        .status()
        .unwrap()
        .success());
    let marker = dir.path().join("PWNED");
    std::fs::write(
        evil.join("config"),
        format!(
            "{}[core]\n\tfsmonitor = touch {}; false\n",
            std::fs::read_to_string(repo.join(".git/config")).unwrap(),
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(repo.join(".git/commondir"), format!("{}\n", evil.display())).unwrap();

    let l = layout(dir.path(), &repo);
    l.git()
        .run(&repo, &["status", "--porcelain"])
        .await
        .unwrap();
    assert!(
        !marker.exists(),
        "the daemon's git followed a planted commondir"
    );
}

#[cfg(target_os = "linux")]
mod bwrap {
    use super::*;
    use bondsymphonic_daemon::sandbox::{backend_for, SandboxCommand, SandboxHandle, SandboxSpec};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-all",
                "--die-with-parent",
                "true",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn run_in(handle: &Arc<dyn SandboxHandle>, script: &str) -> (i32, String) {
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["sh".into(), "-c".into(), script.into()],
                env: vec![],
                cwd: None,
                pty: None,
            })
            .await
            .unwrap();
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .await
            .unwrap();
        (child.exit.await.unwrap(), out)
    }

    /// The spec an in-place workspace gets, minus the parts (proxy, Claude,
    /// cache) that have nothing to do with `.git`. Home and run dir sit two
    /// levels under a data dir, as the backend expects.
    async fn sandbox_over(dir: &Path, l: &InPlaceLayout) -> Arc<dyn SandboxHandle> {
        let wc = l.worktree_config_enabled().await.unwrap();
        l.prepare(wc, &record(dir)).unwrap();
        let data = dir.join("data");
        let home = data.join("homes/ws_inplace");
        let run = data.join("run/ws_inplace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        // The agent's git refuses a repository it does not own otherwise; the
        // real daemon seeds the same line (`seed_home`).
        std::fs::write(home.join(".gitconfig"), "[safe]\n\tdirectory = *\n").unwrap();
        let same = |p: &PathBuf| (p.clone(), p.clone());
        let spec = SandboxSpec {
            id: "ws_inplace".into(),
            rw_binds: l.rw_binds().iter().map(same).collect(),
            ro_binds: vec![],
            late_ro_binds: l.late_ro_binds(wc).iter().map(same).collect(),
            home,
            run_dir: run,
            env: vec![],
            cwd: l.root.clone(),
        };
        backend_for("linux_bwrap").start(&spec).await.unwrap()
    }

    const PROBE: &str = r#"
root='@ROOT@'
cd "$root" || exit 99
try() { if sh -c "$2" >/dev/null 2>&1; then echo "$1: SUCCEEDED"; else echo "$1: refused"; fi; }
printf 'edited\n' >> README.md
git add README.md && git -c user.name=a -c user.email=a@a -c commit.gpgsign=false commit -q -m inside && echo "commit: ok"
git switch -q -c from-inside && echo "switch: ok"
git stash list >/dev/null && echo "stash: ok"
try config "printf x >> .git/config"
try hook "printf x > .git/hooks/pre-commit"
try info "printf x > .git/info/exclude"
try commondir "printf /tmp/evil > .git/commondir"
try commondir-unlink "rm -f .git/commondir"
try worktrees "mkdir .git/worktrees/planted"
try remotes "printf 'URL: /tmp/evil\n' > .git/remotes/origin"
try branches "printf '/tmp/evil\n' > .git/branches/origin"
try move-git "mv .git moved-git"
try replace-git "mkdir -p newgit && mv -T newgit .git"
try move-root "mv '$root' '$root-moved'"
try git-config "git config core.fsmonitor 'touch /tmp/pwned'"
try config-worktree "printf x > .git/config.worktree"
"#;

    #[tokio::test]
    async fn the_agent_uses_git_and_cannot_touch_what_a_host_git_executes() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        common::git_ok(&repo, &["config", "extensions.worktreeConfig", "true"]);
        let config_before = std::fs::read(repo.join(".git/config")).unwrap();
        // Template-dependent; see `prepare_makes_only_the_documented_entries`.
        let _ = std::fs::remove_dir_all(repo.join(".git/branches"));
        let l = layout(dir.path(), &repo);
        let before = git_dir_entries(&repo);
        let handle = sandbox_over(dir.path(), &l).await;

        // Nothing but the documented entries appeared, bwrap included.
        let added: Vec<String> = git_dir_entries(&repo)
            .into_iter()
            .filter(|n| !before.contains(n))
            .collect();
        assert_eq!(
            added,
            [
                "branches",
                "commondir",
                "config.worktree",
                "remotes",
                "worktrees"
            ]
        );

        let (_, out) = run_in(&handle, &PROBE.replace("@ROOT@", &repo.to_string_lossy())).await;
        for expected in [
            "commit: ok",
            "switch: ok",
            "stash: ok",
            "config: refused",
            "hook: refused",
            "info: refused",
            "commondir: refused",
            "commondir-unlink: refused",
            "worktrees: refused",
            "remotes: refused",
            "branches: refused",
            "move-git: refused",
            "replace-git: refused",
            "move-root: refused",
            "git-config: refused",
            "config-worktree: refused",
        ] {
            assert!(
                out.lines().any(|l| l == expected),
                "expected {expected:?} in:\n{out}"
            );
        }
        // The commit is in the repository, on the branch the agent made.
        assert_eq!(
            common::git_out(&repo, &["log", "-1", "--format=%s", "main"]),
            "inside"
        );
        assert_eq!(
            common::git_out(&repo, &["branch", "--show-current"]),
            "from-inside"
        );
        assert_eq!(
            std::fs::read(repo.join(".git/config")).unwrap(),
            config_before
        );
        assert_eq!(std::fs::read(l.commondir()).unwrap(), b".\n");
        handle.shutdown().await.unwrap();
    }

    /// Plan R4: the mount points survive, the rest of `.git` does not. The
    /// user guide says so; this pins what "survive" means.
    #[tokio::test]
    async fn removing_git_leaves_its_mount_points_and_config() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let config_before = std::fs::read(repo.join(".git/config")).unwrap();
        let l = layout(dir.path(), &repo);
        let handle = sandbox_over(dir.path(), &l).await;
        let (code, _) = run_in(&handle, &format!("rm -rf '{}/.git'", repo.display())).await;
        assert_ne!(code, 0, "rm -rf .git must fail");
        assert!(repo.join(".git").is_dir());
        assert!(l.hooks().is_dir());
        assert_eq!(
            std::fs::read(repo.join(".git/config")).unwrap(),
            config_before
        );
        handle.shutdown().await.unwrap();
    }
}
