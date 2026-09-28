//! `system.list_models`: fetching the Anthropic Models API with whichever of
//! the agent's own Claude credentials the daemon has, so the IDE's model
//! dropdowns can be filled with the newest model per family instead of a list
//! baked into a build.
//!
//! Neither the daemon nor the IDE carries an HTTP client (no reqwest, no TLS
//! stack to keep current), and the distro already has `curl`, so a fetch is one
//! child process. The one thing that process must never do is put a credential
//! where a process listing on the host could read it, which is why the
//! request's headers travel on curl's stdin (`-K -`) rather than its argv.
//!
//! The pieces are kept separate and mostly pure so each can be tested without
//! a network: [`parse_models`] on a fixture, [`choose_credential`] on plain
//! values, [`curl_config`] and [`curl_argv`] on their output, and the cache in
//! [`ModelsCache`] against an injected [`Fetcher`] stub.

use crate::daemon::Daemon;
use bondsymphonic_proto::{ErrorCode, ModelInfo, RpcError};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The endpoint `system.list_models` fetches. `limit=100` is comfortably past
/// how many models Anthropic has ever listed at once; there is no pagination
/// here because [`parse_models`] only ever reads the first page.
const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=100";

/// The API version every request to `api.anthropic.com` is pinned to, the same
/// value the agent adapter's own requests use.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The beta flag that lets an OAuth token (a long-lived token or a login's
/// access token) authenticate a plain API call instead of only the CLI.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How long a successful answer is trusted before the next `system.list_models`
/// asks Anthropic again, rather than spawning `curl` on every keystroke a model
/// dropdown might trigger.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// How long one `curl` gets. A model list is a small, local-ish GET; this is
/// generous for a slow link and still short enough that a hung request does
/// not hold a `system.list_models` call open indefinitely.
const CURL_TIMEOUT: Duration = Duration::from_secs(20);

// ---- what a model looks like on the wire, trimmed to what the IDE needs ----

/// Reads `data[]` out of a Models API response, keeping `id`, `display_name`
/// (falling back to `id` when the API omits it) and `created_at`, in the order
/// the API returned them -- newest first, which is the order the model
/// dropdown wants.
///
/// An entry with no `id` is skipped rather than failing the whole answer: it
/// is not a model the IDE could ever start an agent with. Every other unknown
/// field, on an entry or on the envelope, is ignored -- the API can add fields
/// freely without this breaking.
pub fn parse_models(json: &[u8]) -> Result<Vec<ModelInfo>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("not a JSON response: {e}"))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "no \"data\" array in the response".to_string())?;
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let display_name = m
                .get("display_name")
                .and_then(|d| d.as_str())
                .unwrap_or(id.as_str())
                .to_owned();
            let created_at = m
                .get("created_at")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_owned();
            Some(ModelInfo {
                id,
                display_name,
                created_at,
            })
        })
        .collect())
}

/// The API's own error shape, `{"type":"error","error":{"type":...,
/// "message":...}}`, read out of a failed response's body when there is one to
/// read -- `curl --fail-with-body` still writes it even on a non-2xx status.
fn api_error_message(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("error")?.get("message")?.as_str().map(str::to_owned)
}

// ---- which credential a fetch authenticates with ----

/// Which of the daemon's Claude credentials answered a fetch. Kept as the
/// cache key ([`ModelsCache`]) because an API key and the long-lived token or
/// login need not be authorised for the same models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    ApiKey,
    Token,
    Login,
}

impl CredentialKind {
    /// The word the info log names this credential by, matching the daemon
    /// design's "listed N models via <api_key|long-lived token|login>".
    fn label(self) -> &'static str {
        match self {
            CredentialKind::ApiKey => "api_key",
            CredentialKind::Token => "long-lived token",
            CredentialKind::Login => "login",
        }
    }
}

/// What one fetch authenticates with: which kind it is, and the secret value
/// itself. The secret is deliberately the only field -- there is no `Debug`
/// derive here, so a stray `{:?}` of a `Credential` fails to compile rather
/// than printing it.
pub struct Credential {
    pub kind: CredentialKind,
    secret: String,
}

