//! Settings' list of OpenCode model servers, and its "Test connection".
//!
//! The question the test answers is "will a sandboxed OpenCode agent reach
//! this server?". It is asked from the daemon rather than from inside a
//! sandbox, which needs no running agent and no workspace, and it gives the
//! same answer because every step is the one the agent's traffic takes:
//!
//! - the origins granted are [`provider_origins`] of the same mirrored config
//!   the companion sandbox is granted from;
//! - a loopback origin is carried by whichever in-sandbox tunnel
//!   [`tunnel_listens`] gives the address the agent would dial, and none at
//!   all when it gives none (the proxy's own port);
//! - the origin is resolved, admitted and connected by the proxy's own
//!   [`resolve_upstream`], [`reachable_addrs`] and [`connect_upstream`], in
//!   the daemon process, which is where the proxy runs;
//! - then `GET <baseURL>/models`, the OpenAI-compatible model list, with the
//!   key the agent would send.
//!
//! [`tunnel_listens`]: crate::workspace::agent_sandboxes::tunnel_listens

use super::opencode_config::{provider_origins, read_regular_file, strip_jsonc, CONFIGS};
use crate::net::allowlist::{Allowlist, Origin};
use crate::net::proxy::{connect_upstream, reachable_addrs, resolve_upstream, UpstreamError};
use bondsymphonic_proto::{
    OpenCodeProvider, OpenCodeProviderTest, OpenCodeProvidersResult, ProviderTestOutcome,
    SandboxRoute,
};
use serde_json::{Map, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long the server has to answer once connected.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of an answer is read. A model list is a few kilobytes.
const BODY_LIMIT: usize = 1024 * 1024;

/// How many model ids a test result names.
const SHOWN_MODELS: usize = 5;

/// One provider of the mirrored config, merged across its files.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredProvider {
    pub id: String,
    pub name: Option<String>,
    /// `options.baseURL`, else `api`: what OpenCode itself sends requests to.
    pub base_url: Option<String>,
    pub model_count: u32,
    /// `options.apiKey` as written. Never logged and never returned.
    api_key: Option<String>,
}

