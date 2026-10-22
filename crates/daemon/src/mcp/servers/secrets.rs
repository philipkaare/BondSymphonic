//! The private file that holds MCP secret values.
//!
//! Secrets are scoped per server id: `server id -> name -> value`. For a stdio
//! server the name is the environment variable; for a remote server it is the
//! `SecretRef` of its bearer token, header or OAuth client secret (and, later,
//! the OAuth tokens the daemon stores itself).
use bondsymphonic_proto::{McpSecretValue, McpServerId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::Path};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SecretStore(pub BTreeMap<McpServerId, BTreeMap<String, McpSecretValue>>);

impl SecretStore {
    #[allow(dead_code)]
    pub fn get(&self, id: &McpServerId, name: &str) -> Option<&McpSecretValue> {
        self.0.get(id)?.get(name)
    }
    pub fn set(&mut self, id: &McpServerId, name: &str, value: McpSecretValue) {
        self.0
            .entry(id.clone())
            .or_default()
            .insert(name.to_owned(), value);
    }
    pub fn remove(&mut self, id: &McpServerId, name: &str) {
        if let Some(own) = self.0.get_mut(id) {
            own.remove(name);
            if own.is_empty() {
                self.0.remove(id);
            }
        }
    }
    pub fn remove_server(&mut self, id: &McpServerId) {
        self.0.remove(id);
    }
}

/// Whether `name` is a temporary [`write_private`] (through `temp_path`) left
/// behind by a crash: `.secrets-<version>.<pid>.<counter>.tmp`. Anything that
/// merely looks similar is somebody else's file and stays.
pub(crate) fn is_private_temporary(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(".secrets-")
        .and_then(|s| s.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = body.rsplitn(3, '.');
    let counter = parts.next().unwrap_or("");
    let pid = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    digits(counter)
        && digits(pid)
        && !version.is_empty()
        && version.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// Like `write_atomic`, but the temporary is `0600` from creation, so the
/// credential bytes never exist under a laxer mode, not even while landing.
/// On Windows (tests only: the daemon runs in WSL) the file keeps the
/// directory's inherited ACL.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = crate::util::atomic::temp_path(path);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Whether `name` is a secrets version file: `secrets-<random>`.
pub(crate) fn is_version_name(name: &str) -> bool {
    name.strip_prefix("secrets-")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_alphanumeric()))
}
