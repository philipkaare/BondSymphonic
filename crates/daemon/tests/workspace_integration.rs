mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{commit_all, create_ws, start_daemon, Client};

#[tokio::test]
async fn create_list_get_status_destroy_roundtrip_with_events() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let info: RepoInfo = serde_json::from_value(
        c.call(Request::RepoInspect(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(info.default_branch, "main");

    let id = c
        .send(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await;
    let mut events = Vec::new();
    let ws: WorkspaceInfo =
        serde_json::from_value(c.recv_response(id, &mut events).await.unwrap()).unwrap();
    assert_eq!(ws.branch, "bs/agent-1/work");
    assert_eq!(ws.state, WorkspaceState::Ready);
    // A repo with no `bondsymphonic.toml` starts on the default allowlist, so
    // the agent can reach the Anthropic API and the package registries and
    // nothing else.
    assert_eq!(
        ws.allowlist,
        bondsymphonic_daemon::net::allowlist::DEFAULT_ALLOW
            .map(String::from)
            .to_vec()
    );
    assert!(std::path::Path::new(&ws.worktree_path)
        .join("README.md")
        .exists());
    let states: Vec<String> = events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } => Some(format!("{:?}", info.state)),
            _ => None,
        })
        .collect();
    assert_eq!(states, vec!["Creating", "Ready"]);

    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(list.workspaces.len(), 1);
    let got: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(got.name, "agent-1");

    // Duplicate name in the same repo is a Conflict.
    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // Status sees an untracked file, and destroy without force refuses while dirty.
    std::fs::write(std::path::Path::new(&ws.worktree_path).join("wip.txt"), "x").unwrap();
    let st: WorkspaceStatusResult = serde_json::from_value(
        c.call(Request::WorkspaceStatus(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(st
        .entries
        .iter()
        .any(|e| e.path == "wip.txt" && e.status == FileStatus::Untracked));
    let err = c
        .call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(!std::path::Path::new(&ws.worktree_path).exists());
    assert!(daemon.registry.list().is_empty());
    let err = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

#[tokio::test]
async fn restore_marks_missing_worktree_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, _daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "a".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    cancel.cancel();
    std::fs::remove_dir_all(&ws.worktree_path).unwrap();

    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(&data),
        backend_for("noop"),
        server.event_bus(),
    )
    .unwrap();
    daemon.restore().await;
    let w = daemon.registry.get(&ws.id).unwrap();
    assert!(
        matches!(w.state, WorkspaceState::Error(_)),
        "got {:?}",
        w.state
    );
}

fn state_name(s: &WorkspaceState) -> &'static str {
    match s {
        WorkspaceState::Creating => "Creating",
        WorkspaceState::Ready => "Ready",
        WorkspaceState::SandboxDown => "SandboxDown",
        WorkspaceState::Error(_) => "Error",
        WorkspaceState::Destroying => "Destroying",
    }
}

fn state_names(events: &[(Option<WorkspaceId>, Event)]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } => Some(state_name(&info.state)),
            _ => None,
        })
        .collect()
}

/// A worktree directory that has gone missing (the state `restore` records as
/// `Error("worktree directory is missing")`) must not turn `destroy` into a silent
/// `git branch -D` of commits that live only in the workspace's private object dir.
#[tokio::test]
async fn destroy_refuses_unmerged_commits_when_the_worktree_directory_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;

    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    std::fs::write(w.worktree_path.join("work.txt"), "x\n").unwrap();
    commit_all(&w.worktree_path, &layout.sandbox_git_env(), "work");

    cancel.cancel();
    std::fs::remove_dir_all(&w.worktree_path).unwrap();
    daemon.restore().await;
    assert!(
        matches!(
            daemon.registry.get(&ws.id).unwrap().state,
            WorkspaceState::Error(_)
        ),
        "restore should mark the missing worktree as Error"
    );

    let err = lifecycle::destroy(&daemon, &ws.id, false)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    let data = err.data.expect("conflict carries dirty/unmerged");
    assert_eq!(data["unmerged"], serde_json::json!(true));
    assert_eq!(data["dirty"], serde_json::json!(false));
    // The commit is still there: nothing was deleted by the refused destroy.
    assert!(daemon.registry.get(&ws.id).is_some());

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    assert!(daemon.registry.get(&ws.id).is_none());
}

