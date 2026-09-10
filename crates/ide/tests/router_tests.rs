use bondsymphonic_ide::client::router::EventRouter;
use bondsymphonic_proto::*;

fn out(pty: &str, s: &str) -> Event {
    Event::PtyOutput {
        pty_id: pty.into(),
        data_b64: s.into(),
    }
}

#[tokio::test]
async fn routes_by_pty_and_replays_early_output() {
    let r = EventRouter::new();
    let mut all = r.subscribe_all();
    r.dispatch(Some("ws_1".into()), out("pty_a", "early1"));
    r.dispatch(Some("ws_1".into()), out("pty_b", "other"));
    let mut a = r.subscribe_pty(&"pty_a".into());
    r.dispatch(Some("ws_1".into()), out("pty_a", "live2"));
    r.dispatch(
        Some("ws_1".into()),
        Event::PtyExit {
            pty_id: "pty_a".into(),
            code: 0,
        },
    );
    let got: Vec<String> = [a.recv().await, a.recv().await, a.recv().await]
        .into_iter()
        .flatten()
        .map(|(_, e)| match e {
            Event::PtyOutput { data_b64, .. } => data_b64,
            Event::PtyExit { code, .. } => format!("exit{code}"),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(got, vec!["early1", "live2", "exit0"]);
    assert!(a.try_recv().is_err(), "pty_b output must not reach pty_a");
    // subscribe_all saw everything in order
    let mut n = 0;
    while all.try_recv().is_ok() {
        n += 1;
    }
    assert_eq!(n, 4);
}

#[tokio::test]
async fn early_output_buffer_expires() {
    let r = EventRouter::new();
    r.dispatch(None, out("pty_x", "stale"));
    // Test hook: with a zero max age everything already in the buffer is "old".
    r.expire_early_buffers_older_than(std::time::Duration::from_secs(0));
    let mut x = r.subscribe_pty(&"pty_x".into());
    assert!(x.try_recv().is_err());
    r.unsubscribe_pty(&"pty_x".into());
    r.dispatch(None, out("pty_x", "after"));
    assert!(x.recv().await.is_none(), "channel closed after unsubscribe");
}

fn agent_msg(agent: &str, seq: u64, text: &str) -> Event {
    Event::AgentMessage {
        agent_id: agent.into(),
        message: AgentMessage {
            seq,
            ts: "2026-09-09T10:00:00Z".into(),
            body: AgentMessageBody::AssistantText { text: text.into() },
        },
    }
}

/// The transcript view subscribes only once `agent.start` has answered, by
/// which time the agent has usually already said something. Those events are
/// parked exactly as a PTY's are, and replayed on subscription.
#[tokio::test]
async fn routes_by_agent_and_replays_early_messages() {
    let r = EventRouter::new();
    r.dispatch(Some("ws_1".into()), agent_msg("ag_a", 1, "early"));
    r.dispatch(Some("ws_1".into()), agent_msg("ag_b", 1, "other agent"));
    r.dispatch(None, out("pty_a", "not an agent"));

    let mut a = r.subscribe_agent(&"ag_a".into());
    r.dispatch(
        Some("ws_1".into()),
        Event::AgentStateChanged {
            agent_id: "ag_a".into(),
            state: AgentState::Working,
            detail: None,
        },
    );
    r.dispatch(Some("ws_1".into()), agent_msg("ag_a", 2, "live"));

    let got: Vec<String> = [a.recv().await, a.recv().await, a.recv().await]
        .into_iter()
        .flatten()
        .map(|(_, e)| match e {
            Event::AgentMessage { message, .. } => format!("{:?}", message.body),
            Event::AgentStateChanged { state, .. } => format!("{state:?}"),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(got.len(), 3);
    assert!(got[0].contains("early"), "{got:?}");
    assert_eq!(got[1], "Working");
    assert!(got[2].contains("live"), "{got:?}");
    assert!(
        a.try_recv().is_err(),
        "another agent's and a PTY's events must not reach ag_a"
    );
}

#[tokio::test]
async fn unsubscribing_an_agent_closes_the_receiver_and_drops_its_buffer() {
    let r = EventRouter::new();
    r.dispatch(None, agent_msg("ag_x", 1, "stale"));
    r.unsubscribe_agent(&"ag_x".into());
    let mut x = r.subscribe_agent(&"ag_x".into());
    assert!(
        x.try_recv().is_err(),
        "unsubscribe discards the early buffer"
    );
    r.unsubscribe_agent(&"ag_x".into());
    r.dispatch(None, agent_msg("ag_x", 2, "after"));
    assert!(x.recv().await.is_none(), "channel closed after unsubscribe");
}

/// A PTY and an agent that happen to share an id string are different streams.
#[tokio::test]
async fn pty_and_agent_keys_do_not_collide() {
    let r = EventRouter::new();
    let mut pty = r.subscribe_pty(&"x".into());
    let mut agent = r.subscribe_agent(&"x".into());
    r.dispatch(None, out("x", "pty bytes"));
    r.dispatch(None, agent_msg("x", 1, "agent words"));
    assert!(matches!(pty.try_recv(), Ok((_, Event::PtyOutput { .. }))));
    assert!(pty.try_recv().is_err());
    assert!(matches!(
        agent.try_recv(),
        Ok((_, Event::AgentMessage { .. }))
    ));
    assert!(agent.try_recv().is_err());
}

fn run_out(run: &str, line: &str) -> Event {
    Event::RunOutput {
        run_id: run.into(),
        line: line.into(),
    }
}

/// A dev server prints its banner between `run.start` returning and the panel
/// subscribing with the id that reply carried, so a run's first output is
/// parked and replayed exactly as a PTY's is.
#[tokio::test]
async fn routes_by_run_and_replays_early_output() {
    let r = EventRouter::new();
    r.dispatch(Some("ws_1".into()), run_out("run_a", "VITE ready"));
    r.dispatch(Some("ws_1".into()), run_out("run_b", "other run"));
    r.dispatch(None, out("pty_a", "not a run"));

    let mut a = r.subscribe_run(&"run_a".into());
    r.dispatch(
        Some("ws_1".into()),
        Event::RunStateChanged {
            run_id: "run_a".into(),
            state: RunState::Ready,
            url: Some("http://localhost:41873".into()),
            detail: None,
        },
    );
    r.dispatch(Some("ws_1".into()), run_out("run_a", "GET /"));

    let got: Vec<String> = [a.recv().await, a.recv().await, a.recv().await]
        .into_iter()
        .flatten()
        .map(|(_, e)| match e {
            Event::RunOutput { line, .. } => line,
            Event::RunStateChanged { state, .. } => format!("{state:?}"),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(got, vec!["VITE ready", "Ready", "GET /"]);
    assert!(
        a.try_recv().is_err(),
        "another run's output and a PTY's bytes must not reach run_a"
    );
}

#[tokio::test]
async fn unsubscribing_a_run_closes_the_receiver_and_drops_its_buffer() {
    let r = EventRouter::new();
    r.dispatch(None, run_out("run_x", "stale"));
    r.unsubscribe_run(&"run_x".into());
    let mut x = r.subscribe_run(&"run_x".into());
    assert!(
        x.try_recv().is_err(),
        "unsubscribe discards the early buffer"
    );
    r.unsubscribe_run(&"run_x".into());
    r.dispatch(None, run_out("run_x", "after"));
    assert!(x.recv().await.is_none(), "channel closed after unsubscribe");
}

/// Three id spaces, one table. A run, a PTY and an agent that happen to share
/// an id string are different streams.
#[tokio::test]
async fn run_pty_and_agent_keys_do_not_collide() {
    let r = EventRouter::new();
    let mut pty = r.subscribe_pty(&"x".into());
    let mut agent = r.subscribe_agent(&"x".into());
    let mut run = r.subscribe_run(&"x".into());
    r.dispatch(None, out("x", "pty bytes"));
    r.dispatch(None, agent_msg("x", 1, "agent words"));
    r.dispatch(None, run_out("x", "run line"));
    assert!(matches!(pty.try_recv(), Ok((_, Event::PtyOutput { .. }))));
    assert!(pty.try_recv().is_err());
    assert!(matches!(
        agent.try_recv(),
        Ok((_, Event::AgentMessage { .. }))
    ));
    assert!(agent.try_recv().is_err());
    assert!(matches!(run.try_recv(), Ok((_, Event::RunOutput { .. }))));
    assert!(run.try_recv().is_err());
}
