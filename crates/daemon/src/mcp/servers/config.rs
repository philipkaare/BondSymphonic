//! Validation, the Jira preset, and `mcpServers` JSON import.
use bondsymphonic_proto::*;
use std::collections::{BTreeMap, BTreeSet};

/// The name the built-in git tools announce themselves under.
const RESERVED_ID: &str = crate::mcp::SERVER_NAME;
const DEFAULT_BACKENDS: [&str; 3] = ["claude", "codex", "opencode"];

/// `^[a-z0-9][a-z0-9_-]{0,31}$`: the server id appears in file names and in
/// the name the agent sees, so it is kept to a safe, short alphabet.
pub(crate) fn valid_server_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The names a server's secrets live under: variable names for a stdio server,
/// `SecretRef`s for a remote one. See `McpSecretWrite`.
pub(crate) fn refs(server: &McpServerConfig) -> BTreeSet<String> {
    match &server.transport {
        McpTransportConfig::Stdio { secret_env, .. } => secret_env.iter().cloned().collect(),
        McpTransportConfig::Http { auth, .. } => match auth {
            McpAuthConfig::Bearer { token } => [token.0.clone()].into(),
            McpAuthConfig::Headers { headers } => headers.values().map(|r| r.0.clone()).collect(),
            McpAuthConfig::OAuth { client_secret, .. } => {
                client_secret.iter().map(|r| r.0.clone()).collect()
            }
            McpAuthConfig::None => BTreeSet::new(),
        },
    }
}

/// Loopback `http://` exists so tests can talk to a local fake server; it is
/// off unless `BS_MCP_ALLOW_HTTP=1`.
pub(crate) fn loopback_http_allowed() -> bool {
    std::env::var("BS_MCP_ALLOW_HTTP").is_ok_and(|v| v == "1")
}

fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    authority.split(':').next().unwrap_or("")
}

/// Whether an upstream URL may be used at all: `https://`, or loopback
/// `http://` behind the test switch.
pub(crate) fn url_allowed(url: &str) -> bool {
    let (rest, plain) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (rest, false),
        (None, Some(rest)) => (rest, true),
        _ => return false,
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || url.contains(['#', '?'])
    {
        return false;
    }
    !plain
        || (loopback_http_allowed()
            && matches!(host_of(authority), "127.0.0.1" | "localhost" | "::1"))
}

pub(crate) fn validate(server: &McpServerConfig) -> Result<(), RpcError> {
    let id = server.id.as_str();
    if !valid_server_id(id) || id == RESERVED_ID {
        return Err(RpcError::invalid_params("invalid MCP server id"));
    }
    if server.name.trim().is_empty() || server.name.chars().any(char::is_control) {
        return Err(RpcError::invalid_params("invalid MCP server name"));
    }
    if server.enabled_backends.iter().any(|b| !safe_token(b)) {
        return Err(RpcError::invalid_params("invalid MCP backend"));
    }
    match &server.transport {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            secret_env,
        } => {
            // A deliberately bounded policy, not a classifier for arbitrary
            // secrets: named credential options belong in `secret_env`.
            if args.iter().any(|arg| credential_argument(arg)) {
                return Err(RpcError::invalid_params(
                    "MCP credential arguments are unsupported; use secret environment variables",
                ));
            }
            // Names end up in an env file and a process environment: anything
            // beyond a plain identifier (newlines, `=`) could inject variables.
            let bad_name = |k: &String| !env_name_ok(k);
            let distinct: BTreeSet<_> = secret_env.iter().collect();
            if command.trim().is_empty()
                || command.contains('\0')
                || env.keys().chain(secret_env.iter()).any(bad_name)
                || distinct.len() != secret_env.len()
                || secret_env.iter().any(|k| env.contains_key(k))
            {
                return Err(RpcError::invalid_params("invalid MCP stdio configuration"));
            }
        }
        McpTransportConfig::Http { url, auth } => {
            if !url_allowed(url) {
                return Err(RpcError::invalid_params(
                    "MCP URL must be a plain https:// URL",
                ));
            }
            if refs(server).iter().any(|r| !safe_token(r)) {
                return Err(RpcError::invalid_params("invalid MCP secret reference"));
            }
            if let McpAuthConfig::Headers { headers } = auth {
                if headers.keys().any(|k| {
                    k.is_empty() || !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                }) {
                    return Err(RpcError::invalid_params("invalid MCP header name"));
                }
            }
        }
    }
    Ok(())
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn credential_argument(argument: &str) -> bool {
    let normalized = argument
        .trim()
        .trim_start_matches(['-', '/'])
        .to_ascii_lowercase()
        .replace('_', "-");
    let key = normalized.split(['=', ':', ' ']).next().unwrap_or("");
    [
        "token",
        "api-key",
        "apikey",
        "password",
        "passwd",
        "bearer",
        "access-token",
        "refresh-token",
        "client-secret",
        "secret",
        "authorization",
    ]
    .contains(&key)
}

