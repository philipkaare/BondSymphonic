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

/// Settings lists the OpenCode model servers and tests each one. Both methods
/// are new, so a daemon offering them says so in `hello`, and a `hello` from a
/// daemon that predates them reads as "not offered".
#[test]
fn the_provider_methods_round_trip_and_are_capability_gated() {
    use bondsymphonic_proto::*;
    for req in [
        Request::OpenCodeListProviders {},
        Request::OpenCodeTestProvider(OpenCodeTestProviderParams {
            provider_id: "ollama".into(),
            api_key: Some("sk-secret".into()),
        }),
    ] {
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["method"], req.method_name());
        let back: Request = serde_json::from_value(wire).unwrap();
        assert_eq!(back, req);
    }
    assert_eq!(
        Request::OpenCodeListProviders {}.method_name(),
        "opencode.list_providers"
    );
    // The stored key travels in the request; it must not reach a log line.
    let params = OpenCodeTestProviderParams {
        provider_id: "ollama".into(),
        api_key: Some("sk-secret".into()),
    };
    assert!(!format!("{params:?}").contains("sk-secret"));
    // An older IDE sends no key.
    let bare: OpenCodeTestProviderParams = serde_json::from_str(r#"{"provider_id":"x"}"#).unwrap();
    assert_eq!(bare.api_key, None);

    let listed = OpenCodeProvidersResult {
        providers: vec![OpenCodeProvider {
            id: "ollama".into(),
            name: Some("Ollama".into()),
            base_url: Some("http://localhost:11434/v1".into()),
            model_count: 2,
            sandbox_route: SandboxRoute::Tunnelled,
            route_note: Some("carried to WSL's loopback".into()),
        }],
        warnings: vec!["opencode.json: not JSON".into()],
    };
    let back: OpenCodeProvidersResult =
        serde_json::from_value(serde_json::to_value(&listed).unwrap()).unwrap();
    assert_eq!(back, listed);
    let minimal: OpenCodeProvider =
        serde_json::from_str(r#"{"id":"x","sandbox_route":"something_new"}"#).unwrap();
    assert_eq!(minimal.sandbox_route, SandboxRoute::Unknown);
    assert_eq!(minimal.model_count, 0);

    let tested = OpenCodeProviderTest {
        outcome: ProviderTestOutcome::Reachable,
        message: "Reachable: 2 models".into(),
        url: Some("http://localhost:11434/v1/models".into()),
        status: Some(200),
        model_count: Some(2),
        model_ids: vec!["llama3".into(), "qwen".into()],
        key_source: None,
        elapsed_ms: 12,
    };
    let back: OpenCodeProviderTest =
        serde_json::from_value(serde_json::to_value(&tested).unwrap()).unwrap();
    assert_eq!(back, tested);
    for (outcome, word) in [
        (ProviderTestOutcome::HttpError, "http_error"),
        (ProviderTestOutcome::Refused, "refused"),
        (ProviderTestOutcome::Timeout, "timeout"),
        (ProviderTestOutcome::DnsFailure, "dns_failure"),
        (ProviderTestOutcome::NotGranted, "not_granted"),
    ] {
        assert_eq!(serde_json::to_value(outcome).unwrap(), word);
    }
    let later: OpenCodeProviderTest =
        serde_json::from_str(r#"{"outcome":"from_the_future","message":"m"}"#).unwrap();
    assert_eq!(later.outcome, ProviderTestOutcome::Unknown);

    // Gated: an old daemon's capabilities say nothing, which is "no".
    let old: Capabilities = serde_json::from_str(
        r#"{"sandbox_backend":"noop","git_protect":false,"adapters":["claude"]}"#,
    )
    .unwrap();
    assert!(!old.opencode_providers);
    let mut new = old.clone();
    new.opencode_providers = true;
    let back: Capabilities = serde_json::from_value(serde_json::to_value(&new).unwrap()).unwrap();
    assert!(back.opencode_providers);
}
