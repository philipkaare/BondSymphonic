mod common;

use bondsymphonic_daemon::git::repo::RepoKind;
use bondsymphonic_daemon::git::{repo, Git};
use bondsymphonic_proto::ErrorCode;
use common::init_repo;

#[tokio::test]
async fn inspect_reports_default_branch_and_clean_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = init_repo(dir.path());
    let git = Git::new();
    let info = repo::inspect(&git, &repo_path).await.unwrap();
    assert_eq!(info.default_branch, "main");
    assert!(info.branches.contains(&"main".to_string()));
    assert!(!info.is_dirty);
    assert!(info.remotes.is_empty());
    // An edited *tracked* file. An untracked one is deliberately not dirty —
    // see `an_untracked_file_alone_is_not_what_is_dirty_means`.
    std::fs::write(
        repo_path.join("README.md"),
        "edited
",
    )
    .unwrap();
    assert!(repo::inspect(&git, &repo_path).await.unwrap().is_dirty);
    assert!(repo::branch_exists(&git, &repo_path, "main").await.unwrap());
    assert!(!repo::branch_exists(&git, &repo_path, "nope").await.unwrap());
    assert_eq!(
        repo::head_commit(&git, &repo_path, "main")
            .await
            .unwrap()
            .len(),
        40
    );
}

#[tokio::test]
async fn failures_map_to_git_error_with_details() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new();
    let err = git
        .run(dir.path(), &["rev-parse", "--git-common-dir"])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError);
    let data = err.data.unwrap();
    assert!(data["command"]
        .as_str()
        .unwrap()
        .starts_with("git rev-parse"));
    assert!(data["exit_code"].as_i64().unwrap() != 0);
    assert!(data["stderr"]
        .as_str()
        .unwrap()
        .contains("not a git repository"));
}

// ---------------------------------------------------------------------------
// A path that is not a repository (yet)
// ---------------------------------------------------------------------------

/// `repo.inspect` is what the New Agent dialog asks about a folder the user has
/// just picked, and a folder that is not a repository is an ordinary answer
/// there rather than a failure: the dialog offers to initialise it.
#[tokio::test]
async fn inspect_answers_for_a_folder_that_is_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("notes");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("a.txt"), "hello").unwrap();

    let info = repo::inspect(&Git::new(), &plain).await.unwrap();
    assert!(!info.is_repo);
    assert!(info.exists);
    assert!(info.branches.is_empty());
    assert_eq!(
        info.default_branch, "main",
        "the branch such a folder would be initialised with"
    );
    assert!(!info.is_dirty);
    assert!(info.remotes.is_empty());
}

/// A folder that is not there yet is still a legitimate answer: it will be
/// created along with the repository.
#[tokio::test]
async fn inspect_answers_for_a_folder_that_does_not_exist_yet() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-yet");

    let info = repo::inspect(&Git::new(), &missing).await.unwrap();
    assert!(!info.is_repo);
    assert!(!info.exists);
    assert_eq!(info.default_branch, "main");
}

/// The one path shape that stays an error. "The folder will be created" means
/// creating one directory inside a directory the user picked; a whole missing
/// tree is a typo in a path box, and answering "it will be created" to that
/// would have the daemon build a directory tree nobody meant to name.
#[tokio::test]
async fn inspect_fails_when_the_parent_directory_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("nowhere").join("child");

    let err = repo::inspect(&Git::new(), &deep).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(
        err.message.contains("nowhere"),
        "the message must name the directory that is missing: {}",
        err.message
    );
}

/// A file where a folder was named is not a repository and never can be, so it
/// is an error rather than "not a repository yet".
#[tokio::test]
async fn inspect_fails_when_the_path_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file.txt");
    std::fs::write(&file, "x").unwrap();

    let err = repo::inspect(&Git::new(), &file).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("not a directory"), "{}", err.message);
}

// ---------------------------------------------------------------------------
// init_repo
// ---------------------------------------------------------------------------

/// A `Git` with no identity of its own to fall back on: neither the system nor
/// the global config is read, so `user.email` really is unset and the daemon
/// has to supply one.
fn git_without_an_identity(dir: &std::path::Path) -> Git {
    let empty = dir.join("empty-gitconfig");
    std::fs::write(&empty, "").unwrap();
    Git::new()
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .with_env("GIT_CONFIG_GLOBAL", empty.to_string_lossy().into_owned())
}

