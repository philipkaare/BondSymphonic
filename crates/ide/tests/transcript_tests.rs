//! The transcript model: pure Rust, no Qt. Everything the `TranscriptModel`
//! QObject exposes to the view is decided here, so these tests are what the
//! transcript view's behaviour actually rests on.

use bondsymphonic_ide::model::transcript::{
    tool_summary, Applied, PendingPermission, Transcript, TranscriptItem,
};
use bondsymphonic_proto::{AgentMessage, AgentMessageBody, AgentState, PermissionDecision};
use serde_json::json;

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
    assert_eq!(t.state, AgentState::Idle);
    assert!(t.state_detail.is_empty());

    t.set_state(AgentState::Error, Some("exit code 1".to_owned()));
    assert_eq!(t.state_detail, "exit code 1");
}

#[test]
fn decide_permission_answers_only_for_always_allowed_tools() {
    let mut t = Transcript::default();
    assert_eq!(t.decide_permission("Bash"), None);
    t.always_allow.insert("Bash".to_owned());
    assert_eq!(t.decide_permission("Bash"), Some(PermissionDecision::Allow));
    assert_eq!(t.decide_permission("Write"), None);
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
