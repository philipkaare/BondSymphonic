//! ACP session update mapping, as OpenCode 1.14.48 sends it (recorded in
//! `tests/fixtures/opencode/probe-1.14.48`).
//!
//! * Assistant text arrives as `agent_message_chunk` deltas. A stretch of it
//!   ends at anything else the transcript shows, a new `messageId`, or the end
//!   of the turn, and is then repeated in full as `AssistantText`, which is
//!   what finalises the IDE's bubble and lets it render Markdown.
//! * A tool call's first `tool_call` carries `rawInput: {}`; its input comes in
//!   a later `tool_call_update`, so the card waits for it.
//! * Results are `rawOutput: {output, metadata}` or `{error}`.
//! * Tool call IDs are only unique within a turn.
use super::codex_stream::{redact, text};
use bondsymphonic_proto::{AgentMessageBody, TurnUsage};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Keep the ACP command catalog in the transcript so reconnects restore completion.
pub fn command_metadata(update: &Value) -> AgentMessageBody {
    let commands: Vec<Value> = update["availableCommands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command["name"].as_str()?.trim_start_matches('/');
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                return None;
            }
            let mut row = json!({"name":name,"description":command["description"].as_str().unwrap_or_default()});
            if let Some(hint) = command["input"]["hint"].as_str() {
                row["argument_hint"] = json!(hint);
            }
            Some(row)
        })
        .collect();
    AgentMessageBody::System {
        subtype: "agent_commands".into(),
        data: json!({"commands":commands}),
    }
}

#[derive(Default)]
pub struct OpenCodeStream {
    /// Every field seen for each of this turn's tool calls, merged.
    tools: HashMap<String, Value>,
    announced: HashSet<String>,
    completed: HashSet<String>,
    /// The assistant text and reasoning not yet finalised.
    text: String,
    message: String,
    thought: String,
    /// Unrecognised update kinds already shown this turn.
    unknown: HashSet<String>,
    /// Whether this turn produced anything the user can see.
    produced: bool,
    /// The last `usage_update` of this turn: the context in use, its size
    /// and the session's cost.
    usage: Option<Value>,
}

pub fn approval_id(id: &Value) -> String {
    format!("opencode:{id}")
}

