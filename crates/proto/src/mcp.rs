//! Generic MCP registry wire contracts. Secret values exist only in write parameters.
use crate::WorkspaceId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.into())
            }
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
string_id!(McpServerId);
string_id!(SecretRef);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: McpServerId,
    pub name: String,
    pub transport: McpTransportConfig,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default = "enabled_by_default")]
    pub default_workspace_enabled: bool,
    /// Stable backend IDs, allowing future backends without a protocol enum bump.
    #[serde(default = "default_backends")]
    pub enabled_backends: Vec<String>,
}
fn enabled_by_default() -> bool {
    true
}
fn default_backends() -> Vec<String> {
    ["claude", "codex", "opencode"].map(String::from).to_vec()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Nonsecret environment only; credentials belong in `secret_env`.
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Names of environment variables whose values are secrets. Only the
        /// names travel in the registry; the values are write-only
        /// ([`McpSecretWrite`]) and reach the process through the daemon.
        #[serde(default)]
        secret_env: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        auth: McpAuthConfig,
    },
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpAuthConfig {
    #[default]
    None,
    Bearer {
        token: SecretRef,
    },
    Headers {
        headers: BTreeMap<String, SecretRef>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_metadata_url: Option<String>,
        #[serde(default)]
        client_secret: Option<SecretRef>,
        #[serde(default)]
        scopes: Vec<String>,
    },
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpWorkspaceOverride {
    #[default]
    Inherit,
    Enabled,
    Disabled,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpRegistrySnapshot {
    pub revision: u64,
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub overrides: BTreeMap<WorkspaceId, BTreeMap<McpServerId, McpWorkspaceOverride>>,
}
impl McpRegistrySnapshot {
    pub fn workspace_override(
        &self,
        workspace: &WorkspaceId,
        server: &McpServerId,
    ) -> McpWorkspaceOverride {
        self.overrides
            .get(workspace)
            .and_then(|entries| entries.get(server))
            .copied()
            .unwrap_or_default()
    }
}
/// Wire-serializable write-only value. Debug must never expose its contents.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpSecretValue(pub String);
impl fmt::Debug for McpSecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum McpSecretWrite {
    Keep {
        reference: SecretRef,
    },
    Set {
        reference: SecretRef,
        value: McpSecretValue,
    },
    Remove {
        reference: SecretRef,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpUpsertParams {
    pub server: McpServerConfig,
    #[serde(default)]
    pub secrets: Vec<McpSecretWrite>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerIdParams {
    pub server_id: McpServerId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpImportParams {
    /// Raw import documents may embed credentials and are never Debug-printed.
    pub document: McpSecretValue,
    /// Explicit source-name to stable-ID resolutions for collisions.
    #[serde(default)]
    pub resolutions: BTreeMap<String, McpServerId>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpOverrideParams {
    pub server_id: McpServerId,
    pub workspace_id: WorkspaceId,
    pub value: McpWorkspaceOverride,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConnectParams {
    pub server_id: McpServerId,
    pub redirect_uri: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCallbackParams {
    pub server_id: McpServerId,
    pub state: McpSecretValue,
    pub code: McpSecretValue,
    #[serde(default)]
    pub issuer: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTestParams {
    /// The test runs host-side against the upstream, so it names no workspace
    /// or backend.
    pub server_id: McpServerId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub server_id: McpServerId,
    pub state: McpConnectionState,
    /// Machine-readable reason only: never upstream errors containing credentials.
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListResult {
    pub registry: McpRegistrySnapshot,
    pub statuses: Vec<McpServerStatus>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConnectResult {
    pub authorization_url: String,
    pub expires_at: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpImportIssue {
    pub entry: String,
    pub field: String,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpImportPreviewResult {
    pub servers: Vec<McpServerConfig>,
    pub issues: Vec<McpImportIssue>,
    pub collisions: Vec<McpServerId>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTestResult {
    pub status: McpServerStatus,
    pub tool_names: Vec<String>,
}
