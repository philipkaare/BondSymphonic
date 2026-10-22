//! Shared daemon-owned MCP registry. Public state never contains credential values.
pub mod auth;
pub mod bridge;
pub mod broker;
pub(crate) mod config;
pub(crate) mod secrets;
pub mod transport;

use bondsymphonic_proto::*;
use parking_lot::Mutex;
use secrets::{SecretRecord, SecretStore};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    #[serde(flatten)]
    snapshot: McpRegistrySnapshot,
    secret_file: String,
    #[serde(default)]
    generations: std::collections::BTreeMap<McpServerId, u64>,
}
struct State {
    manifest: Manifest,
    secrets: SecretStore,
    // Durable generations prevent old OAuth accounts reviving after edits,
    // deletion/re-addition, or daemon restart.
    generations: std::collections::BTreeMap<McpServerId, u64>,
}
pub struct McpManager {
    pub(crate) root: PathBuf,
    state: Mutex<State>,
    changes: tokio::sync::watch::Sender<u64>,
}
fn storage_error() -> RpcError {
    RpcError::new(ErrorCode::IoError, "MCP storage operation failed")
}
impl McpManager {
    /// root is the daemon state directory; MCP files live in its private mcp child.
    pub fn load(root: PathBuf) -> Result<Self, RpcError> {
        let root = root.join("mcp");
        let path = root.join("registry.json");
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                let manifest: Manifest =
                    serde_json::from_slice(&bytes).map_err(|_| storage_error())?;
                if !config::safe_id(&manifest.secret_file) {
                    return Err(storage_error());
                }
                if manifest
                    .generations
                    .keys()
                    .any(|id| !config::safe_id(id.as_str()))
                {
                    return Err(storage_error());
                }
                let mut ids = BTreeSet::new();
                for server in &manifest.snapshot.servers {
                    config::validate(server)?;
                    if !ids.insert(&server.id) {
                        return Err(storage_error());
                    }
                }
                let secrets: SecretStore = serde_json::from_slice(
                    &std::fs::read(root.join(&manifest.secret_file))
                        .map_err(|_| storage_error())?,
                )
                .map_err(|_| storage_error())?;
                for (reference, record) in &secrets.0 {
                    if !config::safe_id(reference.as_str()) || !ids.contains(&record.owner) {
                        return Err(storage_error());
                    }
                }
                let mut reference_owners = std::collections::BTreeMap::new();
                for server in &manifest.snapshot.servers {
                    for reference in config::refs(server) {
                        if reference_owners
                            .insert(reference.clone(), &server.id)
                            .is_some()
                            || secrets
                                .0
                                .get(&reference)
                                .is_some_and(|record| record.owner != server.id)
                        {
                            return Err(storage_error());
                        }
                    }
                }
                State {
                    generations: manifest.generations.clone(),
                    manifest,
                    secrets,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let manifest = Manifest {
                    snapshot: McpRegistrySnapshot {
                        revision: 0,
                        servers: vec![config::preset()],
                        overrides: Default::default(),
                    },
                    secret_file: "secrets-0".into(),
                    generations: Default::default(),
                };
                secrets::write_private(&root.join(&manifest.secret_file), b"{}")
                    .map_err(|_| storage_error())?;
                crate::util::atomic::write_atomic(
                    &path,
                    &serde_json::to_vec_pretty(&manifest).map_err(|_| storage_error())?,
                )
                .map_err(|_| storage_error())?;
                State {
                    manifest,
                    secrets: SecretStore::default(),
                    generations: Default::default(),
                }
            }
            Err(_) => return Err(storage_error()),
        };
        // A crash before publishing the manifest may leave an unreferenced
        // version. It must not keep credentials from a later deletion alive.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if ((name.starts_with("secrets-") && name != state.manifest.secret_file)
                    || secrets::is_private_temporary(&name))
                    && entry.file_type().is_ok_and(|t| t.is_file())
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let (changes, _) = tokio::sync::watch::channel(state.manifest.snapshot.revision);
        Ok(Self {
            root,
            state: Mutex::new(state),
            changes,
        })
    }
    pub fn snapshot(&self) -> McpRegistrySnapshot {
        self.state.lock().manifest.snapshot.clone()
    }
    /// Changes to this server's configuration or owned credentials invalidate
    /// in-flight authorization and bridge sessions. Other servers/overrides do
    /// not change this generation; subscribers must also re-resolve enablement.
    pub fn generation(&self, id: &McpServerId) -> u64 {
        self.state.lock().generations.get(id).copied().unwrap_or(0)
    }
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub fn server(&self, id: &McpServerId) -> Result<McpServerConfig, RpcError> {
        self.snapshot()
            .servers
            .into_iter()
            .find(|s| &s.id == id)
            .ok_or_else(|| RpcError::not_found("MCP server not found"))
    }
    pub(crate) fn secret(&self, reference: &SecretRef) -> Option<String> {
        self.state
            .lock()
            .secrets
            .0
            .get(reference)
            .map(|s| s.value.0.clone())
    }
    pub fn list(&self) -> McpListResult {
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
        McpListResult { registry, statuses }
    }
    pub fn export(&self) -> Result<String, RpcError> {
        serde_json::to_string_pretty(&self.snapshot())
            .map_err(|_| RpcError::internal("MCP export failed"))
    }
    fn mutate(
        &self,
        expected: Option<u64>,
        change: impl FnOnce(&mut McpRegistrySnapshot, &mut SecretStore) -> Result<(), RpcError>,
    ) -> Result<McpRegistrySnapshot, RpcError> {
        let mut state = self.state.lock();
        if expected.is_some_and(|r| r != state.manifest.snapshot.revision) {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "MCP registry revision conflict",
            ));
        }
        let mut snapshot = state.manifest.snapshot.clone();
        let mut secrets = state.secrets.clone();
        change(&mut snapshot, &mut secrets)?;
        let mut generations = state.generations.clone();
        let ids: BTreeSet<_> = state
            .manifest
            .snapshot
            .servers
            .iter()
            .chain(snapshot.servers.iter())
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            let previous = state.manifest.snapshot.servers.iter().find(|s| s.id == id);
            let next = snapshot.servers.iter().find(|s| s.id == id);
            let owned_before: Vec<_> = state
                .secrets
                .0
                .iter()
                .filter(|(_, r)| r.owner == id)
                .map(|(k, r)| (k, &r.value))
                .collect();
            let owned_after: Vec<_> = secrets
                .0
                .iter()
                .filter(|(_, r)| r.owner == id)
                .map(|(k, r)| (k, &r.value))
                .collect();
            if previous != next || owned_before != owned_after {
                let epoch = generations.entry(id).or_default();
                *epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| RpcError::internal("MCP generation exhausted"))?;
            }
        }
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| RpcError::internal("MCP revision exhausted"))?;
        let file = format!(
            "secrets-{}-{}",
            snapshot.revision,
            hex::encode(rand::random::<[u8; 8]>())
        );
        let manifest = Manifest {
            snapshot: snapshot.clone(),
            secret_file: file.clone(),
            generations: generations.clone(),
        };
        let bytes = serde_json::to_vec(&secrets).map_err(|_| storage_error())?;
        let registry_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| storage_error())?;
        secrets::write_private(&self.root.join(&file), &bytes).map_err(|_| storage_error())?;
        if crate::util::atomic::write_atomic(&self.root.join("registry.json"), &registry_bytes)
            .is_err()
        {
            let _ = std::fs::remove_file(self.root.join(&file));
            return Err(storage_error());
        }
        let old_file = state.manifest.secret_file.clone();
        *state = State {
            manifest,
            secrets,
            generations,
        };
        self.changes.send_replace(snapshot.revision);
        let _ = std::fs::remove_file(self.root.join(old_file));
        Ok(snapshot)
    }
    pub fn upsert(&self, params: McpUpsertParams) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(params.expected_revision, |snapshot, secrets| {
            apply_upsert(snapshot, secrets, params.server, params.secrets)
        })
    }
    pub fn remove(
        &self,
        id: &McpServerId,
        expected: Option<u64>,
    ) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(expected, |snapshot, secrets| {
            if !snapshot.servers.iter().any(|s| &s.id == id) {
                return Err(RpcError::not_found("MCP server not found"));
            }
            snapshot.servers.retain(|s| &s.id != id);
            for entries in snapshot.overrides.values_mut() {
                entries.remove(id);
            }
            secrets.0.retain(|_, r| &r.owner != id);
            Ok(())
        })
    }
    pub fn set_override(
        &self,
        params: McpOverrideParams,
        expected: Option<u64>,
    ) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(expected, |snapshot, _| {
            if !snapshot.servers.iter().any(|s| s.id == params.server_id) {
                return Err(RpcError::not_found("MCP server not found"));
            }
            let entries = snapshot.overrides.entry(params.workspace_id).or_default();
            if params.value == McpWorkspaceOverride::Inherit {
                entries.remove(&params.server_id);
            } else {
                entries.insert(params.server_id, params.value);
            }
            Ok(())
        })
    }
    pub fn resolve(
        &self,
        workspace: WorkspaceId,
        backend: impl AsRef<str>,
    ) -> Vec<McpServerConfig> {
        let snapshot = self.snapshot();
        snapshot
            .servers
            .iter()
            .filter(|s| {
                s.enabled
                    && s.enabled_backends.iter().any(|b| b == backend.as_ref())
                    && match snapshot.workspace_override(&workspace, &s.id) {
                        McpWorkspaceOverride::Inherit => s.default_workspace_enabled,
                        McpWorkspaceOverride::Enabled => true,
                        McpWorkspaceOverride::Disabled => false,
                    }
            })
            .cloned()
            .collect()
    }
    pub fn restore_preset(&self, expected: Option<u64>) -> Result<McpRegistrySnapshot, RpcError> {
        self.mutate(expected, |snapshot, secrets| {
            if snapshot.servers.iter().any(|s| s.id == config::preset().id) {
                return Err(RpcError::new(
                    ErrorCode::Conflict,
                    "MCP preset already exists",
                ));
            }
            apply_upsert(snapshot, secrets, config::preset(), vec![])
        })
    }
    pub fn import_preview(
        &self,
        params: &McpImportParams,
    ) -> Result<McpImportPreviewResult, RpcError> {
        Ok(config::parse_import(params, &self.snapshot().servers)?.preview)
    }
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
fn apply_upsert(
    snapshot: &mut McpRegistrySnapshot,
    secrets: &mut SecretStore,
    server: McpServerConfig,
    writes: Vec<McpSecretWrite>,
) -> Result<(), RpcError> {
    config::validate(&server)?;
    let references = config::refs(&server);
    let mut seen = BTreeSet::new();
    for reference in &references {
        if secrets
            .0
            .get(reference)
            .is_some_and(|s| s.owner != server.id)
            || snapshot
                .servers
                .iter()
                .any(|s| s.id != server.id && config::refs(s).contains(reference))
        {
            return Err(RpcError::invalid_params(
                "MCP secret belongs to another server",
            ));
        }
    }
    for write in &writes {
        let reference = match write {
            McpSecretWrite::Keep { reference }
            | McpSecretWrite::Set { reference, .. }
            | McpSecretWrite::Remove { reference } => reference,
        };
        if !config::safe_id(reference.as_str())
            || !seen.insert(reference.clone())
            || secrets
                .0
                .get(reference)
                .is_some_and(|s| s.owner != server.id)
            || (!references.contains(reference) && !matches!(write, McpSecretWrite::Remove { .. }))
        {
            return Err(RpcError::invalid_params("invalid MCP secret update"));
        }
    }
    for write in writes {
        match write {
            McpSecretWrite::Set { reference, value } if !value.0.trim().is_empty() => {
                secrets.0.insert(
                    reference,
                    SecretRecord {
                        owner: server.id.clone(),
                        value,
                    },
                );
            }
            McpSecretWrite::Remove { reference } => {
                secrets.0.remove(&reference);
            }
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