fn content_text(value: &Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let block = item.get("content").unwrap_or(item);
                    if block["type"] == "text" {
                        text(block, "text").to_owned()
                    } else if item["type"] == "diff" {
                        format!("{}\n{}", text(item, "oldText"), text(item, "newText"))
                    } else {
                        item.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn plain(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// A tool's result text and whether it failed.
fn tool_output(tool: &Value) -> (String, bool) {
    let failed = text(tool, "status") == "failed";
    let raw = &tool["rawOutput"];
    if let Some(error) = raw.get("error").filter(|e| !e.is_null()) {
        (plain(error), true)
    } else if let Some(output) = raw.get("output") {
        (plain(output), failed)
    } else if !raw.is_null() {
        (plain(raw), failed)
    } else {
        (content_text(&tool["content"]), failed)
    }
}

fn has_input(tool: &Value) -> bool {
    match &tool["rawInput"] {
        Value::Null => false,
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

fn text_chunk(update: &Value) -> Option<String> {
    (update["content"]["type"] == "text").then(|| text(&update["content"], "text").to_owned())
}

impl OpenCodeStream {
    pub fn ingest(&mut self, mut update: Value) -> Vec<AgentMessageBody> {
        redact(&mut update, &[]);
        let kind = text(&update, "sessionUpdate").to_owned();
        match kind.as_str() {
            "agent_message_chunk" => {
                let Some(delta) = text_chunk(&update) else {
                    return vec![];
                };
                let mut out = self.flush_thought();
                let message = text(&update, "messageId");
                if message != self.message {
                    out.extend(self.flush_text());
                    self.message = message.into();
                }
                self.text.push_str(&delta);
                self.produced = true;
                out.push(AgentMessageBody::AssistantDelta { text: delta });
                out
            }
            "agent_thought_chunk" => {
                let Some(delta) = text_chunk(&update) else {
                    return vec![];
                };
                let out = self.flush_text();
                self.thought.push_str(&delta);
                self.produced = true;
                out
            }
            "tool_call" | "tool_call_update" => self.tool(update),
            // Not a row: it is folded into the turn's usage when it ends.
            "usage_update" => {
                self.usage = Some(update);
                vec![]
            }
            // Bookkeeping, replayed history and plans: none of it is the
            // conversation, and most of it arrives once per step.
            // The command catalogue is metadata the adapter forwards itself
            // (`command_metadata`), before replay and idle filtering.
            "available_commands_update"
            | "current_mode_update"
            | "config_option_update"
            | "user_message_chunk"
            | "plan" => vec![],
            _ if self.unknown.insert(kind.clone()) => {
                let mut out = self.flush();
                out.push(AgentMessageBody::System {
                    subtype: "opencode_update".into(),
                    data: update,
                });
                out
            }
            _ => vec![],
        }
    }

    fn tool(&mut self, update: Value) -> Vec<AgentMessageBody> {
        let id = text(&update, "toolCallId").to_owned();
        if id.is_empty() {
            return vec![];
        }
        let tool = self.tools.entry(id.clone()).or_insert_with(|| json!({}));
        if let Some(fields) = update.as_object() {
            for (key, value) in fields {
                // The first title names the tool ("bash"); later ones describe
                // the call ("List files").
                if key == "title" && tool.get("name").is_none() {
                    tool["name"] = value.clone();
                }
                tool[key] = value.clone();
            }
        }
        let tool = tool.clone();
        let status = text(&tool, "status");
        let mut out = vec![];
        if !self.announced.contains(&id) && (has_input(&tool) || status != "pending") {
            out.extend(self.flush());
            self.announced.insert(id.clone());
            out.push(AgentMessageBody::ToolUse {
                id: id.clone(),
                name: text(&tool, "name").to_owned(),
                input: if has_input(&tool) {
                    tool["rawInput"].clone()
                } else {
                    tool.clone()
                },
            });
        }
        if matches!(status, "completed" | "failed") && self.completed.insert(id.clone()) {
            out.extend(self.flush());
            let (output, is_error) = tool_output(&tool);
            out.push(AgentMessageBody::ToolResult {
                id,
                output,
                is_error,
            });
        }
        self.produced = true;
        out
    }

    /// The tool name and input to show for a permission request, whose own
    /// `rawInput` is empty: the latest input seen for that call, else its
    /// title and kind.
    pub fn permission(&mut self, tool_call: &Value) -> (String, Value) {
        let mut call = tool_call.clone();
        redact(&mut call, &[]);
        let known = self.tools.get(text(&call, "toolCallId"));
        let name = known
            .map(|tool| text(tool, "name"))
            .filter(|name| !name.is_empty())
            .unwrap_or(text(&call, "title"))
            .to_owned();
        let input = [known, Some(&call)]
            .into_iter()
            .flatten()
            .find(|tool| has_input(tool))
            .map(|tool| tool["rawInput"].clone())
            .unwrap_or_else(|| json!({"title":text(&call, "title"), "kind":text(&call, "kind")}));
        (name, input)
    }

    fn flush_text(&mut self) -> Vec<AgentMessageBody> {
        if self.text.is_empty() {
            return vec![];
        }
        vec![AgentMessageBody::AssistantText {
            text: std::mem::take(&mut self.text),
        }]
    }

    fn flush_thought(&mut self) -> Vec<AgentMessageBody> {
        if self.thought.is_empty() {
            return vec![];
        }
        vec![AgentMessageBody::System {
            subtype: "opencode_reasoning".into(),
            data: json!({"text":std::mem::take(&mut self.thought)}),
        }]
    }

    /// Finalises whatever text or reasoning is open, before something else is
    /// put in the transcript.
    pub fn flush(&mut self) -> Vec<AgentMessageBody> {
        let mut out = self.flush_thought();
        out.extend(self.flush_text());
        out
    }

    /// The turn's usage: the tokens OpenCode's prompt result reports
    /// (`result_usage`, its final model call's) and the last `usage_update`
    /// seen this turn. Call before [`finish_turn`](Self::finish_turn), which
    /// forgets the update.
    ///
    /// A cost of 0 is left out: OpenCode reports 0 for a model it has no price
    /// for, which is not the same as a free one.
    pub fn turn_usage(&self, result_usage: &Value) -> Option<TurnUsage> {
        let update = self.usage.as_ref().unwrap_or(&Value::Null);
        let cost = &update["cost"];
        let usage = TurnUsage {
            input_tokens: result_usage["inputTokens"].as_u64(),
            output_tokens: result_usage["outputTokens"].as_u64(),
            context_tokens: update["used"].as_u64(),
            context_window: update["size"].as_u64().filter(|size| *size > 0),
            session_cost_usd: cost["amount"]
                .as_f64()
                .filter(|amount| *amount > 0.0 && text(cost, "currency") == "USD"),
        };
        (!usage.is_empty()).then_some(usage)
    }

    /// Ends the turn: what is still open, and whether the turn produced
    /// anything at all.
    pub fn finish_turn(&mut self) -> (Vec<AgentMessageBody>, bool) {
        let out = self.flush();
        let produced = self.produced;
        *self = Self::default();
        (out, produced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What OpenCode 1.14.48 sent while answering the `n`th prompt (1-based)
    /// of a recorded probe scenario: its `session/update`s and peer requests,
    /// in order, up to the prompt's response.
    fn recorded_turn(scenario: &str, n: usize) -> Vec<Value> {
        let path = format!(
            "{}/tests/fixtures/opencode/probe-1.14.48/frames/{scenario}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut prompts = 0;
        let mut prompt = None;
        let mut frames = vec![];
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            let frame = &record["frame"];
            if record["dir"] == "c->a" && frame["method"] == "session/prompt" {
                prompts += 1;
                if prompts == n {
                    prompt = Some(frame["id"].clone());
                }
                continue;
            }
            let Some(id) = &prompt else { continue };
            if record["dir"] != "a->c" {
                continue;
            }
            if frame.get("method").is_none() && &frame["id"] == id {
                return frames;
            }
            if frame.get("method").is_some() {
                frames.push(frame.clone());
            }
        }
        frames
    }

    /// Feeds a recorded turn through the stream, permission requests included,
    /// and ends it.
    fn play(stream: &mut OpenCodeStream, frames: &[Value]) -> (Vec<AgentMessageBody>, bool) {
        let mut out = vec![];
        for frame in frames {
            match frame["method"].as_str() {
                Some("session/update") => {
                    out.extend(stream.ingest(frame["params"]["update"].clone()))
                }
                Some("session/request_permission") => {
                    let (tool_name, input) = stream.permission(&frame["params"]["toolCall"]);
                    out.push(AgentMessageBody::PermissionRequest {
                        request_id: approval_id(&frame["id"]),
                        tool_name,
                        input,
                        suggestions: vec![],
                    });
                }
                _ => {}
            }
        }
        let (end, produced) = stream.finish_turn();
        out.extend(end);
        (out, produced)
    }

    fn listing() -> String {
        "total 12\ndrwxr-xr-x 3 bs bs 4096 Oct 20 15:33 .\ndrwxr-xr-x 5 bs bs 4096 Oct 20 15:33 ..\ndrwxr-xr-x 7 bs bs 4096 Oct 20 15:33 .git\n".into()
    }

    /// C2 turn 1: the tool's input arrives after the empty first `tool_call`,
    /// the permission request carries none of its own, the result is
    /// `{output, metadata}`, and the reply text is finalised.
    #[test]
    fn recorded_allowed_tool_shows_its_real_input_output_and_final_text() {
        let mut stream = OpenCodeStream::default();
        let (bodies, produced) = play(&mut stream, &recorded_turn("C2", 1));
        let input = json!({"command":"ls -la","description":"List files"});
        assert_eq!(
            bodies,
            vec![
                AgentMessageBody::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: input.clone()
                },
                AgentMessageBody::PermissionRequest {
                    request_id: "opencode:0".into(),
                    tool_name: "bash".into(),
                    input,
                    suggestions: vec![]
                },
                AgentMessageBody::ToolResult {
                    id: "call_1".into(),
                    output: listing(),
                    is_error: false
                },
                AgentMessageBody::AssistantDelta {
                    text: "DONE".into()
                },
                AgentMessageBody::AssistantText {
                    text: "DONE".into()
                },
            ]
        );
        assert!(produced);
    }

    /// C turn 1: a refused tool fails with `{error}`, which is shown as an
    /// error rather than as JSON.
    #[test]
    fn recorded_rejected_tool_is_an_error_with_its_message() {
        let mut stream = OpenCodeStream::default();
        let (bodies, produced) = play(&mut stream, &recorded_turn("C", 1));
        assert!(produced);
        assert_eq!(
            bodies.last(),
            Some(&AgentMessageBody::ToolResult {
                id: "call_1".into(),
                output: "The user rejected permission to use this specific tool call.".into(),
                is_error: true
            })
        );
    }

    /// C turn 3 was cancelled mid-stream and still sent chunks after the
    /// cancel: one bubble, finalised with everything that arrived.
    #[test]
    fn recorded_cancelled_stream_is_one_finalised_bubble() {
        let mut stream = OpenCodeStream::default();
        let (bodies, _) = play(&mut stream, &recorded_turn("C", 3));
        assert_eq!(
            bodies.last(),
            Some(&AgentMessageBody::AssistantText {
                text: "1 2 3 4 5 ".into()
            })
        );
        assert_eq!(
            bodies
                .iter()
                .filter(|b| matches!(b, AgentMessageBody::AssistantText { .. }))
                .count(),
            1
        );
    }

    /// A 401 (G401) and the free tier's refusal (B, which still reports
    /// `usage_update`) produce nothing at all, which the adapter must notice.
    #[test]
    fn recorded_provider_failures_produce_nothing() {
        for scenario in ["G401", "B"] {
            let mut stream = OpenCodeStream::default();
            let (bodies, produced) = play(&mut stream, &recorded_turn(scenario, 1));
            assert_eq!(bodies, vec![], "{scenario}");
            assert!(!produced, "{scenario}");
        }
    }

    /// E reuses `call_1` in consecutive turns; each is its own tool card.
    #[test]
    fn recorded_tool_ids_reused_across_turns_are_new_calls() {
        let mut stream = OpenCodeStream::default();
        for n in [1, 2] {
            let (bodies, _) = play(&mut stream, &recorded_turn("E", n));
            assert!(bodies.iter().any(
                |b| matches!(b, AgentMessageBody::ToolResult { id, is_error: false, .. } if id == "call_1")
            ));
            assert_eq!(
                bodies
                    .iter()
                    .filter(|b| matches!(b, AgentMessageBody::ToolUse { .. }))
                    .count(),
                1,
                "turn {n}"
            );
        }
    }

    fn chunk(kind: &str, message: &str, text: &str) -> Value {
        json!({"sessionUpdate":kind,"messageId":message,"content":{"type":"text","text":text}})
    }

    /// Reasoning is one row per stretch of thought, never one per token, and
    /// text before a tool call is finalised before the call's card.
    #[test]
    fn reasoning_coalesces_and_text_is_finalised_before_other_rows() {
        let mut stream = OpenCodeStream::default();
        let mut out = vec![];
        for update in [
            chunk("agent_thought_chunk", "m1", "think "),
            chunk("agent_thought_chunk", "m1", "hard"),
            chunk("agent_message_chunk", "m1", "Let me "),
            chunk("agent_message_chunk", "m1", "look."),
            json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"read","status":"pending","rawInput":{"filePath":"a"}}),
            chunk("agent_message_chunk", "m2", "Done."),
        ] {
            out.extend(stream.ingest(update));
        }
        out.extend(stream.finish_turn().0);
        assert_eq!(
            out,
            vec![
                AgentMessageBody::System {
                    subtype: "opencode_reasoning".into(),
                    data: json!({"text":"think hard"})
                },
                AgentMessageBody::AssistantDelta {
                    text: "Let me ".into()
                },
                AgentMessageBody::AssistantDelta {
                    text: "look.".into()
                },
                AgentMessageBody::AssistantText {
                    text: "Let me look.".into()
                },
                AgentMessageBody::ToolUse {
                    id: "t".into(),
                    name: "read".into(),
                    input: json!({"filePath":"a"})
                },
                AgentMessageBody::AssistantDelta {
                    text: "Done.".into()
                },
                AgentMessageBody::AssistantText {
                    text: "Done.".into()
                },
            ]
        );
    }

    /// Bookkeeping updates never reach the transcript; an unrecognised one is
    /// shown once per turn, not once per frame.
    #[test]
    fn bookkeeping_is_dropped_and_unknown_updates_are_shown_once() {
        let mut stream = OpenCodeStream::default();
        let mut out = vec![];
        for update in [
            json!({"sessionUpdate":"available_commands_update","availableCommands":[]}),
            json!({"sessionUpdate":"current_mode_update","currentModeId":"build"}),
            json!({"sessionUpdate":"config_option_update","configOptions":[]}),
            json!({"sessionUpdate":"usage_update","used":1,"size":2}),
            json!({"sessionUpdate":"plan","entries":[]}),
            json!({"sessionUpdate":"novel","n":1,"apiKey":"sk-secret"}),
            json!({"sessionUpdate":"novel","n":2}),
        ] {
            out.extend(stream.ingest(update));
        }
        assert_eq!(
            out,
            vec![AgentMessageBody::System {
                subtype: "opencode_update".into(),
                data: json!({"sessionUpdate":"novel","n":1,"apiKey":"[redacted]"})
            }]
        );
        assert!(!stream.finish_turn().1);
    }

    /// A message the user sends mid-stream lands after the text so far, so the
    /// bubble it interrupts is finalised first and the rest starts afresh.
    #[test]
    fn flushing_mid_stream_finalises_only_what_has_arrived() {
        let mut stream = OpenCodeStream::default();
        stream.ingest(chunk("agent_message_chunk", "m", "one "));
        assert_eq!(
            stream.flush(),
            vec![AgentMessageBody::AssistantText {
                text: "one ".into()
            }]
        );
        stream.ingest(chunk("agent_message_chunk", "m", "two"));
        assert_eq!(
            stream.finish_turn().0,
            vec![AgentMessageBody::AssistantText { text: "two".into() }]
        );
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn commands_are_normalized_into_replayable_metadata() {
        let row = command_metadata(&json!({
            "sessionUpdate":"available_commands_update",
            "availableCommands":[
                {"name":"review","description":"Review changes","input":{"hint":"target"}},
                {"name":"/two words","description":"refused"}
            ]
        }));
        assert_eq!(
            row,
            AgentMessageBody::System {
                subtype: "agent_commands".into(),
                data: json!({"commands":[
                    {"name":"review","description":"Review changes","argument_hint":"target"}
                ]})
            }
        );
    }
}
