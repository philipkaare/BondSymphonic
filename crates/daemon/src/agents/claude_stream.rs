//! Parser for the Claude Code CLI's `--output-format stream-json` protocol.
//!
//! The CLI writes one JSON object per stdout line and reads one per stdin
//! line. This module is the whole of that translation: [`parse_line`] turns a
//! stdout line into zero or more [`Parsed`] items, and the three builders turn
//! our intent into a stdin line. It is deliberately pure and total: no I/O, no
//! process handling, and no input that can make it panic or lose data. A line
//! it does not understand (a future message type, a truncated write, a stray
//! log line on the wrong stream) is kept verbatim as a `System` message with
//! subtype `raw`, so the transcript is never quietly lossy and a new upstream
//! message type degrades to "shown but not styled" rather than to "dropped".
//!
//! The shapes here are Claude Code 2.1's. They are pinned by the fixtures in
//! `tests/fixtures/claude-stream/`, which the fake `claude` in the adapter
//! tests replays verbatim.

use bondsymphonic_proto::{AgentMessageBody, AgentState, PermissionDecision};
use serde_json::{json, Value};

/// One item decoded from a stdout line. A single line can produce several: a
/// `system init` carries both the session id and a transcript entry, and a
/// `result` both ends the turn and moves the agent's state.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A transcript entry to record and publish.
    Message(AgentMessageBody),
    /// A state transition, with the optional detail the UI shows beside it
    /// (the tool awaiting permission, or the error that ended the turn).
    State(AgentState, Option<String>),
    /// The session id the CLI assigned, used later to `--resume`.
    SessionId(String),
    /// Understood and deliberately dropped (stream envelopes we do not render,
    /// acks of our own control requests).
    Nothing,
}

