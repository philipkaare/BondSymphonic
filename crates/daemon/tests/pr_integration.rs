//! `workspace.create_pr` end to end: a real `git push -u` to a *local* bare
//! origin, then `gh pr create` against a stand-in.
//!
//! The real `gh` is never run. `BS_GH_BIN` points at `tests/fixtures/gh_stub.py`
//! for every case here, and the origin is a bare repository in the test's own
//! temporary directory, so nothing in this file can reach github.com or the
//! developer's credentials.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::workspace::lifecycle;
use bondsymphonic_proto::*;
use common::{commit_all, create_ws, init_repo_with_origin, start_daemon, Client};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Serialises the tests: `BS_GH_BIN`, `GH_STUB_LOG` and `GH_STUB_FAIL` are all
/// process-wide, and the daemon under test runs in this same process.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The interpreter to run the stub with, or `None` when this host has none.
fn python() -> Option<&'static str> {
    // Windows ships a `python3` App Execution Alias that is not an interpreter,
    // so the real name is tried first there.
    let candidates: [&str; 2] = if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    };
    candidates.into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A path as `BS_GH_BIN` can carry it. `shell_words` reads backslashes as
/// escapes, so a Windows path goes in with forward slashes, which every Windows
/// API accepts.
fn arg_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Points the daemon at the stub and returns the file it will log its argv to.
fn use_gh_stub(py: &str, log: &Path, fail: bool) {
    std::env::set_var(
        "BS_GH_BIN",
        format!(
            "\"{py}\" \"{}\"",
            arg_path(&fixture_dir().join("gh_stub.py"))
        ),
    );
    std::env::set_var("GH_STUB_LOG", log);
    if fail {
        std::env::set_var("GH_STUB_FAIL", "1");
    } else {
        std::env::remove_var("GH_STUB_FAIL");
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn commit_in_ws(d: &Arc<Daemon>, ws: &WorkspaceInfo, name: &str, msg: &str) {
    let w = d.workspace(&ws.id).unwrap();
    let env = lifecycle::layout_for(d, &w)
        .await
        .unwrap()
        .sandbox_git_env();
    let wt = Path::new(&ws.worktree_path);
    std::fs::write(wt.join(name), "x\n").unwrap();
    commit_all(wt, &env, msg);
}

#[tokio::test]
async fn create_pr_pushes_the_branch_and_returns_the_url_gh_printed() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, "alpha.txt", "alpha work").await;

    let res: CreatePrResult = serde_json::from_value(
        c.call(Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
            workspace_id: ws.id.clone(),
            title: "T".into(),
            body: "B".into(),
            draft: true,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(res.url, "https://github.com/example/repo/pull/42");

    // A real push: the origin has the branch, and it has the workspace's
    // commit, whose objects lived only in the private object directory.
    assert_eq!(
        git_out(
            &origin,
            &["log", "-1", "--format=%s", "refs/heads/bs/alpha/work"]
        ),
        "alpha work"
    );
    // `-u`, so the branch is tracking and a later `git push` from the repo needs
    // no arguments.
    assert_eq!(
        git_out(&repo, &["config", "--get", "branch.bs/alpha/work.remote"]),
        "origin"
    );

    let logged = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        logged.trim(),
        "pr create --title T --body B --head bs/alpha/work --base main --draft"
    );

    cancel.cancel();
}

/// The one exception to "a daemon git operation runs none of the repository's
/// hooks": `create_pr` pushes through `Layout::daemon_push_git`, which leaves
/// them in place.
///
/// `pre-push` is not decoration. It is how `git-lfs` uploads the large objects
/// the pushed commits point at, and a push that skips it puts pointer files on
/// the remote with nothing behind them. The hook is asserted to have received
/// the real push payload on stdin -- `<local ref> <local sha> <remote ref>
/// <remote sha>` -- because that payload is exactly what `git-lfs` reads to
/// decide what to upload.
///
/// The other side of the rule, that a daemon *merge* runs no hooks at all, is
/// `a_daemon_merge_runs_none_of_the_repositorys_hooks` in
/// `merge_integration.rs`. Daemon design 5.4 and 5.5.
#[tokio::test]
async fn create_pr_still_runs_the_repositorys_pre_push_hook() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);

    // Records what git fed it, so the assertion is about a real invocation and
    // not merely about a file appearing.
    let payload = dir.path().join("pre-push-stdin");
    common::install_hook(
        &repo,
        "pre-push",
        &format!("cat > \"{}\"\nexit 0", common::sh_path(&payload)),
    );

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, "alpha.txt", "alpha work").await;

    let res: CreatePrResult = serde_json::from_value(
        c.call(Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
            workspace_id: ws.id.clone(),
            title: "T".into(),
            body: "B".into(),
            draft: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(res.url, "https://github.com/example/repo/pull/42");

    let fed = std::fs::read_to_string(&payload)
        .expect("the repository's pre-push hook must have run: git-lfs uploads from it");
    let tip = git_out(&repo, &["rev-parse", "bs/alpha/work"]);
    assert!(
        fed.contains("refs/heads/bs/alpha/work") && fed.contains(&tip),
        "the hook must get the real push payload on stdin, got: {fed:?}"
    );
    // And the push itself still happened.
    assert_eq!(
        git_out(
            &origin,
            &["log", "-1", "--format=%s", "refs/heads/bs/alpha/work"]
        ),
        "alpha work"
    );

    cancel.cancel();
}

#[tokio::test]
async fn create_pr_reports_a_failing_gh_as_a_git_error_carrying_its_stderr() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (repo, _origin) = init_repo_with_origin(dir.path());
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, true);

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, "alpha.txt", "alpha work").await;

    let err = c
        .call(Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
            workspace_id: ws.id.clone(),
            title: "T".into(),
            body: "B".into(),
            draft: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError, "{err:?}");
    let data = err.data.as_ref().expect("GitError carries data");
    assert_eq!(data["exit_code"], 1, "{err:?}");
    assert!(
        data["stderr"].as_str().unwrap().contains("not logged in"),
        "{err:?}"
    );
    assert!(
        data["command"].as_str().unwrap().contains("pr create"),
        "{err:?}"
    );
    // The user's title and body are their own content and have no business in
    // an error message the IDE puts on screen.
    let printed = format!("{err:?}");
    assert!(!printed.contains("--title"), "{printed}");

    // `gh` was reached at all only because the push succeeded first.
    assert!(std::fs::read_to_string(&log).unwrap().contains("pr create"));

    std::env::remove_var("GH_STUB_FAIL");
    cancel.cancel();
}

