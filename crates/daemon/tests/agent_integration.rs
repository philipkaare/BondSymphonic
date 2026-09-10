//! `agent.*` end to end against a fake `claude` that speaks the real
//! stream-json protocol.
//!
//! The fake (`tests/fixtures/fake_claude.py`) replays a recorded stream, waits
//! for a `control_response` where the real CLI would wait for a permission
//! answer, and echoes anything written to its stdin afterwards. It reaches the
//! daemon through `BS_CLAUDE_BIN`, which the adapter reads when it builds the
//! command line.
//!
//! `BS_CLAUDE_BIN` and `FAKE_CLAUDE_FIXTURE` are process-wide environment
//! variables that the daemon reads when it spawns the agent, so the tests in
//! this file take [`ENV`] for their whole duration and run one at a time.

mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::time::{Duration, Instant};

/// Serialises the tests: each one points `BS_CLAUDE_BIN` and
/// `FAKE_CLAUDE_FIXTURE` at what it needs, and both are process-wide.
///
/// A `tokio` mutex rather than the standard one because the guard is held
/// across the whole test, awaits included. It is runtime-agnostic, so one
/// static works for the separate runtime each `#[tokio::test]` builds.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The interpreter to run the fake with, or `None` when this host has none.
fn python() -> Option<&'static str> {
    // Windows ships a `python3` App Execution Alias that is not an interpreter,
    // so the real name is tried first there.
    let candidates: [&str; 2] = if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    };
    candidates.into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A path as `BS_CLAUDE_BIN` can carry it. `shell_words` reads backslashes as
/// escapes, so a Windows path goes in with forward slashes, which every Windows
/// API accepts.
fn arg_path(p: &std::path::Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Points the adapter at the fake replaying `fixture`.
fn use_fake_claude(py: &str, fixture: &str) {
    std::env::set_var(
        "BS_CLAUDE_BIN",
        format!(
            "\"{py}\" \"{}\"",
            arg_path(&fixture_dir().join("fake_claude.py"))
        ),
    );
    std::env::set_var(
        "FAKE_CLAUDE_FIXTURE",
        fixture_dir().join("claude-stream").join(fixture),
    );
    // Only the test that wants a slow turn sets this; clear whatever the
    // previous test left behind.
    std::env::remove_var("FAKE_CLAUDE_ECHO_DELAY");
}

fn agent_of(ev: &Event) -> Option<&AgentId> {
    match ev {
        Event::AgentMessage { agent_id, .. } | Event::AgentStateChanged { agent_id, .. } => {
            Some(agent_id)
        }
        _ => None,
    }
}

/// Collects the next `want` events for `ag`, or whatever arrived within
/// `limit`. `Client` only surfaces events it read while waiting for a response,
/// so a cheap request is what delivers them.
async fn next_agent_events(
    c: &mut Client,
    ag: &AgentId,
    want: usize,
    limit: Duration,
) -> Vec<Event> {
    let start = Instant::now();
    let mut out = Vec::new();
    loop {
        for (_, ev) in c.drain_events() {
            if agent_of(&ev) == Some(ag) {
                out.push(ev);
            }
        }
        if out.len() >= want || start.elapsed() >= limit {
            return out;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = c.call(Request::WorkspaceList {}).await;
    }
}

fn options() -> AgentStartOptions {
    AgentStartOptions {
        command: None,
        resume_session: None,
        model: None,
        permission_mode: None,
        api_key: None,
    }
}

async fn start_agent(c: &mut Client, ws: &WorkspaceId) -> AgentId {
    let v = c
        .call(Request::AgentStart(AgentStartParams {
            workspace_id: ws.clone(),
            adapter: AgentAdapterKind::Claude,
            options: options(),
        }))
        .await
        .unwrap();
    serde_json::from_value::<AgentStartResult>(v)
        .unwrap()
        .agent_id
}

fn bodies(events: &[Event]) -> Vec<&AgentMessageBody> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AgentMessage { message, .. } => Some(&message.body),
            _ => None,
        })
        .collect()
}

