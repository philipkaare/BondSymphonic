# Milestone 2a: Daemon Workspaces, Sandbox, PTY — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon can create a workspace (git worktree on a protected branch plus a bubblewrap sandbox with an in-sandbox init), list/destroy it, open PTYs and run commands inside the sandbox, and serve file listing/read/write for the worktree, all over the existing protocol, with integration tests proving the isolation properties.

**Architecture:** New daemon modules `workspace/`, `git/`, `sandbox/`, `pty.rs`, `fs.rs`, each behind a small trait or struct, composed into a `WorkspaceHandler` that wraps the existing `SystemHandler`. One `bwrap` per workspace runs `bondsymphonic-daemon sandbox-init`, a tiny init that spawns processes on request over a Unix socket and hands back stdio or PTY fds with `SCM_RIGHTS`. A `noop` backend runs the same protocol without bwrap so tests and Windows builds work.

**Tech Stack:** Rust stable, tokio, `nix` (Unix sockets, fd passing, openpty, signals), `portable-pty` (noop backend PTY on Windows/Linux), `serde_json`, `tempfile` (tests), git CLI, bubblewrap 0.9 in the `bondsymphonic` distro.

**Spec:** `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §2–§6, §9, §11, §13; `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` §6.3 (workspace, fs, pty methods), §6.4, §10 milestone 2.

## Global Constraints

- Rust edition 2021; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all`.
- The daemon crate must still build and pass its non-sandbox tests on Windows (`cargo test -p bondsymphonic-daemon`); Linux-only code is gated with `#[cfg(unix)]` / `#[cfg(target_os = "linux")]`, and sandbox integration tests skip with a printed reason when `bwrap` is unavailable.
- Protocol types in `bondsymphonic-proto` are the contract; do not change wire shapes. Adding a field requires `#[serde(default)]`.
- Requests on one connection are handled concurrently; every shared structure is behind `Arc<Mutex/RwLock>` or a task-owned channel. Never hold a `std::sync::Mutex` across an `.await`.
- Workspace branch naming is exactly `bs/<name>/work`; worktrees live under `<data_dir>/worktrees/<ws_id>/`; private objects under `<data_dir>/objects/<ws_id>/`.
- Inside the sandbox the main repo's `.git` is read-only except `worktrees/<ws_id>/`, `refs/heads/bs/<name>/`, `logs/refs/heads/bs/<name>/`. `GIT_OBJECT_DIRECTORY` points at the private objects dir and `GIT_ALTERNATE_OBJECT_DIRECTORIES` at the main `objects/`.
- Every workspace failure is a `workspace.state` event or an `RpcError` for that workspace; the daemon never exits because of one workspace.
- All git commands: `GIT_TERMINAL_PROMPT=0`, 60 s timeout, errors map to `RpcError { code: GitError, data: {command, exit_code, stderr} }`.
- Paths sent by clients are relative to the worktree root; anything escaping it (including via symlink) is `InvalidParams`.
- Commit after every task (conventional commits) on branch `m2a-daemon-workspaces`; push is the user's call.
- Scratch files go in the session scratchpad, never `/tmp`.

---

## File structure for this milestone

```
crates/daemon/
  Cargo.toml                       + nix, portable-pty, tempfile (dev), uuid? (no: ids from rand), chrono
  src/lib.rs                       + pub mod workspace, git, sandbox, pty, fs, ids
  src/ids.rs                       new_id(prefix) -> "ws_" + 8 hex
  src/git/mod.rs                   Git runner (run, run_in), GitError -> RpcError
  src/git/repo.rs                  inspect(path) -> RepoInfo
  src/git/worktree.rs              WorktreeLayout, create_worktree, remove_worktree
  src/workspace/mod.rs             Workspace, WorkspaceState conversions, DataDirs
  src/workspace/registry.rs        Registry: load/save (atomic), insert/remove/get/list
  src/workspace/lifecycle.rs       create(), destroy(), status(); emits events
  src/sandbox/mod.rs               SandboxBackend, SandboxHandle, SandboxSpec, SandboxCommand, SandboxChild, PtySize
  src/sandbox/protocol.rs          init <-> daemon messages over the exec socket (serde_json, one line + fds)
  src/sandbox/init.rs              `sandbox-init` subcommand (Linux): PID 1 duties, spawn, fd passing, reaping
  src/sandbox/noop.rs              NoopBackend: spawns directly (pipes or portable-pty), same SandboxHandle API
  src/sandbox/linux_bwrap.rs       BwrapBackend: bwrap args from spec, starts init, connects exec.sock
  src/sandbox/exec_client.rs       Linux client side of the exec socket (connect, spawn, receive fds)
  src/pty.rs                       PtyManager: open/write/resize/close; output pump -> events
  src/fs.rs                        FsService: list_dir/read_file/write_file with containment
  src/server/handlers.rs           WorkspaceHandler (wraps SystemHandler): workspace.*, pty.*, fs.* dispatch
  src/main.rs                      + `sandbox-init` subcommand, backend selection (--no-sandbox), Capabilities
  tests/git_worktree.rs            temp repo -> worktree layout, ref dirs, private objects, remove
  tests/registry.rs                load/save/validate
  tests/fs_service.rs              containment, read/write/list
  tests/workspace_integration.rs   over TCP: create/list/get/status/destroy + events (noop backend)
  tests/pty_integration.rs         over TCP: open shell, echo, resize, close (noop backend; unix-gated shell)
  tests/sandbox_integration.rs     Linux + bwrap only: isolation properties (skips otherwise)
```

---

### Task 1: Ids, data directories, workspace model, registry

**Files:**
- Create: `crates/daemon/src/ids.rs`, `crates/daemon/src/workspace/mod.rs`, `crates/daemon/src/workspace/registry.rs`
- Modify: `crates/daemon/src/lib.rs`, `crates/daemon/Cargo.toml`
- Test: `crates/daemon/tests/registry.rs`

**Interfaces:**
- Produces: `ids::new_id(prefix: &str) -> String` (prefix + 8 lowercase hex from `rand`).
- `workspace::DataDirs { root, worktrees, objects, homes, caches, run, transcripts }` with `DataDirs::new(root) -> Self` and `ensure(&self) -> io::Result<()>`; per-workspace helpers `worktree(&self, id)`, `objects(&self, id)`, `home(&self, id)`, `cache(&self, id)`, `run(&self, id)`.
- `workspace::Workspace { id: WorkspaceId, name, repo_path: PathBuf, base_branch, branch, worktree_path: PathBuf, created_at: String, allowlist: Vec<String>, state: WorkspaceState, agents: Vec<AgentId>, runs: Vec<RunId> }` (serde), `Workspace::branch_for(name) -> String` (`bs/<name>/work`), `Workspace::info(&self) -> WorkspaceInfo`.
- `registry::Registry` (`Arc<RwLock<Inner>>` inside): `Registry::load(path) -> anyhow::Result<Registry>` (missing file → empty), `insert(ws)`, `remove(id) -> Option<Workspace>`, `get(id) -> Option<Workspace>`, `list() -> Vec<Workspace>`, `update(id, |ws| ...) -> Result<Workspace, RpcError>`, `save() -> anyhow::Result<()>` (temp + rename). Every mutating method saves before returning.

- [ ] **Step 1: Add dependencies**

In `crates/daemon/Cargo.toml` `[dependencies]` add:

```toml
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
parking_lot = "0.12"
```

and under `[dev-dependencies]`: `tempfile = "3"`.

- [ ] **Step 2: Write the failing test** `crates/daemon/tests/registry.rs`

```rust
use bondsymphonic_daemon::workspace::registry::Registry;
use bondsymphonic_daemon::workspace::Workspace;
use bondsymphonic_proto::WorkspaceState;

fn sample(id: &str, name: &str) -> Workspace {
    Workspace {
        id: id.into(),
        name: name.into(),
        repo_path: "/repo".into(),
        base_branch: "main".into(),
        branch: Workspace::branch_for(name),
        worktree_path: format!("/data/worktrees/{id}").into(),
        created_at: "2026-09-08T10:00:00Z".into(),
        allowlist: vec![],
        state: WorkspaceState::Ready,
        agents: vec![],
        runs: vec![],
    }
}

#[test]
fn branch_naming_convention() {
    assert_eq!(Workspace::branch_for("agent-1"), "bs/agent-1/work");
}

#[test]
fn registry_roundtrips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    assert!(reg.list().is_empty());
    reg.insert(sample("ws_00000001", "a")).unwrap();
    reg.insert(sample("ws_00000002", "b")).unwrap();
    assert_eq!(reg.list().len(), 2);

    let reg2 = Registry::load(&path).unwrap();
    let names: Vec<String> = reg2.list().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["a", "b"]);
    assert_eq!(reg2.get(&"ws_00000001".into()).unwrap().branch, "bs/a/work");
}

#[test]
fn update_and_remove_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    reg.insert(sample("ws_00000001", "a")).unwrap();
    let updated = reg
        .update(&"ws_00000001".into(), |w| w.state = WorkspaceState::Error("boom".into()))
        .unwrap();
    assert_eq!(updated.state, WorkspaceState::Error("boom".into()));
    assert!(reg.remove(&"ws_00000001".into()).unwrap().is_some());
    let reg2 = Registry::load(&path).unwrap();
    assert!(reg2.list().is_empty());
    assert!(reg.update(&"ws_nope".into(), |_| {}).is_err());
}

#[test]
fn ids_have_prefix_and_hex() {
    let id = bondsymphonic_daemon::ids::new_id("ws_");
    assert!(id.starts_with("ws_"));
    assert_eq!(id.len(), 11);
    assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(id, bondsymphonic_daemon::ids::new_id("ws_"));
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p bondsymphonic-daemon --test registry`
Expected: compile error, no `workspace` module.

- [ ] **Step 4: Implement `ids.rs`**

```rust
use rand::RngCore;

/// `prefix` + 8 lowercase hex chars, e.g. `ws_3fa9c1d2`.
pub fn new_id(prefix: &str) -> String {
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}{}", hex::encode(b))
}
```

- [ ] **Step 5: Implement `workspace/mod.rs`**

```rust
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
}

impl DataDirs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn registry_file(&self) -> PathBuf { self.root.join("workspaces.json") }
    pub fn worktree(&self, id: &WorkspaceId) -> PathBuf { self.root.join("worktrees").join(id.as_str()) }
    pub fn objects(&self, id: &WorkspaceId) -> PathBuf { self.root.join("objects").join(id.as_str()) }
    pub fn home(&self, id: &WorkspaceId) -> PathBuf { self.root.join("homes").join(id.as_str()) }
    pub fn cache(&self, id: &WorkspaceId) -> PathBuf { self.root.join("caches").join(id.as_str()) }
    pub fn run(&self, id: &WorkspaceId) -> PathBuf { self.root.join("run").join(id.as_str()) }
    pub fn transcripts(&self) -> PathBuf { self.root.join("transcripts") }
    pub fn bin(&self) -> PathBuf { self.root.join("bin") }

    pub fn ensure(&self) -> std::io::Result<()> {
        for d in ["worktrees", "objects", "homes", "caches", "run", "transcripts", "bin"] {
            std::fs::create_dir_all(self.root.join(d))?;
        }
        Ok(())
    }
    /// Creates (and returns) every per-workspace directory.
    pub fn ensure_workspace(&self, id: &WorkspaceId) -> std::io::Result<()> {
        for p in [self.objects(id), self.home(id), self.cache(id), self.run(id)] {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }
    pub fn remove_workspace(&self, id: &WorkspaceId) {
        for p in [self.worktree(id), self.objects(id), self.home(id), self.cache(id), self.run(id)] {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn path_str(p: &Path) -> String { p.to_string_lossy().into_owned() }
```

- [ ] **Step 6: Implement `workspace/registry.rs`**

```rust
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
            Ok(s) => serde_json::from_str::<FileFormat>(&s)
                .with_context(|| format!("parsing {}", path.display()))?
                .workspaces,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { inner: Arc::new(RwLock::new(Inner { path: path.to_path_buf(), workspaces })) })
    }

    pub fn list(&self) -> Vec<Workspace> { self.inner.read().workspaces.clone() }

    pub fn get(&self, id: &WorkspaceId) -> Option<Workspace> {
        self.inner.read().workspaces.iter().find(|w| &w.id == id).cloned()
    }

    pub fn find_by_name(&self, repo: &std::path::Path, name: &str) -> Option<Workspace> {
        self.inner.read().workspaces.iter().find(|w| w.repo_path == repo && w.name == name).cloned()
    }

    pub fn insert(&self, ws: Workspace) -> Result<()> {
        let mut g = self.inner.write();
        g.workspaces.retain(|w| w.id != ws.id);
        g.workspaces.push(ws);
        save_locked(&g)
    }

    pub fn update(&self, id: &WorkspaceId, f: impl FnOnce(&mut Workspace)) -> Result<Workspace, RpcError> {
        let mut g = self.inner.write();
        let ws = g.workspaces.iter_mut().find(|w| &w.id == id)
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

    pub fn save(&self) -> Result<()> { save_locked(&self.inner.read()) }
}

fn save_locked(inner: &Inner) -> Result<()> {
    if let Some(dir) = inner.path.parent() { std::fs::create_dir_all(dir)?; }
    let tmp = inner.path.with_extension("json.tmp");
    let data = FileFormat { version: 1, workspaces: inner.workspaces.clone() };
    std::fs::write(&tmp, serde_json::to_vec_pretty(&data)?)?;
    std::fs::rename(&tmp, &inner.path)?;
    Ok(())
}
```

Add to `lib.rs`: `pub mod ids; pub mod workspace;`.

- [ ] **Step 7: Run tests, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon --test registry && cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings && cargo fmt --all`
Expected: 4 tests pass.

```bash
git add crates/daemon
git commit -m "feat(daemon): workspace model, data dirs, persistent registry"
```

---

### Task 2: Git runner and repo inspection

**Files:**
- Create: `crates/daemon/src/git/mod.rs`, `crates/daemon/src/git/repo.rs`
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/tests/git_repo.rs`