/// The credential a fetch should use, first that exists:
///
/// 1. `api_key`, when the caller (an `agent.start`-style explicit choice)
///    sent a non-empty one;
/// 2. `token`, the long-lived token, when one is stored;
/// 3. `login`, the daemon user's own Claude Code login's access token, when
///    its `expiresAt` (ms epoch) is still in the future;
/// 4. `None`.
///
/// Pure, so the ordering is pinned by a plain unit test rather than by a test
/// that has to arrange a token file, a credentials file and the clock all at
/// once. The I/O that produces `token` and `login` lives in
/// [`resolve_credential`]; this function never reads anything.
pub fn choose_credential(
    api_key: Option<&str>,
    token: Option<String>,
    login: Option<(String, i64)>,
    now_ms: i64,
) -> Option<Credential> {
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        return Some(Credential {
            kind: CredentialKind::ApiKey,
            secret: key.to_owned(),
        });
    }
    if let Some(t) = token {
        return Some(Credential {
            kind: CredentialKind::Token,
            secret: t,
        });
    }
    if let Some((access_token, expires_at)) = login {
        if expires_at > now_ms && !access_token.is_empty() {
            return Some(Credential {
                kind: CredentialKind::Login,
                secret: access_token,
            });
        }
    }
    None
}

/// The daemon user's own Claude Code login, as [`choose_credential`]'s third
/// preference needs it: the access token and its `expiresAt`, or `None` when
/// there is no credentials file, it does not parse, or it has no access token.
///
/// Read directly rather than through `agents::credentials`, whose `Login`
/// struct is private to that module and carries fields (the refresh token,
/// the write-back comparison) this has no use for.
fn read_host_login() -> Option<(String, i64)> {
    let path = crate::setup::host_home().join(".claude/.credentials.json");
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let oauth = v.get("claudeAiOauth")?.as_object()?;
    let access_token = oauth.get("accessToken")?.as_str()?.to_owned();
    let expires_at = oauth.get("expiresAt")?.as_i64()?;
    Some((access_token, expires_at))
}

/// [`choose_credential`] with its inputs actually read: the long-lived token
/// off disk (through [`crate::agents::token::read`], on the blocking pool the
/// same way `AgentManager::start` reads it) and the host login's access token,
/// which is only worth reading when neither of the two credentials ahead of it
/// in the order is already in hand.
async fn resolve_credential(d: &Daemon, api_key: Option<&str>) -> Option<Credential> {
    let has_key = api_key.filter(|k| !k.is_empty()).is_some();
    let token = if has_key {
        None
    } else {
        let root = d.dirs.root.clone();
        tokio::task::spawn_blocking(move || {
            crate::agents::token::read(&crate::agents::token::token_path(&root))
        })
        .await
        .unwrap_or(None)
    };
    let login = if has_key || token.is_some() {
        None
    } else {
        tokio::task::spawn_blocking(read_host_login)
            .await
            .unwrap_or(None)
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    choose_credential(api_key, token, login, now_ms)
}

// ---- curl: the config on stdin, the argv with nothing to hide ----

/// A secret is refused rather than put in the `-K` config when it carries a
/// character that would let it escape the quoted string curl reads it from --
/// a quote, a backslash, or a line ending that could start a second config
/// line of its own. Every credential this daemon issues or reads (the token's
/// fixed alphabet, an API key, an OAuth access token) is free of all four; this
/// exists only so a value that somehow was not is refused rather than smuggled
/// into curl's own option parsing.
fn curl_safe(secret: &str) -> bool {
    !secret.contains(['"', '\\', '\n', '\r'])
}

/// The config `curl -K -` reads off stdin for one fetch: the URL, the fixed
/// `anthropic-version` header, and the header(s) `cred`'s kind calls for. Never
/// the argv -- see [`curl_argv`] -- so nothing here shows up in a process
/// listing.
fn curl_config(cred: &Credential) -> Result<String, String> {
    if !curl_safe(&cred.secret) {
        return Err("credential contains a character curl's config cannot carry".to_owned());
    }
    let mut cfg =
        format!("url = \"{MODELS_URL}\"\nheader = \"anthropic-version: {ANTHROPIC_VERSION}\"\n");
    match cred.kind {
        CredentialKind::ApiKey => {
            cfg.push_str(&format!("header = \"x-api-key: {}\"\n", cred.secret));
        }
        CredentialKind::Token | CredentialKind::Login => {
            cfg.push_str(&format!(
                "header = \"Authorization: Bearer {}\"\nheader = \"anthropic-beta: {OAUTH_BETA}\"\n",
                cred.secret
            ));
        }
    }
    Ok(cfg)
}

/// `curl`'s fixed argv. `-K -` is what makes every header (and so every
/// credential) arrive on stdin instead: nothing here is, or will become,
/// per-request, so nothing here can carry a secret. `--fail-with-body` is what
/// makes a non-2xx response still write the API's own JSON error out for
/// [`api_error_message`] to read, rather than curl swallowing the body and
/// leaving only an exit code.
fn curl_argv() -> [&'static str; 6] {
    ["-sS", "--fail-with-body", "-m", "15", "-K", "-"]
}

/// The `curl` binary to run: the distro's own copy, so this never resolves to
/// a Windows build the way a bare `claude` can under WSL (see
/// `agents::claude::pinned_claude_path`), falling back to whatever `curl` the
/// daemon's own `PATH` finds for a host where `/usr/bin/curl` is not it.
fn curl_bin() -> &'static str {
    if std::path::Path::new("/usr/bin/curl").is_file() {
        "/usr/bin/curl"
    } else {
        "curl"
    }
}