fn log_line(repo: &std::path::Path, format: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["log", "-1", format, "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git log: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn init_repo_creates_the_directory_and_one_commit_on_main() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("brand-new");
    let git = git_without_an_identity(dir.path());

    repo::init_repo(&git, &fresh).await.unwrap();

    let info = repo::inspect(&git, &fresh).await.unwrap();
    assert!(info.is_repo);
    assert!(info.exists);
    assert_eq!(info.default_branch, "main");
    assert_eq!(info.branches, vec!["main".to_string()]);
    assert!(!info.is_dirty, "a fresh repository has nothing to commit");
    assert_eq!(
        repo::head_commit(&git, &fresh, "main").await.unwrap().len(),
        40
    );
    assert_eq!(log_line(&fresh, "--format=%s"), "Initial commit");
    assert_eq!(
        log_line(&fresh, "--format=%an <%ae>"),
        "BondSymphonic <bondsymphonic@localhost>",
        "with no identity configured the daemon supplies one, or the commit fails"
    );
}

/// A configured identity wins wherever there is one: the fallback exists to
/// keep the commit from failing, not to sign somebody else's work as the
/// daemon.
#[tokio::test]
async fn init_repo_keeps_the_configured_identity_when_there_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("their-gitconfig");
    std::fs::write(
        &cfg,
        "[user]\n\tname = A Person\n\temail = a@example.test\n",
    )
    .unwrap();
    let git = Git::new()
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .with_env("GIT_CONFIG_GLOBAL", cfg.to_string_lossy().into_owned());
    let fresh = dir.path().join("theirs");

    repo::init_repo(&git, &fresh).await.unwrap();

    assert_eq!(
        log_line(&fresh, "--format=%an <%ae>"),
        "A Person <a@example.test>"
    );
}

/// Initialising is only ever a step on the way to creating a workspace, so it
/// has to be safe on a repository that is already there: it must not add a
/// commit, move a branch or touch anything.
#[tokio::test]
async fn init_repo_leaves_an_existing_repository_alone() {
    let dir = tempfile::tempdir().unwrap();
    let existing = init_repo(dir.path());
    let git = git_without_an_identity(dir.path());
    let before = repo::head_commit(&git, &existing, "HEAD").await.unwrap();

    repo::init_repo(&git, &existing).await.unwrap();

    assert_eq!(
        repo::head_commit(&git, &existing, "HEAD").await.unwrap(),
        before
    );
    assert_eq!(log_line(&existing, "--format=%s"), "init");
}

#[tokio::test]
async fn init_repo_refuses_a_path_that_is_not_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file.txt");
    std::fs::write(&file, "x").unwrap();
    let git = git_without_an_identity(dir.path());

    let err = repo::init_repo(&git, &file).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("not a directory"), "{}", err.message);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "x",
        "nothing may be written over"
    );
}

/// `git init` copies `init.templateDir` into the new repository, hooks
/// included, and the commit that follows would run them. Initialising a folder
/// because a dialog offered to is not somebody typing `git commit`, so the
/// caller pins `core.hooksPath` at an empty directory and none of it fires.
#[tokio::test]
async fn init_repo_runs_no_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let template = dir.path().join("template");
    let marker = dir.path().join("the-hook-ran");
    std::fs::create_dir_all(template.join("hooks")).unwrap();
    let hook = template.join("hooks").join("pre-commit");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", common::sh_path(&marker)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let no_hooks = dir.path().join("nohooks");
    std::fs::create_dir_all(&no_hooks).unwrap();
    let git = git_without_an_identity(dir.path())
        .with_config("init.templateDir", &common::sh_path(&template))
        .with_config("core.hooksPath", &common::sh_path(&no_hooks));
    let fresh = dir.path().join("hooked");

    repo::init_repo(&git, &fresh).await.unwrap();

    assert!(
        !marker.exists(),
        "the repository hook must not run when the daemon initialises the folder"
    );
    assert_eq!(log_line(&fresh, "--format=%s"), "Initial commit");
}

