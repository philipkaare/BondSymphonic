//! Transcript mapping for the pinned Codex app-server protocol.
use bondsymphonic_proto::{AgentMessageBody, TurnUsage, TURN_USAGE_KEY};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub struct CodexStream {
    output: HashMap<String, String>,
    started: HashSet<String>,
    usage: Value,
    /// The thread's cumulative input and output tokens when the last turn
    /// ended, which a turn's own tokens are counted from. `None` while that
    /// is unknown: a resumed thread's totals include turns from before.
    baseline: Option<(u64, u64)>,
}

impl Default for CodexStream {
    fn default() -> Self {
        Self {
            output: HashMap::new(),
            started: HashSet::new(),
            usage: Value::Null,
            baseline: Some((0, 0)),
        }
    }
}

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// IDs on server requests are independent of client call IDs and may be strings.
pub fn approval_id(id: &Value) -> String {
    format!("codex:{id}")
}

/// Apply before persisting raw protocol/error data. Known process secrets are
/// also removed by the transport, including when embedded in an error string.
pub fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                if matches!(
                    key.as_str(),
                    "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "apikey"
                        | "authorization"
                        | "password"
                ) {
                    *value = Value::String("[redacted]".into());
                } else {
                    redact(value, secrets);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|v| redact(v, secrets)),
        Value::String(s) => {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                *s = s.replace(secret, "[redacted]");
            }
        }
        _ => {}
    }
}

impl CodexStream {
    /// A stream for a resumed thread, whose first turn's own tokens cannot be
    /// told apart from the thread's earlier ones.
    pub fn resumed() -> Self {
        Self {
            baseline: None,
            ..Self::default()
        }
    }

    /// The turn's usage from the last `thread/tokenUsage/updated`, and the
    /// baseline moved past it. `None` when Codex reported nothing this turn.
    fn turn_usage(&mut self) -> Option<TurnUsage> {
        let usage = std::mem::take(&mut self.usage);
        if usage.is_null() {
            return None;
        }
        let total = &usage["total"];
        let totals = total["inputTokens"]
            .as_u64()
            .zip(total["outputTokens"].as_u64());
        let (input_tokens, output_tokens) = match (self.baseline, totals) {
            (Some((input, output)), Some((total_in, total_out))) => (
                Some(total_in.saturating_sub(input)),
                Some(total_out.saturating_sub(output)),
            ),
            _ => (None, None),
        };
        if totals.is_some() {
            self.baseline = totals;
        }
        let turn = TurnUsage {
            input_tokens,
            output_tokens,
            context_tokens: usage["last"]["totalTokens"].as_u64(),
            context_window: usage["modelContextWindow"].as_u64(),
            session_cost_usd: None,
        };
        (!turn.is_empty()).then_some(turn)
    }

    pub fn ingest(&mut self, mut message: Value) -> Vec<AgentMessageBody> {
        redact(&mut message, &[]);
        let method = text(&message, "method");
        let p = &message["params"];
        let mut bodies = Vec::new();
        match method {
            "item/agentMessage/delta" => bodies.push(AgentMessageBody::AssistantDelta {
                text: text(p, "delta").into(),
            }),
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                self.output
                    .entry(text(p, "itemId").into())
                    .or_default()
                    .push_str(text(p, "delta"));
            }
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                bodies.push(AgentMessageBody::PermissionRequest {
                    request_id: approval_id(&message["id"]),
                    tool_name: if method.contains("commandExecution") {
                        "Bash"
                    } else {
                        "Edit"
                    }
                    .into(),
                    input: p.clone(),
                    suggestions: Vec::new(),
                });
            }
            "item/started" | "item/completed" => {
                let item = &p["item"];
                let id = text(item, "id").to_owned();
                let completed = method == "item/completed";
                let kind = text(item, "type");
                let name = match kind {
                    "commandExecution" => "Bash".to_owned(),
                    "fileChange" => "Edit".to_owned(),
                    "mcpToolCall" => {
                        format!("mcp__{}__{}", text(item, "server"), text(item, "tool"))
                    }
                    "agentMessage" if completed => {
                        bodies.push(AgentMessageBody::AssistantText {
                            text: text(item, "text").into(),
                        });
                        return bodies;
                    }
                    "agentMessage" | "userMessage" | "reasoning" | "plan" => return bodies,
                    _ => {
                        return vec![AgentMessageBody::System {
                            subtype: "raw".into(),
                            data: message,
                        }]
                    }
                };
                if self.started.insert(id.clone()) {
                    bodies.push(AgentMessageBody::ToolUse {
                        id: id.clone(),
                        name,
                        input: item.clone(),
                    });
                }
                if completed {
                    let deltas = self.output.remove(&id).unwrap_or_default();
                    let output = match kind {
                        "commandExecution" => item["aggregatedOutput"]
                            .as_str()
                            .unwrap_or(&deltas)
                            .to_owned(),
                        "fileChange" => item["changes"].to_string(),
                        _ => item
                            .get("error")
                            .filter(|v| !v.is_null())
                            .unwrap_or(&item["result"])
                            .to_string(),
                    };
                    let is_error = matches!(text(item, "status"), "failed" | "declined")
                        || item["exitCode"].as_i64().is_some_and(|n| n != 0)
                        || item.get("error").is_some_and(|v| !v.is_null())
                        || item["result"]["isError"].as_bool() == Some(true);
                    bodies.push(AgentMessageBody::ToolResult {
                        id,
                        output,
                        is_error,
                    });
                }
            }
            "thread/tokenUsage/updated" => self.usage = p["tokenUsage"].clone(),
            "turn/completed" => {
                let raw = self.usage.clone();
                let mut data = json!({"status":p["turn"]["status"], "error":p["turn"]["error"], "usage":raw, "cost_available":false});
                if let Some(usage) = self.turn_usage() {
                    data[TURN_USAGE_KEY] = json!(usage);
                }
                bodies.push(AgentMessageBody::System {
                    subtype: "codex_turn".into(),
                    data,
                });
                bodies.push(AgentMessageBody::Result {
                    cost_usd: 0.0,
                    duration_ms: p["turn"]["durationMs"].as_u64().unwrap_or_default(),
                    num_turns: 1,
                    session_id: text(p, "threadId").into(),
                });
                self.output.clear();
                self.started.clear();
            }
            "" | "turn/started" | "thread/started" | "thread/status/changed" => {}
            m if m.starts_with("item/reasoning/")
                || m.starts_with("item/plan/")
                || m == "turn/plan/updated" => {}
            _ => bodies.push(AgentMessageBody::System {
                subtype: "raw".into(),
                data: message,
            }),
        }
        bodies
    }
}
