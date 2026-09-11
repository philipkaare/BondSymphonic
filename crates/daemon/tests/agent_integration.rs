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

/// Every knob the two fixture programs read. Each test sets the ones it wants
/// and the helpers below clear all of them first, so nothing leaks from one
/// test into the next through the process environment.
const FIXTURE_KNOBS: [&str; 8] = [
    "FAKE_CLAUDE_ECHO_DELAY",
    "FAKE_CLAUDE_NO_WAIT",
    "FAKE_CLAUDE_VERSION_DELAY",
    "FAKE_CLAUDE_VERSION_DELAY_MARKER",
    "DYING_CLAUDE_STDERR",
    "DYING_CLAUDE_STDERR_DELAY",
    "DYING_CLAUDE_STDERR_FROM_CHILD",
    "DYING_CLAUDE_EXIT",
];

fn clear_knobs() {
    for knob in FIXTURE_KNOBS {
        std::env::remove_var(knob);
    }
}

/// Points the adapter at the fake replaying `fixture`.
fn use_fake_claude(py: &str, fixture: &str) {
    use_program(py, &fixture_dir().join("fake_claude.py"), Some(fixture));
}

/// The same, from a *copy* of the fake at `at`.
///
/// The daemon probes `--version` once per program, so a test that needs the
/// probe to actually run -- the two that are about what the probe does -- has to
/// name a program no earlier test in this binary has already probed.
fn use_fake_claude_copy(py: &str, at: &std::path::Path, fixture: &str) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::copy(fixture_dir().join("fake_claude.py"), at).unwrap();
    use_program(py, at, Some(fixture));
}

/// Points the adapter at the fixture program that exits instead of holding the
/// conversation open. `fixture` is the stream it prints on the way out, if any.
fn use_dying_claude(py: &str, fixture: Option<&str>) {
    use_program(py, &fixture_dir().join("dying_claude.py"), fixture);
}

fn use_program(py: &str, program: &std::path::Path, fixture: Option<&str>) {
    std::env::set_var(
        "BS_CLAUDE_BIN",
        format!("\"{py}\" \"{}\"", arg_path(program)),
    );
    match fixture {
        Some(fixture) => std::env::set_var(
            "FAKE_CLAUDE_FIXTURE",
            fixture_dir().join("claude-stream").join(fixture),
        ),
        None => std::env::remove_var("FAKE_CLAUDE_FIXTURE"),
    }
    clear_knobs();
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
    try_start_agent(c, ws).await.unwrap()
}

/// `agent.start` with the error kept, for the tests that are about a start
/// being refused.
async fn try_start_agent(c: &mut Client, ws: &WorkspaceId) -> Result<AgentId, RpcError> {
    let v = c
        .call(Request::AgentStart(AgentStartParams {
            workspace_id: ws.clone(),
            adapter: AgentAdapterKind::Claude,
            options: options(),
        }))
        .await?;
    Ok(serde_json::from_value::<AgentStartResult>(v)
        .unwrap()
        .agent_id)
}

/// The machine-readable half of an error: `data.reason`, which is what a client
/// matches on. These are wire values, so the tests spell them out.
fn reason_of(e: &RpcError) -> &str {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("")
}

async fn history_of(c: &mut Client, ag: &AgentId) -> HistoryResult {
    let v = c
        .call(Request::AgentHistory(AgentIdParams {
            agent_id: ag.clone(),
        }))
        .await
        .unwrap();
    serde_json::from_value(v).unwrap()
}

async fn reply(
    c: &mut Client,
    ag: &AgentId,
    request_id: &str,
) -> Result<serde_json::Value, RpcError> {
    c.call(Request::AgentPermissionReply(AgentPermissionReplyParams {
        agent_id: ag.clone(),
        request_id: request_id.to_owned(),
        decision: PermissionDecision::Allow,
        updated_input: None,
        message: None,
    }))
    .await
}