// ---------------------------------------------------------------------------
// Fix round 1: this path, not an enclosing one; and what may be written to
// ---------------------------------------------------------------------------

/// A directory with a `.git` file git cannot make sense of. Git fails on it with
/// its general fatal (exit 128) *on what is meant to be a repository*, which is
/// the case the "not a repository" classifier has to tell apart from a folder
/// that is simply not one.
fn dir_with_a_broken_gitfile(dir: &std::path::Path) -> std::path::PathBuf {
    let broken = dir.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join(".git"), "this is not a gitfile\n").unwrap();
    broken
}

/// The classifier decides whether the daemon may *write*, so it has to mean
/// exactly one thing. Exit 128 is git's general fatal and is shared with
/// failures that happen on real repositories; only git's own wording says the
/// path is not a repository at all.
#[tokio::test]
async fn only_gits_own_wording_counts_as_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    let err = repo::common_dir(&git, &plain).await.unwrap_err();
    assert!(repo::is_not_a_repository(&err), "{err:?}");

    let broken = dir_with_a_broken_gitfile(dir.path());
    let err = repo::common_dir(&git, &broken).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError);
    assert!(
        !repo::is_not_a_repository(&err),
        "a repository git cannot read is not an empty folder: {err:?}"
    );
}

/// `rev-parse` searches upwards, so the question "is this a repository" answered
/// naively is really about the nearest enclosing one. A plain folder inside a
/// repository would otherwise be reported with its parent's branches, dirty
/// state and remotes, and the dialog would offer them as if they were the
/// folder's own.
#[tokio::test]
async fn inspect_says_a_folder_inside_a_repository_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let outer = init_repo(dir.path());
    let inside = outer.join("sub").join("deeper");
    std::fs::create_dir_all(&inside).unwrap();
    let git = Git::new();

    let info = repo::inspect(&git, &inside).await.unwrap();
    assert!(
        !info.is_repo,
        "the folder is not a repository, its parent is"
    );
    assert!(info.exists);
    assert!(info.branches.is_empty(), "{:?}", info.branches);
    assert_eq!(info.default_branch, "main");

    // A folder that does not exist inside a repository answers the same way,
    // with `exists` false, which is what `create` then acts on.
    let missing = outer.join("not-yet");
    let info = repo::inspect(&git, &missing).await.unwrap();
    assert!(!info.is_repo);
    assert!(!info.exists);

    // And the repository itself still answers as one.
    assert!(repo::inspect(&git, &outer).await.unwrap().is_repo);
}

/// So `init_repo` there makes a repository of its own rather than returning
/// "already a repository" about somebody else's.
#[tokio::test]
async fn init_repo_inside_a_repository_makes_a_repository_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let outer = init_repo(dir.path());
    let git = git_without_an_identity(dir.path());
    let outer_head = repo::head_commit(&git, &outer, "HEAD").await.unwrap();
    let inside = outer.join("nested");

    repo::init_repo(&git, &inside).await.unwrap();

    assert!(inside.join(".git").is_dir(), "its own git directory");
    assert_eq!(
        repo::classify(&git, &inside).await.unwrap(),
        RepoKind::Root,
        "a root of its own, not a folder inside the enclosing repository"
    );
    assert_eq!(log_line(&inside, "--format=%s"), "Initial commit");
    assert_eq!(
        repo::head_commit(&git, &outer, "HEAD").await.unwrap(),
        outer_head,
        "the enclosing repository must not be touched"
    );
}

/// Two targets `init_if_missing` must never act on, whatever a client sends: a
/// filesystem root, and the home of the user the daemon runs as — the home whose
/// credentials and git config the daemon reads.
#[test]
fn a_root_and_the_daemon_users_home_are_refused_as_init_targets() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home").join("someone");
    std::fs::create_dir_all(&home).unwrap();

    assert!(repo::init_target_refusal(&home, Some(&home)).is_some());
    assert!(
        repo::init_target_refusal(&home.join("code"), Some(&home)).is_none(),
        "a directory inside the home is where people keep their code"
    );

    let root = std::path::Path::new(&home)
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();
    assert!(
        repo::init_target_refusal(&root, Some(&home)).is_some(),
        "{}",
        root.display()
    );
    assert!(repo::init_target_refusal(std::path::Path::new("/"), None).is_some());
}

