use bondsymphonic_daemon::agents::backend::{backend_for, runnable_adapters};
use bondsymphonic_proto::{AgentAdapterKind, ErrorCode};

#[test]
fn terminal_starts_still_require_a_pty() {
    let error = backend_for(AgentAdapterKind::Terminal).err().unwrap();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert!(error.message.contains("pty.open"));
}

#[test]
fn unavailable_clis_are_not_advertised_as_runnable() {
    assert_eq!(runnable_adapters(&[]), vec![AgentAdapterKind::Terminal]);
}

#[test]
fn claude_descriptor_preserves_its_modes_and_setup_actions() {
    let backend = backend_for(AgentAdapterKind::Claude).unwrap();
    let descriptor = backend.descriptor();
    assert_eq!(descriptor.default_permission_mode, "bypassPermissions");
    assert!(descriptor.permission_modes.iter().any(|m| m.id == "manual"));
    assert!(descriptor
        .setup_actions
        .contains(&bondsymphonic_proto::SetupAction::ClaudeSetupToken));
}