/// `push -u` leaves `refs/remotes/origin/<branch>` behind, and destroying the
/// workspace does not remove it the way it removes the local branch. The
/// objects it names therefore have to be in the shared store before the
/// workspace's private object directory goes.
#[tokio::test]
async fn the_remote_tracking_ref_still_reads_after_the_workspace_is_destroyed() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (repo, _origin) = init_repo_with_origin(dir.path());
    use_gh_stub(py, &dir.path().join("gh.log"), false);

    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, "alpha.txt", "alpha work").await;

    c.call(Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
        workspace_id: ws.id.clone(),
        title: "T".into(),
        body: "B".into(),
        draft: false,
    }))
    .await
    .unwrap();

    // Nothing merged it, so the branch is ahead of the base and only `force`
    // gets rid of it — which is exactly the "PR opened, workspace closed" flow.
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();

    assert_eq!(
        git_out(
            &repo,
            &[
                "log",
                "-1",
                "--format=%s",
                "refs/remotes/origin/bs/alpha/work"
            ]
        ),
        "alpha work"
    );

    cancel.cancel();
}

/// The same gate `workspace.merge` applies. A workspace whose sandbox is down
/// may have a half-formed branch, and publishing it is not something to do on
/// the user's behalf. Refused before anything is pushed, so `gh` is never
/// reached either.
#[tokio::test]
async fn create_pr_refuses_a_workspace_that_is_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    commit_in_ws(&daemon, &ws, "alpha.txt", "alpha work").await;
    daemon
        .set_state(&ws.id, WorkspaceState::SandboxDown)
        .unwrap();

    let err = c
        .call(Request::WorkspaceCreatePr(WorkspaceCreatePrParams {
            workspace_id: ws.id.clone(),
            title: "T".into(),
            body: "B".into(),
            draft: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams, "{err:?}");
    assert!(err.message.contains("is not ready"), "{err:?}");

    // Nothing was pushed: the origin never heard of the branch.
    let refs = git_out(&origin, &["for-each-ref", "--format=%(refname)"]);
    assert!(!refs.contains("bs/alpha/work"), "{refs}");

    cancel.cancel();
}