**Interfaces:**
- `git::Git { env: Vec<(String, String)> }` with `Git::new()`, `Git::with_env(k, v)`, `async fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, RpcError>` where `GitOutput { stdout: String, stderr: String }`; non-zero exit → `RpcError` code `GitError`, `data: {"command": "git <args>", "exit_code": n, "stderr": "..."}`; 60 s timeout → `GitError` with `exit_code: null`.
- `git::repo::inspect(git: &Git, path: &Path) -> Result<RepoInfo, RpcError>`; `git::repo::common_dir(git, path) -> Result<PathBuf, RpcError>` (absolute `.git` dir); `git::repo::branch_exists(git, repo, branch) -> Result<bool, RpcError>`; `git::repo::head_commit(git, repo, rev) -> Result<String, RpcError>`.
- Test helper (dev, in tests): `fn init_repo(dir) -> PathBuf` creating a repo with one commit on `main`.

- [ ] **Step 1: Write the failing test** `crates/daemon/tests/git_repo.rs`

```rust
use bondsymphonic_daemon::git::{repo, Git};
use bondsymphonic_proto::ErrorCode;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git").args(args).current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    repo
}

#[tokio::test]
async fn inspect_reports_default_branch_and_clean_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = init_repo(dir.path());
    let git = Git::new();
    let info = repo::inspect(&git, &repo_path).await.unwrap();
    assert_eq!(info.default_branch, "main");
    assert!(info.branches.contains(&"main".to_string()));
    assert!(!info.is_dirty);
    assert!(info.remotes.is_empty());
    std::fs::write(repo_path.join("dirty.txt"), "x").unwrap();
    assert!(repo::inspect(&git, &repo_path).await.unwrap().is_dirty);
    assert!(repo::branch_exists(&git, &repo_path, "main").await.unwrap());
    assert!(!repo::branch_exists(&git, &repo_path, "nope").await.unwrap());
    assert_eq!(repo::head_commit(&git, &repo_path, "main").await.unwrap().len(), 40);
}

#[tokio::test]
async fn failures_map_to_git_error_with_details() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new();
    let err = git.run(dir.path(), &["rev-parse", "--git-common-dir"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::GitError);
    let data = err.data.unwrap();
    assert!(data["command"].as_str().unwrap().starts_with("git rev-parse"));
    assert!(data["exit_code"].as_i64().unwrap() != 0);
    assert!(data["stderr"].as_str().unwrap().contains("not a git repository"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p bondsymphonic-daemon --test git_repo`
Expected: compile error.

- [ ] **Step 3: Implement `git/mod.rs`**

```rust
pub mod repo;

use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct Git {
    env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    pub fn new() -> Self { Self::default() }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub async fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, RpcError> {
        let command = format!("git {}", args.join(" "));
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.env { cmd.env(k, v); }
        let out = match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(git_error(&command, None, &e.to_string())),
            Err(_) => return Err(git_error(&command, None, "timed out after 60s")),
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(git_error(&command, out.status.code(), stderr.trim()));
        }
        Ok(GitOutput { stdout, stderr })
    }
}

pub fn git_error(command: &str, exit_code: Option<i32>, stderr: &str) -> RpcError {
    RpcError::new(ErrorCode::GitError, format!("{command} failed: {stderr}"))
        .with_data(serde_json::json!({ "command": command, "exit_code": exit_code, "stderr": stderr }))
}
```

- [ ] **Step 4: Implement `git/repo.rs`**

```rust
use super::Git;
use bondsymphonic_proto::{RepoInfo, RpcError};
use std::path::{Path, PathBuf};

pub async fn common_dir(git: &Git, repo: &Path) -> Result<PathBuf, RpcError> {
    let out = git.run(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    Ok(PathBuf::from(out.stdout.trim()))
}

pub async fn branch_exists(git: &Git, repo: &Path, branch: &str) -> Result<bool, RpcError> {
    match git.run(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await {
        Ok(_) => Ok(true),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(false),
        Err(e) => Err(e),
    }
}

pub async fn head_commit(git: &Git, repo: &Path, rev: &str) -> Result<String, RpcError> {
    Ok(git.run(repo, &["rev-parse", rev]).await?.stdout.trim().to_string())
}

pub async fn inspect(git: &Git, repo: &Path) -> Result<RepoInfo, RpcError> {
    common_dir(git, repo).await?; // fails with GitError if not a repo
    let branches: Vec<String> = git.run(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]).await?
        .stdout.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let default_branch = match git.run(repo, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]).await {
        Ok(o) => o.stdout.trim().trim_start_matches("origin/").to_string(),
        Err(_) => match git.run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await {
            Ok(o) => o.stdout.trim().to_string(),
            Err(_) => branches.iter().find(|b| *b == "main" || *b == "master").cloned()
                .or_else(|| branches.first().cloned()).unwrap_or_default(),
        },
    };
    let is_dirty = !git.run(repo, &["status", "--porcelain"]).await?.stdout.trim().is_empty();
    let remotes: Vec<String> = git.run(repo, &["remote"]).await?
        .stdout.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    Ok(RepoInfo { default_branch, branches, is_dirty, remotes })
}
```

Add `pub mod git;` to `lib.rs`.

- [ ] **Step 5: Run tests, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon --test git_repo && cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings && cargo fmt --all`
Expected: 2 tests pass (git 2.43+/2.52 supports `--path-format=absolute`).

```bash
git add crates/daemon
git commit -m "feat(daemon): git runner with structured errors and repo inspection"
```

---

### Task 3: Worktree creation with protected-ref layout

**Files:**
- Create: `crates/daemon/src/git/worktree.rs`
- Modify: `crates/daemon/src/git/mod.rs` (`pub mod worktree;`)
- Test: `crates/daemon/tests/git_worktree.rs`

**Interfaces:**
- `worktree::Layout { repo: PathBuf, git_common: PathBuf, name: String, branch: String, worktree_path: PathBuf, objects_dir: PathBuf }` with methods: `ref_dir(&self) -> PathBuf` (`<git_common>/refs/heads/bs/<name>`), `reflog_dir(&self)` (`<git_common>/logs/refs/heads/bs/<name>`), `worktree_gitdir(&self)` (`<git_common>/worktrees/<basename of worktree_path>`), `rw_git_paths(&self) -> Vec<PathBuf>` (the three writable subpaths), `sandbox_git_env(&self) -> Vec<(String,String)>` (`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`), `daemon_git(&self) -> Git` (main objects primary + private alternates).
- `worktree::create(git: &Git, layout: &Layout, base_branch: &str) -> Result<(), RpcError>`: fails `Conflict` if the branch exists; pre-creates `ref_dir`, `reflog_dir`, `objects_dir`; runs `git worktree add -b <branch> <worktree_path> <base_branch>`; verifies the loose ref file `<ref_dir>/work` exists.
- `worktree::remove(git: &Git, layout: &Layout) -> Result<(), RpcError>`: `git worktree remove --force`, `git branch -D`, `git worktree prune`; ignores "not found" errors so it is idempotent.

- [ ] **Step 1: Write the failing test** `crates/daemon/tests/git_worktree.rs`

```rust
mod common { include!("git_repo.rs"); }  // reuse init_repo; or copy the helper if include! is awkward
use bondsymphonic_daemon::git::{repo, worktree::{self, Layout}, Git};
use bondsymphonic_proto::ErrorCode;

async fn layout_for(dir: &std::path::Path, repo_path: &std::path::Path, name: &str) -> Layout {
    let git = Git::new();
    let git_common = repo::common_dir(&git, repo_path).await.unwrap();
    Layout {
        repo: repo_path.to_path_buf(),
        git_common,
        name: name.into(),
        branch: format!("bs/{name}/work"),
        worktree_path: dir.join("worktrees").join("ws_00000001"),
        objects_dir: dir.join("objects").join("ws_00000001"),
    }
}

#[tokio::test]
async fn create_makes_branch_worktree_and_writable_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-1").await;

    worktree::create(&git, &layout, "main").await.unwrap();

    assert!(layout.worktree_path.join("README.md").exists());
    assert!(layout.ref_dir().join("work").is_file(), "loose ref file must exist for the rw bind");
    assert!(layout.reflog_dir().is_dir());
    assert!(layout.objects_dir.is_dir());
    assert!(layout.worktree_gitdir().join("HEAD").exists());
    assert!(repo::branch_exists(&git, &repo_path, "bs/agent-1/work").await.unwrap());
    assert_eq!(layout.rw_git_paths().len(), 3);

    // Creating again is a Conflict.
    let err = worktree::create(&git, &layout, "main").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}

#[tokio::test]
async fn commits_in_worktree_go_to_private_objects_and_daemon_can_read_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = common::init_repo(dir.path());
    let git = Git::new();
    let layout = layout_for(dir.path(), &repo_path, "agent-2").await;
    worktree::create(&git, &layout, "main").await.unwrap();

    // Simulate the sandboxed agent: git with the sandbox env.
    let mut agent_git = Git::new().with_env("GIT_AUTHOR_NAME", "a").with_env("GIT_AUTHOR_EMAIL", "a@a")
        .with_env("GIT_COMMITTER_NAME", "a").with_env("GIT_COMMITTER_EMAIL", "a@a");
    for (k, v) in layout.sandbox_git_env() { agent_git = agent_git.with_env(k, v); }
    std::fs::write(layout.worktree_path.join("new.txt"), "agent work\n").unwrap();
    agent_git.run(&layout.worktree_path, &["add", "new.txt"]).await.unwrap();
    agent_git.run(&layout.worktree_path, &["commit", "-q", "-m", "agent commit"]).await.unwrap();

    // New objects landed in the private dir, not the shared store.
    let private_count = walkdir_count(&layout.objects_dir);
    assert!(private_count >= 3, "blob+tree+commit expected in private objects, got {private_count}");

    // The daemon (outside the sandbox) can read the commit via alternates.
    let daemon_git = layout.daemon_git();
    let subject = daemon_git.run(&repo_path, &["log", "-1", "--format=%s", "bs/agent-2/work"]).await.unwrap();
    assert_eq!(subject.stdout.trim(), "agent commit");

    worktree::remove(&git, &layout).await.unwrap();
    assert!(!layout.worktree_path.exists());
    assert!(!repo::branch_exists(&git, &repo_path, "bs/agent-2/work").await.unwrap());
    worktree::remove(&git, &layout).await.unwrap(); // idempotent
}

fn walkdir_count(p: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() { n += walkdir_count(&path); } else { n += 1; }
        }
    }
    n
}
```

If `include!` of `git_repo.rs` pulls in its `#[tokio::test]`s twice, instead create `crates/daemon/tests/common/mod.rs` holding `init_repo` and use `mod common;` from both test files.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p bondsymphonic-daemon --test git_worktree`
Expected: compile error.

- [ ] **Step 3: Implement `git/worktree.rs`**

```rust
use super::{repo, Git};
use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub repo: PathBuf,
    pub git_common: PathBuf,
    pub name: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub objects_dir: PathBuf,
}

impl Layout {
    pub fn ref_dir(&self) -> PathBuf { self.git_common.join("refs/heads/bs").join(&self.name) }
    pub fn reflog_dir(&self) -> PathBuf { self.git_common.join("logs/refs/heads/bs").join(&self.name) }
    pub fn worktree_gitdir(&self) -> PathBuf {
        let base = self.worktree_path.file_name().map(|s| s.to_os_string()).unwrap_or_default();
        self.git_common.join("worktrees").join(base)
    }
    /// Subpaths of the main `.git` that must be bind-mounted read-write into the sandbox.
    pub fn rw_git_paths(&self) -> Vec<PathBuf> {
        vec![self.worktree_gitdir(), self.ref_dir(), self.reflog_dir()]
    }
    /// Env for git *inside* the sandbox: new objects go to the private dir.
    pub fn sandbox_git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_OBJECT_DIRECTORY".into(), s(&self.objects_dir)),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), s(&self.git_common.join("objects"))),
        ]
    }
    /// Git for the daemon side: main store primary, private objects as alternate.
    pub fn daemon_git(&self) -> Git {
        Git::new().with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", s(&self.objects_dir))
    }
}

fn s(p: &Path) -> String { p.to_string_lossy().into_owned() }

pub async fn create(git: &Git, layout: &Layout, base_branch: &str) -> Result<(), RpcError> {
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        return Err(RpcError::new(ErrorCode::Conflict, format!("branch {} already exists", layout.branch)));
    }
    if !repo::branch_exists(git, &layout.repo, base_branch).await? {
        return Err(RpcError::invalid_params(format!("base branch {base_branch} does not exist")));
    }
    for d in [layout.ref_dir(), layout.reflog_dir(), layout.objects_dir.clone()] {
        std::fs::create_dir_all(&d).map_err(|e| RpcError::io(&e))?;
    }
    if let Some(parent) = layout.worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?;
    }
    git.run(&layout.repo, &["worktree", "add", "-b", &layout.branch, &s(&layout.worktree_path), base_branch]).await?;
    if !layout.ref_dir().join("work").is_file() {
        return Err(RpcError::internal(format!("expected loose ref at {}", layout.ref_dir().join("work").display())));
    }
    Ok(())
}

pub async fn remove(git: &Git, layout: &Layout) -> Result<(), RpcError> {
    let ignore_missing = |r: Result<super::GitOutput, RpcError>| match r {
        Ok(_) => Ok(()),
        Err(e) => {
            let stderr = e.data.as_ref().and_then(|d| d["stderr"].as_str()).unwrap_or("");
            if stderr.contains("is not a working tree") || stderr.contains("not found") || stderr.contains("No such file") {
                Ok(())
            } else { Err(e) }
        }
    };
    if layout.worktree_path.exists() {
        ignore_missing(git.run(&layout.repo, &["worktree", "remove", "--force", &s(&layout.worktree_path)]).await)?;
    }
    let _ = std::fs::remove_dir_all(&layout.worktree_path);
    git.run(&layout.repo, &["worktree", "prune"]).await?;
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        git.run(&layout.repo, &["branch", "-D", &layout.branch]).await?;
    }
    let _ = std::fs::remove_dir(layout.ref_dir());
    let _ = std::fs::remove_dir_all(layout.reflog_dir());
    Ok(())
}
```

