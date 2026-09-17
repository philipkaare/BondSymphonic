//! An in-place workspace from create to close, over the no-sandbox backend,
//! and once over bubblewrap: the checkout is used as it is, nothing of the
//! user's is created or deleted, and a checkout that has gone away says so.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::{backend_for, SandboxHandle};
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{create_ws, start_daemon, Client};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn params(repo: &Path, name: &str) -> WorkspaceCreateParams {
    WorkspaceCreateParams {
        repo_path: repo.to_string_lossy().into(),
        base_branch: String::new(),
        name: name.into(),
        init_if_missing: false,
        in_place: true,
    }
}

async fn create_in_place(
    c: &mut Client,
    repo: &Path,
    name: &str,
) -> Result<WorkspaceInfo, RpcError> {
    c.call(Request::WorkspaceCreate(params(repo, name)))
        .await
        .map(|v| serde_json::from_value(v).unwrap())
}

/// Everything under `root`, `.git` included, as (path, bytes); directories as
/// (path + "/", empty). What "byte-identical" means in the tests below.
fn tree_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                stack.push(path);
            } else {
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// The spec's four readings of a repository. `--no-optional-locks` so reading
/// the status does not refresh (and rewrite) the index it is comparing.
fn repo_state(root: &Path) -> (Vec<(String, Vec<u8>)>, String, String, String) {
    let bytes = tree_bytes(root);
    let status = common::git_out(root, &["--no-optional-locks", "status", "--porcelain=v2"]);
    let refs = common::git_out(root, &["for-each-ref"]);
    let config = std::fs::read_to_string(root.join(".git/config")).unwrap();
    (bytes, status, refs, config)
}

async fn shut_down_sandboxes(d: &Daemon) {
    let handles: Vec<Arc<dyn SandboxHandle>> = d.sandboxes.lock().drain().map(|(_, h)| h).collect();
    for h in handles {
        let _ = h.shutdown().await;
    }
}

#[tokio::test]
async fn a_checkout_is_used_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;

    let ws = create_in_place(&mut c, &repo, "here").await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);
    assert_eq!(ws.kind, WorkspaceKind::InPlace);
    assert_eq!(PathBuf::from(&ws.worktree_path), repo);
    assert_eq!(
        (ws.branch.as_str(), ws.base_branch.as_str()),
        ("main", "main")
    );
    // No branch, no worktree, no private objects.
    assert_eq!(
        common::git_out(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        ),
        "refs/heads/main"
    );
    assert_eq!(
        common::git_out(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert!(!DataDirs::new(&data).objects(&ws.id).exists());
    // `layout_for` has nothing to say about it.
    let w = daemon.registry.get(&ws.id).unwrap();
    assert_eq!(
        lifecycle::layout_for(&daemon, &w).await.unwrap_err().code,
        ErrorCode::Internal
    );
    // The kind survives the registry file.
    let raw = std::fs::read_to_string(data.join("workspaces.json")).unwrap();
    assert!(
        raw.contains("\"kind\": \"in_place\"") || raw.contains("\"kind\":\"in_place\""),
        "{raw}"
    );
    cancel.cancel();
}

#[tokio::test]
async fn a_detached_head_is_recorded_as_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_in_place(&mut c, &repo, "loose").await.unwrap();
    assert_eq!((ws.branch.as_str(), ws.base_branch.as_str()), ("", ""));
    cancel.cancel();
}

#[tokio::test]
async fn worktrees_bare_and_nested_paths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let linked = dir.path().join("linked");
    common::git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &linked.to_string_lossy(),
        ],
    );
    let bare = dir.path().join("bare.git");
    common::git_ok(
        dir.path(),
        &["init", "-q", "--bare", &bare.to_string_lossy()],
    );
    let nested = repo.join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let e = create_in_place(&mut c, &linked, "a").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("linked worktree"), "{}", e.message);
    let e = create_in_place(&mut c, &bare, "b").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("bare repository"), "{}", e.message);
    let e = create_in_place(&mut c, &nested, "c").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    common::assert_names_enclosing_repo(&e.message, &repo);
    assert!(daemon.registry.list().is_empty());
    cancel.cancel();
}

#[tokio::test]
async fn a_checkout_that_holds_the_data_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&repo.join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let e = create_in_place(&mut c, &repo, "greedy").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(
        e.message
            .contains("contains the daemon's own data directory"),
        "{}",
        e.message
    );
    cancel.cancel();
}

