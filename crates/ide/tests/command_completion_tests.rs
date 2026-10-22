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
    assert_eq!(
        claude.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["compact", "review"]
    );
    assert!(from_system("other", &json!({"commands":[]})).is_none());
}

#[test]
fn command_updates_replay_without_chat_rows_and_replace_previous_catalog() {
    let mut transcript = Transcript::default();
    let message = |seq, commands| AgentMessage {
        seq,
        ts: "now".into(),
        body: AgentMessageBody::System {
            subtype: "agent_commands".into(),
            data: json!({"commands":commands}),
        },
    };
    assert_eq!(
        transcript.apply(&message(1, json!([{"name":"status"}]))),
        Applied::Nothing
    );
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
        id: AgentId(id.into()),
        adapter,
        state,
        session_id: session.map(str::to_owned),
        command: None,
        model: None,
        permission_mode: None,
    };
    let mut records = vec![
        record("old", Kind::Codex, AgentState::Exited, Some("saved")),
        record(
            "other",
            Kind::OpenCode,
            AgentState::Idle,
            Some("other-session"),
        ),
        record("empty", Kind::Codex, AgentState::Error, None),
    ];
    assert_eq!(
        resume_target(&records, Kind::Codex, "").unwrap(),
        ResumeTarget::Session("saved".into())
    );
    records.push(record(
        "live",
        Kind::Codex,
        AgentState::Working,
        Some("current"),
    ));
    assert_eq!(
        resume_target(&records, Kind::Codex, "").unwrap(),
        ResumeTarget::Attach(AgentId("live".into()))
    );
    assert_eq!(
        resume_target(&records, Kind::Codex, "selected").unwrap(),
        ResumeTarget::Session("selected".into())
    );
    assert!(resume_target(&[], Kind::Codex, "").is_err());
}

#[test]
fn switching_harness_stops_other_live_agents_and_resumes_the_targets_own_session() {
    use bondsymphonic_ide::model::commands::plan_harness_switch;
    use bondsymphonic_proto::{AgentAdapterKind as Kind, AgentId, AgentState, AgentSummary};
    let record =
        |id: &str, adapter, state, session: Option<&str>, model: Option<&str>| AgentSummary {
            id: AgentId(id.into()),
            adapter,
            state,
            session_id: session.map(str::to_owned),
            command: None,
            model: model.map(str::to_owned),
            permission_mode: None,
        };
    let records = vec![
        record(
            "c1",
            Kind::Codex,
            AgentState::Exited,
            Some("codex-saved"),
            Some("gpt-x"),
        ),
        record("o1", Kind::OpenCode, AgentState::Idle, Some("oc"), None),
        record(
            "k1",
            Kind::Claude,
            AgentState::Working,
            Some("claude-now"),
            None,
        ),
    ];

    // Back to Codex: every live agent of another harness stops, and Codex
    // resumes its own conversation with the model it ran.
    let plan = plan_harness_switch(&records, Kind::Codex);
    assert_eq!(plan.stop, vec![AgentId("o1".into()), AgentId("k1".into())]);
    assert_eq!(plan.attach, None);
    let options: serde_json::Value = serde_json::from_str(&plan.options_json).unwrap();
    assert_eq!(
        options,
        json!({"adapter":"codex","model":"gpt-x","resume_session":"codex-saved"})
    );

    // A live agent of the target is attached to, not restarted.
    let plan = plan_harness_switch(&records, Kind::OpenCode);
    assert_eq!(plan.attach, Some(AgentId("o1".into())));
    assert_eq!(plan.stop, vec![AgentId("k1".into())]);

    // Never used here: a fresh start that names only the harness. Another
    // harness's session never carries over.
    let plan = plan_harness_switch(&records[..1], Kind::Claude);
    assert_eq!(plan.stop, Vec::<AgentId>::new());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plan.options_json).unwrap(),
        json!({"adapter":"claude"})
    );
}
