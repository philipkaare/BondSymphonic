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

/// The budget for each startup call. It includes starting the fake's Python,
/// which takes seconds on a loaded machine, so only the test about the budget
/// itself uses a short one.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a test waits for something the fake does promptly. Generous for
/// the same reason; a pass never waits it out.
const WAIT: Duration = Duration::from_secs(30);

async fn fixture(
    mode: &str,
    resume: Option<String>,
) -> (
    tempfile::TempDir,
    CodexAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
) {
    fixture_with(mode, resume, CALL_TIMEOUT).await
}

async fn fixture_with(
    mode: &str,
    resume: Option<String>,
    timeout: Duration,
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
        CodexAdapter::new(sink, handle, prepared).with_timeout(timeout),
        entry,
        store,
    )
}

async fn wait_state(entry: &AgentEntry, wanted: AgentState) {
    tokio::time::timeout(WAIT, async {
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

/// Codex writes a thread's rollout only once its first turn starts, and
/// `thread/resume` of a thread without one fails with "no rollout found". So
/// a new thread is not offered as the session to resume until it has a turn.
#[tokio::test]
async fn a_new_thread_is_reported_as_the_session_only_once_a_turn_starts() {
    let (_dir, mut adapter, entry, _) = fixture("approval", None).await;
    adapter.start().await.unwrap();
    assert_eq!(entry.session_id.lock().as_deref(), None);
    adapter.send("first".into()).await.unwrap();
    wait_state(&entry, AgentState::WaitingPermission).await;
    assert_eq!(entry.session_id.lock().as_deref(), Some("new-thread"));
    adapter.stop().await.unwrap();
}

/// A thread Codex never saved has no history to lose, so resuming one starts
/// a new thread instead of failing the agent.
#[tokio::test]
async fn resuming_a_thread_codex_never_saved_starts_a_new_one() {
    let (_dir, mut adapter, entry, _) = fixture("norollout", Some("unsaved".into())).await;
    adapter.start().await.unwrap();
    assert_eq!(entry.session_id.lock().as_deref(), None);
    adapter.send("first".into()).await.unwrap();
    tokio::time::timeout(WAIT, async {
        while entry.session_id.lock().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(entry.session_id.lock().as_deref(), Some("new-thread"));
    adapter.stop().await.unwrap();
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
    tokio::time::timeout(WAIT, async {
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
        // "hang" is the budget under test: it must end the start, not the
        // outer bound below.
        let (_dir, mut adapter, entry, _) = fixture_with(mode, None, Duration::from_secs(1)).await;
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

/// Codex's cumulative `thread/tokenUsage/updated` reaches the transcript as
/// each turn's own tokens and the context it left, on the turn row before the
/// `Result`; no cost is invented.
#[tokio::test]
async fn each_turn_row_carries_that_turns_tokens_and_context() {
    let (_dir, mut adapter, _entry, store) = fixture("usage", None).await;
    adapter.start().await.unwrap();
    let mut usages = vec![];
    for (n, prompt) in ["first", "second"].into_iter().enumerate() {
        adapter.send(prompt.into()).await.unwrap();
        let messages = tokio::time::timeout(WAIT, async {
            loop {
                let messages = store.read(&AgentId("agent".into())).await.unwrap();
                if messages
                    .iter()
                    .filter(|m| matches!(m.body, AgentMessageBody::Result { .. }))
                    .count()
                    > n
                {
                    return messages;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        usages = messages
            .iter()
            .filter_map(|m| TurnUsage::from_turn_row(&m.body))
            .collect::<Vec<_>>();
    }
    assert_eq!(usages.len(), 2, "{usages:?}");
    for usage in &usages {
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.context_window, Some(258400));
        assert_eq!(usage.session_cost_usd, None);
    }
    assert_eq!(usages[1].context_tokens, Some(2460));
    adapter.stop().await.unwrap();
}

#[tokio::test]
async fn slash_commands_use_native_compaction_and_local_status() {
    let (_dir, mut agent, entry, store) = fixture("commands", None).await;
    agent.start().await.unwrap();
    agent.send("/compact".into()).await.unwrap();
    agent.send("/status".into()).await.unwrap();
    assert!(agent.send("/compact unexpected".into()).await.is_err());
    assert_eq!(entry.state().0, AgentState::Idle);
    let rows = store.read(&AgentId("agent".into())).await.unwrap();
    assert!(rows.iter().any(|row| matches!(&row.body,
        AgentMessageBody::System { subtype, data }
        if subtype == "agent_commands" && data["commands"][0]["name"] == "compact")));
    assert_eq!(rows.iter().filter(|row| matches!(&row.body,
        AgentMessageBody::System { subtype, .. } if subtype == "agent_command_result")).count(), 2);
    agent.stop().await.unwrap();
}
