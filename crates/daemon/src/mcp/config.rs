use bondsymphonic_proto::*;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
pub(crate) fn refs(server: &McpServerConfig) -> BTreeSet<SecretRef> {
    match &server.transport {
        McpTransportConfig::Stdio { env_secrets, .. } => env_secrets.values().cloned().collect(),
        McpTransportConfig::Http { auth, .. } => match auth {
            McpAuthConfig::Bearer { token } => [token.clone()].into(),
            McpAuthConfig::Headers { headers } => headers.values().cloned().collect(),
            McpAuthConfig::OAuth { client_secret, .. } => client_secret.iter().cloned().collect(),
            _ => BTreeSet::new(),
        },
    }
}
pub(crate) fn validate(server: &McpServerConfig) -> Result<(), RpcError> {
    if !safe_id(server.id.as_str())
        || server.name.trim().is_empty()
        || server.name.chars().any(char::is_control)
    {
        return Err(RpcError::invalid_params("invalid MCP server identity"));
    }
    if server.enabled_backends.iter().any(|b| !safe_id(b))
        || refs(server).iter().any(|r| !safe_id(r.as_str()))
    {
        return Err(RpcError::invalid_params(
            "invalid MCP backend or secret reference",
        ));
    }
    match &server.transport {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            env_secrets,
        } => {
            // This is a deliberately bounded policy, not a classifier for
            // arbitrary secrets. Named credential options belong in env_secrets.
            if args.iter().any(|arg| credential_argument(arg)) {
                return Err(RpcError::invalid_params(
                    "MCP credential arguments are unsupported; use environment secret references",
                ));
            }
            if command.trim().is_empty()
                || command.contains('\0')
                || env
                    .keys()
                    .chain(env_secrets.keys())
                    .any(|k| k.is_empty() || k.contains(['=', '\0']))
                || env.keys().any(|k| env_secrets.contains_key(k))
            {
                return Err(RpcError::invalid_params("invalid MCP stdio configuration"));
            }
        }
        McpTransportConfig::Http { url, auth } => {
            let authority = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .and_then(|s| s.split('/').next())
                .unwrap_or("");
            if authority.is_empty()
                || authority.contains('@')
                || url.chars().any(char::is_whitespace)
                || url.contains(['#', '?'])
            {
                return Err(RpcError::invalid_params("invalid MCP HTTP URL"));
            }
            if let McpAuthConfig::Headers { headers } = auth {
                if headers.keys().any(|k| {
                    !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') || k.is_empty()
                }) {
                    return Err(RpcError::invalid_params("invalid MCP header name"));
                }
            }
        }
    }
    Ok(())
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
            auth: McpAuthConfig::OAuth {
                client_id: None,
                client_metadata_url: None,
                client_secret: None,
                scopes: vec![],
            },
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into(), "codex".into(), "opencode".into()],
    }
}

pub(crate) struct Import {
    pub preview: McpImportPreviewResult,
    pub writes: BTreeMap<McpServerId, Vec<McpSecretWrite>>,
}
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
        // Never echo arbitrary imported names/fields: these may contain credentials.
        let label = format!("entry {}", index + 1);
        let generated = name
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let id = params
            .resolutions
            .get(name)
            .cloned()
            .unwrap_or(McpServerId(generated));
        let mut writes = vec![];
        let parsed = (|| -> Result<McpServerConfig, ()> {
            let object = value.as_object().ok_or(())?;
            if object.keys().any(|k| {
                !["type", "command", "args", "env", "url", "headers"].contains(&k.as_str())
            }) {
                return Err(());
            }
            let kind = value.get("type").and_then(|v| v.as_str());
            if kind == Some("sse")
                || kind.is_some_and(|k| {
                    !["stdio", "http", "streamable-http", "streamable_http"].contains(&k)
                })
            {
                return Err(());
            }
            let mut secret_map = |field: &str| -> Result<BTreeMap<String, SecretRef>, ()> {
                let mut out = BTreeMap::new();
                if let Some(values) = value.get(field) {
                    for (n, (key, val)) in values.as_object().ok_or(())?.iter().enumerate() {
                        let reference = SecretRef(format!("{}-{}-{}", id.0, field, n));
                        writes.push(McpSecretWrite::Set {
                            reference: reference.clone(),
                            value: McpSecretValue(val.as_str().ok_or(())?.into()),
                        });
                        out.insert(key.clone(), reference);
                    }
                }
                Ok(out)
            };
            let transport = if let Some(command) = value.get("command") {
                if value.get("url").is_some()
                    || value.get("headers").is_some()
                    || kind.is_some_and(|k| k != "stdio")
                {
                    return Err(());
                }
                McpTransportConfig::Stdio {
                    command: command.as_str().ok_or(())?.into(),
                    args: match value.get("args") {
                        Some(v) => serde_json::from_value(v.clone()).map_err(|_| ())?,
                        None => vec![],
                    },
                    env: BTreeMap::new(),
                    env_secrets: secret_map("env")?,
                }
            } else {
                if value.get("args").is_some()
                    || value.get("env").is_some()
                    || kind == Some("stdio")
                {
                    return Err(());
                }
                let headers = secret_map("headers")?;
                McpTransportConfig::Http {
                    url: value.get("url").and_then(|v| v.as_str()).ok_or(())?.into(),
                    auth: if headers.is_empty() {
                        McpAuthConfig::None
                    } else {
                        McpAuthConfig::Headers { headers }
                    },
                }
            };
            let server = McpServerConfig {
                id: id.clone(),
                name: if safe_id(name) {
                    name.clone()
                } else {
                    format!("Imported server {}", index + 1)
                },
                transport,
                enabled: true,
                default_workspace_enabled: true,
                enabled_backends: vec!["claude".into(), "codex".into(), "opencode".into()],
            };
            validate(&server).map_err(|_| ())?;
            Ok(server)
        })();
        match parsed {
            Ok(server) => {
                if !seen.insert(id.clone())
                    || (existing.iter().any(|s| s.id == id)
                        && !params.resolutions.contains_key(name))
                {
                    result.preview.collisions.push(id.clone());
                }
                result.writes.insert(id, writes);
                result.preview.servers.push(server);
            }
            Err(_) => result.preview.issues.push(McpImportIssue {
                entry: label,
                field: "configuration".into(),
                reason: "unsupported or invalid MCP configuration".into(),
            }),
        }
    }
    Ok(result)
}