/// The providers in the mirrored config under `config_home` (an
/// `XDG_CONFIG_HOME`), sorted by id, and a line per file that could not be
/// used. Files are merged in the order OpenCode reads them, a later one's
/// fields over an earlier one's.
pub fn configured_providers(config_home: &Path) -> (Vec<ConfiguredProvider>, Vec<String>) {
    let dir = config_home.join("opencode");
    let mut merged = Map::new();
    let mut warnings = Vec::new();
    for name in CONFIGS {
        let path = dir.join(name);
        let bytes = match read_regular_file(&path, super::opencode_config::CONFIG_READ_LIMIT, false)
        {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{name} could not be read: {e}"));
                continue;
            }
        };
        let text = strip_jsonc(&String::from_utf8_lossy(&bytes));
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(e) => {
                warnings.push(format!("{name} is not valid JSON: {e}"));
                continue;
            }
        };
        if let Some(providers) = value.get("provider").and_then(Value::as_object) {
            for (id, provider) in providers {
                let slot = merged.entry(id.clone()).or_insert(Value::Null);
                merge(slot, provider);
            }
        }
    }
    let mut out: Vec<ConfiguredProvider> = merged
        .into_iter()
        .map(|(id, p)| {
            let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
            ConfiguredProvider {
                name: text(p.get("name")),
                base_url: text(p.pointer("/options/baseURL")).or_else(|| text(p.get("api"))),
                model_count: p
                    .get("models")
                    .and_then(Value::as_object)
                    .map_or(0, |m| m.len() as u32),
                api_key: text(p.pointer("/options/apiKey")),
                id,
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    (out, warnings)
}

/// `over` merged into `into`: objects field by field, anything else replaced.
fn merge(into: &mut Value, over: &Value) {
    match (into, over) {
        (Value::Object(into), Value::Object(over)) => {
            for (k, v) in over {
                merge(into.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (into, over) => *into = over.clone(),
    }
}

/// How a sandboxed agent's request to `base_url` would leave the sandbox,
/// given the origins its companion is granted.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub kind: SandboxRoute,
    pub note: Option<String>,
    /// The origin the proxy is asked for: the URL's own, or for a loopback
    /// URL the origin of the tunnel that listens where the agent dials.
    pub target: Option<Origin>,
}

pub fn route(base_url: Option<&str>, grants: &[Origin]) -> Route {
    let none = |kind, note: &str| Route {
        kind,
        note: Some(note.to_owned()),
        target: None,
    };
    let Some(url) = base_url else {
        return none(
            SandboxRoute::NoBaseUrl,
            "No baseURL: OpenCode uses the provider's built-in endpoint, which the workspace \
             network allowlist decides about.",
        );
    };
    let Some(origin) = Origin::from_url(url) else {
        return none(
            SandboxRoute::NotGranted,
            "Not a literal http:// or https:// URL ({env:...} is not followed), so sandboxed \
             agents get no route to it.",
        );
    };
    if !grants.contains(&origin) {
        return none(
            SandboxRoute::NotGranted,
            "This origin is not among those granted from the OpenCode config.",
        );
    }
    if !origin.is_loopback() {
        return Route {
            kind: SandboxRoute::Granted,
            note: None,
            target: Some(origin),
        };
    }
    // Where the agent dials it inside the sandbox, and so which tunnel - if
    // any - answers there.
    let port = origin.port();
    let dialled: Vec<String> = if origin.host() == "localhost" {
        vec![format!("127.0.0.1:{port}"), format!("[::1]:{port}")]
    } else {
        vec![origin.authority()]
    };
    // Checked first: `localhost:3128` still gets a tunnel on `[::1]`, but an
    // agent dialling `127.0.0.1` reaches the proxy instead.
    if dialled
        .iter()
        .any(|a| a == crate::workspace::lifecycle::PROXY_LISTEN)
    {
        return none(
            SandboxRoute::NotGranted,
            "Port 3128 on loopback is the sandbox's own proxy; run the server on another port.",
        );
    }
    let listens = crate::workspace::agent_sandboxes::tunnel_listens(grants);
    let tunnel = dialled.iter().find_map(|addr| {
        listens
            .iter()
            .find(|(_, addrs)| addrs.contains(addr))
            .map(|(o, _)| o.clone())
    });
    match tunnel {
        Some(target) => Route {
            kind: SandboxRoute::Tunnelled,
            note: Some(format!(
                "Loopback: carried from the sandbox to {} in the WSL distribution.",
                target.authority()
            )),
            target: Some(target),
        },
        None => none(
            SandboxRoute::NotGranted,
            "Another provider's loopback tunnel already listens on this address in the sandbox.",
        ),
    }
}

/// `opencode.list_providers`, from the mirrored config under `config_home`.
pub fn list_providers(config_home: &Path) -> OpenCodeProvidersResult {
    let (providers, warnings) = configured_providers(config_home);
    let grants = provider_origins(config_home);
    OpenCodeProvidersResult {
        providers: providers
            .into_iter()
            .map(|p| {
                let route = route(p.base_url.as_deref(), &grants);
                OpenCodeProvider {
                    id: p.id,
                    name: p.name,
                    base_url: p.base_url,
                    model_count: p.model_count,
                    sandbox_route: route.kind,
                    route_note: route.note,
                }
            })
            .collect(),
        warnings,
    }
}

/// The mirror refreshed from the Windows user's config, as an agent start
/// refreshes it, or why it could not be.
async fn refresh(root: &Path) -> Option<String> {
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || super::opencode_backend::ensure_native_home(&root))
        .await
    {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("The OpenCode config could not be refreshed: {e}")),
        Err(e) => Some(format!("The OpenCode config could not be refreshed: {e}")),
    }
}

/// `opencode.list_providers` for the daemon whose data root is `root`. A
/// failed refresh lists the last mirror and says why.
pub async fn handle_list(root: &Path) -> OpenCodeProvidersResult {
    let failed = refresh(root).await;
    let home = super::opencode_backend::opencode_home(root).join("config");
    let mut result = tokio::task::spawn_blocking(move || list_providers(&home))
        .await
        .unwrap_or(OpenCodeProvidersResult {
            providers: Vec::new(),
            warnings: Vec::new(),
        });
    result.warnings.extend(failed);
    result
}

/// `opencode.test_provider` for the daemon whose data root is `root`.
pub async fn handle_test(
    root: &Path,
    provider_id: &str,
    stored_key: Option<&str>,
) -> OpenCodeProviderTest {
    let failed = refresh(root).await;
    let home = super::opencode_backend::opencode_home(root);
    let mut result = test_provider(
        &home.join("config"),
        &home.join("data/opencode/auth.json"),
        provider_id,
        stored_key,
    )
    .await;
    if let Some(failed) = failed {
        result.message = format!("{} ({failed}; tested the last copy.)", result.message);
    }
    result
}

/// The key a sandboxed agent would send, and where it came from.
struct Key {
    value: Option<String>,
    source: Option<&'static str>,
    /// Said when the config names a key the agent will not have.
    note: Option<String>,
}

/// What a sandboxed agent authenticates with: `options.apiKey` when it is
/// literal, or `{env:OPENAI_API_KEY}` - the one variable the sandbox is given,
/// from the key stored in the IDE; otherwise the OpenCode sign-in's API key
/// for the provider; otherwise, for `openai`, the stored key.
fn choose_key(p: &ConfiguredProvider, auth_json: &Path, stored: Option<&str>) -> Key {
    let stored = stored.map(str::trim).filter(|k| !k.is_empty());
    if let Some(configured) = p.api_key.as_deref() {
        if let Some(var) = configured
            .strip_prefix("{env:")
            .and_then(|r| r.strip_suffix('}'))
        {
            return match (var, stored) {
                ("OPENAI_API_KEY", Some(key)) => Key {
                    value: Some(key.to_owned()),
                    source: Some("the OpenCode key stored in Settings"),
                    note: None,
                },
                _ => Key {
                    value: None,
                    source: None,
                    note: Some(format!(
                        "The config takes the key from {{env:{var}}}, which sandboxed agents do \
                         not have{}.",
                        if var == "OPENAI_API_KEY" {
                            " until a key is stored in Settings"
                        } else {
                            ""
                        }
                    )),
                },
            };
        }
        if configured.starts_with('{') {
            return Key {
                value: None,
                source: None,
                note: Some(
                    "The config's apiKey is a substitution this test does not follow.".into(),
                ),
            };
        }
        return Key {
            value: Some(configured.to_owned()),
            source: Some("the config's apiKey"),
            note: None,
        };
    }
    let login = read_regular_file(auth_json, 1024 * 1024, false)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| {
            let entry = v.get(&p.id)?;
            (entry.get("type")?.as_str()? == "api")
                .then(|| entry.get("key")?.as_str().map(str::to_owned))
                .flatten()
        });
    if let Some(key) = login {
        return Key {
            value: Some(key),
            source: Some("the OpenCode sign-in"),
            note: None,
        };
    }
    if p.id == "openai" {
        if let Some(key) = stored {
            return Key {
                value: Some(key.to_owned()),
                source: Some("the OpenCode key stored in Settings"),
                note: None,
            };
        }
    }
    Key {
        value: None,
        source: None,
        note: None,
    }
}

/// `<baseURL>/models`.
pub fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim().trim_end_matches('/'))
}

