mod common;

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
    std::fs::write(repo_path.join("dirty.txt"), "x").unwrap();
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
