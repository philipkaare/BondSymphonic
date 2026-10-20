//! The OpenCode adapter against frames recorded from OpenCode 1.14.48
//! (`fixtures/opencode/probe-1.14.48`), replayed by `fake_opencode.py`.
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

struct Fixture {
    _dir: tempfile::TempDir,
    adapter: OpenCodeAdapter,
    entry: Arc<AgentEntry>,
    store: Arc<TranscriptStore>,
}

async fn fixture(mode: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceId("opencode-frames".into());
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
    let prepared = PreparedAgent {
        argv: vec![
            if cfg!(windows) { "python" } else { "python3" }.into(),
            format!(
                "{}/tests/fixtures/fake_opencode.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            mode.into(),
        ],
        env: vec![],
        cwd: dir.path().into(),
        options: serde_json::from_value(serde_json::json!({"model":"provider/model"})).unwrap(),
    };
    Fixture {
        adapter: OpenCodeAdapter::new(sink, handle, prepared)
            .with_timeout(Duration::from_secs(15))
            .with_stall_after(Duration::from_millis(400)),
        _dir: dir,
        entry,
        store,
    }
}

impl Fixture {
    async fn messages(&self) -> Vec<AgentMessageBody> {
        self.store
            .read(&AgentId("agent".into()))
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect()
    }

    async fn wait_for(
        &self,
        predicate: impl Fn(&[AgentMessageBody]) -> bool,
    ) -> Vec<AgentMessageBody> {
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let messages = self.messages().await;
                if predicate(&messages) {
                    return messages;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        match waited {
            Ok(messages) => messages,
            Err(_) => panic!(
                "timed out in state {:?}; transcript: {:#?}",
                self.entry.state(),
                self.messages().await
            ),
        }
    }

    /// The state is published just after the turn's last row.
    async fn wait_state(&self, state: AgentState) {
        let reached = tokio::time::timeout(Duration::from_secs(10), async {
            while self.entry.state().0 != state {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(reached.is_ok(), "never {state:?}: {:?}", self.entry.state());
    }

    async fn results(&self, n: usize) -> Vec<AgentMessageBody> {
        self.wait_for(|m| {
            m.iter()
                .filter(|m| matches!(m, AgentMessageBody::Result { .. }))
                .count()
                >= n
        })
        .await
    }
}

fn notices(messages: &[AgentMessageBody]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessageBody::System { subtype, data } if subtype == "opencode_notice" => {
                data["text"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}

fn turn_rows(messages: &[AgentMessageBody]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessageBody::System { subtype, data } if subtype == "opencode_turn" => {
                Some(data.clone())
            }
            _ => None,
        })
        .collect()
}

/// C2: a bash call whose input only arrives after the first `tool_call`. The
/// card and the permission request both show that input, and the reply ends
/// as finalised text.
#[tokio::test]
async fn recorded_tool_call_shows_its_input_and_the_reply_is_finalised() {
    let mut f = fixture("replay:C2").await;
    f.adapter.start().await.unwrap();
    f.adapter
        .send("Run ls -la with bash, then say DONE.".into())
        .await
        .unwrap();
    let messages = f
        .wait_for(|m| {
            m.iter()
                .any(|m| matches!(m, AgentMessageBody::PermissionRequest { .. }))
        })
        .await;
    let input = serde_json::json!({"command":"ls -la","description":"List files"});
    let request = messages
        .iter()
        .find_map(|m| match m {
            AgentMessageBody::PermissionRequest {
                request_id,
                tool_name,
                input: shown,
                ..
            } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(shown, &input);
                Some(request_id.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(messages.iter().any(
        |m| matches!(m, AgentMessageBody::ToolUse { id, input: shown, .. } if id == "call_1" && shown == &input)
    ));
    assert_eq!(f.entry.state().0, AgentState::WaitingPermission);
    f.adapter
        .permission_reply(request, PermissionDecision::Allow, None, None)
        .await
        .unwrap();
    let messages = f.results(1).await;
    assert!(messages.iter().any(|m| matches!(m, AgentMessageBody::ToolResult { id, output, is_error: false } if id == "call_1" && output.starts_with("total 12\n"))));
    assert!(messages
        .iter()
        .any(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "DONE")));
    assert_eq!(notices(&messages), Vec::<String>::new());
    f.wait_state(AgentState::Idle).await;
    f.adapter.stop().await.unwrap();
}

/// C: a refused tool is an error with OpenCode's message, and cancelling a
/// streaming turn -- which OpenCode reports as an ordinary `end_turn`, after
/// streaming a little more -- is reported as cancelled.
#[tokio::test]
async fn recorded_refusal_is_an_error_and_a_cancelled_turn_says_so() {
    let mut f = fixture("replay:C").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("run ls".into()).await.unwrap();
    let messages = f
        .wait_for(|m| {
            m.iter()
                .any(|m| matches!(m, AgentMessageBody::PermissionRequest { .. }))
        })
        .await;
    let request = messages
        .iter()
        .find_map(|m| match m {
            AgentMessageBody::PermissionRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .unwrap();
    f.adapter
        .permission_reply(request, PermissionDecision::Deny, None, None)
        .await
        .unwrap();
    let messages = f.results(1).await;
    assert!(messages.iter().any(|m| matches!(m, AgentMessageBody::ToolResult { output, is_error: true, .. } if output == "The user rejected permission to use this specific tool call.")));
    f.adapter.send("say DONE".into()).await.unwrap();
    f.results(2).await;
    f.adapter.send("count".into()).await.unwrap();
    f.wait_for(|m| {
        m.iter()
            .any(|m| matches!(m, AgentMessageBody::AssistantDelta { text } if text == "3 "))
    })
    .await;
    f.adapter.interrupt().await.unwrap();
    let messages = f.results(3).await;
    let turns = turn_rows(&messages);
    assert_eq!(turns[0]["stopReason"], "end_turn");
    assert_eq!(turns[2]["stopReason"], "cancelled");
    assert!(messages
        .iter()
        .any(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "1 2 3 4 5 ")));
    // Neither the refusal nor the cancel is mistaken for a silent model.
    assert_eq!(notices(&messages), Vec::<String>::new());
    f.wait_state(AgentState::Idle).await;
    f.adapter.stop().await.unwrap();
}

/// G401, B and A: a rejected key, the free tier's version refusal and an
/// unusable model all end the prompt with an empty `end_turn`. The user is
/// told, with the cause OpenCode logged when there is one.
#[tokio::test]
async fn recorded_empty_turns_are_explained() {
    for (scenario, cause) in [
        ("G401", Some("Incorrect API key provided")),
        (
            "B",
            Some("OpenCode 1.18.0 or newer is required to use the free tier"),
        ),
        ("A", None),
    ] {
        let mut f = fixture(&format!("replay:{scenario}")).await;
        f.adapter.start().await.unwrap();
        f.adapter.send("hi".into()).await.unwrap();
        let messages = f.results(1).await;
        let notices = notices(&messages);
        assert_eq!(notices.len(), 1, "{scenario}: {notices:?}");
        assert!(
            notices[0].contains("returned nothing"),
            "{scenario}: {notices:?}"
        );
        if let Some(cause) = cause {
            assert!(notices[0].contains(cause), "{scenario}: {notices:?}");
        }
        assert!(
            !notices[0].contains("You are opencode"),
            "{scenario}: the logged request body leaked: {notices:?}"
        );
        f.wait_state(AgentState::Idle).await;
        f.adapter.stop().await.unwrap();
    }
}

/// G500: OpenCode retries a failing provider forever and the prompt never
/// answers. The silence is flagged with the logged cause, without ending the
/// turn, and an interrupt still ends it as cancelled.
#[tokio::test]
async fn recorded_endless_retry_is_flagged_and_can_be_interrupted() {
    let mut f = fixture("replay:G500").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("hi".into()).await.unwrap();
    let messages = f.wait_for(|m| !notices(m).is_empty()).await;
    let notice = &notices(&messages)[0];
    assert!(notice.contains("No response from the model"), "{notice}");
    assert!(notice.contains("HTTP 500"), "{notice}");
    assert!(notice.contains("Incorrect API key provided"), "{notice}");
    assert!(!messages
        .iter()
        .any(|m| matches!(m, AgentMessageBody::Result { .. })));
    assert_eq!(f.entry.state().0, AgentState::Working);
    // One notice per silence, not one per check.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(notices(&f.messages().await).len(), 1);
    f.adapter.interrupt().await.unwrap();
    let messages = f.results(1).await;
    assert_eq!(turn_rows(&messages)[0]["stopReason"], "cancelled");
    assert_eq!(notices(&messages).len(), 1, "a cancelled turn is not empty");
    f.wait_state(AgentState::Idle).await;
    f.adapter.stop().await.unwrap();
}

/// Messages sent while a turn runs wait for it. Interrupting discards them,
/// says so, and a message sent afterwards still runs. A message sent while
/// text is streaming lands after that text, which is finalised first.
#[tokio::test]
async fn interrupt_discards_queued_messages_visibly() {
    let mut f = fixture("hold").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("one".into()).await.unwrap();
    f.wait_for(|m| {
        m.iter()
            .any(|m| matches!(m, AgentMessageBody::AssistantDelta { text } if text == "waiting"))
    })
    .await;
    f.adapter.send("two".into()).await.unwrap();
    f.adapter.send("three".into()).await.unwrap();
    let messages = f.messages().await;
    let finalised = messages
        .iter()
        .position(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "waiting"))
        .expect("the streaming text is finalised before the next message");
    let two = messages
        .iter()
        .position(|m| matches!(m, AgentMessageBody::UserText { text } if text == "two"))
        .unwrap();
    assert!(finalised < two);
    f.adapter.interrupt().await.unwrap();
    let messages = f.results(1).await;
    let notices = notices(&messages);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("2 queued messages were not sent"),
        "{notices:?}"
    );
    assert_eq!(turn_rows(&messages)[0]["stopReason"], "cancelled");
    // The fake asserts it never sees overlapping prompts, and `hold` never
    // answers by itself, so a second Result means "four" ran on its own.
    f.adapter.send("four".into()).await.unwrap();
    f.wait_for(|m| {
        m.iter()
            .filter(|m| matches!(m, AgentMessageBody::AssistantDelta { text } if text == "waiting"))
            .count()
            == 2
    })
    .await;
    f.adapter.interrupt().await.unwrap();
    f.results(2).await;
    f.adapter.stop().await.unwrap();
}
