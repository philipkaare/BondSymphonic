//! One Claude agent's transcript: the items the view paints, the running
//! cost, and the permission request waiting for an answer.
//!
//! The daemon streams [`AgentMessage`]s; this module turns that stream into a
//! list the view can rebuild itself from. Three things make that more than a
//! `Vec::push`:
//!
//! * **Partial messages.** `--include-partial-messages` sends the assistant's
//!   text as deltas and then repeats it in full. Deltas coalesce into the one
//!   streaming item they belong to, and the final text replaces it, so a
//!   sentence is one bubble rather than one bubble per token.
//! * **Replay.** A tab that opens fetches `agent.history` while live events are
//!   already arriving. Both are applied here and `seq` de-duplicates them, so
//!   the overlap costs nothing.
//! * **Tool results.** They arrive separately from the call they answer and
//!   are attached to it by id, so a tool card is one frame that fills in.
//!
//! What a permission request means is decided here too: [`Transcript::pending`]
//! is what the permission bar shows, and [`Transcript::decide_permission`] is
//! what lets "always allow this tool" answer the next one without the user.
//!
//! This module must never import Qt types.

use bondsymphonic_proto::{AgentMessage, AgentMessageBody, AgentState, PermissionDecision};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Longest summary taken from a `Bash` command, in characters.
const BASH_SUMMARY_MAX: usize = 80;

/// Longest rendering of an unrecognised system message's data, in characters.
const SYSTEM_DATA_MAX: usize = 200;

/// One frame in the transcript. Serialised with a `kind` tag because the view
/// dispatches on it to decide which widget to build.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        text: String,
    },
    /// `streaming` is true while deltas are still arriving, which is what lets
    /// the next delta extend this item instead of starting another.
    Assistant {
        text: String,
        streaming: bool,
    },
    ToolUse {
        id: String,
        name: String,
        summary: String,
        input_json: String,
        result: Option<String>,
        is_error: bool,
        collapsed: bool,
    },
    Result {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
    System {
        text: String,
    },
}

/// A tool call waiting for the user's answer. Everything the permission bar
/// shows is here, so it never parses a raw message itself.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingPermission {
    pub request_id: String,
    pub tool_name: String,
    pub summary: String,
    pub input_json: String,
}

/// What [`Transcript::apply`] did, so the view can append or repaint one row
/// instead of rebuilding the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Appended(usize),
    Changed(usize),
    Nothing,
}

/// The state of one agent tab.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
    pub state: AgentState,
    /// The daemon's explanation of the state, or empty.
    pub state_detail: String,
    pub cost_usd: f64,
    pub turns: u32,
    pub session_id: Option<String>,
    pub pending: Option<PendingPermission>,
    /// Tool names the user answered with "always allow this tool for this
    /// session". Not persisted: a session is one tab's lifetime.
    pub always_allow: BTreeSet<String>,
    /// Highest `seq` applied. `None` until the first message, so a daemon that
    /// numbers its first message `0` does not have it swallowed by the
    /// duplicate check.
    last_seq: Option<u64>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            state: AgentState::Idle,
            state_detail: String::new(),
            cost_usd: 0.0,
            turns: 0,
            session_id: None,
            pending: None,
            always_allow: BTreeSet::new(),
            last_seq: None,
        }
    }
}

impl Transcript {
    /// Folds one message in. Messages already applied (`seq` at or below the
    /// highest seen) change nothing, which is what makes replaying
    /// `agent.history` over live events safe.
    pub fn apply(&mut self, msg: &AgentMessage) -> Applied {
        if self.last_seq.is_some_and(|last| msg.seq <= last) {
            return Applied::Nothing;
        }
        self.last_seq = Some(msg.seq);
        match &msg.body {
            AgentMessageBody::UserText { text } => {
                self.push(TranscriptItem::User { text: text.clone() })
            }
            AgentMessageBody::AssistantDelta { text } => match self.streaming_assistant() {
                Some((i, existing)) => {
                    existing.push_str(text);
                    Applied::Changed(i)
                }
                None => self.push(TranscriptItem::Assistant {
                    text: text.clone(),
                    streaming: true,
                }),
            },
            AgentMessageBody::AssistantText { text } => match self.streaming_assistant() {
                Some((i, existing)) => {
                    // The full text supersedes what the deltas built: the CLI
                    // may have revised it, and it is the authoritative copy.
                    existing.clear();
                    existing.push_str(text);
                    if let TranscriptItem::Assistant { streaming, .. } = &mut self.items[i] {
                        *streaming = false;
                    }
                    Applied::Changed(i)
                }
                None => self.push(TranscriptItem::Assistant {
                    text: text.clone(),
                    streaming: false,
                }),
            },
            AgentMessageBody::ToolUse { id, name, input } => self.push(TranscriptItem::ToolUse {
                id: id.clone(),
                name: name.clone(),
                summary: tool_summary(name, input),
                input_json: compact(input),
                result: None,
                is_error: false,
                collapsed: true,
            }),
            AgentMessageBody::ToolResult {
                id,
                output,
                is_error,
            } => match self.find_tool_use(id) {
                Some(i) => {
                    if let TranscriptItem::ToolUse {
                        result,
                        is_error: e,
                        ..
                    } = &mut self.items[i]
                    {
                        *result = Some(output.clone());
                        *e = *is_error;
                    }
                    Applied::Changed(i)
                }
                // Nothing is dropped: an unattachable result is still shown,
                // because it is usually the interesting half of a bug.
                None => self.push(TranscriptItem::System {
                    text: format!("tool result for unknown id {id}: {output}"),
                }),
            },
            AgentMessageBody::PermissionRequest {
                request_id,
                tool_name,
                input,
                ..
            } => {
                self.pending = Some(PendingPermission {
                    request_id: request_id.clone(),
                    tool_name: tool_name.clone(),
                    summary: tool_summary(tool_name, input),
                    input_json: compact(input),
                });
                // No item: the permission bar shows it, and the tool card
                // appears once the call actually runs.
                Applied::Nothing
            }
            AgentMessageBody::Result {
                cost_usd,
                duration_ms,
                num_turns,
                session_id,
            } => {
                self.cost_usd += cost_usd;
                self.turns += num_turns;
                self.session_id = Some(session_id.clone());
                self.push(TranscriptItem::Result {
                    cost_usd: *cost_usd,
                    duration_ms: *duration_ms,
                    num_turns: *num_turns,
                })
            }
            AgentMessageBody::System { subtype, data } if subtype == "init" => {
                let session = data
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let model = data
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !session.is_empty() {
                    self.session_id = Some(session.to_owned());
                }
                self.push(TranscriptItem::System {
                    text: format!("session {session}, model {model}"),
                })
            }
            AgentMessageBody::System { subtype, data } => self.push(TranscriptItem::System {
                text: format!("{subtype}: {}", truncate(&compact(data), SYSTEM_DATA_MAX)),
            }),
        }
    }

