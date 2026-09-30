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
//! is what the permission bar shows, and [`decide_permission`] is what lets
//! "always allow this tool" answer the next one without the user.
//!
//! This module must never import Qt types.

use bondsymphonic_proto::{AgentMessage, AgentMessageBody, AgentState, PermissionDecision};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};

/// Longest summary taken from a `Bash` command, in characters.
const BASH_SUMMARY_MAX: usize = 80;

/// Longest rendering of an unrecognised system message's data, in characters.
const SYSTEM_DATA_MAX: usize = 200;

/// How many live items one agent's transcript keeps before the oldest are
/// folded away, per IDE design §13.
///
/// The transcript view builds one widget per item, so an uncapped list is one
/// widget per message for the life of the tab -- tens of thousands on a day-long
/// session, all of them laid out on every resize. Nothing is thrown away: what
/// comes off the top goes into [`TranscriptItem::Earlier`], which puts it all
/// back when the user clicks it.
pub const MAX_LIVE_ITEMS: usize = 2_000;

/// The `System` subtype the daemon records an answered permission request
/// under. Wire format, shared with `crates/daemon/src/agents/claude.rs` by
/// value: the two crates share `bondsymphonic-proto`, not this string's
/// meaning.
const PERMISSION_REPLY: &str = "permission_reply";

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
    /// The fold: `count` older items held out of the list, and the label the
    /// view paints on the button that puts them back. Only ever the first item,
    /// and only ever one.
    Earlier {
        count: usize,
        text: String,
    },
}

/// The daemon's snake_case spelling of an agent state.
///
/// Here, beside [`TabAgentState`], because that is the type that carries the
/// word across the boundary: the `TranscriptModel` publishes it as its `state`
/// property and `GroupModel::setAgentStatus` parses it back, so both ends of
/// one string are defined in one place. It used to live beside the tab model,
/// which meant the pure transcript module had to reach into the application
/// state for it.
pub fn agent_state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::WaitingPermission => "waiting_permission",
        AgentState::Error => "error",
        AgentState::Exited => "exited",
    }
}

/// Inverse of [`agent_state_word`]. `None` for anything else, so a caller
/// decides what an unrecognised word means rather than being handed a guess.
pub fn parse_agent_state(word: &str) -> Option<AgentState> {
    match word {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "waiting_permission" => Some(AgentState::WaitingPermission),
        "error" => Some(AgentState::Error),
        "exited" => Some(AgentState::Exited),
        _ => None,
    }
}

/// The state the tab shows the agent in.
///
/// The daemon's [`AgentState`] plus the one thing it cannot say: that the IDE
/// has not been told. The state comes back with `agent.history`, and a history
/// request that failed -- a connection that dropped mid-request, an agent the
/// daemon cannot read the transcript of -- leaves the tab knowing nothing about
/// the agent.
///
/// Answering that with a fabricated `Idle` was wrong twice over. It painted a
/// running agent as finished, and it satisfied the guard at the end of
/// [`Transcript::replay`], which then took the permission bar down over a
/// request the daemon was still holding. `seq` de-duplication means that
/// request can never set `pending` again, so the tab said it was waiting, showed
/// no bar, and the agent blocked forever.
///
/// Not a variant of the wire enum: the daemon always knows what state its agent
/// is in, and this is the IDE's own ignorance rather than something the protocol
/// can carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabAgentState {
    /// Nothing has been heard yet, or `agent.history` failed.
    #[default]
    Unknown,
    Known(AgentState),
}

impl TabAgentState {
    /// The snake_case word the `state` property carries into C++.
    ///
    /// `Unknown` is a word of its own rather than an empty string: the view
    /// turns Stop off for an empty state, and an agent whose history could not
    /// be read is exactly one the user may still need to stop. The view has
    /// nothing else to say about a word it does not recognise, which is the
    /// right answer here.
    pub fn word(self) -> &'static str {
        match self {
            Self::Unknown => "unavailable",
            Self::Known(state) => agent_state_word(state),
        }
    }

    /// Whether the daemon has told the tab the agent is in `state`. `Unknown` is
    /// never any particular state, which is the whole point of it.
    pub fn is(self, state: AgentState) -> bool {
        self == Self::Known(state)
    }
}