/// The guard is wired into `init_repo` itself, not only into its callers.
#[tokio::test]
async fn init_repo_refuses_a_filesystem_root() {
    let dir = tempfile::tempdir().unwrap();
    let git = git_without_an_identity(dir.path());
    let root = dir.path().ancestors().last().unwrap();

    let err = repo::init_repo(&git, root).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(
        err.message.contains("refusing to initialise"),
        "{}",
        err.message
    );
}

/// An existing folder with files in it is *allowed*: "I have some code, make it
/// a project" is the ordinary case, and the empty commit adds nothing to the
/// index, so those files stay untracked and none of them is committed.
#[tokio::test]
async fn init_repo_accepts_a_folder_that_already_has_files_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("some-code");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("main.rs"), "fn main() {}\n").unwrap();
    let git = git_without_an_identity(dir.path());

    repo::init_repo(&git, &existing).await.unwrap();

    let info = repo::inspect(&git, &existing).await.unwrap();
    assert!(info.is_repo);
    assert!(
        !info.is_dirty,
        "the files are still there and still untracked, which is not an uncommitted change"
    );
    assert_eq!(log_line(&existing, "--format=%s"), "Initial commit");
}

/// A bare repository has no working tree, so it is neither something a
/// workspace can be made from nor a folder the daemon may write into. Both
/// mistakes are available: reporting it as "not a repository" would send
/// `init_if_missing` on to run `git init` and a commit *inside* it, and
/// reporting it as a repository would make a workspace whose worktree cannot be
/// checked out. It is refused by name instead.
#[tokio::test]
async fn a_bare_repository_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("origin.git");
    let out = std::process::Command::new("git")
        .args(["init", "--bare", "-q", "-b", "main"])
        .arg(&bare)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git init --bare: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let git = git_without_an_identity(dir.path());

    for err in [
        repo::inspect(&git, &bare).await.unwrap_err(),
        repo::init_repo(&git, &bare).await.unwrap_err(),
    ] {
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(
            err.message.contains("is a bare repository"),
            "the message has to say what is wrong, not quote git: {}",
            err.message
        );
    }

    // Nothing was initialised inside it: no working tree, and no ref, so no
    // commit was made either.
    assert!(!bare.join(".git").exists());
    let refs = git
        .run(&bare, &["for-each-ref", "--format=%(refname)", "refs/"])
        .await
        .unwrap()
        .stdout;
    assert!(
        refs.trim().is_empty(),
        "the bare repository gained refs: {refs}"
    );
}

// ---------------------------------------------------------------------------
// Task 4 (AL5): one classifier, five answers.
// ---------------------------------------------------------------------------

/// Adds a linked worktree on a new branch and returns its path.
fn add_worktree(repo: &std::path::Path, branch: &str, at: &std::path::Path) {
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", branch])
        .arg(at)
        .arg("main")
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The five shapes a path can have, from the one function every caller now
/// asks.
///
/// There used to be three answers to this question — `is_not_a_repository`
/// reading git's stderr, an `is_repo_root` comparing `--show-toplevel`, and the
/// ladder inside `workspace.create` — and they did not agree. A subdirectory of
/// a repository was "not a repository" to one and "a repository" to another, so
/// `repo.inspect` offered to initialise a folder that `workspace.create` then
/// refused.
#[tokio::test]
async fn classify_tells_the_five_shapes_apart() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new();

    // A plain directory.
    let plain = dir.path().join("notes");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(
        repo::classify(&git, &plain).await.unwrap(),
        RepoKind::NotARepo
    );

    // A path that is not there at all is not a repository either.
    assert_eq!(
        repo::classify(&git, &dir.path().join("not-yet"))
            .await
            .unwrap(),
        RepoKind::NotARepo
    );

    // A repository root.
    let root = init_repo(dir.path());
    assert_eq!(repo::classify(&git, &root).await.unwrap(), RepoKind::Root);

    // A directory inside one: the answer names the repository it is inside, so
    // the caller can say which one rather than "somewhere above you".
    let inside = root.join("sub").join("deeper");
    std::fs::create_dir_all(&inside).unwrap();
    match repo::classify(&git, &inside).await.unwrap() {
        RepoKind::InsideEnclosing { root: found } => assert_eq!(
            repo::canonical_ish(&found),
            repo::canonical_ish(&root),
            "the enclosing repository has to be named"
        ),
        other => panic!("{other:?}"),
    }

    // A bare repository: no working tree, so neither usable nor writable.
    let bare = dir.path().join("origin.git");
    let out = std::process::Command::new("git")
        .args(["init", "--bare", "-q", "-b", "main"])
        .arg(&bare)
        .output()
        .unwrap();
    assert!(out.status.success(), "git init --bare");
    assert_eq!(repo::classify(&git, &bare).await.unwrap(), RepoKind::Bare);

    // A linked worktree of a repository: its own checkout, sharing the main
    // repository's object store and refs.
    let linked = dir.path().join("linked");
    add_worktree(&root, "side", &linked);
    assert_eq!(
        repo::classify(&git, &linked).await.unwrap(),
        RepoKind::Worktree
    );

    // A repository git cannot read is none of the five: it stays a failure, so
    // nothing downstream treats it as an empty folder and writes into it.
    let broken = dir_with_a_broken_gitfile(dir.path());
    assert!(repo::classify(&git, &broken).await.is_err());
}

