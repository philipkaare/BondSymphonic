use bondsymphonic_daemon::{
    agents::{
        adapter::AgentAdapter, backend::PreparedAgent, opencode::OpenCodeAdapter, AgentEntry,
        AgentSink, TranscriptStore,
    },
    sandbox::{noop::NoopBackend, SandboxBackend, SandboxSpec},
    server::broadcast::EventBus,
};
use bondsymphonic_proto::*;
use std::{sync::Arc, time::Duration};

/// The budget for each startup call. It includes starting the fake's Python,
/// which takes seconds on a loaded machine, so only the tests about the budget
/// itself use a short one.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

async fn fixture(
    mode: &str,
    resume: Option<&str>,
) -> (
    tempfile::TempDir,
    OpenCodeAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
) {
    fixture_with(mode, resume, CALL_TIMEOUT).await
}

async fn fixture_with(
    mode: &str,
    resume: Option<&str>,
    timeout: Duration,
) -> (
    tempfile::TempDir,
    OpenCodeAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
) {
    let dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceId("opencode-test".into());
    let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::OpenCode));
    let store = Arc::new(TranscriptStore::new(dir.path().join("transcripts")));
    let sink = AgentSink::new(
        EventBus::new(100),
        store.clone(),
        AgentId("agent".into()),
        ws.clone(),
        entry.clone(),
    );
    let handle = NoopBackend
        .start(&SandboxSpec {
            id: ws,
            rw_binds: vec![],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: dir.path().join("home"),
            run_dir: dir.path().join("run"),
            env: vec![],
            cwd: dir.path().into(),
        })
        .await
        .unwrap();
    let options = serde_json::from_value(
        serde_json::json!({"resume_session":resume,"model":"provider/model"}),
    )
    .unwrap();
    let mut prepared = PreparedAgent {
        argv: vec![
            if cfg!(windows) { "python" } else { "python3" }.into(),
            format!(
                "{}/tests/fixtures/fake_opencode.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            mode.into(),
        ],
        env: vec![("PROVIDER_API_KEY".into(), "secret-value".into())],
        cwd: dir.path().into(),
        options,
    };
    if mode == "sandbox" {
        prepared
            .env
            .push(("BS_OPENCODE_SANDBOX".into(), "1".into()));
    }
    if mode.starts_with("native-") {
        let data = dir.path().join("native-data");
        std::fs::create_dir_all(data.join("opencode")).unwrap();
        std::fs::write(data.join("opencode/auth.json"), r#"{"api":{"type":"api","key":"imported-native-key"},"oauth":{"type":"oauth","access":"imported-native-access","refresh":"imported-native-refresh"}}"#).unwrap();
        prepared
            .env
            .push(("XDG_DATA_HOME".into(), data.display().to_string()));
    }
    if mode.starts_with("config-secret-") {
        let config = dir.path().join("native-config");
        std::fs::create_dir_all(config.join("opencode")).unwrap();
        std::fs::write(
            config.join("opencode/config.json"),
            br#"{"token":"legacy-config-token"}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("opencode/opencode.json"),
            br#"{"provider":{"local":{"options":{"password":"regular-config-password"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("opencode/opencode.jsonc"),
            br#"{
// This URL is a value; it must not be mistaken for a comment.
"provider":{"local":{"options":{
"baseURL":"https://example.test/provider//api",
"api\u004bey" /* key comment */ : /* value comment */ "original\u002djsonc-key",
"client-secret":"original-jsonc-client-secret", // trailing comma follows
}}},
}"#,
        )
        .unwrap();
        prepared
            .env
            .push(("XDG_CONFIG_HOME".into(), config.display().to_string()));
    }
    (
        dir,
        OpenCodeAdapter::new(sink, handle, prepared).with_timeout(timeout),
        entry,
        store,
    )
}

