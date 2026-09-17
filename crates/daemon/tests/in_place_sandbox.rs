//! The building blocks of an in-place workspace: what the daemon does to a
//! repository's `.git` before an agent may work in it, the git it runs there
//! itself, and -- in a real bubblewrap sandbox -- that git still works for the
//! agent while everything a git outside the sandbox would execute stays out of
//! its reach.

mod common;

use bondsymphonic_daemon::git::repo::RepoKind;
use bondsymphonic_daemon::workspace::in_place::{self, InPlaceLayout, ProtectedSnapshot};
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

/// The workspace `record` belongs to, which is also what the persistent hold
/// in `.git/worktrees` is named after.
fn ws_id() -> bondsymphonic_proto::WorkspaceId {
    "ws_test".into()
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

    l.prepare(&ws_id(), &record(dir.path())).unwrap();

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
            "config.worktree",
            "hooks",
            "info",
            "remotes",
            "worktrees"
        ]
    );
    assert_eq!(std::fs::read(l.commondir()).unwrap(), b".\n");
    // Only the on-demand entries are recorded, and only once each.
    let recorded = std::fs::read_to_string(record(dir.path())).unwrap();
    let mut lines: Vec<&str> = recorded.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        ["branches", "config.worktree", "remotes", "worktrees"]
    );
    // Idempotent: a restart prepares the same repository again, and what the
    // first start created is still remembered as the daemon's.
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
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
fn prepare_always_creates_config_worktree_and_keeps_its_content() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    // With the extension off too: `git sparse-checkout init` turns it on
    // later and keeps whatever the file holds by then.
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    assert_eq!(std::fs::read(l.config_worktree()).unwrap(), b"");
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    assert_eq!(std::fs::read(l.config_worktree()).unwrap(), b"");
    // The user's own file is theirs: never truncated.
    std::fs::write(l.config_worktree(), "[core]\n\tsparseCheckout = false\n").unwrap();
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    assert!(std::fs::read_to_string(l.config_worktree())
        .unwrap()
        .contains("sparseCheckout"));
    // ... and not taken back at Close, though the daemon made the file.
    l.release(&ws_id(), &record(dir.path()));
    assert!(l.config_worktree().is_file());
}

/// A bind follows a symlink, and writing a missing file through a dangling
/// one creates its target: every protected entry must be what git makes, and
/// a refusal writes nothing.
#[cfg(unix)]
#[test]
fn prepare_refuses_protected_entries_that_are_not_what_git_makes() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    for (name, make_dir) in [
        ("config.worktree", false),
        ("commondir", false),
        ("hooks", true),
        ("worktrees", true),
        ("config", false),
    ] {
        let sub = dir.path().join(name);
        std::fs::create_dir_all(&sub).unwrap();
        let repo = common::init_repo(&sub);
        let entry = repo.join(".git").join(name);
        let _ = std::fs::remove_dir_all(&entry);
        let _ = std::fs::remove_file(&entry);
        // Dangling: `outside` does not exist.
        std::os::unix::fs::symlink(&outside, &entry).unwrap();
        let before = git_dir_entries(&repo);
        let err = layout(&sub, &repo).prepare(&ws_id(), &record(&sub)).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams, "{name}");
        let expected = if make_dir {
            format!(".git/{name} that is not a directory")
        } else {
            format!(".git/{name} that is not a regular file")
        };
        assert!(err.message.contains(&expected), "{name}: {}", err.message);
        assert!(!outside.exists(), "{name}: wrote through the symlink");
        assert_eq!(git_dir_entries(&repo), before, "{name}");
        assert!(!record(&sub).exists(), "{name}");
    }

    // A directory where git keeps a file is refused the same way.
    let repo = common::init_repo(&dir.path().join("dir"));
    std::fs::create_dir(repo.join(".git/config.worktree")).unwrap();
    let err = layout(dir.path(), &repo)
        .prepare(&ws_id(), &record(dir.path()))
        .unwrap_err();
    assert!(
        err.message
            .contains("config.worktree that is not a regular file"),
        "{}",
        err.message
    );
}