/// A `destroy` that fails after the sandbox is gone must leave the workspace in
/// `Error`, not stranded in `Destroying` forever. The branch delete is made to fail
/// by planting the ref lock file git takes before rewriting the ref.
#[tokio::test]
async fn a_failed_destroy_leaves_the_workspace_in_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;

    let w = daemon.registry.get(&ws.id).unwrap();
    let layout = lifecycle::layout_for(&daemon, &w).await.unwrap();
    let lock = layout.ref_dir().join("work.lock");
    std::fs::write(&lock, "").unwrap();

    let err = lifecycle::destroy(&daemon, &ws.id, true).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError, "got {err:?}");
    let left = daemon
        .registry
        .get(&ws.id)
        .expect("entry is kept on failure");
    match &left.state {
        WorkspaceState::Error(m) => assert!(m.contains("destroy failed"), "got {m}"),
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_file(&lock);
    cancel.cancel();
}

/// A create that fails after the registry entry exists must tell the client the
/// workspace went away: `Error` with the reason, then the same terminal `Destroying`
/// event a real destroy emits.
#[tokio::test]
async fn a_failed_create_emits_error_then_destroying() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let id = c
        .send(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "no-such-branch".into(),
            name: "a".into(),
        }))
        .await;
    let mut events = Vec::new();
    let err = c.recv_response(id, &mut events).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert_eq!(state_names(&events), ["Creating", "Error", "Destroying"]);

    let list: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(daemon.registry.list().is_empty());
    cancel.cancel();
}

// ---------------------------------------------------------------------------
// C1 regression: the worktree is bind-mounted read-write into the sandbox, so
// every file git uses to discover a repository from the working directory is
// agent-controlled. A daemon-side `git status` that lets git discover the
// repository will read the agent's config and execute `core.fsmonitor` on the
// host, outside the sandbox, as the daemon's user.
// ---------------------------------------------------------------------------

/// Runs a git command, asserting it succeeded.
fn git_at(cwd: &std::path::Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

/// Sets `core.fsmonitor` in `config_file` to a command that creates `marker`.
///
/// git runs the fsmonitor hook through a shell, so one string works on both
/// platforms as long as the path has no backslashes in it.
fn plant_fsmonitor(config_file: &std::path::Path, marker: &std::path::Path) {
    let cmd = format!(
        "printf pwned > '{}'",
        marker.display().to_string().replace('\\', "/")
    );
    let st = std::process::Command::new("git")
        .args([
            "config",
            "--file",
            &config_file.to_string_lossy(),
            "core.fsmonitor",
            &cmd,
        ])
        .status()
        .unwrap();
    assert!(st.success(), "planting core.fsmonitor");
}

/// Runs `git status` in the worktree the way the daemon used to — letting git
/// discover the repository from the working directory — and reports whether the
/// planted hook ran. Leaves no marker behind.
fn attack_fires(worktree: &std::path::Path, marker: &std::path::Path) -> bool {
    let _ = std::fs::remove_file(marker);
    let _ = std::process::Command::new("git")
        .args(["status", "--porcelain=v2", "--untracked-files=all"])
        .current_dir(worktree)
        .output();
    let fired = marker.exists();
    let _ = std::fs::remove_file(marker);
    fired
}

/// `status` must still answer, or fail cleanly as a `GitError`; what it must
/// never do is run the agent's command.
fn assert_status_is_sane(r: Result<WorkspaceStatusResult, RpcError>) {
    if let Err(e) = r {
        assert_eq!(e.code, ErrorCode::GitError, "unexpected error: {e:?}");
    }
}

/// Variant A: the agent replaces `<worktree>/.git` with a pointer to a gitdir
/// of its own.
#[tokio::test]
async fn status_ignores_a_gitdir_redirect_planted_in_the_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;
    let wt = std::path::PathBuf::from(&ws.worktree_path);

    let evil_work = wt.join("evilrepo");
    std::fs::create_dir_all(&evil_work).unwrap();
    git_at(&evil_work, &["init", "-q"]);
    let evil_gitdir = evil_work.join(".git");
    let marker = dir.path().join("pwned-gitdir.txt");
    plant_fsmonitor(&evil_gitdir.join("config"), &marker);
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", evil_gitdir.display()),
    )
    .unwrap();

    assert!(
        attack_fires(&wt, &marker),
        "the planted hook must run for a git that discovers the repo, or this test proves nothing"
    );
    assert_status_is_sane(lifecycle::status(&daemon, &ws.id).await);
    assert!(
        !marker.exists(),
        "workspace.status executed agent-controlled git config"
    );
    cancel.cancel();
}

