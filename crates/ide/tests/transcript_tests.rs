//! The transcript model: pure Rust, no Qt. Everything the `TranscriptModel`
//! QObject exposes to the view is decided here, so these tests are what the
//! transcript view's behaviour actually rests on.

use bondsymphonic_ide::model::transcript::{
    decide_permission, tool_summary, Applied, LiveEvent, PendingPermission, TabAgentState,
    Transcript, TranscriptItem,
};
use bondsymphonic_ide::qobjects::transcript_model::{carry_always_allow, restart_options_with};
use bondsymphonic_proto::{AgentMessage, AgentMessageBody, AgentState, PermissionDecision};
use serde_json::json;
use std::collections::BTreeSet;

fn msg(seq: u64, body: AgentMessageBody) -> AgentMessage {
    AgentMessage {
        seq,
        ts: "2026-09-09T10:00:00Z".to_owned(),
        body,
    }
}

fn delta(seq: u64, text: &str) -> AgentMessage {
    msg(
        seq,
        AgentMessageBody::AssistantDelta {
            text: text.to_owned(),
        },
    )
}

#[test]
fn codex_completion_preserves_session_without_fabricating_a_cost_row() {
    let mut t = Transcript::default();
    t.apply(&delta(1, "hello"));
    t.apply(&msg(
        2,
        AgentMessageBody::AssistantText {
            text: "hello".into(),
        },
    ));
    t.apply(&msg(
        3,
        AgentMessageBody::System {
            subtype: "codex_turn".into(),
            data: json!({"status":"completed","cost_available":false}),
        },
    ));
    t.apply(&msg(
        4,
        AgentMessageBody::Result {
            cost_usd: 0.0,
            duration_ms: 12,
            num_turns: 1,
            session_id: "thread".into(),
        },
    ));
    assert_eq!(t.session_id.as_deref(), Some("thread"));
    assert_eq!(
        t.items
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .count(),
        1
    );
    assert!(!t
        .items
        .iter()
        .any(|item| matches!(item, TranscriptItem::Result { .. })));
}

/// Partial messages arrive as deltas and are then repeated in full. One
/// assistant bubble must come out of that, not three.
#[test]
fn deltas_coalesce_into_one_finished_assistant_item() {
    let mut t = Transcript::default();
    assert_eq!(t.apply(&delta(1, "Hel")), Applied::Appended(0));
    assert_eq!(t.apply(&delta(2, "lo")), Applied::Changed(0));
    assert!(matches!(
        &t.items[0],
        TranscriptItem::Assistant { text, streaming: true } if text == "Hello"
    ));
    let done = msg(
        3,
        AgentMessageBody::AssistantText {
            text: "Hello".to_owned(),
        },
    );
    assert_eq!(t.apply(&done), Applied::Changed(0));
    assert_eq!(t.items.len(), 1);
    assert!(matches!(
        &t.items[0],
        TranscriptItem::Assistant { text, streaming: false } if text == "Hello"
    ));
}

/// An assistant message with no deltas before it is still one item, and a
/// second finished message after it does not merge into the first.
#[test]
fn assistant_text_without_deltas_appends() {
    let mut t = Transcript::default();
    let first = msg(
        1,
        AgentMessageBody::AssistantText {
            text: "one".to_owned(),
        },
    );
    let second = msg(
        2,
        AgentMessageBody::AssistantText {
            text: "two".to_owned(),
        },
    );
    assert_eq!(t.apply(&first), Applied::Appended(0));
    assert_eq!(t.apply(&second), Applied::Appended(1));
    assert_eq!(t.items.len(), 2);
}