/// The answer to give without asking the user, if there is one.
///
/// `always_allow` is the set of tool names the user answered with "always allow
/// this tool for this session". It is not persisted -- a session is one tab's
/// lifetime -- and it is held by the pane, not by the transcript, because a
/// reconnect rebuilds the transcript and must not cancel a pre-approval.
pub fn decide_permission(
    always_allow: &BTreeSet<String>,
    tool_name: &str,
) -> Option<PermissionDecision> {
    always_allow
        .contains(tool_name)
        .then_some(PermissionDecision::Allow)
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

/// One thing that reached the transcript from the live event stream. The
/// QObject maps the daemon's `Event` onto this, so [`Transcript::replay`] never
/// has to know about the event envelope and stays testable on its own.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveEvent {
    Message(AgentMessage),
    State(AgentState, Option<String>),
}

/// What [`Transcript::apply`] did, so the view can append or repaint one row
/// instead of rebuilding the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Appended(usize),
    Changed(usize),
    /// The item list was rebuilt around the message rather than extended, so
    /// every index the view is holding has moved. Only [`MAX_LIVE_ITEMS`]
    /// folding produces this, and the view answers it with a full rebuild.
    Reset,
    Nothing,
}

/// The state of one agent tab.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
    /// What the fold has taken off the top, oldest first, and whether the user
    /// has asked for it back. Private: the view reads `items`, and these two
    /// only ever move through [`Transcript::expand_earlier`].
    earlier: Vec<TranscriptItem>,
    earlier_expanded: bool,
    pub state: TabAgentState,
    /// The daemon's explanation of the state, or empty.
    pub state_detail: String,
    pub cost_usd: f64,
    pub cost_available: bool,
    pub turns: u32,
    pub session_id: Option<String>,
    pub pending: Option<PendingPermission>,
    queued_permissions: VecDeque<PendingPermission>,
    /// Highest `seq` applied. `None` until the first message, so a daemon that
    /// numbers its first message `0` does not have it swallowed by the
    /// duplicate check.
    last_seq: Option<u64>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            earlier: Vec::new(),
            earlier_expanded: false,
            state: TabAgentState::Unknown,
            state_detail: String::new(),
            cost_usd: 0.0,
            cost_available: true,
            turns: 0,
            session_id: None,
            pending: None,
            queued_permissions: VecDeque::new(),
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
                // Deliberately does *not* answer a pending request. Only what
                // the agent produced after a request proves it was let through;
                // a prompt is the user's, and it can reach the transcript while
                // the CLI is still blocked on the question -- an adapter that
                // replayed its opening turn before the first prompt does
                // exactly that. Clearing here took the bar down over a request
                // the daemon was still holding, and the answer then had nowhere
                // to go.
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
            AgentMessageBody::ToolUse { id, name, input } => {
                // The call is running, so it was allowed.
                self.answer_permission("");
                self.push(TranscriptItem::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    summary: tool_summary(name, input),
                    input_json: compact(input),
                    result: None,
                    is_error: false,
                    collapsed: true,
                })
            }
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
                let permission = PendingPermission {
                    request_id: request_id.clone(),
                    tool_name: tool_name.clone(),
                    summary: tool_summary(tool_name, input),
                    input_json: compact(input),
                };
                if self.pending.is_some() {
                    self.queued_permissions.push_back(permission);
                } else {
                    self.pending = Some(permission);
                }
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
                // The turn is over; nothing in it is still waiting to be
                // allowed.
                self.pending = None;
                self.queued_permissions.clear();
                self.cost_usd += cost_usd;
                self.turns += num_turns;
                self.session_id = Some(session_id.clone());
                if !self.cost_available {
                    return self.push(TranscriptItem::System {
                        text: format!("turn completed in {duration_ms} ms"),
                    });
                }
                self.push(TranscriptItem::Result {
                    cost_usd: *cost_usd,
                    duration_ms: *duration_ms,
                    num_turns: *num_turns,
                })
            }
            AgentMessageBody::System { subtype, data } if subtype == "codex_turn" => {
                self.cost_available = false;
                self.push(TranscriptItem::System {
                    text: format!("Codex turn: {}", truncate(&compact(data), SYSTEM_DATA_MAX)),
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
            // The daemon's record of an answer it accepted. This is what makes
            // a replayed transcript agree with the live one: the request is a
            // message and comes back from disk, so without the answer beside it
            // a re-attached tab raises a bar over a settled question -- and the
            // reply it sends names a request the adapter has already forgotten.
            AgentMessageBody::System { subtype, data } if subtype == PERMISSION_REPLY => {
                let request_id = data
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let decision = data
                    .get("decision")
                    .and_then(Value::as_str)
                    .unwrap_or("answered");
                self.answer_permission(request_id);
                self.push(TranscriptItem::System {
                    text: format!("permission {decision}"),
                })
            }
            AgentMessageBody::System { subtype, data } => self.push(TranscriptItem::System {
                text: format!("{subtype}: {}", truncate(&compact(data), SYSTEM_DATA_MAX)),
            }),
        }
    }

    /// Records the daemon's state. *Leaving* `WaitingPermission` clears the
    /// pending request: the daemon has moved on, so an answer would be for a
    /// request nobody is waiting on and the bar must come down.
    ///
    /// The guard is on the state being left, not on the state arriving, and
    /// the difference is the whole point. `pending` is set by a *message*,
    /// while the state is still whatever it was: during a replay the history's
    /// unanswered request lands with the state at its `Idle` default, and a
    /// buffered `Working` from before the snapshot follows it. Clearing on the
    /// arriving state alone would drop that request, and `seq` de-duplication
    /// means the same request can never set it again — a tab that says it is
    /// waiting, with no bar, and an agent that blocks forever.
    pub fn set_state(&mut self, state: AgentState, detail: Option<String>) {
        let leaving_wait =
            self.state.is(AgentState::WaitingPermission) && state != AgentState::WaitingPermission;
        if leaving_wait {
            self.pending = None;
            self.queued_permissions.clear();
        }
        self.record_state(state, detail);
    }

    /// The state `agent.history` came back with, applied before the fold in
    /// [`replay`](Transcript::replay).
    ///
    /// It does not clear the pending request, and neither do the state changes
    /// *inside* a replay: during a replay the order of the two sources cannot be
    /// trusted the way it can live. State changes carry no sequence number, so a
    /// buffered `Working` from before the history snapshot arrives after it, and
    /// treating that as "the daemon left the question behind" takes down a bar
    /// the daemon is still holding up. Inside a replay the last word belongs to
    /// the fold and to the single check at the end of it.
    pub fn set_state_from_history(&mut self, state: AgentState, detail: Option<String>) {
        self.record_state(state, detail);
    }

    fn record_state(&mut self, state: AgentState, detail: Option<String>) {
        self.state = TabAgentState::Known(state);
        self.state_detail = detail.unwrap_or_default();
    }

    /// Folds a history snapshot and everything that arrived while it was being
    /// fetched into this transcript, in that order, and reports what each
    /// input did.
    ///
    /// The caller sets the agent's state from the same `agent.history` reply
    /// *before* calling this, so the fold ends on the daemon's view of the
    /// agent rather than on this transcript's `Idle` default.
    ///
    /// This is the whole of the replay decision, kept here rather than in the
    /// QObject so it can be tested without a Qt event loop. The two halves
    /// overlap by design: a live message whose `seq` the history already
    /// carried yields [`Applied::Nothing`], which is what makes subscribing
    /// before the history request safe rather than merely early.
    pub fn replay(&mut self, history: &[AgentMessage], live: &[LiveEvent]) -> Vec<Applied> {
        let mut applied = Vec::with_capacity(history.len() + live.len());
        for message in history {
            applied.push(self.apply(message));
        }
        for event in live {
            applied.push(match event {
                LiveEvent::Message(message) => self.apply(message),
                LiveEvent::State(state, detail) => {
                    // See `set_state_from_history`: inside a replay a state
                    // change records the state and decides nothing.
                    self.set_state_from_history(*state, detail.clone());
                    Applied::Nothing
                }
            });
        }
        // The last word on whether a bar goes up. The fold above answers a
        // request that something later in the transcript settled; this covers
        // the rest -- a daemon that has moved on without recording an answer,
        // an agent that exited while a question was open, or a history whose
        // answer predates this daemon. The state is the daemon's own, carried
        // back by `agent.history` and applied before the fold, so `Idle` here
        // means the agent really is idle rather than that nothing has been
        // heard yet.
        //
        // `Unknown` is that second case and settles nothing. A history that
        // failed says nothing about the agent, and taking the bar down on it
        // would strand a request the daemon is still holding -- see
        // [`TabAgentState`].
        let moved_on = matches!(
            self.state,
            TabAgentState::Known(state) if state != AgentState::WaitingPermission
        );
        if moved_on {
            self.pending = None;
            self.queued_permissions.clear();
        }
        applied
    }

    /// Marks the pending permission request answered.
    ///
    /// An empty `request_id` answers whatever is pending, which is what the
    /// defensive arms want: a tool call that is running, or a turn that has
    /// finished, says the question is settled without naming it. A
    /// `request_id` that names some *other* request leaves the bar alone.
    pub fn answer_permission(&mut self, request_id: &str) {
        // Codex may run other tools while a parallel approval is outstanding.
        // Only an explicit reply or the final state settles those requests.
        if request_id.is_empty() {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.request_id.starts_with("codex:"))
            {
                return;
            }
            self.queued_permissions.clear();
        } else {
            self.queued_permissions
                .retain(|p| p.request_id != request_id);
        }
        let answered = self
            .pending
            .as_ref()
            .is_some_and(|p| request_id.is_empty() || p.request_id == request_id);
        if answered {
            self.pending = self.queued_permissions.pop_front();
        }
    }

    /// Takes the pending request when the session has already been told to
    /// always allow its tool, leaving nothing pending.
    ///
    /// This is the auto-answer: the caller sends the reply, and because the
    /// request was taken rather than published, the permission bar never shows
    /// one the user has already pre-answered. `None` leaves `pending` alone.
    ///
    /// `always_allow` is passed in rather than held here. It used to be both:
    /// a copy on the transcript, which this read, and a copy on the model,
    /// which survives the re-attach that a reconnect makes -- two sets kept in
    /// step by hand at two call sites, and a third one added anywhere would
    /// have been the bug this is named for.
    pub fn take_auto_allowed(
        &mut self,
        always_allow: &BTreeSet<String>,
    ) -> Option<(PendingPermission, PermissionDecision)> {
        let tool = self.pending.as_ref().map(|p| p.tool_name.clone())?;
        let decision = decide_permission(always_allow, &tool)?;
        let pending = self.pending.take()?;
        self.pending = self.queued_permissions.pop_front();
        Some((pending, decision))
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
        if self.fold_earlier() {
            // Every index moved, so the appended-at-N the caller would
            // otherwise get would name the wrong row.
            return Applied::Reset;
        }
        Applied::Appended(self.items.len() - 1)
    }

    /// Moves the oldest live items into `earlier` until at most
    /// [`MAX_LIVE_ITEMS`] are left, and keeps the block that stands for them
    /// at the head of the list. True when anything moved.
    ///
    /// Does nothing once the user has expanded the block: they asked to see the
    /// whole conversation, and re-folding it under them would make the button
    /// undo itself.
    fn fold_earlier(&mut self) -> bool {
        if self.earlier_expanded {
            return false;
        }
        let had_block = matches!(self.items.first(), Some(TranscriptItem::Earlier { .. }));
        let start = usize::from(had_block);
        let live = self.items.len() - start;
        if live <= MAX_LIVE_ITEMS {
            return false;
        }
        let moved: Vec<TranscriptItem> = self
            .items
            .drain(start..start + (live - MAX_LIVE_ITEMS))
            .collect();
        self.earlier.extend(moved);
        let block = earlier_block(self.earlier.len());
        if had_block {
            self.items[0] = block;
        } else {
            self.items.insert(0, block);
        }
        true
    }

    /// Puts every folded item back, in order, and stops folding for the rest of
    /// this transcript's life. False when nothing is being held back.
    ///
    /// The cap is a default for a list nobody asked to read all of, not a
    /// quota: a user who clicked "load earlier" has said which they want, and
    /// a second fold would take it away again while they were reading.
    pub fn expand_earlier(&mut self) -> bool {
        if self.earlier.is_empty() {
            return false;
        }
        if matches!(self.items.first(), Some(TranscriptItem::Earlier { .. })) {
            self.items.remove(0);
        }
        let mut all = std::mem::take(&mut self.earlier);
        all.append(&mut self.items);
        self.items = all;
        self.earlier_expanded = true;
        true
    }

    /// How many items are folded away, for a caller that wants to say so
    /// without reading the block out of `items`.
    pub fn earlier_count(&self) -> usize {
        self.earlier.len()
    }
}

/// The block that stands for `count` folded items.
fn earlier_block(count: usize) -> TranscriptItem {
    TranscriptItem::Earlier {
        count,
        text: format!("Load earlier ({count})"),
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