/// Variant B: the agent repoints `<git_common>/worktrees/<id>/commondir`, which
/// is where git then reads `config` from.
#[tokio::test]
async fn status_ignores_a_commondir_redirect_in_the_worktree_gitdir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;
    let wt = std::path::PathBuf::from(&ws.worktree_path);
    let layout = lifecycle::layout_for(&daemon, &daemon.registry.get(&ws.id).unwrap())
        .await
        .unwrap();

    let evil_work = wt.join("evilcommon");
    std::fs::create_dir_all(&evil_work).unwrap();
    git_at(&evil_work, &["init", "-q"]);
    let evil_common = evil_work.join(".git");
    let marker = dir.path().join("pwned-commondir.txt");
    plant_fsmonitor(&evil_common.join("config"), &marker);
    std::fs::write(
        layout.worktree_gitdir().join("commondir"),
        format!("{}\n", evil_common.display()),
    )
    .unwrap();

    assert!(
        attack_fires(&wt, &marker),
        "the planted hook must run for a git that discovers the repo, or this test proves nothing"
    );
    assert_status_is_sane(lifecycle::status(&daemon, &ws.id).await);
    assert!(
        !marker.exists(),
        "workspace.status executed agent-controlled git config"
    );
    cancel.cancel();
}

/// Variant C: no redirect at all. On a repository that has
/// `extensions.worktreeConfig` enabled, git reads `config.worktree` straight
/// out of the per-worktree gitdir, which the agent can write. Pinning the
/// repository does not close this one, and `-c extensions.worktreeConfig=false`
/// does not either — git takes that extension from the repository format before
/// command-line config exists.
#[tokio::test]
async fn status_ignores_a_config_worktree_planted_in_the_worktree_gitdir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    git_at(&repo, &["config", "extensions.worktreeConfig", "true"]);
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;
    let wt = std::path::PathBuf::from(&ws.worktree_path);
    let layout = lifecycle::layout_for(&daemon, &daemon.registry.get(&ws.id).unwrap())
        .await
        .unwrap();

    let marker = dir.path().join("pwned-worktree-config.txt");
    plant_fsmonitor(&layout.worktree_gitdir().join("config.worktree"), &marker);

    assert!(
        attack_fires(&wt, &marker),
        "the planted hook must run for a git that reads config.worktree, or this test proves nothing"
    );
    assert_status_is_sane(lifecycle::status(&daemon, &ws.id).await);
    assert!(
        !marker.exists(),
        "workspace.status executed agent-controlled git config"
    );
    cancel.cancel();
}