- [ ] **Step 4: Run tests on Windows and in WSL, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon --test git_worktree` and `scripts\test-daemon.ps1`
Expected: both pass. Note: on Windows `git worktree add` with a `bs/name/work` branch creates `refs/heads/bs/name/work` as a loose file; the test asserts it.

```bash
git add crates/daemon
git commit -m "feat(daemon): worktree creation with protected-ref layout and private objects"
```

---

### Task 4: Sandbox trait, exec protocol, and the noop backend

**Files:**
- Create: `crates/daemon/src/sandbox/mod.rs`, `crates/daemon/src/sandbox/protocol.rs`, `crates/daemon/src/sandbox/noop.rs`
- Modify: `crates/daemon/src/lib.rs`, `crates/daemon/Cargo.toml`
- Test: unit tests in `noop.rs`; `crates/daemon/tests/sandbox_noop.rs`

**Interfaces (used by Tasks 5, 6, 7):**

```rust
// sandbox/mod.rs
pub struct PtySize { pub cols: u16, pub rows: u16 }
pub struct SandboxSpec {
    pub id: WorkspaceId,
    pub rw_binds: Vec<(PathBuf, PathBuf)>,   // (host, sandbox)
    pub ro_binds: Vec<(PathBuf, PathBuf)>,
    pub home: PathBuf,                        // host dir mounted at /home/<user> (bwrap) / just HOME (noop)
    pub run_dir: PathBuf,                     // host dir for sockets, mounted at /run/bs (bwrap)
    pub env: Vec<(String, String)>,           // applied to every process
    pub cwd: PathBuf,
}
pub struct SandboxCommand { pub argv: Vec<String>, pub env: Vec<(String, String)>, pub cwd: Option<PathBuf>, pub pty: Option<PtySize> }
pub type ChildReader = Pin<Box<dyn AsyncRead + Send>>;
pub type ChildWriter = Pin<Box<dyn AsyncWrite + Send>>;
pub struct PtyIo { pub reader: ChildReader, pub writer: ChildWriter, pub resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> }
pub struct SandboxChild {
    pub pid: u32,
    pub stdin: Option<ChildWriter>, pub stdout: Option<ChildReader>, pub stderr: Option<ChildReader>,
    pub pty: Option<PtyIo>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    pub killer: Box<dyn Fn() + Send + Sync>,   // SIGTERM/kill the process (group)
}
#[async_trait] pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Vec<PrereqStatus>;
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError>;
}
#[async_trait] pub trait SandboxHandle: Send + Sync {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError>;
    async fn shutdown(&self) -> Result<(), RpcError>;
}
pub fn backend_for(name: &str) -> Arc<dyn SandboxBackend>  // "noop" | "linux_bwrap" (bwrap only on linux; else noop with a warning)
```

```rust
// sandbox/protocol.rs (shared by init and exec client; JSON, one line per message)
pub enum InitRequest { Spawn { id: u64, argv: Vec<String>, env: Vec<(String,String)>, cwd: Option<String>, pty: Option<(u16,u16)> }, Kill { pid: u32, signal: i32 }, Shutdown }
pub enum InitReply { Spawned { id: u64, pid: u32, has_pty: bool },   // fds attached via SCM_RIGHTS: [stdin,stdout,stderr] or [pty_master]
                     SpawnFailed { id: u64, message: String }, Exited { pid: u32, code: i32 }, ShuttingDown }
```

- [ ] **Step 1: Add dependencies** to `crates/daemon/Cargo.toml`:

```toml
portable-pty = "0.8"
futures = "0.3"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.29", features = ["socket", "uio", "fs", "process", "signal", "term", "poll"] }
libc = "0.2"
```

- [ ] **Step 2: Write the failing integration test** `crates/daemon/tests/sandbox_noop.rs`

```rust
use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn spec(dir: &std::path::Path) -> SandboxSpec {
    SandboxSpec {
        id: "ws_test".into(),
        rw_binds: vec![], ro_binds: vec![],
        home: dir.join("home"), run_dir: dir.join("run"),
        env: vec![("BS_TEST_VAR".into(), "from-spec".into())],
        cwd: dir.to_path_buf(),
    }
}

fn shell_echo(text: &str) -> Vec<String> {
    if cfg!(windows) { vec!["cmd".into(), "/c".into(), format!("echo {text}")] }
    else { vec!["sh".into(), "-c".into(), format!("echo {text}")] }
}

#[tokio::test]
async fn noop_spawns_with_pipes_env_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let argv = if cfg!(windows) { vec!["cmd".into(), "/c".into(), "echo %BS_TEST_VAR%".into()] }
               else { vec!["sh".into(), "-c".into(), "echo $BS_TEST_VAR".into()] };
    let mut child = handle.spawn(SandboxCommand { argv, env: vec![], cwd: None, pty: None }).await.unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    assert_eq!(out.trim(), "from-spec");
    assert_eq!(child.exit.await.unwrap(), 0);

    let argv = if cfg!(windows) { vec!["cmd".into(), "/c".into(), "exit 3".into()] } else { vec!["sh".into(), "-c".into(), "exit 3".into()] };
    let child = handle.spawn(SandboxCommand { argv, env: vec![], cwd: None, pty: None }).await.unwrap();
    assert_eq!(child.exit.await.unwrap(), 3);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn noop_pty_echoes_input() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let argv = if cfg!(windows) { vec!["cmd".into()] } else { vec!["sh".into()] };
    let mut child = handle.spawn(SandboxCommand { argv, env: vec![], cwd: None, pty: Some(PtySize { cols: 80, rows: 24 }) }).await.unwrap();
    let mut pty = child.pty.take().unwrap();
    pty.writer.write_all(b"echo bs-pty-marker\r\n").await.unwrap();
    (pty.resizer)(PtySize { cols: 100, rows: 30 }).unwrap();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut chunk = [0u8; 1024];
        let n = tokio::time::timeout_at(deadline, pty.reader.read(&mut chunk)).await.expect("pty output").unwrap();
        if n == 0 { break; }
        buf.extend_from_slice(&chunk[..n]);
        if String::from_utf8_lossy(&buf).matches("bs-pty-marker").count() >= 2 { break; } // echo of input + output
    }
    assert!(String::from_utf8_lossy(&buf).contains("bs-pty-marker"));
    pty.writer.write_all(b"exit\r\n").await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.exit).await.expect("shell exits");
    handle.shutdown().await.unwrap();
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p bondsymphonic-daemon --test sandbox_noop`
Expected: compile error.

- [ ] **Step 4: Implement `sandbox/protocol.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InitRequest {
    Spawn { id: u64, argv: Vec<String>, env: Vec<(String, String)>, cwd: Option<String>, pty: Option<(u16, u16)> },
    Kill { pid: u32, signal: i32 },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum InitReply {
    /// Followed on the wire by SCM_RIGHTS fds: `[stdin, stdout, stderr]` when `has_pty` is false, `[pty_master]` when true.
    Spawned { id: u64, pid: u32, has_pty: bool },
    SpawnFailed { id: u64, message: String },
    Exited { pid: u32, code: i32 },
    ShuttingDown,
}

pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    let mut v = serde_json::to_vec(m).expect("protocol serializes");
    v.push(b'\n');
    v
}
```

- [ ] **Step 5: Implement `sandbox/mod.rs`**

```rust
pub mod noop;
pub mod protocol;
#[cfg(target_os = "linux")] pub mod exec_client;
#[cfg(target_os = "linux")] pub mod init;
#[cfg(target_os = "linux")] pub mod linux_bwrap;

use async_trait::async_trait;
use bondsymphonic_proto::{PrereqStatus, RpcError, WorkspaceId};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize { pub cols: u16, pub rows: u16 }

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub id: WorkspaceId,
    pub rw_binds: Vec<(PathBuf, PathBuf)>,
    pub ro_binds: Vec<(PathBuf, PathBuf)>,
    pub home: PathBuf,
    pub run_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub pty: Option<PtySize>,
}

pub type ChildReader = Pin<Box<dyn AsyncRead + Send>>;
pub type ChildWriter = Pin<Box<dyn AsyncWrite + Send>>;

pub struct PtyIo {
    pub reader: ChildReader,
    pub writer: ChildWriter,
    pub resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync>,
}

pub struct SandboxChild {
    pub pid: u32,
    pub stdin: Option<ChildWriter>,
    pub stdout: Option<ChildReader>,
    pub stderr: Option<ChildReader>,
    pub pty: Option<PtyIo>,
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    pub killer: Box<dyn Fn() + Send + Sync>,
}

#[async_trait]
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Vec<PrereqStatus>;
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError>;
}

#[async_trait]
pub trait SandboxHandle: Send + Sync {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError>;
    async fn shutdown(&self) -> Result<(), RpcError>;
}

/// Picks a backend by name. `linux_bwrap` falls back to `noop` (with a warning) off Linux.
pub fn backend_for(name: &str) -> Arc<dyn SandboxBackend> {
    match name {
        #[cfg(target_os = "linux")]
        "linux_bwrap" => Arc::new(linux_bwrap::BwrapBackend::default()),
        "noop" => Arc::new(noop::NoopBackend),
        other => {
            tracing::warn!(backend = other, "unknown or unsupported sandbox backend; using noop");
            Arc::new(noop::NoopBackend)
        }
    }
}

pub fn sandbox_error(msg: impl Into<String>) -> RpcError {
    RpcError::new(bondsymphonic_proto::ErrorCode::SandboxError, msg)
}
```

- [ ] **Step 6: Implement `sandbox/noop.rs`**

```rust
use super::*;
use portable_pty::{native_pty_system, CommandBuilder, PtySize as PPtySize};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::AsyncWriteExt;

pub struct NoopBackend;

struct NoopHandle { spec: SandboxSpec, children: Mutex<Vec<Box<dyn Fn() + Send + Sync>>> }

#[async_trait]
impl SandboxBackend for NoopBackend {
    fn name(&self) -> &'static str { "noop" }
    async fn check(&self) -> Vec<PrereqStatus> {
        vec![PrereqStatus { name: "sandbox".into(), ok: true, detail: "noop backend: processes run unsandboxed".into(), fix_hint: None }]
    }
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        std::fs::create_dir_all(&spec.home).map_err(|e| RpcError::io(&e))?;
        std::fs::create_dir_all(&spec.run_dir).map_err(|e| RpcError::io(&e))?;
        Ok(Arc::new(NoopHandle { spec: spec.clone(), children: Mutex::new(Vec::new()) }))
    }
}

impl NoopHandle {
    fn env_for(&self, cmd: &SandboxCommand) -> Vec<(String, String)> {
        let mut env = vec![("HOME".to_string(), self.spec.home.to_string_lossy().into_owned())];
        env.extend(self.spec.env.iter().cloned());
        env.extend(cmd.env.iter().cloned());
        env
    }
    fn cwd_for(&self, cmd: &SandboxCommand) -> PathBuf {
        let cwd = cmd.cwd.clone().unwrap_or_else(|| self.spec.cwd.clone());
        if cwd.exists() { cwd } else { std::env::temp_dir() }
    }
}

#[async_trait]
impl SandboxHandle for NoopHandle {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        if cmd.argv.is_empty() { return Err(RpcError::invalid_params("empty argv")); }
        let env = self.env_for(&cmd);
        let cwd = self.cwd_for(&cmd);
        match cmd.pty {
            None => spawn_piped(&cmd.argv, &env, &cwd, &self.children),
            Some(size) => spawn_pty(&cmd.argv, &env, &cwd, size, &self.children).await,
        }
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        let killers = std::mem::take(&mut *self.children.lock().unwrap());
        for k in killers { k(); }
        Ok(())
    }
}

