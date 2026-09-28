use bondsymphonic_daemon::agents::codex_stream::CodexStream;
use bondsymphonic_proto::AgentMessageBody;
use serde_json::json;

#[test]
fn live_turn_maps_tools_approvals_text_and_interruption() {
    let mut stream = CodexStream::default();
    let mut bodies = Vec::new();
    for line in include_str!("fixtures/codex-app-server/live-0.158.0.ndjson").lines() {
        let frame: serde_json::Value = serde_json::from_str(line).unwrap();
        if frame["direction"] == "recv" {
            bodies.extend(stream.ingest(frame["message"].clone()));
        }
    }
    assert!(bodies
        .iter()
        .any(|b| matches!(b, AgentMessageBody::ToolUse { name, .. } if name == "Bash")));
    assert!(bodies
        .iter()
        .any(|b| matches!(b, AgentMessageBody::ToolUse { name, .. } if name == "Edit")));
    assert!(bodies.iter().any(|b| matches!(b, AgentMessageBody::ToolResult { output, is_error: false, .. } if output.contains("PREFLIGHT_COMMAND_OK"))));
    assert_eq!(bodies.iter().filter(|b| matches!(b, AgentMessageBody::AssistantText { text } if text == "PREFLIGHT_DONE")).count(), 1);
    assert!(
        bodies
            .iter()
            .filter(|b| matches!(b, AgentMessageBody::PermissionRequest { .. }))
            .count()
            >= 2
    );
    assert!(bodies.iter().any(|b| matches!(b, AgentMessageBody::System { subtype, data } if subtype == "codex_turn" && data["status"] == "interrupted")));
}

#[test]
fn output_deltas_join_by_item_and_failures_remain_errors() {
    let mut stream = CodexStream::default();
    stream.ingest(json!({"method":"item/commandExecution/outputDelta","params":{"itemId":"a","delta":"first"}}));
    stream.ingest(json!({"method":"item/commandExecution/outputDelta","params":{"itemId":"b","delta":"other"}}));
    stream.ingest(json!({"method":"item/commandExecution/outputDelta","params":{"itemId":"a","delta":" second"}}));
    let bodies = stream.ingest(json!({"method":"item/completed","params":{"item":{"type":"commandExecution","id":"a","status":"failed","exitCode":1}}}));
    assert!(bodies.iter().any(|b| matches!(b, AgentMessageBody::ToolResult { id, output, is_error: true } if id == "a" && output == "first second")));
}

#[test]
fn reasoning_is_hidden_and_unknown_messages_are_preserved_without_secrets() {
    let mut stream = CodexStream::default();
    assert!(stream
        .ingest(json!({"method":"item/reasoning/textDelta","params":{"delta":"private"}}))
        .is_empty());
    let body = stream.ingest(
        json!({"method":"future/event","params":{"accessToken":"secret","value":"visible"}}),
    );
    let text = serde_json::to_string(&body).unwrap();
    assert!(text.contains("visible"));
    assert!(!text.contains("secret"));
}

#[test]
fn edits_keep_diffs_and_mcp_errors_remain_errors() {
    let mut stream = CodexStream::default();
    let edit = stream.ingest(json!({"method":"item/completed","params":{"item":{"type":"fileChange","id":"edit","status":"completed","changes":[{"path":"a.txt","diff":"+new"}]}}}));
    assert!(edit.iter().any(|b| matches!(b, AgentMessageBody::ToolUse { input, .. } if input["changes"][0]["path"] == "a.txt" && input["changes"][0]["diff"] == "+new")));
    let mcp = stream.ingest(json!({"method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"mcp","server":"s","tool":"t","status":"failed","error":{"message":"failed"}}}}));
    assert!(mcp.iter().any(|b| matches!(b, AgentMessageBody::ToolResult { is_error:true, output, .. } if output.contains("failed"))));
}

#[test]
fn each_result_counts_only_its_own_turn() {
    let mut stream = CodexStream::default();
    for _ in 0..2 {
        let messages = stream.ingest(json!({"method":"turn/completed","params":{"threadId":"t","turn":{"status":"completed"}}}));
        assert!(messages
            .iter()
            .any(|b| matches!(b, AgentMessageBody::Result { num_turns: 1, .. })));
    }
}
