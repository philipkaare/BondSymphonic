mod common;
use async_trait::async_trait;
use bondsymphonic_daemon::{
    agents::{
        adapter::AgentAdapter,
        backend::{Backend, PreparedAgent},
        claude_backend::ClaudeBackend,
        AgentSink,
    },
    daemon::Daemon,
    sandbox::{noop::NoopBackend, SandboxBackend, SandboxHandle, SandboxSpec},
    server::broadcast::EventBus,
    workspace::{agent_sandboxes::companion_spec, lifecycle, DataDirs, Workspace},
};
use bondsymphonic_proto::*;
use std::{path::PathBuf, sync::Arc};

struct TestCodex;
#[async_trait]
impl Backend for TestCodex {
    fn descriptor(&self) -> BackendDescriptor {
        let mut d = ClaudeBackend.descriptor();
        d.id = AgentAdapterKind::Codex;
        d
    }
    fn binary(&self) -> Option<PathBuf> {
        Some("/test/codex".into())
    }
    fn ro_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        vec![]
    }
    fn extra_hosts(&self) -> Vec<String> {
        vec!["api.openai.com".into(), "chatgpt.com".into()]
    }
    async fn prepare(
        &self,
        _: &Daemon,
        _: &Workspace,
        _: &AgentStartOptions,
    ) -> Result<PreparedAgent, RpcError> {
        unreachable!()
    }
    fn adapter(
        &self,
        _: AgentSink,
        _: Arc<dyn SandboxHandle>,
        _: PreparedAgent,
    ) -> Box<dyn AgentAdapter> {
        unreachable!()
    }
    async fn list_models(&self, _: &Daemon, _: Option<&str>) -> Result<ListModelsResult, RpcError> {
        unreachable!()
    }
}

#[derive(Default)]
struct RecordingSandbox {
    specs: parking_lot::Mutex<Vec<SandboxSpec>>,
    fail_next: std::sync::atomic::AtomicBool,
    deaths: parking_lot::Mutex<Vec<tokio::sync::watch::Sender<bool>>>,
    pause_next: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct TrackedHandle {
    inner: Arc<dyn SandboxHandle>,
    death: tokio::sync::watch::Sender<bool>,
}
#[async_trait]
impl SandboxHandle for TrackedHandle {
    async fn spawn(
        &self,
        cmd: bondsymphonic_daemon::sandbox::SandboxCommand,
    ) -> Result<bondsymphonic_daemon::sandbox::SandboxChild, RpcError> {
        self.inner.spawn(cmd).await
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        self.death.send_replace(true);
        self.inner.shutdown().await
    }
    fn helper_exe(&self) -> PathBuf {
        self.inner.helper_exe()
    }
    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        Some(self.death.subscribe())
    }
}
#[async_trait]
impl SandboxBackend for RecordingSandbox {
    fn name(&self) -> &'static str {
        "noop"
    }
    async fn check(&self) -> Vec<PrereqStatus> {
        vec![]
    }
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        self.specs.lock().push(spec.clone());
        if self
            .pause_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RpcError::internal("injected startup failure"));
        }
        let (death, _) = tokio::sync::watch::channel(false);
        self.deaths.lock().push(death.clone());
        Ok(Arc::new(TrackedHandle {
            inner: NoopBackend.start(spec).await?,
            death,
        }))
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<Daemon>,
    Workspace,
    Arc<RecordingSandbox>,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let backend = Arc::new(RecordingSandbox::default());
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend.clone(),
        EventBus::new(100),
    )
    .unwrap();
    let info = lifecycle::create(
        &d,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            name: "mixed".into(),
            base_branch: "main".into(),
            in_place: false,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let ws = d.workspace(&info.id).unwrap();
    (dir, d, ws, backend)
}

#[tokio::test]
async fn companion_mounts_share_worktree_but_isolate_homes_and_git_protection() {
    let (_dir, d, ws, backend) = fixture().await;
    let base = backend.specs.lock()[0].clone();
    let spec = companion_spec(&d, &ws, &TestCodex).await.unwrap();
    let codex_home = d.dirs.root.join("codex-home");
    assert!(!base.rw_binds.iter().any(|(host, _)| host == &codex_home));
    assert!(spec.rw_binds.iter().any(|(host, _)| host == &codex_home));
    assert!(!spec.home.starts_with(&base.home));
    assert!(!base.home.starts_with(&spec.home));
    assert_ne!(spec.run_dir, base.run_dir);
    assert_eq!(spec.cwd, base.cwd);
    assert_eq!(spec.late_ro_binds, base.late_ro_binds);
    assert!(spec
        .rw_binds
        .contains(&(ws.worktree_path.clone(), ws.worktree_path.clone())));
    assert!(!spec
        .ro_binds
        .iter()
        .any(|(_, p)| p.to_string_lossy().contains("claude")));
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
}

