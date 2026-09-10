//! Workspace create / destroy / status: the git worktree, the per-workspace
//! directories and the sandbox that fronts them, kept in step with the registry.

use crate::daemon::Daemon;
use crate::git::{
    repo,
    worktree::{self, Layout},
};
use crate::ids::new_id;
use crate::net::allowlist::{Allowlist, HostPattern};
use crate::sandbox::{SandboxCommand, SandboxHandle, SandboxSpec};
use crate::workspace::{now_rfc3339, Workspace};
use bondsymphonic_proto::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

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

/// The workspace's proxy socket, in the host's run directory. The sandbox has
/// that directory bound at `/run/bs`, so the same file is
/// [`PROXY_SOCKET_IN_SANDBOX`] from inside.
pub const PROXY_SOCKET_FILE: &str = "proxy.sock";

/// Where the proxy socket appears inside the sandbox.
pub const PROXY_SOCKET_IN_SANDBOX: &str = "/run/bs/proxy.sock";

/// Where the shim listens inside the sandbox, and therefore what the proxy
/// environment points at. Loopback only: it is the sandbox's own network
/// namespace, so nothing outside it can reach this port whatever the number.
pub const PROXY_LISTEN: &str = "127.0.0.1:3128";

/// Hosts the sandbox reaches directly rather than through the proxy. Loopback
/// is where the port bridge puts a workspace's own services, and sending those
/// through the daemon would be both pointless and wrong: the proxy resolves
/// names and connects on the *host*.
pub const PROXY_BYPASS: &str = "localhost,127.0.0.1";

/// The daemon binary as the sandbox sees it.
///
/// Duplicated from `sandbox::linux_bwrap`, where it is private and behind
/// `cfg(target_os = "linux")`, the same way [`sandbox_user`] duplicates
/// `whoami`. It holds because the daemon is installed under the user's home
/// (`~/.bondsymphonic/bin`) and built under it in development, and bwrap
/// replaces `/home` with a tmpfs, so the binary is always bound in at this
/// path rather than reachable at its own.
pub const INIT_EXE_IN_SANDBOX: &str = "/tmp/.bs-init";

/// How long the shim gets to bind its port before the workspace comes up
/// anyway. Exceeded, the workspace is still usable — it simply has no route out
/// until it is restarted — which beats refusing to open it at all.
const SHIM_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The proxy variables every process in a bwrap sandbox inherits.
///
/// Both spellings of each name: curl reads `http_proxy` in lower case only,
/// while most other tools read the upper-case form, and a workspace where curl
/// silently had no network would be a mystery to debug.
fn proxy_env() -> Vec<(String, String)> {
    let url = format!("http://{PROXY_LISTEN}");
    let mut env: Vec<(String, String)> = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .map(|k| ((*k).to_string(), url.clone()))
    .collect();
    env.push(("NO_PROXY".into(), PROXY_BYPASS.into()));
    env.push(("no_proxy".into(), PROXY_BYPASS.into()));
    env
}

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
    // Only where the sandbox actually has a network namespace of its own. The
    // no-sandbox backend runs processes as plain children of the daemon, which
    // already have the host's network; pointing those at a proxy that exists to
    // make up for the loss of one would take away what they have.
    if d.backend.name() == BWRAP_BACKEND {
        env.extend(proxy_env());
    }
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
    // Before the sandbox, not after: the shim inside it connects to this socket
    // as its first act, and a sandbox that came up first would race it.
    d.proxies
        .start(
            &ws.id,
            &d.dirs.run(&ws.id).join(PROXY_SOCKET_FILE),
            Allowlist::from_strings(&ws.allowlist),
            d.events.clone(),
        )
        .await?;
    let handle = match d.backend.start(&spec_for(d, ws, &layout)).await {
        Ok(h) => h,
        // Nothing will ever connect to that listener now.
        Err(e) => {
            d.proxies.stop(&ws.id);
            return Err(e);
        }
    };
    d.sandboxes.lock().insert(ws.id.clone(), handle.clone());
    watch_sandbox(d, &ws.id, handle.clone());
    if d.backend.name() == BWRAP_BACKEND {
        start_shim(d, ws, &handle).await;
    }
    Ok(())
}

