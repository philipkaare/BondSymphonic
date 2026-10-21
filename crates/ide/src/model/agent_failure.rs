//! What an agent pane says when one of its requests fails.
//!
//! The text a failure arrives with is the client's own rendering of it --
//! `rpc: Conflict: another operation is running in this workspace`, or an
//! agent CLI's JSON-RPC error wrapped in the daemon's `AgentError` -- which is
//! exact and useless to someone who just wanted the agent to start. This turns
//! it into a sentence about what happened and what to do, keeps the original
//! for the banner's Details disclosure, and says which of the banner's actions
//! apply. Pure, so every mapping is tested without a window.
//!
//! The input is the `ClientError` display text rather than the typed error:
//! that is what every reporter in the controller and the transcript model
//! already has in hand, and its shape (`rpc: <Code>: <message>`) is fixed by
//! `ClientError` and `RpcError`'s own `Display`.

use serde::Serialize;

/// What kind of failure this is, as far as the user is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Another start or lifecycle operation holds the workspace.
    Busy,
    /// A CLI or login the agent needs is not there.
    Prereq,
    /// The agent's CLI could not sign in.
    Auth,
    /// The two ends of a conversation do not understand each other: the
    /// agent CLI and the daemon, or the IDE and the daemon.
    Protocol,
    /// The request was not answered in time.
    Timeout,
    /// The IDE has no daemon connection.
    Disconnected,
    /// The workspace's sandbox is down or not ready.
    Sandbox,
    /// The agent the request was for no longer exists.
    Gone,
    /// The agent itself reported an error.
    Agent,
    /// Anything else.
    Unknown,
}

/// A failure, ready for the banner. Crosses to C++ as JSON ([`Self::to_json`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentFailure {
    /// The daemon method that failed, such as `agent.start`.
    pub op: String,
    pub kind: FailureKind,
    /// What did not happen, in the user's terms: "The agent didn't start."
    pub title: String,
    /// Why, and what to do about it. No protocol vocabulary.
    pub detail: String,
    /// The text the failure arrived with, for the Details disclosure.
    pub technical: String,
    /// Whether re-issuing the same request could help.
    pub retry: bool,
    /// Whether starting without the conversation being resumed is offered:
    /// only for a start that tried to resume one and failed in the agent.
    pub start_fresh: bool,
    /// Whether the log is the next place to look.
    pub open_log: bool,
    /// For `agent.start`, the options to retry with: the ones that failed.
    pub retry_options: Option<String>,
    /// For a [`start_fresh`](Self::start_fresh) offer, the same options
    /// without `resume_session`.
    pub fresh_options: Option<String>,
}