#[test]
fn prepare_refuses_a_commondir_it_did_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    std::fs::write(repo.join(".git/commondir"), "/somewhere/else\n").unwrap();
    let err = layout(dir.path(), &repo)
        .prepare(&ws_id(), &record(dir.path()))
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
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    l.release(&ws_id(), &record(dir.path()));
    assert_eq!(git_dir_entries(&repo), before);
    assert!(
        !record(dir.path()).exists(),
        "the record goes with the release"
    );

    // A worktrees dir with a registration in it is git's now, and stays.
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
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
    l.release(&ws_id(), &record(dir.path()));
    assert!(l.worktrees().is_dir());
    assert!(!l.commondir().exists());

    // An empty `remotes` the user already had is theirs: not recorded, not
    // removed.
    let other = tempfile::tempdir().unwrap();
    let repo = common::init_repo(other.path());
    std::fs::create_dir_all(repo.join(".git/remotes")).unwrap();
    let l = layout(other.path(), &repo);
    l.prepare(&ws_id(), &record(other.path())).unwrap();
    l.release(&ws_id(), &record(other.path()));
    assert!(l.remotes().is_dir());

    // A record naming anything but the three on-demand directories is ignored.
    std::fs::create_dir_all(record(other.path()).parent().unwrap()).unwrap();
    std::fs::write(record(other.path()), "hooks\n../..\nobjects\n").unwrap();
    l.release(&ws_id(), &record(other.path()));
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
        names(l.late_ro_binds()),
        [
            ".git/config",
            ".git/commondir",
            ".git/hooks",
            ".git/info",
            ".git/worktrees",
            ".git/remotes",
            ".git/branches",
            ".git/config.worktree"
        ]
    );
    std::fs::create_dir_all(l.modules()).unwrap();
    assert_eq!(
        names(l.late_ro_binds()),
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

/// A replaced or removed entry has lost its bind, and an entry written in
/// place still has one -- which is exactly why the contents are read too: on a
/// Windows drive, writing in place through an alias of the name is how an agent
/// reaches the file at all.
#[test]
fn a_snapshot_notices_replaced_removed_and_rewritten_entries() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.check(None), None);

    std::fs::OpenOptions::new()
        .append(true)
        .open(l.config())
        .unwrap()
        .write_all(b"[bs]\n\tnote = edited in place\n")
        .unwrap();
    assert_eq!(
        snapshot.check(None).map(|b| b.entries),
        Some(vec![".git/config".to_string()])
    );

    // `git config` writes `config.lock` and renames it over `config`.
    common::git_ok(&repo, &["config", "core.fsmonitor", "touch /nonexistent"]);
    let breach = snapshot.check(None).unwrap();
    assert_eq!(breach.entries, [".git/config"]);
    assert!(
        breach
            .config_diff
            .starts_with("--- a/.git/config\n+++ b/.git/config\n"),
        "{}",
        breach.config_diff
    );
    for line in [
        "+[bs]",
        "+\tnote = edited in place",
        "+\tfsmonitor = touch /nonexistent",
        " [core]",
    ] {
        assert!(
            breach.config_diff.lines().any(|l| l == line),
            "{line:?} in:\n{}",
            breach.config_diff
        );
    }
    assert!(!breach
        .config_diff
        .lines()
        .any(|l| l.starts_with('-') && !l.starts_with("---")));

    // What the last `git worktree remove` does -- except that the persistent
    // hold is what stops git from doing it while an in-place workspace exists.
    std::fs::remove_dir_all(l.worktrees()).unwrap();
    let breach = snapshot.check(None).unwrap();
    assert_eq!(breach.entries, [".git/config", ".git/worktrees"]);
    assert_eq!(
        breach.sentence(),
        "Git files this workspace protects changed while the agent was running (.git/config, \
         .git/worktrees), so its sandbox was stopped. Check .git/config for settings you did \
         not make — the daemon log shows what changed — then press Retry."
    );
}

