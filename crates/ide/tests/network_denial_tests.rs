//! A host the proxy is holding (or refused), shown on the network bar of the
//! agent that tried to reach it: which transcript takes it, how repeats fold
//! into one row per host, what Allow and Always block do to the row, and when
//! the row goes. Pure Rust, no Qt.

use bondsymphonic_ide::model::transcript::{
    DenialStatus, NetworkNotice, NetworkRequest, Transcript, NETWORK_SETTLED_MS,
};
use bondsymphonic_ide::qobjects::app_controller::network_notice;
use bondsymphonic_proto::{
    AgentAdapterKind, AgentMessage, AgentMessageBody, AgentState, Event, WorkspaceId,
    NETWORK_APPROVAL_WAIT_SECS,
};

const NOW: u64 = 1_800_000_000_000;
const WAIT_MS: u64 = NETWORK_APPROVAL_WAIT_SECS * 1000;

fn denied(host: &str) -> NetworkNotice {
    NetworkNotice::denied(host, Some(AgentAdapterKind::Codex))
}

fn row<'a>(t: &'a Transcript, host: &str) -> &'a NetworkRequest {
    t.network
        .iter()
        .find(|r| r.host == host)
        .unwrap_or_else(|| panic!("no row for {host}: {:?}", t.network))
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

/// One row per host: repeats count up on it and push its deadline out to the
/// proxy's wait from the latest notice, a different host gets its own row,
/// and none of it is a transcript item.
#[test]
fn repeats_of_a_host_fold_into_one_row() {
    let mut t = Transcript::default();
    t.apply(&AgentMessage {
        seq: 1,
        ts: "2026-10-21T10:00:00Z".to_owned(),
        body: AgentMessageBody::UserText {
            text: "install the deps".to_owned(),
        },
    });
    assert!(t.note_network_denied(&denied("blocked.example"), NOW));
    assert!(t.note_network_denied(&denied("other.example"), NOW));
    assert!(t.note_network_denied(&denied("blocked.example"), NOW + 5_000));
    assert_eq!(t.network.len(), 2);
    assert_eq!(t.items.len(), 1, "a denial is not a transcript item");
    let blocked = row(&t, "blocked.example");
    assert_eq!(blocked.count, 2);
    assert_eq!(blocked.until_ms, NOW + 5_000 + WAIT_MS);
    assert_eq!(row(&t, "other.example").count, 1);

    // What the bar reads.
    let json: serde_json::Value = serde_json::from_str(&t.network_json()).unwrap();
    assert_eq!(json[0]["host"], "blocked.example");
    assert_eq!(json[0]["count"], 2);
    assert_eq!(json[0]["allowable"], true);
    assert_eq!(json[0]["status"], "blocked");
    assert_eq!(json[0]["until_ms"], NOW + 5_000 + WAIT_MS);
}

/// "Allow" moves the row through allowing to allowed, where it stays a few
/// seconds to be read; a failure says why and offers the button again. An
/// allowed host blocked again is a new row, not a count on one that says it
/// is allowed.
#[test]
fn allowing_a_host_settles_its_row() {
    let mut t = Transcript::default();
    t.note_network_denied(&denied("blocked.example"), NOW);
    t.note_network_denied(&denied("other.example"), NOW);

    assert!(t.begin_allow("blocked.example"));
    assert_eq!(row(&t, "blocked.example").status, DenialStatus::Allowing);
    // A second click while the first is on its way does nothing, and nor
    // does a host with no row.
    assert!(!t.begin_allow("blocked.example"));
    assert!(!t.begin_allow("nowhere.example"));

    assert!(t.finish_allow("blocked.example", Err("daemon said no".into()), NOW));
    let failed = row(&t, "blocked.example");
    assert_eq!(failed.status, DenialStatus::Failed);
    assert_eq!(failed.error, "daemon said no");

    assert!(t.begin_allow("blocked.example"));
    assert!(t.finish_allow("blocked.example", Ok(()), NOW + 1_000));
    let allowed = row(&t, "blocked.example");
    assert_eq!(allowed.status, DenialStatus::Allowed);
    assert_eq!(allowed.error, "");
    assert_eq!(allowed.until_ms, NOW + 1_000 + NETWORK_SETTLED_MS);
    assert_eq!(row(&t, "other.example").status, DenialStatus::Blocked);

    // Blocked again after the allow: news, so a fresh row.
    t.note_network_denied(&denied("blocked.example"), NOW + 2_000);
    let again = row(&t, "blocked.example");
    assert_eq!(again.status, DenialStatus::Blocked);
    assert_eq!(again.count, 1);
}