/// One fetch's raw response bytes, or the message to report -- curl's stderr,
/// or the API's own error message when the body carried one, but never the
/// credential that was sent.
///
/// Spawned with the config written to its stdin from a task of its own: `curl`
/// does not start reading the body of a POST-shaped `-K` config until its
/// output pipe has somewhere to go, and writing on the same task that then
/// waits for output would deadlock if the two pipes filled at once. This
/// request's config is a few hundred bytes, far under any pipe's buffer, so in
/// practice the write finishes long before curl has anything to say; the
/// separate task is what makes that true by construction instead of by luck.
async fn spawn_curl(config: String) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncWriteExt;
    let bin = curl_bin();
    let mut child = tokio::process::Command::new(bin)
        .args(curl_argv())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin was requested as piped");
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(config.as_bytes()).await;
        drop(stdin);
    });
    let output = tokio::time::timeout(CURL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("{bin} timed out after {}s", CURL_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())?;
    let _ = feed.await;
    if !output.status.success() {
        let message = api_error_message(&output.stdout).unwrap_or_else(|| {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                format!("{bin} exited with {}", output.status)
            } else {
                stderr
            }
        });
        return Err(message);
    }
    Ok(output.stdout)
}

// ---- the cache, and one fetch through it ----

/// One fetch of the Models API's raw bytes, given the `-K` config to send.
/// Real code passes [`spawn_curl`]; tests pass a stub that records how many
/// times it was called and returns a fixture, so the cache's "does not spawn
/// again within the hour" behaviour is provable without a network.
pub type Fetcher = Arc<dyn Fn(String) -> BoxFuture<'static, Result<Vec<u8>, String>> + Send + Sync>;

/// The last successful answer per credential kind. In memory only -- a daemon
/// restart starts the cache cold, which is fine, since the whole point is to
/// spare the network inside one daemon's run, not to persist an answer past
/// it. A failed fetch is never stored: `system.list_models` asked again a
/// second later should not just replay the same error for an hour.
#[derive(Default)]
pub struct ModelsCache {
    entries: Mutex<HashMap<CredentialKind, (Instant, Vec<ModelInfo>)>>,
}

impl ModelsCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn hit(&self, kind: CredentialKind) -> Option<Vec<ModelInfo>> {
        let entries = self.entries.lock();
        let (at, models) = entries.get(&kind)?;
        (at.elapsed() < CACHE_TTL).then(|| models.clone())
    }

    fn store(&self, kind: CredentialKind, models: Vec<ModelInfo>) {
        self.entries.lock().insert(kind, (Instant::now(), models));
    }
}

/// The cached models for `cred`'s kind, or a fresh fetch through `fetch` --
/// stored in `cache` on success, left uncached on failure. Split from
/// [`list_models`] so a test can drive it directly against a stub fetcher and
/// a bare `ModelsCache`, with no `Daemon` to build.
pub async fn list_models_with(
    cache: &ModelsCache,
    cred: &Credential,
    fetch: &Fetcher,
) -> Result<Vec<ModelInfo>, String> {
    if let Some(models) = cache.hit(cred.kind) {
        return Ok(models);
    }
    let config = curl_config(cred)?;
    let bytes = fetch(config).await?;
    let models = parse_models(&bytes)?;
    cache.store(cred.kind, models.clone());
    Ok(models)
}