/// The request ids of every permission request in `events`, in order.
fn request_ids(events: &[Event]) -> Vec<String> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            AgentMessageBody::PermissionRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// True when the transcript holds a permission request for `request_id` and no
/// answer to it: the question is still open and the client must still show it.
fn still_pending(h: &HistoryResult, request_id: &str) -> bool {
    let asked = h.messages.iter().any(
        |m| matches!(&m.body, AgentMessageBody::PermissionRequest { request_id: r, .. } if r == request_id),
    );
    let answered = h.messages.iter().any(|m| {
        matches!(&m.body, AgentMessageBody::System { subtype, data }
            if subtype == "permission_reply" && data["request_id"] == serde_json::json!(request_id))
    });
    asked && !answered
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

/// The daemon log of the first real run said
/// `claude: Ignoring 9 permissions.allow entries from .claude/settings.json:
/// this workspace has not been trusted`. Claude Code keeps that consent per
/// project directory in `$HOME/.claude.json`, and a sandbox home is a fresh one
/// per workspace.
///
/// Asked of the agent rather than of the file the daemon wrote, because what
/// matters is the path *as the process sees it*: the worktree is bound into the
/// sandbox at its host path and is the working directory the CLI starts in, and
/// a trust entry under any other spelling would be ignored exactly as silently.
#[tokio::test]
async fn the_agents_home_trusts_the_worktree_it_runs_in() {
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
    let ws = create_ws(&mut c, &repo, "trusted").await;
    let ag = start_agent(&mut c, &ws.id).await;
    // Let the fixture finish, so what comes back next is the answer to the ask.
    next_agent_events(&mut c, &ag, 4, Duration::from_secs(20)).await;

    c.call(Request::AgentSend(AgentSendParams {
        agent_id: ag.clone(),
        text: "print-claude-json".into(),
    }))
    .await
    .unwrap();

    let events = next_agent_events(&mut c, &ag, 3, Duration::from_secs(20)).await;
    let answer = bodies(&events)
        .into_iter()
        .find_map(|b| match b {
            AgentMessageBody::AssistantText { text } => Some(text.clone()),
            _ => None,
        })
        .expect("the fake answers with the file it read");
    let json = answer.trim_start_matches("echo: ");
    let v: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("the agent could not read a JSON .claude.json: {e}"));
    // The file is a copy of the daemon user's own and is not printed on failure:
    // it is their account state, and the only part this test is about is which
    // projects it trusts.
    let trusted: Vec<&String> = v["projects"]
        .as_object()
        .map(|p| p.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        v["projects"][&ws.worktree_path]["hasTrustDialogAccepted"], true,
        "the worktree the agent runs in must be a trusted project; trusted: {trusted:?}"
    );

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// State ownership and lifecycle (review findings AG1-AG7).
//
// The rule these pin: the stdout reader is the only thing that publishes an
// agent's state. `agent.send` and `agent.permission_reply` write a line and
// record a transcript entry; what the agent *is* doing is whatever its own
// output last said.
// ---------------------------------------------------------------------------

/// AG1. A model that proposes two tools in one assistant message has two
/// permission requests outstanding at once. Answering the first used to publish
/// `Working`, which takes the bar down over a question nobody has answered: the
/// second request can never be answered from the UI, and the CLI waits for it
/// for ever.
#[tokio::test]
async fn answering_one_permission_leaves_the_other_one_pending() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_fake_claude(py, "two_permissions_turn.ndjson");
    // Both requests go out without waiting, which is what the real CLI does
    // when one assistant message asks for two tools.
    std::env::set_var("FAKE_CLAUDE_NO_WAIT", "1");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "twoperm").await;

    let ag = start_agent(&mut c, &ws.id).await;

    // init, Working, the text, two tool_use, two requests, WaitingPermission x2.
    let events = next_agent_events(&mut c, &ag, 9, Duration::from_secs(20)).await;
    let ids = request_ids(&events);
    assert_eq!(ids.len(), 2, "both requests must arrive: {events:?}");
    assert_eq!(
        states(&events).last(),
        Some(&AgentState::WaitingPermission),
        "{events:?}"
    );

    reply(&mut c, &ag, &ids[0]).await.unwrap();

    // Nothing the daemon did on its own moved the agent: the second question is
    // still open, so the agent is still waiting on a permission.
    let h = history_of(&mut c, &ag).await;
    assert_eq!(
        h.state,
        AgentState::WaitingPermission,
        "answering one of two must not report the agent as working: {:?}",
        h.state
    );
    assert!(
        still_pending(&h, &ids[1]),
        "the second request must still be open: {:?}",
        h.messages
    );
    assert!(
        !still_pending(&h, &ids[0]),
        "the first must be settled: {:?}",
        h.messages
    );
    let since = next_agent_events(&mut c, &ag, 0, Duration::from_millis(200)).await;
    assert!(
        !states(&since).contains(&AgentState::Working),
        "the reply itself must publish no state: {since:?}"
    );

    // And the second is still answerable, which is the whole point.
    reply(&mut c, &ag, &ids[1]).await.unwrap();

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}