/// I5: deleting or moving the source repository must not strand a workspace in
/// the registry forever. `layout_for` asks the repository where its git
/// directory is, so it fails first, before `destroy` has done anything — and it
/// used to fail even with `force`, leaving an entry only a hand edit of
/// `workspaces.json` could remove.
#[tokio::test]
async fn destroy_with_force_succeeds_after_the_repository_has_moved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;
    // The sandbox holds no handle on the repo directory, but the noop backend's
    // children might, so the workspace is torn down to nothing first.
    cancel.cancel();

    std::fs::rename(&repo, dir.path().join("moved-repo")).unwrap();

    let err = lifecycle::destroy(&daemon, &ws.id, false)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError, "got {err:?}");
    assert!(
        daemon.registry.get(&ws.id).is_some(),
        "a refused destroy keeps the entry"
    );

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    assert!(daemon.registry.get(&ws.id).is_none());
    assert!(daemon.registry.list().is_empty());
    assert!(
        !std::path::Path::new(&ws.worktree_path).exists(),
        "the worktree directory is removed even without git"
    );
}

/// Git has no "no hooks" setting. An empty `core.hooksPath` does not disable
/// hooks, it resolves them relative to the filesystem root — `/pre-commit` —
/// so daemon-side worktree commands point it at an empty directory the daemon
/// owns instead.
#[tokio::test]
async fn daemon_side_worktree_git_resolves_hooks_into_an_empty_daemon_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "a").await;
    let layout = lifecycle::layout_for(&daemon, &daemon.registry.get(&ws.id).unwrap())
        .await
        .unwrap();

    let out = layout
        .worktree_git()
        .run(
            std::path::Path::new(&ws.worktree_path),
            &["rev-parse", "--git-path", "hooks/pre-commit"],
        )
        .await
        .unwrap();
    let path = out.stdout.trim();
    assert_ne!(path, "/pre-commit", "hooks were relocated to the root");
    assert!(
        path.ends_with("nohooks/pre-commit"),
        "hooks must resolve into the daemon's empty directory, got {path}"
    );

    let no_hooks = daemon.dirs.no_hooks();
    assert!(no_hooks.is_dir(), "the daemon must own the directory");
    assert_eq!(
        std::fs::read_dir(&no_hooks).unwrap().count(),
        0,
        "the hooks directory must stay empty"
    );
    cancel.cancel();
}

/// The repo's `bondsymphonic.toml` decides what its workspaces may reach, and
/// it is read on the create path, where a bad file is most likely to be found:
/// the user is opening the repo precisely because they want to work in it. So
/// a config that will not parse costs the extra hosts and nothing else — the
/// workspace still comes up, on the defaults.
#[tokio::test]
async fn the_repo_config_extends_the_allowlist_and_a_broken_one_does_not_block_create() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let defaults = bondsymphonic_daemon::net::allowlist::DEFAULT_ALLOW.len();

    std::fs::write(
        repo.join("bondsymphonic.toml"),
        "[network]\nallow = [\"*.mycompany.com\", \"github.com\", \"not a host\"]\n",
    )
    .unwrap();
    let ws = create_ws(&mut c, &repo, "extended").await;
    // The repo's own host is appended; the one it repeats from the defaults is
    // not doubled, and the entry that is not a host at all is dropped.
    assert_eq!(ws.allowlist.len(), defaults + 1);
    assert_eq!(ws.allowlist.last().unwrap(), "*.mycompany.com");

    std::fs::write(repo.join("bondsymphonic.toml"), "[network\nallow = ").unwrap();
    let ws = create_ws(&mut c, &repo, "broken").await;
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert_eq!(ws.allowlist.len(), defaults);

    // The same config, over the wire: `repo.detect_run_configs` answers for a
    // path the client picked, before any workspace exists for it, and a config
    // file it cannot read still leaves detection to answer.
    std::fs::write(repo.join("manage.py"), "#!/usr/bin/env python\n").unwrap();
    let found: DetectRunConfigsResult = serde_json::from_value(
        c.call(Request::RepoDetectRunConfigs(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(found.configs.len(), 1);
    assert_eq!(found.configs[0].name, "django");
    assert_eq!(found.configs[0].port, 8000);
    assert_eq!(found.configs[0].source, RunConfigSource::Detected);
    cancel.cancel();
}
