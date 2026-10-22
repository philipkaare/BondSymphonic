use bondsymphonic_daemon::net::proxy::EndpointGrants;
use bondsymphonic_daemon::opencode::{ProviderEndpoint, ProviderStore, RoutingContext};
use bondsymphonic_proto::opencode::*;
fn provider() -> OpenCodeProvider {
    OpenCodeProvider {
        id: "local".into(),
        name: "Local".into(),
        base_url: "http://[::1]:11434/prefix/v1".into(),
        endpoint_target: EndpointTarget::Wsl,
        models: vec![OpenCodeModel {
            id: "qwen".into(),
            name: "Qwen".into(),
            context_limit: None,
            output_limit: None,
        }],
    }
}
#[test]
fn prefix_and_ipv6_preserved() {
    let e = ProviderEndpoint::parse(&provider()).unwrap();
    assert_eq!(e.models_url(), "http://[::1]:11434/prefix/v1/models");
    assert_eq!(e.host, "::1");
}
#[test]
fn endpoint_grant_is_backend_and_port_scoped() {
    let mut g = EndpointGrants::default();
    g.replace("opencode", 7, vec![("::1".into(), 11434)]);
    assert!(g.allows("opencode", 7, "::1", 11434));
    assert!(!g.allows("claude", 7, "::1", 11434));
    assert!(!g.allows("opencode", 7, "::1", 80));
    g.replace("opencode", 8, vec![]);
    assert!(!g.allows("opencode", 7, "::1", 11434));
}
#[test]
fn model_discovery_generation_guard() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let ticket = s.discovery_ticket("local", 3, 4).unwrap();
    s.upsert(1, provider()).unwrap();
    assert!(!s.apply_discovery(&ticket, 3, 4, vec![]).unwrap());
}
#[test]
fn manual_models_without_metadata() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let t = s.discovery_ticket("local", 3, 4).unwrap();
    assert!(s.apply_discovery(&t, 3, 4, vec![]).unwrap());
    assert_eq!(s.resolve("local").unwrap().models[0].id, "qwen");
}
#[test]
fn provider_export_has_no_key() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let json = serde_json::to_string(&s.snapshot()).unwrap();
    assert!(!json.contains("api_key"));
    assert!(!json.contains("credential"));
}
#[test]
fn unsafe_urls_and_missing_windows_route() {
    let mut p = provider();
    for url in [
        "http://user:secret@localhost/v1",
        "http://localhost/v1#secret",
        "ftp://localhost/v1",
        "http://localhost:0/v1",
        "http://localhost/v1?key=secret",
    ] {
        p.base_url = url.into();
        assert!(ProviderEndpoint::parse(&p).is_err());
    }
    p = provider();
    p.endpoint_target = EndpointTarget::Windows;
    assert!(ProviderEndpoint::parse(&p)
        .unwrap()
        .routed_url(&RoutingContext::default())
        .is_err());
}
#[test]
fn persisted_snapshot_roundtrip_and_remove() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let s = ProviderStore::load(root.clone()).unwrap();
    s.upsert(0, provider()).unwrap();
    let restored = ProviderStore::load(root.clone()).unwrap();
    assert_eq!(restored.snapshot(), s.snapshot());
    assert!(restored.remove(0, "local").is_err());
    restored.remove(1, "local").unwrap();
    assert!(ProviderStore::load(root).unwrap().resolve("local").is_err());
}
#[test]
fn stale_credentials_and_connection_replies_are_ignored() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let ticket = s.discovery_ticket("local", 2, 3).unwrap();
    assert!(!s.apply_discovery(&ticket, 4, 3, vec![]).unwrap());
    assert!(!s.apply_discovery(&ticket, 2, 5, vec![]).unwrap());
    assert_eq!(s.snapshot().revision, 1);
}
#[test]
fn disk_failure_does_not_publish_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("providers");
    let s = ProviderStore::load(root.clone()).unwrap();
    std::fs::write(&root, b"blocking file").unwrap();
    assert!(s.upsert(0, provider()).is_err());
    assert_eq!(s.snapshot().revision, 0);
}
#[test]
fn namespace_routing_preserves_prefix() {
    let mut p = provider();
    p.base_url = "http://localhost:8000/api/v1".into();
    let e = ProviderEndpoint::parse(&p).unwrap();
    assert_eq!(
        e.routed_url(&RoutingContext::default()).unwrap(),
        "http://127.0.0.1:8000/api/v1"
    );
    p.endpoint_target = EndpointTarget::Windows;
    assert_eq!(
        ProviderEndpoint::parse(&p)
            .unwrap()
            .routed_url(&RoutingContext {
                windows_host: Some("172.20.0.1".parse().unwrap())
            })
            .unwrap(),
        "http://172.20.0.1:8000/api/v1"
    );
    p.endpoint_target = EndpointTarget::Remote;
    assert!(ProviderEndpoint::parse(&p)
        .unwrap()
        .routed_url(&RoutingContext::default())
        .is_err());
}
#[cfg(unix)]
#[tokio::test]
async fn companion_listener_rejects_workspace_and_stale_grants() {
    use bondsymphonic_daemon::net::{allowlist::Allowlist, proxy::ProxyRegistry};
    use bondsymphonic_daemon::server::broadcast::EventBus;
    use bondsymphonic_proto::WorkspaceId;
    let dir = tempfile::tempdir().unwrap();
    let registry = ProxyRegistry::default();
    let id = WorkspaceId("provider-test".into());
    let socket = dir.path().join("proxy.sock");
    let gen = registry
        .start(&id, &socket, Allowlist::default(), EventBus::new(16))
        .await
        .unwrap();
    assert!(registry
        .set_provider_endpoints(&id, gen, "opencode", vec![("127.0.0.1".into(), 8000)])
        .is_err());
    assert!(registry
        .start_opencode(&id, &socket, Allowlist::default(), EventBus::new(16))
        .await
        .is_err());
    registry.stop(&id);
    let gen = registry
        .start_opencode(&id, &socket, Allowlist::default(), EventBus::new(16))
        .await
        .unwrap();
    assert!(registry
        .set_provider_endpoints(&id, gen, "claude", vec![])
        .is_err());
    registry
        .set_provider_endpoints(&id, gen, "opencode", vec![("127.0.0.1".into(), 8000)])
        .unwrap();
    let next = registry
        .start_opencode(&id, &socket, Allowlist::default(), EventBus::new(16))
        .await
        .unwrap();
    assert!(registry
        .set_provider_endpoints(&id, gen, "opencode", vec![])
        .is_err());
    assert_ne!(gen, next);
    registry.stop(&id);
}
#[test]
fn independent_provider_discovery_tickets_both_apply() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let mut b = provider();
    b.id = "second".into();
    s.upsert(1, b.clone()).unwrap();
    let a_ticket = s.discovery_ticket("local", 1, 1).unwrap();
    let b_ticket = s.discovery_ticket("second", 1, 1).unwrap();
    let mut discovered = provider().models.remove(0);
    discovered.id = "discovered".into();
    assert!(s
        .apply_discovery(&a_ticket, 1, 1, vec![discovered])
        .unwrap());
    assert!(s.apply_discovery(&b_ticket, 1, 1, vec![]).unwrap());
    assert!(s
        .resolve("local")
        .unwrap()
        .models
        .iter()
        .any(|m| m.id == "discovered"));
    let a_ticket = s.discovery_ticket("local", 1, 1).unwrap();
    b.name = "Edited second".into();
    s.upsert(s.snapshot().revision, b).unwrap();
    assert!(s.apply_discovery(&a_ticket, 1, 1, vec![]).unwrap());
}
#[test]
fn same_provider_edit_and_remove_readd_invalidate_discovery() {
    let s = ProviderStore::default();
    s.upsert(0, provider()).unwrap();
    let old = s.discovery_ticket("local", 1, 1).unwrap();
    s.upsert(1, provider()).unwrap();
    assert!(!s.apply_discovery(&old, 1, 1, vec![]).unwrap());
    let removed = s.discovery_ticket("local", 1, 1).unwrap();
    s.remove(2, "local").unwrap();
    s.upsert(3, provider()).unwrap();
    assert!(!s.apply_discovery(&removed, 1, 1, vec![]).unwrap());
}
#[test]
fn failed_persistence_preserves_provider_discovery_generation() {
    let temp = tempfile::tempdir().unwrap();
    let s = ProviderStore::load(temp.path().to_path_buf()).unwrap();
    s.upsert(0, provider()).unwrap();
    let ticket = s.discovery_ticket("local", 1, 1).unwrap();
    let path = temp.path().join("opencode-providers.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(s.upsert(1, provider()).is_err());
    assert_eq!(s.snapshot().revision, 1);
    std::fs::remove_dir(&path).unwrap();
    assert!(s.apply_discovery(&ticket, 1, 1, vec![]).unwrap());
}
#[test]
fn mapped_loopback_obeys_remote_and_windows_namespaces() {
    let mut p = provider();
    p.base_url = "http://[::ffff:127.0.0.1]:8000/prefix/v1".into();
    p.endpoint_target = EndpointTarget::Remote;
    assert!(ProviderEndpoint::parse(&p)
        .unwrap()
        .routed_url(&RoutingContext::default())
        .is_err());
    p.endpoint_target = EndpointTarget::Windows;
    assert_eq!(
        ProviderEndpoint::parse(&p)
            .unwrap()
            .routed_url(&RoutingContext {
                windows_host: Some("172.20.0.1".parse().unwrap())
            })
            .unwrap(),
        "http://172.20.0.1:8000/prefix/v1"
    );
}

#[tokio::test]
async fn provider_edit_waits_until_pending_launch_finishes_granting() {
    let store = std::sync::Arc::new(ProviderStore::default());
    store.upsert(0, provider()).unwrap();
    let launch = store.lock_launch_edit().await;
    let edit_store = store.clone();
    let edit = tokio::spawn(async move {
        let _guard = edit_store.lock_launch_edit().await;
        edit_store.remove(1, "local").unwrap();
    });
    tokio::task::yield_now().await;
    assert!(store.resolve("local").is_ok());
    assert!(!edit.is_finished());
    drop(launch);
    edit.await.unwrap();
    assert!(store.resolve("local").is_err());
}

#[test]
fn discovery_context_binds_endpoint_and_generation_atomically() {
    let store = ProviderStore::default();
    store.upsert(0, provider()).unwrap();
    let (source, ticket) = store.discovery_context("local", 1, 1).unwrap();
    let mut replacement = provider();
    replacement.base_url = "http://localhost:8000/v1".into();
    store.upsert(1, replacement).unwrap();
    assert_eq!(source.base_url, provider().base_url);
    assert!(!store
        .apply_discovery(
            &ticket,
            1,
            1,
            vec![OpenCodeModel {
                id: "old-endpoint-model".into(),
                name: "Old".into(),
                context_limit: None,
                output_limit: None,
            }]
        )
        .unwrap());
    assert!(!store
        .resolve("local")
        .unwrap()
        .models
        .iter()
        .any(|m| m.id == "old-endpoint-model"));
}
