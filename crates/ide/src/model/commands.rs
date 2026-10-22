//! Command metadata advertised by the attached harness, without Qt dependencies.
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
}

/// `None` means this system message does not update the catalog. An explicit
/// empty list clears it; malformed entries never become completions.
pub fn from_system(subtype: &str, data: &Value) -> Option<Vec<Command>> {
    let entries = match subtype {
        "agent_commands" => data.get("commands")?.as_array()?,
        "init" => data.get("slash_commands")?.as_array()?,
        _ => return None,
    };
    let mut seen = HashSet::new();
    Some(entries.iter().take(512).filter_map(|entry| {
        let name = entry.as_str().or_else(|| entry.get("name")?.as_str())?
            .trim_start_matches('/');
        if name.is_empty() || name.len() > 128
            || name.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/')
            || !seen.insert(name.to_owned()) {
            return None;
        }
        Some(Command {
            name: name.to_owned(),
            description: entry.get("description").and_then(Value::as_str)
                .unwrap_or_default().chars().take(1024).collect(),
            argument_hint: entry.get("argument_hint").and_then(Value::as_str)
                .map(|s| s.chars().take(256).collect()),
        })
    }).collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeTarget {
    Attach(bondsymphonic_proto::AgentId),
    Session(String),
}

/// Resolve against current daemon records, not a tab's possibly stale replay.
/// An explicit native session id need not have been created in this IDE.
pub fn resume_target(
    records: &[bondsymphonic_proto::AgentSummary],
    adapter: bondsymphonic_proto::AgentAdapterKind,
    requested: &str,
) -> Result<ResumeTarget, String> {
    use bondsymphonic_proto::AgentState;
    let requested = requested.trim();
    let matching = || records.iter().rev().filter(|record| record.adapter == adapter);
    if let Some(live) = matching().find(|record| {
        matches!(record.state, AgentState::Idle | AgentState::Working | AgentState::WaitingPermission)
            && (requested.is_empty()
            || record.session_id.as_deref() == Some(requested))
    }) {
        return Ok(ResumeTarget::Attach(live.id.clone()));
    }
    if !requested.is_empty() {
        return Ok(ResumeTarget::Session(requested.to_owned()));
    }
    matching().find_map(|record| record.session_id.as_ref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| ResumeTarget::Session(id.clone())))
        .ok_or_else(|| "No saved session is available for this agent yet.".to_owned())
}