#[tokio::test]
async fn concurrent_starts_reuse_one_companion_and_restart_replaces_it() {
    let (_dir, d, ws, backend) = fixture().await;
    let (one, two) = tokio::join!(
        d.agent_sandboxes.ensure(&d, &ws, &TestCodex),
        d.agent_sandboxes.ensure(&d, &ws, &TestCodex)
    );
    let one = one.unwrap();
    let two = two.unwrap();
    assert!(Arc::ptr_eq(&one, &two));
    assert_eq!(backend.specs.lock().len(), 2);
    assert!(!Arc::ptr_eq(&one, &d.sandbox(&ws.id).unwrap()));
    lifecycle::restart(&d, &ws.id).await.unwrap();
    let next = d
        .agent_sandboxes
        .ensure(&d, &d.workspace(&ws.id).unwrap(), &TestCodex)
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&one, &next));
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
    assert!(d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.is_err());
    assert!(!d
        .dirs
        .root
        .join("agent-homes/codex")
        .join(ws.id.as_str())
        .exists());
}

#[tokio::test]
async fn failed_start_can_retry_and_a_stale_death_does_not_remove_replacement() {
    let (_dir, d, ws, backend) = fixture().await;
    backend
        .fail_next
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.is_err());
    let old = d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.unwrap();
    backend.deaths.lock()[1].send_replace(true);
    let next = d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.unwrap();
    assert!(!Arc::ptr_eq(&old, &next));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(Arc::ptr_eq(
        &next,
        &d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.unwrap()
    ));
    assert_eq!(d.workspace(&ws.id).unwrap().state, WorkspaceState::Ready);
    d.agent_sandboxes.shutdown().await;
    assert!(*next.died().unwrap().borrow());
    assert!(d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.is_err());
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
}

#[tokio::test]
async fn backend_hosts_are_added_only_to_the_companion_allowlist() {
    use bondsymphonic_daemon::{
        net::allowlist::Allowlist, workspace::agent_sandboxes::companion_allowlist,
    };
    let (_dir, d, ws, _) = fixture().await;
    assert!(!Allowlist::from_strings(&ws.allowlist).allows("api.openai.com"));
    assert!(companion_allowlist(&ws, &TestCodex.extra_hosts()).allows("api.openai.com"));
    let mut hosts = ws.allowlist.clone();
    hosts.push("explicit.example.com".into());
    lifecycle::set_allowlist(&d, &ws.id, &hosts).await.unwrap();
    let updated = d.workspace(&ws.id).unwrap();
    assert!(companion_allowlist(&updated, &TestCodex.extra_hosts()).allows("explicit.example.com"));
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
}

#[tokio::test]
async fn destroy_during_start_reclaims_the_unpublished_companion() {
    let (_dir, d, ws, backend) = fixture().await;
    backend
        .pause_next
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let starter = {
        let d = d.clone();
        let ws = ws.clone();
        tokio::spawn(async move { d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await })
    };
    backend.entered.notified().await;
    let destroyer = {
        let d = d.clone();
        let id = ws.id.clone();
        tokio::spawn(async move { lifecycle::destroy(&d, &id, true).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while d.workspace(&ws.id).unwrap().state != WorkspaceState::Destroying {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    backend.release.notify_one();
    assert!(starter.await.unwrap().is_err());
    destroyer.await.unwrap().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !*backend.deaths.lock().last().unwrap().borrow() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(d.workspace(&ws.id).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn real_bwrap_companion_has_private_home_and_shared_files() {
    use bondsymphonic_daemon::sandbox::{backend_for, SandboxCommand};
    use tokio::io::AsyncReadExt;
    async fn run(handle: &Arc<dyn SandboxHandle>, script: String) -> (i32, String) {
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), script],
                env: vec![],
                cwd: None,
                pty: None,
            })
            .await
            .unwrap();
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .await
            .unwrap();
        (child.exit.await.unwrap(), output)
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        EventBus::new(100),
    )
    .unwrap();
    let info = lifecycle::create(
        &d,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            name: "mixed".into(),
            base_branch: "main".into(),
            in_place: false,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let ws = d.workspace(&info.id).unwrap();
    let codex = d.agent_sandboxes.ensure(&d, &ws, &TestCodex).await.unwrap();
    let base = d.sandbox(&ws.id).unwrap();
    let home = d.dirs.root.join("codex-home");
    std::fs::write(home.join("private-proof"), "private").unwrap();
    assert_eq!(
        run(
            &base,
            format!("test ! -e '{}'/private-proof", home.display())
        )
        .await
        .0,
        0
    );
    let (code,output)=run(&codex,format!("cat '{}'/private-proof; printf shared > shared-proof; printf refreshed > '{}'/refresh-proof",home.display(),home.display())).await;
    assert_eq!(code, 0);
    assert_eq!(output, "private");
    assert_eq!(
        run(&base, "cat shared-proof".into()).await,
        (0, "shared".into())
    );
    assert_eq!(
        std::fs::read_to_string(home.join("refresh-proof")).unwrap(),
        "refreshed"
    );
    let spec = companion_spec(&d, &ws, &TestCodex).await.unwrap();
    for (_, protected) in spec.late_ro_binds {
        assert_ne!(
            run(
                &codex,
                format!("printf altered >> '{}'", protected.display())
            )
            .await
            .0,
            0
        );
    }
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
    let mut died = codex.died().unwrap();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        died.wait_for(|dead| *dead),
    )
    .await
    .unwrap();
}