/// `inspect` and `classify` are the same classifier seen from two sides, so a
/// linked worktree is a repository to both: a workspace made from one is a
/// worktree of the same store, which works.
#[tokio::test]
async fn a_linked_worktree_is_a_repository_to_inspect_and_to_classify() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_repo(dir.path());
    let linked = dir.path().join("linked");
    add_worktree(&root, "side", &linked);
    let git = Git::new();

    assert_eq!(
        repo::classify(&git, &linked).await.unwrap(),
        RepoKind::Worktree
    );
    let info = repo::inspect(&git, &linked).await.unwrap();
    assert!(info.is_repo);
    assert!(info.exists);
    assert!(info.branches.contains(&"side".to_string()));
}

/// The disagreement AL5 names, from the outside: what `repo.inspect` says about
/// a folder inside a repository and what `workspace.create` does with it have
/// to be the same story. `inspect` answers "not a repository" — the folder is
/// not one — and `create` refuses by naming the repository it is inside, so the
/// user is told which one to pick.
#[tokio::test]
async fn a_subdirectory_of_a_repository_is_refused_by_naming_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_repo(dir.path());
    let inside = root.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    let git = Git::new();

    let info = repo::inspect(&git, &inside).await.unwrap();
    assert!(
        !info.is_repo,
        "the folder is not a repository, its parent is"
    );
    assert!(info.exists);

    // And the classifier the refusal is built on says the same thing in more
    // words: not "no repository here", but "you are inside this one".
    assert!(matches!(
        repo::classify(&git, &inside).await.unwrap(),
        RepoKind::InsideEnclosing { .. }
    ));
}

/// `RepoInfo.is_dirty` and the merge guard answer the same question the same
/// way.
///
/// The merge guard asks `status --porcelain --untracked-files=no`, because an
/// untracked file is not work a merge can destroy — git refuses by name rather
/// than overwriting one — and a log, a build output or a scratch note lying in a
/// checkout is the ordinary state of a working directory. `inspect` asked a
/// bare `status --porcelain`, so the New Agent dialog announced "uncommitted
/// changes" for a repository the daemon would merge into without a murmur.
#[tokio::test]
async fn an_untracked_file_alone_is_not_what_is_dirty_means() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = init_repo(dir.path());
    let git = Git::new();

    std::fs::write(repo_path.join("scratch.log"), "today's notes\n").unwrap();
    assert!(
        !repo::inspect(&git, &repo_path).await.unwrap().is_dirty,
        "an untracked file is not an uncommitted change"
    );

    // A tracked file that has been edited is, and that is the whole of the
    // difference: the flag still means something.
    std::fs::write(repo_path.join("README.md"), "edited\n").unwrap();
    assert!(
        repo::inspect(&git, &repo_path).await.unwrap().is_dirty,
        "an edited tracked file is an uncommitted change"
    );
}
