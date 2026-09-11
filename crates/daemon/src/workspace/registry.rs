use super::Workspace;
use anyhow::{Context, Result};
use bondsymphonic_proto::{RpcError, WorkspaceId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// The registry format this daemon writes.
///
/// 1. The original.
/// 2. Every workspace carries a host allowlist that is actually enforced. A
///    version-1 file was written when the field existed but nothing read it, so
///    its workspaces have an empty list — see [`migrate`].
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    /// Defaults to 0, which is older than any version there has been, so a file
    /// without the key is migrated rather than rejected.
    #[serde(default)]
    version: u32,
    workspaces: Vec<Workspace>,
}

/// Brings stored workspaces up to [`FORMAT_VERSION`], answering whether
/// anything changed and the file is worth rewriting.
///
/// The only migration so far fills an empty `allowlist` with the defaults.
/// Before version 2 the field was written but never consulted, so every
/// workspace created then has an empty list — which, once the proxy enforces
/// it, means the workspace can reach nothing at all and the agent inside it
/// stops being able to call the Anthropic API.
///
/// It is keyed on the file's version and not on the list being empty, because
/// `workspace.set_allowlist` can empty a list deliberately: from version 2 on,
/// an empty list means "reach nothing" and has to survive a restart intact.
fn migrate(workspaces: &mut [Workspace], version: u32) -> bool {
    if version >= FORMAT_VERSION {
        return false;
    }
    for ws in workspaces.iter_mut() {
        if ws.allowlist.is_empty() {
            ws.allowlist = crate::net::allowlist::DEFAULT_ALLOW
                .iter()
                .map(|h| h.to_string())
                .collect();
            tracing::info!(ws = %ws.id, "filled an empty allowlist with the defaults");
        }
    }
    true
}

struct Inner {
    path: PathBuf,
    workspaces: Vec<Workspace>,
}

/// Persistent workspace registry. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<Inner>>,
}

impl Registry {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let stored = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<FileFormat>(&s)
                .with_context(|| format!("parsing {}", path.display()))?,
            // No file yet: nothing to migrate, and nothing to write until the
            // first workspace is inserted.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileFormat {
                version: FORMAT_VERSION,
                workspaces: Vec::new(),
            },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut inner = Inner {
            path: path.to_path_buf(),
            workspaces: stored.workspaces,
        };
        if migrate(&mut inner.workspaces, stored.version) {
            // The migrated list is already correct in memory, and every insert
            // or update rewrites the file anyway, so a data directory we cannot
            // write to is a reason to complain rather than to refuse to start.
            if let Err(e) = save_locked(&inner) {
                tracing::warn!(path = %path.display(), error = %e, "could not rewrite the migrated registry");
            }
        }
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub fn list(&self) -> Vec<Workspace> {
        self.inner.read().workspaces.clone()
    }

    pub fn get(&self, id: &WorkspaceId) -> Option<Workspace> {
        self.inner
            .read()
            .workspaces
            .iter()
            .find(|w| &w.id == id)
            .cloned()
    }

    pub fn find_by_name(&self, repo: &std::path::Path, name: &str) -> Option<Workspace> {
        self.inner
            .read()
            .workspaces
            .iter()
            .find(|w| w.repo_path == repo && w.name == name)
            .cloned()
    }

    pub fn insert(&self, ws: Workspace) -> Result<()> {
        let mut g = self.inner.write();
        g.workspaces.retain(|w| w.id != ws.id);
        g.workspaces.push(ws);
        save_locked(&g)
    }

    pub fn update(
        &self,
        id: &WorkspaceId,
        f: impl FnOnce(&mut Workspace),
    ) -> Result<Workspace, RpcError> {
        let mut g = self.inner.write();
        let ws = g
            .workspaces
            .iter_mut()
            .find(|w| &w.id == id)
            .ok_or_else(|| RpcError::not_found(format!("workspace {id}")))?;
        f(ws);
        let out = ws.clone();
        save_locked(&g).map_err(|e| RpcError::io(&std::io::Error::other(e.to_string())))?;
        Ok(out)
    }

    pub fn remove(&self, id: &WorkspaceId) -> Result<Option<Workspace>> {
        let mut g = self.inner.write();
        let pos = g.workspaces.iter().position(|w| &w.id == id);
        let removed = pos.map(|i| g.workspaces.remove(i));
        save_locked(&g)?;
        Ok(removed)
    }
}

/// Writes the whole registry, atomically.
///
/// The registry is the daemon's only record of which workspaces exist and where
/// their worktrees are; a half-written one at the next start means workspaces
/// the user can neither open nor destroy, with their worktrees and object
/// directories left on disk. [`crate::util::atomic::write_atomic`] is what makes
/// the file on disk either the previous registry or this one.
fn save_locked(inner: &Inner) -> Result<()> {
    let data = FileFormat {
        version: FORMAT_VERSION,
        workspaces: inner.workspaces.clone(),
    };
    crate::util::atomic::write_atomic(&inner.path, &serde_json::to_vec_pretty(&data)?)?;
    Ok(())
}
