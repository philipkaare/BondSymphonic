mod common;
use bondsymphonic_daemon::{
    agents::{
        backend::{backend_for, Backend, ModelQuery},
        opencode_backend::{launch_config, resolve_selection, OpenCodeBackend},
        persist::{AgentRecord, AgentRecords},
        AgentManager,
    },
    daemon::Daemon,
    opencode::{ProviderStore, RoutingContext},
    sandbox::noop::NoopBackend,
    server::broadcast::EventBus,
    workspace::{agent_sandboxes::companion_spec, lifecycle, DataDirs},
};
use bondsymphonic_proto::{opencode::*, *};
use serde_json::json;
use std::sync::Arc;

#[test]
fn workspace_deletion_removes_opencode_state_only_for_that_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let dirs = DataDirs::new(&root);
    let removed = WorkspaceId("ws-removed".into());
    let retained = WorkspaceId("ws-retained".into());
    let mut removed_paths = Vec::new();
    let mut retained_paths = Vec::new();
    for backend in ["codex", "opencode"] {
        for area in ["agent-homes", "agent-caches", "agent-run"] {
            for id in [&removed, &retained] {
                let path = root.join(area).join(backend).join(id.as_str());
                std::fs::create_dir_all(&path).unwrap();
                assert!(path.canonicalize().unwrap().starts_with(&root));
                std::fs::write(path.join("owned-state"), "session data").unwrap();
                if id == &removed {
                    removed_paths.push(path);
                } else {
                    retained_paths.push(path);
                }
            }
        }
    }
    // Shared backend configuration is not owned by a single workspace.
    let shared = root.join("codex-home");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("config.toml"), "shared config").unwrap();
    dirs.remove_workspace(&removed);
    for path in removed_paths {
        assert!(
            !path.exists(),
            "workspace data survived: {}",
            path.display()
        );
    }
    for path in retained_paths {
        assert_eq!(
            std::fs::read_to_string(path.join("owned-state")).unwrap(),
            "session data"
        );
    }
    assert_eq!(
        std::fs::read_to_string(shared.join("config.toml")).unwrap(),
        "shared config"
    );
}

