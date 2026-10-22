use async_trait::async_trait;
use bondsymphonic_daemon::mcp::{auth::*, transport::*, McpManager};
use bondsymphonic_proto::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
struct Peer {
    requests: Mutex<Vec<HttpRequest>>,
    refreshes: AtomicUsize,
    expiry: u64,
    gate: Option<Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl HttpClient for Peer {
    async fn execute(&self, r: HttpRequest) -> Result<HttpResponse, RpcError> {
        self.requests.lock().unwrap().push(r.clone());
        let (status, headers, body) = if r.url == "https://example.com/mcp" {
            (401,vec![("WWW-Authenticate".into(),"Bearer resource_metadata=\"https://example.com/metadata\", error=\"insufficient_scope\", scope=\"extra\"".into())],json!({}))
        } else if r.url.ends_with("/metadata") {
            (
                200,
                vec![],
                json!({"resource":"https://example.com/mcp","authorization_servers":["https://auth.example.com"]}),
            )
        } else if r.url.contains(".well-known") {
            (
                200,
                vec![],
                json!({"issuer":"https://auth.example.com","authorization_endpoint":"https://auth.example.com/authorize","token_endpoint":"https://auth.example.com/token","code_challenge_methods_supported":["S256"],"client_id_metadata_document_supported":true}),
            )
        } else if r.url.ends_with("/token") {
            let refresh = String::from_utf8_lossy(&r.body).contains("grant_type=refresh_token");
            if refresh {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            if let Some(g) = &self.gate {
                g.notified().await;
            }
            (
                200,
                vec![],
                json!({"access_token":if refresh {"REFRESH_SENTINEL"}else{"ACCESS_SENTINEL"},"refresh_token":"REFRESH_SECRET_SENTINEL","token_type":"Bearer","scope":"read extra","expires_in":if refresh{3600}else{self.expiry}}),
            )
        } else {
            panic!("unexpected request")
        };
        Ok(HttpResponse {
            status,
            headers,
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
}
fn setup(
    expiry: u64,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (
    tempfile::TempDir,
    Arc<McpManager>,
    Arc<Peer>,
    Arc<RemoteMcpManager>,
) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(McpManager::load(dir.path().into()).unwrap());
    registry
        .upsert(McpUpsertParams {
            server: McpServerConfig {
                id: "remote".into(),
                name: "Remote".into(),
                transport: McpTransportConfig::Http {
                    url: "https://example.com/mcp".into(),
                    auth: McpAuthConfig::OAuth {
                        client_id: Some("test-client".into()),
                        client_metadata_url: None,
                        client_secret: None,
                        scopes: vec!["read".into()],
                    },
                },
                enabled: true,
                default_workspace_enabled: true,
                enabled_backends: vec!["codex".into()],
            },
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    let peer = Arc::new(Peer {
        requests: Mutex::new(vec![]),
        refreshes: AtomicUsize::new(0),
        expiry,
        gate,
    });
    let manager = Arc::new(RemoteMcpManager::with_client(
        registry.clone(),
        peer.clone(),
    ));
    (dir, registry, peer, manager)
}
async fn login_begin(manager: &RemoteMcpManager) -> McpCallbackParams {
    let result = manager
        .connect(McpConnectParams {
            server_id: "remote".into(),
            redirect_uri: "http://127.0.0.1:49152/callback".into(),
        })
        .await
        .unwrap();
    let state = result
        .authorization_url
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    McpCallbackParams {
        server_id: "remote".into(),
        state: McpSecretValue(state.into()),
        code: McpSecretValue("CODE_SENTINEL".into()),
        issuer: Some("https://auth.example.com".into()),
    }
}
#[tokio::test]
async fn oauth_wrong_state_rejected() {
    let (_d, _r, p, m) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.state.0 = "wrong".into();
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}
#[tokio::test]
async fn oauth_wrong_issuer_rejected() {
    let (_d, _r, p, m) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.issuer = Some("https://auth.example.com/".into());
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}
#[tokio::test]
async fn refresh_single_flight() {
    let (_d, _r, p, m) = setup(0, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let id = McpServerId::from("remote");
    let (a, b) = tokio::join!(m.headers(&id), m.headers(&id));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(p.refreshes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn disconnect_invalidates_pending_exchange() {
    let (_d, _r, p, m) = setup(3600, Some(Arc::new(tokio::sync::Notify::new())));
    let callback = login_begin(&m).await;
    let other = m.clone();
    let task = tokio::spawn(async move { other.callback(callback).await });
    while !p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token"))
    {
        tokio::task::yield_now().await;
    }
    m.disconnect(&"remote".into()).unwrap();
    p.gate.as_ref().unwrap().notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(m.headers(&"remote".into()).await.is_err());
}
#[tokio::test]
async fn step_up_scope_challenge() {
    let (d, r, p, m) = setup(3600, None);
    let callback = login_begin(&m).await;
    let state = callback.state.0.clone();
    m.callback(callback).await.unwrap();
    {
        let requests = p.requests.lock().unwrap();
        let token = requests.iter().find(|r| r.url.ends_with("/token")).unwrap();
        let body = String::from_utf8(token.body.clone()).unwrap();
        assert!(body.contains("resource=https%3A%2F%2Fexample.com%2Fmcp"));
        assert!(body.contains("scope=read%20extra"));
        assert!(body.contains("code_verifier="));
        let debug = format!("{token:?}");
        assert!(!debug.contains("SENTINEL"));
        assert!(!debug.contains(&state));
    }
    let reopened = RemoteMcpManager::with_client(r, p);
    assert_eq!(
        reopened.headers(&"remote".into()).await.unwrap()[0].1,
        "Bearer ACCESS_SENTINEL"
    );
    assert!(!r_export(&d).contains("SENTINEL"));
}
fn r_export(d: &tempfile::TempDir) -> String {
    McpManager::load(d.path().into()).unwrap().export().unwrap()
}

struct RevocationPeer {
    inner: Arc<Peer>,
    endpoint: Option<String>,
    methods: Option<Vec<String>>,
    hold: bool,
    status: u16,
    access_only: bool,
}
#[async_trait]
impl HttpClient for RevocationPeer {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, RpcError> {
        if request.url.ends_with("/revoke") {
            self.inner.requests.lock().unwrap().push(request);
            if self.hold {
                std::future::pending::<()>().await;
            }
            return Ok(HttpResponse {
                status: self.status,
                headers: vec![],
                body: b"REVOKE_ERROR_SENTINEL".to_vec(),
            });
        }
        let metadata = request.url.contains(".well-known");
        let token = request.url.ends_with("/token");
        let mut response = self.inner.execute(request).await?;
        if metadata || (token && self.access_only) {
            let mut body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            if metadata {
                if let Some(endpoint) = &self.endpoint {
                    body["revocation_endpoint"] = json!(endpoint);
                    if let Some(methods) = &self.methods {
                        body["revocation_endpoint_auth_methods_supported"] = json!(methods);
                    }
                }
            } else {
                body.as_object_mut().unwrap().remove("refresh_token");
            }
            response.body = serde_json::to_vec(&body).unwrap();
        }
        Ok(response)
    }
}
fn revocation_peer(inner: Arc<Peer>, endpoint: Option<&str>) -> Arc<RevocationPeer> {
    Arc::new(RevocationPeer {
        inner,
        endpoint: endpoint.map(str::to_owned),
        methods: Some(vec!["none".into()]),
        hold: false,
        status: 200,
        access_only: false,
    })
}
#[tokio::test]
async fn disconnect_revokes_persisted_refresh_and_access_tokens() {
    let (_dir, registry, inner, _) = setup(0, None);
    let peer = revocation_peer(inner.clone(), Some("https://auth.example.com/revoke"));
    let manager = RemoteMcpManager::with_client(registry.clone(), peer.clone());
    manager.callback(login_begin(&manager).await).await.unwrap();
    manager.headers(&"remote".into()).await.unwrap(); // Metadata must survive refresh, too.
    let manager = RemoteMcpManager::with_client(registry.clone(), peer.clone());
    manager
        .disconnect_and_revoke(&"remote".into())
        .await
        .unwrap();
    let requests = inner.requests.lock().unwrap();
    let revocations: Vec<_> = requests
        .iter()
        .filter(|r| r.url.ends_with("/revoke"))
        .collect();
    assert_eq!(revocations.len(), 2);
    assert_eq!(
        String::from_utf8_lossy(&revocations[0].body),
        "token=REFRESH_SECRET_SENTINEL&token_type_hint=refresh_token&client_id=test-client"
    );
    assert_eq!(
        String::from_utf8_lossy(&revocations[1].body),
        "token=REFRESH_SENTINEL&token_type_hint=access_token&client_id=test-client"
    );
    assert!(revocations.iter().all(|r| r.method == "POST"
        && r.headers
            .iter()
            .any(|(k, v)| k == "Content-Type" && v == "application/x-www-form-urlencoded")));
    assert!(!format!("{revocations:?}").contains("SENTINEL"));
    assert!(!RemoteMcpManager::with_client(registry, peer).has_credentials(&"remote".into()));
}
#[tokio::test]
async fn disconnect_skips_absent_unsafe_and_unsupported_revocation() {
    for endpoint in [
        None,
        Some("http://auth.example.com/revoke"),
        Some("https://127.0.0.1/revoke"),
        Some("https://auth.example.com/revoke"),
    ] {
        let (_dir, registry, inner, _) = setup(3600, None);
        let mut peer = revocation_peer(inner.clone(), endpoint);
        if endpoint == Some("https://auth.example.com/revoke") {
            Arc::get_mut(&mut peer).unwrap().methods = Some(vec!["private_key_jwt".into()]);
        }
        let manager = RemoteMcpManager::with_client(registry, peer);
        manager.callback(login_begin(&manager).await).await.unwrap();
        manager
            .disconnect_and_revoke(&"remote".into())
            .await
            .unwrap();
        assert!(!manager.has_credentials(&"remote".into()));
        assert!(!inner
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.url.ends_with("/revoke")));
    }
}
#[tokio::test(start_paused = true)]
async fn revocation_timeout_preserves_immediate_durable_disconnect() {
    let (_dir, registry, inner, _) = setup(3600, None);
    let mut peer = revocation_peer(inner.clone(), Some("https://auth.example.com/revoke"));
    Arc::get_mut(&mut peer).unwrap().hold = true;
    let manager = Arc::new(RemoteMcpManager::with_client(
        registry.clone(),
        peer.clone(),
    ));
    manager.callback(login_begin(&manager).await).await.unwrap();
    let old_generation = manager.auth_generation(&"remote".into());
    let other = manager.clone();
    let task = tokio::spawn(async move { other.disconnect_and_revoke(&"remote".into()).await });
    while !inner
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/revoke"))
    {
        tokio::task::yield_now().await;
    }
    assert!(!task.is_finished());
    assert!(manager.auth_generation(&"remote".into()) > old_generation);
    assert!(!manager.has_credentials(&"remote".into()));
    assert!(!RemoteMcpManager::with_client(registry, peer).has_credentials(&"remote".into()));
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    assert_eq!(
        task.await.unwrap().unwrap().state,
        McpConnectionState::Disconnected
    );
    assert_eq!(
        inner
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.ends_with("/revoke"))
            .count(),
        1
    );
}
#[tokio::test]
async fn failed_access_only_revocation_does_not_restore_credentials() {
    let (_dir, registry, inner, _) = setup(3600, None);
    let mut peer = revocation_peer(inner.clone(), Some("https://auth.example.com/revoke"));
    let settings = Arc::get_mut(&mut peer).unwrap();
    settings.status = 503;
    settings.access_only = true;
    let manager = RemoteMcpManager::with_client(registry.clone(), peer.clone());
    manager.callback(login_begin(&manager).await).await.unwrap();
    let result = manager
        .disconnect_and_revoke(&"remote".into())
        .await
        .unwrap();
    assert_eq!(result.state, McpConnectionState::Disconnected);
    assert!(!format!("{result:?}").contains("SENTINEL"));
    assert!(!RemoteMcpManager::with_client(registry, peer).has_credentials(&"remote".into()));
    let requests = inner.requests.lock().unwrap();
    let revoke = requests
        .iter()
        .find(|r| r.url.ends_with("/revoke"))
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&revoke.body),
        "token=ACCESS_SENTINEL&token_type_hint=access_token&client_id=test-client"
    );
}
#[tokio::test]
async fn revocation_uses_advertised_confidential_client_auth() {
    for method in ["client_secret_post", "client_secret_basic", "default"] {
        let (_dir, registry, inner, _) = setup(3600, None);
        let mut server = registry.server(&"remote".into()).unwrap();
        if let McpTransportConfig::Http {
            auth: McpAuthConfig::OAuth { client_secret, .. },
            ..
        } = &mut server.transport
        {
            *client_secret = Some("revocation-client-secret".into());
        }
        registry
            .upsert(McpUpsertParams {
                server,
                secrets: vec![McpSecretWrite::Set {
                    reference: "revocation-client-secret".into(),
                    value: McpSecretValue("CLIENT_SECRET_SENTINEL".into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        let mut peer = revocation_peer(inner.clone(), Some("https://auth.example.com/revoke"));
        Arc::get_mut(&mut peer).unwrap().methods =
            (method != "default").then(|| vec![method.into()]);
        let manager = RemoteMcpManager::with_client(registry, peer);
        manager.callback(login_begin(&manager).await).await.unwrap();
        manager
            .disconnect_and_revoke(&"remote".into())
            .await
            .unwrap();
        let requests = inner.requests.lock().unwrap();
        let revoke = requests
            .iter()
            .find(|r| r.url.ends_with("/revoke"))
            .unwrap();
        let body = String::from_utf8_lossy(&revoke.body);
        if method == "client_secret_post" {
            assert!(body.ends_with("&client_secret=CLIENT_SECRET_SENTINEL"));
            assert!(!revoke.headers.iter().any(|(k, _)| k == "Authorization"));
        } else {
            use base64::Engine;
            assert!(!body.contains("CLIENT_SECRET_SENTINEL"));
            assert_eq!(
                revoke
                    .headers
                    .iter()
                    .find(|(k, _)| k == "Authorization")
                    .unwrap()
                    .1,
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode("test-client:CLIENT_SECRET_SENTINEL")
                )
            );
        }
        assert!(!format!("{revoke:?}").contains("SENTINEL"));
    }
}

#[tokio::test]
async fn stale_revocation_never_sends_replacement_client_secret_to_old_issuer() {
    let (_dir, registry, inner, _) = setup(3600, None);
    let mut server = registry.server(&"remote".into()).unwrap();
    if let McpTransportConfig::Http {
        auth: McpAuthConfig::OAuth { client_secret, .. },
        ..
    } = &mut server.transport
    {
        *client_secret = Some("shared-client-secret-ref".into());
    }
    registry
        .upsert(McpUpsertParams {
            server: server.clone(),
            secrets: vec![McpSecretWrite::Set {
                reference: "shared-client-secret-ref".into(),
                value: McpSecretValue("OLD_CLIENT_SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    let mut peer = revocation_peer(inner.clone(), Some("https://auth.example.com/revoke"));
    Arc::get_mut(&mut peer).unwrap().methods = Some(vec!["client_secret_post".into()]);
    let manager = RemoteMcpManager::with_client(registry.clone(), peer);
    manager.callback(login_begin(&manager).await).await.unwrap();
    if let McpTransportConfig::Http { url, .. } = &mut server.transport {
        *url = "https://replacement.example.com/mcp".into();
    }
    registry
        .upsert(McpUpsertParams {
            server,
            secrets: vec![McpSecretWrite::Set {
                reference: "shared-client-secret-ref".into(),
                value: McpSecretValue("NEW_CLIENT_SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    manager
        .disconnect_and_revoke(&"remote".into())
        .await
        .unwrap();
    assert!(!manager.has_credentials(&"remote".into()));
    let requests = inner.requests.lock().unwrap();
    assert!(
        !requests.iter().any(|r| r.url.ends_with("/revoke")),
        "a stale account must never resolve replacement credentials for its old issuer"
    );
    assert!(!requests
        .iter()
        .any(|r| String::from_utf8_lossy(&r.body).contains("NEW_CLIENT_SECRET_SENTINEL")));
}
#[tokio::test]
async fn unrelated_registry_edit_keeps_login() {
    let (_d, r, _p, m) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    r.set_override(
        McpOverrideParams {
            server_id: "remote".into(),
            workspace_id: WorkspaceId("ws_test".into()),
            value: McpWorkspaceOverride::Disabled,
        },
        None,
    )
    .unwrap();
    assert!(m.headers(&"remote".into()).await.is_ok());
}
#[test]
fn oauth_state_is_opaque() {
    assert_ne!(random_secret(), random_secret());
    assert!(random_secret().len() >= 43);
}

#[tokio::test]
async fn issuer_optional_when_not_advertised() {
    let (_d, _r, _p, m) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.issuer = None;
    assert!(m.callback(callback).await.is_ok());
}

#[tokio::test]
async fn server_edit_invalidates_pending_callback() {
    let (_d, r, p, m) = setup(3600, None);
    let callback = login_begin(&m).await;
    let mut config = r.server(&"remote".into()).unwrap();
    config.name = "Changed".into();
    r.upsert(McpUpsertParams {
        server: config,
        secrets: vec![],
        expected_revision: None,
    })
    .unwrap();
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}

#[tokio::test]
async fn callback_is_single_use() {
    let (_d, _r, p, m) = setup(3600, None);
    let callback = login_begin(&m).await;
    m.callback(callback.clone()).await.unwrap();
    assert!(m.callback(callback).await.is_err());
    assert_eq!(
        p.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.ends_with("/token"))
            .count(),
        1
    );
}

struct MetadataPeer {
    peer: Arc<Peer>,
    mode: &'static str,
}
#[async_trait]
impl HttpClient for MetadataPeer {
    async fn execute(&self, r: HttpRequest) -> Result<HttpResponse, RpcError> {
        if self.mode == "root" {
            if r.url == "https://example.com/mcp" {
                return Ok(HttpResponse {
                    status: 401,
                    headers: vec![],
                    body: vec![],
                });
            }
            if r.url.ends_with("oauth-protected-resource/mcp") {
                return Ok(HttpResponse {
                    status: 404,
                    headers: vec![],
                    body: vec![],
                });
            }
            if r.url.ends_with("oauth-protected-resource") {
                return self
                    .peer
                    .execute(HttpRequest {
                        url: "https://example.com/metadata".into(),
                        ..r
                    })
                    .await;
            }
        }
        let metadata = r.url.contains(".well-known");
        let mut response = self.peer.execute(r).await?;
        if metadata {
            let mut body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            if self.mode == "issuer" {
                body["authorization_response_iss_parameter_supported"] = json!(true);
            }
            if self.mode == "no_pkce" {
                body["code_challenge_methods_supported"] = json!(["plain"]);
            }
            response.body = serde_json::to_vec(&body).unwrap();
        }
        Ok(response)
    }
}

#[tokio::test]
async fn advertised_issuer_requires_callback_parameter() {
    let (_d, r, p, _m) = setup(3600, None);
    let m = RemoteMcpManager::with_client(
        r,
        Arc::new(MetadataPeer {
            peer: p.clone(),
            mode: "issuer",
        }),
    );
    let mut callback = login_begin(&m).await;
    callback.issuer = None;
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}

#[tokio::test]
async fn protected_metadata_root_fallback() {
    let (_d, r, p, _m) = setup(3600, None);
    let m = RemoteMcpManager::with_client(
        r,
        Arc::new(MetadataPeer {
            peer: p,
            mode: "root",
        }),
    );
    assert!(m
        .connect(McpConnectParams {
            server_id: "remote".into(),
            redirect_uri: "http://127.0.0.1:49152/callback".into()
        })
        .await
        .is_ok());
}

#[tokio::test]
async fn pkce_s256_is_required_before_browser_login() {
    let (_d, r, p, _m) = setup(3600, None);
    let m = RemoteMcpManager::with_client(
        r,
        Arc::new(MetadataPeer {
            peer: p,
            mode: "no_pkce",
        }),
    );
    assert!(m
        .connect(McpConnectParams {
            server_id: "remote".into(),
            redirect_uri: "http://127.0.0.1:49152/callback".into()
        })
        .await
        .is_err());
}

struct RefreshGate {
    peer: Arc<Peer>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl HttpClient for RefreshGate {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, RpcError> {
        if String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token") {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.peer.execute(request).await
    }
}

#[tokio::test]
async fn disconnect_invalidates_pending_refresh_and_persisted_credentials() {
    let (_d, r, p, m) = setup(0, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let peer = Arc::new(RefreshGate {
        peer: p.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let m = Arc::new(RemoteMcpManager::with_client(r.clone(), peer.clone()));
    let other = m.clone();
    let task = tokio::spawn(async move { other.headers(&"remote".into()).await });
    peer.entered.notified().await;
    m.disconnect(&"remote".into()).unwrap();
    peer.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(RemoteMcpManager::with_client(r, p)
        .headers(&"remote".into())
        .await
        .is_err());
}

#[tokio::test]
async fn edit_and_restore_does_not_revive_pending_login() {
    let (_d, r, p, m) = setup(3600, None);
    let callback = login_begin(&m).await;
    let original = r.server(&"remote".into()).unwrap();
    let mut changed = original.clone();
    changed.name = "Changed".into();
    for server in [changed, original] {
        r.upsert(McpUpsertParams {
            server,
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    }
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}

#[tokio::test]
async fn secret_only_edit_invalidates_pending_login() {
    let (_d, r, p, m) = setup(3600, None);
    let mut server = r.server(&"remote".into()).unwrap();
    if let McpTransportConfig::Http {
        auth: McpAuthConfig::OAuth { client_secret, .. },
        ..
    } = &mut server.transport
    {
        *client_secret = Some("oauth-client-secret".into());
    }
    r.upsert(McpUpsertParams {
        server: server.clone(),
        secrets: vec![McpSecretWrite::Set {
            reference: "oauth-client-secret".into(),
            value: McpSecretValue("INITIAL_SENTINEL".into()),
        }],
        expected_revision: None,
    })
    .unwrap();
    let callback = login_begin(&m).await;
    r.upsert(McpUpsertParams {
        server,
        secrets: vec![McpSecretWrite::Set {
            reference: "oauth-client-secret".into(),
            value: McpSecretValue("REPLACEMENT_SENTINEL".into()),
        }],
        expected_revision: None,
    })
    .unwrap();
    assert!(m.callback(callback).await.is_err());
    assert!(!p
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.url.ends_with("/token")));
}

#[tokio::test]
async fn reading_stale_token_does_not_cancel_new_login() {
    let (_d, _r, _p, m) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let callback = login_begin(&m).await;
    assert!(m.headers(&"remote".into()).await.is_err());
    m.callback(callback).await.unwrap();
}

#[tokio::test]
async fn removed_server_can_erase_oauth_credentials() {
    let (d, r, _p, m) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    r.remove(&"remote".into(), None).unwrap();
    m.disconnect(&"remote".into()).unwrap();
    assert!(!std::fs::read_to_string(d.path().join("mcp/oauth.json"))
        .unwrap()
        .contains("SENTINEL"));
}

#[tokio::test]
async fn registry_generations_reject_stale_persisted_accounts() {
    for secret_edit in [false, true] {
        let (d, r, p, m) = setup(3600, None);
        let mut original = r.server(&"remote".into()).unwrap();
        if secret_edit {
            if let McpTransportConfig::Http {
                auth: McpAuthConfig::OAuth { client_secret, .. },
                ..
            } = &mut original.transport
            {
                *client_secret = Some("client-secret".into());
            }
            r.upsert(McpUpsertParams {
                server: original.clone(),
                secrets: vec![McpSecretWrite::Set {
                    reference: "client-secret".into(),
                    value: McpSecretValue("OLD_SENTINEL".into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        }
        m.callback(login_begin(&m).await).await.unwrap();
        if secret_edit {
            r.upsert(McpUpsertParams {
                server: original,
                secrets: vec![McpSecretWrite::Set {
                    reference: "client-secret".into(),
                    value: McpSecretValue("NEW_SENTINEL".into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        } else {
            r.remove(&"remote".into(), None).unwrap();
            r.upsert(McpUpsertParams {
                server: original,
                secrets: vec![],
                expected_revision: None,
            })
            .unwrap();
        }
        let reopened = Arc::new(McpManager::load(d.path().into()).unwrap());
        assert!(RemoteMcpManager::with_client(reopened, p)
            .headers(&"remote".into())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn auth_watch_and_generation_cover_login_and_disconnect() {
    let (_d, _r, _p, m) = setup(3600, None);
    let mut changes = m.subscribe_auth_changes();
    let before = m.auth_generation(&"remote".into());
    let callback = login_begin(&m).await;
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert!(m.auth_generation(&"remote".into()) > before);
    m.callback(callback).await.unwrap();
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    let connected = m.auth_generation(&"remote".into());
    m.disconnect(&"remote".into()).unwrap();
    assert!(changes.has_changed().unwrap());
    assert!(m.auth_generation(&"remote".into()) > connected);
}

#[tokio::test]
async fn beginning_account_replacement_cannot_restore_old_account_on_restart() {
    let (d, r, p, m) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let _replacement = login_begin(&m).await;
    // Do not read headers: replacement itself must persist invalidation.
    let reopened = Arc::new(McpManager::load(d.path().into()).unwrap());
    assert!(RemoteMcpManager::with_client(reopened, p)
        .headers(&"remote".into())
        .await
        .is_err());
    assert!(!std::fs::read_to_string(d.path().join("mcp/oauth.json"))
        .unwrap()
        .contains("ACCESS_SENTINEL"));
    assert!(r.server(&"remote".into()).is_ok());
}
