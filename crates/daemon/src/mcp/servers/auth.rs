//! Generic MCP OAuth for remote servers: discovery, PKCE login, the code
//! exchange, refresh and disconnect, plus the credential headers every remote
//! request carries and the connection status Settings shows.
//!
//! The IDE owns the loopback callback listener and the browser; the daemon
//! keeps the PKCE verifier and the pending login (at most 10 minutes) and
//! exchanges the code. The resulting token is stored with the server's other
//! secrets under [`TOKEN_SECRET`], written only while the server is still at
//! the generation the login or refresh started under, so a disconnect, edit
//! or removal in the meantime always wins.
use super::{transport::*, McpServers};
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
use tokio_util::sync::CancellationToken;

/// The secret name of a server's OAuth record (a JSON [`Token`]).
pub(crate) const TOKEN_SECRET: &str = "oauth.token";
/// Secret names the daemon writes itself; no user reference can start so.
pub(crate) const RESERVED_PREFIX: &str = "oauth.";
/// How long a started login waits for its callback.
const ATTEMPT_TTL: Duration = Duration::from_secs(600);
/// Refresh this long before the token's stated expiry.
const REFRESH_MARGIN: u64 = 30;
/// Revocation is best effort and must not hold up a disconnect.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(3);

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

/// The stored OAuth record. Deliberately no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
struct Token {
    access: String,
    refresh: Option<String>,
    /// Unix seconds.
    expires: u64,
    issuer: String,
    endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    #[serde(default)]
    revocation: Option<Revocation>,
}
impl Token {
    fn parse(value: Option<McpSecretValue>) -> Option<Self> {
        serde_json::from_str(&value?.0).ok()
    }
    fn to_secret(&self) -> Result<McpSecretValue, RpcError> {
        serde_json::to_string(self)
            .map(McpSecretValue)
            .map_err(|_| error("credential storage failed"))
    }
}

/// A login waiting for its callback.
struct Attempt {
    generation: u64,
    state: String,
    verifier: String,
    redirect: String,
    expires: tokio::time::Instant,
    issuer: String,
    endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    issuer_required: bool,
    revocation: Option<Revocation>,
}

#[derive(Default)]
pub(crate) struct AuthState {
    attempts: BTreeMap<McpServerId, Attempt>,
    refresh_locks: BTreeMap<McpServerId, Arc<tokio::sync::Mutex<()>>>,
    /// The latest test or failure per server, valid only at its generation.
    statuses: BTreeMap<McpServerId, (u64, McpConnectionState, Option<String>)>,
}

fn remote(config: &McpServerConfig) -> Result<(&str, &McpAuthConfig), RpcError> {
    match &config.transport {
        McpTransportConfig::Http { url, auth } => Ok((url, auth)),
        McpTransportConfig::Stdio { .. } => Err(error("remote HTTP required")),
    }
}

impl McpServers {
    /// A server's connection state: a recorded test result or failure while
    /// still current; else, for OAuth, `Connecting` during a login and
    /// `Connected` once a token exists; else `Disconnected`.
    pub fn status(&self, id: &McpServerId) -> McpServerStatus {
        let status = |state, reason| McpServerStatus {
            server_id: id.clone(),
            state,
            reason,
        };
        let Some(config) = self.server(id) else {
            return status(McpConnectionState::Disconnected, None);
        };
        let (generation, token) = self.secret_with_generation(id, TOKEN_SECRET);
        let auth = self.auth.lock();
        if let Some((at, state, reason)) = auth.statuses.get(id) {
            if *at == generation {
                return status(state.clone(), reason.clone());
            }
        }
        if let Ok((_, McpAuthConfig::OAuth { .. })) = remote(&config) {
            if auth.attempts.get(id).is_some_and(|a| {
                a.generation == generation && tokio::time::Instant::now() < a.expires
            }) {
                return status(McpConnectionState::Connecting, None);
            }
            if token.is_some() {
                return status(McpConnectionState::Connected, None);
            }
        }
        status(McpConnectionState::Disconnected, None)
    }

    /// Records a status for `generation`; a later edit makes it stale.
    pub(crate) fn record_status(
        &self,
        id: &McpServerId,
        generation: u64,
        state: McpConnectionState,
        reason: Option<String>,
    ) {
        self.auth
            .lock()
            .statuses
            .insert(id.clone(), (generation, state, reason));
        self.status_changed();
    }

