//! Transcript mapping for the pinned Codex app-server protocol.
use bondsymphonic_proto::AgentMessageBody;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct CodexStream {
    output: HashMap<String, String>,
    started: HashSet<String>,
    usage: Value,
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
                bodies.push(AgentMessageBody::System {
                    subtype: "codex_turn".into(),
                    data: json!({"status":p["turn"]["status"], "error":p["turn"]["error"], "usage":self.usage, "cost_available":false}),
                });
                bodies.push(AgentMessageBody::Result {
                    cost_usd: 0.0,
                    duration_ms: p["turn"]["durationMs"].as_u64().unwrap_or_default(),
                    num_turns: 1,
                    session_id: text(p, "threadId").into(),
                });
                self.output.clear();
                self.started.clear();
                self.usage = Value::Null;
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
