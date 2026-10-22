use bondsymphonic_ide::model::commands::from_system;
use bondsymphonic_ide::model::transcript::{Applied, Transcript};
use bondsymphonic_proto::{AgentMessage, AgentMessageBody};
use serde_json::json;

#[test]
fn harness_catalog_keeps_descriptions_and_argument_hints_without_duplicate_names() {
    let commands = from_system("agent_commands", &json!({"commands":[
        {"name":"/compact","description":"Compact this conversation","argument_hint":"instructions"},
        {"name":"compact","description":"duplicate"},
        {"name":"invalid command"}, {"name":""}
    ]})).unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "compact");
    assert_eq!(commands[0].description, "Compact this conversation");
    assert_eq!(commands[0].argument_hint.as_deref(), Some("instructions"));
    let claude = from_system("init", &json!({"slash_commands":["compact","/review"]})).unwrap();
    assert_eq!(claude.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["compact", "review"]);
    assert!(from_system("other", &json!({"commands":[]})).is_none());
}

#[test]
fn command_updates_replay_without_chat_rows_and_replace_previous_catalog() {
    let mut transcript = Transcript::default();
    let message = |seq, commands| AgentMessage {
        seq, ts: "now".into(), body: AgentMessageBody::System {
            subtype: "agent_commands".into(), data: json!({"commands":commands}),
        },
    };
    assert_eq!(transcript.apply(&message(1, json!([{"name":"status"}]))), Applied::Nothing);
    assert_eq!(transcript.commands[0].name, "status");
    transcript.apply(&message(2, json!([])));
    assert!(transcript.commands.is_empty());
    assert!(transcript.items.is_empty());
}

#[test]
fn resume_uses_the_live_session_or_latest_saved_conversation_for_this_harness() {
    use bondsymphonic_ide::model::commands::{resume_target, ResumeTarget};
    use bondsymphonic_proto::{AgentAdapterKind as Kind, AgentId, AgentState, AgentSummary};
    let record = |id: &str, adapter, state, session: Option<&str>| AgentSummary {
        id: AgentId(id.into()), adapter, state, session_id: session.map(str::to_owned),
        command: None, model: None, permission_mode: None,
    };
    let mut records = vec![
        record("old", Kind::Codex, AgentState::Exited, Some("saved")),
        record("other", Kind::OpenCode, AgentState::Idle, Some("other-session")),
        record("empty", Kind::Codex, AgentState::Error, None),
    ];
    assert_eq!(resume_target(&records, Kind::Codex, "").unwrap(), ResumeTarget::Session("saved".into()));
    records.push(record("live", Kind::Codex, AgentState::Working, Some("current")));
    assert_eq!(resume_target(&records, Kind::Codex, "").unwrap(), ResumeTarget::Attach(AgentId("live".into())));
    assert_eq!(resume_target(&records, Kind::Codex, "selected").unwrap(), ResumeTarget::Session("selected".into()));
    assert!(resume_target(&[], Kind::Codex, "").is_err());
}
