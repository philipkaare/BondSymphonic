//! The user-configured MCP server registry: which servers exist, which
//! workspaces and backends get them, and the secrets they need.
//!
//! The registry is daemon-global and lives in `<data_dir>/mcp/registry.json`.
//! It never contains a secret value: those live in a separate `0600` file the
//! manifest points at, and only [`McpServers::secret`] reads them back.
//!
//! Secrets are scoped per server id. The name a secret is stored under is the
//! `SecretRef` in `McpSecretWrite`, with one exception for stdio servers: there
//! the reference IS the environment variable's name, since the name is the only
//! thing that identifies a `secret_env` entry. Remote servers' bearer, header
//! and OAuth client-secret references are used as given.
pub(crate) mod config;
pub(crate) mod secrets;

use bondsymphonic_proto::*;
use parking_lot::Mutex;
use secrets::SecretStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `registry.json`: the public snapshot plus the name of the secrets file it
/// is consistent with.
///
/// The secrets file is versioned (a fresh random name per save) and only the
/// manifest, replaced atomically, says which one is current. A crash between
/// the two writes therefore leaves either the old pair or the new pair, never
/// a registry that disagrees with its secrets; the unreferenced file is swept
/// on the next load.
#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    #[serde(flatten)]
    snapshot: McpRegistrySnapshot,
    secret_file: String,
}

struct State {
    snapshot: McpRegistrySnapshot,
    secret_file: String,
    secrets: SecretStore,
}

pub struct McpServers {
    dir: PathBuf,
    /// Set when the files on disk could not be used; see [`McpServers::load`].
    damaged: Option<RpcError>,
    state: Mutex<State>,
    changes: tokio::sync::watch::Sender<u64>,
}

/// Storage errors carry no detail: the files hold credentials and the path or
/// parse position is of no use to a client.
fn storage_error() -> RpcError {
    RpcError::new(ErrorCode::IoError, "MCP storage operation failed")
}

fn new_secret_file() -> String {
    format!("secrets-{}", hex::encode(rand::random::<[u8; 8]>()))
}

/// The id used for a backend in `McpServerConfig::enabled_backends`.
fn backend_name(kind: AgentAdapterKind) -> Option<&'static str> {
    match kind {
        AgentAdapterKind::Claude => Some("claude"),
        AgentAdapterKind::Codex => Some("codex"),
        AgentAdapterKind::OpenCode => Some("opencode"),
        AgentAdapterKind::Terminal => None,
    }
}

impl McpServers {
    /// `data_dir` is the daemon's data directory; the files live in its `mcp`
    /// child. A first start seeds the Jira preset exactly once: after that,
    /// deleting it is permanent until [`McpServers::restore_preset`].
    ///
    /// Never fails the daemon: if the files cannot be used (corrupt JSON, a
    /// missing secrets file, a failed first-start write) the registry loads
    /// *degraded*. It logs the path, leaves the files untouched, resolves no
    /// servers, and `list` and every mutation answer an `IoError` naming the
    /// file. So a bad file is neither a daemon that will not start nor a reset
    /// that re-seeds Jira over the user's data. A single stored server that
    /// fails today's validation is skipped and logged instead.
    pub fn load(data_dir: &Path) -> Result<Self, RpcError> {
        let dir = data_dir.join("mcp");
        let path = dir.join("registry.json");
        Ok(Self::try_load(&dir, &path).unwrap_or_else(|_| {
            tracing::warn!(
                path = %path.display(),
                "MCP registry could not be read; MCP servers are disabled until it is fixed"
            );
            let (changes, _) = tokio::sync::watch::channel(0);
            Self {
                dir: dir.clone(),
                damaged: Some(RpcError::new(
                    ErrorCode::IoError,
                    format!(
                        "the MCP registry at {} could not be read; fix or remove it and restart the daemon",
                        path.display()
                    ),
                )),
                state: Mutex::new(State {
                    snapshot: Default::default(),
                    secret_file: String::new(),
                    secrets: SecretStore::default(),
                }),
                changes,
            }
        }))
    }

    /// `Err` when the registry is degraded; see [`McpServers::load`].
    pub fn ensure_usable(&self) -> Result<(), RpcError> {
        self.damaged.clone().map_or(Ok(()), Err)
    }