fn spawn_piped(argv: &[String], env: &[(String, String)], cwd: &std::path::Path,
               children: &Mutex<Vec<Box<dyn Fn() + Send + Sync>>>) -> Result<SandboxChild, RpcError> {
    let mut c = tokio::process::Command::new(&argv[0]);
    c.args(&argv[1..]).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(false);
    for (k, v) in env { c.env(k, v); }
    let mut child = c.spawn().map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    let pid = child.id().unwrap_or(0);
    let stdin: ChildWriter = Box::pin(child.stdin.take().unwrap());
    let stdout: ChildReader = Box::pin(child.stdout.take().unwrap());
    let stderr: ChildReader = Box::pin(child.stderr.take().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let kill_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let kf = kill_flag.clone();
    tokio::spawn(async move {
        let code = loop {
            if kf.load(std::sync::atomic::Ordering::SeqCst) { let _ = child.start_kill(); }
            match tokio::time::timeout(std::time::Duration::from_millis(100), child.wait()).await {
                Ok(Ok(st)) => break st.code().unwrap_or(-1),
                Ok(Err(_)) => break -1,
                Err(_) => continue,
            }
        };
        let _ = tx.send(code);
    });
    // Two closures over the same flag: one handed back to the caller, one kept for shutdown().
    let kf_caller = kill_flag.clone();
    let killer: Box<dyn Fn() + Send + Sync> = Box::new(move || kf_caller.store(true, std::sync::atomic::Ordering::SeqCst));
    let kf_shutdown = kill_flag.clone();
    children.lock().unwrap().push(Box::new(move || kf_shutdown.store(true, std::sync::atomic::Ordering::SeqCst)));
    Ok(SandboxChild { pid, stdin: Some(stdin), stdout: Some(stdout), stderr: Some(stderr), pty: None, exit: rx, killer })
}
```

```rust
async fn spawn_pty(argv: &[String], env: &[(String, String)], cwd: &std::path::Path, size: PtySize,
                   children: &Mutex<Vec<Box<dyn Fn() + Send + Sync>>>) -> Result<SandboxChild, RpcError> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PPtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| sandbox_error(format!("openpty: {e}")))?;
    let mut cb = CommandBuilder::new(&argv[0]);
    cb.args(&argv[1..]);
    cb.cwd(cwd);
    for (k, v) in env { cb.env(k, v); }
    let mut child = pair.slave.spawn_command(cb).map_err(|e| sandbox_error(format!("spawn {}: {e}", argv[0])))?;
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let mut master_reader = pair.master.try_clone_reader().map_err(|e| sandbox_error(e.to_string()))?;
    let mut master_writer = pair.master.take_writer().map_err(|e| sandbox_error(e.to_string()))?;
    let master = Arc::new(Mutex::new(pair.master));

    // Reader pump: blocking thread -> mpsc<Vec<u8>> -> an AsyncRead adapter.
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => { if out_tx.blocking_send(buf[..n].to_vec()).is_err() { break; } }
            }
        }
    });
    let reader: ChildReader = Box::pin(tokio_util::io::StreamReader::new(
        tokio_stream::wrappers::ReceiverStream::new(out_rx).map(|v| Ok::<_, std::io::Error>(bytes::Bytes::from(v)))));

    // Writer pump: async mpsc<Vec<u8>> -> blocking write thread.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        while let Some(chunk) = in_rx.blocking_recv() {
            if master_writer.write_all(&chunk).is_err() || master_writer.flush().is_err() { break; }
        }
    });
    let writer: ChildWriter = Box::pin(ChannelWriter { tx: in_tx });

    let m2 = master.clone();
    let resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> = Box::new(move |s| {
        m2.lock().unwrap().resize(PPtySize { rows: s.rows, cols: s.cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    let killer_child = Arc::new(Mutex::new(Some(child.clone_killer())));
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        let _ = tx.send(code);
    });
    let kc = killer_child.clone();
    let killer: Box<dyn Fn() + Send + Sync> = Box::new(move || { if let Some(k) = kc.lock().unwrap().as_mut() { let _ = k.kill(); } });
    let kc2 = killer_child.clone();
    children.lock().unwrap().push(Box::new(move || { if let Some(k) = kc2.lock().unwrap().as_mut() { let _ = k.kill(); } }));
    Ok(SandboxChild { pid, stdin: None, stdout: None, stderr: None, pty: Some(PtyIo { reader, writer, resizer }), exit: rx, killer })
}

/// AsyncWrite that forwards chunks to a blocking writer thread.
struct ChannelWriter { tx: tokio::sync::mpsc::Sender<Vec<u8>> }
impl AsyncWrite for ChannelWriter {
    fn poll_write(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        match self.tx.try_reserve() {
            Ok(permit) => { permit.send(buf.to_vec()); std::task::Poll::Ready(Ok(buf.len())) }
            Err(tokio::sync::mpsc::error::TrySendError::Full(())) => { cx.waker().wake_by_ref(); std::task::Poll::Pending }
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::other("pty closed"))),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
}
```

Add `tokio-util = { version = "0.7", features = ["rt", "io"] }` (replace the existing tokio-util line), `tokio-stream = "0.1"`, `bytes = "1"`, and `futures = "0.3"` (for `StreamExt::map`) to the daemon dependencies. Add `pub mod sandbox;` to `lib.rs`. Export `ChannelWriter` as `pub(crate)` so the Linux exec client (Task 5) reuses it.

- [ ] **Step 7: Run tests on Windows and in WSL, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon --test sandbox_noop` and `scripts\test-daemon.ps1`
Expected: both tests pass on both platforms (conpty on Windows, openpty on Linux). If the Windows PTY test is flaky because `cmd` prints a banner, keep the marker-count loop as written (it waits for the echoed command and the output).

```bash
git add crates/daemon
git commit -m "feat(daemon): sandbox trait, exec protocol, noop backend with pipes and PTY"
```

---

### Task 5: `sandbox-init`, exec client, and the bubblewrap backend (Linux)

**Files:**
- Create: `crates/daemon/src/sandbox/init.rs`, `crates/daemon/src/sandbox/exec_client.rs`, `crates/daemon/src/sandbox/linux_bwrap.rs`
- Modify: `crates/daemon/src/main.rs` (subcommand), `crates/daemon/src/sandbox/mod.rs` (already declares the modules under cfg)
- Test: `crates/daemon/tests/sandbox_integration.rs` (Linux; skips without bwrap)

**Interfaces:**
- `bondsymphonic-daemon sandbox-init --socket <path>`: runs as PID 1 inside bwrap; listens on the Unix socket; handles `InitRequest`s; reaps children; on `Shutdown` or socket close: SIGTERM all children, SIGKILL after 5 s, exit 0.
- `exec_client::ExecClient::connect(path) -> io::Result<ExecClient>`; `async fn spawn(&self, cmd: &SandboxCommand) -> Result<SandboxChild, RpcError>`; `async fn kill(&self, pid, signal)`; `async fn shutdown(&self)`.
- `linux_bwrap::BwrapBackend { bwrap_path: PathBuf ("bwrap"), self_exe: PathBuf (std::env::current_exe) }`; `bwrap_args(spec: &SandboxSpec, socket_in_sandbox: &Path, self_exe: &Path) -> Vec<String>` is a pure function (unit-tested).
- `SandboxSpec.env` gets `PATH`, `HOME`, `TERM=xterm-256color`, `LANG=C.UTF-8` defaults applied by the backend if absent.

- [ ] **Step 1: Write the bwrap-args unit test** in `linux_bwrap.rs` (`#[cfg(test)]`)

```rust
#[test]
fn bwrap_args_follow_the_spec_layout() {
    let spec = SandboxSpec {
        id: "ws_1".into(),
        rw_binds: vec![("/data/worktrees/ws_1".into(), "/data/worktrees/ws_1".into())],
        ro_binds: vec![("/repo/.git".into(), "/repo/.git".into())],
        home: "/data/homes/ws_1".into(), run_dir: "/data/run/ws_1".into(),
        env: vec![], cwd: "/data/worktrees/ws_1".into(),
    };
    let args = bwrap_args(&spec, Path::new("/run/bs/exec.sock"), Path::new("/usr/bin/bondsymphonic-daemon"));
    let s = args.join(" ");
    assert!(s.starts_with("--ro-bind / / --tmpfs /tmp --proc /proc --dev /dev"));
    assert!(s.contains("--tmpfs /home"));
    assert!(s.contains("--bind /data/homes/ws_1 /home/bs"));
    assert!(s.contains("--ro-bind /repo/.git /repo/.git"));
    assert!(s.contains("--bind /data/worktrees/ws_1 /data/worktrees/ws_1"));
    assert!(s.contains("--bind /data/run/ws_1 /run/bs"));
    assert!(s.contains("--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup --unshare-net"));
    assert!(s.contains("--die-with-parent --new-session"));
    assert!(s.ends_with("-- /usr/bin/bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock"));
    // ro binds come before rw binds so rw subpaths override.
    assert!(s.find("--ro-bind /repo/.git").unwrap() < s.find("--bind /data/worktrees").unwrap());
}
```

- [ ] **Step 2: Implement `sandbox/init.rs`** (blocking std + threads; no tokio inside the sandbox)

```rust
//! PID 1 inside the bubblewrap sandbox. Spawns processes on request and hands
//! their stdio / PTY master back to the daemon over a Unix socket with SCM_RIGHTS.
use super::protocol::{encode, InitReply, InitRequest};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, IoSlice, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn run(socket: &std::path::Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let children: Arc<Mutex<HashSet<i32>>> = Arc::new(Mutex::new(HashSet::new()));
    let conns: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));

    // Reaper: waitpid(-1) loop broadcasting Exited to every connection.
    {
        let children = children.clone();
        let conns = conns.clone();
        std::thread::spawn(move || loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => notify_exit(&conns, &children, pid.as_raw(), code),
                Ok(WaitStatus::Signaled(pid, sig, _)) => notify_exit(&conns, &children, pid.as_raw(), 128 + sig as i32),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        });
    }

    for stream in listener.incoming() {
        let stream = match stream { Ok(s) => s, Err(_) => break };
        conns.lock().unwrap().push(stream.try_clone()?);
        let children = children.clone();
        let conns = conns.clone();
        std::thread::spawn(move || serve(stream, children, conns));
    }
    Ok(())
}

fn notify_exit(conns: &Arc<Mutex<Vec<UnixStream>>>, children: &Arc<Mutex<HashSet<i32>>>, pid: i32, code: i32) {
    if !children.lock().unwrap().remove(&pid) { return; }
    let msg = encode(&InitReply::Exited { pid: pid as u32, code });
    conns.lock().unwrap().retain_mut(|c| c.write_all(&msg).is_ok());
}

fn serve(stream: UnixStream, children: Arc<Mutex<HashSet<i32>>>, conns: Arc<Mutex<Vec<UnixStream>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) { Ok(0) | Err(_) => break, Ok(_) => {} }
        let req: InitRequest = match serde_json::from_str(line.trim()) { Ok(r) => r, Err(_) => continue };
        match req {
            InitRequest::Spawn { id, argv, env, cwd, pty } => match spawn(&argv, &env, cwd.as_deref(), pty) {
                Ok((pid, fds)) => {
                    children.lock().unwrap().insert(pid);
                    let msg = encode(&InitReply::Spawned { id, pid: pid as u32, has_pty: pty.is_some() });
                    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
                    let _ = sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&msg)], &[ControlMessage::ScmRights(&raw)], MsgFlags::empty(), None);
                    // fds dropped here: the daemon now owns its copies.
                }
                Err(e) => { let _ = (&stream).write_all(&encode(&InitReply::SpawnFailed { id, message: e })); }
            },
            InitRequest::Kill { pid, signal } => { let _ = kill(Pid::from_raw(pid as i32), Signal::try_from(signal).ok()); }
            InitRequest::Shutdown => { shutdown_all(&children); let _ = (&stream).write_all(&encode(&InitReply::ShuttingDown)); std::process::exit(0); }
        }
    }
    // Daemon went away: tear everything down.
    conns.lock().unwrap().retain(|c| c.as_raw_fd() != stream.as_raw_fd());
    if conns.lock().unwrap().is_empty() { shutdown_all(&children); std::process::exit(0); }
}

fn shutdown_all(children: &Arc<Mutex<HashSet<i32>>>) {
    let pids: Vec<i32> = children.lock().unwrap().iter().copied().collect();
    for p in &pids { let _ = kill(Pid::from_raw(-*p), Signal::SIGTERM); let _ = kill(Pid::from_raw(*p), Signal::SIGTERM); }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !children.lock().unwrap().is_empty() { std::thread::sleep(Duration::from_millis(50)); }
    for p in &pids { let _ = kill(Pid::from_raw(-*p), Signal::SIGKILL); let _ = kill(Pid::from_raw(*p), Signal::SIGKILL); }
}

/// Returns (pid, fds to pass): [stdin, stdout, stderr] or [pty_master].
fn spawn(argv: &[String], env: &[(String, String)], cwd: Option<&str>, pty: Option<(u16, u16)>) -> Result<(i32, Vec<OwnedFd>), String> {
    if argv.is_empty() { return Err("empty argv".into()); }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in env { cmd.env(k, v); }
    if let Some(c) = cwd { cmd.current_dir(c); }
    match pty {
        None => {
            let (in_r, in_w) = nix::unistd::pipe().map_err(|e| e.to_string())?;
            let (out_r, out_w) = nix::unistd::pipe().map_err(|e| e.to_string())?;
            let (err_r, err_w) = nix::unistd::pipe().map_err(|e| e.to_string())?;
            cmd.stdin(Stdio::from(in_r)).stdout(Stdio::from(out_w)).stderr(Stdio::from(err_w));
            unsafe { cmd.pre_exec(|| { nix::unistd::setsid().ok(); Ok(()) }); }
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            Ok((child.id() as i32, vec![in_w, out_r, err_r]))
        }
        Some((cols, rows)) => {
            let ws = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
            let pty = openpty(Some(&ws), None).map_err(|e| e.to_string())?;
            let slave_fd = pty.slave.as_raw_fd();
            cmd.stdin(Stdio::from(pty.slave.try_clone().map_err(|e| e.to_string())?))
               .stdout(Stdio::from(pty.slave.try_clone().map_err(|e| e.to_string())?))
               .stderr(Stdio::from(pty.slave));
            cmd.env("TERM", "xterm-256color");
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::setsid().map_err(std::io::Error::other)?;
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 { return Err(std::io::Error::last_os_error()); }
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            Ok((child.id() as i32, vec![pty.master]))
        }
    }
}
```

Note `std::process::Child` is dropped after spawn: the reaper thread's `waitpid(-1)` collects it (dropping `Child` does not wait). Because `Stdio::from(OwnedFd)` consumes the fds, keep the *other* ends in the returned `Vec<OwnedFd>` as written.

- [ ] **Step 3: Implement `sandbox/exec_client.rs`** (daemon side; blocking reader thread + tokio channels)

```rust
use super::protocol::{encode, InitReply, InitRequest};
use super::*;
use nix::cmsg_space;
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use std::collections::HashMap;
use std::io::{IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

type SpawnReply = Result<(u32, bool, Vec<OwnedFd>), String>;

pub struct ExecClient {
    writer: Mutex<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<SpawnReply>>>>,
    exits: Arc<Mutex<HashMap<u32, oneshot::Sender<i32>>>>,
    early_exits: Arc<Mutex<HashMap<u32, i32>>>,
    next_id: AtomicU64,
}

impl ExecClient {
    pub fn connect(path: &std::path::Path) -> std::io::Result<Arc<Self>> {
        let stream = UnixStream::connect(path)?;
        let reader = stream.try_clone()?;
        let client = Arc::new(Self {
            writer: Mutex::new(stream),
            pending: Default::default(), exits: Default::default(), early_exits: Default::default(),
            next_id: AtomicU64::new(1),
        });
        let c = client.clone();
        std::thread::spawn(move || c.read_loop(reader));
        Ok(client)
    }

    fn read_loop(&self, stream: UnixStream) {
        let fd = stream.as_raw_fd();
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut cmsg = cmsg_space!([RawFd; 3]);
            let mut iov = [IoSliceMut::new(&mut buf)];
            let msg = match recvmsg::<()>(fd, &mut iov, Some(&mut cmsg), MsgFlags::empty()) { Ok(m) => m, Err(_) => break };
            if msg.bytes == 0 { break; }
            let mut fds: Vec<OwnedFd> = Vec::new();
            if let Ok(cmsgs) = msg.cmsgs() {
                for c in cmsgs { if let ControlMessageOwned::ScmRights(list) = c { fds.extend(list.into_iter().map(|f| unsafe { OwnedFd::from_raw_fd(f) })); } }
            }
            acc.extend_from_slice(&buf[..msg.bytes]);
            while let Some(nl) = acc.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = acc.drain(..=nl).collect();
                if let Ok(reply) = serde_json::from_slice::<InitReply>(&line[..line.len() - 1]) {
                    self.dispatch(reply, std::mem::take(&mut fds));
                }
            }
        }
        // Socket closed: fail everything pending.
        for (_, tx) in self.pending.lock().unwrap().drain() { let _ = tx.send(Err("sandbox init went away".into())); }
        for (_, tx) in self.exits.lock().unwrap().drain() { let _ = tx.send(-1); }
    }

    fn dispatch(&self, reply: InitReply, fds: Vec<OwnedFd>) {
        match reply {
            InitReply::Spawned { id, pid, has_pty } => { if let Some(tx) = self.pending.lock().unwrap().remove(&id) { let _ = tx.send(Ok((pid, has_pty, fds))); } }
            InitReply::SpawnFailed { id, message } => { if let Some(tx) = self.pending.lock().unwrap().remove(&id) { let _ = tx.send(Err(message)); } }
            InitReply::Exited { pid, code } => {
                if let Some(tx) = self.exits.lock().unwrap().remove(&pid) { let _ = tx.send(code); }
                else { self.early_exits.lock().unwrap().insert(pid, code); }
            }
            InitReply::ShuttingDown => {}
        }
    }

    fn send(&self, req: &InitRequest) -> Result<(), RpcError> {
        self.writer.lock().unwrap().write_all(&encode(req)).map_err(|e| sandbox_error(format!("exec socket: {e}")))
    }

    pub async fn spawn(self: &Arc<Self>, cmd: &SandboxCommand, base_env: &[(String, String)], default_cwd: &std::path::Path) -> Result<SandboxChild, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut env = base_env.to_vec();
        env.extend(cmd.env.iter().cloned());
        let cwd = cmd.cwd.clone().unwrap_or_else(|| default_cwd.to_path_buf());
        self.send(&InitRequest::Spawn { id, argv: cmd.argv.clone(), env, cwd: Some(cwd.to_string_lossy().into_owned()), pty: cmd.pty.map(|p| (p.cols, p.rows)) })?;
        let (pid, has_pty, mut fds) = rx.await.map_err(|_| sandbox_error("exec client closed"))?.map_err(sandbox_error)?;

        let (exit_tx, exit_rx) = oneshot::channel();
        if let Some(code) = self.early_exits.lock().unwrap().remove(&pid) { let _ = exit_tx.send(code); }
        else { self.exits.lock().unwrap().insert(pid, exit_tx); }

        let me = self.clone();
        let killer: Box<dyn Fn() + Send + Sync> = Box::new(move || { let _ = me.send(&InitRequest::Kill { pid, signal: libc::SIGTERM }); });

        if has_pty {
            let master = fds.pop().ok_or_else(|| sandbox_error("no pty fd received"))?;
            let raw = master.as_raw_fd();
            let file = tokio::fs::File::from_std(std::fs::File::from(master.try_clone().map_err(|e| sandbox_error(e.to_string()))?));
            let (r, w) = tokio::io::split(file);
            let reader: ChildReader = Box::pin(r);
            let writer: ChildWriter = Box::pin(w);
            let resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> = Box::new(move |s| {
                let ws = libc::winsize { ws_row: s.rows, ws_col: s.cols, ws_xpixel: 0, ws_ypixel: 0 };
                if unsafe { libc::ioctl(raw, libc::TIOCSWINSZ, &ws) } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
            // Keep `master` alive for the resizer's lifetime.
            let _keep = Arc::new(master);
            let keep = _keep.clone();
            let resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync> = Box::new(move |s| { let _ = &keep; resizer(s) });
            return Ok(SandboxChild { pid, stdin: None, stdout: None, stderr: None, pty: Some(PtyIo { reader, writer, resizer }), exit: exit_rx, killer });
        }
        if fds.len() != 3 { return Err(sandbox_error(format!("expected 3 fds, got {}", fds.len()))); }
        let stderr = fds.pop().unwrap(); let stdout = fds.pop().unwrap(); let stdin = fds.pop().unwrap();
        let to_file = |fd: OwnedFd| tokio::fs::File::from_std(std::fs::File::from(fd));
        Ok(SandboxChild {
            pid,
            stdin: Some(Box::pin(to_file(stdin))), stdout: Some(Box::pin(to_file(stdout))), stderr: Some(Box::pin(to_file(stderr))),
            pty: None, exit: exit_rx, killer,
        })
    }

    pub fn shutdown(&self) -> Result<(), RpcError> { self.send(&InitRequest::Shutdown) }
}
```

`tokio::fs::File` over a pipe/pty fd performs reads on the blocking pool; that is acceptable at this milestone (one task per PTY). Note this in the report; Milestone 7 can switch to `AsyncFd`.

- [ ] **Step 4: Implement `sandbox/linux_bwrap.rs`**

```rust
use super::exec_client::ExecClient;
use super::*;
use bondsymphonic_proto::PrereqStatus;
use std::path::Path;
use std::process::Stdio;

pub struct BwrapBackend { pub bwrap_path: PathBuf, pub self_exe: PathBuf, pub user: String }

impl Default for BwrapBackend {
    fn default() -> Self {
        Self { bwrap_path: "bwrap".into(), self_exe: std::env::current_exe().unwrap_or_else(|_| "bondsymphonic-daemon".into()), user: whoami() }
    }
}

fn whoami() -> String { std::env::var("USER").unwrap_or_else(|_| "bs".into()) }

pub fn bwrap_args(spec: &SandboxSpec, socket_in_sandbox: &Path, self_exe: &Path) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let home_in = format!("/home/{}", whoami());
    let mut a: Vec<String> = vec![
        "--ro-bind", "/", "/", "--tmpfs", "/tmp", "--proc", "/proc", "--dev", "/dev",
        "--tmpfs", "/home",
    ].into_iter().map(String::from).collect();
    a.extend(["--bind".into(), s(&spec.home), home_in]);
    for (h, sb) in &spec.ro_binds { a.extend(["--ro-bind".into(), s(h), s(sb)]); }
    for (h, sb) in &spec.rw_binds { a.extend(["--bind".into(), s(h), s(sb)]); }
    a.extend(["--bind".into(), s(&spec.run_dir), "/run/bs".into()]);
    a.extend(["--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup", "--unshare-net",
              "--die-with-parent", "--new-session", "--chdir"].into_iter().map(String::from));
    a.push(s(&spec.cwd));
    a.extend(["--".into(), s(self_exe), "sandbox-init".into(), "--socket".into(), s(socket_in_sandbox)]);
    a
}

struct BwrapHandle { spec: SandboxSpec, client: Arc<ExecClient>, base_env: Vec<(String, String)>, _bwrap: tokio::sync::Mutex<tokio::process::Child> }

#[async_trait]
impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str { "linux_bwrap" }
    async fn check(&self) -> Vec<PrereqStatus> {
        let ok = tokio::process::Command::new(&self.bwrap_path).args(["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "true"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await.map(|s| s.success()).unwrap_or(false);
        vec![PrereqStatus { name: "sandbox".into(), ok, detail: if ok { "bubblewrap user namespaces work".into() } else { "bwrap --unshare-all failed".into() },
                            fix_hint: if ok { None } else { Some("sudo apt-get install -y bubblewrap; see setup-wsl.sh for the AppArmor sysctl".into()) } }]
    }
    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        std::fs::create_dir_all(&spec.home).map_err(|e| RpcError::io(&e))?;
        std::fs::create_dir_all(&spec.run_dir).map_err(|e| RpcError::io(&e))?;
        let host_sock = spec.run_dir.join("exec.sock");
        let _ = std::fs::remove_file(&host_sock);
        let args = bwrap_args(spec, Path::new("/run/bs/exec.sock"), &self.self_exe);
        let child = tokio::process::Command::new(&self.bwrap_path).args(&args)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true)
            .spawn().map_err(|e| sandbox_error(format!("bwrap: {e}")))?;
        // Wait for the socket (init is up) — 5 s.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !host_sock.exists() {
            if tokio::time::Instant::now() > deadline { return Err(sandbox_error("sandbox init did not start within 5s")); }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let client = tokio::task::spawn_blocking({ let p = host_sock.clone(); move || ExecClient::connect(&p) }).await
            .map_err(|e| sandbox_error(e.to_string()))?.map_err(|e| sandbox_error(format!("connect exec.sock: {e}")))?;
        let home_in = format!("/home/{}", self.user);
        let mut base_env = vec![
            ("HOME".to_string(), home_in.clone()), ("USER".to_string(), self.user.clone()),
            ("PATH".to_string(), format!("{home_in}/.local/bin:{home_in}/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")),
            ("TERM".to_string(), "xterm-256color".into()), ("LANG".to_string(), "C.UTF-8".into()),
        ];
        base_env.extend(spec.env.iter().cloned());
        Ok(Arc::new(BwrapHandle { spec: spec.clone(), client, base_env, _bwrap: tokio::sync::Mutex::new(child) }))
    }
}

#[async_trait]
impl SandboxHandle for BwrapHandle {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        self.client.spawn(&cmd, &self.base_env, &self.spec.cwd).await
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        let _ = self.client.shutdown();
        let mut b = self._bwrap.lock().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(7), b.wait()).await;
        let _ = b.kill().await;
        Ok(())
    }
}
```

- [ ] **Step 5: Add the `sandbox-init` subcommand to `main.rs`**

Turn `Args` into a clap struct with `#[command(subcommand)] cmd: Option<Cmd>` where `enum Cmd { SandboxInit { #[arg(long)] socket: PathBuf } }`. Before building the tokio runtime (convert `#[tokio::main] async fn main` into a sync `main` that matches the subcommand and otherwise calls `tokio::runtime::Runtime::new()?.block_on(serve(args))`), handle:

```rust
#[cfg(target_os = "linux")]
Some(Cmd::SandboxInit { socket }) => return bondsymphonic_daemon::sandbox::init::run(&socket),
#[cfg(not(target_os = "linux"))]
Some(Cmd::SandboxInit { .. }) => anyhow::bail!("sandbox-init is Linux only"),
```

Also thread `--no-sandbox` through: `let backend_name = if args.no_sandbox || !cfg!(target_os = "linux") { "noop" } else { "linux_bwrap" };` and store `backend_for(backend_name)` for Task 6; put `sandbox_backend: backend.name().into()` into `Capabilities`.

- [ ] **Step 6: Write the Linux integration test** `crates/daemon/tests/sandbox_integration.rs`

```rust
#![cfg(target_os = "linux")]
use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap").args(["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "true"]).status().map(|s| s.success()).unwrap_or(false)
}

async fn run_in(handle: &std::sync::Arc<dyn bondsymphonic_daemon::sandbox::SandboxHandle>, script: &str) -> (i32, String) {
    let mut child = handle.spawn(SandboxCommand { argv: vec!["sh".into(), "-c".into(), script.into()], env: vec![], cwd: None, pty: None }).await.unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    let code = child.exit.await.unwrap();
    (code, out)
}

#[tokio::test]
async fn bwrap_isolates_filesystem_pids_and_network() {
    if !bwrap_available() { eprintln!("SKIP: bwrap unavailable"); return; }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work"); std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_t".into(), rw_binds: vec![(work.clone(), work.clone())], ro_binds: vec![],
        home: dir.path().join("home"), run_dir: dir.path().join("run"), env: vec![], cwd: work.clone(),
    };
    // The daemon binary under test must be the one running sandbox-init: point at the test build's daemon.
    let backend = backend_for("linux_bwrap");
    let handle = backend.start(&spec).await.unwrap();

    let (code, _) = run_in(&handle, "touch /etc/bs-should-fail").await;
    assert_ne!(code, 0, "root filesystem must be read-only");
    let (code, _) = run_in(&handle, &format!("echo ok > {}/inside.txt", work.display())).await;
    assert_eq!(code, 0, "worktree must be writable");
    assert!(work.join("inside.txt").exists());
    let (code, out) = run_in(&handle, "echo $HOME; touch $HOME/x && echo home-ok").await;
    assert_eq!(code, 0); assert!(out.contains("home-ok"));
    let (_, out) = run_in(&handle, "echo $$").await;
    assert!(out.trim().parse::<i32>().unwrap() < 200, "own PID namespace expected, got {out}");
    let (code, _) = run_in(&handle, "cat /etc/hostname >/dev/null; (exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null").await;
    assert_ne!(code, 0, "network must be unreachable");

    let mut child = handle.spawn(SandboxCommand { argv: vec!["sh".into()], env: vec![], cwd: None, pty: Some(PtySize { cols: 80, rows: 24 }) }).await.unwrap();
    let mut pty = child.pty.take().unwrap();
    pty.writer.write_all(b"stty size; exit\n").await.unwrap();
    let mut buf = Vec::new(); let mut chunk = [0u8; 4096];
    while let Ok(Ok(n)) = tokio::time::timeout(std::time::Duration::from_secs(5), pty.reader.read(&mut chunk)).await { if n == 0 { break; } buf.extend_from_slice(&chunk[..n]); }
    assert!(String::from_utf8_lossy(&buf).contains("24 80"), "pty size should be applied: {}", String::from_utf8_lossy(&buf));
    assert_eq!(child.exit.await.unwrap(), 0);
    handle.shutdown().await.unwrap();
}
```

`BwrapBackend::default()` uses `current_exe()`, which inside a test is the **test binary**, not the daemon. Fix in `BwrapBackend`: resolve `self_exe` as `std::env::var_os("BS_DAEMON_EXE")` first, else the sibling `bondsymphonic-daemon` next to `current_exe()` (`target/debug/bondsymphonic-daemon`), else `current_exe()`. `scripts/test-daemon.ps1` builds the daemon binary before tests (`cargo build -p bondsymphonic-daemon` then `cargo test`), and the test uses the sibling path.

- [ ] **Step 7: Run in WSL, clippy on both platforms, fmt; commit**

Run: `scripts\test-daemon.ps1` (must show `bwrap_isolates_filesystem_pids_and_network ... ok`, not SKIP, on this machine) and on Windows `cargo test -p bondsymphonic-daemon` (Linux test compiled out) and `cargo clippy --workspace --all-targets -- -D warnings`; also `wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings"`.

```bash
git add crates/daemon scripts/test-daemon.ps1
git commit -m "feat(daemon): sandbox-init, exec client with fd passing, bubblewrap backend"
```

---

### Task 6: Workspace lifecycle, `WorkspaceHandler`, daemon wiring

**Files:**
- Create: `crates/daemon/src/workspace/lifecycle.rs`, `crates/daemon/src/server/handlers.rs`, `crates/daemon/src/daemon.rs`
- Modify: `crates/daemon/src/workspace/mod.rs` (`pub mod lifecycle;`), `crates/daemon/src/server/mod.rs` (`pub mod handlers;`), `crates/daemon/src/lib.rs` (`pub mod daemon;`), `crates/daemon/src/main.rs`
- Test: `crates/daemon/tests/workspace_integration.rs`, unit tests for the porcelain parser in `lifecycle.rs`

**Interfaces:**
- `daemon::Daemon { dirs: DataDirs, registry: Registry, git: Git, backend: Arc<dyn SandboxBackend>, sandboxes: Mutex<HashMap<WorkspaceId, Arc<dyn SandboxHandle>>>, events: EventBus, ptys: pty::PtyManager }` with `Daemon::new(dirs, backend, events) -> anyhow::Result<Arc<Daemon>>` (loads registry, `dirs.ensure()`), `async fn restore(&self)` (re-validates and restarts sandboxes for registered workspaces), `fn sandbox(&self, id) -> Result<Arc<dyn SandboxHandle>, RpcError>` (`SandboxError` if down), `fn workspace(&self, id) -> Result<Workspace, RpcError>` (`NotFound`), `fn emit_state(&self, ws: &Workspace)`.
- `lifecycle::create(d: &Daemon, p: WorkspaceCreateParams) -> Result<WorkspaceInfo, RpcError>`; `lifecycle::destroy(d, id, force) -> Result<Empty, RpcError>`; `lifecycle::status(d, id) -> Result<WorkspaceStatusResult, RpcError>`; `lifecycle::layout_for(d, ws) -> Result<Layout, RpcError>`; `lifecycle::spec_for(d, ws, layout) -> SandboxSpec`; `lifecycle::parse_porcelain_v2(text) -> Vec<GitStatusEntry>`.
- `handlers::WorkspaceHandler { system: SystemHandler, daemon: Arc<Daemon> }` implementing `Handler`: `Hello`/`SystemCheckPrereqs`/`SystemShutdown` delegate to `system` (auth is checked by `system` for hello; for everything else `WorkspaceHandler` checks `ctx.is_authenticated()` first); `RepoInspect`, `WorkspaceCreate/List/Get/Destroy/Status` handled here; `Pty*` and `Fs*` handled in Tasks 7 and 8 (until then they fall through to `system`, which returns not-implemented).
- Events: `workspace.state` on Creating, Ready, Error, Destroying (workspace_id set).

- [ ] **Step 1: Write the failing integration test** `crates/daemon/tests/workspace_integration.rs`

```rust
mod common;
use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::dispatch::SystemHandler;
use bondsymphonic_daemon::server::handlers::WorkspaceHandler;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::DataDirs;
use bondsymphonic_proto::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

struct Client { r: BufReader<tokio::net::tcp::OwnedReadHalf>, w: tokio::net::tcp::OwnedWriteHalf, next: u64 }
impl Client {
    async fn connect(port: u16, token: &str) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (r, w) = s.into_split();
        let mut c = Client { r: BufReader::new(r), w, next: 1 };
        let v = c.call(Request::Hello(HelloParams { token: token.into(), client_version: "t".into() })).await.unwrap();
        assert!(v["daemon_version"].is_string());
        c
    }
    async fn send(&mut self, req: Request) -> u64 {
        let id = self.next; self.next += 1;
        self.w.write_all(codec::encode(&ClientMessage::Request { id, request: req }).as_bytes()).await.unwrap();
        id
    }
    /// Reads until the response for `id` arrives; returns intermediate events.
    async fn recv_response(&mut self, id: u64, events: &mut Vec<(Option<WorkspaceId>, Event)>) -> Result<serde_json::Value, RpcError> {
        loop {
            let mut line = String::new();
            assert!(self.r.read_line(&mut line).await.unwrap() > 0, "connection closed");
            match codec::decode::<ServerMessage>(line.trim_end()).unwrap() {
                ServerMessage::Response { id: rid, result, error } if rid == id => return match error { Some(e) => Err(e), None => Ok(result.unwrap_or(serde_json::Value::Null)) },
                ServerMessage::Response { .. } => {}
                ServerMessage::Event { workspace_id, event } => events.push((workspace_id, event)),
            }
        }
    }
    async fn call(&mut self, req: Request) -> Result<serde_json::Value, RpcError> {
        let id = self.send(req).await; let mut ev = Vec::new(); self.recv_response(id, &mut ev).await
    }
}

async fn start_daemon(root: &std::path::Path) -> (u16, String, Arc<Daemon>, CancellationToken) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(DataDirs::new(root), backend_for("noop"), server.event_bus()).unwrap();
    let system = SystemHandler { token: server.token().to_string(), capabilities: ServerConfig::default().capabilities };
    let handler = Arc::new(WorkspaceHandler { system, daemon: daemon.clone() });
    let (port, token) = (server.port(), server.token().to_string());
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.with_handler(handler).run(c2).await.unwrap() });
    (port, token, daemon, cancel)
}

#[tokio::test]
async fn create_list_get_status_destroy_roundtrip_with_events() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let info: RepoInfo = serde_json::from_value(c.call(Request::RepoInspect(RepoPathParams { path: repo.to_string_lossy().into() })).await.unwrap()).unwrap();
    assert_eq!(info.default_branch, "main");

    let id = c.send(Request::WorkspaceCreate(WorkspaceCreateParams { repo_path: repo.to_string_lossy().into(), base_branch: "main".into(), name: "agent-1".into() })).await;
    let mut events = Vec::new();
    let ws: WorkspaceInfo = serde_json::from_value(c.recv_response(id, &mut events).await.unwrap()).unwrap();
    assert_eq!(ws.branch, "bs/agent-1/work");
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(std::path::Path::new(&ws.worktree_path).join("README.md").exists());
    let states: Vec<String> = events.iter().filter_map(|(_, e)| match e { Event::WorkspaceStateChanged { info } => Some(format!("{:?}", info.state)), _ => None }).collect();
    assert_eq!(states, vec!["Creating", "Ready"]);

    let list: WorkspaceListResult = serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert_eq!(list.workspaces.len(), 1);
    let got: WorkspaceInfo = serde_json::from_value(c.call(Request::WorkspaceGet(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap()).unwrap();
    assert_eq!(got.name, "agent-1");

    // Duplicate name in the same repo is a Conflict.
    let err = c.call(Request::WorkspaceCreate(WorkspaceCreateParams { repo_path: repo.to_string_lossy().into(), base_branch: "main".into(), name: "agent-1".into() })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // Status sees an untracked file, and destroy without force refuses while dirty.
    std::fs::write(std::path::Path::new(&ws.worktree_path).join("wip.txt"), "x").unwrap();
    let st: WorkspaceStatusResult = serde_json::from_value(c.call(Request::WorkspaceStatus(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap()).unwrap();
    assert!(st.entries.iter().any(|e| e.path == "wip.txt" && e.status == FileStatus::Untracked));
    let err = c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams { workspace_id: ws.id.clone(), force: false })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams { workspace_id: ws.id.clone(), force: true })).await.unwrap();
    let list: WorkspaceListResult = serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    assert!(list.workspaces.is_empty());
    assert!(!std::path::Path::new(&ws.worktree_path).exists());
    assert!(daemon.registry.list().is_empty());
    let err = c.call(Request::WorkspaceGet(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

#[tokio::test]
async fn restore_marks_missing_worktree_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, _daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(c.call(Request::WorkspaceCreate(WorkspaceCreateParams { repo_path: repo.to_string_lossy().into(), base_branch: "main".into(), name: "a".into() })).await.unwrap()).unwrap();
    cancel.cancel();
    std::fs::remove_dir_all(&ws.worktree_path).unwrap();

    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(DataDirs::new(&data), backend_for("noop"), server.event_bus()).unwrap();
    daemon.restore().await;
    let w = daemon.registry.get(&ws.id).unwrap();
    assert!(matches!(w.state, WorkspaceState::Error(_)), "got {:?}", w.state);
}
```

Create `crates/daemon/tests/common/mod.rs` with the `init_repo` helper from Task 2 (and switch `git_repo.rs`/`git_worktree.rs` to `mod common;` if they still inline it).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p bondsymphonic-daemon --test workspace_integration`
Expected: compile error.

- [ ] **Step 3: Implement `daemon.rs`**

```rust
use crate::git::Git;
use crate::pty::PtyManager;
use crate::sandbox::{SandboxBackend, SandboxHandle};
use crate::server::broadcast::EventBus;
use crate::workspace::registry::Registry;
use crate::workspace::{lifecycle, DataDirs, Workspace};
use bondsymphonic_proto::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Daemon {
    pub dirs: DataDirs,
    pub registry: Registry,
    pub git: Git,
    pub backend: Arc<dyn SandboxBackend>,
    pub sandboxes: Mutex<HashMap<WorkspaceId, Arc<dyn SandboxHandle>>>,
    pub events: EventBus,
    pub ptys: PtyManager,
}

impl Daemon {
    pub fn new(dirs: DataDirs, backend: Arc<dyn SandboxBackend>, events: EventBus) -> anyhow::Result<Arc<Self>> {
        dirs.ensure()?;
        let registry = Registry::load(&dirs.registry_file())?;
        Ok(Arc::new(Self { dirs, registry, git: Git::new(), backend, sandboxes: Mutex::new(HashMap::new()), events: events.clone(), ptys: PtyManager::new(events) }))
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Result<Workspace, RpcError> {
        self.registry.get(id).ok_or_else(|| RpcError::not_found(format!("workspace {id}")))
    }

    pub fn sandbox(&self, id: &WorkspaceId) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        self.sandboxes.lock().get(id).cloned().ok_or_else(|| RpcError::new(ErrorCode::SandboxError, format!("sandbox for {id} is not running")))
    }

    pub fn emit_state(&self, ws: &Workspace) {
        self.events.publish(Some(ws.id.clone()), Event::WorkspaceStateChanged { info: ws.info() });
    }

    pub fn set_state(&self, id: &WorkspaceId, state: WorkspaceState) -> Result<Workspace, RpcError> {
        let ws = self.registry.update(id, |w| w.state = state)?;
        self.emit_state(&ws);
        Ok(ws)
    }

    /// On startup: validate every registered workspace and restart its sandbox.
    pub async fn restore(&self) {
        for ws in self.registry.list() {
            if !ws.worktree_path.exists() {
                let _ = self.set_state(&ws.id, WorkspaceState::Error("worktree directory is missing".into()));
                continue;
            }
            match lifecycle::start_sandbox(self, &ws).await {
                Ok(()) => { let _ = self.set_state(&ws.id, WorkspaceState::Ready); }
                Err(e) => { tracing::warn!(ws = %ws.id, "sandbox restore failed: {e}"); let _ = self.set_state(&ws.id, WorkspaceState::SandboxDown); }
            }
        }
    }
}
```

- [ ] **Step 4: Implement `workspace/lifecycle.rs`**

```rust
use crate::daemon::Daemon;
use crate::git::{repo, worktree::{self, Layout}};
use crate::ids::new_id;
use crate::sandbox::SandboxSpec;
use crate::workspace::{now_rfc3339, Workspace};
use bondsymphonic_proto::*;
use std::path::{Path, PathBuf};

pub async fn layout_for(d: &Daemon, ws: &Workspace) -> Result<Layout, RpcError> {
    let git_common = repo::common_dir(&d.git, &ws.repo_path).await?;
    Ok(Layout { repo: ws.repo_path.clone(), git_common, name: ws.name.clone(), branch: ws.branch.clone(),
                worktree_path: ws.worktree_path.clone(), objects_dir: d.dirs.objects(&ws.id) })
}

pub fn spec_for(d: &Daemon, ws: &Workspace, layout: &Layout) -> SandboxSpec {
    let same = |p: &Path| (p.to_path_buf(), p.to_path_buf());
    let mut rw_binds = vec![same(&ws.worktree_path), same(&layout.objects_dir)];
    rw_binds.extend(layout.rw_git_paths().iter().map(|p| same(p)));
    let cache = d.dirs.cache(&ws.id);
    let home_in_sandbox = PathBuf::from(format!("/home/{}", std::env::var("USER").unwrap_or_else(|_| "bs".into())));
    rw_binds.push((cache, home_in_sandbox.join(".cache")));
    let ro_binds = vec![same(&layout.git_common)];
    let mut env = layout.sandbox_git_env();
    env.push(("BS_WORKSPACE".into(), ws.id.to_string()));
    SandboxSpec { id: ws.id.clone(), rw_binds, ro_binds, home: d.dirs.home(&ws.id), run_dir: d.dirs.run(&ws.id), env, cwd: ws.worktree_path.clone() }
}

pub async fn start_sandbox(d: &Daemon, ws: &Workspace) -> Result<(), RpcError> {
    let layout = layout_for(d, ws).await?;
    for p in [d.dirs.cache(&ws.id), d.dirs.home(&ws.id), d.dirs.run(&ws.id)] { std::fs::create_dir_all(p).map_err(|e| RpcError::io(&e))?; }
    let handle = d.backend.start(&spec_for(d, ws, &layout)).await?;
    d.sandboxes.lock().insert(ws.id.clone(), handle);
    Ok(())
}

async fn seed_home(d: &Daemon, ws: &Workspace) {
    let home = d.dirs.home(&ws.id);
    let _ = std::fs::create_dir_all(&home);
    let name = d.git.run(&ws.repo_path, &["config", "--get", "user.name"]).await.map(|o| o.stdout.trim().to_string()).unwrap_or_default();
    let email = d.git.run(&ws.repo_path, &["config", "--get", "user.email"]).await.map(|o| o.stdout.trim().to_string()).unwrap_or_default();
    if !name.is_empty() || !email.is_empty() {
        let _ = std::fs::write(home.join(".gitconfig"), format!("[user]\n\tname = {name}\n\temail = {email}\n[safe]\n\tdirectory = *\n"));
    }
}

pub async fn create(d: &Daemon, p: WorkspaceCreateParams) -> Result<WorkspaceInfo, RpcError> {
    let repo_path = PathBuf::from(&p.repo_path);
    if p.name.is_empty() || p.name.contains('/') || p.name.contains("..") || p.name.contains(char::is_whitespace) {
        return Err(RpcError::invalid_params("workspace name must be a single path-safe word"));
    }
    let git_common = repo::common_dir(&d.git, &repo_path).await?;
    if d.registry.find_by_name(&repo_path, &p.name).is_some() {
        return Err(RpcError::new(ErrorCode::Conflict, format!("workspace {} already exists for this repo", p.name)));
    }
    let id: WorkspaceId = new_id("ws_").into();
    d.dirs.ensure_workspace(&id).map_err(|e| RpcError::io(&e))?;
    let ws = Workspace {
        id: id.clone(), name: p.name.clone(), repo_path: repo_path.clone(), base_branch: p.base_branch.clone(),
        branch: Workspace::branch_for(&p.name), worktree_path: d.dirs.worktree(&id), created_at: now_rfc3339(),
        allowlist: vec![], state: WorkspaceState::Creating, agents: vec![], runs: vec![],
    };
    d.registry.insert(ws.clone()).map_err(|e| RpcError::internal(e.to_string()))?;
    d.emit_state(&ws);

    let layout = Layout { repo: repo_path, git_common, name: ws.name.clone(), branch: ws.branch.clone(), worktree_path: ws.worktree_path.clone(), objects_dir: d.dirs.objects(&id) };
    if let Err(e) = worktree::create(&d.git, &layout, &p.base_branch).await {
        let _ = d.registry.remove(&id);
        d.dirs.remove_workspace(&id);
        return Err(e);
    }
    seed_home(d, &ws).await;
    match start_sandbox(d, &ws).await {
        Ok(()) => Ok(d.set_state(&id, WorkspaceState::Ready)?.info()),
        Err(e) => { let ws = d.set_state(&id, WorkspaceState::Error(format!("sandbox failed: {}", e.message)))?; Ok(ws.info()) }
    }
}

pub async fn destroy(d: &Daemon, id: &WorkspaceId, force: bool) -> Result<Empty, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    if !force && ws.worktree_path.exists() {
        let dirty = !layout.daemon_git().run(&ws.worktree_path, &["status", "--porcelain"]).await?.stdout.trim().is_empty();
        let unmerged = !layout.daemon_git().run(&ws.repo_path, &["rev-list", &format!("{}..{}", ws.base_branch, ws.branch)]).await?.stdout.trim().is_empty();
        if dirty || unmerged {
            return Err(RpcError::new(ErrorCode::Conflict, "workspace has uncommitted changes or unmerged commits; use force to discard")
                .with_data(serde_json::json!({ "dirty": dirty, "unmerged": unmerged })));
        }
    }
    d.set_state(id, WorkspaceState::Destroying)?;
    d.ptys.close_workspace(id).await;
    if let Some(h) = d.sandboxes.lock().remove(id) { let _ = h.shutdown().await; }
    worktree::remove(&d.git, &layout).await?;
    d.dirs.remove_workspace(id);
    d.registry.remove(id).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(Empty {})
}

