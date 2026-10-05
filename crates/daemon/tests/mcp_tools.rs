//! The agents' `bondsymphonic` MCP tools, called directly through
//! [`ToolHost`] against a real daemon and workspace.
//!
//! The real `gh` is never run: `BS_GH_BIN` points at `tests/fixtures/gh_stub.py`,
//! and every push goes to a bare repository in the test's own temporary
//! directory, so nothing here can reach github.com or the developer's
//! credentials.

mod common;

use bondsymphonic_daemon::mcp::protocol::{ToolHost, ToolOutcome};
use bondsymphonic_daemon::mcp::tools::{WorkspaceTools, READ_ONLY_TOOLS};
use bondsymphonic_proto::*;
use common::{create_ws, init_repo_with_origin, start_daemon, Client};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Serialises the tests that touch `BS_GH_BIN`, `GH_STUB_*`: all process-wide,
/// and the daemon under test runs in this same process.
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

/// Points the daemon at the stub and names the file it will log its argv to.
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

fn text(o: ToolOutcome) -> Result<String, String> {
    match o {
        ToolOutcome::Ok(t) => Ok(t),
        ToolOutcome::Failed(t) => Err(format!("failed: {t}")),
        ToolOutcome::InvalidArguments(t) => Err(format!("invalid: {t}")),
    }
}

/// A repository whose origin *reads* as GitHub (so `--repo` resolves) but
/// pushes to the local bare repository, so nothing leaves the machine.
fn github_looking_origin(repo: &Path, bare: &Path) {
    common::git_out(
        repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    common::git_out(
        repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            &bare.display().to_string(),
        ],
    );
}

async fn setup(
    name: &str,
) -> (
    tempfile::TempDir,
    Arc<bondsymphonic_daemon::daemon::Daemon>,
    WorkspaceInfo,
    PathBuf,
    tokio_util::sync::CancellationToken,
) {
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, name).await;
    github_looking_origin(&repo, &origin);
    (dir, daemon, ws, origin, cancel)
}

#[tokio::test]
async fn the_tool_list_marks_the_read_only_ones() {
    let (_dir, d, ws, _o, cancel) = setup("list").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone()).tools();
    let names: Vec<_> = tools.iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [
            "git_fetch",
            "git_push",
            "pr_create",
            "pr_view",
            "pr_update",
            "pr_comment",
            "ci_logs",
            "issue_view"
        ]
    );
    for t in &tools {
        assert_eq!(t.read_only, READ_ONLY_TOOLS.contains(&t.name), "{}", t.name);
        assert_eq!(t.input_schema["type"], "object");
    }
    cancel.cancel();
}

#[tokio::test]
async fn pr_create_pushes_and_names_the_repo_explicitly() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, origin, cancel) = setup("prc").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(
        tools
            .call("pr_create", json!({"title":"T","body":"B"}))
            .await,
    )
    .unwrap();
    assert_eq!(out.trim(), "https://github.com/example/repo/pull/42");
    assert!(common::git_try(&origin, &["rev-parse", "refs/heads/bs/prc/work"]).is_ok());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().trim(),
        "pr create --repo example/repo --title T --body B --head bs/prc/work --base main"
    );
    cancel.cancel();
}

#[tokio::test]
async fn a_title_that_looks_like_a_flag_stays_a_value() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("flag").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    text(
        tools
            .call("pr_create", json!({"title":"--repo evil/x","body":"B"}))
            .await,
    )
    .unwrap();
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains("--repo example/repo --title --repo evil/x --body"),
        "{logged}"
    );
    cancel.cancel();
}

#[tokio::test]
async fn an_existing_pr_is_reported_not_failed() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("exists").await;
    use_gh_stub(py, &dir.path().join("gh.log"), false);
    std::env::set_var("GH_STUB_PR_EXISTS", "1");
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(
        tools
            .call("pr_create", json!({"title":"T","body":"B"}))
            .await,
    );
    std::env::remove_var("GH_STUB_PR_EXISTS");
    assert!(out
        .unwrap()
        .contains("already exists: https://github.com/example/repo/pull/7"));
    cancel.cancel();
}

#[tokio::test]
async fn read_tools_call_gh_with_the_branch_and_repo() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("read").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    text(tools.call("pr_view", json!({})).await).unwrap();
    text(tools.call("issue_view", json!({"number": 5})).await).unwrap();
    text(
        tools
            .call("pr_comment", json!({"body":"hello", "number": "42"}))
            .await,
    )
    .unwrap();
    text(tools.call("pr_update", json!({"ready": true})).await).unwrap();
    let logged = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = logged.lines().collect();
    assert!(
        lines[0].starts_with("pr view bs/read/work --repo example/repo --json number,url,state"),
        "{logged}"
    );
    assert!(
        lines[1].starts_with("issue view 5 --repo example/repo --json"),
        "{logged}"
    );
    assert_eq!(lines[2], "pr comment 42 --repo example/repo --body hello");
    assert_eq!(lines[3], "pr ready bs/read/work --repo example/repo");
    cancel.cancel();
}

#[tokio::test]
async fn ci_logs_keeps_the_tail_of_a_long_log() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("ci").await;
    use_gh_stub(py, &dir.path().join("gh.log"), false);
    // ~110 KB: well past TEXT_CAP, but under Linux's 128 KiB limit on one
    // environment string, which every process the daemon spawns inherits.
    let long = format!("{}THE END\n", "noise line\n".repeat(10_000));
    std::env::set_var("GH_STUB_LOG_TEXT", &long);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(tools.call("ci_logs", json!({})).await);
    std::env::remove_var("GH_STUB_LOG_TEXT");
    let out = out.unwrap();
    assert!(
        out.len() <= bondsymphonic_daemon::mcp::tools::TEXT_CAP + 4096,
        "{}",
        out.len()
    );
    assert!(out.trim_end().ends_with("THE END"));
    assert!(out.contains("earlier output truncated"));
    cancel.cancel();
}

#[tokio::test]
async fn github_tools_refuse_a_non_github_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _origin) = init_repo_with_origin(dir.path()); // origin is a local path
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "local").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let err = text(tools.call("pr_view", json!({})).await).unwrap_err();
    assert!(err.contains("not a GitHub repository"), "{err}");
    // Fetch and push still work: they only need git.
    text(tools.call("git_fetch", json!({})).await).unwrap();
    text(tools.call("git_push", json!({})).await).unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn bad_arguments_are_invalid_not_failed() {
    let (_dir, d, ws, _o, cancel) = setup("args").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    for (tool, args) in [
        ("pr_create", json!({"body":"no title"})),
        ("pr_create", json!({"title":"", "body":"x"})),
        ("issue_view", json!({"number": -1})),
        ("issue_view", json!({"number": "abc"})),
        ("pr_update", json!({})),
        ("pr_comment", json!({"body": ""})),
        ("no_such_tool", json!({})),
    ] {
        assert!(
            matches!(
                tools.call(tool, args.clone()).await,
                ToolOutcome::InvalidArguments(_)
            ),
            "{tool} {args}"
        );
    }
    cancel.cancel();
}

#[tokio::test]
async fn a_tool_on_a_destroyed_workspace_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "gone").await;
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    assert!(matches!(
        tools.call("git_fetch", json!({})).await,
        ToolOutcome::Failed(_)
    ));
    cancel.cancel();
}

#[tokio::test]
async fn push_is_refused_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: String::new(),
            name: "inplace".into(),
            init_if_missing: false,
            in_place: true,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let err = text(tools.call("git_push", json!({})).await).unwrap_err();
    assert!(err.contains("in-place"), "{err}");
    cancel.cancel();
}
