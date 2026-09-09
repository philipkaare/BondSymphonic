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
