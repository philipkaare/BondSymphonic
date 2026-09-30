use bondsymphonic_daemon::{
    agents::{
        adapter::AgentAdapter, backend::PreparedAgent, codex::CodexAdapter, AgentEntry, AgentSink,
        TranscriptStore,
    },
    sandbox::{noop::NoopBackend, SandboxBackend, SandboxSpec},
    server::broadcast::EventBus,
};
use bondsymphonic_proto::*;
use std::{sync::Arc, time::Duration};

async fn fixture(
    mode: &str,
    resume: Option<String>,
) -> (
    tempfile::TempDir,
    CodexAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
) {
    let dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceId("codex-test".into());
    let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Codex));
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
    let options: AgentStartOptions =
        serde_json::from_value(serde_json::json!({"resume_session":resume})).unwrap();
    let prepared = PreparedAgent {
        argv: vec![
            if cfg!(windows) { "python" } else { "python3" }.into(),
            format!(
                "{}/tests/fixtures/fake_codex.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            mode.into(),
        ],
        env: vec![],
        cwd: dir.path().into(),
        options,
    };
    (
        dir,
        CodexAdapter::new(sink, handle, prepared).with_timeout(Duration::from_millis(500)),
        entry,
        store,
    )
}

async fn wait_state(entry: &AgentEntry, wanted: AgentState) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while entry.state().0 != wanted {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn starts_resumes_steers_interrupts_and_stops() {
    let (_dir, mut adapter, entry, store) = fixture("normal", Some("saved-thread".into())).await;
    adapter.start().await.unwrap();
    assert_eq!(entry.session_id.lock().as_deref(), Some("saved-thread"));
    adapter.send("first".into()).await.unwrap();
    adapter.send("steer".into()).await.unwrap();
    adapter.interrupt().await.unwrap();
    wait_state(&entry, AgentState::Idle).await;
    adapter.stop().await.unwrap();
    assert_eq!(entry.state().0, AgentState::Exited);
    let messages = store.read(&AgentId("agent".into())).await.unwrap();
    assert!(messages
        .iter()
        .any(|m| matches!(&m.body, AgentMessageBody::AssistantText {text} if text == "steered")));
}

#[tokio::test]
async fn approves_raw_string_ids_once_and_rejects_unknown_ids() {
    let (_dir, mut adapter, entry, _) = fixture("approval", None).await;
    adapter.start().await.unwrap();
    adapter.send("first".into()).await.unwrap();
    wait_state(&entry, AgentState::WaitingPermission).await;
    assert!(adapter
        .permission_reply("unknown".into(), PermissionDecision::Allow, None, None)
        .await
        .is_err());
    let id = "codex:\"server-approval\"".to_owned();
    adapter
        .permission_reply(id.clone(), PermissionDecision::AllowForSession, None, None)
        .await
        .unwrap();
    assert!(adapter
        .permission_reply(id, PermissionDecision::Allow, None, None)
        .await
        .is_err());
    wait_state(&entry, AgentState::Idle).await;
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn answering_one_of_two_codex_approvals_keeps_the_agent_waiting() {
    let (_dir, mut adapter, entry, store) = fixture("doubleapproval", None).await;
    adapter.start().await.unwrap();
    adapter.send("first".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let messages = store.read(&AgentId("agent".into())).await.unwrap();
            if messages
                .iter()
                .filter(|m| matches!(m.body, AgentMessageBody::PermissionRequest { .. }))
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    adapter
        .permission_reply(
            "codex:\"server-approval\"".into(),
            PermissionDecision::Allow,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(entry.state().0, AgentState::WaitingPermission);
    adapter
        .permission_reply(
            "codex:\"second-approval\"".into(),
            PermissionDecision::Deny,
            None,
            None,
        )
        .await
        .unwrap();
    wait_state(&entry, AgentState::Idle).await;
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn initialization_eof_and_timeout_are_bounded_and_end_agent() {
    for mode in ["die", "hang"] {
        let (_dir, mut adapter, entry, _) = fixture(mode, None).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(8), adapter.start())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(entry.state().0, AgentState::Exited);
    }
}

#[tokio::test]
async fn shutdown_kills_a_peer_that_ignores_closed_stdin() {
    let (_dir, mut adapter, entry, _) = fixture("stubborn", None).await;
    adapter.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), adapter.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.state().0, AgentState::Exited);
}

#[tokio::test]
async fn eof_with_approval_outstanding_announces_exit_only_once() {
    let (_dir, mut adapter, entry, _) = fixture("approvaldie", None).await;
    adapter.start().await.unwrap();
    let _ = adapter.send("first".into()).await;
    wait_state(&entry, AgentState::Exited).await;
    let epoch = entry.epoch();
    assert!(adapter
        .permission_reply(
            "codex:\"server-approval\"".into(),
            PermissionDecision::Deny,
            None,
            None
        )
        .await
        .is_err());
    adapter.stop().await.unwrap();
    adapter.stop().await.unwrap();
    assert_eq!(entry.epoch(), epoch);
}

#[tokio::test]
async fn unsupported_server_requests_are_rejected_without_stalling_the_turn() {
    let (_dir, mut adapter, entry, store) = fixture("unsupported", None).await;
    adapter.start().await.unwrap();
    adapter.send("first".into()).await.unwrap();
    wait_state(&entry, AgentState::Idle).await;
    let messages = store.read(&AgentId("agent".into())).await.unwrap();
    assert!(messages.iter().any(|m| matches!(&m.body,
        AgentMessageBody::System { data, .. } if data["method"] == "item/tool/requestUserInput")));
    adapter.stop().await.unwrap();
}
