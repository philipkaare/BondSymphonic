#![cfg(target_os = "linux")]
//! Explicit live compatibility probe, never part of the offline test suite.
//! Uses the real sandbox/proxy with only a disposable work directory writable.

use bondsymphonic_daemon::net::{allowlist::Allowlist, proxy::ProxyRegistry};
use bondsymphonic_daemon::sandbox::{backend_for, SandboxCommand, SandboxSpec};
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_proto::WorkspaceId;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

fn required(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}

#[tokio::test]
#[ignore = "requires explicit Codex binary, authenticated home, live model access"]
async fn codex_commands_and_patches_inside_project_sandbox() {
    let binary = required("BS_CODEX_BIN");
    let code_mode_host = required("BS_CODEX_CODE_MODE_HOST");
    let auth_home = required("BS_CODEX_HOME");
    let probe = required("BS_CODEX_PROBE");
    let root = required("BS_CODEX_CAPTURE_DIR");
    let approvals = std::env::var_os("BS_CODEX_PREFLIGHT_APPROVALS").is_some();
    let work = root.join("work");
    let run = root.join("run");
    let home = root.join("home");
    for dir in [&work, &run, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let id: WorkspaceId = "ws_codex_preflight".into();
    let hosts = ["api.openai.com", "auth.openai.com", "chatgpt.com"].map(str::to_owned);
    let proxy = ProxyRegistry::default();
    let events = EventBus::new(256);
    let mut notices = events.subscribe();
    proxy
        .start(
            &id,
            &run.join("proxy.sock"),
            Allowlist::from_strings(&hosts),
            events,
        )
        .await
        .unwrap();
    let spec = SandboxSpec {
        id: id.clone(),
        rw_binds: vec![
            (work.clone(), work.clone()),
            (auth_home, "/opt/bs/codex-home".into()),
        ],
        ro_binds: vec![
            (binary, "/opt/bs/codex".into()),
            (code_mode_host, "/opt/bs/codex-code-mode-host".into()),
            (probe, "/opt/bs/probe-codex.py".into()),
        ],
        late_ro_binds: vec![],
        home,
        run_dir: run,
        env: [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ]
        .map(|key| (key.to_string(), "http://127.0.0.1:18080".into()))
        .to_vec(),
        cwd: work.clone(),
    };
    let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();
    let result = async {
        let mut shim = handle.spawn(SandboxCommand {
            argv: vec![handle.helper_exe().to_string_lossy().into_owned(), "proxy-shim".into(),
                "--socket".into(), "/run/bs/proxy.sock".into(), "--listen".into(), "127.0.0.1:18080".into()],
            env: vec![], cwd: None, pty: None,
        }).await.unwrap();
        let mut ready = String::new();
        tokio::time::timeout(Duration::from_secs(10),
            BufReader::new(shim.stdout.take().unwrap()).read_line(&mut ready)).await.unwrap().unwrap();
        assert!(ready.contains(bondsymphonic_daemon::net::shim::READY_LINE), "shim: {ready}");
        let mut child = handle.spawn(SandboxCommand {
            argv: vec!["python3".into(), "/opt/bs/probe-codex.py".into(), "--codex".into(),
                "/opt/bs/codex".into(), "--output".into(), work.join("transcript.ndjson").to_string_lossy().into_owned(),
                "--timeout".into(), "120".into(), "--exercise-control".into(), "--approval-policy".into(),
                if approvals { "on-request".into() } else { "never".into() },
                "--sandbox".into(), if approvals { "read-only".into() } else { "danger-full-access".into() }, "--approve".into(),
                "--prompt".into(), "Compatibility test in this disposable directory only. Run a shell command that writes PREFLIGHT_COMMAND_OK to command-proof.txt and reads it back. If permission is needed, request escalation for that command. Then use apply_patch to create proof.txt containing exactly PREFLIGHT_PATCH_OK and a newline. Read the file to verify it. Do nothing else; end with PREFLIGHT_DONE.".into()],
            env: vec![("CODEX_HOME".into(), "/opt/bs/codex-home".into())], cwd: None, pty: None,
        }).await.unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = tokio::spawn(async move {
            let mut text = String::new();
            stderr.read_to_string(&mut text).await.unwrap();
            text
        });
        let status = tokio::time::timeout(Duration::from_secs(140), &mut child.exit).await;
        if status.is_err() { (child.killer)(); }
        let error_text = tokio::time::timeout(Duration::from_secs(5), errors).await;
        (status, error_text)
    }.await;
    handle.shutdown().await.unwrap();
    proxy.stop(&id);
    while let Ok(event) = notices.try_recv() {
        eprintln!(
            "preflight proxy event: {}",
            serde_json::to_string(&event).unwrap()
        );
    }
    assert_eq!(
        result.0.unwrap().unwrap(),
        0,
        "probe stderr: {:?}",
        result.1
    );
    assert_eq!(
        std::fs::read_to_string(work.join("proof.txt")).unwrap(),
        "PREFLIGHT_PATCH_OK\n"
    );
    let transcript = std::fs::read_to_string(work.join("transcript.ndjson")).unwrap();
    assert!(transcript.contains("commandExecution"));
    assert!(transcript.contains("fileChange"));
    assert!(transcript.contains("PREFLIGHT_DONE"));
}
