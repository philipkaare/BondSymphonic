//! Workspace create / destroy / status: the git worktree, the per-workspace
//! directories and the sandbox that fronts them, kept in step with the registry.

use crate::daemon::Daemon;
use crate::git::{
    repo,
    worktree::{self, Layout},
};
use crate::ids::new_id;
use crate::sandbox::SandboxSpec;
use crate::workspace::{now_rfc3339, Workspace};
use bondsymphonic_proto::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The account whose `/home/<user>` the bwrap backend mounts the sandbox home at.
///
/// Duplicated from `sandbox::linux_bwrap::whoami`, which is private and behind
/// `cfg(target_os = "linux")`; the spec has to be built on every host so the
/// noop backend and the Windows tests see the same layout.
fn sandbox_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "bs".into())
}

pub async fn layout_for(d: &Daemon, ws: &Workspace) -> Result<Layout, RpcError> {
    let git_common = repo::common_dir(&d.git, &ws.repo_path).await?;
    Ok(Layout {
        repo: ws.repo_path.clone(),
        git_common,
        name: ws.name.clone(),
        branch: ws.branch.clone(),
        worktree_path: ws.worktree_path.clone(),
        objects_dir: d.dirs.objects(&ws.id),
        no_hooks_dir: d.dirs.no_hooks(),
    })
}

/// The backend whose sandbox needs host binaries bound in. Compared by name so
/// this module does not have to be conditional on the target OS.
const BWRAP_BACKEND: &str = "linux_bwrap";

pub fn spec_for(d: &Daemon, ws: &Workspace, layout: &Layout) -> SandboxSpec {
    let same = |p: &Path| (p.to_path_buf(), p.to_path_buf());
    let mut rw_binds = vec![same(&ws.worktree_path), same(&layout.objects_dir)];
    rw_binds.extend(layout.rw_git_paths().iter().map(|p| same(p)));
    let cache = d.dirs.cache(&ws.id);
    let home_in_sandbox = PathBuf::from(format!("/home/{}", sandbox_user()));
    rw_binds.push((cache, home_in_sandbox.join(".cache")));
    let mut ro_binds = vec![same(&layout.git_common)];
    // The Claude Code CLI, bound in at a fixed path. A bwrap sandbox has its
    // own `/home`, so the daemon user's install is not reachable from inside it
    // and `claude_argv` names the bound path instead. Only where the backend
    // has mounts: `noop` runs processes as plain children of the daemon, which
    // already see the host binary at its own path. Absent Claude Code adds no
    // bind, and `agent.start` refuses with `PrereqMissing` rather than starting
    // a workspace that cannot host an agent.
    if d.backend.name() == BWRAP_BACKEND {
        ro_binds.extend(crate::agents::claude::claude_ro_bind());
    }
    // Read-only *after* the read-write bind of its parent gitdir, or the parent
    // would put the writable original straight back on top of it.
    let late_ro_binds = vec![same(&layout.config_worktree())];
    let mut env = layout.sandbox_git_env();
    env.push(("BS_WORKSPACE".into(), ws.id.to_string()));
    SandboxSpec {
        id: ws.id.clone(),
        rw_binds,
        ro_binds,
        late_ro_binds,
        home: d.dirs.home(&ws.id),
        run_dir: d.dirs.run(&ws.id),
        env,
        cwd: ws.worktree_path.clone(),
    }
}

pub async fn start_sandbox(d: &Arc<Daemon>, ws: &Workspace) -> Result<(), RpcError> {
    let layout = layout_for(d, ws).await?;
    for p in [
        d.dirs.cache(&ws.id),
        d.dirs.home(&ws.id),
        d.dirs.run(&ws.id),
    ] {
        std::fs::create_dir_all(p).map_err(|e| RpcError::io(&e))?;
    }
    // The daemon owns `config.worktree`, empty, and the sandbox gets it
    // read-only. Written on every start rather than only at creation, so that a
    // workspace made before this existed — or one whose file an agent managed to
    // write — starts from a file the daemon put there. Nothing legitimate is
    // lost: git only writes this file for features (sparse-checkout) the
    // read-only bind rules out inside the sandbox anyway.
    std::fs::write(layout.config_worktree(), b"").map_err(|e| RpcError::io(&e))?;
    let handle = d.backend.start(&spec_for(d, ws, &layout)).await?;
    d.sandboxes.lock().insert(ws.id.clone(), handle.clone());
    watch_sandbox(d, &ws.id, handle);
    Ok(())
}