/// `opencode.test_provider`. `auth_json` is the host's OpenCode login, which
/// each agent start copies into its sandbox; `stored_key` is the IDE's.
pub async fn test_provider(
    config_home: &Path,
    auth_json: &Path,
    provider_id: &str,
    stored_key: Option<&str>,
) -> OpenCodeProviderTest {
    test_provider_with(
        config_home,
        auth_json,
        provider_id,
        stored_key,
        RESPONSE_TIMEOUT,
    )
    .await
}

/// [`test_provider`] with the answer's deadline given, so a test of a server
/// that never answers does not wait the full five seconds.
pub async fn test_provider_with(
    config_home: &Path,
    auth_json: &Path,
    provider_id: &str,
    stored_key: Option<&str>,
    response_timeout: Duration,
) -> OpenCodeProviderTest {
    let started = Instant::now();
    let mut result = probe(
        config_home,
        auth_json,
        provider_id,
        stored_key,
        response_timeout,
    )
    .await;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    result
}

fn answer(outcome: ProviderTestOutcome, message: impl Into<String>) -> OpenCodeProviderTest {
    OpenCodeProviderTest {
        outcome,
        message: message.into(),
        url: None,
        status: None,
        model_count: None,
        model_ids: Vec::new(),
        key_source: None,
        elapsed_ms: 0,
    }
}

const WINDOWS_HINT: &str = "A server running on Windows is reached through localhost only with \
WSL mirrored networking; otherwise put the Windows host's address in the baseURL (see the user \
guide's \"Local and LAN model servers\").";

