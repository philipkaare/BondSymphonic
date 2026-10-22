//! Generic MCP OAuth: discovery, PKCE login, callback validation, refresh,
//! disconnect and revocation, against a fake authorization server. Tokens are
//! sentinels that must never leave the private secrets file.
use async_trait::async_trait;
use bondsymphonic_daemon::mcp::servers::{auth::random_secret, transport::*, McpServers};
use bondsymphonic_proto::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

struct Peer {
    requests: Mutex<Vec<HttpRequest>>,
    refreshes: AtomicUsize,
    expiry: u64,
    gate: Option<Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl HttpClient for Peer {
    async fn send(&self, r: HttpRequest, _: CancellationToken) -> Result<HttpResponse, RpcError> {
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

fn oauth_server() -> McpServerConfig {
    McpServerConfig {
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
    }
}

fn reopen(dir: &tempfile::TempDir, http: Arc<dyn HttpClient>) -> Arc<McpServers> {
    Arc::new(McpServers::load(dir.path()).unwrap().with_http(http))
}

/// The registry over `wrap(peer)`, holding the OAuth server `remote`.
fn setup_with(
    expiry: u64,
    gate: Option<Arc<tokio::sync::Notify>>,
    wrap: impl FnOnce(Arc<Peer>) -> Arc<dyn HttpClient>,
) -> (tempfile::TempDir, Arc<McpServers>, Arc<Peer>) {
    let dir = tempfile::tempdir().unwrap();
    let peer = Arc::new(Peer {
        requests: Mutex::new(vec![]),
        refreshes: AtomicUsize::new(0),
        expiry,
        gate,
    });
    let servers = reopen(&dir, wrap(peer.clone()));
    servers
        .upsert(McpUpsertParams {
            server: oauth_server(),
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    (dir, servers, peer)
}

fn setup(
    expiry: u64,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (tempfile::TempDir, Arc<McpServers>, Arc<Peer>) {
    setup_with(expiry, gate, |p| p)
}

fn id() -> McpServerId {
    "remote".into()
}

fn server(r: &McpServers) -> McpServerConfig {
    r.snapshot()
        .servers
        .into_iter()
        .find(|s| s.id == id())
        .unwrap()
}

fn upsert(r: &McpServers, server: McpServerConfig, secrets: Vec<McpSecretWrite>) {
    r.upsert(McpUpsertParams {
        server,
        secrets,
        expected_revision: None,
    })
    .unwrap();
}

fn token_requests(p: &Peer) -> usize {
    p.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.url.ends_with("/token"))
        .count()
}

/// Everything the registry keeps on disk, as text.
fn files(d: &tempfile::TempDir) -> String {
    let mut all = String::new();
    for entry in std::fs::read_dir(d.path().join("mcp")).unwrap() {
        all.push_str(&String::from_utf8_lossy(
            &std::fs::read(entry.unwrap().path()).unwrap(),
        ));
    }
    all
}

async fn login_begin(manager: &Arc<McpServers>) -> McpCallbackParams {
    let result = manager
        .connect(McpConnectParams {
            server_id: id(),
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
        server_id: id(),
        state: McpSecretValue(state.into()),
        code: McpSecretValue("CODE_SENTINEL".into()),
        issuer: Some("https://auth.example.com".into()),
    }
}

#[tokio::test]
async fn oauth_wrong_state_rejected() {
    let (_d, m, p) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.state.0 = "wrong".into();
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test]
async fn oauth_wrong_issuer_rejected() {
    let (_d, m, p) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.issuer = Some("https://auth.example.com/".into());
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test(start_paused = true)]
async fn expired_attempt_rejected() {
    let (_d, m, p) = setup(3600, None);
    let early = login_begin(&m).await;
    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    assert_eq!(m.status(&id()).state, McpConnectionState::Connecting);
    m.callback(early).await.unwrap();
    assert_eq!(token_requests(&p), 1);

    let late = login_begin(&m).await;
    tokio::time::advance(std::time::Duration::from_secs(601)).await;
    assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
    assert!(m.callback(late).await.is_err());
    assert_eq!(token_requests(&p), 1);
}

#[tokio::test]
async fn login_status_and_list_never_carry_tokens() {
    let (_d, m, _p) = setup(3600, None);
    assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
    let callback = login_begin(&m).await;
    assert_eq!(m.status(&id()).state, McpConnectionState::Connecting);
    let status = m.callback(callback).await.unwrap();
    assert_eq!(status.state, McpConnectionState::Connected);
    let listed = serde_json::to_string(&m.list().unwrap()).unwrap();
    assert!(listed.contains("connected") && !listed.contains("SENTINEL"));
    assert!(!m.export().contains("SENTINEL"));
    let status = m.disconnect(&id()).await.unwrap();
    assert_eq!(status.state, McpConnectionState::Disconnected);
}

#[tokio::test]
async fn refresh_single_flight() {
    let (_d, m, p) = setup(0, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let id = id();
    let (a, b) = tokio::join!(m.auth_headers(&id), m.auth_headers(&id));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(p.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disconnect_invalidates_pending_exchange() {
    let (_d, m, p) = setup(3600, Some(Arc::new(tokio::sync::Notify::new())));
    let callback = login_begin(&m).await;
    let other = m.clone();
    let task = tokio::spawn(async move { other.callback(callback).await });
    while token_requests(&p) == 0 {
        tokio::task::yield_now().await;
    }
    m.disconnect(&id()).await.unwrap();
    p.gate.as_ref().unwrap().notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(m.auth_headers(&id()).await.is_err());
    assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
}

#[tokio::test]
async fn step_up_scope_challenge() {
    let (d, m, p) = setup(3600, None);
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
    let reopened = reopen(&d, p);
    assert_eq!(
        reopened.auth_headers(&id()).await.unwrap()[0].1,
        "Bearer ACCESS_SENTINEL"
    );
    assert!(!reopened.export().contains("SENTINEL"));
}

/// Wraps [`Peer`] with an advertised revocation endpoint.
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
    async fn send(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
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
        let mut response = self.inner.send(request, cancel).await?;
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

fn revocation(
    endpoint: Option<&str>,
    configure: impl FnOnce(&mut RevocationPeer),
) -> impl FnOnce(Arc<Peer>) -> Arc<dyn HttpClient> {
    let endpoint = endpoint.map(str::to_owned);
    move |inner| {
        let mut peer = RevocationPeer {
            inner,
            endpoint,
            methods: Some(vec!["none".into()]),
            hold: false,
            status: 200,
            access_only: false,
        };
        configure(&mut peer);
        Arc::new(peer)
    }
}

fn revocations(p: &Peer) -> Vec<HttpRequest> {
    p.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.url.ends_with("/revoke"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn disconnect_revokes_persisted_refresh_and_access_tokens() {
    let wrap = revocation(Some("https://auth.example.com/revoke"), |_| {});
    let (dir, m, inner) = setup_with(0, None, wrap);
    m.callback(login_begin(&m).await).await.unwrap();
    m.auth_headers(&id()).await.unwrap(); // Metadata must survive refresh, too.
    let http = revocation(Some("https://auth.example.com/revoke"), |_| {})(inner.clone());
    let m = reopen(&dir, http);
    m.disconnect(&id()).await.unwrap();
    let revocations = revocations(&inner);
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
    assert!(!files(&dir).contains("SENTINEL"));
}

#[tokio::test]
async fn disconnect_skips_absent_unsafe_and_unsupported_revocation() {
    for endpoint in [
        None,
        Some("http://auth.example.com/revoke"),
        Some("https://127.0.0.1/revoke"),
        Some("https://auth.example.com/revoke"),
    ] {
        let unsupported = endpoint == Some("https://auth.example.com/revoke");
        let wrap = revocation(endpoint, |peer| {
            if unsupported {
                peer.methods = Some(vec!["private_key_jwt".into()]);
            }
        });
        let (dir, m, inner) = setup_with(3600, None, wrap);
        m.callback(login_begin(&m).await).await.unwrap();
        m.disconnect(&id()).await.unwrap();
        assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
        assert!(!files(&dir).contains("SENTINEL"));
        assert!(revocations(&inner).is_empty(), "{endpoint:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn revocation_timeout_preserves_immediate_durable_disconnect() {
    let wrap = revocation(Some("https://auth.example.com/revoke"), |p| p.hold = true);
    let (dir, m, inner) = setup_with(3600, None, wrap);
    m.callback(login_begin(&m).await).await.unwrap();
    let other = m.clone();
    let task = tokio::spawn(async move { other.disconnect(&id()).await });
    while revocations(&inner).is_empty() {
        tokio::task::yield_now().await;
    }
    assert!(!task.is_finished());
    assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
    assert!(m.auth_headers(&id()).await.is_err());
    assert!(!files(&dir).contains("ACCESS_SENTINEL"));
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    assert_eq!(
        task.await.unwrap().unwrap().state,
        McpConnectionState::Disconnected
    );
    assert_eq!(revocations(&inner).len(), 1);
}

#[tokio::test]
async fn failed_access_only_revocation_does_not_restore_credentials() {
    let wrap = revocation(Some("https://auth.example.com/revoke"), |p| {
        p.status = 503;
        p.access_only = true;
    });
    let (dir, m, inner) = setup_with(3600, None, wrap);
    m.callback(login_begin(&m).await).await.unwrap();
    let result = m.disconnect(&id()).await.unwrap();
    assert_eq!(result.state, McpConnectionState::Disconnected);
    assert!(!format!("{result:?}").contains("SENTINEL"));
    assert!(!files(&dir).contains("ACCESS_SENTINEL"));
    let revoke = &revocations(&inner)[0];
    assert_eq!(
        String::from_utf8_lossy(&revoke.body),
        "token=ACCESS_SENTINEL&token_type_hint=access_token&client_id=test-client"
    );
}

fn with_client_secret(m: &McpServers, reference: &str, value: &str) -> McpServerConfig {
    let mut server = server(m);
    if let McpTransportConfig::Http {
        auth: McpAuthConfig::OAuth { client_secret, .. },
        ..
    } = &mut server.transport
    {
        *client_secret = Some(reference.into());
    }
    upsert(
        m,
        server.clone(),
        vec![McpSecretWrite::Set {
            reference: reference.into(),
            value: McpSecretValue(value.into()),
        }],
    );
    server
}

#[tokio::test]
async fn revocation_uses_advertised_confidential_client_auth() {
    for method in ["client_secret_post", "client_secret_basic", "default"] {
        let wrap = revocation(Some("https://auth.example.com/revoke"), |p| {
            p.methods = (method != "default").then(|| vec![method.into()]);
        });
        let (_dir, m, inner) = setup_with(3600, None, wrap);
        with_client_secret(&m, "revocation-client-secret", "CLIENT_SECRET_SENTINEL");
        m.callback(login_begin(&m).await).await.unwrap();
        m.disconnect(&id()).await.unwrap();
        let revoke = revocations(&inner)[0].clone();
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
    let wrap = revocation(Some("https://auth.example.com/revoke"), |p| {
        p.methods = Some(vec!["client_secret_post".into()]);
    });
    let (_dir, m, inner) = setup_with(3600, None, wrap);
    let mut server =
        with_client_secret(&m, "shared-client-secret-ref", "OLD_CLIENT_SECRET_SENTINEL");
    m.callback(login_begin(&m).await).await.unwrap();
    if let McpTransportConfig::Http { url, .. } = &mut server.transport {
        *url = "https://replacement.example.com/mcp".into();
    }
    upsert(
        &m,
        server,
        vec![McpSecretWrite::Set {
            reference: "shared-client-secret-ref".into(),
            value: McpSecretValue("NEW_CLIENT_SECRET_SENTINEL".into()),
        }],
    );
    m.disconnect(&id()).await.unwrap();
    assert_eq!(m.status(&id()).state, McpConnectionState::Disconnected);
    assert!(
        revocations(&inner).is_empty(),
        "a stale account must never resolve replacement credentials for its old issuer"
    );
    assert!(!inner
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| String::from_utf8_lossy(&r.body).contains("NEW_CLIENT_SECRET_SENTINEL")));
}

#[tokio::test]
async fn unrelated_registry_edit_keeps_login() {
    let (_d, m, _p) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    m.set_override(McpOverrideParams {
        server_id: id(),
        workspace_id: WorkspaceId("ws_test".into()),
        value: McpWorkspaceOverride::Disabled,
    })
    .unwrap();
    let mut renamed = server(&m);
    renamed.name = "Renamed".into();
    upsert(&m, renamed, vec![]);
    assert!(m.auth_headers(&id()).await.is_ok());
    assert_eq!(m.status(&id()).state, McpConnectionState::Connected);
}

#[test]
fn oauth_state_is_opaque() {
    assert_ne!(random_secret(), random_secret());
    assert!(random_secret().len() >= 43);
}

#[tokio::test]
async fn issuer_optional_when_not_advertised() {
    let (_d, m, _p) = setup(3600, None);
    let mut callback = login_begin(&m).await;
    callback.issuer = None;
    assert!(m.callback(callback).await.is_ok());
}

#[tokio::test]
async fn server_edit_invalidates_pending_callback() {
    let (_d, m, p) = setup(3600, None);
    let callback = login_begin(&m).await;
    let mut config = server(&m);
    config.name = "Changed".into();
    upsert(&m, config, vec![]);
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test]
async fn callback_is_single_use() {
    let (_d, m, p) = setup(3600, None);
    let callback = login_begin(&m).await;
    m.callback(callback.clone()).await.unwrap();
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 1);
}

struct MetadataPeer {
    peer: Arc<Peer>,
    mode: &'static str,
}
#[async_trait]
impl HttpClient for MetadataPeer {
    async fn send(
        &self,
        r: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
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
                    .send(
                        HttpRequest {
                            url: "https://example.com/metadata".into(),
                            ..r
                        },
                        cancel,
                    )
                    .await;
            }
        }
        let metadata = r.url.contains(".well-known");
        let mut response = self.peer.send(r, cancel).await?;
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

fn metadata(mode: &'static str) -> impl FnOnce(Arc<Peer>) -> Arc<dyn HttpClient> {
    move |peer| Arc::new(MetadataPeer { peer, mode })
}

#[tokio::test]
async fn advertised_issuer_requires_callback_parameter() {
    let (_d, m, p) = setup_with(3600, None, metadata("issuer"));
    let mut callback = login_begin(&m).await;
    callback.issuer = None;
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test]
async fn protected_metadata_root_fallback() {
    let (_d, m, _p) = setup_with(3600, None, metadata("root"));
    assert!(m
        .connect(McpConnectParams {
            server_id: id(),
            redirect_uri: "http://127.0.0.1:49152/callback".into()
        })
        .await
        .is_ok());
}

#[tokio::test]
async fn pkce_s256_is_required_before_browser_login() {
    let (_d, m, _p) = setup_with(3600, None, metadata("no_pkce"));
    assert!(m
        .connect(McpConnectParams {
            server_id: id(),
            redirect_uri: "http://127.0.0.1:49152/callback".into()
        })
        .await
        .is_err());
    // A failed login is an error status with a fixed reason.
    let status = m.status(&id());
    assert_eq!(status.state, McpConnectionState::Error);
    assert!(status.reason.unwrap().contains("PKCE"));
}

#[tokio::test]
async fn non_loopback_redirect_rejected() {
    let (_d, m, p) = setup(3600, None);
    for redirect in [
        "https://evil.example/callback",
        "http://evil.example:80/callback",
        "http://127.0.0.1/callback",
        "http://127.0.0.1:5000/callback?x=1",
    ] {
        assert!(m
            .connect(McpConnectParams {
                server_id: id(),
                redirect_uri: redirect.into(),
            })
            .await
            .is_err());
    }
    assert!(p.requests.lock().unwrap().is_empty());
}

/// Holds every refresh until released.
struct RefreshGate {
    peer: Arc<Peer>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl HttpClient for RefreshGate {
    async fn send(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        if String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token") {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.peer.send(request, cancel).await
    }
}

#[tokio::test]
async fn refresh_after_disconnect_is_discarded() {
    for invalidate in ["disconnect", "disable", "remove"] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (gate_in, gate_out) = (entered.clone(), release.clone());
        let (d, m, p) = setup_with(0, None, move |peer| {
            Arc::new(RefreshGate {
                peer,
                entered: gate_in,
                release: gate_out,
            })
        });
        m.callback(login_begin(&m).await).await.unwrap();
        let other = m.clone();
        let task = tokio::spawn(async move { other.auth_headers(&id()).await });
        entered.notified().await;
        match invalidate {
            "disconnect" => {
                m.disconnect(&id()).await.unwrap();
            }
            "disable" => {
                let mut disabled = server(&m);
                disabled.enabled = false;
                upsert(&m, disabled, vec![]);
            }
            _ => {
                m.remove(&id()).unwrap();
            }
        }
        release.notify_one();
        assert!(task.await.unwrap().is_err(), "{invalidate}");
        // The refreshed token was never stored, in memory or on disk.
        assert!(!files(&d).contains("REFRESH_SENTINEL"), "{invalidate}");
        assert_eq!(p.refreshes.load(Ordering::SeqCst), 1);
        if invalidate == "disconnect" {
            assert!(reopen(&d, p).auth_headers(&id()).await.is_err());
        }
    }
}

#[tokio::test]
async fn edit_and_restore_does_not_revive_pending_login() {
    let (_d, m, p) = setup(3600, None);
    let callback = login_begin(&m).await;
    let original = server(&m);
    let mut changed = original.clone();
    changed.name = "Changed".into();
    for server in [changed, original] {
        upsert(&m, server, vec![]);
    }
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test]
async fn secret_only_edit_invalidates_pending_login() {
    let (_d, m, p) = setup(3600, None);
    let server = with_client_secret(&m, "oauth-client-secret", "INITIAL_SENTINEL");
    let callback = login_begin(&m).await;
    upsert(
        &m,
        server,
        vec![McpSecretWrite::Set {
            reference: "oauth-client-secret".into(),
            value: McpSecretValue("REPLACEMENT_SENTINEL".into()),
        }],
    );
    assert!(m.callback(callback).await.is_err());
    assert_eq!(token_requests(&p), 0);
}

#[tokio::test]
async fn reading_stale_token_does_not_cancel_new_login() {
    let (_d, m, _p) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let callback = login_begin(&m).await;
    assert!(m.auth_headers(&id()).await.is_err());
    m.callback(callback).await.unwrap();
}

#[tokio::test]
async fn removed_server_erases_oauth_credentials() {
    let (d, m, _p) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    assert!(files(&d).contains("ACCESS_SENTINEL"));
    m.remove(&id()).unwrap();
    assert!(!files(&d).contains("SENTINEL"));
    assert_eq!(
        m.disconnect(&id()).await.unwrap_err().code,
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn credential_edits_reject_stale_persisted_accounts() {
    for edit in ["secret", "url", "remove_and_add"] {
        let (d, m, p) = setup(3600, None);
        let mut original = server(&m);
        if edit == "secret" {
            original = with_client_secret(&m, "client-secret", "OLD_SENTINEL");
        }
        m.callback(login_begin(&m).await).await.unwrap();
        match edit {
            "secret" => upsert(
                &m,
                original,
                vec![McpSecretWrite::Set {
                    reference: "client-secret".into(),
                    value: McpSecretValue("NEW_SENTINEL".into()),
                }],
            ),
            "url" => {
                let mut moved = original;
                if let McpTransportConfig::Http { url, .. } = &mut moved.transport {
                    *url = "https://example.com/other".into();
                }
                upsert(&m, moved, vec![]);
            }
            _ => {
                m.remove(&id()).unwrap();
                upsert(&m, original, vec![]);
            }
        }
        assert!(!files(&d).contains("ACCESS_SENTINEL"), "{edit}");
        assert!(reopen(&d, p).auth_headers(&id()).await.is_err(), "{edit}");
    }
}

#[tokio::test]
async fn status_watch_and_generation_cover_login_and_disconnect() {
    let (_d, m, _p) = setup(3600, None);
    let mut changes = m.subscribe_status();
    let before = m.generation(&id());
    let callback = login_begin(&m).await;
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert!(m.generation(&id()) > before);
    m.callback(callback).await.unwrap();
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    let connected = m.generation(&id());
    m.disconnect(&id()).await.unwrap();
    assert!(changes.has_changed().unwrap());
    assert!(m.generation(&id()) > connected);
}

#[tokio::test]
async fn beginning_account_replacement_cannot_restore_old_account_on_restart() {
    let (d, m, p) = setup(3600, None);
    m.callback(login_begin(&m).await).await.unwrap();
    let _replacement = login_begin(&m).await;
    // Do not read headers: replacement itself must persist invalidation.
    assert!(reopen(&d, p).auth_headers(&id()).await.is_err());
    assert!(!files(&d).contains("ACCESS_SENTINEL"));
}