    /// A request failed upstream: `error` is one of this module's fixed
    /// reasons, never upstream text.
    pub(crate) fn note_failure(&self, id: &McpServerId, error: &RpcError) {
        let generation = self.generation(id);
        self.record_status(
            id,
            generation,
            McpConnectionState::Error,
            Some(error.message.clone()),
        );
    }

    fn clear_status(&self, id: &McpServerId) {
        self.auth.lock().statuses.remove(id);
        self.status_changed();
    }

    async fn http_request(&self, request: HttpRequest) -> Result<HttpResponse, RpcError> {
        HttpsUrl::parse(&request.url)?;
        let response = tokio::time::timeout(
            Duration::from_secs(65),
            self.http.send(request, CancellationToken::new()),
        )
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
            .http_request(HttpRequest {
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
                .http_request(HttpRequest {
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

    /// Begins a login (OAuth) or checks the connection (any other remote
    /// server). For OAuth this replaces any existing account at once, durably,
    /// and returns the URL the IDE opens; the callback must arrive within
    /// 10 minutes. Other servers get an empty URL after a successful test.
    pub async fn connect(
        self: &Arc<Self>,
        params: McpConnectParams,
    ) -> Result<McpConnectResult, RpcError> {
        self.ensure_usable()?;
        let id = params.server_id.clone();
        let config = self
            .server(&id)
            .ok_or_else(|| RpcError::not_found("MCP server not found"))?;
        let (_, auth) = remote(&config)?;
        if !matches!(auth, McpAuthConfig::OAuth { .. }) {
            let result = self.test(&id).await?;
            if result.status.state != McpConnectionState::Connected {
                return Err(error(
                    result
                        .status
                        .reason
                        .as_deref()
                        .unwrap_or("connection failed")
                        .trim_start_matches("MCP "),
                ));
            }
            return Ok(McpConnectResult {
                authorization_url: String::new(),
                expires_at: String::new(),
            });
        }
        validate_redirect(&params.redirect_uri)?;
        // An attempted account replacement must not revive the previous
        // account, not even on restart: the old token goes before anything
        // else happens.
        let (generation, _) = self.invalidate(&id, TOKEN_SECRET)?;
        {
            let mut state = self.auth.lock();
            state.attempts.remove(&id);
            state.statuses.remove(&id);
        }
        self.status_changed();
        match self
            .begin_login(&config, generation, params.redirect_uri)
            .await
        {
            Ok((attempt, authorization_url)) => {
                let expires_at = chrono::Utc::now()
                    + chrono::Duration::from_std(ATTEMPT_TTL).unwrap_or_default();
                {
                    let mut state = self.auth.lock();
                    if self.generation(&id) != generation {
                        return Err(error("OAuth attempt invalidated"));
                    }
                    state.attempts.insert(id, attempt);
                }
                self.status_changed();
                Ok(McpConnectResult {
                    authorization_url,
                    expires_at: expires_at.to_rfc3339(),
                })
            }
            Err(e) => {
                self.record_status(
                    &id,
                    generation,
                    McpConnectionState::Error,
                    Some(e.message.clone()),
                );
                Err(e)
            }
        }
    }

    /// Discovery (protected-resource, then authorization-server metadata),
    /// the client identity, and the PKCE authorization URL.
    async fn begin_login(
        &self,
        config: &McpServerConfig,
        generation: u64,
        redirect_uri: String,
    ) -> Result<(Attempt, String), RpcError> {
        let (url, auth) = remote(config)?;
        let McpAuthConfig::OAuth {
            client_id,
            client_metadata_url,
            scopes,
            ..
        } = auth
        else {
            return Err(error("OAuth required"));
        };
        let url = url.to_owned();
        let resource = HttpsUrl::parse(&url)?;
        let challenge = self
            .http_request(HttpRequest {
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
        let suffix = if path.is_empty() || path == "/" {
            ""
        } else {
            path
        };
        let metadata = self
            .discover(&[
                format!(
                    "{}/.well-known/oauth-authorization-server{suffix}",
                    issuer_url.origin
                ),
                format!(
                    "{}/.well-known/openid-configuration{suffix}",
                    issuer_url.origin
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
        let revocation = revocation_metadata(&metadata, config);
        let mut requested_scopes = scopes.clone();
        if let Some(scope) = challenge_fields.get("scope") {
            for s in scope.split_whitespace() {
                if !requested_scopes.iter().any(|v| v == s) {
                    requested_scopes.push(s.into());
                }
            }
        }
        let client_id = if let Some(id) = client_id {
            if id.is_empty() || id.len() > 2048 {
                return Err(error("invalid OAuth client ID"));
            }
            id.clone()
        } else if let Some(metadata_url) = client_metadata_url {
            HttpsUrl::parse(metadata_url)?;
            if !metadata
                .get("client_id_metadata_document_supported")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(error(
                    "OAuth client metadata unsupported; configure client ID",
                ));
            }
            metadata_url.clone()
        } else {
            self.register(&metadata, &redirect_uri).await?
        };
        let state_value = random_secret();
        let verifier = random_secret();
        let code_challenge = URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes())?);
        let scope = requested_scopes.join(" ");
        let query = form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("state", &state_value),
            ("code_challenge", &code_challenge),
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
        Ok((
            Attempt {
                generation,
                state: state_value,
                verifier,
                redirect: redirect_uri,
                expires: tokio::time::Instant::now() + ATTEMPT_TTL,
                issuer,
                endpoint: token_endpoint,
                client_id,
                scopes: requested_scopes,
                issuer_required: metadata
                    .get("authorization_response_iss_parameter_supported")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                revocation,
            },
            authorization_url,
        ))
    }

    /// Dynamic client registration as a public client. A server that insists
    /// on issuing a client secret needs a configured confidential client.
    async fn register(&self, metadata: &Value, redirect_uri: &str) -> Result<String, RpcError> {
        let registration = endpoint(metadata, "registration_endpoint")?;
        let body = serde_json::to_vec(&json!({
            "client_name": "BondSymphonic",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .map_err(|_| error("invalid OAuth registration"))?;
        let response = self
            .http_request(HttpRequest {
                method: "POST".into(),
                url: registration,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body,
            })
            .await?;
        if response.status != 201 && response.status != 200 {
            return Err(error("OAuth registration failed; configure client ID"));
        }
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|_| error("invalid OAuth registration"))?;
        if body.get("client_secret").is_some() {
            return Err(error(
                "OAuth registration requires configured confidential client",
            ));
        }
        Ok(body
            .get("client_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 2048)
            .ok_or_else(|| error("OAuth client ID missing"))?
            .to_owned())
    }

    /// Completes a login: the state must match, the issuer too (and be
    /// present if the server advertises it), within 10 minutes and with the
    /// server unchanged since the login began. A wrong state leaves the login
    /// pending; anything else ends it.
    pub async fn callback(&self, params: McpCallbackParams) -> Result<McpServerStatus, RpcError> {
        self.ensure_usable()?;
        let id = params.server_id.clone();
        let generation = self.generation(&id);
        let attempt = {
            let mut state = self.auth.lock();
            let attempt = state
                .attempts
                .get(&id)
                .ok_or_else(|| error("OAuth attempt missing"))?;
            if !constant_eq(&attempt.state, &params.state.0) {
                return Err(error("OAuth state mismatch"));
            }
            let issuer_ok = match params.issuer.as_deref() {
                Some(issuer) => issuer == attempt.issuer,
                None => !attempt.issuer_required,
            };
            let attempt = state.attempts.remove(&id).expect("checked above");
            if !issuer_ok {
                return Err(error("OAuth issuer mismatch"));
            }
            if tokio::time::Instant::now() >= attempt.expires || attempt.generation != generation {
                return Err(error("OAuth attempt expired or invalidated"));
            }
            attempt
        };
        self.status_changed();
        if params.code.0.is_empty() || params.code.0.len() > 8192 {
            return Err(error("invalid OAuth code"));
        }
        let result = self.finish_login(&id, &attempt, &params.code.0).await;
        if let Err(e) = &result {
            self.record_status(
                &id,
                attempt.generation,
                McpConnectionState::Error,
                Some(e.message.clone()),
            );
            return Err(e.clone());
        }
        self.clear_status(&id);
        Ok(self.status(&id))
    }

    async fn finish_login(
        &self,
        id: &McpServerId,
        attempt: &Attempt,
        code: &str,
    ) -> Result<(), RpcError> {
        let config = self
            .server(id)
            .ok_or_else(|| RpcError::not_found("MCP server not found"))?;
        let (resource, _) = remote(&config)?;
        let scope = attempt.scopes.join(" ");
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &attempt.client_id),
            ("redirect_uri", &attempt.redirect),
            ("code_verifier", &attempt.verifier),
            ("resource", resource),
            ("scope", &scope),
        ];
        let secret = self.client_secret(&config, attempt.generation)?;
        if let Some(secret) = &secret {
            fields.push(("client_secret", secret));
        }
        let mut token = self
            .exchange(
                &attempt.endpoint,
                &fields,
                &attempt.issuer,
                &attempt.client_id,
                &attempt.scopes,
            )
            .await?;
        token.revocation = attempt.revocation.clone();
        self.set_secret_at(
            id,
            attempt.generation,
            TOKEN_SECRET,
            Some(token.to_secret()?),
        )
        .map_err(|_| error("OAuth exchange invalidated"))
    }

    /// The configured client secret, read only while the server is still at
    /// `generation`: an edit that replaced it must never send the new value
    /// to the old issuer.
    fn client_secret(
        &self,
        config: &McpServerConfig,
        generation: u64,
    ) -> Result<Option<String>, RpcError> {
        let Ok((
            _,
            McpAuthConfig::OAuth {
                client_secret: Some(reference),
                ..
            },
        )) = remote(config)
        else {
            return Ok(None);
        };
        let (at, value) = self.secret_with_generation(&config.id, reference.as_str());
        if at != generation {
            return Err(error("credential update invalidated"));
        }
        Ok(Some(
            value
                .ok_or_else(|| error("OAuth client credential missing"))?
                .0,
        ))
    }

    async fn exchange(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
        issuer: &str,
        client_id: &str,
        scopes: &[String],
    ) -> Result<Token, RpcError> {
        let response = self
            .http_request(HttpRequest {
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
            access,
            refresh,
            expires,
            issuer: issuer.into(),
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            scopes: scopes.to_vec(),
            revocation: None,
        })
    }

    /// Ends the server's login at once and durably: the pending login, the
    /// token and anything in flight (a refresh, an exchange) are discarded.
    /// Then, best effort and bounded, the tokens are revoked upstream where
    /// the server advertises a usable revocation endpoint. For other remote
    /// servers it only forgets the last test result.
    pub async fn disconnect(&self, id: &McpServerId) -> Result<McpServerStatus, RpcError> {
        self.ensure_usable()?;
        let (generation, removed) = self.invalidate(id, TOKEN_SECRET)?;
        {
            let mut state = self.auth.lock();
            state.attempts.remove(id);
            state.statuses.remove(id);
        }
        self.status_changed();
        if let Some(token) = Token::parse(removed) {
            let _ = tokio::time::timeout(REVOKE_TIMEOUT, self.revoke(id, generation, &token)).await;
        }
        Ok(self.status(id))
    }

    async fn revoke(&self, id: &McpServerId, generation: u64, token: &Token) {
        let Some(revocation) = &token.revocation else {
            return;
        };
        if HttpsUrl::parse(&revocation.endpoint).is_err() {
            return;
        }
        let Some(config) = self.server(id) else {
            return;
        };
        let secret = match revocation.auth_method.as_str() {
            "none" => None,
            "client_secret_post" | "client_secret_basic" => {
                match self.client_secret(&config, generation) {
                    Ok(Some(secret)) => Some(secret),
                    _ => return,
                }
            }
            _ => return,
        };
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
            // No retries, and nothing upstream says is published.
            let _ = self
                .http_request(HttpRequest {
                    method: "POST".into(),
                    url: revocation.endpoint.clone(),
                    headers,
                    body: form(&fields).into_bytes(),
                })
                .await;
        }
    }

    /// The credential headers for one request to an enabled server,
    /// refreshing its OAuth token first when due (one refresh at a time per
    /// server). Rust-only: no RPC ever returns them.
    pub async fn auth_headers(&self, id: &McpServerId) -> Result<Vec<(String, String)>, RpcError> {
        self.auth_headers_for(id, false).await
    }

    pub(crate) async fn auth_headers_for(
        &self,
        id: &McpServerId,
        allow_disabled: bool,
    ) -> Result<Vec<(String, String)>, RpcError> {
        self.ensure_usable()?;
        let config = self.server(id).ok_or_else(|| error("server removed"))?;
        if !config.enabled && !allow_disabled {
            return Err(error("server disabled"));
        }
        let (_, auth) = remote(&config)?;
        let secret = |reference: &SecretRef| {
            self.secret(id, reference.as_str())
                .map(|v| v.0)
                .ok_or_else(|| error("credential missing"))
        };
        let headers = match auth {
            McpAuthConfig::None => vec![],
            McpAuthConfig::Bearer { token } => {
                vec![("Authorization".into(), format!("Bearer {}", secret(token)?))]
            }
            McpAuthConfig::Headers { headers } => headers
                .iter()
                .map(|(name, reference)| Ok((name.clone(), secret(reference)?)))
                .collect::<Result<_, RpcError>>()?,
            McpAuthConfig::OAuth { .. } => self.oauth_header(id, &config).await?,
        };
        validate_auth_headers(&headers)?;
        Ok(headers)
    }

    async fn oauth_header(
        &self,
        id: &McpServerId,
        config: &McpServerConfig,
    ) -> Result<Vec<(String, String)>, RpcError> {
        let lock = self
            .auth
            .lock()
            .refresh_locks
            .entry(id.clone())
            .or_default()
            .clone();
        let _single_flight = lock.lock().await;
        // Read after the lock: a refresh that just finished is reused.
        let (generation, stored) = self.secret_with_generation(id, TOKEN_SECRET);
        let token = Token::parse(stored).ok_or_else(|| error("not connected"))?;
        if token.expires > now() + REFRESH_MARGIN {
            return Ok(vec![(
                "Authorization".into(),
                format!("Bearer {}", token.access),
            )]);
        }
        let result = self.refresh(id, config, generation, token).await;
        match &result {
            Ok(_) => {
                let stale_error = self
                    .auth
                    .lock()
                    .statuses
                    .get(id)
                    .is_some_and(|(_, state, _)| *state == McpConnectionState::Error);
                if stale_error {
                    self.clear_status(id);
                } else {
                    self.status_changed();
                }
            }
            Err(e) if self.generation(id) == generation => self.record_status(
                id,
                generation,
                McpConnectionState::Error,
                Some(e.message.clone()),
            ),
            Err(_) => {}
        }
        result
    }

    async fn refresh(
        &self,
        id: &McpServerId,
        config: &McpServerConfig,
        generation: u64,
        token: Token,
    ) -> Result<Vec<(String, String)>, RpcError> {
        let refresh = token
            .refresh
            .clone()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| error("authentication_required"))?;
        let (resource, _) = remote(config)?;
        let scope = token.scopes.join(" ");
        let mut fields = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", token.client_id.as_str()),
            ("resource", resource),
            ("scope", scope.as_str()),
        ];
        let secret = self.client_secret(config, generation)?;
        if let Some(secret) = &secret {
            fields.push(("client_secret", secret));
        }
        let mut replacement = self
            .exchange(
                &token.endpoint,
                &fields,
                &token.issuer,
                &token.client_id,
                &token.scopes,
            )
            .await?;
        if replacement.refresh.is_none() {
            replacement.refresh = token.refresh;
        }
        replacement.revocation = token.revocation;
        // Stored only if nothing (disconnect, edit, removal) happened since.
        self.set_secret_at(id, generation, TOKEN_SECRET, Some(replacement.to_secret()?))
            .map_err(|_| error("OAuth refresh invalidated"))?;
        Ok(vec![(
            "Authorization".into(),
            format!("Bearer {}", replacement.access),
        )])
    }
}

fn revocation_metadata(metadata: &Value, config: &McpServerConfig) -> Option<Revocation> {
    let endpoint = endpoint(metadata, "revocation_endpoint").ok()?;
    let confidential = matches!(
        remote(config),
        Ok((
            _,
            McpAuthConfig::OAuth {
                client_secret: Some(_),
                ..
            }
        ))
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

/// A discovered endpoint, admitted by the same URL policy as the server.
fn endpoint(metadata: &Value, name: &str) -> Result<String, RpcError> {
    let url = metadata
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| error("OAuth endpoint missing; configure client ID"))?;
    HttpsUrl::parse(url)?;
    Ok(url.into())
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

/// The IDE's callback: `http://` to a loopback host with an explicit port.
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