#[tokio::test]
async fn one_in_place_workspace_per_checkout_and_worktrees_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    create_in_place(&mut c, &repo, "first").await.unwrap();
    // Another spelling of the same checkout is the same checkout.
    let other_spelling = repo.join(".").join("..").join("repo");
    let e = create_in_place(&mut c, &other_spelling, "second")
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert_eq!(
        e.message,
        "this checkout already has an in-place workspace: first"
    );
    let ws = create_ws(&mut c, &repo, "beside").await;
    assert_eq!(ws.kind, WorkspaceKind::Worktree);
    assert_eq!(ws.state, WorkspaceState::Ready);
    cancel.cancel();
}

#[tokio::test]
async fn a_new_folder_can_be_initialised_and_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("fresh");
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    // Without the flag it is refused as before.
    let e = create_in_place(&mut c, &folder, "fresh").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            init_if_missing: true,
            ..params(&folder, "fresh")
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert_eq!(ws.branch, "main");
    assert_eq!(
        common::git_out(&folder, &["rev-list", "--count", "HEAD"]),
        "1"
    );
    cancel.cancel();
}

#[tokio::test]
async fn closing_leaves_the_repository_byte_identical_whatever_force_says() {
    for force in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        // Work the user has in flight: none of it may be touched.
        std::fs::write(repo.join("README.md"), "edited\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "mine\n").unwrap();
        common::git_ok(&repo, &["branch", "keep-me"]);
        common::git_ok(&repo, &["--no-optional-locks", "status"]);
        let before = repo_state(&repo);
        let data = dir.path().join("data");
        let (port, token, daemon, cancel) = start_daemon(&data).await;
        let mut c = Client::connect(port, &token).await;
        let ws = create_in_place(&mut c, &repo, "closing").await.unwrap();

        c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force,
        }))
        .await
        .unwrap();

        assert_eq!(repo_state(&repo), before, "force = {force}");
        assert!(daemon.registry.get(&ws.id).is_none());
        let dirs = DataDirs::new(&data);
        for gone in [dirs.home(&ws.id), dirs.cache(&ws.id), dirs.run(&ws.id)] {
            assert!(!gone.exists(), "{} survived", gone.display());
        }
        cancel.cancel();
    }
}

#[tokio::test]
async fn status_is_measured_against_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_in_place(&mut c, &repo, "status").await.unwrap();
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "n\n").unwrap();
    common::git_ok(&repo, &["add", "new.txt"]);
    let r: WorkspaceStatusResult = serde_json::from_value(
        c.call(Request::WorkspaceStatus(WorkspaceIdParams {
            workspace_id: ws.id,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let mut seen: Vec<(String, FileStatus, bool)> = r
        .entries
        .into_iter()
        .map(|e| (e.path, e.status, e.staged))
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        [
            ("README.md".to_string(), FileStatus::Modified, false),
            ("new.txt".to_string(), FileStatus::Added, true),
        ]
    );
    cancel.cancel();
}

/// Plan R3: an embedded repository's own config must not run when the daemon
/// asks for status, in either kind of workspace.
#[cfg(unix)]
#[tokio::test]
async fn status_does_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let in_place = create_in_place(&mut c, &repo, "ip").await.unwrap();
    let worktree = create_ws(&mut c, &repo, "wt").await;
    for (ws, env) in [
        (&in_place, Vec::new()),
        (
            &worktree,
            lifecycle::layout_for(&daemon, &daemon.registry.get(&worktree.id).unwrap())
                .await
                .unwrap()
                .sandbox_git_env(),
        ),
    ] {
        let root = PathBuf::from(&ws.worktree_path);
        let marker = dir.path().join(format!("PWNED-{}", ws.name));
        plant_embedded_repo(&root, &marker, &env);
        // Anything the test's own git set off while planting is not the
        // daemon's doing.
        let _ = std::fs::remove_file(&marker);
        lifecycle::status(&daemon, &ws.id).await.unwrap();
        assert!(
            !marker.exists(),
            "{}: status ran the embedded repository's config",
            ws.name
        );
    }
    cancel.cancel();
}

/// `sub/` as a repository of its own, committed as a gitlink, then given a
/// `core.fsmonitor` and touched, which is what makes a parent's status look in
/// and run it. The config comes after the parent's commit, whose own status
/// would otherwise set the marker off before the daemon is asked anything.
pub fn plant_embedded_repo(root: &Path, marker: &Path, env: &[(String, String)]) {
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    common::git_ok(&sub, &["init", "-q", "-b", "main"]);
    common::git_ok(&sub, &["config", "commit.gpgsign", "false"]);
    std::fs::write(sub.join("f.txt"), "f\n").unwrap();
    common::commit_all(&sub, &[], "sub");
    common::commit_all(root, env, "gitlink");
    let config = sub.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\tfsmonitor = touch {}\n",
        marker.display()
    ));
    std::fs::write(&config, text).unwrap();
    std::fs::write(sub.join("dirty.txt"), "x\n").unwrap();
}

