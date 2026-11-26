//! The git shim lets a cherry-pick, a revert and a conflicted merge finish in
//! a worktree whose shared `.git` directory cannot be written, as a worktree
//! workspace's is in its sandbox.
//!
//! Git before 2.48 locks the shared `packed-refs` to delete any ref, the
//! worktree's own `CHERRY_PICK_HEAD` included, and the lock is created beside
//! `packed-refs`. Without the common-directory view a cherry-pick committed
//! but left `CHERRY_PICK_HEAD` behind, and every cherry-pick after it was
//! refused. This runs the daemon binary as `git`, the way the sandbox does,
//! against a worktree whose shared `.git` is made read-only (its refs and the
//! worktree's own directory stay writable, as the sandbox's mounts leave
//! them), and whose `commondir` names the view as the daemon's read-only file
//! does in the sandbox -- so it needs no bwrap.

#![cfg(unix)]

use bondsymphonic_daemon::sandbox::git_shim::{refresh_common_view, COMMON_VIEW};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DAEMON: &str = env!("CARGO_BIN_EXE_bondsymphonic-daemon");

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    worktree: PathBuf,
    common: PathBuf,
    objects: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("wt");
        let objects = dir.path().join("private-objects");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(DAEMON, bin.join("git")).unwrap();
        let fixture = Fixture {
            common: repo.join(".git"),
            _dir: dir,
            repo,
            worktree,
            objects,
            bin,
        };
        fixture.host(&["init", "-q", "-b", "main"]);
        std::fs::write(fixture.repo.join("f"), "one\n").unwrap();
        fixture.host(&["add", "f"]);
        fixture.host(&["commit", "-q", "-m", "one"]);
        std::fs::write(fixture.repo.join("f"), "one\ntwo\n").unwrap();
        fixture.host(&["commit", "-q", "-am", "two"]);
        std::fs::write(fixture.repo.join("g"), "g\n").unwrap();
        fixture.host(&["add", "g"]);
        fixture.host(&["commit", "-q", "-m", "three"]);
        fixture.host(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "bs/x/work",
            fixture.worktree.to_str().unwrap(),
            "main~2",
        ]);
        // What the daemon sets up for the sandbox: the view, and a
        // `commondir` naming it.
        let view = fixture.view();
        refresh_common_view(&fixture.common, &view).unwrap();
        std::fs::write(
            fixture.common.join("worktrees/wt/commondir"),
            format!("{}\n", view.display()),
        )
        .unwrap();
        // What the sandbox's mounts amount to for this: nothing can be created
        // in the shared `.git` itself.
        set_writable(&fixture.common, false);
        fixture
    }

    fn view(&self) -> PathBuf {
        self.common.join("worktrees/wt").join(COMMON_VIEW)
    }

    fn host(&self, args: &[&str]) {
        let out = with_identity(Command::new("git").args(args).current_dir(&self.repo))
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .output()
            .unwrap();
        assert!(out.status.success(), "host git {args:?}: {out:?}");
    }

    /// `git` in the worktree, through the shim.
    fn shim(&self, args: &[&str]) -> (bool, String) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let rest = std::env::split_paths(&path).filter(|d| d != Path::new("/opt/bs/bin"));
        let path = std::env::join_paths(std::iter::once(self.bin.clone()).chain(rest)).unwrap();
        let out = with_identity(Command::new(self.bin.join("git")).args(args))
            .current_dir(&self.worktree)
            .env("PATH", path)
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_COMMON_DIR")
            .env("BS_GIT_COMMON_DIR", self.view())
            .env("BS_GIT_REAL_COMMON_DIR", &self.common)
            .env("BS_GIT_OBJECT_DIRECTORY", &self.objects)
            .env(
                "BS_GIT_ALTERNATE_OBJECT_DIRECTORIES",
                self.common.join("objects"),
            )
            .env("BS_GIT_BRANCH_PREFIX", "bs/x/")
            .env("BS_GIT_WORKSPACE_BRANCH", "bs/x/work")
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn root_ref(&self, name: &str) -> bool {
        self.common.join("worktrees/wt").join(name).exists()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        set_writable(&self.common, true);
    }
}

fn with_identity(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
}

fn set_writable(dir: &Path, writable: bool) {
    let mode = if writable { 0o755 } else { 0o555 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn cherry_pick_and_revert_finish_in_a_read_only_shared_git_dir() {
    if is_root() {
        eprintln!("SKIP: root ignores the read-only shared .git");
        return;
    }
    let f = Fixture::new();

    // A clean pick: committed, and nothing left in progress.
    let (ok, out) = f.shim(&["cherry-pick", "main~1"]);
    assert!(ok, "{out}");
    assert!(!f.root_ref("CHERRY_PICK_HEAD"), "left behind: {out}");
    assert!(!out.contains("packed-refs.lock"), "{out}");

    // A conflicted one, resolved and continued.
    std::fs::write(f.worktree.join("g"), "mine\n").unwrap();
    let (ok, out) = f.shim(&["add", "g"]);
    assert!(ok, "{out}");
    let (ok, out) = f.shim(&["commit", "-qm", "conflicting g"]);
    assert!(ok, "{out}");
    let (ok, out) = f.shim(&["cherry-pick", "main"]);
    assert!(!ok, "expected a conflict: {out}");
    assert!(f.root_ref("CHERRY_PICK_HEAD"));
    std::fs::write(f.worktree.join("g"), "resolved\n").unwrap();
    let (ok, out) = f.shim(&["add", "g"]);
    assert!(ok, "{out}");
    let (ok, out) = f.shim(&["cherry-pick", "--continue"]);
    assert!(ok, "{out}");
    assert!(!f.root_ref("CHERRY_PICK_HEAD"), "left behind: {out}");
    assert!(!f.root_ref("AUTO_MERGE"), "left behind: {out}");

    // A revert after it, which a leftover CHERRY_PICK_HEAD used to refuse.
    let (ok, out) = f.shim(&["revert", "--no-edit", "HEAD"]);
    assert!(ok, "{out}");
    assert!(!f.root_ref("REVERT_HEAD"), "left behind: {out}");

    // The commits went to the workspace's private store, as ever, and the
    // shared `.git` was not written.
    let (ok, head) = f.shim(&["rev-parse", "HEAD"]);
    assert!(ok, "{head}");
    let head = head.trim();
    let loose = f.objects.join(&head[..2]).join(&head[2..]);
    assert!(loose.is_file(), "{head} is not in the private store");
    assert!(!f.common.join("packed-refs").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&f.common)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".lock"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// Refs the host packs after the sandbox started are seen at the agent's next
/// git command, and the agent cannot pack refs into the view.
#[test]
fn the_view_follows_the_hosts_packed_refs_and_refuses_pack_refs() {
    if is_root() {
        eprintln!("SKIP: root ignores the read-only shared .git");
        return;
    }
    let f = Fixture::new();
    set_writable(&f.common, true);
    f.host(&["branch", "late"]);
    f.host(&["pack-refs", "--all"]);
    set_writable(&f.common, false);
    assert!(
        !f.common.join("refs/heads/late").exists(),
        "packed, not loose"
    );
    let (ok, out) = f.shim(&["rev-parse", "--verify", "-q", "refs/heads/late"]);
    assert!(ok, "the host's packed branch is not seen: {out}");

    let (ok, out) = f.shim(&["pack-refs", "--all"]);
    assert!(!ok, "{out}");
    assert!(out.contains("not available"), "{out}");
    let (ok, out) = f.shim(&["rev-parse", "--verify", "-q", "refs/heads/bs/x/work"]);
    assert!(ok, "{out}");
}