/// The check has to be quiet about everything git and the daemon do by
/// themselves, or an agent is stopped at random by the user's own work.
#[test]
fn what_git_and_the_daemon_do_by_themselves_is_not_a_breach() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    // A sibling worktree workspace of the same repository, registered before
    // the agent starts.
    let first = dir.path().join("first");
    common::git_ok(
        &repo,
        &["worktree", "add", "-b", "a", &first.to_string_lossy()],
    );
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.check(None), None);

    // `git gc` rewrites `.git/info/refs` through `update-server-info`, and
    // git's own `--auto` maintenance runs it behind an ordinary commit.
    common::git_ok(&repo, &["gc"]);
    assert!(l.info().join("refs").is_file(), "gc wrote no info/refs");
    assert_eq!(snapshot.check(None), None, "after git gc");

    // The daemon writes the sibling's `config.worktree` empty on every start
    // of that workspace, which is not news.
    let registration = l.worktrees().join("first");
    std::fs::write(registration.join("config.worktree"), b"").unwrap();
    assert_eq!(snapshot.check(None), None, "after config.worktree is reset");
    // Its index and `HEAD` are rewritten by the daemon's own work in that
    // worktree, several times a minute, and are nothing git executes.
    common::git_ok(&first, &["status", "--porcelain"]);
    common::git_ok(&first, &["switch", "-c", "b"]);
    assert_eq!(snapshot.check(None), None, "after work in the sibling");

    // Another sibling registered while the agent works is the daemon's own
    // doing too: `workspace.create` on this repository does exactly this.
    let second = dir.path().join("second");
    common::git_ok(
        &repo,
        &["worktree", "add", "-b", "c", &second.to_string_lossy()],
    );
    assert_eq!(snapshot.check(None), None, "after a second worktree add");

    // A `config.worktree` with something in it is not the daemon's doing: it
    // is what that sibling's own `git status` would execute.
    std::fs::write(
        registration.join("config.worktree"),
        "[core]\n\tfsmonitor = touch /nonexistent\n",
    )
    .unwrap();
    let breach = snapshot.check(None).unwrap();
    assert_eq!(breach.entries, [".git/worktrees/first/config.worktree"]);
}

/// Planting a program under a protected directory is what the content check
/// is for; so is taking one away.
#[test]
fn a_hook_appearing_or_going_is_a_breach() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.check(None), None);

    let hook = l.hooks().join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ntouch /nonexistent\n").unwrap();
    assert_eq!(
        snapshot.check(None).map(|b| b.entries),
        Some(vec![".git/hooks/pre-commit".to_string()])
    );
    std::fs::remove_file(&hook).unwrap();
    assert_eq!(snapshot.check(None), None, "the hook is gone again");

    // A legacy remote definition is a URL, and an `ext::` URL is a command.
    let remote = l.remotes().join("origin");
    std::fs::write(&remote, "URL: ext::sh -c touch% /nonexistent\n").unwrap();
    assert_eq!(
        snapshot.check(None).map(|b| b.entries),
        Some(vec![".git/remotes/origin".to_string()])
    );
    std::fs::remove_file(&remote).unwrap();

    // A sample hook is covered too: it is only its name that keeps git from
    // running it, and the name is the agent's to change.
    let sample = l.hooks().join("pre-commit.sample");
    if sample.is_file() {
        let mut text = std::fs::read_to_string(&sample).unwrap();
        text.push_str("touch /nonexistent\n");
        std::fs::write(&sample, text).unwrap();
        assert_eq!(
            snapshot.check(None).map(|b| b.entries),
            Some(vec![".git/hooks/pre-commit.sample".to_string()])
        );
    }
}

/// Plan I3: a second name for a protected file is a way past every bind, so
/// it is refused before a sandbox starts and is a breach while one runs.
#[cfg(unix)]
#[test]
fn a_second_name_for_a_protected_file_is_refused_and_is_a_breach() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.check(None), None);

    // What an agent does in the window a Retry opens: both paths are on the
    // same writable mount, and the bind is re-made on only one of them.
    let elsewhere = repo.join("theirs");
    std::fs::hard_link(l.config(), &elsewhere).unwrap();
    assert_eq!(
        snapshot.check(None).map(|b| b.entries),
        Some(vec![".git/config".to_string()])
    );
    // And the next start does not paper over it.
    let err = l.check_preparable().unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert_eq!(
        err.message,
        ".git/config has more than one name; an agent cannot work in place in it"
    );
    assert!(l.snapshot().is_err());

    std::fs::remove_file(&elsewhere).unwrap();
    assert!(l.check_preparable().is_ok());
    assert_eq!(snapshot.check(None), None);
}