/// Rows go when their time is up: a waiting one when the proxy stops holding
/// the request, an answered one once it has been on show, and never one whose
/// answer is still being saved.
#[test]
fn rows_expire_with_the_proxys_wait() {
    let mut t = Transcript::default();
    t.note_network_denied(&denied("waiting.example"), NOW);
    t.note_network_denied(&denied("saving.example"), NOW);
    t.note_network_denied(&denied("answered.example"), NOW);
    assert!(t.begin_allow("saving.example"));
    assert!(t.begin_allow("answered.example"));
    t.finish_allow("answered.example", Ok(()), NOW);

    assert!(!t.expire_network(NOW + 1_000));
    assert!(t.expire_network(NOW + NETWORK_SETTLED_MS + 1));
    assert!(t.network.iter().all(|r| r.host != "answered.example"));
    assert!(t.expire_network(NOW + WAIT_MS + 1));
    let left: Vec<&str> = t.network.iter().map(|r| r.host.as_str()).collect();
    assert_eq!(left, ["saving.example"]);
}

/// Dismissing stops asking; it never takes away a row whose answer is still
/// being saved, which would hide how that save ended.
#[test]
fn a_row_can_be_dismissed_unless_its_answer_is_being_saved() {
    let mut t = Transcript::default();
    t.note_network_denied(&denied("a.example"), NOW);
    t.note_network_denied(&denied("b.example"), NOW);
    assert!(t.begin_allow("b.example"));
    assert!(t.dismiss_network("a.example"));
    assert!(!t.dismiss_network("b.example"));
    assert_eq!(t.network.len(), 1);
}

/// A refused model-server tunnel is shown the same way but can never be
/// allowed from here: the workspace allowlist is not what refused it.
#[test]
fn a_tunnel_refusal_is_shown_without_an_allow() {
    let mut t = Transcript::default();
    let tunnel = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let (_, notice) = network_notice(&Some(WorkspaceId("ws_1".into())), &tunnel).unwrap();
    t.note_network_denied(&notice, NOW);
    t.note_network_denied(&notice, NOW);
    assert!(!t.begin_allow("ollama.lan"));
    let refused = row(&t, "ollama.lan");
    assert!(!refused.allowable);
    assert_eq!(refused.count, 2);
    assert!(refused
        .text
        .starts_with("The agent's connection to ollama.lan was refused"));
}

#[test]
fn always_block_settles_the_row_and_a_failure_can_be_retried() {
    let mut t = Transcript::default();
    t.note_network_denied(&denied("blocked.example"), NOW);
    assert!(t.begin_block("blocked.example"));
    assert!(!t.begin_block("blocked.example"));
    assert!(t.finish_block("blocked.example", Err("disk full".into()), NOW));
    let failed = row(&t, "blocked.example");
    assert_eq!(failed.status, DenialStatus::Failed);
    assert_eq!(failed.error, "disk full");
    assert!(t.begin_block("blocked.example"));
    assert!(t.finish_block("blocked.example", Ok(()), NOW));
    assert_eq!(
        row(&t, "blocked.example").status,
        DenialStatus::AlwaysBlocked
    );
    assert!(!t.begin_allow("blocked.example"));
    assert!(!t.begin_block("blocked.example"));
}

#[test]
fn a_refused_model_tunnel_can_also_be_always_blocked() {
    let mut t = Transcript::default();
    let event = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let (_, notice) = network_notice(&Some(WorkspaceId("ws_1".into())), &event).unwrap();
    t.note_network_denied(&notice, NOW);
    assert!(!t.begin_allow("ollama.lan"));
    assert!(t.begin_block("ollama.lan"));
    t.finish_block("ollama.lan", Err("disconnected".into()), NOW);
    assert_eq!(row(&t, "ollama.lan").status, DenialStatus::Failed);
    assert!(t.begin_block("ollama.lan"));
    t.finish_block("ollama.lan", Ok(()), NOW);
    assert_eq!(row(&t, "ollama.lan").status, DenialStatus::AlwaysBlocked);
}
