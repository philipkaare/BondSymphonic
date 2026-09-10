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
            init_if_missing: false,
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
            init_if_missing: false,
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
            init_if_missing: false,
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
            init_if_missing: false,
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

/// A `[[run]]` block that is missing its `port` costs the repo that one entry
/// and nothing else.
///
/// The whole file used to be discarded: `port` was a required field, so one
/// forgotten line turned a repo with three configured runs into a repo the Run
/// panel offered detection for. The entries that do parse are kept, and the one
/// that does not comes back as a line in `warnings` so the IDE can say which.
#[tokio::test]
async fn a_run_entry_without_a_port_is_reported_and_the_others_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    std::fs::write(
        repo.join("bondsymphonic.toml"),
        "[[run]]\nname = \"web\"\ncommand = \"npm run dev\"\nport = 3000\n\n\
         [[run]]\nname = \"api\"\ncommand = \"cargo run -p api\"\n\n\
         [[run]]\nname = \"docs\"\ncommand = \"mkdocs serve\"\nport = 8000\n",
    )
    .unwrap();
    // Detection would find this, and must not be reached: the file still
    // declares runs that parse.
    std::fs::write(repo.join("manage.py"), "#!/usr/bin/env python\n").unwrap();

    let found: DetectRunConfigsResult = serde_json::from_value(
        c.call(Request::RepoDetectRunConfigs(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let names: Vec<&str> = found.configs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["web", "docs"], "{:?}", found.configs);
    assert_eq!(found.configs[0].source, RunConfigSource::ConfigFile);
    assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    let w = &found.warnings[0];
    assert!(w.contains("api"), "the warning must name the entry: {w}");
    assert!(w.contains("port"), "and say what is missing: {w}");

    // A file whose runs all parse warns about nothing.
    std::fs::write(
        repo.join("bondsymphonic.toml"),
        "[[run]]\nname = \"web\"\ncommand = \"npm run dev\"\nport = 3000\n",
    )
    .unwrap();
    let found: DetectRunConfigsResult = serde_json::from_value(
        c.call(Request::RepoDetectRunConfigs(RepoPathParams {
            path: repo.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);

    cancel.cancel();
}

/// The chain daemon design §13 asks `workspace_integration` to walk, end to
/// end and in one test: a temp repo, a workspace, a commit made the way the
/// sandbox makes it, `workspace.changes`, `workspace.merge`, the base branch
/// holding the work, the objects readable *without* the workspace's private
/// object directory, and a destroy that takes every per-workspace directory and
/// every agent record and transcript with it.
///
/// The last two steps are the ones worth having. A merge that leaves the
/// commits behind in `objects/<ws>/` looks like a success and breaks the user's
/// own `git log main` the moment the workspace is destroyed, and a destroy that
/// leaves a home or a cache behind grows the data directory forever.
///
/// The commit is made by a host `git` carrying the environment the sandbox
/// gives git, not by a `git` running *inside* a sandbox. What it has to prove is
/// where the objects land, and that is decided by `GIT_OBJECT_DIRECTORY` and
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` alone; running it this way is what lets
/// the chain be walked on Windows and under the noop backend as well as under
/// bwrap. The sandbox's own git is exercised by
/// `sandbox_integration::bwrap_workspace_protects_main_branch_and_shared_objects`,
/// which is where the ref and object protections belong.
#[tokio::test]
async fn the_full_workspace_chain_from_create_to_a_destroy_that_leaves_nothing() {
    use bondsymphonic_daemon::agents::persist::{AgentRecord, AgentRecords};

    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;

    let ws = create_ws(&mut c, &repo, "chain").await;
    let wt = std::path::Path::new(&ws.worktree_path).to_path_buf();
    let layout = lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap())
        .await
        .unwrap();

    // The commit is made with the environment the sandbox gives git, so its
    // objects land in the workspace's private directory and nowhere else.
    std::fs::write(wt.join("feature.txt"), "from the sandbox\n").unwrap();
    commit_all(&wt, &layout.sandbox_git_env(), "sandboxed work");
    let tip = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&wt)
            .envs(layout.sandbox_git_env())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let (shard, rest) = tip.split_at(2);
    assert!(
        layout.objects_dir.join(shard).join(rest).is_file(),
        "the commit must start out in the workspace's private object store"
    );

    // `workspace.changes` sees it.
    let changed: ChangesResult = serde_json::from_value(
        c.call(Request::WorkspaceChanges(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(
        changed
            .files
            .iter()
            .any(|f| f.path == "feature.txt" && f.status == FileStatus::Added),
        "{:?}",
        changed.files
    );

    // Records and a transcript, as an agent that ran in this workspace would
    // have left them. Written through the same file the daemon uses, so the
    // destroy path finds them exactly as it finds a real agent's.
    let records = AgentRecords::new(daemon.dirs.agents_file());
    records.upsert(AgentRecord {
        agent_id: "ag_chain".into(),
        workspace_id: ws.id.clone(),
        adapter: AgentAdapterKind::Claude,
        session_id: Some("sess-chain".into()),
        options: AgentStartOptions {
            command: None,
            resume_session: None,
            model: None,
            permission_mode: None,
            api_key: None,
        },
        started_at: "2026-09-10T10:00:00Z".into(),
        ended_at: None,
    });
    let transcript = daemon.dirs.transcripts.join("ag_chain.ndjson");
    std::fs::write(&transcript, "{\"kind\":\"text\"}\n").unwrap();

    // Merge, and the base branch has the work.
    let res: MergeResult = serde_json::from_value(
        c.call(Request::WorkspaceMerge(WorkspaceMergeParams {
            workspace_id: ws.id.clone(),
            mode: MergeMode::Merge,
            message: None,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(res.ok, "{res:?}");

    // Read the way the *user's* git reads it: no alternate object directory in
    // the environment at all. This is what proves the merge copied the objects
    // out of the workspace rather than leaving the base branch pointing into it.
    let plain = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(plain(&["show", "main:feature.txt"]), "from the sandbox");
    assert!(plain(&["rev-list", "--objects", "main"]).contains(&tip));

    // Destroy, and nothing of this workspace is left anywhere. Without `force`,
    // which is the path a user actually takes: the work is merged and the
    // worktree is clean, so the uncommitted-or-unmerged guard has to let this
    // through rather than having to be overridden.
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: false,
    }))
    .await
    .unwrap();

    for (what, path) in [
        ("worktree", daemon.dirs.worktree(&ws.id)),
        ("objects", daemon.dirs.objects(&ws.id)),
        ("home", daemon.dirs.home(&ws.id)),
        ("cache", daemon.dirs.cache(&ws.id)),
        ("run dir", daemon.dirs.run(&ws.id)),
    ] {
        assert!(
            !path.exists(),
            "{what} survived the destroy: {}",
            path.display()
        );
    }
    assert!(
        records.load().is_empty(),
        "the agent record survived the destroy"
    );
    assert!(
        !transcript.exists(),
        "the transcript survived the destroy: {}",
        transcript.display()
    );
    assert!(daemon.registry.list().is_empty());

    // And the base branch still reads without the workspace behind it.
    assert_eq!(plain(&["show", "main:feature.txt"]), "from the sandbox");

    cancel.cancel();
}

/// The whole point of `init_if_missing`, end to end: the user picks a folder
/// that has never been a repository, and gets a workspace they can work in.
///
/// `repo.inspect` is asked first, the way the New Agent dialog asks it, because
/// the dialog's offer to initialise and the daemon's willingness to do it have
/// to agree about the same path.
#[tokio::test]
async fn create_initialises_a_folder_that_is_not_a_repository_yet() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("brand-new-project");
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let info: RepoInfo = serde_json::from_value(
        c.call(Request::RepoInspect(RepoPathParams {
            path: fresh.to_string_lossy().into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(!info.is_repo);
    assert!(!info.exists);

    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: fresh.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: true,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    assert_eq!(ws.state, WorkspaceState::Ready);
    assert_eq!(ws.branch, "bs/alpha/work");
    assert!(
        fresh.join(".git").exists(),
        "the folder is a repository now"
    );
    // The worktree is a checkout of the branch the initial commit is on, which
    // is what makes the workspace usable at all: a repository with no commits
    // has nothing for `git worktree add` to branch from.
    let head = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", "HEAD"])
        .current_dir(&ws.worktree_path)
        .output()
        .unwrap();
    assert!(
        head.status.success(),
        "{}",
        String::from_utf8_lossy(&head.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "Initial commit"
    );

    cancel.cancel();
}

/// Without the flag the answer is the one it always was. A client that does not
/// know about `init_if_missing` is a client whose user was never shown that a
/// folder is about to become a repository.
#[tokio::test]
async fn create_still_refuses_a_folder_that_is_not_a_repository_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("just-a-folder");
    std::fs::create_dir_all(&plain).unwrap();
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: plain.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::GitError);
    assert!(
        err.message.contains("not a git repository"),
        "{}",
        err.message
    );
    assert!(!plain.join(".git").exists(), "nothing was initialised");

    cancel.cancel();
}

/// The sandbox home is what the agent reads its Claude Code configuration from,
/// and the worktree has to be a trusted project in it or the repository's own
/// `.claude/settings.json` is ignored with a line in the daemon log and no way
/// for anyone to accept the dialog that would fix it.
#[tokio::test]
async fn a_new_workspace_home_trusts_its_own_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let ws = create_ws(&mut c, &repo, "trusted").await;

    let claude_json = daemon.dirs.home(&ws.id).join(".claude.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    // The file is a merge of the daemon user's own and is not printed on
    // failure: it is their account state, and the part this test is about is
    // which projects it trusts.
    let trusted: Vec<&String> = v["projects"]
        .as_object()
        .map(|p| p.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        v["projects"][&ws.worktree_path]["hasTrustDialogAccepted"], true,
        "trusted: {trusted:?}"
    );

    cancel.cancel();
}

/// The failure this pass was written after was a `repo.inspect` that timed out
/// on a large repository. A create that reads *any* git failure as "not a
/// repository yet" would answer that by running `git init` and an empty commit
/// over the user's repository. Only git's own "not a git repository" may lead to
/// a write; a repository git cannot read is a repository.
#[tokio::test]
async fn create_does_not_initialise_when_git_fails_for_another_reason() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken-repo");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join(".git"), "this is not a gitfile\n").unwrap();
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: broken.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: true,
        }))
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::GitError);
    assert!(
        !err.message.contains("not a git repository"),
        "this is a repository git could not read: {}",
        err.message
    );
    // The error is the one from asking *whether* it is a repository. An error
    // from `git init` here would mean the daemon had already decided to write.
    assert!(
        err.data.as_ref().unwrap()["command"]
            .as_str()
            .unwrap()
            .starts_with("git rev-parse"),
        "{:?}",
        err.data
    );
    assert_eq!(
        std::fs::read_to_string(broken.join(".git")).unwrap(),
        "this is not a gitfile\n",
        "nothing may be initialised over it"
    );
    assert!(!broken.join(".git").is_dir());

    cancel.cancel();
}

/// `rev-parse` searches upwards, so a new folder inside somebody's repository
/// looks like a repository unless the question is asked about the folder itself.
/// Adopting the parent would make the workspace a worktree of a repository the
/// user did not pick, with a `bs/<name>/work` branch in it — the opposite of the
/// "this folder will be initialised" the dialog showed.
#[tokio::test]
async fn create_initialises_a_folder_inside_a_repository_rather_than_adopting_it() {
    let dir = tempfile::tempdir().unwrap();
    let outer = common::init_repo(dir.path());
    let inside = outer.join("new-thing");
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: inside.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: true,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(inside.join(".git").is_dir(), "a repository of its own");
    let branches = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        .current_dir(&outer)
        .output()
        .unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(
        !branches.contains("bs/"),
        "the enclosing repository must be untouched: {branches}"
    );

    cancel.cancel();
}

/// Without the flag, a folder inside a repository is refused rather than
/// silently adopted, so the answer agrees with the one `repo.inspect` gives for
/// the same path.
#[tokio::test]
async fn create_refuses_a_folder_inside_a_repository_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let outer = common::init_repo(dir.path());
    let inside = outer.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    let (port, token, _daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: inside.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(
        err.message.contains("inside a git repository"),
        "{}",
        err.message
    );

    cancel.cancel();
}

/// The daemon's data directory holds every worktree, sandbox home, object store
/// and the registry itself. A `git init` and a commit in there would make all of
/// it one repository, and a stale default in a path box is an ordinary way to
/// get there.
#[tokio::test]
async fn create_refuses_to_initialise_inside_the_daemons_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (port, token, _daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let inside_the_data_dir = data.join("worktrees").join("ws_made_up");

    let err = c
        .call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: inside_the_data_dir.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "alpha".into(),
            init_if_missing: true,
        }))
        .await
        .unwrap_err();

    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("data directory"), "{}", err.message);
    assert!(!inside_the_data_dir.join(".git").exists());

    cancel.cancel();
}