fn states(events: &[Event]) -> Vec<AgentState> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AgentStateChanged { state, .. } => Some(*state),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn claude_agent_streams_a_turn_and_records_history() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "tool_use_turn.ndjson");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    let ag = start_agent(&mut c, &ws.id).await;

    // init, Working, "Listing files.", tool_use, tool_result, "Two files.",
    // result, Idle.
    let events = next_agent_events(&mut c, &ag, 8, Duration::from_secs(20)).await;
    let turn = bodies(&events);
    let names: Vec<String> = turn.iter().map(|b| format!("{b:?}")).collect();
    assert!(
        turn.iter()
            .any(|b| matches!(b, AgentMessageBody::System { subtype, .. } if subtype == "init")),
        "{names:?}"
    );
    assert!(
        turn.iter()
            .any(|b| matches!(b, AgentMessageBody::ToolUse { name, .. } if name == "Bash")),
        "{names:?}"
    );
    assert!(
        turn.iter().any(
            |b| matches!(b, AgentMessageBody::ToolResult { output, .. } if output.contains("a.txt"))
        ),
        "{names:?}"
    );
    assert!(
        turn.iter()
            .any(|b| matches!(b, AgentMessageBody::Result { .. })),
        "{names:?}"
    );
    let states = states(&events);
    assert_eq!(states.first(), Some(&AgentState::Working), "{states:?}");
    assert_eq!(states.last(), Some(&AgentState::Idle), "{states:?}");

    // A user message is recorded before it is sent, then the fake echoes it.
    c.call(Request::AgentSend(AgentSendParams {
        agent_id: ag.clone(),
        text: "hello".into(),
    }))
    .await
    .unwrap();
    let more = next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;
    let more_bodies = bodies(&more);
    assert!(
        more_bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::UserText { text } if text == "hello")),
        "{more_bodies:?}"
    );
    assert!(
        more_bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::AssistantText { text } if text.contains("echo: hello"))),
        "{more_bodies:?}"
    );

    let v = c
        .call(Request::AgentHistory(AgentIdParams {
            agent_id: ag.clone(),
        }))
        .await
        .unwrap();
    let h: HistoryResult = serde_json::from_value(v).unwrap();
    let seqs: Vec<u64> = h.messages.iter().map(|m| m.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq monotonic: {seqs:?}"
    );
    assert!(h
        .messages
        .iter()
        .any(|m| matches!(&m.body, AgentMessageBody::UserText { text } if text == "hello")));

    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    // The list an older client reads is unchanged: bare ids.
    assert_eq!(info.agents, vec![ag.clone()]);
    // And beside it, the record that lets a client which restarted rebuild this
    // as a Claude tab rather than as a terminal.
    assert_eq!(info.agent_records.len(), 1);
    assert_eq!(info.agent_records[0].id, ag);
    assert_eq!(info.agent_records[0].adapter, AgentAdapterKind::Claude);

    c.call(Request::AgentStop(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();
    let last = next_agent_events(&mut c, &ag, 1, Duration::from_secs(20)).await;
    assert!(
        last.iter().any(|e| matches!(
            e,
            Event::AgentStateChanged {
                state: AgentState::Exited,
                ..
            }
        )),
        "{last:?}"
    );
    cancel.cancel();
}

#[tokio::test]
async fn permission_request_waits_for_the_reply() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "permission_turn.ndjson");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "perm").await;

    let ag = start_agent(&mut c, &ws.id).await;

    // init, Working, the request, WaitingPermission — and then nothing, because
    // the fake is blocked until we answer.
    let events = next_agent_events(&mut c, &ag, 4, Duration::from_secs(20)).await;
    let request_id = bodies(&events)
        .iter()
        .find_map(|b| match b {
            AgentMessageBody::PermissionRequest {
                request_id,
                tool_name,
                ..
            } if tool_name == "Bash" => Some(request_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a permission request: {events:?}"));
    assert_eq!(
        states(&events).last(),
        Some(&AgentState::WaitingPermission),
        "{events:?}"
    );

    // An id nobody is waiting on is a NotFound, not a stray line on stdin.
    let e = c
        .call(Request::AgentPermissionReply(AgentPermissionReplyParams {
            agent_id: ag.clone(),
            request_id: "req-nobody".into(),
            decision: PermissionDecision::Allow,
            updated_input: None,
            message: None,
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");

    c.call(Request::AgentPermissionReply(AgentPermissionReplyParams {
        agent_id: ag.clone(),
        request_id: request_id.clone(),
        decision: PermissionDecision::Allow,
        updated_input: None,
        message: None,
    }))
    .await
    .unwrap();

    // Working (ours), tool_use, tool_result, result, Idle.
    let after = next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;
    let after_bodies = bodies(&after);
    assert!(
        after_bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::ToolUse { name, .. } if name == "Bash")),
        "{after_bodies:?}"
    );
    assert!(
        after_bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::ToolResult { .. })),
        "{after_bodies:?}"
    );
    assert!(
        after_bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::Result { .. })),
        "{after_bodies:?}"
    );
    assert_eq!(states(&after).last(), Some(&AgentState::Idle), "{after:?}");

    // Answering the same request twice is a NotFound: it is no longer pending.
    let e = c
        .call(Request::AgentPermissionReply(AgentPermissionReplyParams {
            agent_id: ag.clone(),
            request_id: request_id.clone(),
            decision: PermissionDecision::Allow,
            updated_input: None,
            message: None,
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");

    // The answer is in the transcript, beside the question. This is what lets a
    // client that replays the history know the request was settled: without it
    // the request is the last word on disk, the bar goes back up on re-attach,
    // and the reply the user then sends is the `NotFound` just asserted above.
    let history = c
        .call(Request::AgentHistory(AgentIdParams {
            agent_id: ag.clone(),
        }))
        .await
        .unwrap();
    let history: HistoryResult = serde_json::from_value(history).unwrap();
    let marker = history
        .messages
        .iter()
        .find_map(|m| match &m.body {
            AgentMessageBody::System { subtype, data } if subtype == "permission_reply" => {
                Some(data.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no permission_reply in {:?}", history.messages));
    assert_eq!(marker["request_id"], serde_json::json!(request_id));
    assert_eq!(marker["decision"], serde_json::json!("allow"));
    // It comes after the request it answers, which is what makes a fold that
    // reads the transcript in order arrive at "answered".
    let position = |want: fn(&AgentMessageBody) -> bool| {
        history.messages.iter().position(|m| want(&m.body)).unwrap()
    };
    assert!(
        position(|b| matches!(b, AgentMessageBody::PermissionRequest { .. }))
            < position(
                |b| matches!(b, AgentMessageBody::System { subtype, .. } if subtype == "permission_reply")
            ),
        "{:?}",
        history.messages
    );
    // And the history carries the agent's live state, so a re-attaching client
    // does not start from `Idle` and paint a working agent as finished.
    assert_eq!(history.state, AgentState::Idle, "{history:?}");

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn agent_start_refuses_a_terminal_adapter_and_an_unready_workspace() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "simple_turn.ndjson");
    let (port, token, d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "refuse").await;

    // Terminals are opened with `pty.open`; the adapter kind exists so the IDE
    // can name one, not so the daemon can start it as an agent.
    let e = c
        .call(Request::AgentStart(AgentStartParams {
            workspace_id: ws.id.clone(),
            adapter: AgentAdapterKind::Terminal,
            options: options(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");

    d.set_state(&ws.id, WorkspaceState::SandboxDown).unwrap();
    let e = c
        .call(Request::AgentStart(AgentStartParams {
            workspace_id: ws.id.clone(),
            adapter: AgentAdapterKind::Claude,
            options: options(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");

    // An unknown agent is a NotFound on every request that names one.
    let e = c
        .call(Request::AgentHistory(AgentIdParams {
            agent_id: "ag_nope".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
    cancel.cancel();
}

#[tokio::test]
async fn destroying_a_workspace_stops_its_agents() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "simple_turn.ndjson");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "doomed").await;

    let ag = start_agent(&mut c, &ws.id).await;
    // Let the turn finish so the agent is not killed mid-stream.
    next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();

    // The agent is stopped inside `destroy`, so its last event is already
    // buffered by the time the response arrives.
    let seen: Vec<Event> = c
        .drain_events()
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| agent_of(e) == Some(&ag))
        .collect();
    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::AgentStateChanged {
                state: AgentState::Exited,
                ..
            }
        )),
        "the agent must be stopped before the workspace goes: {seen:?}"
    );

    // And the agent is gone with the workspace.
    let e = c
        .call(Request::AgentSend(AgentSendParams {
            agent_id: ag,
            text: "anyone there".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
    cancel.cancel();
}

/// `agent.interrupt` asks the CLI to abandon the turn through the control
/// protocol. The CLI acknowledges and ends the turn cleanly, so the agent goes
/// back to `Idle` rather than to `Error`, and the session stays usable.
#[tokio::test]
async fn interrupting_a_turn_ends_it_without_an_error() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "simple_turn.ndjson");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "stopme").await;

    let ag = start_agent(&mut c, &ws.id).await;
    // init, Working, the assistant text, the result, Idle.
    next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;

    c.call(Request::AgentInterrupt(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();

    let after = next_agent_events(&mut c, &ag, 2, Duration::from_secs(20)).await;
    assert!(
        bodies(&after)
            .iter()
            .any(|b| matches!(b, AgentMessageBody::Result { num_turns: 0, .. })),
        "the interrupted turn ends with a result: {after:?}"
    );
    assert_eq!(states(&after).last(), Some(&AgentState::Idle), "{after:?}");

    // The session is still usable afterwards.
    c.call(Request::AgentSend(AgentSendParams {
        agent_id: ag.clone(),
        text: "still there".into(),
    }))
    .await
    .unwrap();
    let more = next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;
    assert!(
        bodies(&more).iter().any(
            |b| matches!(b, AgentMessageBody::AssistantText { text } if text.contains("still there"))
        ),
        "{more:?}"
    );

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}

/// The SIGINT fallback belongs to the turn it was armed for.
///
/// `interrupt` writes a control request and arms a two-second SIGINT in case the
/// CLI never answers. The CLI usually answers in milliseconds, and the user's
/// next move is to type the corrected prompt straight away -- so two seconds
/// later the agent is busy again, on a *different* turn. A fallback that only
/// asks "is it working now?" fires into that new turn, and because the signal
/// goes to the whole process group a real `claude -p` dies rather than merely
/// abandoning the turn.
///
/// The kill is only reproducible where signals exist, so on Windows (whose noop
/// backend maps SIGTERM and SIGKILL and drops everything else) this asserts the
/// same thing trivially. The WSL run is the one that would have caught it.
#[tokio::test]
async fn an_interrupt_does_not_kill_the_turn_that_follows_it() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "simple_turn.ndjson");
    // Longer than the adapter's two-second interrupt grace, so the stale timer
    // has something live to fire into.
    std::env::set_var("FAKE_CLAUDE_ECHO_DELAY", "3");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "epoch").await;

    let ag = start_agent(&mut c, &ws.id).await;
    // init, Working, the assistant text, the result, Idle.
    next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;

    c.call(Request::AgentInterrupt(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();
    // The acknowledgement, which is what puts the agent back to Idle and ends
    // the turn the timer was armed for.
    let acked = next_agent_events(&mut c, &ag, 2, Duration::from_secs(20)).await;
    assert_eq!(states(&acked).last(), Some(&AgentState::Idle), "{acked:?}");

    // The corrected prompt, typed immediately, as a user would.
    c.call(Request::AgentSend(AgentSendParams {
        agent_id: ag.clone(),
        text: "slow".into(),
    }))
    .await
    .unwrap();

    // UserText, Working, the echo, the result, Idle -- and nothing else. The
    // stale timer fires somewhere in the middle of this wait.
    let after = next_agent_events(&mut c, &ag, 5, Duration::from_secs(30)).await;
    assert!(
        bodies(&after).iter().any(
            |b| matches!(b, AgentMessageBody::AssistantText { text } if text.contains("echo: slow"))
        ),
        "the new turn must finish: {after:?}"
    );
    assert!(
        !after.iter().any(|e| matches!(
            e,
            Event::AgentStateChanged {
                state: AgentState::Exited,
                ..
            }
        )),
        "the interrupt must not kill the turn that followed it: {after:?}"
    );
    assert_eq!(states(&after).last(), Some(&AgentState::Idle), "{after:?}");

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}