async fn wait_for<F: Fn(&[AgentMessage]) -> bool>(
    store: &TranscriptStore,
    predicate: F,
) -> Vec<AgentMessage> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let messages = store.read(&AgentId("agent".into())).await.unwrap();
            if predicate(&messages) {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn acp_start_resume_stream_tools_and_stop_preserve_session() {
    let (_dir, mut adapter, entry, store) = fixture("normal", Some("saved-session")).await;
    adapter.start().await.unwrap();
    assert_eq!(entry.session_id.lock().as_deref(), Some("saved-session"));
    adapter.send("hello".into()).await.unwrap();
    let messages = wait_for(&store, |m| {
        m.iter()
            .any(|m| matches!(m.body, AgentMessageBody::Result { .. }))
    })
    .await;
    assert!(messages
        .iter()
        .any(|m| matches!(&m.body, AgentMessageBody::AssistantDelta{text} if text == "hello")));
    assert!(!messages
        .iter()
        .any(|m| matches!(&m.body, AgentMessageBody::AssistantDelta{text} if text == "replayed")));
    assert!(messages.iter().any(|m| matches!(&m.body, AgentMessageBody::ToolResult{id,output,is_error:false} if id == "tool" && output == "hi")));
    adapter.stop().await.unwrap();
    let epoch = entry.epoch();
    adapter.stop().await.unwrap();
    assert_eq!(entry.state().0, AgentState::Exited);
    assert_eq!(entry.epoch(), epoch);
}

#[tokio::test]
async fn queued_prompts_do_not_block_overlapping_permissions_and_native_choices() {
    let (_dir, mut adapter, entry, store) = fixture("queue", None).await;
    adapter.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), adapter.send("one".into()))
        .await
        .unwrap()
        .unwrap();
    let messages = wait_for(&store, |m| {
        m.iter()
            .filter(|m| matches!(m.body, AgentMessageBody::PermissionRequest { .. }))
            .count()
            == 2
    })
    .await;
    let ids: Vec<String> = messages
        .iter()
        .filter_map(|m| match &m.body {
            AgentMessageBody::PermissionRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .collect();
    adapter.send("two".into()).await.unwrap();
    adapter
        .permission_reply(
            ids[0].clone(),
            PermissionDecision::AllowForSession,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(entry.state().0, AgentState::WaitingPermission);
    assert!(adapter
        .permission_reply(ids[0].clone(), PermissionDecision::Allow, None, None)
        .await
        .is_err());
    adapter
        .permission_reply(ids[1].clone(), PermissionDecision::Deny, None, None)
        .await
        .unwrap();
    wait_for(&store, |m| {
        m.iter()
            .filter(|m| matches!(m.body, AgentMessageBody::Result { .. }))
            .count()
            == 2
    })
    .await;
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_responds_while_prompt_is_pending_beyond_handshake_timeout() {
    let budget = Duration::from_secs(2);
    // The handshake must fit the short budget under test; on a loaded machine
    // starting Python alone may not, which says nothing about the prompt.
    let mut attempts = 0;
    let (_dir, mut adapter, _entry, store) = loop {
        let (dir, mut adapter, entry, store) = fixture_with("hold", None, budget).await;
        match adapter.start().await {
            Ok(()) => break (dir, adapter, entry, store),
            Err(failure) if failure.message.contains("timed out") && attempts < 3 => attempts += 1,
            Err(failure) => panic!("{failure:?}"),
        }
    };
    adapter.send("one".into()).await.unwrap();
    wait_for(&store, |m| {
        m.iter()
            .any(|m| matches!(&m.body, AgentMessageBody::AssistantDelta{text} if text == "waiting"))
    })
    .await;
    tokio::time::sleep(budget + Duration::from_millis(250)).await;
    adapter.interrupt().await.unwrap();
    wait_for(&store, |m| m.iter().any(|m| matches!(&m.body, AgentMessageBody::System{subtype,data} if subtype == "opencode_turn" && data["stopReason"] == "cancelled"))).await;
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn unsupported_host_rpc_is_refused_and_prompt_errors_are_redacted() {
    for mode in ["unsupported", "error"] {
        let (dir, mut adapter, entry, store) = fixture(mode, None).await;
        adapter.start().await.unwrap();
        adapter.send("one".into()).await.unwrap();
        let messages = wait_for(&store, |m| m.iter().any(|m| matches!(&m.body, AgentMessageBody::System{subtype,..} if subtype == "opencode_turn"))).await;
        assert!(!dir.path().join("HOST-PWNED").exists());
        assert!(!serde_json::to_string(&messages)
            .unwrap()
            .contains("secret-value"));
        if mode == "error" {
            // Published just after the turn's row.
            tokio::time::timeout(Duration::from_secs(10), async {
                while entry.state().0 != AgentState::Error {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        adapter.stop().await.unwrap();
    }
}

#[tokio::test]
async fn startup_rejects_bad_protocol_unsupported_resume_and_hung_peer() {
    for mode in ["version", "no-resume", "die", "hang"] {
        // A hung peer is given up on after the budget, however short.
        let (_dir, mut adapter, entry, _) =
            fixture_with(mode, Some("saved-session"), Duration::from_secs(1)).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(20), adapter.start())
                .await
                .unwrap()
                .is_err(),
            "{mode}"
        );
        assert_eq!(entry.state().0, AgentState::Exited);
    }
}

#[tokio::test]
async fn bounded_stop_kills_peer_ignoring_closed_input() {
    let (_dir, mut adapter, entry, _) = fixture("stubborn", None).await;
    adapter.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), adapter.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.state().0, AgentState::Exited);
}

#[tokio::test]
async fn advertised_resume_and_legacy_model_selection_work() {
    for mode in ["resume-cap", "legacy"] {
        let (_dir, mut adapter, entry, store) = fixture(mode, Some("saved-session")).await;
        adapter.start().await.unwrap();
        assert_eq!(entry.session_id.lock().as_deref(), Some("saved-session"));
        adapter.send("hello".into()).await.unwrap();
        wait_for(&store, |m| {
            m.iter()
                .any(|m| matches!(m.body, AgentMessageBody::Result { .. }))
        })
        .await;
        adapter.stop().await.unwrap();
    }
}

#[tokio::test]
async fn foreign_session_permissions_are_rejected_without_exposing_an_approval() {
    let (_dir, mut adapter, _entry, store) = fixture("foreign", None).await;
    adapter.start().await.unwrap();
    adapter.send("hello".into()).await.unwrap();
    let messages = wait_for(&store, |m| {
        m.iter()
            .any(|m| matches!(m.body, AgentMessageBody::Result { .. }))
    })
    .await;
    assert!(!messages
        .iter()
        .any(|m| matches!(m.body, AgentMessageBody::PermissionRequest { .. })));
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn sandbox_session_receives_the_brokered_git_mcp_bridge() {
    let (_dir, mut adapter, _entry, store) = fixture("sandbox", None).await;
    adapter.start().await.unwrap();
    adapter.send("hello".into()).await.unwrap();
    wait_for(&store, |m| {
        m.iter()
            .any(|m| matches!(m.body, AgentMessageBody::Result { .. }))
    })
    .await;
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn native_api_and_oauth_credentials_never_reach_transcripts_errors_or_stderr() {
    for mode in ["native-tool", "native-error", "native-stderr"] {
        let (_dir, mut adapter, entry, store) = fixture(mode, None).await;
        adapter.start().await.unwrap();
        adapter.send("hello".into()).await.unwrap();
        if mode == "native-stderr" {
            tokio::time::timeout(Duration::from_secs(30), async {
                while entry.state().0 != AgentState::Exited {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            wait_for(&store, |m| m.iter().any(|m| matches!(&m.body, AgentMessageBody::System {subtype,..} if subtype == "opencode_turn"))).await;
        }
        let messages = store.read(&AgentId("agent".into())).await.unwrap();
        let observed = format!(
            "{} {:?}",
            serde_json::to_string(&messages).unwrap(),
            entry.state().1
        );
        for secret in [
            "imported-native-key",
            "imported-native-access",
            "imported-native-refresh",
            "rotated-native-key",
            "rotated-native-access",
            "rotated-native-refresh",
            "not-yet-written-key",
            "not-yet-written-access",
            "not-yet-written-refresh",
        ] {
            assert!(!observed.contains(secret), "{mode} leaked {secret}");
        }
        assert!(
            observed.contains("[redacted]"),
            "{mode}: output was lost instead of redacted"
        );
        adapter.stop().await.unwrap();
    }
}

#[tokio::test]
async fn literal_jsonc_provider_credentials_are_redacted_from_plain_outputs_after_rotation() {
    for mode in [
        "config-secret-tool",
        "config-secret-error",
        "config-secret-stderr",
    ] {
        let (_dir, mut adapter, entry, store) = fixture(mode, None).await;
        adapter.start().await.unwrap();
        adapter.send("hello".into()).await.unwrap();
        if mode == "config-secret-stderr" {
            tokio::time::timeout(Duration::from_secs(30), async {
                while entry.state().0 != AgentState::Exited {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            wait_for(&store,|m|m.iter().any(|m|matches!(&m.body,AgentMessageBody::System{subtype,..}if subtype=="opencode_turn"))).await;
        }
        let observed = format!(
            "{} {:?}",
            serde_json::to_string(&store.read(&AgentId("agent".into())).await.unwrap()).unwrap(),
            entry.state().1
        );
        for secret in [
            "legacy-config-token",
            "regular-config-password",
            "original-jsonc-key",
            "original-jsonc-client-secret",
            "rotated-jsonc-key",
            "rotated-jsonc-client-secret",
        ] {
            assert!(!observed.contains(secret), "{mode} leaked {secret}");
        }
        assert!(
            observed.contains("[redacted]"),
            "{mode} dropped output instead of redacting"
        );
        adapter.stop().await.unwrap();
    }
}

#[cfg(unix)]
#[test]
fn native_secret_refresh_does_not_wait_on_an_agent_replaced_fifo() {
    use bondsymphonic_daemon::agents::acp::AcpSecrets;
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    for auth in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("native");
        std::fs::create_dir_all(base.join("opencode")).unwrap();
        let path = base.join(if auth {
            "opencode/auth.json"
        } else {
            "opencode/opencode.json"
        });
        std::fs::write(&path, br#"{"key":"remembered-key"}"#).unwrap();
        let secrets = AcpSecrets::new(
            &[(
                if auth {
                    "XDG_DATA_HOME"
                } else {
                    "XDG_CONFIG_HOME"
                }
                .into(),
                base.display().to_string(),
            )],
            None,
        );
        std::fs::remove_file(&path).unwrap();
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut frame = serde_json::json!({"message":"remembered-key"});
            secrets.redact(&mut frame);
            sender.send(frame).unwrap();
        });
        let answer = receiver.recv_timeout(Duration::from_secs(1));
        // Always unblock and join the old blocking implementation before the
        // assertion fails, so RED cannot strand a FIFO reader/test process.
        if answer.is_err() {
            let mut writer = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&path)
                .unwrap();
            writer.write_all(b"{}").unwrap();
            drop(writer);
            receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        worker.join().unwrap();
        assert!(
            answer.is_ok(),
            "secret refresh blocked opening an agent-owned FIFO"
        );
        assert_eq!(answer.unwrap()["message"], "[redacted]");
    }
}

#[test]
fn accepted_large_jsonc_configs_have_late_literal_keys_redacted() {
    use bondsymphonic_daemon::agents::{
        acp::AcpSecrets,
        opencode_backend::{opencode_home, seed_native_files},
    };
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("native-config");
    std::fs::create_dir_all(native.join("opencode")).unwrap();
    let config = format!(
        "/*{}*/\n{{\"provider\":{{\"local\":{{\"options\":{{\"apiKey\":\"late-config-key\"}}}}}}}}",
        "padding".repeat(160_000)
    );
    assert!(config.len() > 1024 * 1024);
    std::fs::write(native.join("opencode/opencode.jsonc"), config).unwrap();
    let managed = dir.path().join("managed");
    seed_native_files(&managed, &native, &dir.path().join("data")).unwrap();
    let secrets = AcpSecrets::new(
        &[(
            "XDG_CONFIG_HOME".into(),
            opencode_home(&managed).join("config").display().to_string(),
        )],
        None,
    );
    let mut output = serde_json::json!({"message":"provider diagnostic late-config-key"});
    secrets.redact(&mut output);
    assert_eq!(output["message"], "provider diagnostic [redacted]");
}