/// Starts the in-sandbox half of the proxy and waits for it to be listening.
///
/// A failure here is logged rather than returned: the workspace still works,
/// with no route out of the sandbox, and that is a far better outcome than
/// refusing to open it. The child is owned by the task this spawns, so its
/// `killer` — and with it the daemon's hold on the shim — lives exactly as long
/// as the process does; init takes the shim down with the sandbox in any case,
/// since it is one of its children like any other.
async fn start_shim(d: &Arc<Daemon>, ws: &Workspace, handle: &Arc<dyn SandboxHandle>) {
    let argv = [
        INIT_EXE_IN_SANDBOX,
        "proxy-shim",
        "--socket",
        PROXY_SOCKET_IN_SANDBOX,
        "--listen",
        PROXY_LISTEN,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let mut child = match handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(ws = %ws.id, "proxy shim did not start: {e}; this workspace has no network");
            return;
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        tracing::warn!(ws = %ws.id, "proxy shim started without pipes; this workspace has no network");
        return;
    };
    let mut out = tokio::io::BufReader::new(stdout).lines();
    let mut err = tokio::io::BufReader::new(stderr).lines();
    // The shim announces its bind, so the first request out of the sandbox
    // cannot arrive before the port exists.
    let ready = tokio::time::timeout(SHIM_READY_TIMEOUT, async {
        while let Ok(Some(line)) = out.next_line().await {
            if line.starts_with(crate::net::shim::READY_LINE) {
                return true;
            }
            tracing::debug!(ws = %ws.id, "proxy-shim: {line}");
        }
        false
    })
    .await
    .unwrap_or(false);
    if !ready {
        tracing::warn!(ws = %ws.id, "proxy shim never reported listening; this workspace may have no network");
    }
    let d = d.clone();
    let id = ws.id.clone();
    tokio::spawn(async move {
        tokio::join!(
            async {
                while let Ok(Some(line)) = out.next_line().await {
                    tracing::debug!(ws = %id, "proxy-shim: {line}");
                }
            },
            async {
                while let Ok(Some(line)) = err.next_line().await {
                    tracing::warn!(ws = %id, "proxy-shim: {line}");
                }
            }
        );
        let code = (&mut child.exit).await.ok();
        // A sandbox on its way down takes the shim with it, which is ordinary; a
        // shim that dies under a live sandbox has cost that workspace its
        // network and is worth saying so.
        if d.sandboxes.lock().contains_key(&id) {
            tracing::warn!(ws = %id, ?code, "proxy shim exited; this workspace has no network");
        } else {
            tracing::debug!(ws = %id, ?code, "proxy shim exited with its sandbox");
        }
    });
}

/// Replaces a workspace's allowlist: validated, persisted, applied to the live
/// proxy and announced.
///
/// Every pattern is checked before anything is written, so a list with one
/// mistake in it leaves the old list in place rather than half-applying it. What
/// gets stored is the canonical form [`HostPattern`] produces, so the IDE sees
/// back exactly what the proxy will match.
pub fn set_allowlist(d: &Daemon, id: &WorkspaceId, hosts: &[String]) -> Result<Empty, RpcError> {
    let mut patterns = Vec::with_capacity(hosts.len());
    for host in hosts {
        let pattern = HostPattern::parse(host).map_err(RpcError::invalid_params)?;
        patterns.push(pattern.as_str().to_string());
    }
    let ws = d.registry.update(id, |w| w.allowlist = patterns.clone())?;
    d.proxies
        .set_allowlist(id, Allowlist::from_strings(&ws.allowlist));
    d.emit_state(&ws);
    Ok(Empty {})
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
        // The processes went with the sandbox, but their bridges did not: a
        // host port still accepting connections for a run that no longer exists
        // would hang a browser rather than refuse it.
        d.runs.stop_all_in(&id).await;
        // Nothing can reach the socket now that the sandbox holding the shim is
        // gone, and a listener left behind would outlive the workspace.
        d.proxies.stop(&id);
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
    // The repo's own `[network] allow` extends the defaults for this workspace.
    // A `bondsymphonic.toml` that will not parse is worth a warning and nothing
    // more: refusing to create the workspace over it would leave the user
    // unable to open the very repo they need a workspace in to fix the file.
    let repo_config = match crate::runs::config::load_repo_config(&repo_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "using the default allowlist");
            None
        }
    };
    let allowlist = crate::net::allowlist::effective(repo_config.as_ref());
    let ws = Workspace {
        id: id.clone(),
        name: p.name.clone(),
        repo_path: repo_path.clone(),
        base_branch: p.base_branch.clone(),
        branch: Workspace::branch_for(&p.name),
        worktree_path: d.dirs.worktree(&id),
        created_at: now_rfc3339(),
        allowlist,
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
    // Runs first: each one holds a bridge on the host as well as a process in
    // the sandbox, and the bridge would outlive the workspace it belongs to.
    // Stopping them here also means the last `run.state` a client sees is
    // `stopped` rather than a run frozen at `ready` for a workspace that is
    // gone.
    d.runs.stop_all_in(id).await;
    // Then the agents: each one ends through its own `stop` (stdin closed, exit
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
    // After the sandbox, so a last request is answered rather than cut off, and
    // before the directories go, since the socket file lives in one of them.
    d.proxies.stop(id);
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