#[test]
fn tool_results_attach_to_their_use_and_unknown_ids_become_system_items() {
    let mut t = Transcript::default();
    let use_a = msg(
        1,
        AgentMessageBody::ToolUse {
            id: "tu_a".to_owned(),
            name: "Bash".to_owned(),
            input: json!({ "command": "cargo test" }),
        },
    );
    let use_b = msg(
        2,
        AgentMessageBody::ToolUse {
            id: "tu_b".to_owned(),
            name: "Read".to_owned(),
            input: json!({ "file_path": "/repo/src/lib.rs" }),
        },
    );
    assert_eq!(t.apply(&use_a), Applied::Appended(0));
    assert_eq!(t.apply(&use_b), Applied::Appended(1));
    // Collapsed by default, and summarised from the input.
    assert!(matches!(
        &t.items[0],
        TranscriptItem::ToolUse { summary, collapsed: true, result: None, .. } if summary == "cargo test"
    ));

    let result_a = msg(
        3,
        AgentMessageBody::ToolResult {
            id: "tu_a".to_owned(),
            output: "ok".to_owned(),
            is_error: false,
        },
    );
    assert_eq!(t.apply(&result_a), Applied::Changed(0));
    assert!(matches!(
        &t.items[0],
        TranscriptItem::ToolUse { result: Some(out), is_error: false, .. } if out == "ok"
    ));

    let orphan = msg(
        4,
        AgentMessageBody::ToolResult {
            id: "tu_zzz".to_owned(),
            output: "boom".to_owned(),
            is_error: true,
        },
    );
    assert_eq!(t.apply(&orphan), Applied::Appended(2));
    assert!(matches!(
        &t.items[2],
        TranscriptItem::System { text } if text.contains("unknown id")
    ));
}

/// History replay and live events overlap: the same `seq` must not be applied
/// twice, or a replayed turn would be doubled in the view.
#[test]
fn duplicate_and_out_of_order_seqs_are_ignored() {
    let mut t = Transcript::default();
    assert_eq!(
        t.apply(&msg(
            7,
            AgentMessageBody::UserText {
                text: "hi".to_owned()
            }
        )),
        Applied::Appended(0)
    );
    // The same message again, and an older one, change nothing.
    assert_eq!(
        t.apply(&msg(
            7,
            AgentMessageBody::UserText {
                text: "hi".to_owned()
            }
        )),
        Applied::Nothing
    );
    assert_eq!(
        t.apply(&msg(
            3,
            AgentMessageBody::UserText {
                text: "older".to_owned()
            }
        )),
        Applied::Nothing
    );
    assert_eq!(t.items.len(), 1);
}

/// A daemon whose first message carries `seq: 0` must not have it swallowed by
/// the duplicate check.
#[test]
fn a_first_message_with_seq_zero_is_applied() {
    let mut t = Transcript::default();
    assert_eq!(
        t.apply(&msg(
            0,
            AgentMessageBody::UserText {
                text: "first".to_owned()
            }
        )),
        Applied::Appended(0)
    );
    assert_eq!(
        t.apply(&msg(
            0,
            AgentMessageBody::UserText {
                text: "first".to_owned()
            }
        )),
        Applied::Nothing
    );
}

#[test]
fn result_messages_accumulate_cost_and_turns() {
    let mut t = Transcript::default();
    let turn = |seq: u64, cost: f64, turns: u32| {
        msg(
            seq,
            AgentMessageBody::Result {
                cost_usd: cost,
                duration_ms: 1200,
                num_turns: turns,
                session_id: "sess_1".to_owned(),
            },
        )
    };
    assert_eq!(t.apply(&turn(1, 0.0042, 1)), Applied::Appended(0));
    assert_eq!(t.apply(&turn(2, 0.0021, 2)), Applied::Appended(1));
    assert!((t.cost_usd - 0.0063).abs() < 1e-9, "{}", t.cost_usd);
    assert_eq!(t.turns, 3);
    assert_eq!(t.session_id.as_deref(), Some("sess_1"));
}