pub async fn status(d: &Daemon, id: &WorkspaceId) -> Result<WorkspaceStatusResult, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    let out = layout.daemon_git().run(&ws.worktree_path, &["status", "--porcelain=v2", "--untracked-files=all"]).await?;
    Ok(WorkspaceStatusResult { entries: parse_porcelain_v2(&out.stdout) })
}

/// Parses `git status --porcelain=v2` output into entries (one per path).
pub fn parse_porcelain_v2(text: &str) -> Vec<GitStatusEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(2, ' ');
        match it.next() {
            Some("?") => out.push(GitStatusEntry { path: it.next().unwrap_or("").to_string(), status: FileStatus::Untracked, staged: false }),
            Some("1") | Some("2") | Some("u") => {
                let rest = it.next().unwrap_or("");
                let fields: Vec<&str> = rest.split(' ').collect();
                let xy = fields.first().copied().unwrap_or("..");
                let (x, y) = (xy.chars().next().unwrap_or('.'), xy.chars().nth(1).unwrap_or('.'));
                // `2` (rename) lines end with "<new path>\t<original path>"; keep the new path.
                let path = rest.split('\t').next().unwrap_or("").rsplit(' ').next().unwrap_or("").to_string();
                let code = if x != '.' { x } else { y };
                let status = match code { 'A' => FileStatus::Added, 'M' => FileStatus::Modified, 'D' => FileStatus::Deleted, 'R' | 'C' => FileStatus::Renamed, 'U' => FileStatus::Modified, _ => FileStatus::Unchanged };
                out.push(GitStatusEntry { path, status, staged: x != '.' });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_porcelain_v2_lines() {
        let text = "1 .M N... 100644 100644 100644 abc def src/lib.rs\n1 A. N... 000000 100644 100644 000 abc new.rs\n? wip.txt\n2 R. N... 100644 100644 100644 a b R100 new\told\n";
        let e = parse_porcelain_v2(text);
        assert_eq!(e.len(), 4);
        assert_eq!((e[0].path.as_str(), e[0].status, e[0].staged), ("src/lib.rs", FileStatus::Modified, false));
        assert_eq!((e[1].path.as_str(), e[1].status, e[1].staged), ("new.rs", FileStatus::Added, true));
        assert_eq!((e[2].path.as_str(), e[2].status), ("wip.txt", FileStatus::Untracked));
        assert_eq!((e[3].path.as_str(), e[3].status), ("new", FileStatus::Renamed));
    }
}
```

- [ ] **Step 5: Implement `server/handlers.rs`**

```rust
use crate::daemon::Daemon;
use crate::server::dispatch::{ConnCtx, Handler, SystemHandler};
use crate::workspace::lifecycle;
use async_trait::async_trait;
use bondsymphonic_proto::*;
use serde_json::Value;
use std::sync::Arc;

pub struct WorkspaceHandler { pub system: SystemHandler, pub daemon: Arc<Daemon> }

fn ok<T: serde::Serialize>(v: T) -> Result<Value, RpcError> { serde_json::to_value(v).map_err(|e| RpcError::internal(e.to_string())) }

#[async_trait]
impl Handler for WorkspaceHandler {
    async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<Value, RpcError> {
        if matches!(req, Request::Hello(_)) { return self.system.handle(req, ctx).await; }
        if !ctx.is_authenticated() { return Err(RpcError::unauthorized()); }
        let d = &self.daemon;
        match req {
            Request::RepoInspect(p) => ok(crate::git::repo::inspect(&d.git, std::path::Path::new(&p.path)).await?),
            Request::WorkspaceCreate(p) => ok(lifecycle::create(d, p).await?),
            Request::WorkspaceList {} => ok(WorkspaceListResult { workspaces: d.registry.list().iter().map(|w| w.info()).collect() }),
            Request::WorkspaceGet(p) => ok(d.workspace(&p.workspace_id)?.info()),
            Request::WorkspaceDestroy(p) => ok(lifecycle::destroy(d, &p.workspace_id, p.force).await?),
            Request::WorkspaceStatus(p) => ok(lifecycle::status(d, &p.workspace_id).await?),
            // Task 7 and Task 8 add Pty* and Fs* arms here.
            other => self.system.handle(other, ctx).await,
        }
    }
}
```

- [ ] **Step 6: Wire `main.rs`**

After `Server::bind`: build `DataDirs::new(data_dir)`, `backend_for(backend_name)`, `Daemon::new(dirs, backend, server.event_bus())?`, call `daemon.restore().await`, then `let system = SystemHandler { token: server.token().to_string(), capabilities: Capabilities { sandbox_backend: backend_name.into(), git_protect: backend_name == "linux_bwrap", adapters: vec![AgentAdapterKind::Terminal] } };` and `server = server.with_handler(Arc::new(WorkspaceHandler { system, daemon }));`. On shutdown, after `server.run` returns, shut every sandbox down: `for (_, h) in daemon.sandboxes.lock().drain() { let _ = h.shutdown().await; }` (collect the handles into a Vec first so the lock is not held across the await).

- [ ] **Step 7: Run tests on Windows and in WSL, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon` and `scripts\test-daemon.ps1`
Expected: all pass, including the 2 new integration tests and the porcelain unit test.

```bash
git add crates/daemon
git commit -m "feat(daemon): workspace create/list/get/status/destroy with sandbox lifecycle and events"
```

---

### Task 7: PTY sessions over the protocol

**Files:**
- Create: `crates/daemon/src/pty.rs`
- Modify: `crates/daemon/src/lib.rs`, `crates/daemon/src/server/handlers.rs`, `crates/daemon/Cargo.toml` (`base64 = "0.22"`)
- Test: `crates/daemon/tests/pty_integration.rs`

**Interfaces:**
- `pty::PtyManager::new(events: EventBus) -> Self`; `async fn open(&self, d: &Daemon, p: PtyOpenParams) -> Result<PtyOpenResult, RpcError>`; `async fn write(&self, p: PtyWriteParams) -> Result<Empty, RpcError>`; `async fn resize(&self, p: PtyResizeParams) -> Result<Empty, RpcError>`; `async fn close(&self, id: &PtyId) -> Result<Empty, RpcError>`; `async fn close_workspace(&self, ws: &WorkspaceId)`.
- Default command when `command` is `None`: `["bash", "-l"]` on Linux backends, `["cmd"]` when the noop backend runs on Windows; a `command` string is split with `shell-words` semantics (add `shell-words = "1"`).
- Events: `pty.output { pty_id, data_b64 }` (4 KiB chunks), `pty.exit { pty_id, code }` once; both with `workspace_id` set.

- [ ] **Step 1: Write the failing test** `crates/daemon/tests/pty_integration.rs` — reuse the `Client`/`start_daemon` helpers by moving them into `tests/common/mod.rs` (make `Client` and `start_daemon` `pub`).

```rust
mod common;
use bondsymphonic_proto::*;
use base64::Engine;

#[tokio::test]
async fn pty_open_echo_resize_close_over_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(c.call(Request::WorkspaceCreate(WorkspaceCreateParams { repo_path: repo.to_string_lossy().into(), base_branch: "main".into(), name: "p".into() })).await.unwrap()).unwrap();

    let command = if cfg!(windows) { Some("cmd".to_string()) } else { Some("sh".to_string()) };
    let opened: PtyOpenResult = serde_json::from_value(c.call(Request::PtyOpen(PtyOpenParams { workspace_id: ws.id.clone(), cols: 80, rows: 24, command })).await.unwrap()).unwrap();
    let marker = "bs-marker-4242";
    c.call(Request::PtyWrite(PtyWriteParams { pty_id: opened.pty_id.clone(), data_b64: base64::engine::general_purpose::STANDARD.encode(format!("echo {marker}\r\n")) })).await.unwrap();
    c.call(Request::PtyResize(PtyResizeParams { pty_id: opened.pty_id.clone(), cols: 120, rows: 40 })).await.unwrap();

    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && collected.matches(marker).count() < 2 {
        let mut events = Vec::new();
        let id = c.send(Request::WorkspaceGet(WorkspaceIdParams { workspace_id: ws.id.clone() })).await; // any request; we harvest events on the way
        c.recv_response(id, &mut events).await.unwrap();
        for (_, e) in events { if let Event::PtyOutput { data_b64, .. } = e { collected.push_str(&String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap())); } }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(collected.contains(marker), "collected: {collected}");

    c.call(Request::PtyClose(PtyIdParams { pty_id: opened.pty_id.clone() })).await.unwrap();
    let mut saw_exit = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_exit {
        let mut events = Vec::new();
        let id = c.send(Request::WorkspaceList {}).await;
        c.recv_response(id, &mut events).await.unwrap();
        saw_exit = events.iter().any(|(_, e)| matches!(e, Event::PtyExit { pty_id, .. } if *pty_id == opened.pty_id));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saw_exit, "pty.exit event expected after close");
    let err = c.call(Request::PtyWrite(PtyWriteParams { pty_id: opened.pty_id.clone(), data_b64: "".into() })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p bondsymphonic-daemon --test pty_integration` → compile error.

- [ ] **Step 3: Implement `pty.rs`**

```rust
use crate::daemon::Daemon;
use crate::ids::new_id;
use crate::sandbox::{PtySize, SandboxCommand};
use crate::server::broadcast::EventBus;
use base64::Engine;
use bondsymphonic_proto::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

struct Session {
    workspace_id: WorkspaceId,
    writer: Mutex<crate::sandbox::ChildWriter>,
    resizer: Box<dyn Fn(PtySize) -> std::io::Result<()> + Send + Sync>,
    killer: Box<dyn Fn() + Send + Sync>,
}

/// `sessions` is an `Arc` so the per-PTY pump task can remove its own entry on exit.
pub struct PtyManager { events: EventBus, sessions: Arc<Mutex<HashMap<PtyId, Arc<Session>>>> }

fn b64(bytes: &[u8]) -> String { base64::engine::general_purpose::STANDARD.encode(bytes) }

impl PtyManager {
    pub fn new(events: EventBus) -> Self { Self { events, sessions: Arc::new(Mutex::new(HashMap::new())) } }

    fn default_command(backend: &str) -> Vec<String> {
        if backend == "noop" && cfg!(windows) { vec!["cmd".into()] } else { vec!["bash".into(), "-l".into()] }
    }

    pub async fn open(&self, d: &Daemon, p: PtyOpenParams) -> Result<PtyOpenResult, RpcError> {
        let ws = d.workspace(&p.workspace_id)?;
        let handle = d.sandbox(&ws.id)?;
        let argv = match p.command.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(cmd) => shell_words::split(cmd).map_err(|e| RpcError::invalid_params(format!("command: {e}")))?,
            None => Self::default_command(d.backend.name()),
        };
        let size = PtySize { cols: p.cols.max(2), rows: p.rows.max(1) };
        let mut child = handle.spawn(SandboxCommand { argv, env: vec![], cwd: None, pty: Some(size) }).await?;
        let pty = child.pty.take().ok_or_else(|| RpcError::internal("backend returned no pty"))?;
        let id: PtyId = new_id("pty_").into();
        let session = Arc::new(Session { workspace_id: ws.id.clone(), writer: Mutex::new(pty.writer), resizer: pty.resizer, killer: child.killer });
        self.sessions.lock().await.insert(id.clone(), session);

        // Output pump + exit watcher.
        let events = self.events.clone();
        let (pid, wsid) = (id.clone(), ws.id.clone());
        let mut reader = pty.reader;
        let exit = child.exit;
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => events.publish(Some(wsid.clone()), Event::PtyOutput { pty_id: pid.clone(), data_b64: b64(&buf[..n]) }),
                }
            }
            let code = tokio::time::timeout(std::time::Duration::from_secs(5), exit).await.ok().and_then(|r| r.ok()).unwrap_or(-1);
            sessions.lock().await.remove(&pid);
            events.publish(Some(wsid), Event::PtyExit { pty_id: pid, code });
        });
        Ok(PtyOpenResult { pty_id: id })
    }

    async fn session(&self, id: &PtyId) -> Result<Arc<Session>, RpcError> {
        self.sessions.lock().await.get(id).cloned().ok_or_else(|| RpcError::not_found(format!("pty {id}")))
    }

    pub async fn write(&self, p: PtyWriteParams) -> Result<Empty, RpcError> {
        let s = self.session(&p.pty_id).await?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(p.data_b64.as_bytes()).map_err(|e| RpcError::invalid_params(format!("data_b64: {e}")))?;
        s.writer.lock().await.write_all(&bytes).await.map_err(|e| RpcError::io(&e))?;
        Ok(Empty {})
    }

    pub async fn resize(&self, p: PtyResizeParams) -> Result<Empty, RpcError> {
        let s = self.session(&p.pty_id).await?;
        (s.resizer)(PtySize { cols: p.cols.max(2), rows: p.rows.max(1) }).map_err(|e| RpcError::io(&e))?;
        Ok(Empty {})
    }

    pub async fn close(&self, id: &PtyId) -> Result<Empty, RpcError> {
        let s = self.session(id).await?;
        (s.killer)();
        Ok(Empty {})
    }

    pub async fn close_workspace(&self, ws: &WorkspaceId) {
        let victims: Vec<Arc<Session>> = self.sessions.lock().await.values().filter(|s| &s.workspace_id == ws).cloned().collect();
        for s in victims { (s.killer)(); }
    }
}
```

Handler arms (in `handlers.rs`): `Request::PtyOpen(p) => ok(d.ptys.open(d, p).await?)`, `PtyWrite(p) => ok(d.ptys.write(p).await?)`, `PtyResize(p) => ok(d.ptys.resize(p).await?)`, `PtyClose(p) => ok(d.ptys.close(&p.pty_id).await?)`.

- [ ] **Step 4: Run tests on Windows and in WSL, clippy, fmt; commit**

Run: `cargo test -p bondsymphonic-daemon` and `scripts\test-daemon.ps1`. Expected: pass on both (Windows via conpty + `cmd`, WSL via the noop backend's `sh` in this test; the bwrap PTY path is covered by `sandbox_integration`).

```bash
git add crates/daemon
git commit -m "feat(daemon): PTY sessions inside the sandbox with output and exit events"
```

---

### Task 8: File service

**Files:**
- Create: `crates/daemon/src/fs.rs`
- Modify: `crates/daemon/src/lib.rs`, `crates/daemon/src/server/handlers.rs`
- Test: `crates/daemon/tests/fs_service.rs`

**Interfaces:**
- `fs::resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError>`: rejects absolute paths, `..` components, NUL; joins; canonicalizes the deepest existing ancestor and requires it to start with the canonical root (so symlinks out of the tree are `InvalidParams`).
- `fs::list_dir(root, rel) -> Result<ListDirResult, RpcError>` (sorted: dirs first, then names; `status: Unchanged` at this milestone; skips `.git`), `fs::read_file(root, rel) -> Result<ReadFileResult, RpcError>` (`encoding: "utf-8"` or `"binary"` with empty content; `truncated: true` above 4 MiB, content cut at the last char boundary), `fs::write_file(root, rel, content) -> Result<Empty, RpcError>` (temp + rename; creates parent dirs; preserves mode on unix).
- Handler arms: `FsListDir`, `FsReadFile`, `FsWriteFile` (root = `d.workspace(id)?.worktree_path`); `FsWatch` stays not-implemented until Milestone 3.

- [ ] **Step 1: Write the failing test** `crates/daemon/tests/fs_service.rs`

```rust
use bondsymphonic_daemon::fs;
use bondsymphonic_proto::ErrorCode;

#[test]
fn resolve_contains_paths_and_rejects_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt"); std::fs::create_dir_all(root.join("src")).unwrap();
    assert!(fs::resolve(&root, "src/main.rs").unwrap().ends_with("src/main.rs") || fs::resolve(&root, "src/main.rs").unwrap().ends_with("src\\main.rs"));
    for bad in ["../x", "src/../../x", "/etc/passwd", "C:\\Windows", "a\0b"] {
        assert_eq!(fs::resolve(&root, bad).unwrap_err().code, ErrorCode::InvalidParams, "{bad}");
    }
    #[cfg(unix)] {
        std::os::unix::fs::symlink(dir.path(), root.join("escape")).unwrap();
        assert_eq!(fs::resolve(&root, "escape/secret").unwrap_err().code, ErrorCode::InvalidParams);
    }
}

#[test]
fn list_read_write_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wt"); std::fs::create_dir_all(root.join("src")).unwrap(); std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("b.txt"), "bee").unwrap(); std::fs::write(root.join("a.bin"), [0u8, 159, 146, 150]).unwrap();
    let l = fs::list_dir(&root, "").unwrap();
    let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["src", "a.bin", "b.txt"]);
    assert!(l.entries[0].is_dir && !l.entries[2].is_dir && l.entries[2].size == 3);
    assert_eq!(fs::read_file(&root, "b.txt").unwrap().content, "bee");
    assert_eq!(fs::read_file(&root, "a.bin").unwrap().encoding, "binary");
    fs::write_file(&root, "src/new/deep.txt", "hi").unwrap();
    assert_eq!(std::fs::read_to_string(root.join("src/new/deep.txt")).unwrap(), "hi");
    assert_eq!(fs::read_file(&root, "missing").unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(fs::list_dir(&root, "b.txt").unwrap_err().code, ErrorCode::InvalidParams);
}
```

- [ ] **Step 2: Run to verify failure** — compile error.

- [ ] **Step 3: Implement `fs.rs`**

```rust
use bondsymphonic_proto::*;
use std::path::{Component, Path, PathBuf};

pub const MAX_READ: usize = 4 * 1024 * 1024;

pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    if rel.contains('\0') { return Err(RpcError::invalid_params("path contains NUL")); }
    let p = Path::new(rel);
    if p.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') || p.components().any(|c| matches!(c, Component::Prefix(_) | Component::RootDir)) {
        return Err(RpcError::invalid_params("path must be relative to the worktree"));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) { return Err(RpcError::invalid_params("path may not contain '..'")); }
    let joined = root.join(p);
    let root_c = root.canonicalize().map_err(|e| RpcError::io(&e))?;
    // Canonicalize the deepest existing ancestor to defeat symlink escapes.
    let mut probe = joined.clone();
    let mut tail = Vec::new();
    while !probe.exists() { tail.push(probe.file_name().map(|s| s.to_os_string()).unwrap_or_default()); if !probe.pop() { break; } }
    let mut canon = probe.canonicalize().map_err(|e| RpcError::io(&e))?;
    for t in tail.into_iter().rev() { canon.push(t); }
    if !canon.starts_with(&root_c) { return Err(RpcError::invalid_params("path escapes the worktree")); }
    Ok(canon)
}

pub fn list_dir(root: &Path, rel: &str) -> Result<ListDirResult, RpcError> {
    let dir = resolve(root, rel)?;
    if !dir.is_dir() { return Err(RpcError::invalid_params("not a directory")); }
    let mut entries: Vec<FileEntry> = std::fs::read_dir(&dir).map_err(|e| RpcError::io(&e))?.filter_map(|e| e.ok()).filter_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == ".git" { return None; }
        let md = e.metadata().ok()?;
        Some(FileEntry { name, is_dir: md.is_dir(), size: if md.is_dir() { 0 } else { md.len() }, status: FileStatus::Unchanged })
    }).collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(ListDirResult { entries })
}

pub fn read_file(root: &Path, rel: &str) -> Result<ReadFileResult, RpcError> {
    let path = resolve(root, rel)?;
    if !path.exists() { return Err(RpcError::not_found(rel)); }
    let bytes = std::fs::read(&path).map_err(|e| RpcError::io(&e))?;
    let truncated = bytes.len() > MAX_READ;
    let slice = if truncated { &bytes[..MAX_READ] } else { &bytes[..] };
    match std::str::from_utf8(slice) {
        Ok(s) => Ok(ReadFileResult { content: s.to_string(), encoding: "utf-8".into(), truncated }),
        Err(e) if truncated && e.valid_up_to() > 0 && slice[..e.valid_up_to()].len() + 4 >= slice.len() =>
            Ok(ReadFileResult { content: std::str::from_utf8(&slice[..e.valid_up_to()]).unwrap().to_string(), encoding: "utf-8".into(), truncated: true }),
        Err(_) => Ok(ReadFileResult { content: String::new(), encoding: "binary".into(), truncated }),
    }
}

pub fn write_file(root: &Path, rel: &str, content: &str) -> Result<Empty, RpcError> {
    let path = resolve(root, rel)?;
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| RpcError::io(&e))?; }
    let tmp = path.with_extension(format!("{}.bs-tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, content).map_err(|e| RpcError::io(&e))?;
    #[cfg(unix)] if let Ok(md) = std::fs::metadata(&path) { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(md.permissions().mode())); }
    std::fs::rename(&tmp, &path).map_err(|e| RpcError::io(&e))?;
    Ok(Empty {})
}
```

Handler arms: `FsListDir(p) => ok(fs::list_dir(&d.workspace(&p.workspace_id)?.worktree_path, &p.path)?)`, likewise `FsReadFile`, `FsWriteFile(p) => ok(fs::write_file(&root, &p.path, &p.content)?)`. Run these through `tokio::task::spawn_blocking` so large reads do not block the runtime.

- [ ] **Step 4: Run tests both platforms, clippy, fmt; commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): worktree file service with path containment"
```

---

### Task 9: Real-sandbox workspace verification, docs, and exit checks

**Files:**
- Modify: `crates/daemon/tests/sandbox_integration.rs` (add the workspace-level bwrap test), `README.md` (daemon capabilities and `--no-sandbox`), `scripts/test-daemon.ps1` (build the daemon binary before tests so `sandbox-init` exists)
- Create: `docs/daemon-protocol-notes.md` (short: how to drive the daemon by hand with a one-liner client for debugging)

**Interfaces:** none new.

- [ ] **Step 1: Add the workspace-level bwrap test** to `sandbox_integration.rs` (Linux; skips without bwrap)

```rust
#[tokio::test]
async fn bwrap_workspace_protects_main_branch_and_shared_objects() {
    if !bwrap_available() { eprintln!("SKIP: bwrap unavailable"); return; }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = bondsymphonic_daemon::server::Server::bind(Default::default()).await.unwrap();
    let daemon = bondsymphonic_daemon::daemon::Daemon::new(bondsymphonic_daemon::workspace::DataDirs::new(dir.path().join("data")), backend_for("linux_bwrap"), server.event_bus()).unwrap();
    let ws = bondsymphonic_daemon::workspace::lifecycle::create(&daemon, WorkspaceCreateParams { repo_path: repo.to_string_lossy().into(), base_branch: "main".into(), name: "sb".into() }).await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "sandbox must come up: {:?}", ws.state);
    let handle = daemon.sandbox(&ws.id).unwrap();
    let wt = ws.worktree_path.clone();

    // Agent commits on its own branch: allowed.
    let (code, out) = run_in(&handle, &format!("cd '{wt}' && git -c user.name=a -c user.email=a@a commit -q --allow-empty -m sandboxed && git rev-parse --abbrev-ref HEAD")).await;
    assert_eq!(code, 0, "{out}"); assert_eq!(out.trim(), "bs/sb/work");
    // Moving main: denied (refs/heads is read-only).
    let (code, out) = run_in(&handle, &format!("cd '{wt}' && git update-ref refs/heads/main HEAD 2>&1")).await;
    assert_ne!(code, 0, "main must be protected: {out}");
    // Writing into the shared object store: denied.
    let git_common = bondsymphonic_daemon::git::repo::common_dir(&daemon.git, &repo).await.unwrap();
    let (code, _) = run_in(&handle, &format!("touch '{}/objects/should-fail'", git_common.display())).await;
    assert_ne!(code, 0);
    // The daemon sees the commit through alternates.
    let layout = bondsymphonic_daemon::workspace::lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap()).await.unwrap();
    let subject = layout.daemon_git().run(&repo, &["log", "-1", "--format=%s", "bs/sb/work"]).await.unwrap();
    assert_eq!(subject.stdout.trim(), "sandboxed");
    bondsymphonic_daemon::workspace::lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}