/// Reports a sandbox that dies on its own as `SandboxDown`.
///
/// Without this the workspace stays `Ready` in the registry after its sandbox
/// is gone, and the first sign anyone gets is a `pty.open` failing with a raw
/// broken pipe — which the plan's constraint that every workspace failure
/// arrives as a `workspace.state` event or an `RpcError` does not allow.
fn watch_sandbox(
    d: &Arc<Daemon>,
    id: &WorkspaceId,
    handle: Arc<dyn crate::sandbox::SandboxHandle>,
) {
    let Some(mut died) = handle.died() else {
        return;
    };
    let d = d.clone();
    let id = id.clone();
    tokio::spawn(async move {
        // An error means the sender is gone, which is death by another name.
        let _ = died.wait_for(|dead| *dead).await;
        // A handle that is no longer the registered one belongs to a `destroy`
        // or a restart that has already taken over this workspace's state.
        let still_ours = d
            .sandboxes
            .lock()
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &handle));
        if !still_ours {
            return;
        }
        d.sandboxes.lock().remove(&id);
        tracing::warn!(ws = %id, "sandbox died");
        let _ = d.set_state(&id, WorkspaceState::SandboxDown);
    });
}

/// Gives the sandbox home a git identity, so commits made inside it are not
/// rejected for a missing `user.email`. Best effort: a repo without an
/// effective identity simply gets no `.gitconfig`. The daemon user's Claude
/// Code login is copied in here too, so an agent started in this workspace is
/// not logged out; a user who has not logged in simply gets no credentials.
async fn seed_home(d: &Daemon, ws: &Workspace) {
    let home = d.dirs.home(&ws.id);
    let _ = std::fs::create_dir_all(&home);
    let name = d
        .git
        .run(&ws.repo_path, &["config", "--get", "user.name"])
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
    let email = d
        .git
        .run(&ws.repo_path, &["config", "--get", "user.email"])
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
    if !name.is_empty() || !email.is_empty() {
        let _ = std::fs::write(
            home.join(".gitconfig"),
            format!("[user]\n\tname = {name}\n\temail = {email}\n[safe]\n\tdirectory = *\n"),
        );
    }
    let seeded = crate::agents::credentials::seed_claude_files(&home);
    if !seeded.is_empty() {
        tracing::info!(ws = %ws.id, files = ?seeded, "seeded claude credentials");
    }
}

pub async fn create(d: &Arc<Daemon>, p: WorkspaceCreateParams) -> Result<WorkspaceInfo, RpcError> {
    let repo_path = PathBuf::from(&p.repo_path);
    if p.name.is_empty()
        || p.name.contains('/')
        || p.name.contains("..")
        || p.name.contains(char::is_whitespace)
    {
        return Err(RpcError::invalid_params(
            "workspace name must be a single path-safe word",
        ));
    }
    let git_common = repo::common_dir(&d.git, &repo_path).await?;
    if d.registry.find_by_name(&repo_path, &p.name).is_some() {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!("workspace {} already exists for this repo", p.name),
        ));
    }
    let id = WorkspaceId(new_id("ws_"));
    d.dirs.ensure_workspace(&id).map_err(|e| RpcError::io(&e))?;
    let ws = Workspace {
        id: id.clone(),
        name: p.name.clone(),
        repo_path: repo_path.clone(),
        base_branch: p.base_branch.clone(),
        branch: Workspace::branch_for(&p.name),
        worktree_path: d.dirs.worktree(&id),
        created_at: now_rfc3339(),
        allowlist: vec![],
        state: WorkspaceState::Creating,
        agents: vec![],
        runs: vec![],
    };
    d.registry
        .insert(ws.clone())
        .map_err(|e| RpcError::internal(e.to_string()))?;
    d.emit_state(&ws);

    let layout = Layout {
        repo: repo_path,
        git_common,
        name: ws.name.clone(),
        branch: ws.branch.clone(),
        worktree_path: ws.worktree_path.clone(),
        objects_dir: d.dirs.objects(&id),
        no_hooks_dir: d.dirs.no_hooks(),
    };
    if let Err(e) = worktree::create(&d.git, &layout, &p.base_branch).await {
        // `worktree::create` pre-creates ref, reflog and object directories, and
        // `git worktree add` can fail halfway; `worktree::remove` is idempotent and
        // clears all of it. The one failure it must not answer is `Conflict`, which
        // means the branch already existed: that branch predates this workspace and
        // is not ours to delete, and `create` rejects it before touching anything.
        if e.code != ErrorCode::Conflict {
            let _ = worktree::remove(&d.git, &layout).await;
        }
        // The client has already seen `Creating`. Tell it why the workspace failed, then
        // send the terminal `Destroying` event a real destroy ends on, so the workspace
        // disappears from the client's list instead of hanging there forever.
        let _ = d.set_state(&id, WorkspaceState::Error(e.message.clone()));
        let _ = d.set_state(&id, WorkspaceState::Destroying);
        let _ = d.registry.remove(&id);
        d.dirs.remove_workspace(&id);
        return Err(e);
    }
    seed_home(d, &ws).await;
    match start_sandbox(d, &ws).await {
        Ok(()) => Ok(d.workspace_info(&d.set_state(&id, WorkspaceState::Ready)?)),
        Err(e) => {
            let ws = d.set_state(
                &id,
                WorkspaceState::Error(format!("sandbox failed: {}", e.message)),
            )?;
            Ok(d.workspace_info(&ws))
        }
    }
}