    /// Records the daemon's state. Leaving `WaitingPermission` clears the
    /// pending request: the daemon has moved on, so an answer would be for a
    /// request nobody is waiting on, and the bar must come down.
    pub fn set_state(&mut self, state: AgentState, detail: Option<String>) {
        if state != AgentState::WaitingPermission {
            self.pending = None;
        }
        self.state = state;
        self.state_detail = detail.unwrap_or_default();
    }

    /// The answer to give without asking the user, if there is one.
    pub fn decide_permission(&self, tool_name: &str) -> Option<PermissionDecision> {
        self.always_allow
            .contains(tool_name)
            .then_some(PermissionDecision::Allow)
    }

    /// Every item, for a view rebuilding itself from scratch.
    pub fn items_json(&self) -> String {
        serde_json::to_string(&self.items).unwrap_or_else(|_| "[]".to_owned())
    }

    /// One item, or empty for an index the transcript does not have.
    pub fn item_json(&self, i: usize) -> String {
        match self.items.get(i) {
            Some(item) => serde_json::to_string(item).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Collapses or expands one tool card. False for anything that is not a
    /// tool call, or an index out of range.
    pub fn set_collapsed(&mut self, i: usize, value: bool) -> bool {
        match self.items.get_mut(i) {
            Some(TranscriptItem::ToolUse { collapsed, .. }) => {
                *collapsed = value;
                true
            }
            _ => false,
        }
    }

    /// The last item's text buffer, if it is an assistant message still being
    /// streamed. Only the last one: an assistant message is finished by
    /// anything that follows it.
    fn streaming_assistant(&mut self) -> Option<(usize, &mut String)> {
        let i = self.items.len().checked_sub(1)?;
        match &mut self.items[i] {
            TranscriptItem::Assistant {
                text,
                streaming: true,
            } => Some((i, text)),
            _ => None,
        }
    }

    /// The tool call with `id`, searched from the end: ids repeat across a
    /// long session, and the newest call is the one being answered.
    fn find_tool_use(&self, id: &str) -> Option<usize> {
        self.items.iter().rposition(|item| {
            matches!(item, TranscriptItem::ToolUse { id: existing, result: None, .. } if existing == id)
        })
    }

    fn push(&mut self, item: TranscriptItem) -> Applied {
        self.items.push(item);
        Applied::Appended(self.items.len() - 1)
    }
}

/// One line naming what a tool call is about, for the collapsed card and the
/// permission bar. The fields are the ones Claude Code's own tools use; an
/// unrecognised tool falls back to its first string argument, which is right
/// far more often than it is wrong, and to the tool's name when it has none.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let summary = match name {
        "Edit" | "Write" | "Read" | "MultiEdit" | "NotebookEdit" => field("file_path"),
        "Bash" | "BashOutput" => field("command"),
        "Grep" | "Glob" => field("pattern"),
        _ => input
            .as_object()
            .and_then(|map| map.values().find_map(Value::as_str)),
    };
    let Some(summary) = summary else {
        return name.to_owned();
    };
    // One line: a summary sits on a single row of the card header.
    let first_line = summary.lines().next().unwrap_or_default().trim();
    if first_line.is_empty() {
        return name.to_owned();
    }
    match name {
        "Bash" | "BashOutput" => truncate(first_line, BASH_SUMMARY_MAX),
        _ => first_line.to_owned(),
    }
}

/// `value` on one line. Used for the tool card's input pane and for the
/// bounded rendering of a system message's data.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

/// The first `max` characters of `text`. Counts characters, not bytes, so a
/// command containing non-ASCII cannot be cut mid-character.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte, _)) => text[..byte].to_owned(),
        None => text.to_owned(),
    }
}