/// Plan I7: git deletes `.git/worktrees` as soon as it finds it empty --
/// `worktree prune` and `gc --auto` do it behind an ordinary commit -- and the
/// read-only bind does not survive the directory. The hold is what keeps it
/// there for as long as an agent works in the checkout.
#[test]
fn the_hold_keeps_the_worktrees_directory_for_as_long_as_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let hold = l.worktrees().join(".bs-inplace-ws_test");
    assert!(hold.join("locked").is_file(), "prepare made no hold");

    // Git's own cleanups leave it, and leave the directory standing.
    for argv in [
        vec!["worktree", "prune", "-v"],
        vec!["gc"],
        vec!["fsck"],
        vec!["status", "--porcelain"],
    ] {
        common::git_ok(&repo, &argv);
        assert!(l.worktrees().is_dir(), "{argv:?} removed .git/worktrees");
        assert!(hold.join("locked").is_file(), "{argv:?} removed the hold");
    }
    // An entry with no `gitdir` is not a worktree as far as git is concerned.
    assert!(!common::git_out(&repo, &["worktree", "list", "--porcelain"]).contains("bs-inplace"));

    // A worktree of the repository comes and goes underneath it, which is what
    // used to take the directory with it.
    let sibling = dir.path().join("sibling");
    common::git_ok(
        &repo,
        &["worktree", "add", "-b", "x", &sibling.to_string_lossy()],
    );
    common::git_ok(
        &repo,
        &["worktree", "remove", "--force", &sibling.to_string_lossy()],
    );
    assert!(l.worktrees().is_dir(), "the last worktree took it away");

    l.release(&ws_id(), &record(dir.path()));
    assert!(!hold.exists(), "release left the hold behind");
    assert!(!l.worktrees().exists(), "release left .git/worktrees behind");
}

#[test]
fn a_breach_with_unchanged_files_has_no_diff() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    std::fs::remove_dir(l.remotes()).unwrap();
    let breach = snapshot.check(None).unwrap();
    assert_eq!(breach.entries, [".git/remotes"]);
    assert_eq!(breach.config_diff, "");
}

#[test]
fn the_snapshot_binds_exactly_what_it_watches() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.late_ro_binds(), l.late_ro_binds());
    // `modules` appearing afterwards changes the layout's answer, not the
    // snapshot's.
    std::fs::create_dir(l.modules()).unwrap();
    assert!(!snapshot.late_ro_binds().contains(&l.modules()));
    assert!(l.late_ro_binds().contains(&l.modules()));
}

/// With a pid, a `mountinfo` that cannot be read is no evidence that anything
/// is still bound.
#[test]
fn an_unreadable_mountinfo_is_a_breach() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(&ws_id(), &record(dir.path())).unwrap();
    let snapshot = l.snapshot().unwrap();
    assert_eq!(snapshot.check(None), None);
    let breach = snapshot.check(Some(u32::MAX)).unwrap();
    assert_eq!(breach.entries.len(), snapshot.late_ro_binds().len() + 1);
    assert_eq!(breach.entries[0], ".git");
    assert!(breach.entries.contains(&".git/config".to_string()));
    assert_eq!(breach.config_diff, "");
}