pub async fn destroy(d: &Daemon, id: &WorkspaceId, force: bool) -> Result<Empty, RpcError> {
    let ws = d.workspace(id)?;
    // `layout_for` asks the source repository where its git directory is, so a
    // repository the user has since deleted or moved fails here. Without the
    // `force` escape that failure is permanent: the registry entry can never be
    // destroyed, `restore` brings it back every start, and the only way out is
    // editing `workspaces.json` by hand.
    let layout = match layout_for(d, &ws).await {
        Ok(l) => Some(l),
        Err(_) if force => {
            tracing::warn!(
                ws = %id,
                repo = %ws.repo_path.display(),
                "repository is gone; destroying without the git steps"
            );
            None
        }
        Err(e) => return Err(e),
    };
    if !force {
        // Always present here: `layout` is only `None` when `force` is set.
        let layout = layout.as_ref().expect("a layout unless forced");
        // A missing worktree directory rules out the dirty check but not the unmerged
        // one. `restore` records that state as `Error("worktree directory is missing")`,
        // and the branch still points at commits whose objects live only in this
        // workspace's private object dir, which the `git branch -D` in `worktree::remove`
        // would discard for good.
        let dirty = if ws.worktree_path.exists() {
            !layout
                .worktree_git()
                .run(&ws.worktree_path, &["status", "--porcelain"])
                .await?
                .stdout
                .trim()
                .is_empty()
        } else {
            false
        };
        let unmerged = if repo::branch_exists(&d.git, &ws.repo_path, &ws.branch).await? {
            !layout
                .daemon_git()
                .run(
                    &ws.repo_path,
                    &["rev-list", &format!("{}..{}", ws.base_branch, ws.branch)],
                )
                .await?
                .stdout
                .trim()
                .is_empty()
        } else {
            false
        };
        if dirty || unmerged {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "workspace has uncommitted changes or unmerged commits; use force to discard",
            )
            .with_data(serde_json::json!({ "dirty": dirty, "unmerged": unmerged })));
        }
    }
    d.set_state(id, WorkspaceState::Destroying)?;
    // Agents first: each one ends through its own `stop` (stdin closed, exit
    // awaited, `Exited` announced), so the IDE learns the agent is gone. Left
    // until after the sandbox went down they would simply vanish with it, and a
    // client would go on showing an agent that no longer exists.
    d.agents.stop_all_in(id).await;
    // Before the worktree goes away, so the teardown itself is not reported as a
    // burst of `fs.changed` for a workspace that is on its way out.
    d.watchers.disable(id);
    d.ptys.close_workspace(id).await;
    // The guard is dropped before the await: a `parking_lot` guard held across one
    // would deadlock any handler that touches `sandboxes` in the meantime.
    let handle = d.sandboxes.lock().remove(id);
    if let Some(h) = handle {
        let _ = h.shutdown().await;
    }
    if let Some(layout) = layout.as_ref() {
        if let Err(e) = worktree::remove(&d.git, layout).await {
            // The ptys are closed and the sandbox is down, so the workspace must not be
            // left stranded in `Destroying`: a client would wait on a state that never
            // arrives.
            let _ = d.set_state(
                id,
                WorkspaceState::Error(format!("destroy failed: {}", e.message)),
            );
            return Err(e);
        }
    }
    d.dirs.remove_workspace(id);
    d.registry
        .remove(id)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(Empty {})
}

