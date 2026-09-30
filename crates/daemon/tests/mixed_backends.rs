mod common;
use bondsymphonic_daemon::{
    daemon::Daemon,
    sandbox::backend_for,
    server::broadcast::EventBus,
    workspace::{lifecycle, DataDirs},
};
use bondsymphonic_proto::*;
use std::{sync::Arc, time::Duration};

async fn wait_for(
    d: &Arc<Daemon>,
    id: &AgentId,
    predicate: impl Fn(&HistoryResult) -> bool,
) -> HistoryResult {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let history = d
                .agents
                .history(AgentIdParams {
                    agent_id: id.clone(),
                })
                .await
                .unwrap();
            if predicate(&history) {
                return history;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("agent history did not reach the expected state")
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_agents_keep_separate_histories_permissions_and_resume_after_daemon_restart() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let path = |name: &str| fixtures.join(name).display().to_string().replace('\\', "/");
    std::env::set_var(
        "BS_CLAUDE_BIN",
        format!("{python} \"{}\"", path("fake_claude.py")),
    );
    std::env::set_var(
        "FAKE_CLAUDE_FIXTURE",
        path("claude-stream/simple_turn.ndjson"),
    );
    std::env::set_var(
        "BS_CODEX_BIN",
        format!("{python} \"{}\" approval", path("fake_codex.py")),
    );
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let root = dir.path().join("data");
    let d = Daemon::new(
        DataDirs::new(root.clone()),
        backend_for("noop"),
        EventBus::new(256),
    )
    .unwrap();
    let ws = lifecycle::create(
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
    let options = |mode: &str, session: Option<&str>| {
        serde_json::from_value(serde_json::json!({"permission_mode":mode,"resume_session":session,"api_key":"mixed-key-sentinel"})).unwrap()
    };
    let claude = d
        .agents
        .start(
            &d,
            AgentStartParams {
                workspace_id: ws.id.clone(),
                adapter: AgentAdapterKind::Claude,
                options: options("bypassPermissions", None),
            },
        )
        .await
        .unwrap()
        .agent_id;
    let codex = d
        .agents
        .start(
            &d,
            AgentStartParams {
                workspace_id: ws.id.clone(),
                adapter: AgentAdapterKind::Codex,
                options: options("on-request", None),
            },
        )
        .await
        .unwrap()
        .agent_id;
    d.agents
        .send(AgentSendParams {
            agent_id: codex.clone(),
            text: "codex-only".into(),
        })
        .await
        .unwrap();
    let pending = wait_for(&d, &codex, |h| h.state == AgentState::WaitingPermission).await;
    assert!(pending
        .messages
        .iter()
        .any(|m| matches!(&m.body,AgentMessageBody::UserText{text} if text=="codex-only")));
    d.agents
        .send(AgentSendParams {
            agent_id: claude.clone(),
            text: "claude-only".into(),
        })
        .await
        .unwrap();
    wait_for(&d,&claude,|h|h.messages.iter().any(|m|matches!(&m.body,AgentMessageBody::AssistantText{text} if text.contains("claude-only")))).await;
    d.agents
        .permission_reply(AgentPermissionReplyParams {
            agent_id: codex.clone(),
            request_id: "codex:\"server-approval\"".into(),
            decision: PermissionDecision::AllowForSession,
            updated_input: None,
            message: None,
        })
        .await
        .unwrap();
    let codex_history = wait_for(&d, &codex, |h| h.state == AgentState::Idle).await;
    assert!(!codex_history
        .messages
        .iter()
        .any(|m| matches!(&m.body,AgentMessageBody::UserText{text} if text=="claude-only")));
    let session = codex_history
        .messages
        .iter()
        .find_map(|m| match &m.body {
            AgentMessageBody::Result { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap();
    d.agents.stop_all_in(&ws.id).await;
    d.agent_sandboxes.shutdown().await;
    d.sandbox(&ws.id).unwrap().shutdown().await.unwrap();
    let d2 = Daemon::new(
        DataDirs::new(root.clone()),
        backend_for("noop"),
        EventBus::new(256),
    )
    .unwrap();
    d2.restore().await;
    let info = d2.workspace_info(&d2.workspace(&ws.id).unwrap());
    assert_eq!(info.agent_records.len(), 2);
    assert_eq!(info.agent_records[0].adapter, AgentAdapterKind::Claude);
    assert_eq!(info.agent_records[1].adapter, AgentAdapterKind::Codex);
    assert_eq!(
        info.agent_records[1].session_id.as_deref(),
        Some(session.as_str())
    );
    let restored = d2
        .agents
        .history(AgentIdParams { agent_id: codex })
        .await
        .unwrap();
    assert_eq!(restored.messages.len(), codex_history.messages.len());
    assert!(!std::fs::read_to_string(root.join("agents.json"))
        .unwrap()
        .contains("mixed-key-sentinel"));
    let resumed = d2
        .agents
        .start(
            &d2,
            AgentStartParams {
                workspace_id: ws.id.clone(),
                adapter: AgentAdapterKind::Codex,
                options: options("on-request", Some(&session)),
            },
        )
        .await
        .unwrap()
        .agent_id;
    assert_ne!(resumed, claude);
    assert_eq!(
        d2.workspace_info(&d2.workspace(&ws.id).unwrap())
            .agent_records
            .last()
            .unwrap()
            .session_id
            .as_deref(),
        Some(session.as_str())
    );
    lifecycle::destroy(&d2, &ws.id, true).await.unwrap();
    for name in ["BS_CLAUDE_BIN", "BS_CODEX_BIN", "FAKE_CLAUDE_FIXTURE"] {
        std::env::remove_var(name);
    }
}
