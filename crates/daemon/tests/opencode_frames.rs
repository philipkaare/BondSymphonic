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
        mcp_servers: vec![],
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
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "1 2 3 4 5 ")),
        "{messages:#?}"
    );
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

/// Messages sent while a turn runs wait for it, shown as queued rather than
/// as sent. Interrupting keeps them and sends them in order once the
/// interrupted turn has ended; an interrupt that finds them still queued
/// discards them and says so. A message sent while text is streaming lands
/// after that text, which is finalised first.
#[tokio::test]
async fn interrupt_keeps_queued_messages_and_a_second_discards_them() {
    let mut f = fixture("hold").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("one".into()).await.unwrap();
    f.wait_for(|m| waiting(m) == 1).await;
    f.adapter.send("two".into()).await.unwrap();
    f.adapter.send("three".into()).await.unwrap();
    let messages = f.messages().await;
    let finalised = messages
        .iter()
        .position(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "waiting"))
        .expect("the streaming text is finalised before the next message");
    let queued = queued(&messages);
    assert_eq!(
        queued.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
        ["two", "three"]
    );
    let two = messages
        .iter()
        .position(|m| QueuedPrompt::from_message(m).is_some())
        .unwrap();
    assert!(finalised < two);
    assert_eq!(user_texts(&messages), ["one"], "not sent yet");
    f.adapter.interrupt().await.unwrap();
    // "two" is sent once "one" has ended, and runs.
    let messages = f.wait_for(|m| waiting(m) == 2).await;
    assert_eq!(turn_rows(&messages)[0]["stopReason"], "cancelled");
    assert_eq!(user_texts(&messages), ["one", "two"]);
    assert_eq!(
        dequeued(&messages),
        [(queued[0].id.clone(), DequeueOutcome::Sent)]
    );
    assert_eq!(notices(&messages), Vec::<String>::new());
    // The second interrupt finds "three" still queued: discarded, visibly.
    f.adapter.interrupt().await.unwrap();
    let messages = f.results(2).await;
    assert_eq!(turn_rows(&messages)[1]["stopReason"], "cancelled");
    assert_eq!(
        dequeued(&messages)[1],
        (queued[1].id.clone(), DequeueOutcome::Discarded)
    );
    let notice = structured(&messages).pop().unwrap();
    assert_eq!(notice.kind, NoticeKind::QueueDiscarded);
    assert!(notice.text.contains("“three”"), "{}", notice.text);
    // The fake asserts it never sees overlapping prompts, and `hold` never
    // answers by itself, so another "waiting" means "four" ran on its own.
    f.adapter.send("four".into()).await.unwrap();
    f.wait_for(|m| waiting(m) == 3).await;
    assert_eq!(user_texts(&f.messages().await), ["one", "two", "four"]);
    f.adapter.interrupt().await.unwrap();
    f.results(3).await;
    f.adapter.stop().await.unwrap();
}

fn waiting(messages: &[AgentMessageBody]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, AgentMessageBody::AssistantDelta { text } if text == "waiting"))
        .count()
}