#[test]
fn an_init_system_message_records_the_session_and_summarises_itself() {
    let mut t = Transcript::default();
    let init = msg(
        1,
        AgentMessageBody::System {
            subtype: "init".to_owned(),
            data: json!({ "session_id": "sess_9", "model": "claude-opus-5" }),
        },
    );
    assert_eq!(t.apply(&init), Applied::Appended(0));
    assert_eq!(t.session_id.as_deref(), Some("sess_9"));
    let TranscriptItem::System { text } = &t.items[0] else {
        panic!("expected a system item, got {:?}", t.items[0]);
    };
    assert!(text.contains("sess_9"), "{text}");
    assert!(text.contains("claude-opus-5"), "{text}");

    // Anything else keeps its subtype and a bounded rendering of its data.
    let other = msg(
        2,
        AgentMessageBody::System {
            subtype: "raw".to_owned(),
            data: json!({ "blob": "x".repeat(500) }),
        },
    );
    assert_eq!(t.apply(&other), Applied::Appended(1));
    let TranscriptItem::System { text } = &t.items[1] else {
        panic!("expected a system item");
    };
    assert!(text.starts_with("raw: "), "{text}");
    assert!(text.chars().count() <= 5 + 200, "{}", text.chars().count());

    // Spaced, so the line wraps between fields instead of being one word as
    // wide as the whole of its data.
    let spaced = msg(
        3,
        AgentMessageBody::System {
            subtype: "raw".to_owned(),
            data: json!({ "a": 1, "b": [2, 3] }),
        },
    );
    assert_eq!(t.apply(&spaced), Applied::Appended(2));
    let TranscriptItem::System { text } = &t.items[2] else {
        panic!("expected a system item");
    };
    assert_eq!(text, r#"raw: {"a": 1, "b": [2, 3]}"#);
}

#[test]
fn a_permission_request_sets_pending_and_leaving_the_state_clears_it() {
    let mut t = Transcript::default();
    let req = msg(
        1,
        AgentMessageBody::PermissionRequest {
            request_id: "req_1".to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({ "command": "rm -rf /" }),
            suggestions: Vec::new(),
        },
    );
    // No item: the permission bar shows it, not the transcript.
    assert_eq!(t.apply(&req), Applied::Nothing);
    assert!(t.items.is_empty());
    let pending = t.pending.clone().expect("pending permission");
    assert_eq!(
        pending,
        PendingPermission {
            request_id: "req_1".to_owned(),
            tool_name: "Bash".to_owned(),
            summary: "rm -rf /".to_owned(),
            input_json: pending.input_json.clone(),
        }
    );
    assert!(pending.input_json.contains("rm -rf /"), "{pending:?}");

    // Staying in WaitingPermission keeps it; leaving clears it.
    t.set_state(AgentState::WaitingPermission, None);
    assert!(t.pending.is_some());
    t.set_state(AgentState::Idle, None);
    assert!(t.pending.is_none());
    assert_eq!(t.state, TabAgentState::Known(AgentState::Idle));
    assert!(t.state_detail.is_empty());

    t.set_state(AgentState::Error, Some("exit code 1".to_owned()));
    assert_eq!(t.state_detail, "exit code 1");
}

#[test]
fn decide_permission_answers_only_for_always_allowed_tools() {
    // The set is the pane's, not the transcript's: a reconnect rebuilds the
    // transcript and must not cancel a pre-approval the user has given.
    let mut always_allow = BTreeSet::new();
    assert_eq!(decide_permission(&always_allow, "Bash"), None);
    always_allow.insert("Bash".to_owned());
    assert_eq!(
        decide_permission(&always_allow, "Bash"),
        Some(PermissionDecision::Allow)
    );
    assert_eq!(decide_permission(&always_allow, "Write"), None);
}

#[test]
fn tool_summaries_pick_the_field_that_identifies_the_call() {
    assert_eq!(
        tool_summary(
            "Edit",
            &json!({ "file_path": "/repo/a.rs", "old_string": "x" })
        ),
        "/repo/a.rs"
    );
    assert_eq!(
        tool_summary("Write", &json!({ "file_path": "/repo/b.rs" })),
        "/repo/b.rs"
    );
    assert_eq!(
        tool_summary("Bash", &json!({ "command": "cargo test\n--all" })),
        "cargo test"
    );
    let long = "e".repeat(200);
    let summary = tool_summary("Bash", &json!({ "command": long }));
    assert_eq!(summary.chars().count(), 80);
    assert_eq!(
        tool_summary("Grep", &json!({ "pattern": "fn main" })),
        "fn main"
    );
    assert_eq!(
        tool_summary("Glob", &json!({ "pattern": "**/*.rs" })),
        "**/*.rs"
    );
    // Unknown tool: the first string-valued field, else the tool's own name.
    assert_eq!(
        tool_summary("Sparkle", &json!({ "n": 3, "target": "the moon" })),
        "the moon"
    );
    assert_eq!(tool_summary("Sparkle", &json!({ "n": 3 })), "Sparkle");
    assert_eq!(tool_summary("Sparkle", &json!(null)), "Sparkle");
}

/// The view rebuilds itself from this JSON, dispatching on `kind`.
#[test]
fn items_json_tags_every_item_with_its_kind() {
    let mut t = Transcript::default();
    t.apply(&msg(
        1,
        AgentMessageBody::UserText {
            text: "go".to_owned(),
        },
    ));
    t.apply(&msg(
        2,
        AgentMessageBody::ToolUse {
            id: "tu_a".to_owned(),
            name: "Bash".to_owned(),
            input: json!({ "command": "ls" }),
        },
    ));
    t.apply(&msg(
        3,
        AgentMessageBody::AssistantText {
            text: "done".to_owned(),
        },
    ));
    let items: Vec<serde_json::Value> = serde_json::from_str(&t.items_json()).unwrap();
    let kinds: Vec<&str> = items
        .iter()
        .map(|i| i["kind"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(kinds, vec!["user", "tool_use", "assistant"]);
    assert_eq!(items[1]["name"], "Bash");
    assert_eq!(items[1]["collapsed"], true);

    let one: serde_json::Value = serde_json::from_str(&t.item_json(2)).unwrap();
    assert_eq!(one["kind"], "assistant");
    assert_eq!(one["text"], "done");
    // Out of range is empty rather than a panic: the view asks by index.
    assert!(t.item_json(99).is_empty());
}

/// The bug the replay design walks straight into: history ends on an
/// unanswered permission request, and a `Working` state buffered from *before*
/// that snapshot is applied after it. Clearing `pending` on the arriving state
/// would drop the request, and `seq` de-duplication means the same request can
/// never set it again, so the tab would say it is waiting with no bar and the
/// agent would block forever.
#[test]
fn a_stale_state_event_does_not_drop_a_pending_permission() {
    let mut t = Transcript::default();
    let request = msg(
        5,
        AgentMessageBody::PermissionRequest {
            request_id: "req_1".to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({ "command": "ls" }),
            suggestions: Vec::new(),
        },
    );
    t.apply(&request);
    assert!(t.pending.is_some());

    // A state the agent was in before the request. It is not leaving
    // WaitingPermission, because it never got there.
    t.set_state(AgentState::Working, None);
    assert!(
        t.pending.is_some(),
        "a state the agent left before the request must not clear it"
    );
    // The same request again is a duplicate and cannot restore it.
    assert_eq!(t.apply(&request), Applied::Nothing);
    assert!(t.pending.is_some());

    // Only actually leaving WaitingPermission clears it.
    t.set_state(AgentState::WaitingPermission, None);
    assert!(t.pending.is_some());
    t.set_state(AgentState::Idle, None);
    assert!(t.pending.is_none());
}

/// The whole replay decision in one call: history first, then whatever arrived
/// while the history request was in flight, with the overlap de-duplicated.
#[test]
fn replay_applies_history_then_live_and_de_duplicates_the_overlap() {
    let mut t = Transcript::default();
    let history = vec![
        msg(
            1,
            AgentMessageBody::UserText {
                text: "go".to_owned(),
            },
        ),
        msg(
            2,
            AgentMessageBody::AssistantText {
                text: "on it".to_owned(),
            },
        ),
    ];
    let live = vec![
        // Already in the history snapshot: seq at or below its last.
        LiveEvent::Message(msg(
            2,
            AgentMessageBody::AssistantText {
                text: "on it".to_owned(),
            },
        )),
        LiveEvent::State(AgentState::Working, None),
        LiveEvent::Message(msg(
            3,
            AgentMessageBody::AssistantText {
                text: "done".to_owned(),
            },
        )),
        LiveEvent::State(AgentState::Idle, Some("finished".to_owned())),
    ];
    let applied = t.replay(&history, &live);
    assert_eq!(
        applied,
        vec![
            Applied::Appended(0),
            Applied::Appended(1),
            // The overlap.
            Applied::Nothing,
            // A state change moves no item.
            Applied::Nothing,
            Applied::Appended(2),
            Applied::Nothing,
        ]
    );
    assert_eq!(t.items.len(), 3, "{:?}", t.items);
    assert_eq!(t.state, TabAgentState::Known(AgentState::Idle));
    assert_eq!(t.state_detail, "finished");
}

/// A request for a tool the user pre-approved is taken, not published: the
/// permission bar must never show one the session has already answered.
#[test]
fn replay_leaves_an_always_allowed_request_to_be_auto_answered() {
    let mut t = Transcript::default();
    let always_allow = BTreeSet::from(["Read".to_owned()]);
    // The state `agent.history` came back with, applied before the fold the way
    // the model does it. Without it the request would be folded and then
    // dropped: an agent that is not waiting has nothing to be answered.
    t.set_state(AgentState::WaitingPermission, None);
    let history = vec![msg(
        1,
        AgentMessageBody::PermissionRequest {
            request_id: "req_7".to_owned(),
            tool_name: "Read".to_owned(),
            input: json!({ "file_path": "/repo/a.rs" }),
            suggestions: Vec::new(),
        },
    )];
    t.replay(&history, &[]);
    // Still pending at this point: `replay` folds, the caller decides.
    assert!(t.pending.is_some());

    let (taken, decision) = t.take_auto_allowed(&always_allow).expect("auto-answered");
    assert_eq!(taken.request_id, "req_7");
    assert_eq!(taken.summary, "/repo/a.rs");
    assert_eq!(decision, PermissionDecision::Allow);
    assert!(
        t.pending.is_none(),
        "an auto-answered request must not reach the bar"
    );
    // Nothing left to take, and a tool that is not on the list is left alone.
    assert!(t.take_auto_allowed(&always_allow).is_none());

    let mut other = Transcript::default();
    other.apply(&msg(
        1,
        AgentMessageBody::PermissionRequest {
            request_id: "req_8".to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({ "command": "rm -rf /" }),
            suggestions: Vec::new(),
        },
    ));
    assert!(other.take_auto_allowed(&always_allow).is_none());
    assert!(other.pending.is_some(), "an unlisted tool still asks");
}

/// One permission request, and everything that can answer it.
fn request(seq: u64, request_id: &str) -> AgentMessage {
    msg(
        seq,
        AgentMessageBody::PermissionRequest {
            request_id: request_id.to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({ "command": "ls -la" }),
            suggestions: Vec::new(),
        },
    )
}

fn reply(seq: u64, request_id: &str, decision: &str) -> AgentMessage {
    msg(
        seq,
        AgentMessageBody::System {
            subtype: "permission_reply".to_owned(),
            data: json!({ "request_id": request_id, "decision": decision }),
        },
    )
}

#[test]
fn overlapping_codex_approvals_remain_answerable_in_order() {
    let mut t = Transcript::default();
    t.set_state(AgentState::WaitingPermission, None);
    t.apply(&request(1, "codex:a"));
    t.apply(&request(2, "codex:b"));
    assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:a");
    t.apply(&reply(3, "codex:a", "allow"));
    assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:b");
    t.apply(&msg(
        4,
        AgentMessageBody::ToolUse {
            id: "tool-a".into(),
            name: "Bash".into(),
            input: json!({"command":"ls"}),
        },
    ));
    assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:b");
    t.apply(&reply(5, "codex:b", "deny"));
    assert!(t.pending.is_none());
}

#[test]
fn queued_codex_replies_and_local_acknowledgments_do_not_hide_other_requests() {
    let mut t = Transcript::default();
    t.apply(&request(1, "codex:a"));
    t.apply(&request(2, "codex:b"));
    t.apply(&reply(3, "codex:b", "deny"));
    assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:a");
    t.apply(&request(4, "codex:c"));
    t.answer_permission("codex:a");
    t.apply(&reply(5, "codex:a", "allow"));
    assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:c");
    t.answer_permission("codex:c");
    assert!(t.pending.is_none());
}

#[test]
fn ending_a_turn_discards_all_queued_codex_approvals() {
    for exit in [false, true] {
        let mut t = Transcript::default();
        t.set_state(AgentState::WaitingPermission, None);
        t.apply(&request(1, "codex:a"));
        t.apply(&request(2, "codex:b"));
        if exit {
            t.set_state(AgentState::Exited, None);
        } else {
            t.apply(&msg(
                3,
                AgentMessageBody::Result {
                    cost_usd: 0.0,
                    duration_ms: 1,
                    num_turns: 1,
                    session_id: "thread".into(),
                },
            ));
        }
        t.apply(&request(4, "codex:c"));
        assert_eq!(t.pending.as_ref().unwrap().request_id, "codex:c");
        t.answer_permission("codex:c");
        assert!(t.pending.is_none());
    }
}

/// The defect this fixes: a tab that re-attaches replays the request out of
/// `agent.history`, raises the bar again, and the answer it sends names a
/// request the adapter has already forgotten -- an error banner over a settled
/// question. The daemon now records the answer as a transcript message, and
/// replaying the two together must leave nothing pending.
#[test]
fn a_replayed_request_that_was_answered_does_not_ask_again() {
    let mut t = Transcript::default();
    t.replay(&[request(1, "req_1"), reply(2, "req_1", "allow")], &[]);
    assert!(
        t.pending.is_none(),
        "an answered request must not come back up"
    );
    // The answer is visible in the conversation, not merely absorbed.
    assert!(
        matches!(t.items.last(), Some(TranscriptItem::System { text }) if text == "permission allow"),
        "{:?}",
        t.items
    );
}

/// The defensive half: a history whose answer predates this daemon, or whose
/// marker was lost, still must not raise a bar over a question the agent
/// plainly moved on from. Anything after the request that only a *running*
/// agent could have produced settles it.
///
/// A user prompt is not on that list, and the case below says why: the prompt
/// is the user's, not the agent's, and it can reach the transcript while the
/// CLI is still blocked on the question.
#[test]
fn what_the_agent_produced_after_a_request_settles_it_and_a_prompt_does_not() {
    let after: [(&str, AgentMessage); 2] = [
        (
            "a tool that ran",
            msg(
                2,
                AgentMessageBody::ToolUse {
                    id: "t1".to_owned(),
                    name: "Bash".to_owned(),
                    input: json!({ "command": "ls -la" }),
                },
            ),
        ),
        (
            "a finished turn",
            msg(
                2,
                AgentMessageBody::Result {
                    cost_usd: 0.01,
                    duration_ms: 10,
                    num_turns: 1,
                    session_id: "s".to_owned(),
                },
            ),
        ),
    ];
    for (what, message) in after {
        let mut t = Transcript::default();
        // Even claiming to wait: the transcript says otherwise.
        t.set_state(AgentState::WaitingPermission, None);
        t.replay(&[request(1, "req_1"), message], &[]);
        assert!(t.pending.is_none(), "{what} must answer the request");
    }

    // A prompt typed while the question is open leaves the bar exactly where it
    // was: the daemon is still holding that request, and taking the bar down
    // would leave the user no way to answer it.
    let mut typing = Transcript::default();
    typing.set_state(AgentState::WaitingPermission, None);
    typing.replay(
        &[
            request(1, "req_1"),
            msg(
                2,
                AgentMessageBody::UserText {
                    text: "carry on".to_owned(),
                },
            ),
        ],
        &[],
    );
    assert_eq!(
        typing.pending.as_ref().map(|p| p.request_id.as_str()),
        Some("req_1"),
        "a prompt must not answer a permission request"
    );
}

/// The other direction: a request nothing answered, on an agent the daemon says
/// is waiting, is exactly the case the bar exists for.
#[test]
fn an_unanswered_request_on_a_waiting_agent_still_asks() {
    let mut t = Transcript::default();
    t.set_state(AgentState::WaitingPermission, None);
    t.replay(&[request(1, "req_1")], &[]);
    let pending = t.pending.as_ref().expect("the bar must be up");
    assert_eq!(pending.request_id, "req_1");
    assert_eq!(pending.summary, "ls -la");

    // And a daemon that is not waiting is the case it must not: the state comes
    // back with the history, so `Idle` here means the agent really is idle. It
    // has to have come back with it -- a transcript that was told nothing keeps
    // the bar up rather than guessing.
    let mut moved_on = Transcript::default();
    moved_on.set_state_from_history(AgentState::Idle, None);
    moved_on.replay(&[request(1, "req_1")], &[]);
    assert!(moved_on.pending.is_none());
}

/// An answer that names some other request leaves the open one alone: two
/// requests can be in the same transcript, and only the one that was answered
/// is settled.
#[test]
fn a_reply_for_another_request_leaves_the_open_one_up() {
    let mut t = Transcript::default();
    t.set_state(AgentState::WaitingPermission, None);
    t.replay(&[request(1, "req_2"), reply(2, "req_1", "deny")], &[]);
    assert_eq!(
        t.pending.as_ref().map(|p| p.request_id.as_str()),
        Some("req_2")
    );
}

/// The trap that made the first fix wrong: `agent.history` carries the daemon's
/// state, but the events buffered while it was in flight can *predate* it --
/// state changes carry no sequence number, so a `Working` from before the
/// snapshot arrives after it. Treating that as "the daemon left the question
/// behind" took the bar down over a request the daemon was still holding, and
/// the answer the user then pressed had nowhere to go.
#[test]
fn a_buffered_state_older_than_the_history_does_not_take_the_bar_down() {
    let mut t = Transcript::default();
    t.set_state_from_history(AgentState::WaitingPermission, Some("Bash".to_owned()));
    t.replay(
        &[request(1, "req_1")],
        &[
            // Both of these were on the bus before the history was read.
            LiveEvent::State(AgentState::Working, None),
            LiveEvent::Message(request(1, "req_1")),
            LiveEvent::State(AgentState::WaitingPermission, Some("Bash".to_owned())),
        ],
    );
    assert_eq!(
        t.pending.as_ref().map(|p| p.request_id.as_str()),
        Some("req_1"),
        "the bar must survive a replay that folds the state backwards"
    );
    assert_eq!(t.state, TabAgentState::Known(AgentState::WaitingPermission));

    // And the live path still clears on the way out of the wait, which is what
    // takes the bar down when the daemon moves on.
    t.set_state(AgentState::Working, None);
    assert!(t.pending.is_none());
}

// ---------------------------------------------------------------------------
// A history that could not be read, and answers that outlive a reconnect
// (review fixes, IQ6/IQ7).
// ---------------------------------------------------------------------------

/// `agent.history` failing tells the tab nothing about the agent. It used to be
/// answered with a fabricated `Idle`, and the two things that followed from
/// that were both wrong: the tab painted a running agent as finished, and the
/// guard at the end of the replay took down the bar over a permission request
/// the daemon was still holding -- which `seq` de-duplication then made
/// impossible to raise again, so the agent blocked forever.
#[test]
fn a_history_that_could_not_be_read_leaves_the_state_unknown_and_the_bar_up() {
    let mut t = Transcript::default();
    assert_eq!(
        t.state,
        TabAgentState::Unknown,
        "a transcript that has heard nothing does not claim the agent is idle"
    );

    // The request reached the live buffer while the history request was in
    // flight; the history itself never answered, so nothing sets the state.
    t.replay(&[], &[LiveEvent::Message(request(1, "req_1"))]);
    assert_eq!(
        t.pending.as_ref().map(|p| p.request_id.as_str()),
        Some("req_1"),
        "a state nobody reported must not settle a question the daemon is holding"
    );
    assert_eq!(t.state, TabAgentState::Unknown);
    assert_eq!(
        t.state.word(),
        "unavailable",
        "the view is told the state is unknown, not given a made-up one"
    );

    // The live stream still decides, both ways. A state that arrives is the
    // daemon's own word.
    t.set_state(AgentState::WaitingPermission, None);
    assert_eq!(t.state, TabAgentState::Known(AgentState::WaitingPermission));
    assert_eq!(t.state.word(), "waiting_permission");
    t.set_state(AgentState::Working, None);
    assert!(t.pending.is_none(), "leaving the wait still clears it");
}

/// A history that answered is still the last word: an agent the daemon says is
/// idle has nothing waiting, and the bar comes down.
#[test]
fn a_history_that_answered_still_settles_the_question() {
    let mut t = Transcript::default();
    t.set_state_from_history(AgentState::Idle, None);
    t.replay(&[request(1, "req_1")], &[]);
    assert!(t.pending.is_none());
    assert_eq!(t.state, TabAgentState::Known(AgentState::Idle));
}

/// "Always allow this tool" is answered once per tab, for as long as that tab
/// is looking at that agent. A daemon restart re-attaches the pane to the same
/// agent with a fresh `Transcript`, and losing the answers there made the user
/// re-approve every tool they had already approved. Pointing the tab at a
/// different agent is a different session and starts with none.
#[test]
fn always_allow_survives_a_reattach_to_the_same_agent() {
    let allowed = || BTreeSet::from(["Read".to_owned(), "Bash".to_owned()]);

    assert_eq!(
        carry_always_allow("agent_1", "agent_1", allowed()),
        allowed(),
        "the same agent after a reconnect keeps what the user answered"
    );
    assert_eq!(
        carry_always_allow("agent_1", "agent_2", allowed()),
        BTreeSet::new(),
        "another agent is another session"
    );
    assert_eq!(
        carry_always_allow("agent_1", "", allowed()),
        BTreeSet::new(),
        "detaching ends the session the answers belonged to"
    );
    assert_eq!(
        carry_always_allow("", "agent_1", BTreeSet::new()),
        BTreeSet::new(),
        "a first attach starts with nothing"
    );
}

/// `claude -p` fixes its model and its permission mode when the process starts,
/// so changing either is a restart -- and a restart that loses the conversation
/// is not a switch, it is a new agent. The session id the history carries is
/// what makes it the same one, and everything the tab was created with that is
/// not being changed has to come through untouched.
#[test]
fn changing_the_model_keeps_the_session_and_the_other_options() {
    let options = r#"{"model":"claude-opus-5","permission_mode":"manual","api_key":"kept"}"#;
    let merged = restart_options_with(
        options,
        Some("sess_42"),
        Some("claude-haiku-4-5-20251001"),
        None,
    );
    let value: serde_json::Value = serde_json::from_str(&merged).expect("json");
    assert_eq!(value["model"], "claude-haiku-4-5-20251001");
    assert_eq!(value["permission_mode"], "manual");
    assert_eq!(value["resume_session"], "sess_42");
    assert_eq!(value["api_key"], "kept");
}

/// The other dropdown, and the two together. A mode is always sent, so an
/// options string that never had one gains it rather than being left to the
/// CLI's own default -- which is a fifth behaviour nobody chose and nobody can
/// see from the pane.
#[test]
fn changing_the_permission_mode_sends_one_even_when_the_tab_had_none() {
    let merged = restart_options_with(r#"{"model":"claude-opus-5"}"#, None, None, Some("plan"));
    let value: serde_json::Value = serde_json::from_str(&merged).expect("json");
    assert_eq!(value["permission_mode"], "plan");
    assert_eq!(value["model"], "claude-opus-5");

    let both = restart_options_with("{}", Some("s"), Some(""), Some("bypassPermissions"));
    let value: serde_json::Value = serde_json::from_str(&both).expect("json");
    assert_eq!(value["permission_mode"], "bypassPermissions");
    // The empty model id is "let Claude Code decide", which is a choice and not
    // an absence: retain an explicit empty string so configured backend defaults
    // do not replace the user's choice on restart.
    assert_eq!(value.get("model").and_then(|v| v.as_str()), Some(""));
}

/// Neither override is given, which is what the Restart button sends. The
/// stored model and mode survive exactly as they were.
#[test]
fn a_plain_restart_changes_nothing_but_the_session() {
    let options = r#"{"model":"claude-opus-5","permission_mode":"acceptEdits"}"#;
    let merged = restart_options_with(options, Some("sess_1"), None, None);
    let value: serde_json::Value = serde_json::from_str(&merged).expect("json");
    assert_eq!(value["model"], "claude-opus-5");
    assert_eq!(value["permission_mode"], "acceptEdits");
    assert_eq!(value["resume_session"], "sess_1");
}