/// Plan N1: after a breach the agent may have had `.git/config` to itself, so
/// what is there must not stall or flood the check that stops it.
#[cfg(unix)]
#[test]
fn a_breach_is_answered_promptly_whatever_replaced_the_config() {
    fn check_within(snapshot: in_place::ProtectedSnapshot) -> in_place::ProtectionBreach {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(snapshot.check(None));
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("check blocked")
            .expect("a breach")
    }
    let fresh = || {
        let sub = tempfile::tempdir().unwrap();
        let repo = common::init_repo(sub.path());
        let l = layout(sub.path(), &repo);
        l.prepare(&ws_id(), &record(sub.path())).unwrap();
        let snapshot = l.snapshot().unwrap();
        // Moved aside rather than removed, so the new entry cannot get its
        // inode number back: identity is all `check(None)` has.
        std::fs::rename(l.config(), l.git_dir.join("config.old")).unwrap();
        (sub, l, snapshot)
    };

    // A FIFO with no writer: a plain open would wait forever.
    let (_keep, l, snapshot) = fresh();
    assert!(std::process::Command::new("mkfifo")
        .arg(l.config())
        .status()
        .unwrap()
        .success());
    let breach = check_within(snapshot);
    assert_eq!(breach.entries, [".git/config"]);
    assert_eq!(
        breach.config_diff,
        ".git/config is no longer a regular file\n"
    );

    // A symlink to an endless device.
    let (_keep, l, snapshot) = fresh();
    std::os::unix::fs::symlink("/dev/zero", l.config()).unwrap();
    let breach = check_within(snapshot);
    assert_eq!(
        breach.config_diff,
        ".git/config is no longer a regular file\n"
    );

    // A sparse file far larger than anything worth reading.
    let (_keep, l, snapshot) = fresh();
    std::fs::File::create(l.config())
        .unwrap()
        .set_len(100 << 30)
        .unwrap();
    let breach = check_within(snapshot);
    assert_eq!(
        breach.config_diff,
        ".git/config is larger than 256 KiB; not shown\n"
    );

    // A regular file of many lines, with a terminal escape in it: the diff
    // is capped and the escape does not reach the log as one.
    let (_keep, l, snapshot) = fresh();
    let long: String = (0..1000)
        .map(|i| format!("\tkey{i} = \x1b[2Jvalue\n"))
        .collect();
    std::fs::write(l.config(), long).unwrap();
    let breach = check_within(snapshot);
    let lines: Vec<&str> = breach.config_diff.lines().collect();
    assert_eq!(lines.len(), 201, "{}", breach.config_diff);
    assert!(lines[200].starts_with("... ") && lines[200].ends_with(" more lines not shown"));
    assert!(!breach.config_diff.contains('\x1b'));
    assert!(breach.config_diff.contains("\\u{1b}[2Jvalue"));
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
    use bondsymphonic_daemon::sandbox::{
        backend_for, SandboxChild, SandboxCommand, SandboxHandle, SandboxSpec,
    };
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
    async fn sandbox_over(
        dir: &Path,
        l: &InPlaceLayout,
    ) -> (Arc<dyn SandboxHandle>, ProtectedSnapshot) {
        l.prepare(&ws_id(), &record(dir)).unwrap();
        let snapshot = l.snapshot().unwrap();
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
            late_ro_binds: snapshot.late_ro_binds().iter().map(same).collect(),
            home,
            run_dir: run,
            env: vec![],
            cwd: l.root.clone(),
        };
        let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();
        (handle, snapshot)
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
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;

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
        // Nothing the agent did, allowed or refused, reads as a breach.
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None);
        (sleeper.killer)();
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
        let (handle, _) = sandbox_over(dir.path(), &l).await;
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

    /// The host pid of a process inside the sandbox, whose `mountinfo` is the
    /// sandbox's. The spawn answers with the pid inside the sandbox's own pid
    /// namespace, so the sleeper is found by a command line no other process
    /// has.
    async fn sandbox_host_pid(handle: &Arc<dyn SandboxHandle>) -> (u32, SandboxChild) {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let arg = format!(
            "3600.{}{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let child = handle
            .spawn(SandboxCommand {
                argv: vec!["sleep".into(), arg.clone()],
                env: vec![],
                cwd: None,
                pty: None,
            })
            .await
            .unwrap();
        let wanted = format!("sleep\0{arg}\0");
        for _ in 0..100 {
            for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                if std::fs::read(entry.path().join("cmdline")).is_ok_and(|c| c == wanted.as_bytes())
                {
                    return (pid, child);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the sleeper never showed up in /proc");
    }

    /// Plan C1: a git outside the sandbox that replaces a bound entry
    /// detaches its bind, and the only defence is noticing. The repository
    /// path has a space in it, which `mountinfo` spells `\040`.
    #[tokio::test]
    async fn a_host_git_replacing_config_mid_session_is_a_breach() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("in place ")
            .tempdir()
            .unwrap();
        let repo = common::init_repo(dir.path());
        let l = layout(dir.path(), &repo);
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None, "an untouched session");

        // The user edits the file in place. The bind stands and the inode is
        // the same, so only the contents say anything happened -- and they
        // have to, because on a Windows drive an alias of the name is how an
        // agent writes this very file with the bind standing.
        let mut config = std::fs::read_to_string(l.config()).unwrap();
        config.push_str("[bs]\n\tnote = edited in place\n");
        std::fs::OpenOptions::new()
            .write(true)
            .open(l.config())
            .and_then(|mut f| std::io::Write::write_all(&mut f, config.as_bytes()))
            .unwrap();
        let edited = snapshot.check(Some(pid)).expect("an edit in place");
        assert_eq!(edited.entries, [".git/config"]);
        assert!(
            edited
                .config_diff
                .lines()
                .any(|line| line == "+\tnote = edited in place"),
            "{}",
            edited.config_diff
        );

        // What `git config` does: write `config.lock`, rename it over.
        let lock = repo.join(".git/config.lock");
        std::fs::write(
            &lock,
            format!("{config}[core]\n\tfsmonitor = touch /tmp/pwned\n"),
        )
        .unwrap();
        std::fs::rename(&lock, l.config()).unwrap();
        let breach = snapshot.check(Some(pid)).unwrap();
        assert_eq!(breach.entries, [".git/config"]);
        assert!(
            breach
                .config_diff
                .lines()
                .any(|line| line == "+\tfsmonitor = touch /tmp/pwned"),
            "{}",
            breach.config_diff
        );
        (sleeper.killer)();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_host_removing_worktrees_mid_session_is_a_breach_even_once_it_is_back() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let l = layout(dir.path(), &repo);
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None);

        // What the last `git worktree remove` used to do, before the hold.
        std::fs::remove_dir_all(l.worktrees()).unwrap();
        let breach = snapshot.check(Some(pid)).unwrap();
        assert_eq!(breach.entries, [".git/worktrees"]);
        assert_eq!(breach.config_diff, "");

        // The next `git worktree add` makes it again, possibly with the old
        // inode number; the sandbox's mount is gone either way.
        std::fs::create_dir(l.worktrees()).unwrap();
        let breach = snapshot.check(Some(pid)).unwrap();
        assert_eq!(breach.entries, [".git/worktrees"]);
        let mounts = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap();
        assert!(
            !mounts.contains(&format!(" {} ", l.worktrees().display())),
            "the bind should be gone:\n{mounts}"
        );
        (sleeper.killer)();
        handle.shutdown().await.unwrap();
    }

    /// Plan N2: git's lock-and-rename often hands `.git/config` its old inode
    /// number back (two writes in a row, as `git branch -u` does), so only the
    /// mount check sees that the bind is gone.
    #[tokio::test]
    async fn two_host_config_writes_are_a_breach_whatever_the_inode() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let l = layout(dir.path(), &repo);
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None);
        common::git_ok(&repo, &["config", "branch.main.remote", "origin"]);
        common::git_ok(&repo, &["config", "branch.main.merge", "refs/heads/main"]);
        let breach = snapshot.check(Some(pid)).unwrap();
        assert_eq!(breach.entries, [".git/config"]);
        assert!(
            breach
                .config_diff
                .lines()
                .any(|line| line == "+\tmerge = refs/heads/main"),
            "{}",
            breach.config_diff
        );
        (sleeper.killer)();
        handle.shutdown().await.unwrap();
    }

    /// A temporary directory on a Windows drive, or `None` when this host has
    /// none to offer.
    ///
    /// DrvFs is where the alias hole lives, and it is also where this IDE's
    /// users keep their repositories: they pick a `C:\...` folder in a Windows
    /// dialog and the daemon is handed `/mnt/c/...`. `$BS_DRVFS_TEST_DIR`
    /// first, so a host that keeps its scratch space elsewhere can say where;
    /// otherwise a Windows user's own temp directory, which is the one place
    /// under `/mnt/c` a Linux process can be sure it may write.
    fn drvfs_tempdir() -> Option<tempfile::TempDir> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(named) = std::env::var("BS_DRVFS_TEST_DIR") {
            candidates.push(PathBuf::from(named));
        }
        if let Ok(users) = std::fs::read_dir("/mnt/c/Users") {
            for user in users.flatten() {
                candidates.push(user.path().join("AppData/Local/Temp"));
            }
        }
        candidates.into_iter().find_map(|base| {
            tempfile::Builder::new()
                .prefix("bs-inplace-")
                .tempdir_in(base)
                .ok()
        })
    }

    /// Rewrites a file without replacing it: the same inode, the same mount,
    /// which is how a Windows drive's aliases and a user's editor both write.
    fn write_in_place(path: &Path, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .and_then(|mut f| f.write_all(bytes))
            .unwrap();
    }

    /// Plan C1, the DrvFs hole: a read-only bind protects one Linux dentry,
    /// and on a Windows drive `.git/CONFIG`, `.GIT/config` and the 8.3 short
    /// name `GIT~1/config` are three more dentries for the same file. The
    /// mount, the device and the inode all say nothing happened. What the
    /// agent wrote is the only thing that does.
    #[tokio::test]
    async fn a_write_through_a_windows_name_alias_is_caught() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let Some(drive) = drvfs_tempdir() else {
            eprintln!("SKIP: no Windows drive to test on");
            return;
        };
        // Only the repository goes on the Windows drive. The sandbox's home
        // and run directory stay on a Linux filesystem, because `sandbox-init`
        // cannot make what it needs there on 9p (`EOPNOTSUPP`) -- and the
        // daemon's own data directory is never on a Windows drive either.
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(drive.path());
        let l = layout(dir.path(), &repo);
        let config_before = std::fs::read(l.config()).unwrap();
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None, "an untouched session");

        let root = repo.display().to_string();
        let mut got_past_a_bind = false;
        for alias in [".git/CONFIG", ".GIT/config", "GIT~1/config"] {
            let (_, out) = run_in(
                &handle,
                &format!(
                    "cd '{root}' && printf '[core]\\n\\tfsmonitor = touch /tmp/pwned\\n' >> \
                     '{alias}' && echo WROTE"
                ),
            )
            .await;
            if !out.contains("WROTE") {
                // A case-sensitive mount, or a volume with 8.3 names turned
                // off. Nothing reached the file, so there is nothing to catch.
                continue;
            }
            got_past_a_bind = true;
            let breach = snapshot
                .check(Some(pid))
                .unwrap_or_else(|| panic!("a write through {alias} went unnoticed"));
            assert_eq!(breach.entries, [".git/config"], "through {alias}");
            assert!(
                breach
                    .config_diff
                    .lines()
                    .any(|line| line == "+\tfsmonitor = touch /tmp/pwned"),
                "through {alias}:\n{}",
                breach.config_diff
            );
            // Put back, so the next alias is judged on its own.
            write_in_place(&l.config(), &config_before);
            assert_eq!(
                snapshot.check(Some(pid)),
                None,
                "after the write through {alias} was undone"
            );
        }
        assert!(
            got_past_a_bind,
            "no alias got past a bind: this is not a case-insensitive filesystem, and the test \
             proves nothing"
        );

        // A hook is a program git runs, and it is planted by appearing rather
        // than by changing.
        let (_, out) = run_in(
            &handle,
            &format!("cd '{root}' && printf '#!/bin/sh\\ntouch /tmp/pwned\\n' > .git/HOOKS/pre-commit && echo WROTE"),
        )
        .await;
        assert!(out.contains("WROTE"), "the hooks alias no longer resolves");
        let breach = snapshot
            .check(Some(pid))
            .expect("a hook planted through an alias went unnoticed");
        assert_eq!(breach.entries, [".git/hooks/pre-commit"]);

        (sleeper.killer)();
        handle.shutdown().await.unwrap();
    }

    /// Plan N4: a checkout reached through a symlinked directory is watched
    /// under whichever spelling the sandbox's `mountinfo` uses, so an
    /// untouched session is not a breach on every poll.
    #[tokio::test]
    async fn a_checkout_under_a_symlinked_directory_is_not_a_breach() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        common::init_repo(&real);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let l = layout(dir.path(), &link.join("repo"));
        let (handle, snapshot) = sandbox_over(dir.path(), &l).await;
        let (pid, sleeper) = sandbox_host_pid(&handle).await;
        assert_eq!(snapshot.check(Some(pid)), None);
        let (_, out) = run_in(
            &handle,
            &format!("printf x >> '{}' && echo WROTE", l.config().display()),
        )
        .await;
        assert!(!out.contains("WROTE"), "the config bind is missing");
        (sleeper.killer)();
        handle.shutdown().await.unwrap();
    }
}