/// AG2. A turn typed while the agent is waiting for a permission answer cannot
/// be delivered: the CLI is blocked on the question and will not read it. It is
/// refused, with a reason the IDE can match on, and nothing is recorded.
#[tokio::test]
async fn sending_a_turn_while_a_permission_is_open_is_refused() {
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
    let ws = create_ws(&mut c, &repo, "sendwait").await;

    let ag = start_agent(&mut c, &ws.id).await;
    let events = next_agent_events(&mut c, &ag, 4, Duration::from_secs(20)).await;
    assert_eq!(
        states(&events).last(),
        Some(&AgentState::WaitingPermission),
        "{events:?}"
    );

    let e = c
        .call(Request::AgentSend(AgentSendParams {
            agent_id: ag.clone(),
            text: "never mind".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::AgentError, "{e:?}");
    assert_eq!(reason_of(&e), "waiting_permission", "{e:?}");

    let h = history_of(&mut c, &ag).await;
    assert_eq!(h.state, AgentState::WaitingPermission, "{:?}", h.state);
    assert!(
        !h.messages.iter().any(
            |m| matches!(&m.body, AgentMessageBody::UserText { text } if text == "never mind")
        ),
        "a refused turn must not reach the transcript: {:?}",
        h.messages
    );

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}

/// AG3. What the agent said on its way out is the useful half of an exit: "not
/// logged in", "invalid API key", a stack trace. The stderr reader is a task of
/// its own, so the exit detail used to be whatever it happened to have read by
/// the time the process died -- often nothing. Looped, because a race that is
/// usually won is still a bug.
#[tokio::test]
async fn an_exit_detail_always_carries_the_agents_last_words() {
    const ROUNDS: usize = 20;

    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_dying_claude(py, None);
    // The agent's last words are written by a process that inherits its stderr
    // and outlives it by a moment, so the daemon has the exit code in hand
    // while they are still on their way. That is the ordinary shape of it --
    // the CLI hands its stderr to every tool it runs -- and it is the one
    // ordering that is not a coin toss: text written just before the exit is
    // usually, but only usually, read in time, and "usually" is what kept this
    // from being a bug anybody saw.
    let mut noise: Vec<String> = (0..400).map(|i| format!("  at frame {i}")).collect();
    noise.push("boom".to_owned());
    std::env::set_var("DYING_CLAUDE_STDERR", noise.join("\n"));
    std::env::set_var("DYING_CLAUDE_STDERR_FROM_CHILD", "1");
    // Long enough that the daemon has the exit code first every time, short
    // enough to sit well inside the second the adapter waits: the fixture holds
    // the writer's interpreter start outside this window, so it is the whole of
    // the gap.
    std::env::set_var("DYING_CLAUDE_STDERR_DELAY", "0.1");
    std::env::set_var("DYING_CLAUDE_EXIT", "3");
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "boom").await;

    let mut details = Vec::new();
    for round in 0..ROUNDS {
        let ag = start_agent(&mut c, &ws.id).await;
        let events = next_agent_events(&mut c, &ag, 1, Duration::from_secs(20)).await;
        let detail = events
            .iter()
            .find_map(|e| match e {
                Event::AgentStateChanged {
                    state: AgentState::Exited | AgentState::Error,
                    detail,
                    ..
                } => Some(detail.clone().unwrap_or_default()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("round {round}: no exit event: {events:?}"));
        details.push(detail);
    }
    let missing: Vec<(usize, &String)> = details
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.contains("boom"))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {ROUNDS} exits lost the agent's stderr: {missing:?}",
        missing.len()
    );
    assert!(
        details.iter().all(|d| d.contains("exit code 3")),
        "every exit must name the code too: {details:?}"
    );
    cancel.cancel();
}

/// Makes a worktree impossible to remove, so a `workspace.destroy` gets all the
/// way to the git step and fails there. `None` where the host will not play
/// along, which is a skip rather than a pass.
struct RemovalBlock {
    #[cfg(windows)]
    _held: std::fs::File,
    #[cfg(unix)]
    dir: std::path::PathBuf,
}

#[cfg(windows)]
fn block_removal(worktree: &std::path::Path) -> Option<RemovalBlock> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = worktree.join("held-open.txt");
    std::fs::write(&path, "held open by the test").ok()?;
    // Share mode 0: every other open of this name fails, deletion included.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .ok()?;
    Some(RemovalBlock { _held: held })
}