async fn probe(
    config_home: &Path,
    auth_json: &Path,
    provider_id: &str,
    stored_key: Option<&str>,
    response_timeout: Duration,
) -> OpenCodeProviderTest {
    let (providers, _) = configured_providers(config_home);
    let Some(provider) = providers.into_iter().find(|p| p.id == provider_id) else {
        return answer(
            ProviderTestOutcome::Failed,
            format!("There is no provider \"{provider_id}\" in the OpenCode config any more."),
        );
    };
    let grants = provider_origins(config_home);
    let route = route(provider.base_url.as_deref(), &grants);
    let (Some(target), Some(base_url)) = (route.target.clone(), provider.base_url.clone()) else {
        let outcome = if route.kind == SandboxRoute::NoBaseUrl {
            ProviderTestOutcome::Failed
        } else {
            ProviderTestOutcome::NotGranted
        };
        let note = route.note.unwrap_or_default();
        return answer(
            outcome,
            if outcome == ProviderTestOutcome::NotGranted {
                format!("Not reachable from sandboxes. {note}")
            } else {
                format!("Nothing to test. {note}")
            },
        );
    };
    let url = models_url(&base_url);
    let key = choose_key(&provider, auth_json, stored_key);
    let mut out = test_url(&url, &target, key.value.as_deref(), response_timeout).await;
    out.url = Some(url);
    out.key_source = key.value.as_ref().and(key.source).map(str::to_owned);
    match out.outcome {
        ProviderTestOutcome::HttpError if matches!(out.status, Some(401 | 403)) => {
            out.message.push(' ');
            out.message.push_str(&match (key.source, &key.note) {
                (Some(source), _) if key.value.is_some() => {
                    format!("The key sent came from {source}.")
                }
                (_, Some(note)) => note.clone(),
                _ => "No API key was sent: set options.apiKey in the config or sign in to \
                      OpenCode for this provider."
                    .to_owned(),
            });
        }
        _ => {
            if let Some(note) = key.note {
                out.message.push(' ');
                out.message.push_str(&note);
            }
        }
    }
    out
}

/// Resolves and connects to `target` as the proxy does, then fetches `url`.
async fn test_url(
    url: &str,
    target: &Origin,
    key: Option<&str>,
    response_timeout: Duration,
) -> OpenCodeProviderTest {
    let at = target.authority();
    let refused_hint = if target.is_loopback() {
        format!(" {WINDOWS_HINT}")
    } else {
        String::new()
    };
    let resolved = match resolve_upstream(target.host(), target.port()).await {
        Ok(addrs) => addrs,
        Err(UpstreamError::ResolveTimeout) => {
            return answer(
                ProviderTestOutcome::Timeout,
                format!("Looking up {} timed out.", target.host()),
            )
        }
        Err(_) => {
            return answer(
                ProviderTestOutcome::DnsFailure,
                format!(
                    "{} does not resolve from the WSL distribution, so a sandboxed agent cannot \
                     reach it: check the host name in the baseURL.",
                    target.host()
                ),
            )
        }
    };
    // A grant admits whatever the name resolves to, private addresses too.
    let reachable = reachable_addrs(&resolved, true, &Allowlist::default());
    let stream = match connect_upstream(&reachable).await {
        Ok(stream) => stream,
        Err(UpstreamError::ConnectTimeout) => {
            return answer(
                ProviderTestOutcome::Timeout,
                format!(
                    "Connecting to {at} timed out: nothing answered. Check the address, and that \
                     a firewall lets WSL reach it.{refused_hint}"
                ),
            )
        }
        Err(UpstreamError::Connect(Some(e)))
            if e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            return answer(
                ProviderTestOutcome::Refused,
                format!(
                    "Connection refused at {at}: the server is not running, or listens on \
                     another port or address.{refused_hint}"
                ),
            )
        }
        Err(e) => {
            return answer(
                ProviderTestOutcome::Failed,
                format!("Could not connect to {at}: {e}."),
            )
        }
    };
    let fetched = if url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        let addr = stream.peer_addr().ok();
        drop(stream);
        match addr {
            Some(addr) => curl_get(url, target, addr, key, response_timeout).await,
            None => Err(Fetch::Other("the connection closed at once".into())),
        }
    } else {
        http_get(stream, url, key, response_timeout).await
    };
    let (status, body) = match fetched {
        Ok(answer) => answer,
        Err(Fetch::Timeout) => {
            return answer(
                ProviderTestOutcome::Timeout,
                format!(
                    "{at} accepted the connection but did not answer within {} s.",
                    response_timeout.as_secs_f32()
                ),
            )
        }
        Err(Fetch::Other(why)) => {
            return answer(
                ProviderTestOutcome::Failed,
                format!("{at} accepted the connection, but the request failed: {why}."),
            )
        }
    };
    let mut out = answer(ProviderTestOutcome::HttpError, String::new());
    out.status = Some(status);
    if (200..300).contains(&status) {
        out.outcome = ProviderTestOutcome::Reachable;
        match model_ids(&body) {
            Some(ids) => {
                out.model_count = Some(ids.len() as u32);
                let shown: Vec<_> = ids.iter().take(SHOWN_MODELS).cloned().collect();
                out.message = if ids.is_empty() {
                    "Reachable, and the server lists no models.".to_owned()
                } else {
                    format!(
                        "Reachable: {} model{} ({}{}).",
                        ids.len(),
                        if ids.len() == 1 { "" } else { "s" },
                        shown.join(", "),
                        if ids.len() > shown.len() { ", …" } else { "" }
                    )
                };
                out.model_ids = shown;
            }
            None => {
                out.message = format!(
                    "Reachable (HTTP {status}), but the answer is not an OpenAI-style model list."
                )
            }
        }
        return out;
    }
    out.message = match status {
        401 | 403 => format!(
            "Reachable, but the server refused the request (HTTP {status}): an API key problem."
        ),
        404 => format!(
            "Reachable, but there is no model list at {url} (HTTP 404): check the baseURL's \
             path prefix, such as /v1."
        ),
        _ => format!("Reachable, but the server answered HTTP {status}."),
    };
    out
}

