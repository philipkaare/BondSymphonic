//! What an agent pane says when one of its requests fails: the error the
//! daemon (or the client) produced, turned into a plain sentence, the
//! technical text kept for the Details disclosure, and the actions that make
//! sense for it.

use bondsymphonic_ide::model::agent_failure::{describe, fresh_start_options, FailureKind};

/// No protocol vocabulary in what the user reads first.
fn assert_plain(text: &str) {
    for word in [
        "rpc:",
        "Conflict:",
        "AgentError",
        "InvalidParams",
        "{\"code\"",
        "-32600",
    ] {
        assert!(!text.contains(word), "{word:?} leaked into {text:?}");
    }
}

#[test]
fn a_busy_workspace_says_so_and_offers_a_retry_with_the_same_options() {
    let raw = "rpc: Conflict: another operation is running in this workspace";
    let f = describe("agent.start", raw, Some(r#"{"model":"opus"}"#));
    assert_eq!(f.kind, FailureKind::Busy);
    assert_eq!(f.op, "agent.start");
    assert_plain(&f.title);
    assert_plain(&f.detail);
    assert!(f.title.contains("didn't start"), "{}", f.title);
    assert!(f.detail.to_lowercase().contains("busy"), "{}", f.detail);
    assert_eq!(f.technical, raw);
    assert!(f.retry);
    assert_eq!(f.retry_options.as_deref(), Some(r#"{"model":"opus"}"#));
    assert!(!f.start_fresh);
    assert_eq!(f.fresh_options, None);

    let also = describe(
        "agent.start",
        "rpc: Conflict: another agent is already starting in workspace ws_1",
        None,
    );
    assert_eq!(also.kind, FailureKind::Busy);
}

#[test]
fn a_json_rpc_error_from_the_agent_is_unwrapped_and_read_as_a_protocol_mismatch() {
    let raw = r#"rpc: AgentError: Codex: {"code":-32600,"message":"Invalid request: unknown field `foo`"}"#;
    let f = describe("agent.start", raw, None);
    assert_eq!(f.kind, FailureKind::Protocol);
    assert_plain(&f.title);
    assert_plain(&f.detail);
    assert!(f.detail.contains("Codex"), "{}", f.detail);
    assert!(
        f.detail.contains("Invalid request: unknown field `foo`"),
        "{}",
        f.detail
    );
    assert_eq!(f.technical, raw);
    assert!(f.open_log);
}

#[test]
fn a_missing_prerequisite_passes_the_daemons_instruction_through() {
    let f = describe(
        "agent.start",
        "rpc: PrereqMissing: Install OpenCode in Settings > Agents",
        None,
    );
    assert_eq!(f.kind, FailureKind::Prereq);
    assert!(
        f.detail.contains("Install OpenCode in Settings > Agents"),
        "{}",
        f.detail
    );
    assert_plain(&f.detail);
    assert!(f.retry);
}

#[test]
fn a_sign_in_failure_points_at_setup() {
    for raw in [
        "rpc: AgentError: Claude Code is not logged in. Run /login",
        r#"rpc: AgentError: Codex: {"code":-32000,"message":"401 Unauthorized"}"#,
        "rpc: AgentError: OpenCode: authentication failed for provider openai",
    ] {
        let f = describe("agent.start", raw, None);
        assert_eq!(f.kind, FailureKind::Auth, "{raw}");
        assert!(f.detail.contains("Settings"), "{}", f.detail);
        assert_plain(&f.detail);
    }
}

#[test]
fn a_timeout_and_a_lost_connection_are_retryable_and_point_at_the_log() {
    let f = describe("agent.interrupt", "request timed out", None);
    assert_eq!(f.kind, FailureKind::Timeout);
    assert!(f.retry && f.open_log);

    for raw in [
        "disconnected",
        "not connected to the daemon",
        "daemon connection lost",
        "io: connection reset",
    ] {
        let f = describe("agent.start", raw, None);
        assert_eq!(f.kind, FailureKind::Disconnected, "{raw}");
        assert!(f.open_log, "{raw}");
    }
}

#[test]
fn a_sandbox_that_is_down_or_not_ready_reads_as_the_sandbox() {
    for raw in [
        "rpc: SandboxError: bwrap exited with status 1",
        "rpc: InvalidParams: workspace ws_1 is not ready",
        "rpc: InvalidParams: workspace sandbox is stopping",
    ] {
        let f = describe("agent.start", raw, None);
        assert_eq!(f.kind, FailureKind::Sandbox, "{raw}");
        assert!(f.retry && f.open_log, "{raw}");
        assert_plain(&f.title);
        assert_plain(&f.detail);
    }
}

#[test]
fn a_protocol_mismatch_between_ide_and_daemon_is_not_retried() {
    let f = describe(
        "agent.start",
        "daemon speaks protocol 4, this IDE speaks 3",
        None,
    );
    assert_eq!(f.kind, FailureKind::Protocol);
    assert!(!f.retry);
    assert!(f.detail.to_lowercase().contains("update"), "{}", f.detail);
}

#[test]
fn a_failed_resume_offers_a_fresh_start_without_the_session() {
    let options = r#"{"model":"opus","resume_session":"sess-9","permission_mode":"ask"}"#;
    let f = describe(
        "agent.start",
        "rpc: AgentError: Codex: no rollout found for thread id sess-9",
        Some(options),
    );
    assert!(f.retry);
    assert!(f.start_fresh);
    assert_eq!(f.retry_options.as_deref(), Some(options));
    let fresh: serde_json::Value =
        serde_json::from_str(f.fresh_options.as_deref().unwrap()).unwrap();
    assert_eq!(fresh.get("resume_session"), None, "{fresh}");
    assert_eq!(fresh["model"], "opus");
    assert_eq!(fresh["permission_mode"], "ask");

    // A busy workspace is not the session's fault: starting fresh would throw
    // the conversation away for nothing.
    let busy = describe(
        "agent.start",
        "rpc: Conflict: another operation is running in this workspace",
        Some(options),
    );
    assert!(!busy.start_fresh);
    // And a start that resumed nothing has nothing to start fresh from.
    let plain = describe(
        "agent.start",
        "rpc: AgentError: boom",
        Some(r#"{"model":"opus"}"#),
    );
    assert!(!plain.start_fresh);
}

#[test]
fn an_unknown_error_keeps_its_sentence_and_offers_the_log() {
    let f = describe("agent.start", "rpc: Internal: kaboom happened", None);
    assert_eq!(f.kind, FailureKind::Unknown);
    assert_eq!(f.detail, "kaboom happened");
    assert!(f.open_log);
    assert!(f.retry);
}

#[test]
fn transcript_requests_are_named_for_what_the_user_did() {
    let f = describe(
        "agent.send",
        "agent.send failed: rpc: NotFound: agent ag_1",
        None,
    );
    assert_eq!(f.op, "agent.send");
    assert_eq!(f.kind, FailureKind::Gone);
    assert!(f.title.contains("message"), "{}", f.title);
    assert_plain(&f.detail);
    assert_eq!(f.technical, "agent.send failed: rpc: NotFound: agent ag_1");
    // There is no agent to send it to again.
    assert!(!f.retry);

    let f = describe(
        "agent.permission_reply",
        "agent.permission_reply failed: request timed out",
        None,
    );
    assert!(f.title.contains("permission"), "{}", f.title);
    assert!(f.retry);
    let f = describe("agent.stop", "agent.stop failed: request timed out", None);
    assert!(f.title.contains("stop"), "{}", f.title);
    // Only a start has options to retry with.
    assert_eq!(f.retry_options, None);
}

#[test]
fn the_view_crosses_to_cpp_as_json() {
    let f = describe("agent.start", "rpc: SandboxError: bwrap exited", Some("{}"));
    let v: serde_json::Value = serde_json::from_str(&f.to_json()).unwrap();
    assert_eq!(v["op"], "agent.start");
    assert_eq!(v["kind"], "sandbox");
    assert_eq!(v["retry"], true);
    assert_eq!(v["start_fresh"], false);
    assert_eq!(v["open_log"], true);
    assert_eq!(v["retry_options"], "{}");
    assert!(v["title"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(v["technical"].is_string());
}

#[test]
fn fresh_start_options_drop_only_the_session() {
    let fresh = fresh_start_options(r#"{"resume_session":"s","model":"m","future":1}"#);
    let v: serde_json::Value = serde_json::from_str(&fresh).unwrap();
    assert_eq!(v, serde_json::json!({"model":"m","future":1}));
    // Not an object: the daemon's defaults are a working agent.
    assert_eq!(fresh_start_options("nonsense"), "{}");
}

#[test]
fn a_failed_queued_message_action_says_what_did_not_happen() {
    let f = describe(
        "agent.queued_prompt",
        "rpc: InvalidParams: no queued prompt q1",
        None,
    );
    assert_eq!(f.op, "agent.queued_prompt");
    assert_eq!(f.title, "The queued message couldn't be changed.");
    assert_plain(&f.title);
    assert_plain(&f.detail);
}

/// A start or a message the daemon did not answer in time may still go
/// through: Retry would start a second agent or send the prompt twice, so the
/// pane offers none and says to look before trying again.
#[test]
fn a_timed_out_start_or_send_offers_no_retry_and_says_it_may_still_complete() {
    for op in ["agent.start", "agent.send"] {
        let f = describe(op, "request timed out", Some("{}"));
        assert_eq!(f.kind, FailureKind::Timeout, "{op}");
        assert!(!f.retry, "{op}");
        assert!(f.open_log, "{op}");
        assert!(f.detail.contains("may still"), "{op}: {}", f.detail);
        assert!(f.detail.contains("reconnect"), "{op}: {}", f.detail);
        assert!(!f.detail.contains("retry"), "{op}: {}", f.detail);
        assert_plain(&f.detail);
    }
}

#[test]
fn a_send_timeout_reports_unconfirmed_delivery_instead_of_a_rejected_message() {
    for raw in ["request timed out", "agent.send failed: request timed out"] {
        let f = describe("agent.send", raw, None);
        assert_eq!(f.kind, FailureKind::Timeout);
        assert_eq!(f.title, "Message delivery wasn't confirmed.");
        assert!(f.detail.contains("may already have reached"));
        assert!(!f.retry, "a retry could deliver the same message twice");
        assert_eq!(f.technical, raw);
    }
}

/// Send now or Discard on a message the agent has just taken off the queue is
/// a race the user lost by a moment: the transcript's own row settles it, and
/// no banner goes up. Any other failure of the request still does.
#[test]
fn a_queued_message_already_settled_is_not_a_failure() {
    use bondsymphonic_ide::client::ClientError;
    use bondsymphonic_ide::model::agent_failure::is_settled_race;
    use bondsymphonic_proto::RpcError;

    assert!(is_settled_race(&ClientError::Rpc(
        RpcError::queued_prompt_settled()
    )));
    assert!(!is_settled_race(&ClientError::Rpc(
        RpcError::invalid_params("no such agent")
    )));
    assert!(!is_settled_race(&ClientError::Timeout));
    assert!(!is_settled_race(&ClientError::Disconnected));
}
