//! ACP session update mapping.
use super::codex_stream::{redact, text};
use bondsymphonic_proto::AgentMessageBody;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct OpenCodeStream {
    tools: HashMap<String, Value>,
    completed: HashSet<String>,
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

impl OpenCodeStream {
    pub fn ingest(&mut self, mut update: Value) -> Vec<AgentMessageBody> {
        redact(&mut update, &[]);
        match text(&update, "sessionUpdate") {
            "assistant_message_chunk" if update["content"]["type"] == "text" => {
                vec![AgentMessageBody::AssistantDelta {
                    text: text(&update["content"], "text").into(),
                }]
            }
            "tool_call" | "tool_call_update" => {
                let id = text(&update, "toolCallId").to_owned();
                if id.is_empty() {
                    return vec![];
                }
                let mut out = vec![];
                let first = !self.tools.contains_key(&id);
                let tool = self
                    .tools
                    .entry(id.clone())
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(fields) = update.as_object() {
                    for (key, value) in fields {
                        tool[key] = value.clone();
                    }
                }
                if first {
                    out.push(AgentMessageBody::ToolUse {
                        id: id.clone(),
                        name: text(tool, "title").to_owned(),
                        input: tool
                            .get("rawInput")
                            .cloned()
                            .unwrap_or_else(|| tool.clone()),
                    });
                }
                if matches!(text(tool, "status"), "completed" | "failed")
                    && self.completed.insert(id.clone())
                {
                    out.push(AgentMessageBody::ToolResult {
                        id,
                        output: tool
                            .get("rawOutput")
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
                            })
                            .unwrap_or_else(|| content_text(&tool["content"])),
                        is_error: text(tool, "status") == "failed",
                    });
                }
                out
            }
            // Preserve metadata without presenting internal reasoning as assistant text.
            _ => vec![AgentMessageBody::System {
                subtype: "opencode_update".into(),
                data: update,
            }],
        }
    }
}
