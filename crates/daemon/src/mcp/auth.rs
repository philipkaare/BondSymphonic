//! Generic MCP OAuth with per-server epochs, private token persistence and refresh singleflight.
use super::{secrets, transport::*, McpManager};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bondsymphonic_proto::*;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub fn random_secret() -> String {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn form(values: &[(&str, &str)]) -> String {
    values
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
#[derive(Clone, Serialize, Deserialize)]
struct Revocation {
    endpoint: String,
    auth_method: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Token {
    config: McpServerConfig,
    access: String,
    refresh: Option<String>,
    expires: u64,
    issuer: String,
    endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    generation: u64,
    #[serde(default)]
    registry_generation: u64,
    #[serde(default)]
    revocation: Option<Revocation>,
}
struct TokenContext<'a> {
    config: &'a McpServerConfig,
    generation: u64,
    issuer: &'a str,
    client_id: &'a str,
    scopes: &'a [String],
}
struct Attempt {
    config: McpServerConfig,
    generation: u64,
    state: String,
    verifier: String,
    redirect: String,
    expires: u64,
    issuer: String,
    endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    registry_generation: u64,
    issuer_required: bool,
    revocation: Option<Revocation>,
}
#[derive(Default)]
struct AuthState {
    tokens: BTreeMap<McpServerId, Token>,
    attempts: BTreeMap<McpServerId, Attempt>,
    epochs: BTreeMap<McpServerId, u64>,
}
pub type Sha256Provider = Arc<dyn Fn(&[u8]) -> Result<[u8; 32], RpcError> + Send + Sync>;
pub struct RemoteMcpManager {
    registry: Arc<McpManager>,
    client: Arc<dyn HttpClient>,
    hash: Sha256Provider,
    state: parking_lot::Mutex<AuthState>,
    refresh_locks: parking_lot::Mutex<BTreeMap<McpServerId, Arc<tokio::sync::Mutex<()>>>>,
    auth_changes: tokio::sync::watch::Sender<u64>,
}
impl RemoteMcpManager {
    pub fn new(registry: Arc<McpManager>) -> Self {
        Self::with_client(registry, Arc::new(CurlHttpClient))
    }
    pub fn with_client(registry: Arc<McpManager>, client: Arc<dyn HttpClient>) -> Self {
        Self::with_providers(registry, client, Arc::new(native_sha256))
    }
    pub fn with_providers(
        registry: Arc<McpManager>,
        client: Arc<dyn HttpClient>,
        hash: Sha256Provider,
    ) -> Self {
        let tokens = std::fs::read(registry.root.join("oauth.json"))
            .ok()
            .and_then(|bytes| {
                if bytes.len() <= MAX_BODY {
                    serde_json::from_slice::<BTreeMap<McpServerId, Token>>(&bytes).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let mut epochs = BTreeMap::new();
        let tokens = tokens
            .into_iter()
            .filter_map(|(id, t)| {
                let valid = registry.server(&id).is_ok_and(|c| c == t.config)
                    && HttpsUrl::parse(&t.endpoint).is_ok()
                    && HttpsUrl::parse(&t.issuer).is_ok();
                let valid = valid && t.registry_generation == registry.generation(&id);
                if valid {
                    epochs.insert(id.clone(), t.generation);
                }
                valid.then_some((id, t))
            })
            .collect();
        let (auth_changes, _) = tokio::sync::watch::channel(0);
        Self {
            registry,
            client,
            hash,
            state: parking_lot::Mutex::new(AuthState {
                tokens,
                epochs,
                ..Default::default()
            }),
            refresh_locks: Default::default(),
            auth_changes,
        }
    }
    pub fn auth_generation(&self, id: &McpServerId) -> u64 {
        self.state.lock().epochs.get(id).copied().unwrap_or(0)
    }
    /// Authentication readiness only, without opening a connection or refreshing
    /// tokens. An expired OAuth token with a refresh token remains configured.
    pub fn has_credentials(&self, id: &McpServerId) -> bool {
        let Ok(config) = self.registry.server(id) else {
            return false;
        };
        match &config.transport {
            McpTransportConfig::Http {
                auth: McpAuthConfig::OAuth { .. },
                ..
            } => {
                let state = self.state.lock();
                state.tokens.get(id).is_some_and(|token| {
                    self.current(
                        id,
                        &token.config,
                        token.generation,
                        token.registry_generation,
                        &state,
                    ) && !token.access.is_empty()
                        && (token.expires > now()
                            || token
                                .refresh
                                .as_ref()
                                .is_some_and(|refresh| !refresh.is_empty()))
                })
            }
            McpTransportConfig::Http {
                auth: McpAuthConfig::Bearer { token },
                ..
            } => self.registry.secret(token).is_some_and(|value| {
                !value.trim().is_empty()
                    && validate_auth_headers(&[("Authorization".into(), format!("Bearer {value}"))])
                        .is_ok()
            }),
            McpTransportConfig::Http {
                auth: McpAuthConfig::Headers { headers },
                ..
            } => {
                let values: Option<Vec<_>> = headers
                    .iter()
                    .map(|(name, reference)| {
                        self.registry
                            .secret(reference)
                            .map(|value| (name.clone(), value))
                    })
                    .collect();
                values.is_some_and(|values| validate_auth_headers(&values).is_ok())
            }
            _ => true,
        }
    }
    pub fn has_pending_login(&self, id: &McpServerId) -> bool {
        let state = self.state.lock();
        state.attempts.get(id).is_some_and(|attempt| {
            attempt.expires > now()
                && self.current(
                    id,
                    &attempt.config,
                    attempt.generation,
                    attempt.registry_generation,
                    &state,
                )
        })
    }
    /// Notify on login begin/completion, disconnect, stale-token removal and
    /// refresh. Refresh retains the account epoch: update request headers;
    /// login/disconnect epoch changes revoke sessions.
    pub fn subscribe_auth_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.auth_changes.subscribe()
    }
    fn auth_changed(&self) {
        self.auth_changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    fn save(&self, state: &AuthState) -> Result<(), RpcError> {
        let bytes =
            serde_json::to_vec(&state.tokens).map_err(|_| error("credential storage failed"))?;
        secrets::write_private(&self.registry.root.join("oauth.json"), &bytes)
            .map_err(|_| error("credential storage failed"))
    }
    fn status(id: &McpServerId, state: McpConnectionState) -> McpServerStatus {
        McpServerStatus {
            server_id: id.clone(),
            state,
            reason: None,
        }
    }
    fn current(
        &self,
        id: &McpServerId,
        config: &McpServerConfig,
        generation: u64,
        registry_generation: u64,
        state: &AuthState,
    ) -> bool {
        self.registry.server(id).is_ok_and(|s| s == *config)
            && self.registry.generation(id) == registry_generation
            && state.epochs.get(id).copied().unwrap_or(0) == generation
    }
    pub fn disconnect(&self, id: &McpServerId) -> Result<McpServerStatus, RpcError> {
        self.disconnect_token(id).map(|(status, _)| status)
    }
    fn disconnect_token(
        &self,
        id: &McpServerId,
    ) -> Result<(McpServerStatus, Option<Token>), RpcError> {
        let mut state = self.state.lock();
        let epoch = state.epochs.entry(id.clone()).or_default();
        *epoch = epoch
            .checked_add(1)
            .ok_or_else(|| error("generation exhausted"))?;
        state.attempts.remove(id);
        let token = state.tokens.remove(id);
        self.auth_changed();
        if let Err(e) = self.save(&state) {
            let _ = std::fs::remove_file(self.registry.root.join("oauth.json"));
            return Err(e);
        }
        Ok((Self::status(id, McpConnectionState::Disconnected), token))
    }
    /// Persist local invalidation before contacting an advertised revocation
    /// endpoint. Upstream failure or cancellation cannot restore the account.
    pub async fn disconnect_and_revoke(
        &self,
        id: &McpServerId,
    ) -> Result<McpServerStatus, RpcError> {
        let (status, token) = self.disconnect_token(id)?;
        if let Some(token) = token {
            let _ = tokio::time::timeout(Duration::from_secs(3), self.revoke_token(&token)).await;
        }
        Ok(status)
    }
    async fn revoke_token(&self, token: &Token) {
        let Some(revocation) = &token.revocation else {
            return;
        };
        if self.registry.generation(&token.config.id) != token.registry_generation
            || !self
                .registry
                .server(&token.config.id)
                .is_ok_and(|c| c == token.config)
        {
            return;
        }
        if HttpsUrl::parse(&revocation.endpoint).is_err() {
            return;
        }
        let secret = match revocation.auth_method.as_str() {
            "none" => None,
            "client_secret_post" | "client_secret_basic" => {
                let Ok(Some(secret)) = self.client_secret(&token.config) else {
                    return;
                };
                Some(secret)
            }
            _ => return,
        };
        // Generations are monotonic and secret writes advance them atomically.
        // Recheck around the immutable secret copy so an edit cannot send a
        // replacement client's credential to this token's former issuer.
        if self.registry.generation(&token.config.id) != token.registry_generation {
            return;
        }
        let mut tokens = vec![];
        if let Some(refresh) = token.refresh.as_deref().filter(|s| !s.is_empty()) {
            tokens.push((refresh, "refresh_token"));
        }
        tokens.push((token.access.as_str(), "access_token"));
        for (value, hint) in tokens {
            let mut fields = vec![
                ("token", value),
                ("token_type_hint", hint),
                ("client_id", token.client_id.as_str()),
            ];
            let mut headers = vec![(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )];
            if let Some(secret) = &secret {
                if revocation.auth_method == "client_secret_post" {
                    fields.push(("client_secret", secret));
                } else {
                    let credentials = format!("{}:{}", encode(&token.client_id), encode(secret));
                    headers.push((
                        "Authorization".into(),
                        format!(
                            "Basic {}",
                            base64::engine::general_purpose::STANDARD.encode(credentials)
                        ),
                    ));
                }
            }
            // There are no retries and no upstream diagnostics are published.
            let _ = self
                .request(HttpRequest {
                    method: "POST".into(),
                    url: revocation.endpoint.clone(),
                    headers,
                    body: form(&fields).into_bytes(),
                })
                .await;
        }
    }
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, RpcError> {
        HttpsUrl::parse(&request.url)?;
        let response = tokio::time::timeout(Duration::from_secs(35), self.client.execute(request))
            .await
            .map_err(|_| error("HTTP deadline exceeded"))??;
        if response.body.len() > MAX_BODY {
            return Err(error("response too large"));
        }
        validate_headers(&response.headers)?;
        if (300..400).contains(&response.status) {
            return Err(error("OAuth redirect denied"));
        }
        Ok(response)
    }
    async fn metadata(&self, url: String) -> Result<Value, RpcError> {
        let response = self
            .request(HttpRequest {
                method: "GET".into(),
                url,
                headers: vec![("Accept".into(), "application/json".into())],
                body: vec![],
            })
            .await?;
        if response.status != 200 {
            return Err(error("OAuth metadata unavailable"));
        }
        serde_json::from_slice(&response.body).map_err(|_| error("invalid OAuth metadata"))
    }
    async fn discover(&self, locations: &[String]) -> Result<Value, RpcError> {
        let mut tried = std::collections::BTreeSet::new();
        for location in locations {
            if !tried.insert(location) {
                continue;
            }
            let response = self
                .request(HttpRequest {
                    method: "GET".into(),
                    url: location.clone(),
                    headers: vec![("Accept".into(), "application/json".into())],
                    body: vec![],
                })
                .await?;
            if matches!(response.status, 404 | 405) {
                continue;
            }
            if response.status != 200 {
                return Err(error("OAuth metadata unavailable"));
            }
            return serde_json::from_slice(&response.body)
                .map_err(|_| error("invalid OAuth metadata"));
        }
        Err(error("OAuth metadata unavailable"))
    }
    pub async fn connect(&self, params: McpConnectParams) -> Result<McpConnectResult, RpcError> {
        let config = self.registry.server(&params.server_id)?;
        let registry_generation = self.registry.generation(&params.server_id);
        let (url, auth) = match &config.transport {
            McpTransportConfig::Http { url, auth } => (url.clone(), auth.clone()),
            _ => return Err(error("remote authentication requires HTTP")),
        };
        let (client_id, metadata_url, scopes) = match auth {
            McpAuthConfig::OAuth {
                client_id,
                client_metadata_url,
                scopes,
                ..
            } => (client_id, client_metadata_url, scopes),
            _ => {
                self.headers(&params.server_id).await?;
                return Ok(McpConnectResult {
                    authorization_url: String::new(),
                    expires_at: String::new(),
                });
            }
        };
        validate_redirect(&params.redirect_uri)?;
        let resource = HttpsUrl::parse(&url)?;
        let generation = {
            let mut state = self.state.lock();
            let previous_epoch = state.epochs.get(&params.server_id).copied().unwrap_or(0);
            let epoch = state.epochs.entry(params.server_id.clone()).or_default();
            *epoch = epoch
                .checked_add(1)
                .ok_or_else(|| error("generation exhausted"))?;
            let generation = *epoch;
            let old_attempt = state.attempts.remove(&params.server_id);
            let old_token = state.tokens.remove(&params.server_id);
            // An attempted account replacement must not revive the previous
            // account on restart. Publish no new attempt until this is durable.
            if let Err(e) = self.save(&state) {
                state
                    .epochs
                    .insert(params.server_id.clone(), previous_epoch);
                if let Some(attempt) = old_attempt {
                    state.attempts.insert(params.server_id.clone(), attempt);
                }
                if let Some(token) = old_token {
                    state.tokens.insert(params.server_id.clone(), token);
                }
                return Err(e);
            }
            self.auth_changed();
            generation
        };
        let challenge = self
            .request(HttpRequest {
                method: "GET".into(),
                url: url.clone(),
                headers: vec![],
                body: vec![],
            })
            .await?;
        let challenge_fields = challenge
            .header("www-authenticate")
            .map(parse_challenge)
            .transpose()?
            .unwrap_or_default();
        let protected = if let Some(location) = challenge_fields.get("resource_metadata") {
            self.metadata(location.clone()).await?
        } else {
            let path = resource.path.split('?').next().unwrap_or("/");
            self.discover(&[
                format!(
                    "{}/.well-known/oauth-protected-resource{}",
                    resource.origin,
                    if path == "/" { "" } else { path }
                ),
                format!("{}/.well-known/oauth-protected-resource", resource.origin),
            ])
            .await?
        };
        if protected.get("resource").and_then(Value::as_str) != Some(url.as_str()) {
            return Err(error("OAuth resource mismatch"));
        }
        let issuer = protected
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(Value::as_str)
            .ok_or_else(|| error("OAuth authorization server missing"))?
            .to_owned();
        let issuer_url = HttpsUrl::parse(&issuer)?;
        if issuer.contains('?') {
            return Err(error("invalid OAuth issuer"));
        }
        let path = issuer_url.path.trim_end_matches('/');
        let metadata_location = format!(
            "{}/.well-known/oauth-authorization-server{}",
            issuer_url.origin,
            if path == "/" { "" } else { path }
        );
        let metadata = self
            .discover(&[
                metadata_location,
                format!(
                    "{}/.well-known/openid-configuration{}",
                    issuer_url.origin,
                    if path == "/" { "" } else { path }
                ),
                format!(
                    "{}/.well-known/openid-configuration",
                    issuer.trim_end_matches('/')
                ),
            ])
            .await?;
        if metadata.get("issuer").and_then(Value::as_str) != Some(&issuer) {
            return Err(error("OAuth issuer mismatch"));
        }
        if !metadata
            .get("code_challenge_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|v| v.iter().any(|x| x == "S256"))
        {
            return Err(error("OAuth PKCE S256 required"));
        }
        let authorize = endpoint(&metadata, "authorization_endpoint")?;
        let token_endpoint = endpoint(&metadata, "token_endpoint")?;
        let revocation = revocation_metadata(&metadata, &config);
        let requested_scopes = if let Some(scope) = challenge_fields.get("scope") {
            let mut result = scopes.clone();
            for s in scope.split_whitespace() {
                if !result.iter().any(|v| v == s) {
                    result.push(s.into());
                }
            }
            result
        } else {
            scopes
        };
        let client_id = if let Some(id) = client_id {
            if id.is_empty() || id.len() > 2048 {
                return Err(error("invalid OAuth client ID"));
            }
            id
        } else if let Some(metadata_url) = metadata_url {
            HttpsUrl::parse(&metadata_url)?;
            if !metadata
                .get("client_id_metadata_document_supported")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(error(
                    "OAuth client metadata unsupported; configure client ID",
                ));
            }
            metadata_url
        } else {
            let registration = endpoint(&metadata, "registration_endpoint")?;
            let response=self.request(HttpRequest{method:"POST".into(),url:registration,headers:vec![("Content-Type".into(),"application/json".into())],body:serde_json::to_vec(&json!({"client_name":"BondSymphonic","redirect_uris":[params.redirect_uri],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"})).unwrap()}).await?;
            if response.status != 201 && response.status != 200 {
                return Err(error("OAuth registration failed; configure client ID"));
            }
            let body: Value = serde_json::from_slice(&response.body)
                .map_err(|_| error("invalid OAuth registration"))?;
            let id = body
                .get("client_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 2048)
                .ok_or_else(|| error("OAuth client ID missing"))?
                .to_owned();
            if body.get("client_secret").is_some() {
                return Err(error(
                    "OAuth registration requires configured confidential client",
                ));
            }
            id
        };
        let state_value = random_secret();
        let verifier = random_secret();
        let challenge = URL_SAFE_NO_PAD.encode((self.hash)(verifier.as_bytes())?);
        let scope = requested_scopes.join(" ");
        let query = form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &params.redirect_uri),
            ("state", &state_value),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("resource", &url),
            ("scope", &scope),
        ]);
        let authorization_url = format!(
            "{}{}{}",
            authorize,
            if authorize.contains('?') { "&" } else { "?" },
            query
        );
        let expires = now() + 600;
        let attempt = Attempt {
            config: config.clone(),
            generation,
            state: state_value,
            verifier,
            redirect: params.redirect_uri,
            expires,
            issuer,
            endpoint: token_endpoint,
            client_id,
            scopes: requested_scopes,
            registry_generation,
            issuer_required: metadata
                .get("authorization_response_iss_parameter_supported")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            revocation,
        };
        let mut state = self.state.lock();
        if !self.current(
            &params.server_id,
            &config,
            generation,
            registry_generation,
            &state,
        ) {
            return Err(error("OAuth attempt invalidated"));
        }
        state.attempts.insert(params.server_id, attempt);
        self.auth_changed();
        Ok(McpConnectResult {
            authorization_url,
            expires_at: chrono::DateTime::from_timestamp(expires as i64, 0)
                .unwrap()
                .to_rfc3339(),
        })
    }
    pub async fn callback(&self, params: McpCallbackParams) -> Result<McpServerStatus, RpcError> {
        let attempt = {
            let mut state = self.state.lock();
            let attempt = state
                .attempts
                .get(&params.server_id)
                .ok_or_else(|| error("OAuth attempt missing"))?;
            if !constant_eq(&attempt.state, &params.state.0) {
                return Err(error("OAuth state mismatch"));
            }
            if params
                .issuer
                .as_deref()
                .is_some_and(|issuer| issuer != attempt.issuer)
                || (attempt.issuer_required && params.issuer.is_none())
            {
                return Err(error("OAuth issuer mismatch"));
            }
            if attempt.expires <= now()
                || !self.current(
                    &params.server_id,
                    &attempt.config,
                    attempt.generation,
                    attempt.registry_generation,
                    &state,
                )
            {
                state.attempts.remove(&params.server_id);
                return Err(error("OAuth attempt expired or invalidated"));
            }
            state.attempts.remove(&params.server_id).unwrap()
        };
        if params.code.0.is_empty() || params.code.0.len() > 8192 {
            return Err(error("invalid OAuth code"));
        }
        let resource = remote_url(&attempt.config)?;
        let scope = attempt.scopes.join(" ");
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", &params.code.0),
            ("client_id", &attempt.client_id),
            ("redirect_uri", &attempt.redirect),
            ("code_verifier", &attempt.verifier),
            ("resource", resource),
            ("scope", &scope),
        ];
        let secret = self.client_secret(&attempt.config)?;
        if let Some(secret) = &secret {
            fields.push(("client_secret", secret));
        }
        let mut token = self
            .exchange(
                &attempt.endpoint,
                &fields,
                TokenContext {
                    config: &attempt.config,
                    generation: attempt.generation,
                    issuer: &attempt.issuer,
                    client_id: &attempt.client_id,
                    scopes: &attempt.scopes,
                },
            )
            .await?;
        token.revocation = attempt.revocation;
        let mut state = self.state.lock();
        if !self.current(
            &params.server_id,
            &attempt.config,
            attempt.generation,
            attempt.registry_generation,
            &state,
        ) {
            return Err(error("OAuth exchange invalidated"));
        }
        state.tokens.insert(params.server_id.clone(), token);
        if let Err(e) = self.save(&state) {
            state.tokens.remove(&params.server_id);
            return Err(e);
        }
        self.auth_changed();
        Ok(Self::status(
            &params.server_id,
            McpConnectionState::Connected,
        ))
    }
    fn client_secret(&self, config: &McpServerConfig) -> Result<Option<String>, RpcError> {
        if let McpTransportConfig::Http {
            auth:
                McpAuthConfig::OAuth {
                    client_secret: Some(reference),
                    ..
                },
            ..
        } = &config.transport
        {
            Ok(Some(
                self.registry
                    .secret(reference)
                    .ok_or_else(|| error("OAuth client credential missing"))?,
            ))
        } else {
            Ok(None)
        }
    }
    async fn exchange(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
        context: TokenContext<'_>,
    ) -> Result<Token, RpcError> {
        let TokenContext {
            config,
            generation,
            issuer,
            client_id,
            scopes,
        } = context;
        let response = self
            .request(HttpRequest {
                method: "POST".into(),
                url: endpoint.into(),
                headers: vec![(
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                )],
                body: form(fields).into_bytes(),
            })
            .await?;
        if response.status != 200 {
            return Err(error("OAuth token exchange rejected"));
        }
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|_| error("invalid OAuth token response"))?;
        let access = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 16384 && !s.chars().any(char::is_control))
            .ok_or_else(|| error("OAuth access token missing"))?
            .to_owned();
        if !body
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
        {
            return Err(error("OAuth bearer token required"));
        }
        if let Some(granted) = body.get("scope").and_then(Value::as_str) {
            if scopes
                .iter()
                .any(|s| !granted.split_whitespace().any(|g| g == s))
            {
                return Err(error("step_up_scope_required"));
            }
        }
        let refresh = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if refresh
            .as_ref()
            .is_some_and(|s| s.len() > 16384 || s.chars().any(char::is_control))
        {
            return Err(error("invalid OAuth token response"));
        }
        let expires = now().saturating_add(
            body.get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(300)
                .min(365 * 86400),
        );
        Ok(Token {
            config: config.clone(),
            access,
            refresh,
            expires,
            issuer: issuer.into(),
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            scopes: scopes.to_vec(),
            generation,
            registry_generation: self.registry.generation(&config.id),
            revocation: None,
        })
    }
    pub async fn headers(&self, id: &McpServerId) -> Result<Vec<(String, String)>, RpcError> {
        let config = self.registry.server(id)?;
        let auth = match &config.transport {
            McpTransportConfig::Http { auth, .. } => auth,
            _ => return Err(error("remote HTTP required")),
        };
        let headers = match auth {
            McpAuthConfig::None => vec![],
            McpAuthConfig::Bearer { token } => vec![(
                "Authorization".into(),
                format!(
                    "Bearer {}",
                    self.registry
                        .secret(token)
                        .ok_or_else(|| error("credential missing"))?
                ),
            )],
            McpAuthConfig::Headers { headers } => {
                let mut result = vec![];
                for (n, r) in headers {
                    result.push((
                        n.clone(),
                        self.registry
                            .secret(r)
                            .ok_or_else(|| error("credential missing"))?,
                    ));
                }
                result
            }
            McpAuthConfig::OAuth { .. } => {
                let lock = {
                    self.refresh_locks
                        .lock()
                        .entry(id.clone())
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                        .clone()
                };
                let _guard = lock.lock().await;
                let token = {
                    let mut state = self.state.lock();
                    let token = state
                        .tokens
                        .get(id)
                        .cloned()
                        .ok_or_else(|| error("authentication_required"))?;
                    if !self.current(
                        id,
                        &token.config,
                        token.generation,
                        token.registry_generation,
                        &state,
                    ) {
                        state.tokens.remove(id);
                        if state.attempts.get(id).is_some_and(|attempt| {
                            attempt.generation == token.generation
                                || attempt.registry_generation != self.registry.generation(id)
                        }) {
                            state.attempts.remove(id);
                        }
                        let _ = self.save(&state);
                        self.auth_changed();
                        return Err(error("authentication_required"));
                    }
                    token
                };
                if token.expires > now() + 30 {
                    vec![("Authorization".into(), format!("Bearer {}", token.access))]
                } else {
                    let refresh = token
                        .refresh
                        .as_ref()
                        .ok_or_else(|| error("authentication_required"))?;
                    let scope = token.scopes.join(" ");
                    let resource = remote_url(&config)?;
                    let mut fields = vec![
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh.as_str()),
                        ("client_id", token.client_id.as_str()),
                        ("resource", resource),
                        ("scope", scope.as_str()),
                    ];
                    let secret = self.client_secret(&config)?;
                    if let Some(secret) = &secret {
                        fields.push(("client_secret", secret));
                    }
                    let mut replacement = self
                        .exchange(
                            &token.endpoint,
                            &fields,
                            TokenContext {
                                config: &config,
                                generation: token.generation,
                                issuer: &token.issuer,
                                client_id: &token.client_id,
                                scopes: &token.scopes,
                            },
                        )
                        .await?;
                    if replacement.refresh.is_none() {
                        replacement.refresh = token.refresh;
                    }
                    replacement.revocation = token.revocation;
                    let mut state = self.state.lock();
                    if !self.current(
                        id,
                        &config,
                        token.generation,
                        token.registry_generation,
                        &state,
                    ) {
                        return Err(error("OAuth refresh invalidated"));
                    }
                    let result = vec![(
                        "Authorization".into(),
                        format!("Bearer {}", replacement.access),
                    )];
                    state.tokens.insert(id.clone(), replacement);
                    if let Err(e) = self.save(&state) {
                        state.tokens.remove(id);
                        return Err(e);
                    }
                    self.auth_changed();
                    result
                }
            }
        };
        validate_auth_headers(&headers)?;
        Ok(headers)
    }
}
fn revocation_metadata(metadata: &Value, config: &McpServerConfig) -> Option<Revocation> {
    let endpoint = endpoint(metadata, "revocation_endpoint").ok()?;
    let confidential = matches!(
        &config.transport,
        McpTransportConfig::Http {
            auth: McpAuthConfig::OAuth {
                client_secret: Some(_),
                ..
            },
            ..
        }
    );
    // RFC 8414 defaults omitted authentication metadata to client_secret_basic.
    let methods = metadata.get("revocation_endpoint_auth_methods_supported");
    let supported = |method: &str| match methods {
        Some(Value::Array(values)) => values.iter().any(|v| v.as_str() == Some(method)),
        None => method == "client_secret_basic",
        _ => false,
    };
    let method = if confidential {
        ["client_secret_post", "client_secret_basic"]
            .into_iter()
            .find(|m| supported(m))?
    } else if supported("none") {
        "none"
    } else {
        return None;
    };
    Some(Revocation {
        endpoint,
        auth_method: method.into(),
    })
}
fn endpoint(metadata: &Value, name: &str) -> Result<String, RpcError> {
    let url = metadata
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| error("OAuth endpoint missing; configure client ID"))?;
    HttpsUrl::parse(url)?;
    Ok(url.into())
}
fn remote_url(config: &McpServerConfig) -> Result<&str, RpcError> {
    match &config.transport {
        McpTransportConfig::Http { url, .. } => Ok(url),
        _ => Err(error("remote HTTP required")),
    }
}
fn constant_eq(a: &str, b: &str) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(
            a.as_bytes().get(i).copied().unwrap_or(0) ^ b.as_bytes().get(i).copied().unwrap_or(0),
        );
    }
    diff == 0
}
fn validate_redirect(uri: &str) -> Result<(), RpcError> {
    if uri.len() > 2048 || uri.chars().any(char::is_control) || uri.contains(['?', '#', '@', '\\'])
    {
        return Err(error("invalid OAuth callback URL"));
    }
    let rest = uri
        .strip_prefix("http://")
        .ok_or_else(|| error("loopback callback required"))?;
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| error("callback port required"))?;
    if !matches!(host, "127.0.0.1" | "[::1]" | "localhost")
        || port.parse::<u16>().ok().filter(|p| *p > 0).is_none()
    {
        return Err(error("loopback callback required"));
    }
    Ok(())
}
fn parse_challenge(header: &str) -> Result<BTreeMap<String, String>, RpcError> {
    if header.len() > MAX_HEADERS {
        return Err(error("OAuth challenge too large"));
    }
    let mut fields = BTreeMap::new();
    let tail = header
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| header.trim().strip_prefix("bearer "))
        .ok_or_else(|| error("unsupported authentication challenge"))?;
    let mut pieces = vec![];
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in tail.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ',' && !quoted {
            pieces.push(&tail[start..i]);
            start = i + 1;
        }
    }
    pieces.push(&tail[start..]);
    if quoted {
        return Err(error("invalid OAuth challenge"));
    }
    for piece in pieces {
        let (k, v) = piece
            .trim()
            .split_once('=')
            .ok_or_else(|| error("invalid OAuth challenge"))?;
        let v = v.trim();
        let v = if v.starts_with('"') {
            v.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .ok_or_else(|| error("invalid OAuth challenge"))?
        } else {
            v
        };
        if fields.insert(k.trim().to_owned(), v.to_owned()).is_some() {
            return Err(error("invalid OAuth challenge"));
        }
    }
    Ok(fields)
}