pub async fn status(d: &Daemon, id: &WorkspaceId) -> Result<WorkspaceStatusResult, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    // `worktree_git`, not `daemon_git`: the worktree is agent-writable, so git
    // must not be allowed to discover its repository (and its config) from it.
    let out = layout
        .worktree_git()
        .run(
            &ws.worktree_path,
            &["status", "--porcelain=v2", "--untracked-files=all"],
        )
        .await?;
    Ok(WorkspaceStatusResult {
        entries: parse_porcelain_v2(&out.stdout),
    })
}

/// Parses `git status --porcelain=v2` output into entries (one per path).
///
/// The path is the last field of a line, and it may contain spaces, so each line
/// type is split by its exact field count rather than on the last space:
/// `1` has 7 fields before the path, `2` has 8 (and is followed by a tab and the
/// original path), `u` has 9, and `?`/`!` have none.
pub fn parse_porcelain_v2(text: &str) -> Vec<GitStatusEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(2, ' ');
        let kind = it.next();
        let rest = it.next().unwrap_or("");
        match kind {
            Some("?") => out.push(GitStatusEntry {
                path: rest.to_string(),
                status: FileStatus::Untracked,
                staged: false,
            }),
            Some(k @ ("1" | "2" | "u")) => {
                let leading = match k {
                    "1" => 7,
                    "2" => 8,
                    _ => 9,
                };
                let fields: Vec<&str> = rest.splitn(leading + 1, ' ').collect();
                let xy = fields.first().copied().unwrap_or("..");
                let (x, y) = (
                    xy.chars().next().unwrap_or('.'),
                    xy.chars().nth(1).unwrap_or('.'),
                );
                // `2` (rename/copy) lines end with "<new path>\t<original path>"; keep the new path.
                let path = fields
                    .get(leading)
                    .copied()
                    .unwrap_or("")
                    .split('\t')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let code = if x != '.' { x } else { y };
                let status = match code {
                    'A' => FileStatus::Added,
                    'M' => FileStatus::Modified,
                    'D' => FileStatus::Deleted,
                    'R' | 'C' => FileStatus::Renamed,
                    'U' => FileStatus::Modified,
                    _ => FileStatus::Unchanged,
                };
                out.push(GitStatusEntry {
                    path,
                    status,
                    staged: x != '.',
                });
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
        assert_eq!(
            (e[0].path.as_str(), e[0].status, e[0].staged),
            ("src/lib.rs", FileStatus::Modified, false)
        );
        assert_eq!(
            (e[1].path.as_str(), e[1].status, e[1].staged),
            ("new.rs", FileStatus::Added, true)
        );
        assert_eq!(
            (e[2].path.as_str(), e[2].status),
            ("wip.txt", FileStatus::Untracked)
        );
        assert_eq!(
            (e[3].path.as_str(), e[3].status),
            ("new", FileStatus::Renamed)
        );
    }

    /// Porcelain v2 leaves spaces in a path unquoted, and the path is the last
    /// field, so it must be recovered by field count rather than by splitting.
    #[test]
    fn keeps_spaces_in_paths() {
        let text = "1 .D N... 100644 100644 000000 abc def my notes.txt\n? new file.txt\nu UU N... 100644 100644 100644 100644 a b c src/a b.rs\n2 R. N... 100644 100644 100644 a b R100 a b.rs\tc d.rs\n";
        let e = parse_porcelain_v2(text);
        assert_eq!(
            (e[0].path.as_str(), e[0].status, e[0].staged),
            ("my notes.txt", FileStatus::Deleted, false)
        );
        assert_eq!(e[1].path, "new file.txt");
        assert_eq!(
            (e[2].path.as_str(), e[2].status),
            ("src/a b.rs", FileStatus::Modified)
        );
        assert_eq!(
            (e[3].path.as_str(), e[3].status),
            ("a b.rs", FileStatus::Renamed)
        );
    }

    /// Unparseable and header lines are skipped rather than yielding empty paths.
    #[test]
    fn ignores_lines_that_are_not_entries() {
        assert!(parse_porcelain_v2("# branch.oid abc\n\n! ignored.txt\n").is_empty());
    }
}