```

Note: the test repo lives under the Linux tmp dir, so `git_common` is a real ext4 path; the ro-bind of `git_common` plus the three rw sub-binds is exactly the production layout.

- [ ] **Step 2: `scripts/test-daemon.ps1`** — before `cargo test`, run `cargo build -p bondsymphonic-daemon` in the same WSL command so `target/debug/bondsymphonic-daemon` exists next to the test binaries, and export `BS_DAEMON_EXE=~/.bondsymphonic/target/debug/bondsymphonic-daemon` for the test run.

- [ ] **Step 3: Docs** — README: a "Daemon" paragraph listing what Milestone 2a delivers, the `--no-sandbox` flag, and the data directory layout. `docs/daemon-protocol-notes.md`: how to start the daemon in WSL by hand, read its port line, and send `hello` + `workspace.create` with a `python3` one-liner or `nc`, for debugging.

- [ ] **Step 4: Full verification; commit**

Run on Windows: `cargo test --workspace` (with `scripts\env.ps1` sourced), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`. In WSL: `scripts\test-daemon.ps1` — every sandbox test must run (not SKIP) on this machine. Then a manual end-to-end from Windows: `.\launch.ps1` still connects (status bar `daemon: connected v0.1.0`), and `wsl -d bondsymphonic -- ls ~/.bondsymphonic` shows the new directories.

```bash
git add crates/daemon scripts/test-daemon.ps1 README.md docs/daemon-protocol-notes.md
git commit -m "test(daemon): real-sandbox workspace protection test; docs"
```

---

## Milestone 2a exit criteria

- `workspace.create` on a real repo inside WSL yields a worktree on `bs/<name>/work`, a running bubblewrap sandbox, and `workspace.state` events; `list/get/status/destroy` behave per spec; a dirty workspace refuses destroy without `force`.
- Inside the sandbox: root filesystem read-only, worktree writable, `refs/heads/main` and the shared object store not writable, own PID namespace, no network; commits land in the private object dir and are visible to the daemon via alternates.
- `pty.open/write/resize/close` work over the protocol with `pty.output`/`pty.exit` events, both with the noop backend on Windows and inside bwrap in WSL.
- `fs.list_dir/read_file/write_file` enforce containment.
- Daemon still builds and passes its non-sandbox tests on Windows; clippy and fmt clean on both platforms; the IDE from Milestone 1 still connects.