    fn try_load(dir: &Path, path: &Path) -> Result<Self, RpcError> {
        let dir = dir.to_path_buf();
        let state = match std::fs::read(path) {
            Ok(bytes) => {
                let mut manifest: Manifest =
                    serde_json::from_slice(&bytes).map_err(|_| storage_error())?;
                let mut ids = BTreeSet::new();
                manifest.snapshot.servers.retain(|server| {
                    let ok = config::validate(server).is_ok() && ids.insert(server.id.clone());
                    if !ok {
                        tracing::warn!(
                            server = %server.id.as_str(),
                            "skipping an invalid or duplicate MCP server in the registry"
                        );
                    }
                    ok
                });
                if !secrets::is_version_name(&manifest.secret_file) {
                    return Err(storage_error());
                }
                let mut store: SecretStore = serde_json::from_slice(
                    &std::fs::read(dir.join(&manifest.secret_file)).map_err(|_| storage_error())?,
                )
                .map_err(|_| storage_error())?;
                // Secrets of a server the registry no longer has are dead weight.
                store.0.retain(|id, _| ids.contains(id));
                State {
                    snapshot: manifest.snapshot,
                    secret_file: manifest.secret_file,
                    secrets: store,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let state = State {
                    snapshot: McpRegistrySnapshot {
                        revision: 0,
                        servers: vec![config::preset()],
                        overrides: Default::default(),
                    },
                    secret_file: new_secret_file(),
                    secrets: SecretStore::default(),
                };
                write_state(&dir, &state)?;
                state
            }
            Err(_) => return Err(storage_error()),
        };
        // A crash before the manifest was replaced may leave an unreferenced
        // secrets version. It must not keep a deleted credential alive.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if ((secrets::is_version_name(&name) && name != state.secret_file)
                    || secrets::is_private_temporary(&name))
                    && entry.file_type().is_ok_and(|t| t.is_file())
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let (changes, _) = tokio::sync::watch::channel(state.snapshot.revision);
        Ok(Self {
            dir,
            damaged: None,
            state: Mutex::new(state),
            changes,
        })
    }

    pub fn snapshot(&self) -> McpRegistrySnapshot {
        self.state.lock().snapshot.clone()
    }

    /// The registry revision, republished after every change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Every server `Disconnected`: live connection state arrives with the
    /// transport layer.
    pub fn list(&self) -> Result<McpListResult, RpcError> {
        self.ensure_usable()?;
        let registry = self.snapshot();
        let statuses = registry
            .servers
            .iter()
            .map(|s| McpServerStatus {
                server_id: s.id.clone(),
                state: McpConnectionState::Disconnected,
                reason: None,
            })
            .collect();
        Ok(McpListResult { registry, statuses })
    }

    /// The registry as JSON: configuration and secret references, never values.
    pub fn export(&self) -> String {
        serde_json::to_string_pretty(&self.snapshot()).unwrap_or_default()
    }

    // Read by the transport and backend layers that build on this registry.
    #[allow(dead_code)]
    /// The secret stored for `id` under `name`, if any. Crate-private on
    /// purpose: no RPC reads a value back.
    pub(crate) fn secret(&self, id: &McpServerId, name: &str) -> Option<McpSecretValue> {
        self.state.lock().secrets.get(id, name).cloned()
    }

    #[allow(dead_code)]
    /// Stores (`Some`) or removes (`None`) one secret of an existing server,
    /// e.g. an OAuth token the daemon obtained. It does not change the
    /// registry, so the revision stays and nothing is announced.
    pub(crate) fn set_secret(
        &self,
        id: &McpServerId,
        name: &str,
        value: Option<McpSecretValue>,
    ) -> Result<(), RpcError> {
        self.ensure_usable()?;
        let mut state = self.state.lock();
        if !state.snapshot.servers.iter().any(|s| &s.id == id) {
            return Err(RpcError::not_found("MCP server not found"));
        }
        let mut secrets = state.secrets.clone();
        match value {
            Some(v) => secrets.set(id, name, v),
            None => secrets.remove(id, name),
        }
        let snapshot = state.snapshot.clone();
        self.publish(&mut state, snapshot, secrets)?;
        Ok(())
    }

    /// Writes the new secrets version, then the manifest that points at it,
    /// and only then swaps the in-memory state. A failure leaves the published
    /// state and the files as they were.
    fn publish(
        &self,
        state: &mut State,
        snapshot: McpRegistrySnapshot,
        secrets: SecretStore,
    ) -> Result<(), RpcError> {
        let next = State {
            snapshot,
            secret_file: new_secret_file(),
            secrets,
        };
        write_state(&self.dir, &next)?;
        let old = std::mem::replace(state, next).secret_file;
        let _ = std::fs::remove_file(self.dir.join(old));
        Ok(())
    }

