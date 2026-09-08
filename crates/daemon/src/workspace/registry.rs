use super::Workspace;
use anyhow::{Context, Result};
use bondsymphonic_proto::{RpcError, WorkspaceId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    workspaces: Vec<Workspace>,
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
        let workspaces = match std::fs::read_to_string(path) {
            Ok(s) => {
                serde_json::from_str::<FileFormat>(&s)
                    .with_context(|| format!("parsing {}", path.display()))?
                    .workspaces
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            inner: Arc::new(RwLock::new(Inner {
                path: path.to_path_buf(),
                workspaces,
            })),
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

    pub fn save(&self) -> Result<()> {
        save_locked(&self.inner.read())
    }
}

fn save_locked(inner: &Inner) -> Result<()> {
    if let Some(dir) = inner.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = inner.path.with_extension("json.tmp");
    let data = FileFormat {
        version: 1,
        workspaces: inner.workspaces.clone(),
    };
    std::fs::write(&tmp, serde_json::to_vec_pretty(&data)?)?;
    std::fs::rename(&tmp, &inner.path)?;
    Ok(())
}
