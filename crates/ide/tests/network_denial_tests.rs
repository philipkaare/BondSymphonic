//! A host the proxy blocked, shown in the transcript of the agent that tried
//! to reach it: which transcript takes it, how repeats coalesce, and what the
//! inline "Allow" does to the item. Pure Rust, no Qt.

use bondsymphonic_ide::model::transcript::{
    Applied, DenialStatus, NetworkNotice, Transcript, TranscriptItem,
};
use bondsymphonic_ide::qobjects::app_controller::network_notice;
use bondsymphonic_proto::{
    AgentAdapterKind, AgentMessage, AgentMessageBody, AgentState, Event, WorkspaceId,
};

fn msg(seq: u64, body: AgentMessageBody) -> AgentMessage {
    AgentMessage {
        seq,
        ts: "2026-10-21T10:00:00Z".to_owned(),
        body,
    }
}

fn user(seq: u64, text: &str) -> AgentMessage {
    msg(
        seq,
        AgentMessageBody::UserText {
            text: text.to_owned(),
        },
    )
}

fn denied(host: &str) -> NetworkNotice {
    NetworkNotice::denied(host, Some(AgentAdapterKind::Codex))
}

fn denial_at(t: &Transcript, i: usize) -> (String, u32, DenialStatus, String) {
    match &t.items[i] {
        TranscriptItem::NetworkDenied {
            host,
            count,
            status,
            error,
            ..
        } => (host.clone(), *count, *status, error.clone()),
        other => panic!("item {i} is not a denial: {other:?}"),
    }
}

/// The event stream's two network notices become the same inline item, one
/// with an "Allow" and one without, both naming the backend that asked.
#[test]
fn the_daemon_notices_map_onto_inline_notices() {
    let ws = Some(WorkspaceId("ws_1".into()));
    let (workspace, notice) = network_notice(
        &ws,
        &Event::network_denied_by("blocked.example", AgentAdapterKind::Codex),
    )
    .unwrap();
    assert_eq!(workspace, "ws_1");
    assert_eq!(notice.host, "blocked.example");
    assert_eq!(notice.backend, Some(AgentAdapterKind::Codex));
    assert!(notice.allowable);
    assert!(notice.text.contains("blocked.example"), "{}", notice.text);

    // The tunnel refusal keeps the daemon's own wording and offers no Allow.
    let tunnel = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let (_, notice) = network_notice(&ws, &tunnel).unwrap();
    assert!(!notice.allowable);
    assert_eq!(notice.backend, Some(AgentAdapterKind::OpenCode));
    match &tunnel {
        Event::DaemonLog { message, .. } => assert_eq!(&notice.text, message),
        _ => unreachable!(),
    }

    // An older daemon's denial is still a notice, attributed to nobody, and
    // nothing without a workspace or that is not a denial is one at all.
    let (_, old) = network_notice(&ws, &Event::network_denied("blocked.example")).unwrap();
    assert_eq!(old.backend, None);
    assert!(network_notice(&None, &Event::network_denied("blocked.example")).is_none());
    assert!(network_notice(&ws, &Event::events_dropped(2)).is_none());
}

/// A transcript takes a denial only when it is attributed to its own backend
/// and its agent is still there to have made the request. An unattributed one
/// (an older daemon) goes to the toast instead, as it always did.
#[test]
fn a_transcript_takes_only_its_own_backends_denials() {
    let mut t = Transcript::default();
    let codex = Some(AgentAdapterKind::Codex);
    assert!(t.takes_denial(AgentAdapterKind::Codex, true, codex));
    assert!(!t.takes_denial(AgentAdapterKind::Claude, true, codex));
    assert!(!t.takes_denial(AgentAdapterKind::Codex, true, None));
    assert!(!t.takes_denial(AgentAdapterKind::Codex, false, codex));
    t.set_state(AgentState::Working, None);
    assert!(t.takes_denial(AgentAdapterKind::Codex, true, codex));
    // An agent that has gone made nothing; whatever the sandbox is still
    // doing is a run's or a terminal's, and the toast is where that goes.
    t.set_state(AgentState::Exited, None);
    assert!(!t.takes_denial(AgentAdapterKind::Codex, true, codex));
}

/// One item per host per turn: repeats count up on it, a different host gets
/// its own, and a new turn starts afresh.
#[test]
fn repeats_of_a_host_coalesce_within_a_turn() {
    let mut t = Transcript::default();
    t.apply(&user(1, "install the deps"));
    assert_eq!(
        t.note_network_denied(&denied("blocked.example")),
        Applied::Appended(1)
    );
    assert_eq!(
        t.note_network_denied(&denied("other.example")),
        Applied::Appended(2)
    );
    assert_eq!(
        t.note_network_denied(&denied("blocked.example")),
        Applied::Changed(1)
    );
    assert_eq!(t.items.len(), 3);
    assert_eq!(denial_at(&t, 1).1, 2);
    assert_eq!(denial_at(&t, 2).1, 1);

    // The next turn's denial of the same host is news again.
    t.apply(&user(2, "try again"));
    assert_eq!(
        t.note_network_denied(&denied("blocked.example")),
        Applied::Appended(4)
    );
    assert_eq!(denial_at(&t, 4).1, 1);
    assert_eq!(denial_at(&t, 1).1, 2);

    // The item says what it is in the JSON the view reads.
    let json: serde_json::Value = serde_json::from_str(&t.item_json(4)).unwrap();
    assert_eq!(json["kind"], "network_denied");
    assert_eq!(json["host"], "blocked.example");
    assert_eq!(json["count"], 1);
    assert_eq!(json["allowable"], true);
    assert_eq!(json["status"], "blocked");
}

