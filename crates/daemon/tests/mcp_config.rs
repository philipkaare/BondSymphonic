//! The user-configured MCP server registry: persistence, validation, secrets
//! and import/export. Secret values use the sentinel `s3cr3t-sentinel`, which
//! must never surface outside the private secrets file.
use bondsymphonic_daemon::mcp::servers::McpServers;
mod common;

use bondsymphonic_proto::*;

const SENTINEL: &str = "s3cr3t-sentinel";

fn load(root: &std::path::Path) -> McpServers {
    McpServers::load(root).unwrap()
}

fn http_server(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: id.into(),
        transport: McpTransportConfig::Http {
            url: "https://example.com/mcp".into(),
            auth: McpAuthConfig::Bearer {
                token: "test-token".into(),
            },
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

fn stdio_server(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: id.into(),
        transport: McpTransportConfig::Stdio {
            command: "node".into(),
            args: vec!["server.js".into()],
            env: Default::default(),
            secret_env: vec!["API_KEY".into()],
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

fn set(reference: &str, value: &str) -> McpSecretWrite {
    McpSecretWrite::Set {
        reference: reference.into(),
        value: McpSecretValue(value.into()),
    }
}

fn upsert(server: McpServerConfig, secrets: Vec<McpSecretWrite>) -> McpUpsertParams {
    McpUpsertParams {
        server,
        secrets,
        expected_revision: None,
    }
}

fn import(document: &str) -> McpImportParams {
    McpImportParams {
        document: McpSecretValue(document.into()),
        resolutions: Default::default(),
        expected_revision: None,
    }
}

/// Every file under `<root>/mcp`, with its content, for "the secret is nowhere
/// but the private file" checks.
fn mcp_files(root: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_dir(root.join("mcp"))
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&p).unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn fresh_registry_is_seeded_with_the_jira_preset() {
    let root = tempfile::tempdir().unwrap();
    let snapshot = load(root.path()).snapshot();
    assert_eq!(snapshot.servers.len(), 1);
    let jira = &snapshot.servers[0];
    assert_eq!(jira.id.as_str(), "jira");
    assert_eq!(jira.name, "Jira (Atlassian)");
    assert_eq!(jira.enabled_backends, ["claude", "codex", "opencode"]);
    assert!(jira.default_workspace_enabled);
    match &jira.transport {
        McpTransportConfig::Http { url, auth } => {
            assert_eq!(url, "https://mcp.atlassian.com/v2/mcp");
            assert!(matches!(
                auth,
                McpAuthConfig::OAuth {
                    client_id: None,
                    ..
                }
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn preset_removed_stays_removed() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    servers.remove(&"jira".into()).unwrap();
    assert!(load(root.path()).snapshot().servers.is_empty());
    // Only an explicit restore brings it back, and only once.
    let restored = load(root.path()).restore_preset().unwrap();
    assert_eq!(restored.servers.len(), 1);
    assert_eq!(
        load(root.path()).restore_preset().unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[test]
fn revision_conflict_preserves_config() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let before = servers.snapshot();
    let mut server = before.servers[0].clone();
    server.name = "changed".into();
    let error = servers
        .upsert(McpUpsertParams {
            server,
            secrets: vec![],
            expected_revision: Some(99),
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert_eq!(before, servers.snapshot());
}

#[test]
fn import_requires_collision_decision() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let doc = r#"{"mcpServers":{"jira":{"command":"node"}}}"#;
    let params = import(doc);
    assert_eq!(servers.import_preview(&params).unwrap().collisions.len(), 1);
    assert_eq!(
        servers.import_apply(params).unwrap_err().code,
        ErrorCode::Conflict
    );
    let params = McpImportParams {
        resolutions: [("jira".to_string(), "jira-copy".into())].into(),
        expected_revision: Some(0),
        ..import(doc)
    };
    let saved = servers.import_apply(params).unwrap();
    assert_eq!(saved.servers.len(), 2);
    assert_eq!(load(root.path()).snapshot(), saved);
}

#[test]
fn export_omits_secret_values() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let params = import(&format!(
        r#"{{"mcpServers":{{"test":{{"command":"node","env":{{"TOKEN":"{SENTINEL}"}}}}}}}}"#
    ));
    assert!(!format!("{:?}", servers.import_preview(&params).unwrap()).contains(SENTINEL));
    servers.import_apply(params).unwrap();
    let exported = servers.export();
    assert!(!exported.contains(SENTINEL));
    // The reference (the variable name) is exported, the value is not.
    assert!(exported.contains("TOKEN"));
    for (name, content) in mcp_files(root.path()) {
        assert_eq!(
            content.contains(SENTINEL),
            name.starts_with("secrets-"),
            "{name}"
        );
    }
}

#[test]
fn workspace_override_inherits() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let ws = WorkspaceId("ws_test".into());
    let over = |value| McpOverrideParams {
        server_id: "jira".into(),
        workspace_id: ws.clone(),
        value,
    };
    let claude = AgentAdapterKind::Claude;
    assert_eq!(servers.resolve(&ws, claude).len(), 1);
    servers
        .set_override(over(McpWorkspaceOverride::Disabled))
        .unwrap();
    assert!(servers.resolve(&ws, claude).is_empty());
    assert_eq!(load(root.path()).resolve(&ws, claude).len(), 0, "persisted");
    // Another workspace was never affected.
    let other = WorkspaceId("ws_other".into());
    assert_eq!(servers.resolve(&other, claude).len(), 1);
    servers
        .set_override(over(McpWorkspaceOverride::Inherit))
        .unwrap();
    assert_eq!(servers.resolve(&ws, claude).len(), 1);
    assert!(servers
        .snapshot()
        .overrides
        .get(&ws)
        .is_none_or(|m| m.is_empty()));
}

#[test]
fn resolve_filters_by_backend_enabled_and_default() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let ws = WorkspaceId("ws_test".into());
    servers
        .upsert(upsert(stdio_server("local"), vec![]))
        .unwrap();
    let ids = |kind| -> Vec<String> {
        servers
            .resolve(&ws, kind)
            .iter()
            .map(|s| s.id.0.clone())
            .collect()
    };
    assert_eq!(ids(AgentAdapterKind::Claude), ["jira", "local"]);
    assert_eq!(ids(AgentAdapterKind::Codex), ["jira"]);
    assert!(ids(AgentAdapterKind::Terminal).is_empty());
    let mut off = stdio_server("local");
    off.default_workspace_enabled = false;
    servers.upsert(upsert(off, vec![])).unwrap();
    assert_eq!(ids(AgentAdapterKind::Claude), ["jira"]);
    servers
        .set_override(McpOverrideParams {
            server_id: "local".into(),
            workspace_id: ws.clone(),
            value: McpWorkspaceOverride::Enabled,
        })
        .unwrap();
    assert_eq!(ids(AgentAdapterKind::Claude), ["jira", "local"]);
    let mut disabled = stdio_server("local");
    disabled.enabled = false;
    servers.upsert(upsert(disabled, vec![])).unwrap();
    assert_eq!(ids(AgentAdapterKind::Claude), ["jira"]);
}

#[cfg(unix)]
#[test]
fn secrets_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    servers
        .upsert(upsert(
            http_server("test"),
            vec![set("test-token", SENTINEL)],
        ))
        .unwrap();
    let private: Vec<_> = std::fs::read_dir(root.path().join("mcp"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("secrets-")
        })
        .collect();
    assert_eq!(private.len(), 1, "{private:?}");
    let mode = std::fs::metadata(&private[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn reserved_id_rejected() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let before = servers.snapshot();
    let error = servers
        .upsert(upsert(http_server("bondsymphonic"), vec![]))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert_eq!(before, servers.snapshot());
    // An import cannot smuggle the reserved name in either.
    let preview = servers
        .import_preview(&import(
            r#"{"mcpServers":{"bondsymphonic":{"command":"node"}}}"#,
        ))
        .unwrap();
    assert!(preview.servers.is_empty());
    assert_eq!(preview.issues.len(), 1);
}

#[test]
fn invalid_ids_and_commands_rejected() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    for id in [
        "",
        "Upper",
        "-lead",
        "has space",
        "../escape",
        &"a".repeat(33),
    ] {
        let error = servers.upsert(upsert(http_server(id), vec![])).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams, "{id:?}");
    }
    assert!(servers
        .upsert(upsert(http_server(&"a".repeat(32)), vec![]))
        .is_ok());
    let mut empty = stdio_server("local");
    if let McpTransportConfig::Stdio { command, .. } = &mut empty.transport {
        *command = "  ".into();
    }
    assert_eq!(
        servers.upsert(upsert(empty, vec![])).unwrap_err().code,
        ErrorCode::InvalidParams
    );
}

/// One test, because it flips a process-wide variable.
#[test]
fn http_url_policy() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let with_url = |url: &str| {
        let mut s = http_server("remote");
        s.transport = McpTransportConfig::Http {
            url: url.into(),
            auth: McpAuthConfig::None,
        };
        s
    };
    let try_url = |url: &str| servers.upsert(upsert(with_url(url), vec![]));
    std::env::remove_var("BS_MCP_ALLOW_HTTP");
    assert!(try_url("https://example.com/mcp").is_ok());
    for bad in [
        "http://example.com/mcp",
        "http://127.0.0.1:8080/mcp",
        "ftp://example.com/mcp",
        "https://",
        "https://user:pw@example.com/mcp",
        "https://example.com/mcp?key=abc",
    ] {
        assert_eq!(
            try_url(bad).unwrap_err().code,
            ErrorCode::InvalidParams,
            "{bad}"
        );
    }
    std::env::set_var("BS_MCP_ALLOW_HTTP", "1");
    assert!(try_url("http://127.0.0.1:8080/mcp").is_ok());
    assert!(try_url("http://localhost:8080/mcp").is_ok());
    assert!(try_url("http://[::1]:8080/mcp").is_ok());
    // Loopback only: the variable is not a license for plaintext to the internet.
    assert!(try_url("http://example.com/mcp").is_err());
    assert!(try_url("http://127.0.0.1.example.com/mcp").is_err());
    std::env::remove_var("BS_MCP_ALLOW_HTTP");
}

#[test]
fn sse_import_reported_unsupported() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let params =
        import(r#"{"mcpServers":{"legacy":{"type":"sse","url":"https://example.com/sse"}}}"#);
    let preview = servers.import_preview(&params).unwrap();
    assert!(preview.servers.is_empty());
    assert_eq!(preview.issues.len(), 1);
    assert_eq!(preview.issues[0].field, "type");
    assert_eq!(
        servers.import_apply(params).unwrap_err().code,
        ErrorCode::InvalidParams
    );
    assert_eq!(servers.snapshot().servers.len(), 1);
}

#[test]
fn plain_http_import_reported_unsupported() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let preview = servers
        .import_preview(&import(
            r#"{"mcpServers":{"plain":{"url":"http://example.com/mcp"}}}"#,
        ))
        .unwrap();
    assert!(preview.servers.is_empty());
    assert_eq!(preview.issues[0].field, "url");
}

#[test]
fn import_reports_unknown_fields_without_echoing_values() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let doc = format!(
        r#"{{"mcpServers":{{"x":{{"command":"node","password":"{SENTINEL}"}},"{SENTINEL}":{{"type":"sse","url":"https://e.com/"}}}}}}"#
    );
    let preview = servers.import_preview(&import(&doc)).unwrap();
    assert_eq!(preview.issues.len(), 2);
    assert!(!format!("{preview:?}").contains(SENTINEL));
}

#[test]
fn import_turns_headers_into_secret_references() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let doc = format!(
        r#"{{"mcpServers":{{"remote":{{"url":"https://example.com/mcp","headers":{{"X-Api-Key":"{SENTINEL}"}}}}}}}}"#
    );
    let saved = servers.import_apply(import(&doc)).unwrap();
    let remote = saved
        .servers
        .iter()
        .find(|s| s.id.as_str() == "remote")
        .unwrap();
    match &remote.transport {
        McpTransportConfig::Http {
            auth: McpAuthConfig::Headers { headers },
            ..
        } => {
            assert_eq!(headers.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    assert!(!servers.export().contains(SENTINEL));
}

#[test]
fn credential_arguments_are_rejected() {
    for args in [
        vec!["--token", "x"],
        vec!["--api-key=x"],
        vec!["password=x"],
        vec!["Bearer x"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let servers = load(root.path());
        let doc = serde_json::json!({"mcpServers":{"test":{"command":"node","args":args}}});
        let params = import(&doc.to_string());
        let preview = servers.import_preview(&params).unwrap();
        assert!(preview.servers.is_empty(), "{args:?}");
        assert!(servers.import_apply(params).is_err());
        let mut server = stdio_server("test");
        if let McpTransportConfig::Stdio { args: a, .. } = &mut server.transport {
            *a = args.iter().map(|s| s.to_string()).collect();
        }
        assert!(servers.upsert(upsert(server, vec![])).is_err());
    }
}

#[test]
fn secrets_are_write_only_and_removed_with_server() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let params = McpUpsertParams {
        expected_revision: Some(0),
        ..upsert(http_server("test"), vec![set("test-token", SENTINEL)])
    };
    assert!(!format!("{params:?}").contains(SENTINEL));
    servers.upsert(params).unwrap();
    assert!(!format!("{:?}", servers.snapshot()).contains(SENTINEL));
    assert!(!servers.export().contains(SENTINEL));
    // A blank Set keeps the stored value (the edit form's untouched field).
    servers
        .upsert(upsert(http_server("test"), vec![set("test-token", " ")]))
        .unwrap();
    let held: Vec<_> = mcp_files(root.path())
        .into_iter()
        .filter(|(_, c)| c.contains(SENTINEL))
        .collect();
    assert_eq!(held.len(), 1);
    assert!(held[0].0.starts_with("secrets-"));
    // The previous version of the secrets file is gone, not left to leak.
    assert_eq!(
        mcp_files(root.path())
            .iter()
            .filter(|(n, _)| n.starts_with("secrets-"))
            .count(),
        1
    );
    servers.remove(&"test".into()).unwrap();
    assert!(mcp_files(root.path())
        .iter()
        .all(|(_, c)| !c.contains(SENTINEL)));
}

#[test]
fn explicit_secret_remove_clears_value_but_keeps_reference() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    servers
        .upsert(upsert(
            http_server("test"),
            vec![set("test-token", SENTINEL)],
        ))
        .unwrap();
    servers
        .upsert(upsert(
            http_server("test"),
            vec![McpSecretWrite::Remove {
                reference: "test-token".into(),
            }],
        ))
        .unwrap();
    assert!(servers.export().contains("test-token"));
    assert!(mcp_files(root.path())
        .iter()
        .all(|(_, c)| !c.contains(SENTINEL)));
}

#[test]
fn secret_writes_must_name_a_reference_of_the_server() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let before = servers.snapshot();
    for writes in [
        vec![set("not-referenced", SENTINEL)],
        vec![set("test-token", SENTINEL), set("test-token", "again")],
    ] {
        let error = servers
            .upsert(upsert(http_server("test"), writes))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(!format!("{error:?}").contains(SENTINEL));
    }
    assert_eq!(before, servers.snapshot());
    assert!(mcp_files(root.path())
        .iter()
        .all(|(_, c)| !c.contains(SENTINEL)));
}

#[test]
fn stdio_secret_env_is_keyed_by_variable_name() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    servers
        .upsert(upsert(
            stdio_server("local"),
            vec![set("API_KEY", SENTINEL)],
        ))
        .unwrap();
    // Two servers may each have an API_KEY: secrets are scoped per server id.
    servers
        .upsert(upsert(
            stdio_server("other"),
            vec![set("API_KEY", "other-value")],
        ))
        .unwrap();
    // A name that is not one of the server's secret_env is refused.
    assert!(servers
        .upsert(upsert(stdio_server("local"), vec![set("OTHER", "x")]))
        .is_err());
}

#[test]
fn invalid_update_never_persists_partial_secrets() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let before = servers.snapshot();
    let error = servers
        .upsert(upsert(
            http_server("../escape"),
            vec![set("test-token", SENTINEL)],
        ))
        .unwrap_err();
    assert!(!format!("{error:?}").contains(SENTINEL));
    assert_eq!(before, servers.snapshot());
    assert!(mcp_files(root.path())
        .iter()
        .all(|(_, c)| !c.contains(SENTINEL)));
}

#[test]
fn failed_registry_write_preserves_published_state() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let before = servers.snapshot();
    let path = root.path().join("mcp/registry.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(servers
        .upsert(upsert(
            http_server("test"),
            vec![set("test-token", SENTINEL)]
        ))
        .is_err());
    assert_eq!(before, servers.snapshot());
    // The registry.json stand-in and the one live secrets file; the new
    // version written for the failed save is removed again.
    assert_eq!(
        std::fs::read_dir(root.path().join("mcp")).unwrap().count(),
        2
    );
}