    fn mutate(
        &self,
        expected: Option<u64>,
        change: impl FnOnce(&mut McpRegistrySnapshot, &mut SecretStore) -> Result<(), RpcError>,
    ) -> Result<McpRegistrySnapshot, RpcError> {
        self.ensure_usable()?;
        let mut state = self.state.lock();
        if expected.is_some_and(|r| r != state.snapshot.revision) {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "MCP registry revision conflict",
            ));
        }
        let mut snapshot = state.snapshot.clone();
        let mut secrets = state.secrets.clone();
        change(&mut snapshot, &mut secrets)?;
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| RpcError::internal("MCP revision exhausted"))?;
        self.publish(&mut state, snapshot.clone(), secrets)?;
        self.changes.send_replace(snapshot.revision);
        Ok(snapshot)
    }

    pub fn upsert(&self, params: McpUpsertParams) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(params.expected_revision, |snapshot, secrets| {
            apply_upsert(snapshot, secrets, params.server, params.secrets)
        })
    }

    pub fn remove(&self, id: &McpServerId) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(None, |snapshot, secrets| {
            if !snapshot.servers.iter().any(|s| &s.id == id) {
                return Err(RpcError::not_found("MCP server not found"));
            }
            snapshot.servers.retain(|s| &s.id != id);
            for entries in snapshot.overrides.values_mut() {
                entries.remove(id);
            }
            snapshot.overrides.retain(|_, entries| !entries.is_empty());
            secrets.remove_server(id);
            Ok(())
        })
    }

    pub fn set_override(&self, params: McpOverrideParams) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(None, |snapshot, _| {
            if !snapshot.servers.iter().any(|s| s.id == params.server_id) {
                return Err(RpcError::not_found("MCP server not found"));
            }
            let entries = snapshot
                .overrides
                .entry(params.workspace_id.clone())
                .or_default();
            if params.value == McpWorkspaceOverride::Inherit {
                entries.remove(&params.server_id);
            } else {
                entries.insert(params.server_id, params.value);
            }
            if entries.is_empty() {
                snapshot.overrides.remove(&params.workspace_id);
            }
            Ok(())
        })
    }

    /// The servers an agent of `backend` in `ws` gets: enabled, enabled for
    /// that backend, and on for the workspace after its override.
    pub fn resolve(&self, ws: &WorkspaceId, backend: AgentAdapterKind) -> Vec<McpServerConfig> {
        let Some(backend) = backend_name(backend) else {
            return vec![];
        };
        let snapshot = self.snapshot();
        snapshot
            .servers
            .iter()
            .filter(|s| {
                s.enabled
                    && s.enabled_backends.iter().any(|b| b == backend)
                    && match snapshot.workspace_override(ws, &s.id) {
                        McpWorkspaceOverride::Inherit => s.default_workspace_enabled,
                        McpWorkspaceOverride::Enabled => true,
                        McpWorkspaceOverride::Disabled => false,
                    }
            })
            .cloned()
            .collect()
    }

    pub fn restore_preset(&self) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(None, |snapshot, secrets| {
            let preset = config::preset();
            if snapshot.servers.iter().any(|s| s.id == preset.id) {
                return Err(RpcError::new(
                    ErrorCode::Conflict,
                    "MCP preset already exists",
                ));
            }
            apply_upsert(snapshot, secrets, preset, vec![])
        })
    }

    pub fn import_preview(
        &self,
        params: &McpImportParams,
    ) -> Result<McpImportPreviewResult, RpcError> {
        self.ensure_usable()?;
        Ok(config::parse_import(params, &self.snapshot().servers)?.preview)
    }

    /// All or nothing: any unsupported entry or unresolved collision refuses
    /// the whole import.
    pub fn import_apply(&self, params: McpImportParams) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(params.expected_revision, |snapshot, secrets| {
            let mut import = config::parse_import(&params, &snapshot.servers)?;
            if !import.preview.issues.is_empty() {
                return Err(RpcError::invalid_params(
                    "MCP import has unsupported or invalid entries",
                ));
            }
            if !import.preview.collisions.is_empty() {
                return Err(RpcError::new(
                    ErrorCode::Conflict,
                    "MCP import requires explicit collision resolutions",
                ));
            }
            for server in import.preview.servers {
                let writes = import.writes.remove(&server.id).unwrap_or_default();
                apply_upsert(snapshot, secrets, server, writes)?;
            }
            Ok(())
        })
    }
}

