//! `workspace.fetch`, and the automatic fetch when a workspace comes up.
//!
//! Agents cannot fetch for themselves, by design: the sandbox holds none of the
//! user's credentials, and the repository's shared `.git` -- where
//! `refs/remotes/origin/*` live -- is mounted read-only into a worktree
//! workspace. So the daemon fetches on the host, as the user, and every sandbox
//! of the repository sees the new `origin/*` refs and objects through the mounts
//! it already has: the read-only `.git` and the object store its alternates
//! name.
//!
//! The fetch runs in the main repository with hooks pinned to the daemon's
//! empty directory, like every other daemon-side git operation that is not a
//! push (see [`crate::git::worktree::Layout::daemon_git`]).
//!
//! It also runs with every worktree workspace's private object store of the
//! repository as an alternate. An agent's branches live in the shared
//! `refs/heads/bs/<name>/`, but the commits on them are in its workspace's own
//! store until they are pushed or merged; to plain git in the main repository
//! such a branch names an object that is not there, and `git fetch` -- which
//! reads every local ref -- stops with `fatal: bad object refs/heads/bs/...`.

use crate::daemon::Daemon;
use crate::git::{path_arg, Git};
use bondsymphonic_proto::{FetchResult, RpcError, WorkspaceId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How recently a repository must have been fetched for a workspace coming up
/// to skip its own fetch. A startup restores every workspace at once, and
/// several of them are often of one repository.
const AUTO_FETCH_INTERVAL: Duration = Duration::from_secs(300);

/// When each repository was last fetched, by [`crate::git::repo_lock`]'s key.
fn last_fetched() -> &'static parking_lot::Mutex<HashMap<PathBuf, Instant>> {
    static LAST: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    LAST.get_or_init(Default::default)
}

/// `workspace.fetch`: fetches the workspace's repository now, however
/// recently it was fetched.
pub async fn fetch(d: &Daemon, id: &WorkspaceId) -> Result<FetchResult, RpcError> {
    let ws = d.workspace(id)?;
    let stores = private_stores(d, &ws.repo_path);
    fetch_repo(&ws.repo_path, &d.dirs.no_hooks(), &stores).await
}

/// The private object stores of every workspace of `repo` that has one, so a
/// daemon-side git in the main repository can read the objects its agents'
/// branches point at. In-place workspaces have none.
pub fn private_stores(d: &Daemon, repo: &Path) -> Vec<PathBuf> {
    let key = crate::git::repo::canonical_ish(repo);
    d.registry
        .list()
        .iter()
        .filter(|ws| crate::git::repo::canonical_ish(&ws.repo_path) == key)
        .map(|ws| d.dirs.objects(&ws.id))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// The fetch a workspace gets when it becomes `Ready`, in the background.
///
/// Skipped when the repository was fetched within [`AUTO_FETCH_INTERVAL`].
/// Failures only go to the log: the workspace is up either way, and an offline
/// laptop must not turn every start into an error.
pub fn spawn_auto_fetch(repo: PathBuf, no_hooks: PathBuf, stores: Vec<PathBuf>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let key = crate::git::repo::canonical_ish(&repo);
        {
            let mut last = last_fetched().lock();
            if last
                .get(&key)
                .is_some_and(|at| at.elapsed() < AUTO_FETCH_INTERVAL)
            {
                return;
            }
            // Booked before the fetch, so the other workspaces of this
            // repository coming up at the same moment do not queue behind it.
            last.insert(key, Instant::now());
        }
        match fetch_repo(&repo, &no_hooks, &stores).await {
            Ok(r) if r.has_origin => {
                tracing::info!(repo = %repo.display(), updated = r.updated, "fetched origin")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(repo = %repo.display(), "fetching origin failed: {}", e.message)
            }
        }
    });
}

/// Fetches `origin` into `repo`, under the repository's lock so it never
/// races a merge or a push of the same repository. `stores` are the
/// workspaces' private object stores ([`private_stores`]).
pub async fn fetch_repo(
    repo: &Path,
    no_hooks: &Path,
    stores: &[PathBuf],
) -> Result<FetchResult, RpcError> {
    let mut git = host_git(no_hooks);
    if !stores.is_empty() {
        if let Ok(joined) = std::env::join_paths(stores) {
            git = git.with_env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                joined.to_string_lossy().into_owned(),
            );
        }
    }
    let lock = crate::git::repo_lock(repo);
    let _guard = lock.lock().await;
    let remotes = git.run(repo, &["remote"]).await?;
    if !remotes.stdout.lines().any(|r| r.trim() == "origin") {
        return Ok(FetchResult {
            updated: 0,
            has_origin: false,
        });
    }
    let out = git
        .run(
            repo,
            &["fetch", "--prune", "--recurse-submodules=no", "origin"],
        )
        .await?;
    last_fetched()
        .lock()
        .insert(crate::git::repo::canonical_ish(repo), Instant::now());
    Ok(FetchResult {
        updated: updated_refs(&out.stderr),
        has_origin: true,
    })
}