fn provider() -> OpenCodeProvider {
    OpenCodeProvider {
        id: "local".into(),
        name: "Local".into(),
        base_url: "http://localhost:11434/v1".into(),
        endpoint_target: EndpointTarget::Wsl,
        models: vec![OpenCodeModel {
            id: "qwen".into(),
            name: "Qwen".into(),
            context_limit: Some(32000),
            output_limit: Some(4000),
        }],
    }
}
fn options() -> AgentStartOptions {
    AgentStartOptions {
        command: None,
        resume_session: None,
        model: Some("qwen".into()),
        provider_id: Some("local".into()),
        permission_mode: Some("ask".into()),
        api_key: Some("secret-provider-key".into()),
    }
}
#[test]
fn provider_key_debug_redacted() {
    assert!(!format!("{:?}", options()).contains("secret-provider-key"));
    let query = Request::SystemListModels(ListModelsParams {
        adapter: Some(AgentAdapterKind::OpenCode),
        provider_id: Some("local".into()),
        api_key: Some("secret-provider-key".into()),
    });
    assert!(!format!("{query:?}").contains("secret-provider-key"));
}
#[test]
fn missing_provider_is_actionable() {
    let e = resolve_selection(&ProviderStore::default(), &options()).unwrap_err();
    assert!(e.message.contains("Settings"));
}
#[test]
fn provider_selection_preserves_identity_and_limits() {
    let store = ProviderStore::default();
    store.upsert(0, provider()).unwrap();
    let (p, m) = resolve_selection(&store, &options()).unwrap();
    assert_eq!((p.id.as_str(), m.as_str()), ("local", "qwen"));
    let c = launch_config(
        &p,
        "qwen",
        &options(),
        &RoutingContext::default(),
        vec![],
        json!({}),
    )
    .unwrap();
    assert_eq!(c.config["model"], "local/qwen");
    assert_eq!(
        c.config["provider"]["local"]["models"]["qwen"]["limit"]["context"],
        32000
    );
    assert_eq!(
        c.config["provider"]["local"]["options"]["baseURL"],
        "http://127.0.0.1:11434/v1"
    );
    assert_eq!(c.provider_endpoints, vec![("127.0.0.1".into(), 11434)]);
    assert!(!c.config.to_string().contains("secret-provider-key"));
    assert!(!format!("{c:?}").contains("secret-provider-key"));
}
#[test]
fn mcp_attached_once() {
    let c=launch_config(&provider(),"qwen",&options(),&RoutingContext::default(),vec![json!({"name":"jira","url":"http://bridge"})],json!({"jira":{"type":"remote","url":"http://duplicate"},"native":{"type":"local","command":["tool"]}})).unwrap();
    assert_eq!(c.mcp_servers.len(), 1);
    assert!(c.config["mcp"].get("jira").is_none());
    assert_eq!(c.config["mcp"]["native"]["command"][0], "tool");
}
#[test]
fn unauthenticated_provider_omits_key_reference() {
    let mut o = options();
    o.api_key = None;
    let c = launch_config(
        &provider(),
        "qwen",
        &o,
        &RoutingContext::default(),
        vec![],
        json!({}),
    )
    .unwrap();
    assert!(c.config["provider"]["local"]["options"]
        .get("apiKey")
        .is_none());
    assert!(c.env.is_empty());
}
#[test]
fn permissions_are_native_policy_and_no_cloud_fallback() {
    let c = launch_config(
        &provider(),
        "qwen",
        &options(),
        &RoutingContext::default(),
        vec![],
        json!({}),
    )
    .unwrap();
    assert_eq!(c.config["permission"]["*"], "ask");
    assert_eq!(c.config["enabled_providers"], json!(["local"]));
    let mut o = options();
    o.permission_mode = Some("allow".into());
    let c = launch_config(
        &provider(),
        "qwen",
        &o,
        &RoutingContext::default(),
        vec![],
        json!({}),
    )
    .unwrap();
    assert_eq!(c.config["permission"], "allow");
    assert_eq!(
        OpenCodeBackend.descriptor().default_permission_mode,
        "allow"
    );
}
#[test]
fn wrong_provider_model_cannot_fallback() {
    let store = ProviderStore::default();
    store.upsert(0, provider()).unwrap();
    let mut o = options();
    o.model = Some("cloud/model".into());
    assert!(resolve_selection(&store, &o).is_err());
}

#[test]
fn resume_requires_original_provider_and_model() {
    let store = ProviderStore::default();
    store.upsert(0, provider()).unwrap();
    let mut resumed = options();
    resumed.resume_session = Some("native-session".into());
    resumed.model = None;
    assert!(resolve_selection(&store, &resumed)
        .unwrap_err()
        .message
        .contains("saved model"));
    resumed.model = Some("removed".into());
    assert!(resolve_selection(&store, &resumed).is_err());
    resumed.model = Some("qwen".into());
    resumed.provider_id = Some("removed".into());
    assert!(resolve_selection(&store, &resumed).is_err());
}

#[test]
fn opencode_disabled_by_default() {
    assert_eq!(
        backend_for(AgentAdapterKind::OpenCode)
            .unwrap()
            .descriptor()
            .id,
        AgentAdapterKind::OpenCode
    );
    let mut no_selection = options();
    no_selection.provider_id = None;
    assert!(resolve_selection(&ProviderStore::default(), &no_selection).is_err());
    assert_eq!(
        serde_json::to_value(SetupAction::InstallOpenCode).unwrap(),
        "install_opencode"
    );
}