fn write_state(dir: &Path, state: &State) -> Result<(), RpcError> {
    let manifest = Manifest {
        snapshot: state.snapshot.clone(),
        secret_file: state.secret_file.clone(),
    };
    let secret_bytes = serde_json::to_vec(&state.secrets).map_err(|_| storage_error())?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| storage_error())?;
    let secret_path = dir.join(&state.secret_file);
    secrets::write_private(&secret_path, &secret_bytes).map_err(|_| storage_error())?;
    if crate::util::atomic::write_atomic(&dir.join("registry.json"), &manifest_bytes).is_err() {
        let _ = std::fs::remove_file(secret_path);
        return Err(storage_error());
    }
    Ok(())
}

fn apply_upsert(
    snapshot: &mut McpRegistrySnapshot,
    secrets: &mut SecretStore,
    server: McpServerConfig,
    writes: Vec<McpSecretWrite>,
) -> Result<(), RpcError> {
    config::validate(&server)?;
    let references = config::refs(&server);
    let mut seen = BTreeSet::new();
    for write in &writes {
        let (McpSecretWrite::Keep { reference }
        | McpSecretWrite::Set { reference, .. }
        | McpSecretWrite::Remove { reference }) = write;
        // Remove may name a reference the config no longer has (clearing a
        // leftover); Keep and Set must name one it has.
        if !seen.insert(reference.clone())
            || (!references.contains(reference.as_str())
                && !matches!(write, McpSecretWrite::Remove { .. }))
        {
            return Err(RpcError::invalid_params("invalid MCP secret update"));
        }
    }
    // A reference the edit dropped takes its value with it.
    if let Some(previous) = snapshot.servers.iter().find(|s| s.id == server.id) {
        for dropped in config::refs(previous).difference(&references) {
            secrets.remove(&server.id, dropped);
        }
    }
    for write in writes {
        match write {
            // A blank value is the edit form's untouched field: keep the old one.
            McpSecretWrite::Set { reference, value } if !value.0.trim().is_empty() => {
                secrets.set(&server.id, reference.as_str(), value);
            }
            McpSecretWrite::Remove { reference } => secrets.remove(&server.id, reference.as_str()),
            _ => {}
        }
    }
    if let Some(existing) = snapshot.servers.iter_mut().find(|s| s.id == server.id) {
        *existing = server;
    } else {
        snapshot.servers.push(server);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bearer(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            name: id.into(),
            transport: McpTransportConfig::Http {
                url: "https://example.com/mcp".into(),
                auth: McpAuthConfig::Bearer {
                    token: "tok".into(),
                },
            },
            enabled: true,
            default_workspace_enabled: true,
            enabled_backends: vec![],
        }
    }

    #[test]
    fn secret_and_set_secret_roundtrip_per_server() {
        let root = tempfile::tempdir().unwrap();
        let servers = McpServers::load(root.path()).unwrap();
        servers
            .upsert(McpUpsertParams {
                server: bearer("a"),
                secrets: vec![McpSecretWrite::Set {
                    reference: "tok".into(),
                    value: McpSecretValue("one".into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        let revision = servers.snapshot().revision;
        let a: McpServerId = "a".into();
        assert_eq!(servers.secret(&a, "tok").unwrap().0, "one");
        assert!(servers.secret(&"jira".into(), "tok").is_none());

        // OAuth tokens: stored under a name outside the config's references,
        // without a registry revision, and kept across the owner's edits.
        servers
            .set_secret(&a, "oauth.access", Some(McpSecretValue("two".into())))
            .unwrap();
        assert_eq!(servers.snapshot().revision, revision);
        servers
            .upsert(McpUpsertParams {
                server: bearer("a"),
                secrets: vec![],
                expected_revision: None,
            })
            .unwrap();
        let reloaded = McpServers::load(root.path()).unwrap();
        assert_eq!(reloaded.secret(&a, "oauth.access").unwrap().0, "two");
        reloaded.set_secret(&a, "oauth.access", None).unwrap();
        assert!(McpServers::load(root.path())
            .unwrap()
            .secret(&a, "oauth.access")
            .is_none());
        assert_eq!(
            reloaded
                .set_secret(&"nope".into(), "x", None)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }
}