impl AgentFailure {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The error codes `RpcError` displays as, which is how they appear in the
/// text after `rpc: `.
const CODES: &[&str] = &[
    "Unauthorized",
    "InvalidParams",
    "NotFound",
    "GitError",
    "SandboxError",
    "PrereqMissing",
    "AgentError",
    "IoError",
    "Conflict",
    "Internal",
];

/// The names an agent's own error is prefixed with by its adapter.
const AGENT_NAMES: &[&str] = &["Claude Code", "Claude", "Codex", "OpenCode"];

/// Turns the failure of `op` with `message` into what the banner says.
/// `options_json` is the `AgentStartOptions` an `agent.start` was sent with,
/// when there was one; it is what Retry and Start fresh send again.
pub fn describe(op: &str, message: &str, options_json: Option<&str>) -> AgentFailure {
    let technical = message.trim().to_owned();
    let mut text = technical.as_str();
    if let Some(rest) = text
        .strip_prefix(op)
        .and_then(|r| r.strip_prefix(" failed: "))
    {
        text = rest;
    }
    let (code, body) = split_code(text);
    let (who, said) = split_agent(body);
    let rpc = said.and_then(json_rpc_error);
    let cleaned = match (&rpc, said) {
        (Some((_, m)), _) => m.clone(),
        (None, Some(s)) => s.to_owned(),
        (None, None) => body.to_owned(),
    };
    let who_name = who.unwrap_or("The agent");
    let lower = text.to_lowercase();

    let mut ide_daemon_mismatch = false;
    let (kind, detail) = if lower.contains("daemon speaks protocol") {
        ide_daemon_mismatch = true;
        (
            FailureKind::Protocol,
            "The IDE and the daemon are different versions. Update or reinstall BondSymphonic so \
             they match."
                .to_owned(),
        )
    } else if code.is_none() && lower.ends_with("request timed out") {
        let detail = match op {
            "agent.start" => {
                "The daemon didn't answer in time, and the agent may still be starting. Check \
                 the pane before starting it again; if nothing changes, reconnect or open the \
                 log to see what it is waiting on."
            }
            "agent.send" => {
                "The daemon didn't answer in time, and your message may still reach the agent. \
                 Check the pane before sending it again; if nothing changes, reconnect or open \
                 the log."
            }
            _ => {
                "The daemon didn't answer in time. Retry in a moment, or open the log to see \
                 what it is waiting on."
            }
        };
        (FailureKind::Timeout, detail.to_owned())
    } else if code.is_none()
        && (lower == "disconnected"
            || lower.starts_with("io: ")
            || lower.starts_with("not connected to the daemon")
            || lower.starts_with("daemon connection lost"))
        || code == Some("Unauthorized")
    {
        (
            FailureKind::Disconnected,
            "BondSymphonic isn't connected to its daemon. It reconnects by itself; retry once the \
             status bar says it is connected."
                .to_owned(),
        )
    } else if code.is_none() && lower.starts_with("protocol: ") {
        (
            FailureKind::Protocol,
            "The daemon sent something the IDE couldn't read. Open the log for the details."
                .to_owned(),
        )
    } else if code == Some("Conflict") {
        (
            FailureKind::Busy,
            "This workspace is busy with another start or operation. Wait a moment, then retry."
                .to_owned(),
        )
    } else if code == Some("PrereqMissing") {
        (
            FailureKind::Prereq,
            format!(
                "Something this agent needs isn't set up yet. {}",
                sentence(&cleaned)
            ),
        )
    } else if is_auth(&lower) {
        (
            FailureKind::Auth,
            format!(
                "{who_name} couldn't sign in. Check its sign-in under Settings > Setup, then \
                 retry."
            ),
        )
    } else if rpc
        .as_ref()
        .is_some_and(|(c, _)| (-32700..=-32600).contains(c))
    {
        (
            FailureKind::Protocol,
            format!(
                "{who_name} didn't understand what it was sent: {} Its CLI may be a version \
                 BondSymphonic doesn't support yet; updating it may help.",
                sentence(&cleaned)
            ),
        )
    } else if code == Some("SandboxError")
        || (code == Some("InvalidParams")
            && (lower.contains("is not ready") || lower.contains("sandbox")))
    {
        (
            FailureKind::Sandbox,
            "The workspace's sandbox isn't running. Retry once it is back, or open the log if \
             this keeps happening."
                .to_owned(),
        )
    } else if code == Some("NotFound") && body.starts_with("agent ") {
        (
            FailureKind::Gone,
            "That agent is no longer running. Restart it to carry on.".to_owned(),
        )
    } else if code == Some("AgentError") {
        (
            FailureKind::Agent,
            format!("{who_name} reported: {}", sentence(&cleaned)),
        )
    } else {
        (FailureKind::Unknown, cleaned.clone())
    };

    let start = op == "agent.start";
    let resuming = options_json.is_some_and(resumes_a_session);
    let start_fresh = start
        && resuming
        && !ide_daemon_mismatch
        && matches!(
            kind,
            FailureKind::Agent | FailureKind::Protocol | FailureKind::Unknown | FailureKind::Gone
        );
    // A start or a message that timed out may still complete in the daemon;
    // repeating it would start a second agent or send the prompt twice.
    let may_still_complete =
        kind == FailureKind::Timeout && matches!(op, "agent.start" | "agent.send");
    let retry = !ide_daemon_mismatch
        && kind != FailureKind::Gone
        && op != "agent.history"
        && !may_still_complete;
    AgentFailure {
        op: op.to_owned(),
        kind,
        title: title_for(op).to_owned(),
        detail,
        technical,
        retry,
        start_fresh,
        open_log: matches!(
            kind,
            FailureKind::Timeout
                | FailureKind::Disconnected
                | FailureKind::Sandbox
                | FailureKind::Protocol
                | FailureKind::Agent
                | FailureKind::Unknown
        ),
        retry_options: if start {
            options_json.map(str::to_owned)
        } else {
            None
        },
        fresh_options: if start_fresh {
            options_json.map(fresh_start_options)
        } else {
            None
        },
    }
}

/// `options_json` without `resume_session`, everything else copied through.
/// A string that is not a JSON object becomes `{}`: the daemon's defaults are
/// a working agent.
pub fn fresh_start_options(options_json: &str) -> String {
    let mut options = serde_json::from_str::<serde_json::Value>(options_json)
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = options.as_object_mut() {
        map.remove("resume_session");
    }
    options.to_string()
}

/// Whether a failed request lost a race rather than failed: Send now or
/// Discard on a queued message the agent had just sent or dropped. The row the
/// daemon writes for that settles the item, so nothing is raised.
pub fn is_settled_race(err: &crate::client::ClientError) -> bool {
    matches!(err, crate::client::ClientError::Rpc(e) if bondsymphonic_proto::is_queued_prompt_settled(e))
}

fn resumes_a_session(options_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(options_json)
        .ok()
        .and_then(|v| {
            v.get("resume_session")
                .and_then(|s| s.as_str())
                .map(|s| !s.is_empty())
        })
        .unwrap_or(false)
}

fn title_for(op: &str) -> &'static str {
    match op {
        "agent.start" => "The agent didn't start.",
        "agent.send" => "Your message wasn't sent.",
        "agent.permission_reply" => "Your answer to the permission request didn't reach the agent.",
        "agent.interrupt" => "The agent couldn't be interrupted.",
        "agent.stop" => "The agent couldn't be stopped.",
        "agent.history" => "The conversation couldn't be loaded.",
        "agent.queued_prompt" => "The queued message couldn't be changed.",
        _ => "The agent request failed.",
    }
}