#[test]
fn reload_removes_only_own_crash_leftovers() {
    let root = tempfile::tempdir().unwrap();
    load(root.path());
    let dir = root.path().join("mcp");
    let stale_version = dir.join("secrets-deadbeef");
    let stale_tmp = dir.join(".secrets-deadbeef.123.4.tmp");
    let unrelated = dir.join(".credentials.123.4.tmp");
    let misleading = dir.join(".secrets-important.tmp");
    for path in [&stale_version, &stale_tmp, &unrelated, &misleading] {
        std::fs::write(path, SENTINEL).unwrap();
    }
    let servers = load(root.path());
    assert!(!stale_version.exists());
    assert!(!stale_tmp.exists());
    assert!(unrelated.exists());
    assert!(misleading.exists());
    assert_eq!(servers.snapshot().servers.len(), 1);
}

#[test]
fn state_survives_reload_and_changes_are_announced() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    let rx = servers.subscribe();
    assert_eq!(*rx.borrow(), 0);
    let saved = servers
        .upsert(upsert(
            http_server("test"),
            vec![set("test-token", SENTINEL)],
        ))
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(*rx.borrow(), 1);
    assert_eq!(load(root.path()).snapshot(), saved);
}

#[tokio::test]
async fn rpc_handlers_serve_the_registry_and_announce_changes() {
    let root = tempfile::tempdir().unwrap();
    let (port, token, _daemon, cancel) = common::start_daemon(&root.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;

    let list: McpListResult =
        serde_json::from_value(c.call(Request::McpList {}).await.unwrap()).unwrap();
    assert_eq!(list.registry.servers.len(), 1);
    assert_eq!(list.statuses.len(), 1);
    assert_eq!(list.statuses[0].state, McpConnectionState::Disconnected);

    let saved: McpRegistrySnapshot = serde_json::from_value(
        c.call(Request::McpUpsert(upsert(
            http_server("test"),
            vec![set("test-token", SENTINEL)],
        )))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved.revision, 1);
    assert!(c
        .drain_events()
        .iter()
        .any(|(_, e)| matches!(e, Event::McpChanged { revision: 1 })));

    // A refused change announces nothing.
    let refused = c
        .call(Request::McpUpsert(upsert(
            http_server("bondsymphonic"),
            vec![],
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidParams);
    assert!(c.drain_events().is_empty());

    let export = c.call(Request::McpExport {}).await.unwrap();
    let document = export["document"].as_str().unwrap();
    assert!(document.contains("test-token") && !document.contains(SENTINEL));
    let relisted = c.call(Request::McpList {}).await.unwrap().to_string();
    assert!(!relisted.contains(SENTINEL));

    c.call(Request::McpRemove(McpServerIdParams {
        server_id: "test".into(),
    }))
    .await
    .unwrap();
    c.call(Request::McpRemove(McpServerIdParams {
        server_id: "jira".into(),
    }))
    .await
    .unwrap();
    let restored: McpRegistrySnapshot =
        serde_json::from_value(c.call(Request::McpRestorePreset {}).await.unwrap()).unwrap();
    assert_eq!(restored.servers.len(), 1);

    // Connection-state requests are served, and each one tells the clients
    // to re-list. A callback URL without a port is refused before any
    // network traffic.
    c.drain_events();
    let connect = c
        .call(Request::McpConnect(McpConnectParams {
            server_id: "jira".into(),
            redirect_uri: "http://127.0.0.1/cb".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(connect.code, ErrorCode::InvalidParams, "{connect:?}");
    let callback = c
        .call(Request::McpCallback(McpCallbackParams {
            server_id: "jira".into(),
            state: McpSecretValue(SENTINEL.into()),
            code: McpSecretValue(SENTINEL.into()),
            issuer: None,
        }))
        .await
        .unwrap_err();
    assert!(!callback.message.contains(SENTINEL));
    let status: McpServerStatus = serde_json::from_value(
        c.call(Request::McpDisconnect(McpServerIdParams {
            server_id: "jira".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(status.state, McpConnectionState::Disconnected);
    assert!(c
        .drain_events()
        .iter()
        .any(|(_, e)| matches!(e, Event::McpChanged { .. })));
    c.call(Request::McpUpsert(upsert(stdio_server("local"), vec![])))
        .await
        .unwrap();
    let tested: McpTestResult = serde_json::from_value(
        c.call(Request::McpTest(McpTestParams {
            server_id: "local".into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tested.status.state, McpConnectionState::Error);
    cancel.cancel();
}

#[test]
fn env_name_with_newline_rejected() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    for name in ["A\nLD_PRELOAD", "1ABC", "A B", "A=B", ""] {
        let mut secret = stdio_server("local");
        if let McpTransportConfig::Stdio { secret_env, .. } = &mut secret.transport {
            *secret_env = vec![name.into()];
        }
        let mut plain = stdio_server("local");
        if let McpTransportConfig::Stdio {
            env, secret_env, ..
        } = &mut plain.transport
        {
            secret_env.clear();
            env.insert(name.into(), "v".into());
        }
        for server in [secret, plain] {
            assert_eq!(
                servers.upsert(upsert(server, vec![])).unwrap_err().code,
                ErrorCode::InvalidParams,
                "{name:?}"
            );
        }
    }
    let doc =
        serde_json::json!({"mcpServers":{"x":{"command":"node","env":{"A\nLD_PRELOAD":"v"}}}});
    let preview = servers.import_preview(&import(&doc.to_string())).unwrap();
    assert!(preview.servers.is_empty());
    assert_eq!(preview.issues.len(), 1);
    assert!(servers.import_apply(import(&doc.to_string())).is_err());
}

fn assert_degraded(servers: &McpServers, path: &std::path::Path) {
    assert!(servers
        .resolve(&WorkspaceId("ws".into()), AgentAdapterKind::Claude)
        .is_empty());
    let message = format!("{path:?}");
    let message = message.trim_matches('"');
    for error in [
        servers.list().unwrap_err(),
        servers
            .upsert(upsert(http_server("a"), vec![]))
            .unwrap_err(),
        servers.remove(&"jira".into()).unwrap_err(),
        servers.restore_preset().unwrap_err(),
        servers.import_preview(&import("{}")).unwrap_err(),
    ] {
        assert_eq!(error.code, ErrorCode::IoError);
        assert!(error.message.contains("MCP registry"), "{error:?}");
        assert!(
            error.message.contains("registry.json"),
            "{error:?} {message}"
        );
    }
    assert!(servers.ensure_usable().is_err());
}

#[test]
fn corrupt_registry_loads_degraded_and_is_left_untouched() {
    let root = tempfile::tempdir().unwrap();
    load(root.path());
    let path = root.path().join("mcp/registry.json");
    std::fs::write(&path, "{ nope").unwrap();
    let before = mcp_files(root.path());
    let servers = McpServers::load(root.path()).unwrap();
    assert_degraded(&servers, &path);
    // No re-seeded Jira, no sweep, nothing rewritten.
    let mut after = mcp_files(root.path());
    let mut before = before;
    before.sort();
    after.sort();
    assert_eq!(before, after);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ nope");
}

#[test]
fn missing_secrets_file_loads_degraded() {
    let root = tempfile::tempdir().unwrap();
    load(root.path());
    for (name, _) in mcp_files(root.path()) {
        if name.starts_with("secrets-") {
            std::fs::remove_file(root.path().join("mcp").join(name)).unwrap();
        }
    }
    let servers = McpServers::load(root.path()).unwrap();
    assert_degraded(&servers, &root.path().join("mcp/registry.json"));
}

#[test]
fn unwritable_first_start_loads_degraded() {
    let root = tempfile::tempdir().unwrap();
    // A file where the mcp directory should be: the seed write cannot succeed.
    std::fs::write(root.path().join("mcp"), "x").unwrap();
    let servers = McpServers::load(root.path()).unwrap();
    assert!(servers.ensure_usable().is_err());
}

#[test]
fn invalid_stored_server_is_skipped_not_fatal() {
    let root = tempfile::tempdir().unwrap();
    let servers = load(root.path());
    servers.upsert(upsert(http_server("good"), vec![])).unwrap();
    let path = root.path().join("mcp/registry.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut bad = serde_json::to_value(http_server("plain")).unwrap();
    bad["transport"]["url"] = "http://example.com/mcp".into();
    manifest["servers"].as_array_mut().unwrap().push(bad);
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let reloaded = McpServers::load(root.path()).unwrap();
    assert!(reloaded.ensure_usable().is_ok());
    let ids: Vec<_> = reloaded
        .snapshot()
        .servers
        .iter()
        .map(|s| s.id.0.clone())
        .collect();
    assert_eq!(ids, ["jira", "good"]);
}

#[test]
fn daemon_still_constructs_with_a_corrupt_registry() {
    use bondsymphonic_daemon::{daemon::Daemon, server::broadcast::EventBus, workspace::DataDirs};
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(data.join("mcp")).unwrap();
    std::fs::write(data.join("mcp/registry.json"), "{ nope").unwrap();
    let daemon = Daemon::new(
        DataDirs::new(&data),
        bondsymphonic_daemon::sandbox::backend_for("noop"),
        EventBus::new(16),
    )
    .unwrap();
    assert!(daemon.mcp_servers.ensure_usable().is_err());
}