/// Why a fetch over an open connection gave no status.
enum Fetch {
    Timeout,
    Other(String),
}

/// `GET url` over `stream`, HTTP/1.1, closing after the one response.
async fn http_get(
    mut stream: tokio::net::TcpStream,
    url: &str,
    key: Option<&str>,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), Fetch> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.trim().split_once("://").map_or(url, |(_, r)| r);
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path = path.split('#').next().unwrap_or("/");
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\n\
         User-Agent: BondSymphonic\r\nConnection: close\r\n"
    );
    if let Some(key) = key {
        if key.contains(['\r', '\n']) {
            return Err(Fetch::Other("the API key contains a line break".into()));
        }
        request.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    request.push_str("\r\n");
    let deadline = tokio::time::Instant::now() + timeout;
    match tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(Fetch::Other(e.to_string())),
        Err(_) => return Err(Fetch::Timeout),
    }
    // Read until the response is whole, not until the server hangs up: one
    // that keeps the connection open despite `Connection: close` has still
    // answered. Out of time with the head in hand is an answer too.
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while !response_complete(&bytes) && bytes.len() < BODY_LIMIT {
        match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => bytes.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) | Err(_) if head_end(&bytes).is_some() => break,
            Ok(Err(e)) => return Err(Fetch::Other(e.to_string())),
            Err(_) => return Err(Fetch::Timeout),
        }
    }
    parse_response(&bytes).ok_or_else(|| Fetch::Other("the answer is not HTTP".into()))
}

/// Where a response's head ends, once it has.
fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Whether `bytes` hold a whole response: its `Content-Length` worth of body,
/// or a chunked body's last chunk. Without either it ends at the hang-up.
fn response_complete(bytes: &[u8]) -> bool {
    let Some(end) = head_end(bytes) else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let body = &bytes[end + 4..];
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|line| {
            let (n, v) = line.split_once(':')?;
            (n.trim() == name).then(|| v.trim().to_owned())
        })
    };
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return body.ends_with(b"0\r\n\r\n");
    }
    header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| body.len() >= len)
}

/// The status and body of a whole HTTP/1.x response.
fn parse_response(bytes: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    if !status_line.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = &bytes[end + 4..];
    Some((
        status,
        if chunked {
            dechunk(body)
        } else {
            body.to_vec()
        },
    ))
}

/// A chunked body's data, as far as it goes.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = body.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&body[..eol])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok());
        let Some(size) = size.filter(|s| *s > 0) else {
            break;
        };
        let start = eol + 2;
        let end = (start + size).min(body.len());
        out.extend_from_slice(&body[start..end]);
        body = body.get(end + 2..).unwrap_or_default();
    }
    out
}

/// The ids of an OpenAI-style model list, `{"data":[{"id":...}]}`.
fn model_ids(body: &[u8]) -> Option<Vec<String>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let data = value.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|m| m.get("id")?.as_str().map(str::to_owned))
            .collect(),
    )
}