/// `rpc: <Code>: <rest>` into the code and the rest, or no code and the
/// whole text.
fn split_code(text: &str) -> (Option<&'static str>, &str) {
    let rest = text.strip_prefix("rpc: ").unwrap_or(text);
    for code in CODES {
        if let Some(body) = rest.strip_prefix(code).and_then(|r| r.strip_prefix(": ")) {
            return (Some(code), body.trim());
        }
    }
    (None, text)
}

/// `Codex: <what it said>` into the agent's name and what it said.
fn split_agent(body: &str) -> (Option<&'static str>, Option<&str>) {
    for name in AGENT_NAMES {
        if let Some(said) = body.strip_prefix(name).and_then(|r| r.strip_prefix(": ")) {
            return (Some(name), Some(said.trim()));
        }
        // "Claude Code is not logged in": the name, without a colon.
        if body.starts_with(name) {
            return (Some(name), None);
        }
    }
    (None, None)
}

/// A JSON-RPC error object's code and message, when `said` is one.
fn json_rpc_error(said: &str) -> Option<(i64, String)> {
    let v: serde_json::Value = serde_json::from_str(said).ok()?;
    let v = v.get("error").unwrap_or(&v);
    let code = v.get("code")?.as_i64()?;
    let message = v.get("message")?.as_str()?.to_owned();
    Some((code, message))
}

fn is_auth(lower: &str) -> bool {
    [
        "not logged in",
        "log in",
        "login",
        "unauthorized",
        "401",
        "authentication",
        "api key",
        "sign in",
        "signed in",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

/// `s` as a sentence: a full stop added when it ends without one.
fn sentence(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() || s.ends_with(['.', '!', '?']) {
        s.to_owned()
    } else {
        format!("{s}.")
    }
}