#[tokio::test]
async fn a_checkout_that_went_away_says_so_and_comes_back_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let first = Daemon::new(DataDirs::new(&data), backend_for("noop"), EventBus::new(64)).unwrap();
    let ws = lifecycle::create(&first, params(&repo, "moved"))
        .await
        .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    shut_down_sandboxes(&first).await;
    drop(first);

    let aside = dir.path().join("aside");
    std::fs::rename(&repo, &aside).unwrap();
    let daemon = Daemon::new(DataDirs::new(&data), backend_for("noop"), EventBus::new(64)).unwrap();
    daemon
        .restore_workspaces_from(lifecycle::restore_snapshot(&daemon))
        .await;
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Error(format!(
            "The repository {} is missing or is no longer a git repository. Close the \
             workspace, or restore the folder and press Retry.",
            repo.display()
        ))
    );

    std::fs::rename(&aside, &repo).unwrap();
    let info = lifecycle::restart(&daemon, &ws.id).await.unwrap();
    assert_eq!(info.state, WorkspaceState::Ready);
    // No worktree repair ran: the repository still has exactly one worktree.
    assert_eq!(
        common::git_out(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    // A plain restart of a Ready workspace works too.
    assert_eq!(
        lifecycle::restart(&daemon, &ws.id).await.unwrap().state,
        WorkspaceState::Ready
    );
    lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
    assert!(repo.join(".git/config").is_file());
    assert!(!repo.join(".git/commondir").exists());
}

/// Git deletes `.git/worktrees` when the last linked worktree goes, which
/// would take a running in-place sandbox's read-only bind with it. The
/// daemon's own removal leaves the directory standing, and nothing of its
/// hold behind.
#[tokio::test]
async fn removing_a_worktree_workspace_keeps_the_worktrees_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "last").await;
    // What `git push -u` leaves: the branch's section goes with the branch.
    common::git_ok(&repo, &["config", "branch.bs/last/work.remote", "origin"]);
    lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
    let sections = std::process::Command::new("git")
        .args(["config", "--local", "--get-regexp", r"^branch\."])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&sections.stdout), "");
    assert_eq!(
        common::git_out(&repo, &["branch", "--list", "bs/*"]),
        "",
        "the branch survived"
    );
    let worktrees = repo.join(".git/worktrees");
    assert!(worktrees.is_dir(), "git removed .git/worktrees");
    assert_eq!(
        std::fs::read_dir(&worktrees).unwrap().count(),
        0,
        "the hold was left behind"
    );
    assert_eq!(
        common::git_out(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    cancel.cancel();
}

/// A sandbox that calls itself `linux_bwrap`, so the protection watcher runs,
/// over the no-sandbox backend. The first time the watcher asks for its pid it
/// dies, and answers only once `watch_sandbox` has taken it out of the
/// registry: the watcher then checks a sandbox that is already gone, every
/// time, rather than once in a scheduler's while.
mod dying {
    use super::*;
    use async_trait::async_trait;
    use bondsymphonic_daemon::sandbox::{
        SandboxBackend, SandboxChild, SandboxCommand, SandboxSpec,
    };
    use bondsymphonic_daemon::workspace::in_place;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{OnceLock, Weak};

    type DaemonCell = Arc<OnceLock<Weak<Daemon>>>;

    /// Set once the dying sandbox was out of the registry before its pid was
    /// answered, which is the order the test is about.
    static GONE_BEFORE_ANSWER: AtomicBool = AtomicBool::new(false);

    struct DyingBackend {
        daemon: DaemonCell,
    }

    struct DyingHandle {
        inner: Arc<dyn SandboxHandle>,
        id: WorkspaceId,
        died: tokio::sync::watch::Sender<bool>,
        dead: AtomicBool,
        daemon: DaemonCell,
    }

    #[async_trait]
    impl SandboxBackend for DyingBackend {
        fn name(&self) -> &'static str {
            "linux_bwrap"
        }
        async fn check(&self) -> Vec<PrereqStatus> {
            Vec::new()
        }
        async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
            Ok(Arc::new(DyingHandle {
                inner: backend_for("noop").start(spec).await?,
                id: spec.id.clone(),
                died: tokio::sync::watch::channel(false).0,
                dead: AtomicBool::new(false),
                daemon: self.daemon.clone(),
            }))
        }
    }

    #[async_trait]
    impl SandboxHandle for DyingHandle {
        async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
            self.inner.spawn(cmd).await
        }
        async fn shutdown(&self) -> Result<(), RpcError> {
            self.inner.shutdown().await
        }
        fn helper_exe(&self) -> PathBuf {
            self.inner.helper_exe()
        }
        fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
            Some(self.died.subscribe())
        }
        fn host_pid(&self) -> Option<u32> {
            if !self.dead.swap(true, Ordering::SeqCst) {
                let _ = self.died.send(true);
                // `block_in_place` hands this worker's queued tasks -- the
                // woken `watch_sandbox` among them -- to another worker while
                // this one waits.
                let daemon = self.daemon.get().and_then(Weak::upgrade).unwrap();
                tokio::task::block_in_place(|| {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while daemon.sandboxes.lock().contains_key(&self.id) {
                        if std::time::Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    GONE_BEFORE_ANSWER.store(true, Ordering::SeqCst);
                });
            }
            None
        }
    }

    /// A sandbox that dies on its own is `SandboxDown`, and the watcher that
    /// finds its mount table gone says nothing about replaced files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sandbox_that_dies_is_down_not_breached() {
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let cell: DaemonCell = Arc::default();
        let bus = EventBus::new(256);
        let mut events = bus.subscribe();
        let daemon = Daemon::new(
            DataDirs::new(dir.path().join("data")),
            Arc::new(DyingBackend {
                daemon: cell.clone(),
            }),
            bus,
        )
        .unwrap();
        cell.set(Arc::downgrade(&daemon)).unwrap();
        let ws = lifecycle::create(&daemon, params(&repo, "dying"))
            .await
            .unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while daemon.registry.get(&ws.id).unwrap().state != WorkspaceState::SandboxDown {
            assert!(std::time::Instant::now() < deadline, "never SandboxDown");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Long enough for the watcher to finish its check and its look under
        // the gate.
        tokio::time::sleep(in_place::PROTECTION_POLL * 3).await;
        assert!(
            GONE_BEFORE_ANSWER.load(Ordering::SeqCst),
            "the watcher never asked a dead sandbox for its pid"
        );
        assert_eq!(
            daemon.registry.get(&ws.id).unwrap().state,
            WorkspaceState::SandboxDown
        );
        while let Ok(msg) = events.try_recv() {
            if let ServerMessage::Event {
                event: Event::DaemonLog { message, .. },
                ..
            } = msg
            {
                assert!(
                    !message.contains("Git files") && !message.contains("worktree was removed"),
                    "a breach was reported for a sandbox that died: {message}"
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod bwrap {
    use super::*;
    use bondsymphonic_daemon::sandbox::SandboxCommand;
    use bondsymphonic_daemon::server::{Server, ServerConfig};
    use bondsymphonic_daemon::workspace::in_place::{self, ProtectionBreach};
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

    fn names(root: &Path) -> Vec<String> {
        tree_bytes(root).into_iter().map(|(p, _)| p).collect()
    }

    #[tokio::test]
    async fn an_agent_commits_in_the_checkout_and_the_binds_create_only_what_is_documented() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        // `branches` depends on git's template; take the question away.
        for gone in [".git/hooks", ".git/branches"] {
            let _ = std::fs::remove_dir_all(repo.join(gone));
        }
        let before = names(&repo);
        let server = Server::bind(ServerConfig::default()).await.unwrap();
        let daemon = Daemon::new(
            DataDirs::new(dir.path().join("data")),
            backend_for("linux_bwrap"),
            server.event_bus(),
        )
        .unwrap();
        let ws = lifecycle::create(&daemon, params(&repo, "live"))
            .await
            .unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

        let added: Vec<String> = names(&repo)
            .into_iter()
            .filter(|n| !before.contains(n))
            .collect();
        assert_eq!(
            added,
            [
                ".git/branches/",
                ".git/commondir",
                ".git/config.worktree",
                ".git/hooks/",
                ".git/remotes/",
                ".git/worktrees/"
            ]
        );

        let handle = daemon.sandbox(&ws.id).unwrap();
        let script = format!(
            "cd '{}' && pwd && echo more >> README.md && git add README.md && \
             git -c commit.gpgsign=false commit -q -m from-the-agent && git switch -q -c agent-work && \
             (printf x >> .git/config && echo CONFIG-WRITTEN || echo config-refused)",
            repo.display()
        );
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["sh".into(), "-c".into(), script],
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
        assert_eq!(child.exit.await.unwrap(), 0, "{out}");
        assert!(out.contains("config-refused"), "{out}");
        assert_eq!(
            common::git_out(&repo, &["log", "-1", "--format=%s"]),
            "from-the-agent"
        );
        assert_eq!(
            common::git_out(&repo, &["branch", "--show-current"]),
            "agent-work"
        );

        lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
        let after: Vec<String> = names(&repo)
            .into_iter()
            .filter(|n| !before.contains(n))
            .collect();
        // The hooks directory stays (git would have made it); the guard and the
        // empty entries the daemon made are taken back, and so is its record
        // of them.
        for gone in [
            ".git/commondir",
            ".git/config.worktree",
            ".git/worktrees/",
            ".git/remotes/",
            ".git/branches/",
        ] {
            assert!(
                !after.iter().any(|n| n == gone),
                "{gone} survived Close: {after:?}"
            );
        }
        assert!(before.iter().all(|n| n != ".git/hooks/"));
        assert!(names(&repo).iter().any(|n| n == ".git/hooks/"));
        assert!(!DataDirs::new(dir.path().join("data"))
            .in_place_record(&ws.id)
            .exists());
    }

    /// A git on the host that replaces `.git/config` -- as every `git config`
    /// does, by renaming `config.lock` over it -- takes the read-only bind
    /// away from the running sandbox. The daemon notices, says what changed,
    /// stops the sandbox and leaves the workspace in `Error` until Retry,
    /// which protects the new file.
    #[tokio::test]
    async fn a_config_replaced_from_the_host_stops_the_sandbox() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let server = Server::bind(ServerConfig::default()).await.unwrap();
        let bus = server.event_bus();
        let mut events = bus.subscribe();
        let daemon = Daemon::new(
            DataDirs::new(dir.path().join("data")),
            backend_for("linux_bwrap"),
            bus,
        )
        .unwrap();
        let ws = lifecycle::create(&daemon, params(&repo, "swapped"))
            .await
            .unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

        let config = repo.join(".git/config");
        // The pid the mount check reads is in the sandbox's mount namespace:
        // its mount table has the read-only bind on the config.
        let pid = daemon
            .sandbox(&ws.id)
            .unwrap()
            .host_pid()
            .expect("a host pid");
        let mountinfo = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap();
        let canonical = std::fs::canonicalize(&config).unwrap();
        assert!(
            mountinfo.contains(&format!(" {} ", canonical.display())),
            "{mountinfo}"
        );
        let lock = repo.join(".git/config.lock");
        // Twice, as two `git config` writes in a row do: the second rename
        // usually hands the file its old inode back on ext4 (measured), so only
        // the sandbox's mount table can tell it was replaced.
        for line in ["[core]\n\tfsmonitor = echo planted\n", "[x]\n\ty = z\n"] {
            let mut text = std::fs::read_to_string(&config).unwrap();
            text.push_str(line);
            std::fs::write(&lock, text).unwrap();
            std::fs::rename(&lock, &config).unwrap();
        }

        let expected = WorkspaceState::Error(
            ProtectionBreach {
                entries: vec![".git/config".into()],
                removed: vec![],
                config_diff: String::new(),
            }
            .sentence(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while daemon.registry.get(&ws.id).unwrap().state != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "still {:?}",
                daemon.registry.get(&ws.id).unwrap().state
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            daemon.sandbox(&ws.id).is_err(),
            "a sandbox was left running"
        );

        // The warning names the change, on this workspace.
        let mut warned = false;
        while let Ok(msg) = events.try_recv() {
            if let ServerMessage::Event {
                workspace_id: Some(id),
                event: Event::DaemonLog { level, message, .. },
            } = msg
            {
                if id == ws.id
                    && level == LogLevel::Warn
                    && message.contains("+\tfsmonitor = echo planted")
                {
                    assert!(
                        message.starts_with("Git files this workspace protects"),
                        "{message}"
                    );
                    warned = true;
                }
            }
        }
        assert!(warned, "no daemon.log warning with the config diff");

        // Retry protects the file that is there now.
        let info = lifecycle::restart(&daemon, &ws.id).await.unwrap();
        assert_eq!(info.state, WorkspaceState::Ready);
        tokio::time::sleep(in_place::PROTECTION_POLL * 3).await;
        assert_eq!(
            daemon.registry.get(&ws.id).unwrap().state,
            WorkspaceState::Ready
        );
        lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
    }

    async fn bwrap_daemon(dir: &Path) -> Arc<Daemon> {
        let server = Server::bind(ServerConfig::default()).await.unwrap();
        Daemon::new(
            DataDirs::new(dir.join("data")),
            backend_for("linux_bwrap"),
            server.event_bus(),
        )
        .unwrap()
    }

    /// Waits up to 2 s for `ws` to reach `state`.
    async fn wait_for_state(daemon: &Daemon, ws: &WorkspaceId, state: &WorkspaceState) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while daemon.registry.get(ws).unwrap().state != *state {
            assert!(
                std::time::Instant::now() < deadline,
                "still {:?}",
                daemon.registry.get(ws).unwrap().state
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The daemon removing the repository's last worktree workspace is its own
    /// cleanup, not a breach: the in-place sandbox beside it keeps running.
    #[tokio::test]
    async fn destroying_a_sibling_worktree_workspace_leaves_the_agent_running() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let daemon = bwrap_daemon(dir.path()).await;
        let here = lifecycle::create(&daemon, params(&repo, "here"))
            .await
            .unwrap();
        assert_eq!(here.state, WorkspaceState::Ready, "{:?}", here.state);
        let beside = lifecycle::create(
            &daemon,
            WorkspaceCreateParams {
                base_branch: "main".into(),
                in_place: false,
                ..params(&repo, "beside")
            },
        )
        .await
        .unwrap();
        assert_eq!(beside.state, WorkspaceState::Ready, "{:?}", beside.state);
        let sandbox = daemon.sandbox(&here.id).unwrap();

        lifecycle::destroy(&daemon, &beside.id, false)
            .await
            .unwrap();

        tokio::time::sleep(in_place::PROTECTION_POLL * 4).await;
        assert_eq!(
            daemon.registry.get(&here.id).unwrap().state,
            WorkspaceState::Ready
        );
        assert!(Arc::ptr_eq(&daemon.sandbox(&here.id).unwrap(), &sandbox));
        lifecycle::destroy(&daemon, &here.id, false).await.unwrap();
    }

    /// The user removing their own last worktree does take the bind away; the
    /// sandbox stops, and the sentence says what happened rather than raising
    /// an alarm about `.git/config`.
    #[tokio::test]
    async fn the_users_last_worktree_going_stops_the_sandbox_with_its_own_sentence() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let daemon = bwrap_daemon(dir.path()).await;
        let ws = lifecycle::create(&daemon, params(&repo, "mine"))
            .await
            .unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

        let theirs = dir.path().join("theirs");
        let theirs = theirs.to_string_lossy();
        common::git_ok(&repo, &["worktree", "add", "-q", "-b", "theirs", &theirs]);
        common::git_ok(&repo, &["worktree", "remove", &theirs]);
        assert!(!repo.join(".git/worktrees").exists());

        let sentence = ProtectionBreach {
            entries: vec![".git/worktrees".into()],
            removed: vec![".git/worktrees".into()],
            config_diff: String::new(),
        }
        .sentence();
        assert!(
            sentence.starts_with("This repository's last worktree was removed"),
            "{sentence}"
        );
        wait_for_state(&daemon, &ws.id, &WorkspaceState::Error(sentence)).await;
        assert!(
            daemon.sandbox(&ws.id).is_err(),
            "a sandbox was left running"
        );

        // Retry puts the directory back and starts again.
        assert_eq!(
            lifecycle::restart(&daemon, &ws.id).await.unwrap().state,
            WorkspaceState::Ready
        );
        assert!(repo.join(".git/worktrees").is_dir());
        lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
    }
}