#[cfg(unix)]
fn block_removal(worktree: &std::path::Path) -> Option<RemovalBlock> {
    use std::os::unix::fs::PermissionsExt;
    let dir = worktree.join("held-open");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join("file"), "held open by the test").ok()?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).ok()?;
    // root ignores the mode, and then there is nothing here to arrange.
    if std::fs::remove_file(dir.join("file")).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    Some(RemovalBlock { dir })
}

#[cfg(unix)]
impl Drop for RemovalBlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// AG4. A destroy that fails late leaves the workspace behind in `Error`, and
/// its agents' transcripts are the one thing still worth having out of it. They
/// used to be dropped from the map before the destroy had done anything, so the
/// history was gone while the workspace was still there.
#[tokio::test]
async fn a_failed_destroy_keeps_its_agents_history() {
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
    let ws = create_ws(&mut c, &repo, "stubborn").await;

    let ag = start_agent(&mut c, &ws.id).await;
    // Let the turn finish, so there is a transcript worth keeping.
    next_agent_events(&mut c, &ag, 5, Duration::from_secs(20)).await;
    let before = history_of(&mut c, &ag).await;
    assert!(!before.messages.is_empty());

    let Some(block) = block_removal(std::path::Path::new(&ws.worktree_path)) else {
        eprintln!("SKIP: this host will not make a directory undeletable");
        cancel.cancel();
        return;
    };

    let e = c
        .call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: true,
        }))
        .await
        .expect_err("the worktree cannot be removed, so the destroy must fail");
    assert_ne!(e.code, ErrorCode::NotFound, "{e:?}");

    let after = history_of(&mut c, &ag).await;
    assert_eq!(
        after.messages, before.messages,
        "the transcript must outlive a destroy that did not happen"
    );
    // The workspace is still there, in error, and still lists its agent.
    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(info.agents, vec![ag.clone()], "{:?}", info.agents);

    drop(block);
    // And once the obstacle is gone the destroy does work, taking the agent.
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    let e = c
        .call(Request::AgentHistory(AgentIdParams { agent_id: ag }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
    cancel.cancel();
}

/// AG5. Starting an agent seeds the workspace home and copies the repository's
/// `[claude] settings` into it before it spawns anything. Two starts at once
/// interleave those writes, and the settings file -- which is unlinked and then
/// created -- can be left missing or half written for the agent that wins.
/// One start per workspace at a time; the second is a `Conflict`.
#[tokio::test]
async fn two_starts_at_once_in_one_workspace_do_not_race() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    // A repository that pins its agents' settings, so a start really does write
    // into the workspace home on its way to the spawn.
    std::fs::write(
        repo.join("bondsymphonic.toml"),
        "[claude]\nsettings = \"claude-settings.json\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("claude-settings.json"), "{\"pinned\":true}").unwrap();
    common::commit_all(&repo, &[], "pin the agent settings");

    // A copy of the fake nothing has probed yet, and a probe that takes its
    // time: that is what holds the first start inside the critical section long
    // enough for the second to arrive while it is there.
    let fake = dir.path().join("race-fake").join("fake_claude.py");
    use_fake_claude_copy(py, &fake, "simple_turn.ndjson");
    std::env::set_var("FAKE_CLAUDE_VERSION_DELAY", "3");

    let (port, token, d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "race").await;
    let mut c2 = Client::connect(port, &token).await;

    let (a, b) = tokio::join!(
        try_start_agent(&mut c, &ws.id),
        try_start_agent(&mut c2, &ws.id)
    );
    let (ok, refused) = match (a, b) {
        (Ok(id), Err(e)) | (Err(e), Ok(id)) => (id, e),
        (Ok(x), Ok(y)) => panic!("both starts were allowed into the workspace: {x}, {y}"),
        (Err(x), Err(y)) => panic!("neither start succeeded: {x:?}, {y:?}"),
    };
    assert_eq!(refused.code, ErrorCode::Conflict, "{refused:?}");
    assert_eq!(reason_of(&refused), "agent_running", "{refused:?}");

    // The winner's settings landed whole.
    let settings = d.dirs.home(&ws.id).join(".claude").join("settings.json");
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap_or_default(),
        "{\"pinned\":true}",
        "the settings the winner wrote must be intact: {}",
        settings.display()
    );
    // And the workspace holds exactly the one agent that started.
    assert_eq!(d.agents.agents_of(&ws.id), vec![ok.clone()]);

    c.call(Request::AgentStop(AgentIdParams { agent_id: ok }))
        .await
        .unwrap();
    cancel.cancel();
}

