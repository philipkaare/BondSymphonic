use bondsymphonic_proto::{AgentAdapterKind, SetupAction};

#[test]
fn opencode_requires_a_protocol_that_old_backend_decoders_will_reject() {
    const _: () = assert!(bondsymphonic_proto::PROTOCOL_VERSION >= 3);
}

#[test]
fn opencode_is_a_distinct_backend_with_setup_actions() {
    let kind: AgentAdapterKind = serde_json::from_str("\"opencode\"").unwrap();
    assert_eq!(serde_json::to_string(&kind).unwrap(), "\"opencode\"");
    for action in ["install_opencode", "opencode_login", "opencode_logout"] {
        let parsed: SetupAction = serde_json::from_value(action.into()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), action);
    }
}

/// The uninstall actions are listed in `hello` capabilities, which a version-3
/// decoder cannot read, so they came with version 4.
#[test]
fn the_uninstall_actions_need_protocol_four() {
    assert_eq!(bondsymphonic_proto::PROTOCOL_VERSION, 4);
    for action in ["uninstall_claude", "uninstall_codex", "uninstall_opencode"] {
        let parsed: SetupAction = serde_json::from_value(action.into()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), action);
    }
}