fn user_texts(messages: &[AgentMessageBody]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessageBody::UserText { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn queued(messages: &[AgentMessageBody]) -> Vec<QueuedPrompt> {
    messages
        .iter()
        .filter_map(QueuedPrompt::from_message)
        .collect()
}

fn dequeued(messages: &[AgentMessageBody]) -> Vec<(String, DequeueOutcome)> {
    messages
        .iter()
        .filter_map(DequeuedPrompt::from_message)
        .map(|d| (d.id, d.outcome))
        .collect()
}

fn structured(messages: &[AgentMessageBody]) -> Vec<AgentNotice> {
    messages
        .iter()
        .filter_map(AgentNotice::from_message)
        .collect()
}

/// G401: the empty turn's notice offers the config folder and the log, and
/// the log it carries is OpenCode's, reduced and redacted like the cause.
#[tokio::test]
async fn recorded_empty_turn_notice_offers_the_config_folder_and_the_log() {
    let mut f = fixture("replay:G401").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("hi".into()).await.unwrap();
    let messages = f.results(1).await;
    let notice = structured(&messages).pop().unwrap();
    assert_eq!(notice.kind, NoticeKind::EmptyTurn);
    assert_eq!(
        notice.actions,
        [NoticeAction::OpenConfigFolder, NoticeAction::ShowLog]
    );
    assert!(
        notice
            .log
            .iter()
            .any(|line| line.contains("Incorrect API key provided")),
        "{:?}",
        notice.log
    );
    assert!(
        notice
            .log
            .iter()
            .all(|line| !line.contains("You are opencode")),
        "the logged request body leaked: {:?}",
        notice.log
    );
    f.adapter.stop().await.unwrap();
}

/// G500: the stall notice offers to interrupt, and the log behind it.
#[tokio::test]
async fn recorded_stall_notice_offers_interrupt_and_the_log() {
    let mut f = fixture("replay:G500").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("hi".into()).await.unwrap();
    let messages = f.wait_for(|m| !structured(m).is_empty()).await;
    let notice = structured(&messages).pop().unwrap();
    assert_eq!(notice.kind, NoticeKind::Stall);
    assert_eq!(notice.actions[0], NoticeAction::Interrupt);
    assert!(notice.actions.contains(&NoticeAction::ShowLog));
    assert!(notice.actions.contains(&NoticeAction::OpenConfigFolder));
    assert!(notice.log.iter().any(|line| line.contains("HTTP 500")));
    f.adapter.interrupt().await.unwrap();
    f.results(1).await;
    f.adapter.stop().await.unwrap();
}

/// "Send now" interrupts the turn and sends that message ahead of the ones
/// queued before it; "Discard" drops one. Neither touches a message that is
/// no longer queued.
#[tokio::test]
async fn a_queued_message_can_be_sent_now_or_discarded() {
    let mut f = fixture("hold").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("one".into()).await.unwrap();
    f.wait_for(|m| waiting(m) == 1).await;
    for text in ["two", "three", "four"] {
        f.adapter.send(text.into()).await.unwrap();
    }
    let ids: Vec<String> = queued(&f.messages().await)
        .into_iter()
        .map(|q| q.id)
        .collect();
    f.adapter
        .queued_prompt(ids[2].clone(), QueuedPromptAction::Discard)
        .await
        .unwrap();
    f.adapter
        .queued_prompt(ids[1].clone(), QueuedPromptAction::SendNow)
        .await
        .unwrap();
    let messages = f.wait_for(|m| waiting(m) == 2).await;
    assert_eq!(turn_rows(&messages)[0]["stopReason"], "cancelled");
    assert_eq!(user_texts(&messages), ["one", "three"]);
    assert_eq!(
        dequeued(&messages),
        [
            (ids[2].clone(), DequeueOutcome::Discarded),
            (ids[1].clone(), DequeueOutcome::Sent)
        ]
    );
    // Already settled: refused, and marked as the race it is rather than a
    // failure for the IDE to raise.
    for id in [&ids[1], &ids[2]] {
        let refused = f
            .adapter
            .queued_prompt(id.clone(), QueuedPromptAction::Discard)
            .await
            .unwrap_err();
        assert!(is_queued_prompt_settled(&refused), "{refused:?}");
    }
    // "two" is still queued behind "three", and runs after it.
    f.adapter.interrupt().await.unwrap();
    f.wait_for(|m| waiting(m) == 3).await;
    assert_eq!(user_texts(&f.messages().await), ["one", "three", "two"]);
    f.adapter.interrupt().await.unwrap();
    f.results(3).await;
    f.adapter.stop().await.unwrap();
}

/// Stopping the agent discards what is queued, visibly, before it exits.
#[tokio::test]
async fn stop_discards_queued_messages_visibly() {
    let mut f = fixture("hold").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("one".into()).await.unwrap();
    f.wait_for(|m| waiting(m) == 1).await;
    f.adapter.send("two".into()).await.unwrap();
    f.adapter.stop().await.unwrap();
    let messages = f.messages().await;
    let id = queued(&messages)[0].id.clone();
    assert_eq!(dequeued(&messages), [(id, DequeueOutcome::Discarded)]);
    let notice = structured(&messages).pop().unwrap();
    assert_eq!(notice.kind, NoticeKind::QueueDiscarded);
    assert!(notice.text.contains("“two”"), "{}", notice.text);
    assert_eq!(f.entry.state().0, AgentState::Exited);
}

/// OpenCode's prompt result reports the turn's tokens; a `usage_update`
/// adds the context in use, its size and the session's cost when OpenCode
/// knows a price. A zero cost is a model without a price, not a free one.
#[tokio::test]
async fn turn_rows_carry_opencodes_usage_and_only_a_real_cost() {
    let mut f = fixture("usage").await;
    f.adapter.start().await.unwrap();
    f.adapter.send("priced".into()).await.unwrap();
    f.results(1).await;
    f.adapter.send("unpriced".into()).await.unwrap();
    let messages = f.results(2).await;
    let usages: Vec<TurnUsage> = messages
        .iter()
        .filter_map(TurnUsage::from_turn_row)
        .collect();
    assert_eq!(
        usages[0],
        TurnUsage {
            input_tokens: Some(5),
            output_tokens: Some(3),
            context_tokens: Some(1200),
            context_window: Some(200000),
            session_cost_usd: Some(0.0123),
        }
    );
    assert_eq!(usages[1].session_cost_usd, None);
    assert_eq!(usages[1].context_tokens, Some(1300));
    f.adapter.stop().await.unwrap();
}
