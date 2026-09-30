use bondsymphonic_ide::{
    model::backends::{
        credential_name, legacy_descriptors, select_default_backend, start_options_with_defaults,
        BackendSettings, ReplyGenerations,
    },
    qobjects::settings::Settings,
};
use bondsymphonic_proto::AgentAdapterKind;

#[test]
fn legacy_modes_migrate_without_escalating_and_explicit_backend_settings_win() {
    let legacy = Settings::from_json(r#"{"default_permission_mode":"default"}"#).unwrap();
    assert_eq!(legacy.backends["claude"].default_permission_mode, "manual");
    let migrated=Settings::from_json(r#"{"default_permission_mode":"plan","backends":{"claude":{"enabled":true,"default_model":"chosen","default_permission_mode":"acceptEdits"},"future":{"enabled":true,"default_model":"future-model","default_permission_mode":"safe"}}}"#).unwrap();
    assert_eq!(
        migrated.backends["claude"].default_permission_mode,
        "acceptEdits"
    );
    assert_eq!(migrated.backends["future"].default_model, "future-model");
    assert!(!serde_json::to_value(&migrated)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("default_permission_mode"));
}

#[test]
fn disabled_default_falls_back_and_all_disabled_has_no_selection() {
    let mut s = Settings::default();
    assert!(s.backends["claude"].enabled);
    assert!(!s.backends["codex"].enabled);
    let mut descriptors = legacy_descriptors();
    let mut codex = descriptors[0].clone();
    codex.id = AgentAdapterKind::Codex;
    descriptors.push(codex);
    s.default_backend = "codex".into();
    assert_eq!(
        select_default_backend(&s, &descriptors),
        Some(AgentAdapterKind::Claude)
    );
    s.backends.get_mut("claude").unwrap().enabled = false;
    assert_eq!(select_default_backend(&s, &descriptors), None);
    s.backends.get_mut("codex").unwrap().enabled = true;
    assert_eq!(
        select_default_backend(&s, &descriptors),
        Some(AgentAdapterKind::Codex)
    );
    assert_eq!(select_default_backend(&s, &legacy_descriptors()), None);
}

#[test]
fn defaults_preserve_explicit_choices_and_credentials_stay_backend_specific() {
    let defaults = BackendSettings {
        enabled: true,
        default_model: "default-model".into(),
        default_permission_mode: "never".into(),
    };
    let explicit = start_options_with_defaults(
        r#"{"model":"chosen","permission_mode":"on-request","resume_session":"thread"}"#,
        &defaults,
    )
    .unwrap();
    assert_eq!(explicit.model.as_deref(), Some("chosen"));
    assert_eq!(explicit.permission_mode.as_deref(), Some("on-request"));
    assert_eq!(explicit.resume_session.as_deref(), Some("thread"));
    let empty = start_options_with_defaults("{}", &defaults).unwrap();
    assert_eq!(empty.model.as_deref(), Some("default-model"));
    assert_eq!(
        credential_name(AgentAdapterKind::Codex),
        Some("openai_api_key")
    );
    assert_eq!(
        credential_name(AgentAdapterKind::Claude),
        Some("anthropic_api_key")
    );
}

#[test]
fn late_replies_cannot_cross_credentials_backends_or_connections() {
    let mut generations = ReplyGenerations::default();
    let claude = generations.begin("claude");
    let codex = generations.begin("codex");
    assert!(generations.accepts("claude", claude));
    generations.begin("codex");
    assert!(!generations.accepts("codex", codex));
    assert!(generations.accepts("claude", claude));
    generations.disconnect();
    assert!(!generations.accepts("claude", claude));
}

#[test]
fn start_routing_rejects_disabled_unknown_and_legacy_unsupported_backends() {
    use bondsymphonic_ide::model::backends::resolve_start;
    let mut settings = Settings::default();
    let mut descriptors = legacy_descriptors();
    assert!(resolve_start(r#"{"adapter":"codex"}"#, &settings, &descriptors).is_err());
    settings.backends.get_mut("codex").unwrap().enabled = true;
    assert!(resolve_start(r#"{"adapter":"codex"}"#, &settings, &descriptors).is_err());
    let mut codex = descriptors[0].clone();
    codex.id = AgentAdapterKind::Codex;
    descriptors.push(codex);
    let (kind,options)=resolve_start(r#"{"adapter":"codex","model":"saved-model","permission_mode":"on-request","resume_session":"saved-thread","api_key":"untrusted"}"#,&settings,&descriptors).unwrap();
    assert_eq!(kind, AgentAdapterKind::Codex);
    assert_eq!(options.model.as_deref(), Some("saved-model"));
    assert_eq!(options.resume_session.as_deref(), Some("saved-thread"));
    assert!(options.api_key.is_none());
    assert!(resolve_start(r#"{"adapter":"future"}"#, &settings, &descriptors).is_err());
    settings.backends.get_mut("claude").unwrap().enabled = false;
    assert!(resolve_start("{}", &settings, &descriptors).is_err());
    assert!(resolve_start("broken", &settings, &descriptors).is_err());
}

#[test]
fn codex_session_approval_is_not_a_local_tool_allowlist() {
    use bondsymphonic_ide::model::backends::permission_reply;
    use bondsymphonic_proto::PermissionDecision;
    assert_eq!(
        permission_reply(AgentAdapterKind::Codex, true, true),
        (PermissionDecision::AllowForSession, false)
    );
    assert_eq!(
        permission_reply(AgentAdapterKind::Claude, true, true),
        (PermissionDecision::Allow, true)
    );
    assert_eq!(
        permission_reply(AgentAdapterKind::Codex, true, false),
        (PermissionDecision::Allow, false)
    );
    assert_eq!(
        permission_reply(AgentAdapterKind::Codex, false, true),
        (PermissionDecision::Deny, false)
    );
}

#[test]
fn choosing_cli_default_does_not_reapply_the_configured_default_model() {
    let defaults = BackendSettings {
        enabled: true,
        default_model: "configured-model".into(),
        default_permission_mode: "never".into(),
    };
    let raw = bondsymphonic_ide::qobjects::transcript_model::restart_options_with(
        r#"{"adapter":"codex","model":"old-model"}"#,
        Some("thread"),
        Some(""),
        None,
    );
    assert_eq!(
        start_options_with_defaults(&raw, &defaults)
            .unwrap()
            .model
            .as_deref(),
        Some("")
    );
}