/// `system.list_models`'s handler: resolves the credential to fetch with,
/// answers from the cache or a real `curl`, and logs which credential kind
/// answered -- never the credential itself.
pub async fn list_models(
    d: &Daemon,
    api_key: Option<&str>,
) -> Result<bondsymphonic_proto::ListModelsResult, RpcError> {
    let cred = resolve_credential(d, api_key).await.ok_or_else(|| {
        RpcError::new(
            ErrorCode::PrereqMissing,
            "no Claude credentials to list models with",
        )
    })?;
    let kind = cred.kind;
    let fetch: Fetcher = Arc::new(|config| Box::pin(spawn_curl(config)));
    let models = list_models_with(&d.models, &cred, &fetch)
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, e))?;
    tracing::info!("listed {} models via {}", models.len(), kind.label());
    Ok(bondsymphonic_proto::ListModelsResult { models })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, display_name: &str, created_at: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: display_name.into(),
            created_at: created_at.into(),
        }
    }

    /// The fixture shaped like the real response in the brief: an unknown
    /// top-level field, an unknown per-model field, and one entry missing
    /// `display_name` -- which falls back to `id` rather than failing the
    /// whole parse.
    #[test]
    fn parse_models_reads_the_fields_it_needs_and_ignores_the_rest() {
        let body = br#"{
            "data": [
                {"type":"model","id":"claude-opus-5-5","display_name":"Claude Opus 5.5",
                 "created_at":"2026-09-21T16:24:00Z","context_window":200000},
                {"type":"model","id":"claude-haiku-5","created_at":"2026-08-01T00:00:00Z"}
            ],
            "has_more": false,
            "first_id": "claude-opus-5-5",
            "last_id": "claude-haiku-5"
        }"#;
        let models = parse_models(body).unwrap();
        assert_eq!(
            models,
            vec![
                model("claude-opus-5-5", "Claude Opus 5.5", "2026-09-21T16:24:00Z"),
                model("claude-haiku-5", "claude-haiku-5", "2026-08-01T00:00:00Z"),
            ]
        );
    }

    /// An entry with no `id` names no model the IDE could ever start an agent
    /// with, so it is dropped rather than failing every other entry in the
    /// same response.
    #[test]
    fn an_entry_without_an_id_is_skipped_not_fatal() {
        let body = br#"{"data": [{"display_name":"no id"}, {"id":"claude-x"}]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models, vec![model("claude-x", "claude-x", "")]);
    }

    #[test]
    fn a_response_with_no_data_array_is_an_error_not_an_empty_list() {
        assert!(parse_models(b"{}").is_err());
        assert!(parse_models(b"not json").is_err());
    }

    // ---- credential choice ----

    const KEY: &str = "sk-ant-api03-key";
    const TOKEN: &str = "sk-ant-oat01-token";
    const LOGIN: &str = "sk-ant-oat01-login-access";
    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn an_api_key_wins_over_everything_else() {
        let cred = choose_credential(
            Some(KEY),
            Some(TOKEN.into()),
            Some((LOGIN.into(), NOW + 1)),
            NOW,
        )
        .unwrap();
        assert_eq!(cred.kind, CredentialKind::ApiKey);
        assert_eq!(cred.secret, KEY);
    }

    #[test]
    fn a_blank_api_key_falls_through_to_the_token() {
        let cred = choose_credential(Some(""), Some(TOKEN.into()), None, NOW).unwrap();
        assert_eq!(cred.kind, CredentialKind::Token);
        assert_eq!(cred.secret, TOKEN);
    }

    #[test]
    fn no_key_and_no_token_falls_through_to_an_unexpired_login() {
        let cred = choose_credential(None, None, Some((LOGIN.into(), NOW + 1)), NOW).unwrap();
        assert_eq!(cred.kind, CredentialKind::Login);
        assert_eq!(cred.secret, LOGIN);
    }

    #[test]
    fn an_expired_login_is_no_credential_at_all() {
        assert!(choose_credential(None, None, Some((LOGIN.into(), NOW - 1)), NOW).is_none());
        assert!(choose_credential(None, None, Some((LOGIN.into(), NOW)), NOW).is_none());
        assert!(choose_credential(None, None, None, NOW).is_none());
    }

    // ---- curl config and argv ----

    #[test]
    fn the_config_carries_the_api_key_header_and_no_oauth_beta() {
        let cred = Credential {
            kind: CredentialKind::ApiKey,
            secret: KEY.into(),
        };
        let cfg = curl_config(&cred).unwrap();
        assert!(
            cfg.contains(&format!("header = \"x-api-key: {KEY}\"")),
            "{cfg}"
        );
        assert!(!cfg.contains("Authorization"), "{cfg}");
        assert!(!cfg.contains(OAUTH_BETA), "{cfg}");
        assert!(cfg.contains(MODELS_URL), "{cfg}");
    }

    #[test]
    fn the_config_carries_the_bearer_and_oauth_beta_for_token_and_login() {
        for kind in [CredentialKind::Token, CredentialKind::Login] {
            let cred = Credential {
                kind,
                secret: TOKEN.into(),
            };
            let cfg = curl_config(&cred).unwrap();
            assert!(
                cfg.contains(&format!("header = \"Authorization: Bearer {TOKEN}\"")),
                "{kind:?}: {cfg}"
            );
            assert!(
                cfg.contains(&format!("anthropic-beta: {OAUTH_BETA}")),
                "{kind:?}: {cfg}"
            );
            assert!(!cfg.contains("x-api-key"), "{kind:?}: {cfg}");
        }
    }

    /// The one thing that must never be true of the argv: a credential on it,
    /// where a process listing on the host could read it. The argv is fixed
    /// and takes no credential as input at all, so this is really a check that
    /// nobody adds one later.
    #[test]
    fn the_argv_carries_no_credential() {
        let argv = curl_argv();
        for secret in [KEY, TOKEN, LOGIN] {
            assert!(
                !argv.iter().any(|a| a.contains(secret)),
                "argv leaked a credential: {argv:?}"
            );
        }
        assert_eq!(argv, ["-sS", "--fail-with-body", "-m", "15", "-K", "-"]);
    }

    #[test]
    fn a_secret_that_could_escape_the_quoted_config_is_refused() {
        for bad in ["a\"b", "a\\b", "a\nb", "a\rb"] {
            let cred = Credential {
                kind: CredentialKind::ApiKey,
                secret: bad.into(),
            };
            assert!(curl_config(&cred).is_err(), "{bad:?}");
        }
    }

    // ---- the cache ----

    fn counting_fetcher(
        result: Result<Vec<u8>, String>,
    ) -> (Fetcher, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let fetch: Fetcher = Arc::new(move |_config| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = result.clone();
            Box::pin(async move { result })
        });
        (fetch, calls)
    }

    #[tokio::test]
    async fn a_second_call_within_the_hour_does_not_fetch_again() {
        let body = br#"{"data":[{"id":"claude-x","display_name":"X","created_at":"t"}]}"#.to_vec();
        let (fetch, calls) = counting_fetcher(Ok(body));
        let cache = ModelsCache::new();
        let cred = Credential {
            kind: CredentialKind::Token,
            secret: TOKEN.into(),
        };

        let first = list_models_with(&cache, &cred, &fetch).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let second = list_models_with(&cache, &cred, &fetch).await.unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cached answer must not spawn a second fetch"
        );
        assert_eq!(first, second);
    }

    /// A different credential kind is a different cache entry: an API key and
    /// the long-lived token need not be authorised for the same models, so one
    /// being cached must not answer for the other.
    #[tokio::test]
    async fn different_credential_kinds_are_cached_separately() {
        let body = br#"{"data":[{"id":"claude-x","display_name":"X","created_at":"t"}]}"#.to_vec();
        let (fetch, calls) = counting_fetcher(Ok(body));
        let cache = ModelsCache::new();
        let token_cred = Credential {
            kind: CredentialKind::Token,
            secret: TOKEN.into(),
        };
        let key_cred = Credential {
            kind: CredentialKind::ApiKey,
            secret: KEY.into(),
        };

        list_models_with(&cache, &token_cred, &fetch).await.unwrap();
        list_models_with(&cache, &key_cred, &fetch).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A failed fetch is never cached: the next call must try again rather
    /// than replaying the same error for an hour.
    #[tokio::test]
    async fn a_failed_fetch_is_not_cached() {
        let (fetch, calls) = counting_fetcher(Err("boom".into()));
        let cache = ModelsCache::new();
        let cred = Credential {
            kind: CredentialKind::Token,
            secret: TOKEN.into(),
        };

        assert!(list_models_with(&cache, &cred, &fetch).await.is_err());
        assert!(list_models_with(&cache, &cred, &fetch).await.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// The API's own error message, when the body carried one, is what a
    /// caller sees -- never the credential, which never appears in the body at
    /// all.
    #[test]
    fn the_apis_own_error_message_is_read_off_a_failed_bodys_json() {
        let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            api_error_message(body).as_deref(),
            Some("invalid x-api-key")
        );
        assert_eq!(api_error_message(b"not json"), None);
        assert_eq!(api_error_message(b"{}"), None);
    }
}