/// AG6. An agent that ends its turn with an error and then exits is gone. It
/// used to stay in `Error` for ever: the tab never said the process had died,
/// and the next turn the user typed came back as a broken pipe rather than as
/// "this agent has ended".
#[tokio::test]
async fn an_errored_turn_that_ends_the_process_is_reported_as_an_exit() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    use_dying_claude(py, Some("error_result_turn.ndjson"));
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "dead").await;

    let ag = start_agent(&mut c, &ws.id).await;
    // init, Working, the text, the result, Error, Exited.
    let events = next_agent_events(&mut c, &ag, 6, Duration::from_secs(20)).await;
    assert_eq!(
        states(&events).last(),
        Some(&AgentState::Exited),
        "the process is gone, so the agent has exited: {events:?}"
    );
    let detail = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::AgentStateChanged {
                state: AgentState::Exited,
                detail,
                ..
            } => Some(detail.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap();
    assert!(
        detail.contains("Not logged in"),
        "the exit must keep the reason the turn failed: {detail:?}"
    );

    let h = history_of(&mut c, &ag).await;
    assert_eq!(h.state, AgentState::Exited, "{:?}", h.state);
    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(info.agent_records[0].state, AgentState::Exited);

    let e = c
        .call(Request::AgentSend(AgentSendParams {
            agent_id: ag.clone(),
            text: "are you there".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(reason_of(&e), "agent_exited", "{e:?}");

    // And the same answer once the agent has been stopped as well, which is
    // where the process really is gone: the state is what decides, not whether
    // there is still a handle to write to.
    c.call(Request::AgentStop(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();
    let e = c
        .call(Request::AgentSend(AgentSendParams {
            agent_id: ag.clone(),
            text: "still there".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(reason_of(&e), "agent_exited", "{e:?}");
    cancel.cancel();
}

/// AG7. The daemon runs `claude --version` before it starts an agent. A CLI
/// that never answers -- a half-installed binary waiting on a terminal, a
/// filesystem that has gone away -- used to hang `agent.start` for ever, and the
/// answer was remembered for the life of the daemon, so a fixed install could
/// not be picked up without a restart.
#[tokio::test]
async fn a_version_probe_that_hangs_fails_the_start_and_is_not_remembered() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let fake = dir.path().join("probe-fake").join("fake_claude.py");
    use_fake_claude_copy(py, &fake, "simple_turn.ndjson");
    // The same program, slow while the marker is there and prompt once it is
    // gone: that is how "the failure is not remembered" is observable at all.
    let marker = dir.path().join("hang-the-probe");
    std::fs::write(&marker, "").unwrap();
    std::env::set_var("FAKE_CLAUDE_VERSION_DELAY", "60");
    std::env::set_var("FAKE_CLAUDE_VERSION_DELAY_MARKER", arg_path(&marker));

    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "probe").await;

    let started = Instant::now();
    let e = try_start_agent(&mut c, &ws.id)
        .await
        .expect_err("a probe that never answers cannot start an agent");
    let waited = started.elapsed();
    assert_eq!(reason_of(&e), "claude_probe_timeout", "{e:?}");
    assert!(
        waited >= Duration::from_secs(8) && waited < Duration::from_secs(30),
        "the start must give up at the timeout, not before or never: {waited:?}"
    );

    // Not remembered: the same program, now answering, starts an agent.
    std::fs::remove_file(&marker).unwrap();
    let ag = start_agent(&mut c, &ws.id).await;
    let events = next_agent_events(&mut c, &ag, 4, Duration::from_secs(20)).await;
    assert_eq!(
        states(&events).last(),
        Some(&AgentState::Idle),
        "{events:?}"
    );

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag }))
        .await
        .unwrap();
    cancel.cancel();
}
