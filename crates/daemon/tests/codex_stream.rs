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

fn token_usage(total_in: u64, total_out: u64, last_total: u64) -> serde_json::Value {
    json!({"method":"thread/tokenUsage/updated","params":{"threadId":"t","turnId":"1","tokenUsage":{
        "total":{"totalTokens":total_in + total_out,"inputTokens":total_in,"cachedInputTokens":0,"outputTokens":total_out,"reasoningOutputTokens":0},
        "last":{"totalTokens":last_total,"inputTokens":last_total,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},
        "modelContextWindow":258400}}})
}

fn turn_usage_of(bodies: &[AgentMessageBody]) -> Option<bondsymphonic_proto::TurnUsage> {
    bodies
        .iter()
        .find_map(bondsymphonic_proto::TurnUsage::from_turn_row)
}

fn completed() -> serde_json::Value {
    json!({"method":"turn/completed","params":{"threadId":"t","turn":{"status":"completed"}}})
}

/// `thread/tokenUsage/updated` is cumulative for the thread; a turn's own
/// tokens are what it added, and the context is the last request's size.
#[test]
fn a_turn_reports_its_own_tokens_and_the_context_it_left() {
    let mut stream = CodexStream::default();
    stream.ingest(token_usage(15932, 87, 16019));
    let first = turn_usage_of(&stream.ingest(completed())).unwrap();
    assert_eq!(first.input_tokens, Some(15932));
    assert_eq!(first.output_tokens, Some(87));
    assert_eq!(first.context_tokens, Some(16019));
    assert_eq!(first.context_window, Some(258400));
    assert_eq!(first.session_cost_usd, None, "Codex reports no price");
    stream.ingest(token_usage(32015, 140, 16136));
    stream.ingest(token_usage(48175, 169, 16189));
    let second = turn_usage_of(&stream.ingest(completed())).unwrap();
    assert_eq!(second.input_tokens, Some(48175 - 15932));
    assert_eq!(second.output_tokens, Some(169 - 87));
    assert_eq!(second.context_tokens, Some(16189));
    // A turn Codex reported nothing for carries nothing.
    assert_eq!(turn_usage_of(&stream.ingest(completed())), None);
}

/// A resumed thread's totals include turns this process never saw, so its
/// first turn's own tokens are unknown rather than the whole thread's.
#[test]
fn a_resumed_threads_first_turn_reports_only_its_context() {
    let mut stream = CodexStream::resumed();
    stream.ingest(token_usage(90000, 900, 20000));
    let first = turn_usage_of(&stream.ingest(completed())).unwrap();
    assert_eq!(first.input_tokens, None);
    assert_eq!(first.output_tokens, None);
    assert_eq!(first.context_tokens, Some(20000));
    stream.ingest(token_usage(110000, 950, 21000));
    let second = turn_usage_of(&stream.ingest(completed())).unwrap();
    assert_eq!(second.input_tokens, Some(20000));
    assert_eq!(second.output_tokens, Some(50));
}
