use async_trait::async_trait;
use bondsymphonic_daemon::harness_updates::{self, Candidate, Harness, UpdateSource};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

struct Source {
    installed: bool,
    compatible: bool,
    delay: bool,
    seen: Mutex<Vec<Harness>>,
    current: &'static str,
    newest: &'static str,
    unchanged: bool,
    broken_download: bool,
}
#[async_trait]
impl UpdateSource for Source {
    fn installed(&self, harness: Harness) -> Option<PathBuf> {
        (self.installed && harness == Harness::Codex).then(|| PathBuf::from("old-codex"))
    }
    async fn version(&self, _: &Path) -> anyhow::Result<String> {
        Ok(self.current.into())
    }
    async fn stage(
        &self,
        harness: Harness,
        directory: &Path,
        _: Option<&str>,
    ) -> anyhow::Result<Candidate> {
        self.seen.lock().unwrap().push(harness);
        anyhow::ensure!(!self.broken_download, "offline");
        if self.unchanged {
            return Ok(Candidate {
                version: self.newest.into(),
                binary: PathBuf::new(),
                helper: None,
                unchanged: true,
            });
        }
        if self.delay {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let binary = directory.join("codex");
        let helper = directory.join("codex-code-mode-host");
        std::fs::write(&binary, "new codex")?;
        std::fs::write(&helper, "matching helper")?;
        Ok(Candidate {
            version: self.newest.into(),
            binary,
            helper: Some(helper),
            unchanged: false,
        })
    }
    async fn compatible(&self, _: Harness, _: &Candidate) -> anyhow::Result<()> {
        anyhow::ensure!(self.compatible, "incompatible protocol");
        Ok(())
    }
}
fn source(installed: bool, compatible: bool) -> Source {
    Source {
        installed,
        compatible,
        delay: false,
        seen: Mutex::new(vec![]),
        current: "1.0.0",
        newest: "2.0.0",
        unchanged: false,
        broken_download: false,
    }
}

#[tokio::test]
async fn installed_harness_is_validated_and_binary_helper_are_published_together() {
    let root = tempfile::tempdir().unwrap();
    let source = source(true, true);
    let mut events = vec![];
    let report =
        harness_updates::update_installed(root.path(), &source, Duration::from_secs(2), |event| {
            events.push(event)
        })
        .await;
    assert_eq!(report[1].status, "updated");
    let binary = harness_updates::managed_binary(root.path(), Harness::Codex).unwrap();
    assert_eq!(std::fs::read_to_string(&binary).unwrap(), "new codex");
    assert_eq!(
        std::fs::read_to_string(binary.with_file_name("codex-code-mode-host")).unwrap(),
        "matching helper"
    );
    assert!(events.iter().any(|e| e.status == "checking"));
    assert!(events.iter().any(|e| e.status == "updating"));
    assert_eq!(*source.seen.lock().unwrap(), vec![Harness::Codex]);
}

#[tokio::test]
async fn compatibility_failure_preserves_previous_selection_and_reports_warning() {
    let root = tempfile::tempdir().unwrap();
    harness_updates::update_installed(
        root.path(),
        &source(true, true),
        Duration::from_secs(2),
        |_| {},
    )
    .await;
    let before = std::fs::read(root.path().join("codex/current.json")).unwrap();
    let report = harness_updates::update_installed(
        root.path(),
        &source(true, false),
        Duration::from_secs(2),
        |_| {},
    )
    .await;
    assert_eq!(report[1].status, "warning");
    assert_eq!(
        std::fs::read(root.path().join("codex/current.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn absent_harnesses_are_not_installed_and_slow_updates_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let absent = source(false, true);
    let report =
        harness_updates::update_installed(root.path(), &absent, Duration::from_secs(1), |_| {})
            .await;
    assert!(report.iter().all(|r| r.status == "skipped"));
    assert!(absent.seen.lock().unwrap().is_empty());
    let mut slow = source(true, true);
    slow.delay = true;
    let report =
        harness_updates::update_installed(root.path(), &slow, Duration::from_millis(10), |_| {})
            .await;
    assert_eq!(report[1].status, "warning");
    assert!(harness_updates::managed_binary(root.path(), Harness::Codex).is_none());
}

#[tokio::test]
async fn unchanged_metadata_never_downgrades_or_requires_a_downloaded_candidate() {
    let root = tempfile::tempdir().unwrap();
    let mut source = source(true, false);
    source.current = "3.0.0";
    source.newest = "2.0.0";
    source.unchanged = true;
    let report =
        harness_updates::update_installed(root.path(), &source, Duration::from_secs(2), |_| {})
            .await;
    assert_eq!(report[1].status, "current");
    assert_eq!(report[1].to_version.as_deref(), Some("3.0.0"));
    assert!(harness_updates::managed_binary(root.path(), Harness::Codex).is_none());
}

#[tokio::test]
async fn offline_update_preserves_an_already_validated_release() {
    let root = tempfile::tempdir().unwrap();
    harness_updates::update_installed(
        root.path(),
        &source(true, true),
        Duration::from_secs(2),
        |_| {},
    )
    .await;
    let before = harness_updates::managed_binary(root.path(), Harness::Codex).unwrap();
    let mut offline = source(true, true);
    offline.broken_download = true;
    let report =
        harness_updates::update_installed(root.path(), &offline, Duration::from_secs(2), |_| {})
            .await;
    assert_eq!(report[1].status, "warning");
    assert_eq!(
        harness_updates::managed_binary(root.path(), Harness::Codex),
        Some(before)
    );
}

#[test]
fn managed_manifest_cannot_select_an_executable_outside_its_release_directory() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("unrelated");
    std::fs::write(&outside, "not an installed harness").unwrap();
    std::fs::create_dir_all(root.path().join("claude")).unwrap();
    let candidate = Candidate {
        version: "2.0.0".into(),
        binary: outside,
        helper: None,
        unchanged: false,
    };
    std::fs::write(
        root.path().join("claude/current.json"),
        serde_json::to_vec(&candidate).unwrap(),
    )
    .unwrap();
    assert!(harness_updates::managed_binary(root.path(), Harness::Claude).is_none());
}

fn python_command(script: &str) -> tokio::process::Command {
    let python = ["python", "python3"]
        .into_iter()
        .find(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .expect("Python required for fake harness protocol integration");
    let mut command = tokio::process::Command::new(python);
    command.arg("-c").arg(script);
    command
}

#[tokio::test]
async fn codex_compatibility_probe_creates_isolated_home_before_initializing() {
    let root = tempfile::tempdir().unwrap();
    let command = python_command(
        r#"import json,os,sys
assert os.path.isdir(os.environ['CODEX_HOME']), 'CODEX_HOME must exist'
assert os.path.dirname(os.environ['CODEX_HOME']) == os.environ['HOME']
assert os.getcwd() == os.environ['HOME']
assert os.environ['DISABLE_AUTOUPDATER'] == '1'
assert os.environ['DISABLE_UPDATES'] == '1'
request=json.loads(sys.stdin.readline())
assert request['method'] == 'initialize'
print(json.dumps({'id':request['id'],'result':{'userAgent':'fake-compatible-codex'}}),flush=True)
sys.stdin.read()
"#,
    );
    harness_updates::probe_protocol_command(
        Harness::Codex,
        command,
        &root.path().join("probe-home"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn opencode_compatibility_probe_rejects_missing_resume_capability() {
    let root = tempfile::tempdir().unwrap();
    let command = python_command(
        r#"import json,sys
request=json.loads(sys.stdin.readline())
print(json.dumps({'id':request['id'],'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':False}}}),flush=True)
sys.stdin.read()
"#,
    );
    let error = harness_updates::probe_protocol_command(
        Harness::OpenCode,
        command,
        &root.path().join("probe-home"),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("resume capability"));
}

#[tokio::test]
async fn live_daemon_lock_refuses_updates_before_any_installation_is_touched() {
    let root = tempfile::tempdir().unwrap();
    let _owner = bondsymphonic_daemon::daemon::InstanceLock::acquire(root.path()).unwrap();
    let error = harness_updates::run_cli(root.path(), false)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<bondsymphonic_daemon::daemon::InstanceLockError>(),
        Some(bondsymphonic_daemon::daemon::InstanceLockError::Busy(_))
    ));
}