/// `GET url` over TLS, which the daemon has no stack for: the distro's `curl`,
/// pinned to the address the proxy's connect just reached, with the key on
/// its stdin rather than its argv.
async fn curl_get(
    url: &str,
    target: &Origin,
    addr: std::net::SocketAddr,
    key: Option<&str>,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), Fetch> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    if key.is_some_and(|k| k.contains(['\r', '\n'])) || url.contains(['\r', '\n']) {
        return Err(Fetch::Other(
            "the URL or API key contains a line break".into(),
        ));
    }
    let mut config = format!(
        "url = \"{}\"\nheader = \"Accept: application/json\"\n",
        quote(url)
    );
    if let Some(key) = key {
        config.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            quote(key)
        ));
    }
    let ip = match addr.ip() {
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    let bin = if Path::new("/usr/bin/curl").is_file() {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let mut child = tokio::process::Command::new(bin)
        .args([
            "-sS",
            "--noproxy",
            "*",
            "--max-time",
            &format!("{}", timeout.as_secs().max(1)),
            "--resolve",
            &format!("{}:{}:{ip}", target.host(), target.port()),
            "-o",
            "-",
            "-w",
            "\n%{http_code}",
            "-K",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Fetch::Other(format!("curl could not be started: {e}")))?;
    let mut stdin = child.stdin.take();
    tokio::spawn(async move {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(config.as_bytes()).await;
        }
    });
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut limited = (&mut stdout).take(BODY_LIMIT as u64);
        let _ = tokio::join!(limited.read_to_end(&mut out), stderr.read_to_end(&mut err));
        let status = child.wait().await.ok().and_then(|s| s.code());
        (status, out, err)
    };
    let (code, out, err) = tokio::time::timeout(timeout + Duration::from_secs(2), run)
        .await
        .map_err(|_| Fetch::Timeout)?;
    match code {
        Some(0) => {}
        Some(28) => return Err(Fetch::Timeout),
        _ => {
            let err = String::from_utf8_lossy(&err);
            let err = err.trim().trim_start_matches("curl: ");
            return Err(Fetch::Other(if err.is_empty() {
                "curl failed".into()
            } else {
                err.to_owned()
            }));
        }
    }
    let split = out.iter().rposition(|b| *b == b'\n').unwrap_or(0);
    let status = std::str::from_utf8(&out[split..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| Fetch::Other("curl gave no status".into()))?;
    Ok((status, out[..split].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn config(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("opencode")).unwrap();
        std::fs::write(dir.path().join("opencode/opencode.jsonc"), json).unwrap();
        dir
    }

    /// Answers every connection with `response`, once read up to the blank
    /// line, and keeps each request it saw.
    async fn server(response: &str) -> (u16, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let response: Arc<str> = response.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let kept = kept.clone();
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        if s.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        buf.push(byte[0]);
                    }
                    kept.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    if response.is_empty() {
                        // A server that never answers.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    let _ = s.write_all(response.as_bytes()).await;
                    if response.contains("keep-alive") {
                        // Holds the connection open after answering.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                });
            }
        });
        (port, seen)
    }

    async fn test(dir: &tempfile::TempDir, id: &str) -> OpenCodeProviderTest {
        test_provider_with(
            dir.path(),
            &dir.path().join("auth.json"),
            id,
            None,
            Duration::from_millis(500),
        )
        .await
    }

    #[test]
    fn providers_are_listed_with_their_base_url_name_and_model_count() {
        let dir = config(
            r#"{
              // a comment
              "provider": {
                "ollama": {"name": "Ollama", "options": {"baseURL": "http://localhost:11434/v1"},
                           "models": {"llama3": {}, "qwen": {},}},
                "lan": {"api": "http://192.168.1.20:8000/v1"},
                "envy": {"options": {"baseURL": "{env:MODEL_URL}"}},
                "openai": {}
              }
            }"#,
        );
        std::fs::write(
            dir.path().join("opencode/config.json"),
            r#"{"provider":{"ollama":{"models":{"old":{}}},"legacy":{"options":{"baseURL":"https://models.example.test/v1"}}}}"#,
        )
        .unwrap();
        let listed = list_providers(dir.path());
        let ids: Vec<_> = listed.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["envy", "lan", "legacy", "ollama", "openai"]);
        let by = |id: &str| {
            listed
                .providers
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .clone()
        };
        let ollama = by("ollama");
        assert_eq!(ollama.name.as_deref(), Some("Ollama"));
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        // Merged over config.json's entry: three models in all.
        assert_eq!(ollama.model_count, 3);
        assert_eq!(ollama.sandbox_route, SandboxRoute::Tunnelled);
        assert_eq!(by("lan").sandbox_route, SandboxRoute::Granted);
        assert_eq!(
            by("lan").base_url.as_deref(),
            Some("http://192.168.1.20:8000/v1")
        );
        assert_eq!(by("legacy").sandbox_route, SandboxRoute::Granted);
        assert_eq!(by("envy").sandbox_route, SandboxRoute::NotGranted);
        assert!(by("envy").route_note.unwrap().contains("{env:"));
        assert_eq!(by("openai").sandbox_route, SandboxRoute::NoBaseUrl);
        assert!(listed.warnings.is_empty());

        std::fs::write(dir.path().join("opencode/opencode.json"), "{ nope").unwrap();
        let listed = list_providers(dir.path());
        assert_eq!(listed.warnings.len(), 1, "{:?}", listed.warnings);
        assert!(listed.warnings[0].contains("opencode.json"));
    }

    /// The list says "reachable" for exactly the origins the companion
    /// sandbox is granted, from the same files.
    #[test]
    fn the_route_matches_the_companion_grants() {
        let dir = config(
            r#"{"provider": {
                "a": {"options": {"baseURL": "http://localhost:11434/v1"}},
                "b": {"options": {"baseURL": "http://[::1]:8000"}},
                "c": {"options": {"baseURL": "https://Models.Example.test/v1/"}},
                "d": {"options": {"baseURL": "{env:X}"}},
                "e": {"options": {"baseURL": "http://localhost:3128/v1"}},
                "f": {"options": {"baseURL": "http://10.0.0.5:9000/v1"}}
            }}"#,
        );
        let grants = provider_origins(dir.path());
        let listed = list_providers(dir.path());
        let mut routed: Vec<Origin> = listed
            .providers
            .iter()
            .filter(|p| {
                p.sandbox_route == SandboxRoute::Granted
                    || p.sandbox_route == SandboxRoute::Tunnelled
            })
            .map(|p| Origin::from_url(p.base_url.as_deref().unwrap()).unwrap())
            .collect();
        routed.sort();
        let mut tunnelled_or_granted = grants.clone();
        // The proxy's own port is granted but no tunnel can listen there.
        tunnelled_or_granted.retain(|o| o.authority() != "localhost:3128");
        assert_eq!(routed, tunnelled_or_granted);
        let e = listed.providers.iter().find(|p| p.id == "e").unwrap();
        assert_eq!(e.sandbox_route, SandboxRoute::NotGranted);
        assert!(e.route_note.as_deref().unwrap().contains("3128"));
        let b = route(Some("http://[::1]:8000"), &grants);
        assert_eq!(b.kind, SandboxRoute::Tunnelled);
        assert_eq!(b.target.unwrap().authority(), "[::1]:8000");
    }

    #[tokio::test]
    async fn a_server_that_lists_models_is_reachable() {
        // Chunked, split mid-token, as a streaming server sends it.
        let body =
            r#"{"data":[{"id":"a"},{"id":"b"},{"id":"c"},{"id":"d"},{"id":"e"},{"id":"f"}]}"#;
        let (one, two) = body.split_at(13);
        let (port, seen) = server(&format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
             {:x}\r\n{one}\r\n{:x}\r\n{two}\r\n0\r\n\r\n",
            one.len(),
            two.len()
        ))
        .await;
        let dir = config(&format!(
            r#"{{"provider":{{"local":{{"options":{{"baseURL":"http://127.0.0.1:{port}/v1/"}}}}}}}}"#
        ));
        let result = test(&dir, "local").await;
        assert_eq!(
            result.outcome,
            ProviderTestOutcome::Reachable,
            "{}",
            result.message
        );
        assert_eq!(result.status, Some(200));
        assert_eq!(result.model_count, Some(6));
        assert_eq!(result.model_ids, ["a", "b", "c", "d", "e"]);
        assert!(result.message.contains("6 models"), "{}", result.message);
        assert_eq!(
            result.url.as_deref(),
            Some(format!("http://127.0.0.1:{port}/v1/models").as_str())
        );
        let request = seen.lock().unwrap()[0].clone();
        assert!(
            request.starts_with("GET /v1/models HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains(&format!("Host: 127.0.0.1:{port}\r\n")));
        assert!(!request.to_ascii_lowercase().contains("authorization"));
        assert_eq!(result.key_source, None);
    }

    #[tokio::test]
    async fn a_rejected_key_and_a_wrong_prefix_are_told_apart() {
        let (port, seen) = server("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await;
        let dir = config(&format!(
            r#"{{"provider":{{"k":{{"options":{{"baseURL":"http://127.0.0.1:{port}","apiKey":"sk-config"}}}}}}}}"#
        ));
        let result = test(&dir, "k").await;
        assert_eq!(result.outcome, ProviderTestOutcome::HttpError);
        assert_eq!(result.status, Some(401));
        assert!(result.message.contains("API key"), "{}", result.message);
        assert!(!result.message.contains("sk-config"));
        assert_eq!(result.key_source.as_deref(), Some("the config's apiKey"));
        assert!(seen.lock().unwrap()[0].contains("Authorization: Bearer sk-config\r\n"));

        let (port, _) = server("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
        let dir = config(&format!(
            r#"{{"provider":{{"p":{{"options":{{"baseURL":"http://localhost:{port}"}}}}}}}}"#
        ));
        let result = test(&dir, "p").await;
        assert_eq!(result.outcome, ProviderTestOutcome::HttpError);
        assert_eq!(result.status, Some(404));
        assert!(result.message.contains("path prefix"), "{}", result.message);
    }

    #[tokio::test]
    async fn the_key_is_the_one_a_sandboxed_agent_would_send() {
        let (port, seen) =
            server("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"data\":[]}").await;
        let url = format!("http://127.0.0.1:{port}/v1");
        // `{env:OPENAI_API_KEY}` is the stored key; any other variable is not
        // in the sandbox, so nothing is sent and the result says why.
        let dir = config(&format!(
            r#"{{"provider":{{
                "a":{{"options":{{"baseURL":"{url}","apiKey":"{{env:OPENAI_API_KEY}}"}}}},
                "b":{{"options":{{"baseURL":"{url}","apiKey":"{{env:OTHER_KEY}}"}}}},
                "c":{{"options":{{"baseURL":"{url}"}}}}
            }}}}"#
        ));
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"c":{"type":"api","key":"sk-login"}}"#,
        )
        .unwrap();
        let auth = dir.path().join("auth.json");
        let run = |id: &'static str| {
            let dir = dir.path().to_path_buf();
            let auth = auth.clone();
            async move {
                test_provider_with(&dir, &auth, id, Some("sk-stored"), Duration::from_secs(2)).await
            }
        };
        let a = run("a").await;
        assert_eq!(a.outcome, ProviderTestOutcome::Reachable, "{}", a.message);
        let b = run("b").await;
        assert!(b.message.contains("{env:OTHER_KEY}"), "{}", b.message);
        assert_eq!(b.key_source, None);
        let c = run("c").await;
        assert_eq!(c.key_source.as_deref(), Some("the OpenCode sign-in"));
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("Authorization: Bearer sk-stored\r\n"));
        assert!(!seen[1].contains("Authorization"));
        assert!(seen[2].contains("Authorization: Bearer sk-login\r\n"));
    }

    #[tokio::test]
    async fn nothing_listening_is_a_refusal_with_the_windows_hint() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = config(&format!(
            r#"{{"provider":{{"down":{{"options":{{"baseURL":"http://127.0.0.1:{port}/v1"}}}}}}}}"#
        ));
        let result = test(&dir, "down").await;
        assert_eq!(
            result.outcome,
            ProviderTestOutcome::Refused,
            "{}",
            result.message
        );
        assert!(result.message.contains("mirrored"), "{}", result.message);
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out() {
        let (port, _) = server("").await;
        let dir = config(&format!(
            r#"{{"provider":{{"slow":{{"options":{{"baseURL":"http://127.0.0.1:{port}/v1"}}}}}}}}"#
        ));
        let result = test(&dir, "slow").await;
        assert_eq!(
            result.outcome,
            ProviderTestOutcome::Timeout,
            "{}",
            result.message
        );
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_a_dns_failure() {
        let dir = config(
            r#"{"provider":{"gone":{"options":{"baseURL":"http://no-such-host.invalid:8000/v1"}}}}"#,
        );
        let result = test(&dir, "gone").await;
        assert_eq!(
            result.outcome,
            ProviderTestOutcome::DnsFailure,
            "{}",
            result.message
        );
    }

    #[tokio::test]
    async fn an_origin_sandboxes_are_not_granted_is_not_dialled() {
        let dir = config(
            r#"{"provider":{
                "envy":{"options":{"baseURL":"{env:MODEL_URL}"}},
                "proxy":{"options":{"baseURL":"http://127.0.0.1:3128/v1"}},
                "builtin":{}
            }}"#,
        );
        for id in ["envy", "proxy"] {
            let result = test(&dir, id).await;
            assert_eq!(result.outcome, ProviderTestOutcome::NotGranted, "{id}");
            assert!(result.message.starts_with("Not reachable from sandboxes"));
            assert_eq!(result.url, None);
        }
        assert_eq!(
            test(&dir, "builtin").await.outcome,
            ProviderTestOutcome::Failed
        );
        assert_eq!(
            test(&dir, "missing").await.outcome,
            ProviderTestOutcome::Failed
        );
    }

    /// A server that keeps the connection open after a whole answer has
    /// answered; waiting for it to hang up would call that a timeout.
    #[tokio::test]
    async fn a_whole_answer_on_a_kept_alive_connection_is_an_answer() {
        let (port, _) = server(
            "HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 21\r\n\r\n{\"data\":[{\"id\":\"m\"}]}",
        )
        .await;
        let dir = config(&format!(
            r#"{{"provider":{{"k":{{"options":{{"baseURL":"http://127.0.0.1:{port}/v1"}}}}}}}}"#
        ));
        let result = test(&dir, "k").await;
        assert_eq!(
            result.outcome,
            ProviderTestOutcome::Reachable,
            "{}",
            result.message
        );
        assert_eq!(result.model_ids, ["m"]);
    }
}