pub(crate) fn preset() -> McpServerConfig {
    McpServerConfig {
        id: "jira".into(),
        name: "Jira (Atlassian)".into(),
        transport: McpTransportConfig::Http {
            url: "https://mcp.atlassian.com/v2/mcp".into(),
            // Discovery: no endpoints are hard-coded, the server advertises them.
            auth: McpAuthConfig::OAuth {
                client_id: None,
                client_metadata_url: None,
                client_secret: None,
                scopes: vec![],
            },
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: DEFAULT_BACKENDS.map(String::from).to_vec(),
    }
}

pub(crate) struct Import {
    pub preview: McpImportPreviewResult,
    /// The secret values each imported server brings, applied with it.
    pub writes: BTreeMap<McpServerId, Vec<McpSecretWrite>>,
}

/// Why one entry was not importable: the offending field and a fixed reason.
/// Never built from the entry's own text, which may be a credential.
type Rejection = (&'static str, &'static str);

pub(crate) fn parse_import(
    params: &McpImportParams,
    existing: &[McpServerConfig],
) -> Result<Import, RpcError> {
    let document: serde_json::Value = serde_json::from_str(&params.document.0)
        .map_err(|_| RpcError::invalid_params("invalid MCP import JSON"))?;
    let entries = document
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| RpcError::invalid_params("expected mcpServers object"))?;
    let mut result = Import {
        preview: McpImportPreviewResult {
            servers: vec![],
            issues: vec![],
            collisions: vec![],
        },
        writes: BTreeMap::new(),
    };
    let mut seen = BTreeSet::new();
    for (index, (name, value)) in entries.iter().enumerate() {
        let id = params
            .resolutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| McpServerId(generated_id(name)));
        match parse_entry(name, value, &id, index) {
            Ok((server, writes)) => {
                // A resolution naming an existing id is an explicit replace.
                if !seen.insert(id.clone())
                    || (existing.iter().any(|s| s.id == id)
                        && !params.resolutions.contains_key(name))
                {
                    result.preview.collisions.push(id.clone());
                }
                result.writes.insert(id, writes);
                result.preview.servers.push(server);
            }
            Err((field, reason)) => result.preview.issues.push(McpImportIssue {
                // The entry's name may be a pasted credential, so issues are
                // identified by position in the document, never by text.
                entry: format!("entry {}", index + 1),
                field: field.into(),
                reason: reason.into(),
            }),
        }
    }
    Ok(result)
}

fn generated_id(name: &str) -> String {
    name.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect()
}

fn parse_entry(
    name: &str,
    value: &serde_json::Value,
    id: &McpServerId,
    index: usize,
) -> Result<(McpServerConfig, Vec<McpSecretWrite>), Rejection> {
    let object = value
        .as_object()
        .ok_or(("configuration", "entry is not an object"))?;
    if object
        .keys()
        .any(|k| !["type", "command", "args", "env", "url", "headers"].contains(&k.as_str()))
    {
        return Err(("configuration", "unsupported field"));
    }
    let kind = value.get("type").and_then(|v| v.as_str());
    match kind {
        None | Some("stdio" | "http" | "streamable-http" | "streamable_http") => {}
        Some("sse") => return Err(("type", "SSE servers are not supported")),
        Some(_) => return Err(("type", "unsupported transport type")),
    }
    let mut writes = vec![];
    // Every imported env/header value is a credential until proven otherwise:
    // it becomes a write-only secret, never part of the config.
    let mut secret_map = |field: &'static str| -> Result<BTreeMap<String, String>, Rejection> {
        let mut out = BTreeMap::new();
        if let Some(values) = value.get(field) {
            let values = values
                .as_object()
                .ok_or((field, "expected an object of strings"))?;
            for (key, val) in values {
                let val = val
                    .as_str()
                    .ok_or((field, "expected an object of strings"))?;
                writes.push(McpSecretWrite::Set {
                    reference: SecretRef(key.clone()),
                    value: McpSecretValue(val.into()),
                });
                out.insert(key.clone(), key.clone());
            }
        }
        Ok(out)
    };
    let transport = if let Some(command) = value.get("command") {
        if value.get("url").is_some() || value.get("headers").is_some() {
            return Err(("configuration", "both command and url given"));
        }
        if kind.is_some_and(|k| k != "stdio") {
            return Err(("type", "type does not match command"));
        }
        McpTransportConfig::Stdio {
            command: command
                .as_str()
                .ok_or(("command", "expected a string"))?
                .into(),
            args: match value.get("args") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|_| ("args", "expected an array of strings"))?,
                None => vec![],
            },
            env: BTreeMap::new(),
            secret_env: secret_map("env")?.into_keys().collect(),
        }
    } else {
        if value.get("args").is_some() || value.get("env").is_some() {
            return Err(("configuration", "args and env need a command"));
        }
        if kind == Some("stdio") {
            return Err(("type", "stdio needs a command"));
        }
        let url = value
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or(("url", "expected a string"))?;
        if !crate::mcp::servers::config::url_allowed(url) {
            return Err(("url", "only https:// servers are supported"));
        }
        // The header name doubles as the secret's reference.
        let headers: BTreeMap<String, SecretRef> = secret_map("headers")?
            .into_keys()
            .map(|k| (k.clone(), SecretRef(k)))
            .collect();
        McpTransportConfig::Http {
            url: url.into(),
            auth: if headers.is_empty() {
                McpAuthConfig::None
            } else {
                McpAuthConfig::Headers { headers }
            },
        }
    };
    let server = McpServerConfig {
        id: id.clone(),
        name: if safe_token(name) {
            name.into()
        } else {
            format!("Imported server {}", index + 1)
        },
        transport,
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: DEFAULT_BACKENDS.map(String::from).to_vec(),
    };
    validate(&server).map_err(|_| ("configuration", "invalid MCP configuration"))?;
    Ok((server, writes))
}
