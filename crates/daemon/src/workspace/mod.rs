pub mod registry;

use bondsymphonic_proto::{AgentId, RunId, WorkspaceId, WorkspaceInfo, WorkspaceState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub repo_path: PathBuf,
    pub base_branch: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub created_at: String,
    pub allowlist: Vec<String>,
    pub state: WorkspaceState,
    pub agents: Vec<AgentId>,
    pub runs: Vec<RunId>,
}

impl Workspace {
    pub fn branch_for(name: &str) -> String {
        format!("bs/{name}/work")
    }
    pub fn info(&self) -> WorkspaceInfo {
        WorkspaceInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            repo_path: self.repo_path.to_string_lossy().into_owned(),
            base_branch: self.base_branch.clone(),
            branch: self.branch.clone(),
            worktree_path: self.worktree_path.to_string_lossy().into_owned(),
            created_at: self.created_at.clone(),
            allowlist: self.allowlist.clone(),
            state: self.state.clone(),
            agents: self.agents.clone(),
            runs: self.runs.clone(),
        }
    }
}

/// Layout of the daemon data directory (default `~/.bondsymphonic`).
#[derive(Debug, Clone)]
pub struct DataDirs {
    pub root: PathBuf,
    pub worktrees: PathBuf,
    pub objects: PathBuf,
    pub homes: PathBuf,
    pub caches: PathBuf,
    pub run: PathBuf,
    pub transcripts: PathBuf,
}

impl DataDirs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            worktrees: root.join("worktrees"),
            objects: root.join("objects"),
            homes: root.join("homes"),
            caches: root.join("caches"),
            run: root.join("run"),
            transcripts: root.join("transcripts"),
            root,
        }
    }
    pub fn registry_file(&self) -> PathBuf {
        self.root.join("workspaces.json")
    }
    pub fn worktree(&self, id: &WorkspaceId) -> PathBuf {
        self.worktrees.join(id.as_str())
    }
    pub fn objects(&self, id: &WorkspaceId) -> PathBuf {
        self.objects.join(id.as_str())
    }
    pub fn home(&self, id: &WorkspaceId) -> PathBuf {
        self.homes.join(id.as_str())
    }
    pub fn cache(&self, id: &WorkspaceId) -> PathBuf {
        self.caches.join(id.as_str())
    }
    pub fn run(&self, id: &WorkspaceId) -> PathBuf {
        self.run.join(id.as_str())
    }
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            &self.worktrees,
            &self.objects,
            &self.homes,
            &self.caches,
            &self.run,
            &self.transcripts,
        ] {
            std::fs::create_dir_all(d)?;
        }
        std::fs::create_dir_all(self.bin())?;
        Ok(())
    }
    /// Creates (and returns) every per-workspace directory.
    pub fn ensure_workspace(&self, id: &WorkspaceId) -> std::io::Result<()> {
        for p in [
            self.objects(id),
            self.home(id),
            self.cache(id),
            self.run(id),
        ] {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }
    pub fn remove_workspace(&self, id: &WorkspaceId) {
        for p in [
            self.worktree(id),
            self.objects(id),
            self.home(id),
            self.cache(id),
            self.run(id),
        ] {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
