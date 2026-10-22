use bondsymphonic_proto::*;

#[test]
fn mcp_additive_defaults() {
    // A daemon that predates the registry sends no `mcp_servers`, which must
    // read as "not offered" so the IDE hides the MCP controls.
    let caps: Capabilities =
        serde_json::from_str(r#"{"sandbox_backend":"noop","git_protect":false,"adapters":[]}"#)
            .unwrap();
    assert!(!caps.mcp_servers);
    let on = Capabilities {
        mcp_servers: true,
        ..caps.clone()
    };
    assert_eq!(serde_json::to_value(&on).unwrap()["mcp_servers"], true);
    assert!(serde_json::to_value(&caps)
        .unwrap()
        .get("mcp_servers")
        .is_none());
    let registry: McpRegistrySnapshot =
        serde_json::from_str(r#"{"revision":1,"servers":[]}"#).unwrap();
    assert_eq!(
        registry.workspace_override(&"ws_a".into(), &"mcp_a".into()),
        McpWorkspaceOverride::Inherit
    );
    let config: McpServerConfig = serde_json::from_str(r#"{"id":"mcp_a","name":"Example","transport":{"type":"http","url":"https://example.com/mcp"}}"#).unwrap();
    assert!(config.enabled);
    assert!(config.default_workspace_enabled);
    assert_eq!(config.enabled_backends, vec!["claude", "codex", "opencode"]);
}

#[test]
fn mcp_transport_roundtrip() {
    for fixture in [
        r#"{"type":"stdio","command":"example","args":[],"env":{},"secret_env":["API_KEY"]}"#,
        r#"{"type":"http","url":"https://example.com/mcp","auth":{"type":"bearer","token":"secret_ref"}}"#,
        r#"{"type":"http","url":"https://example.com/mcp","auth":{"type":"headers","headers":{"X-Key":"ref_a"}}}"#,
    ] {
        let transport: McpTransportConfig = serde_json::from_str(fixture).unwrap();
        let encoded = serde_json::to_string(&transport).unwrap();
        assert_eq!(
            serde_json::from_str::<McpTransportConfig>(&encoded).unwrap(),
            transport
        );
    }
    let stdio: McpTransportConfig =
        serde_json::from_str(r#"{"type":"stdio","command":"example"}"#).unwrap();
    assert_eq!(
        stdio,
        McpTransportConfig::Stdio {
            command: "example".into(),
            args: vec![],
            env: Default::default(),
            secret_env: vec![],
        }
    );
    assert!(serde_json::from_str::<McpTransportConfig>(
        r#"{"type":"sse","url":"https://example.com"}"#
    )
    .is_err());
}

#[test]
fn mcp_secret_debug_redacted() {
    const SECRET: &str = "s3cr3t-sentinel";
    for method in [
        "mcp.upsert",
        "mcp.import_preview",
        "mcp.import_apply",
        "mcp.callback",
    ] {
        let params = match method {
            "mcp.upsert" => {
                serde_json::json!({"server":{"id":"mcp_a","name":"Example","transport":{"type":"http","url":"https://example.com/mcp"}},"secrets":[{"reference":"ref_a","action":"set","value":SECRET}]})
            }
            "mcp.callback" => serde_json::json!({"server_id":"mcp_a","state":SECRET,"code":SECRET}),
            _ => serde_json::json!({"document":SECRET}),
        };
        let request: Request =
            serde_json::from_value(serde_json::json!({"method":method,"params":params})).unwrap();
        assert!(!format!("{request:?}").contains(SECRET));
    }
    let snapshot: McpRegistrySnapshot = serde_json::from_value(serde_json::json!({"revision":1,"servers":[{"id":"mcp_a","name":"Example","transport":{"type":"http","url":"https://example.com/mcp","auth":{"type":"bearer","token":"ref_a"}},"secrets":[SECRET]}]})).unwrap();
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(SECRET));
    assert_eq!(format!("{:?}", McpSecretValue(SECRET.into())), "[REDACTED]");
}

#[test]
fn mcp_requests_roundtrip() {
    for (method, params) in [
        ("mcp.list", serde_json::json!({})),
        ("mcp.export", serde_json::json!({})),
        ("mcp.restore_preset", serde_json::json!({})),
        ("mcp.remove", serde_json::json!({"server_id":"mcp_a"})),
        ("mcp.disconnect", serde_json::json!({"server_id":"mcp_a"})),
        ("mcp.test", serde_json::json!({"server_id":"mcp_a"})),
        (
            "mcp.connect",
            serde_json::json!({"server_id":"mcp_a","redirect_uri":"http://127.0.0.1:4123/callback"}),
        ),
        (
            "mcp.override",
            serde_json::json!({"server_id":"mcp_a","workspace_id":"ws_a","value":"disabled"}),
        ),
    ] {
        let request: Request =
            serde_json::from_value(serde_json::json!({"method":method,"params":params})).unwrap();
        assert_eq!(request.method_name(), method);
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
    }
}

#[test]
fn mcp_oauth_metadata_and_callback_issuer_roundtrip() {
    let auth: McpAuthConfig = serde_json::from_str(r#"{"type":"oauth","client_id":"registered","client_metadata_url":"https://client.example/metadata.json"}"#).unwrap();
    assert_eq!(
        serde_json::to_value(auth).unwrap()["client_metadata_url"],
        "https://client.example/metadata.json"
    );
    let callback: McpCallbackParams = serde_json::from_str(
        r#"{"server_id":"mcp_a","state":"state","code":"code","issuer":"https://issuer.example"}"#,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(callback).unwrap()["issuer"],
        "https://issuer.example"
    );
    let old: McpCallbackParams =
        serde_json::from_str(r#"{"server_id":"mcp_a","state":"state","code":"code"}"#).unwrap();
    assert!(serde_json::to_value(old).unwrap()["issuer"].is_null());
}