/// "Allow" moves the item through allowing to allowed, and every blocked item
/// for that host in the transcript says so. A failure says why and offers the
/// button again. An allowed host blocked again is a new item, not a count on
/// one that says it is allowed.
#[test]
fn allowing_a_host_settles_every_item_for_it() {
    let mut t = Transcript::default();
    t.apply(&user(1, "go"));
    t.note_network_denied(&denied("blocked.example"));
    t.apply(&user(2, "again"));
    t.note_network_denied(&denied("blocked.example"));
    t.note_network_denied(&denied("other.example"));

    assert_eq!(t.begin_allow(3), Some("blocked.example".to_owned()));
    assert_eq!(denial_at(&t, 3).2, DenialStatus::Allowing);
    // A second click while the first is on its way does nothing.
    assert_eq!(t.begin_allow(3), None);
    // Not a denial, or out of range: nothing to allow.
    assert_eq!(t.begin_allow(0), None);
    assert_eq!(t.begin_allow(99), None);

    let failed = t.finish_allow("blocked.example", Err("daemon said no".into()));
    assert_eq!(failed, vec![3]);
    let (_, _, status, error) = denial_at(&t, 3);
    assert_eq!(status, DenialStatus::Failed);
    assert_eq!(error, "daemon said no");

    assert_eq!(t.begin_allow(3), Some("blocked.example".to_owned()));
    let changed = t.finish_allow("blocked.example", Ok(()));
    assert_eq!(changed, vec![1, 3]);
    assert_eq!(denial_at(&t, 1).2, DenialStatus::Allowed);
    assert_eq!(denial_at(&t, 3).2, DenialStatus::Allowed);
    assert_eq!(denial_at(&t, 3).3, "");
    assert_eq!(denial_at(&t, 4).2, DenialStatus::Blocked);

    assert_eq!(
        t.note_network_denied(&denied("blocked.example")),
        Applied::Appended(5)
    );
}

/// A refused model-server tunnel is shown the same way but can never be
/// allowed from here: the workspace allowlist is not what refused it.
#[test]
fn a_tunnel_refusal_is_shown_without_an_allow() {
    let mut t = Transcript::default();
    let tunnel = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let (_, notice) = network_notice(&Some(WorkspaceId("ws_1".into())), &tunnel).unwrap();
    assert_eq!(t.note_network_denied(&notice), Applied::Appended(0));
    assert_eq!(t.note_network_denied(&notice), Applied::Changed(0));
    assert_eq!(t.begin_allow(0), None);
    let json: serde_json::Value = serde_json::from_str(&t.item_json(0)).unwrap();
    assert_eq!(json["allowable"], false);
    assert_eq!(json["count"], 2);
    assert!(json["text"]
        .as_str()
        .unwrap()
        .starts_with("The agent's connection to ollama.lan was refused"));
}

#[test]
fn always_block_settles_denials_and_a_failure_can_be_retried() {
    let mut t = Transcript::default();
    t.note_network_denied(&denied("blocked.example"));
    assert_eq!(t.begin_block(0), Some("blocked.example".into()));
    assert_eq!(t.begin_block(0), None);
    assert_eq!(
        t.finish_block("blocked.example", Err("disk full".into())),
        vec![0]
    );
    assert_eq!(denial_at(&t, 0).2, DenialStatus::Failed);
    assert_eq!(denial_at(&t, 0).3, "disk full");
    assert_eq!(t.begin_block(0), Some("blocked.example".into()));
    assert_eq!(t.finish_block("blocked.example", Ok(())), vec![0]);
    assert_eq!(denial_at(&t, 0).2, DenialStatus::AlwaysBlocked);
    assert_eq!(t.begin_allow(0), None);
    assert_eq!(t.begin_block(0), None);
}

#[test]
fn a_refused_model_tunnel_can_also_be_always_blocked() {
    let mut t = Transcript::default();
    let event = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let (_, notice) = network_notice(&Some(WorkspaceId("ws_1".into())), &event).unwrap();
    t.note_network_denied(&notice);
    assert_eq!(t.begin_allow(0), None);
    assert_eq!(t.begin_block(0), Some("ollama.lan".into()));
    t.finish_block("ollama.lan", Err("disconnected".into()));
    assert_eq!(denial_at(&t, 0).2, DenialStatus::Failed);
    assert_eq!(t.begin_block(0), Some("ollama.lan".into()));
    t.finish_block("ollama.lan", Ok(()));
    assert_eq!(denial_at(&t, 0).2, DenialStatus::AlwaysBlocked);
}