#[test]
fn provider_and_model_survive_restore_without_key() {
    let dir = tempfile::tempdir().unwrap();
    let records_path = dir.path().join("agents.json");
    let records = AgentRecords::new(&records_path);
    let mut selected = options();
    selected.model = Some("local/qwen".into());
    records.upsert(AgentRecord {
        agent_id: "ag_open".into(),
        workspace_id: "ws_open".into(),
        adapter: AgentAdapterKind::OpenCode,
        session_id: Some("native-session".into()),
        options: selected,
        started_at: "2026-10-02T00:00:00Z".into(),
        ended_at: None,
    });
    assert!(!std::fs::read_to_string(&records_path)
        .unwrap()
        .contains("secret-provider-key"));
    let manager = AgentManager::new(
        EventBus::new(8),
        dir.path().join("transcripts"),
        records_path,
    );
    manager.restore(&["ws_open".into()]);
    let restored = manager.records_of(&"ws_open".into());
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].provider_id.as_deref(), Some("local"));
    assert_eq!(restored[0].model.as_deref(), Some("local/qwen"));
    assert_eq!(restored[0].session_id.as_deref(), Some("native-session"));
}

#[tokio::test]
async fn opencode_has_own_home_and_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        Arc::new(NoopBackend),
        EventBus::new(8),
    )
    .unwrap();
    let created = lifecycle::create(
        &d,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            name: "open".into(),
            base_branch: "main".into(),
            in_place: false,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let ws = d.workspace(&created.id).unwrap();
    let spec = companion_spec(&d, &ws, &OpenCodeBackend).await.unwrap();
    assert_ne!(spec.home, d.dirs.home(&ws.id));
    assert_ne!(spec.run_dir, d.dirs.run(&ws.id));
    assert_eq!(
        spec.home,
        d.dirs
            .root
            .join("agent-homes/opencode")
            .join(ws.id.as_str())
    );
    assert_eq!(
        spec.env
            .iter()
            .find(|(k, _)| k == "NO_PROXY")
            .map(|(_, v)| v.as_str()),
        if cfg!(unix) { Some("") } else { None }
    );
    let companion = d
        .agent_sandboxes
        .ensure(&d, &ws, &OpenCodeBackend)
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&companion, &d.sandbox(&ws.id).unwrap()));
    d.agent_sandboxes
        .set_provider_endpoints(&ws.id, "first", vec![("127.0.0.1".into(), 11434)])
        .await
        .unwrap();
    d.agent_sandboxes
        .set_provider_endpoints(&ws.id, "second", vec![("10.0.0.2".into(), 8000)])
        .await
        .unwrap();
    d.agent_sandboxes
        .revoke_provider_endpoints("first")
        .await
        .unwrap();
    assert!(d
        .proxies
        .set_provider_endpoints(&ws.id, 1, "opencode", vec![("127.0.0.1".into(), 11434)])
        .is_err());
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
}

#[tokio::test]
async fn discovery_uses_prefixed_models_url_and_process_key() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 4096];
        let n = stream.read(&mut bytes).await.unwrap();
        let request = String::from_utf8_lossy(&bytes[..n]).to_string();
        let body = r#"{"data":[{"id":"new-model","name":"Discovered"}]}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        request
    });
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        Arc::new(NoopBackend),
        EventBus::new(8),
    )
    .unwrap();
    let mut p = provider();
    p.base_url = format!("http://127.0.0.1:{port}/v1");
    d.opencode_providers.upsert(0, p).unwrap();
    let result = OpenCodeBackend
        .list_models(
            &d,
            ModelQuery {
                provider_id: Some("local"),
                api_key: Some("secret-provider-key"),
            },
        )
        .await
        .unwrap();
    assert!(result.models.iter().any(|m| m.id == "new-model"));
    assert!(result.models.iter().any(|m| m.id == "qwen"));
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /v1/models HTTP/1.1"), "{request}");
    assert!(request.contains("Authorization: Bearer secret-provider-key"));
    assert!(
        !std::fs::read_to_string(d.dirs.root.join("opencode-providers.json"))
            .unwrap()
            .contains("secret-provider-key")
    );
}