/// The git a fetch runs with: no hooks, and -- when the GitHub CLI is
/// installed -- its credential helper for github.com.
///
/// `gh auth login` stores a token without configuring git to use it unless
/// `gh auth setup-git` is run too, and the daemon's Setup page only does the
/// first. Added for github.com alone and after any helper the user has
/// configured, so their own setup still answers first.
fn host_git(no_hooks: &Path) -> Git {
    let git = Git::new().with_config("core.hooksPath", &path_arg(no_hooks));
    match gh_on_path() {
        Some(gh) => git.with_config(
            "credential.https://github.com.helper",
            &format!("!'{}' auth git-credential", gh.display()),
        ),
        None => git,
    }
}

/// The real `gh`, if `PATH` has one. Not `BS_GH_BIN`: that hook stands in for
/// `gh pr create` in tests and answers nothing else.
fn gh_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.starts_with("/mnt/"))
        .map(|dir| dir.join("gh"))
        .find(|p| p.is_file())
}

/// How many refs a fetch's report says it created, moved or pruned: the lines
/// of the form ` * [new branch]  main -> origin/main`. Refs already up to
/// date are only listed with `--verbose`, which this fetch does not pass.
fn updated_refs(stderr: &str) -> u32 {
    stderr.lines().filter(|l| l.contains(" -> ")).count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_refs_a_fetch_reports() {
        let report = "From https://github.com/o/r\n   \
                      1a2b3c4..5d6e7f8  main       -> origin/main\n \
                      * [new branch]      feature    -> origin/feature\n \
                      - [deleted]         (none)     -> origin/gone\n";
        assert_eq!(updated_refs(report), 3);
        assert_eq!(updated_refs(""), 0);
    }

    #[tokio::test]
    async fn a_repository_without_origin_has_nothing_to_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let hooks = tempfile::tempdir().unwrap();
        let r = fetch_repo(dir.path(), hooks.path(), &[]).await.unwrap();
        assert!(!r.has_origin);
        assert_eq!(r.updated, 0);
    }

    #[tokio::test]
    async fn fetches_new_branches_from_origin() {
        let git = |dir: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        git(
            upstream.path(),
            &["commit", "-q", "--allow-empty", "-m", "one"],
        );
        let clone = tempfile::tempdir().unwrap();
        git(
            clone.path(),
            &["clone", "-q", &path_arg(upstream.path()), "."],
        );
        git(
            upstream.path(),
            &["commit", "-q", "--allow-empty", "-m", "two"],
        );
        git(upstream.path(), &["branch", "feature"]);

        let hooks = tempfile::tempdir().unwrap();
        let r = fetch_repo(clone.path(), hooks.path(), &[]).await.unwrap();
        assert!(r.has_origin);
        assert_eq!(r.updated, 2, "main moved and feature is new");
        let again = fetch_repo(clone.path(), hooks.path(), &[]).await.unwrap();
        assert_eq!(again.updated, 0);
    }

    /// An agent's branch is in the shared refs, but its commit is only in the
    /// workspace's private store. Plain git in the main repository cannot
    /// fetch at all then; with the store as an alternate it can.
    #[tokio::test]
    async fn a_branch_whose_commit_is_in_a_private_store_does_not_stop_the_fetch() {
        let git = |dir: &Path, args: &[&str], env: &[(&str, &Path)]| {
            let mut cmd = std::process::Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t");
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"], &[]);
        git(
            upstream.path(),
            &["commit", "-q", "--allow-empty", "-m", "one"],
            &[],
        );
        let clone = tempfile::tempdir().unwrap();
        git(
            clone.path(),
            &["clone", "-q", &path_arg(upstream.path()), "."],
            &[],
        );
        // What the sandbox does: a commit into the private store, and the
        // branch written to the shared refs.
        let store = tempfile::tempdir().unwrap();
        let main_objects = clone.path().join(".git/objects");
        let routed = [
            ("GIT_OBJECT_DIRECTORY", store.path()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", main_objects.as_path()),
        ];
        let tree = git(clone.path(), &["write-tree"], &routed);
        let commit = git(
            clone.path(),
            &["commit-tree", &tree, "-p", "HEAD", "-m", "agent"],
            &routed,
        );
        git(
            clone.path(),
            &["update-ref", "refs/heads/bs/agent/work", &commit],
            &routed,
        );
        git(
            upstream.path(),
            &["commit", "-q", "--allow-empty", "-m", "two"],
            &[],
        );

        let hooks = tempfile::tempdir().unwrap();
        let plain = fetch_repo(clone.path(), hooks.path(), &[]).await;
        assert!(
            plain.is_err(),
            "plain git is expected to refuse; if it no longer does, the alternate is moot"
        );
        let r = fetch_repo(clone.path(), hooks.path(), &[store.path().to_path_buf()])
            .await
            .unwrap();
        assert_eq!(r.updated, 1, "main moved");
    }
}