/// Wraps a line we could not interpret so that it still reaches the transcript.
fn raw(data: Value) -> Parsed {
    Parsed::Message(AgentMessageBody::System {
        subtype: "raw".to_owned(),
        data,
    })
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn owned_at(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

/// A `tool_result` block's `content` as flat text. The CLI sends a string for
/// simple output and an array of content blocks when the tool returned
/// structured parts, so both are flattened here rather than in every consumer.
///
/// Nothing is dropped. A tool that returns an image -- reading a screenshot is
/// the everyday case -- would otherwise flatten to the empty string and render
/// as a blank result with no sign anything was there, which is exactly the
/// quiet loss this module promises not to do. Blocks are keyed by their own
/// `type`, so one that happens to carry an unrelated `text` field is not
/// mistaken for text, and they are separated by newlines rather than run
/// together.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match str_at(item, "type") {
                "text" => str_at(item, "text").to_owned(),
                "image" => "[image]".to_owned(),
                // Verbatim rather than a bare `[type]`: a block we do not know
                // how to render is still worth keeping in full.
                _ => item.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The blocks of `message.content`, or an empty slice when the line is shaped
/// unexpectedly.
fn content_blocks(v: &Value) -> &[Value] {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Decodes one stdout line. Never panics, and never returns nothing for a line
/// that carried information: anything unrecognised becomes a `raw` message.
pub fn parse_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![raw(json!({ "line": line }))];
    };
    match str_at(&v, "type") {
        "system" => {
            let subtype = str_at(&v, "subtype").to_owned();
            if subtype == "init" {
                // `init` opens the turn: it names the session we may later
                // resume, and it means the CLI has started thinking.
                return vec![
                    Parsed::SessionId(str_at(&v, "session_id").to_owned()),
                    Parsed::Message(AgentMessageBody::System {
                        subtype,
                        data: v.clone(),
                    }),
                    Parsed::State(AgentState::Working, None),
                ];
            }
            vec![Parsed::Message(AgentMessageBody::System {
                subtype,
                data: v.clone(),
            })]
        }
        // Partial output. Only text deltas are worth a transcript entry; the
        // block and message envelopes are reconstructible from the final
        // `assistant` line that follows, so keeping them would double the
        // transcript for no gain.
        "stream_event" => {
            let event = owned_at(&v, "event");
            let delta = event.get("delta");
            let is_text_delta = str_at(&event, "type") == "content_block_delta"
                && delta.map(|d| str_at(d, "type")) == Some("text_delta");
            if is_text_delta {
                let text = delta.map(|d| str_at(d, "text")).unwrap_or_default();
                return vec![Parsed::Message(AgentMessageBody::AssistantDelta {
                    text: text.to_owned(),
                })];
            }
            vec![Parsed::Nothing]
        }
        "assistant" => content_blocks(&v)
            .iter()
            .filter_map(|block| match str_at(block, "type") {
                // An empty text block is what the CLI emits alongside a pure
                // tool call; it would render as a blank bubble.
                "text" => {
                    let text = str_at(block, "text");
                    (!text.is_empty()).then(|| {
                        Parsed::Message(AgentMessageBody::AssistantText {
                            text: text.to_owned(),
                        })
                    })
                }
                "tool_use" => Some(Parsed::Message(AgentMessageBody::ToolUse {
                    id: str_at(block, "id").to_owned(),
                    name: str_at(block, "name").to_owned(),
                    input: owned_at(block, "input"),
                })),
                _ => Some(raw(block.clone())),
            })
            .collect(),
        // Tool results, and -- with `--replay-user-messages` -- an echo of what
        // we wrote to stdin.
        "user" => content_blocks(&v)
            .iter()
            .map(|block| match str_at(block, "type") {
                "tool_result" => Parsed::Message(AgentMessageBody::ToolResult {
                    id: str_at(block, "tool_use_id").to_owned(),
                    output: tool_result_text(block.get("content").unwrap_or(&Value::Null)),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                "text" => Parsed::Message(AgentMessageBody::UserText {
                    text: str_at(block, "text").to_owned(),
                }),
                _ => raw(block.clone()),
            })
            .collect(),
        "control_request" => {
            let request = owned_at(&v, "request");
            if str_at(&request, "subtype") == "can_use_tool" {
                let tool_name = str_at(&request, "tool_name").to_owned();
                let suggestions = request
                    .get("permission_suggestions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                return vec![
                    Parsed::Message(AgentMessageBody::PermissionRequest {
                        request_id: str_at(&v, "request_id").to_owned(),
                        tool_name: tool_name.clone(),
                        input: owned_at(&request, "input"),
                        suggestions,
                    }),
                    Parsed::State(AgentState::WaitingPermission, Some(tool_name)),
                ];
            }
            vec![raw(v.clone())]
        }
        // The CLI acking a control request of ours (an interrupt). Nothing to
        // show; the state change comes from the `result` that follows.
        "control_response" => vec![Parsed::Nothing],
        "result" => {
            // 2.1 sends `total_cost_usd`; older builds sent `cost_usd`. A
            // missing cost is 0.0 rather than a parse failure, because the
            // rest of the line still ends the turn.
            let cost_usd = v
                .get("total_cost_usd")
                .or_else(|| v.get("cost_usd"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let message = Parsed::Message(AgentMessageBody::Result {
                cost_usd,
                duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
                num_turns: u32::try_from(v.get("num_turns").and_then(Value::as_u64).unwrap_or(0))
                    .unwrap_or(u32::MAX),
                session_id: str_at(&v, "session_id").to_owned(),
            });
            let state = if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                Parsed::State(AgentState::Error, Some(error_detail(&v)))
            } else {
                Parsed::State(AgentState::Idle, None)
            };
            vec![message, state]
        }
        _ => vec![raw(v.clone())],
    }
}

/// What to show beside an errored turn: the CLI's own error list when it sent
/// one, else the result's own text, else the result subtype, which at least
/// names the failure mode.
///
/// The text matters for a failed login. A token the server refuses does not
/// end the CLI in stream-json input mode; 2.1.263 answers the turn with
/// `is_error: true`, `subtype: "success"`, no `errors`, and `result: "Failed to
/// authenticate. API Error: 401 …"`, then waits for the next message. That
/// sentence is the one place the failure is named, and the IDE reads this
/// detail to mark Claude as not logged in (see its `auth_failure_in`).
///
/// A subtype that says nothing is dropped rather than shown. Claude Code
/// 2.1.263 answers a logged-out turn with `is_error: true` and
/// `subtype: "success"` and no `errors` array (recorded against the real CLI on
/// 2026-09-09), and a banner reading "success" over a failed turn is worse than
/// no banner at all -- the transcript itself carries the assistant's "Not logged
/// in" line, which is the thing to read.
fn error_detail(result: &Value) -> String {
    let joined = result
        .get("errors")
        .and_then(Value::as_array)
        .map(|errs| {
            errs.iter()
                .map(|e| {
                    e.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| e.to_string())
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    if !joined.is_empty() {
        return joined;
    }
    let text = str_at(result, "result").trim();
    if !text.is_empty() {
        return text.to_owned();
    }
    match str_at(result, "subtype") {
        "" | "success" => "the agent ended the turn with an error".to_owned(),
        subtype => subtype.to_owned(),
    }
}

/// One JSON object plus the newline the CLI's line-delimited stdin expects.
/// `Value::to_string` never emits an interior newline, so a prompt that
/// contains one still crosses as a single line.
fn line_of(v: Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

/// A user turn to write to the CLI's stdin.
pub fn user_line(text: &str) -> String {
    line_of(json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        },
    }))
}

/// The answer to a `can_use_tool` request. `updated_input` replaces the tool
/// input the model proposed (an edited command); it is omitted, rather than
/// sent as null, when the input is unchanged.
pub fn control_response_line(
    request_id: &str,
    decision: PermissionDecision,
    updated_input: Option<Value>,
    message: Option<String>,
) -> String {
    let inner = match decision {
        PermissionDecision::Allow => {
            let mut allow = json!({ "behavior": "allow" });
            if let (Some(obj), Some(input)) = (allow.as_object_mut(), updated_input) {
                obj.insert("updatedInput".to_owned(), input);
            }
            allow
        }
        PermissionDecision::Deny => json!({
            "behavior": "deny",
            "message": message.unwrap_or_default(),
        }),
    };
    line_of(json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": inner,
        },
    }))
}

/// Asks the CLI to abandon the turn in flight. `request_id` is ours to choose;
/// the CLI echoes it back in a `control_response`.
pub fn interrupt_line(request_id: &str) -> String {
    line_of(json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::{AgentMessageBody as B, AgentState as S, PermissionDecision};

    fn fixture(name: &str) -> Vec<String> {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-stream/");
        std::fs::read_to_string(format!("{p}{name}"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn all(name: &str) -> Vec<Parsed> {
        fixture(name).iter().flat_map(|l| parse_line(l)).collect()
    }

    #[test]
    fn simple_turn_yields_init_text_result_and_idle() {
        let items = all("simple_turn.ndjson");
        assert!(matches!(&items[0], Parsed::SessionId(s) if s == "sess-1"));
        assert!(
            matches!(&items[1], Parsed::Message(B::System { subtype, .. }) if subtype == "init")
        );
        assert!(
            matches!(&items[2], Parsed::State(S::Working, _)),
            "init means the agent is working"
        );
        assert!(
            matches!(&items[3], Parsed::Message(B::AssistantText { text }) if text == "Hello! How can I help?")
        );
        assert!(
            matches!(&items[4], Parsed::Message(B::Result { cost_usd, num_turns, session_id, .. }) if (*cost_usd - 0.0042).abs() < 1e-9 && *num_turns == 1 && session_id == "sess-1")
        );
        assert!(matches!(&items[5], Parsed::State(S::Idle, None)));
    }

    #[test]
    fn tool_use_and_tool_result_are_linked_by_id() {
        let items = all("tool_use_turn.ndjson");
        let uses: Vec<_> = items
            .iter()
            .filter(|p| matches!(p, Parsed::Message(B::ToolUse { .. })))
            .collect();
        let results: Vec<_> = items
            .iter()
            .filter(|p| matches!(p, Parsed::Message(B::ToolResult { .. })))
            .collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(results.len(), 1);
        assert!(
            matches!(uses[0], Parsed::Message(B::ToolUse { id, name, input }) if id == "toolu_1" && name == "Bash" && input["command"] == "ls")
        );
        assert!(
            matches!(results[0], Parsed::Message(B::ToolResult { id, output, is_error }) if id == "toolu_1" && output == "a.txt\nb.txt" && !is_error)
        );
        // The text block that accompanied the tool_use is still emitted.
        assert!(items.iter().any(
            |p| matches!(p, Parsed::Message(B::AssistantText { text }) if text == "Listing files.")
        ));
    }

    #[test]
    fn permission_request_switches_state_and_response_lines_are_well_formed() {
        let items = all("permission_turn.ndjson");
        let idx = items
            .iter()
            .position(|p| matches!(p, Parsed::Message(B::PermissionRequest { .. })))
            .unwrap();
        assert!(
            matches!(&items[idx], Parsed::Message(B::PermissionRequest { request_id, tool_name, input, suggestions }) if request_id == "req-1" && tool_name == "Bash" && input["command"] == "rm -rf build" && suggestions.len() == 1)
        );
        assert!(
            matches!(&items[idx + 1], Parsed::State(S::WaitingPermission, Some(t)) if t == "Bash")
        );
        let allow: serde_json::Value = serde_json::from_str(
            control_response_line("req-1", PermissionDecision::Allow, None, None).trim(),
        )
        .unwrap();
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["request_id"], "req-1");
        assert_eq!(allow["response"]["subtype"], "success");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        let deny: serde_json::Value = serde_json::from_str(
            control_response_line("req-1", PermissionDecision::Deny, None, Some("no".into()))
                .trim(),
        )
        .unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert_eq!(deny["response"]["response"]["message"], "no");
    }

    #[test]
    fn partial_messages_become_deltas_and_the_final_text_is_not_duplicated() {
        let items = all("partial_messages.ndjson");
        let deltas: Vec<String> = items
            .iter()
            .filter_map(|p| match p {
                Parsed::Message(B::AssistantDelta { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hel", "lo"]);
        // The assistant message that follows a streamed block carries the same
        // text; it is emitted as AssistantText so consumers that ignore deltas
        // still see it, and consumers that render deltas replace, not append.
        assert!(items
            .iter()
            .any(|p| matches!(p, Parsed::Message(B::AssistantText { text }) if text == "Hello")));
    }

    #[test]
    fn unknown_and_malformed_lines_are_kept_as_raw_system_messages() {
        let items = all("unknown_types.ndjson");
        let raws: Vec<_> = items
            .iter()
            .filter(|p| matches!(p, Parsed::Message(B::System { subtype, .. }) if subtype == "raw"))
            .collect();
        assert_eq!(raws.len(), 2, "one unknown type, one non-JSON line");
        assert!(
            items
                .iter()
                .any(|p| matches!(p, Parsed::State(S::Error, Some(d)) if d.contains("boom"))),
            "an error result becomes state Error with the detail"
        );
    }

    /// The detail beside an errored turn has to say something. Claude Code
    /// 2.1.263 answers a logged-out turn with `is_error: true`, no `errors`
    /// array and `subtype: "success"` (recorded from the real CLI inside a
    /// sandbox on 2026-09-09), and the IDE puts this string in the pane's
    /// banner, where the word "success" over a failed turn is a lie.
    #[test]
    fn an_errored_result_never_reports_itself_as_a_success() {
        let detail = |line: Value| match parse_line(&line.to_string())
            .into_iter()
            .find(|p| matches!(p, Parsed::State(S::Error, _)))
        {
            Some(Parsed::State(_, detail)) => detail.unwrap_or_default(),
            other => panic!("expected an error state, got {other:?}"),
        };
        let base = |subtype: &str| {
            json!({
                "type": "result",
                "subtype": subtype,
                "is_error": true,
                "session_id": "s",
                "total_cost_usd": 0.0,
                "duration_ms": 50,
                "num_turns": 1,
            })
        };
        assert_eq!(
            detail(base("success")),
            "the agent ended the turn with an error"
        );
        assert_eq!(detail(base("error_max_turns")), "error_max_turns");
        let mut with_errors = base("success");
        with_errors["errors"] = json!(["Not logged in", "run /login"]);
        assert_eq!(detail(with_errors), "Not logged in; run /login");
    }

    /// A token the server refuses -- revoked, or the long-lived one gone bad --
    /// does not end the CLI in stream-json input mode: it answers the turn with
    /// an errored `result` whose `result` text is the API's sentence, and waits
    /// for the next message. Recorded from CLI 2.1.263 with a well-shaped bogus
    /// `CLAUDE_CODE_OAUTH_TOKEN` on 2026-09-28. That sentence is the only place
    /// the failure is named, and the IDE reads it from the state's detail to
    /// mark Claude as not logged in, so it has to be the detail.
    #[test]
    fn an_errored_result_names_the_clis_own_sentence() {
        let line = json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "result": "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
            "terminal_reason": "api_error",
            "session_id": "s",
            "total_cost_usd": 0.0,
            "duration_ms": 50,
            "num_turns": 1,
        });
        let detail = parse_line(&line.to_string())
            .into_iter()
            .find_map(|p| match p {
                Parsed::State(S::Error, detail) => detail,
                _ => None,
            });
        assert_eq!(
            detail.as_deref(),
            Some("Failed to authenticate. API Error: 401 OAuth access token is invalid.")
        );
    }

    /// A tool that returns anything but plain text -- reading a screenshot is
    /// the everyday case -- must not vanish from the transcript. Each block is
    /// keyed by its own `type`, so a future block carrying an unrelated `text`
    /// field is not mistaken for text, and the blocks are separated rather than
    /// run together.
    #[test]
    fn a_tool_result_with_mixed_blocks_keeps_every_block() {
        let line = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","#,
            r#""tool_use_id":"toolu_9","content":[{"type":"text","text":"before"},"#,
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}},"#,
            r#"{"type":"text","text":"after"},{"kind":"unlabelled"}],"is_error":false}]},"#,
            r#""session_id":"sess-9"}"#
        );
        let items = parse_line(line);
        let output = items
            .iter()
            .find_map(|p| match p {
                Parsed::Message(B::ToolResult { output, .. }) => Some(output.clone()),
                _ => None,
            })
            .expect("a tool result");
        assert_eq!(output, "before\n[image]\nafter\n{\"kind\":\"unlabelled\"}");
    }

    #[test]
    fn user_and_interrupt_lines_are_single_line_json() {
        let u = user_line("hi\nthere");
        assert!(u.ends_with('\n') && !u.trim_end().contains('\n'));
        let v: serde_json::Value = serde_json::from_str(u.trim()).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], "hi\nthere");
        let i: serde_json::Value = serde_json::from_str(interrupt_line("r9").trim()).unwrap();
        assert_eq!(i["request"]["subtype"], "interrupt");
        assert_eq!(i["request_id"], "r9");
    }
}
