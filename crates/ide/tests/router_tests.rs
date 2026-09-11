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

// ---------------------------------------------------------------------------
// Review fixes 2026-09-11, task 7: unsubscribing by token (IM6) and the
// per-workspace `fs.changed` stream (CI2).
// ---------------------------------------------------------------------------

mod review_fixes_task_7 {
    use super::out;
    use bondsymphonic_ide::client::router::{EventRouter, StreamKey};
    use bondsymphonic_proto::*;

    fn fs_changed(path: &str) -> Event {
        Event::FsChanged {
            paths: vec![path.into()],
        }
    }

    /// Consumer A is replaced by consumer B on the same id (a re-attached
    /// transcript, a re-opened terminal). A's receiver closes, A's task ends
    /// and unsubscribes on its way out. By key, that would take B's live
    /// subscription with it; by token, it takes nothing.
    #[tokio::test]
    async fn a_stale_tail_unsubscribe_leaves_the_live_subscriber_receiving() {
        let r = EventRouter::new();
        let key = StreamKey::Pty("pty_1".into());
        let (mut a, token_a) = r.subscribe_stream(key.clone());
        let (mut b, token_b) = r.subscribe_stream(key.clone());
        assert!(a.recv().await.is_none(), "A was replaced by B");
        assert_ne!(token_a, token_b);

        r.unsubscribe_stream(&key, token_a);
        r.dispatch(None, out("pty_1", "live"));
        assert!(
            matches!(b.try_recv(), Ok((_, Event::PtyOutput { data_b64, .. })) if data_b64 == "live"),
            "B must still receive after A's stale unsubscribe"
        );

        // B's own token does end B, and clears what was parked for the key.
        r.unsubscribe_stream(&key, token_b);
        assert!(b.recv().await.is_none());
        r.dispatch(None, out("pty_1", "parked"));
        let (mut c, _) = r.subscribe_stream(key);
        assert!(
            matches!(c.try_recv(), Ok((_, Event::PtyOutput { data_b64, .. })) if data_b64 == "parked")
        );
    }

    /// The by-key form still exists for callers that hold no token; it keeps
    /// its old meaning of "whoever holds the key".
    #[tokio::test]
    async fn the_by_key_unsubscribe_still_ends_the_current_subscriber() {
        let r = EventRouter::new();
        let mut a = r.subscribe_pty(&"pty_2".into());
        r.unsubscribe_pty(&"pty_2".into());
        assert!(a.recv().await.is_none());
    }

    /// `fs.changed` is routed by workspace: an editor on workspace A never
    /// hears about B's worktree, two editors on A both hear about A's, and an
    /// event with no workspace reaches no fs subscriber at all.
    #[tokio::test]
    async fn fs_changes_reach_only_the_subscribers_of_their_workspace() {
        let r = EventRouter::new();
        let mut a1 = r.subscribe_fs(&"ws_a".into());
        let mut a2 = r.subscribe_fs(&"ws_a".into());
        let mut b = r.subscribe_fs(&"ws_b".into());
        let mut all = r.subscribe_all();

        r.dispatch(Some("ws_a".into()), fs_changed("src/main.rs"));
        r.dispatch(None, fs_changed("nowhere"));
        r.dispatch(Some("ws_a".into()), out("pty_x", "not a change"));

        for (name, rx) in [("a1", &mut a1), ("a2", &mut a2)] {
            assert!(
                matches!(rx.try_recv(), Ok((Some(ws), Event::FsChanged { paths })) if ws.0 == "ws_a" && paths == ["src/main.rs"]),
                "{name} must receive ws_a's change"
            );
            assert!(
                rx.try_recv().is_err(),
                "{name} got more than the one change"
            );
        }
        assert!(b.try_recv().is_err(), "ws_b must not hear about ws_a");
        // `subscribe_all` still sees everything.
        let mut n = 0;
        while all.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, 3);

        // Dropping one editor's receiver does not end the other's.
        drop(a1);
        r.dispatch(Some("ws_a".into()), fs_changed("src/lib.rs"));
        assert!(matches!(a2.try_recv(), Ok((_, Event::FsChanged { .. }))));
        assert!(b.try_recv().is_err());
    }
}
