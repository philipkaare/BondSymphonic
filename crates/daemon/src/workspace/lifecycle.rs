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

/// How long the shim gets to bind its port before the workspace comes up
/// anyway. Exceeded, the workspace is still usable — it simply has no route out
/// until it is restarted — which beats refusing to open it at all.
const SHIM_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Says out loud that a workspace has lost its way out of the sandbox.
///
/// A `tracing::warn!` reaches the daemon's stderr and nobody else, and every
/// one of these leaves a workspace that is up and Ready with no network, which
/// an agent then experiences as connection timeouts it cannot explain. The
/// plan's rule is that a workspace failure arrives as a `workspace.state` event
/// or an `RpcError`; this is neither serious enough to fail the workspace nor
/// quiet enough to keep to ourselves, so it goes out as the same `daemon.log`
/// warn the denial path uses, tagged with the workspace it belongs to.
fn report_no_network(d: &Daemon, id: &WorkspaceId, detail: &str) {
    tracing::warn!(ws = %id, "{detail}");
    d.events.publish(
        Some(id.clone()),
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!("this workspace has no network: {detail}"),
            host: None,
        },
    );
}

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
    let proxy = d
        .proxies
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
            d.proxies.stop_generation(&ws.id, proxy);
            return Err(e);
        }
    };
    d.sandboxes.lock().insert(ws.id.clone(), handle.clone());
    watch_sandbox(d, &ws.id, handle.clone(), proxy);
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
    // Asked of the handle rather than assumed: bwrap binds the daemon binary in
    // at a fixed path only when its real one is hidden by a tmpfs, and a
    // hardcoded guess is right on one host and silently wrong on the next.
    let helper = handle.helper_exe();
    let argv = [
        helper.to_string_lossy().as_ref(),
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
            report_no_network(
                d,
                &ws.id,
                &format!(
                    "the proxy shim did not start from {}: {}",
                    helper.display(),
                    e.message
                ),
            );
            return;
        }
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        (child.killer)();
        report_no_network(d, &ws.id, "the proxy shim started without pipes");
        return;
    };
    let mut helper =
        crate::util::ready_line::Helper::new("proxy-shim", ws.id.as_str(), stdout, stderr);
    // The shim announces its bind, so the first request out of the sandbox
    // cannot arrive before the port exists.
    let ready = helper
        .ready(crate::net::shim::READY_LINE, SHIM_READY_TIMEOUT)
        .await;
    // Not returning: a workspace with no route out is still a workspace worth
    // opening, so the notice goes out and the shim is watched exactly as a
    // healthy one would be.
    if !ready {
        report_no_network(
            d,
            &ws.id,
            &format!("the proxy shim never reported listening on {PROXY_LISTEN}"),
        );
    }
    let d = d.clone();
    let id = ws.id.clone();
    tokio::spawn(async move {
        helper.drain().await;
        let code = (&mut child.exit).await.ok();
        // A sandbox on its way down takes the shim with it, which is ordinary; a
        // shim that dies under a live sandbox has cost that workspace its
        // network and is worth saying so.
        let alive = d.sandboxes.lock().contains_key(&id);
        if alive {
            report_no_network(
                &d,
                &id,
                &match code {
                    Some(c) => format!("the proxy shim exited with status {c}"),
                    None => "the proxy shim is gone".to_string(),
                },
            );
        } else {
            tracing::debug!(ws = %id, ?code, "proxy shim exited with its sandbox");
        }
    });
}

/// The most patterns one workspace's allowlist may hold.
///
/// Only the local IDE can call `workspace.set_allowlist`, so this is hygiene
/// rather than a boundary: the proxy walks the list once per connection, and a
/// list nobody could have meant to write should be refused where it is typed
/// rather than paid for on every request afterwards.
const MAX_ALLOWLIST_ENTRIES: usize = 256;

/// The longest one entry may be: the maximum length of a DNS name.
const MAX_HOST_LEN: usize = 253;

/// Replaces a workspace's allowlist: validated, persisted, applied to the live
/// proxy and announced.
///
/// Every pattern is checked before anything is written, so a list with one
/// mistake in it leaves the old list in place rather than half-applying it. What
/// gets stored is the canonical form [`HostPattern`] produces, so the IDE sees
/// back exactly what the proxy will match.
pub async fn set_allowlist(
    d: &Daemon,
    id: &WorkspaceId,
    hosts: &[String],
) -> Result<Empty, RpcError> {
    if hosts.len() > MAX_ALLOWLIST_ENTRIES {
        return Err(RpcError::invalid_params(format!(
            "an allowlist may hold at most {MAX_ALLOWLIST_ENTRIES} entries, not {}",
            hosts.len()
        )));
    }
    let mut patterns = Vec::with_capacity(hosts.len());
    for host in hosts {
        if host.len() > MAX_HOST_LEN {
            return Err(RpcError::invalid_params(format!(
                "allowlist entry is {} bytes; a host name is at most {MAX_HOST_LEN}",
                host.len()
            )));
        }
        let pattern = HostPattern::parse(host).map_err(RpcError::invalid_params)?;
        patterns.push(pattern.as_str().to_string());
    }
    let ws = d
        .registry
        .update(id, |w| w.allowlist = patterns.clone())
        .await?;
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
    proxy: u64,
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
        // gone, and a listener left behind would outlive the workspace. By
        // generation, so a restart that has already re-bound the socket keeps
        // the listener it is about to need.
        d.proxies.stop_generation(&id, proxy);
        tracing::warn!(ws = %id, "sandbox died");
        // Nobody is waiting on this one -- there is no request behind it to
        // answer -- so a registry that would not take the write can only say so
        // here. Without that line the workspace goes on reading `Ready` in
        // `workspace.list` with no sandbox behind it and nothing said about why.
        if let Err(e) = d.set_state(&id, WorkspaceState::SandboxDown).await {
            tracing::warn!(ws = %id, error = %e.message, "could not record the sandbox as down");
        }
    });
}

/// Gives the sandbox home a git identity, so commits made inside it are not
/// rejected for a missing `user.email`. Best effort: a repo without an
/// effective identity simply gets no `.gitconfig`. The daemon user's Claude
/// Code login is copied in here too, so an agent started in this workspace is
/// not logged out; a user who has not logged in simply gets no credentials.
async fn seed_home(d: &Daemon, ws: &Workspace) {
    use crate::agents::credentials::{ensure_real_dir, write_guarded};
    let home = d.dirs.home(&ws.id);
    // `ensure_real_dir`, not `create_dir_all`, and `write_guarded`, not
    // `fs::write`: every write into `homes/<id>` de-symlinks its own path
    // (daemon design §8.3). This one runs at creation, before the workspace has
    // an agent to plant anything, so it is defence in depth rather than a live
    // hole — but the rule belongs to the directory, not to the caller, and a
    // guard that holds only on some of the writers is one nobody can rely on.
    if let Err(e) = ensure_real_dir(&home) {
        tracing::warn!(ws = %ws.id, path = %home.display(), error = %e, "could not create the workspace home");
        return;
    }
    // One `git config` for both halves of the identity rather than two. A
    // `--get-regexp` that matches nothing exits non-zero, which is the same
    // "there is no identity here" the two `--get` calls answered with, and is
    // handled the same way: no `.gitconfig` is written.
    let identity = d
        .git
        .run(
            &ws.repo_path,
            &["config", "--get-regexp", r"^user\.(name|email)$"],
        )
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();
    let name = config_value(&identity, "user.name");
    let email = config_value(&identity, "user.email");
    if !name.is_empty() || !email.is_empty() {
        let _ = write_guarded(
            &home.join(".gitconfig"),
            &format!("[user]\n\tname = {name}\n\temail = {email}\n[safe]\n\tdirectory = *\n"),
        );
    }
    let seeded = crate::agents::credentials::seed_claude_files(&home, &ws.worktree_path);
    if !seeded.is_empty() {
        tracing::info!(ws = %ws.id, files = ?seeded, "seeded claude credentials");
    }
}

/// One key's value out of `git config --get-regexp` output, or `""`.
///
/// Each line is `<key> <value>`, the value running to the end of the line and
/// free to contain spaces — which a person's name usually does. The **last**
/// match wins, because that is what `git config --get` answers for a key set
/// more than once, and this stands in for two of those.
fn config_value(output: &str, key: &str) -> String {
    output
        .lines()
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
        .next_back()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Refuses to initialise a repository inside the daemon's own data directory,
/// or on top of a workspace worktree.
///
/// [`repo::init_target_refusal`] covers the targets any caller must refuse — a
/// filesystem root, the daemon user's home. This is the one that needs to know
/// where this daemon keeps its things: `~/.bondsymphonic` holds every worktree,
/// sandbox home, object store and the registry itself, and a `git init` plus a
/// commit in there would make the daemon's own state a repository — with a
/// worktree's `.git` file, an agent's credentials and another workspace's
/// objects inside it. Every registered worktree lives under that root, so one
/// check covers both; the registry is consulted anyway, because a worktree path
/// that predates a change of data directory would not.
fn refuse_writing_into_the_daemons_own_directories(
    d: &Daemon,
    path: &Path,
) -> Result<(), RpcError> {
    let target = repo::canonical_ish(path);
    let refuse = |what: &str| {
        Err(RpcError::invalid_params(format!(
            "refusing to initialise a git repository at {}: it is {what}",
            path.display()
        )))
    };
    if target.starts_with(repo::canonical_ish(&d.dirs.root)) {
        return refuse("inside the daemon's own data directory");
    }
    if d.registry
        .list()
        .iter()
        .any(|ws| repo::canonical_ish(&ws.worktree_path) == target)
    {
        return refuse("a workspace worktree");
    }
    Ok(())
}

pub async fn create(d: &Arc<Daemon>, p: WorkspaceCreateParams) -> Result<WorkspaceInfo, RpcError> {
    let repo_path = PathBuf::from(&p.repo_path);
    // The one workspace-name rule, shared with the IDE through `proto` so that
    // what the New Agent dialog accepts and what the daemon accepts cannot drift
    // apart. First, before anything runs git: the name becomes the branch
    // `bs/<name>/work`, and a name git cannot make a ref of used to be
    // discovered halfway through `git worktree add` — after the client had
    // already been told the workspace was being created.
    bondsymphonic_proto::workspace_name::validate(&p.name).map_err(RpcError::invalid_params)?;

    // One create at a time per repository, held for the whole of the name
    // check, the branch and the registry insert.
    //
    // Two creates of one name could otherwise pass the registry check together,
    // both reach `git worktree add -b bs/<name>/work`, and have the loser's
    // cleanup run `git branch -D` on the branch the winner was checking out.
    // The branch, its loose-ref directory and its reflog are named after the
    // workspace *name*, so none of them is private to one workspace; making the
    // sequence serial is what keeps "the branch is not there yet" true from the
    // check to the `worktree add`.
    //
    // The same lock `workspace.destroy` and `workspace.merge` take, and keyed on
    // the canonicalised path, so two spellings of one repository are one
    // repository here. Released before the sandbox starts: that is slow, touches
    // no git, and holding a repository's merges behind it would be its own bug.
    let ws = {
        let repo_lock = crate::git::repo_lock(&repo_path);
        let _repo_guard = repo_lock.lock().await;
        create_the_workspace(d, &p, &repo_path).await?
    };

    seed_home(d, &ws).await;
    match start_sandbox(d, &ws).await {
        Ok(()) => Ok(d.workspace_info(&d.set_state(&ws.id, WorkspaceState::Ready).await?)),
        Err(e) => {
            let ws = d
                .set_state(
                    &ws.id,
                    WorkspaceState::Error(format!("sandbox failed: {}", e.message)),
                )
                .await?;
            Ok(d.workspace_info(&ws))
        }
    }
}

/// Everything in [`create`] that has to happen with the repository lock held:
/// the repository is identified (and initialised if the client asked for that),
/// the name is claimed in the registry, and the branch and worktree are made.
///
/// Split out so the lock has a scope rather than a lifetime: the caller drops it
/// the moment this returns, before seeding the home and starting the sandbox.
async fn create_the_workspace(
    d: &Arc<Daemon>,
    p: &WorkspaceCreateParams,
    repo_path: &Path,
) -> Result<Workspace, RpcError> {
    // Is this path a repository of its own? [`repo::classify`] is the one place
    // that answers, so `repo.inspect`, `init_repo` and this all say the same
    // thing about the same folder. Only the first two answers lead to a write:
    //
    // * a repository root, or a linked worktree of one — use it;
    // * a plain folder, or one inside an enclosing repository — not a repository,
    //   so `init_if_missing` may make it one. Adopting the enclosing repository
    //   would make the folder a worktree of a repository the user did not pick,
    //   which is the opposite of what the dialog offered, so the refusal names
    //   that repository instead;
    // * a bare repository — refused by name, because neither of the other two
    //   answers is true of it;
    // * anything else git said — a timeout, a missing binary, an ownership
    //   refusal, an unreadable gitfile. Every one of those happens *on a real
    //   repository*, so the error goes back untouched rather than becoming a
    //   licence to run `git init` over somebody's work.
    //
    // `not_a_repo` is why the path is not usable as it stands, kept for the
    // client that did not ask for it to be initialised. `None` beside
    // `existing: None` means the folder is simply not there yet.
    let mut not_a_repo: Option<RpcError> = None;
    let existing: Option<PathBuf> = if !repo::exists_as_directory(repo_path)? {
        // Asked before git, because git run in a directory that does not exist
        // fails on the spawn — with no exit code, and so indistinguishable from a
        // git that could not be started at all.
        None
    } else {
        match repo::classify(&d.git, repo_path).await? {
            repo::RepoKind::Root | repo::RepoKind::Worktree => {
                Some(repo::common_dir(&d.git, repo_path).await?)
            }
            repo::RepoKind::InsideEnclosing { root } => {
                // The repository named the way the user's filesystem spells it.
                // `root` is git's `--show-toplevel`, which answers with forward
                // slashes on Windows — so the sentence telling somebody to pick
                // `C:/code/thing` would name a path that looks nothing like the
                // one in the folder picker they just came from.
                let root = repo::canonical_ish(&root);
                not_a_repo = Some(RpcError::invalid_params(format!(
                    "{} is inside the git repository {}, but is not one itself; pick {}, \
                     or create this folder as a repository of its own",
                    repo_path.display(),
                    root.display(),
                    root.display()
                )));
                None
            }
            repo::RepoKind::NotARepo => {
                not_a_repo = Some(repo::not_a_repository_error(repo_path));
                None
            }
            repo::RepoKind::Bare => return Err(repo::bare_repository_error(repo_path)),
        }
    };
    let git_common = match existing {
        Some(c) => c,
        None if p.init_if_missing => {
            refuse_writing_into_the_daemons_own_directories(d, repo_path)?;
            // `core.hooksPath` pinned at the daemon's empty directory: `git init`
            // copies `init.templateDir` into the new repository, hooks included,
            // and the commit that follows would run them (daemon design §5.4).
            let git = d
                .git
                .clone()
                .with_config("core.hooksPath", &d.dirs.no_hooks().to_string_lossy());
            let why = not_a_repo
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "the folder is not there".to_string());
            tracing::info!(repo = %repo_path.display(), "initialising a folder that is not a repository: {why}");
            repo::init_repo(&git, repo_path).await?;
            repo::common_dir(&d.git, repo_path).await?
        }
        // The error an older client sees is the one it always saw: the IDE sets
        // the flag only after telling the user, in the New Agent dialog, that the
        // folder will be initialised.
        None => {
            return Err(not_a_repo.unwrap_or_else(|| {
                RpcError::invalid_params(format!("{} does not exist", repo_path.display()))
            }))
        }
    };
    if d.registry.find_by_name(repo_path, &p.name).is_some() {
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
    let repo_config = match crate::runs::config::load_repo_config(repo_path) {
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
        repo_path: repo_path.to_path_buf(),
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
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    d.emit_state(&ws);

    let layout = Layout {
        repo: repo_path.to_path_buf(),
        git_common,
        name: ws.name.clone(),
        branch: ws.branch.clone(),
        worktree_path: ws.worktree_path.clone(),
        objects_dir: d.dirs.objects(&id),
        no_hooks_dir: d.dirs.no_hooks(),
    };
    if let Err(failed) = worktree::create(&layout, &p.base_branch).await {
        // `worktree::create` unwinds whatever it managed to make, the branch
        // included when the branch was its own. This is the safety net for the
        // rest: the worktree directory and its registration, and never the
        // branch — `RemoveBranch::Never`, because a cleanup that cannot show the
        // branch is its own is a cleanup that must not run `git branch -D` on
        // somebody else's.
        //
        // Only when there is something to clean up, which `create` reports
        // rather than leaving to be guessed at. Half of its refusals happen
        // before it writes anything — a name already taken, a base branch that
        // does not exist, a `branch_exists` that failed outright — and the
        // other half unwind themselves. Running this anyway meant a
        // `git worktree prune` **on the user's repository**, and prune forgets
        // every registration whose directory is not there at that moment: a
        // worktree on an unmounted disk, or one the user had moved aside. Those
        // are not this call's to lose over a typo in a branch name.
        if failed.left == worktree::Leftovers::Something {
            let _ = worktree::remove_with(&layout, worktree::RemoveBranch::Never).await;
        }
        let e = failed.error;
        // The client has already seen `Creating`. Tell it why the workspace failed, then
        // send the terminal `Destroying` event a real destroy ends on, so the workspace
        // disappears from the client's list instead of hanging there forever.
        let _ = d
            .set_state(&id, WorkspaceState::Error(e.message.clone()))
            .await;
        let _ = d.set_state(&id, WorkspaceState::Destroying).await;
        let _ = d.registry.remove(&id).await;
        d.dirs.remove_workspace(&id);
        return Err(e);
    }
    Ok(ws)
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
        // one. `restore` records that state as an `Error` naming the missing directory,
        // and the branch still points at commits whose objects live only in this
        // workspace's private object dir, which the `git branch -D` in `worktree::remove`
        // would discard for good.
        //
        // Asked at once: one reads the worktree and the other the repository's
        // refs, neither looks at the other's answer, and a destroy the user is
        // waiting on should cost the slower of the two rather than both.
        let (dirty, unmerged) = tokio::try_join!(
            async {
                if !ws.worktree_path.exists() {
                    return Ok(false);
                }
                Ok(!layout
                    .worktree_git()
                    .run(&ws.worktree_path, &["status", "--porcelain"])
                    .await?
                    .stdout
                    .trim()
                    .is_empty())
            },
            async {
                if !repo::branch_exists(&d.git, &ws.repo_path, &ws.branch).await? {
                    return Ok(false);
                }
                Ok(!layout
                    .daemon_git()
                    .run(
                        &ws.repo_path,
                        &["rev-list", &format!("{}..{}", ws.base_branch, ws.branch)],
                    )
                    .await?
                    .stdout
                    .trim()
                    .is_empty())
            },
        )?;
        if dirty || unmerged {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                "workspace has uncommitted changes or unmerged commits; use force to discard",
            )
            .with_data(serde_json::json!({ "dirty": dirty, "unmerged": unmerged })));
        }
    }
    d.set_state(id, WorkspaceState::Destroying).await?;
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
    // Everything from here on touches the repository: `worktree::remove` deletes
    // the branch and the worktree registration, and `remove_workspace` deletes
    // `objects/<id>` — the private object directory a merge or a push of the
    // same repository may still be copying out into the shared store. Those hold
    // this same lock across their `absorb_objects` (`git/merge.rs`, `git/pr.rs`),
    // so taking it here is what stops a destroy from deleting objects the base
    // branch already points at and leaving the user's own `git log main`
    // unreadable.
    //
    // Deliberately not taken any earlier: the sandbox shutdown above is slow and
    // touches no git, and holding a repository's merges behind it would be its
    // own bug.
    let repo_lock = crate::git::repo_lock(&ws.repo_path);
    let _repo_guard = repo_lock.lock().await;
    if let Some(layout) = layout.as_ref() {
        if let Err(e) = worktree::remove(layout).await {
            // The ptys are closed and the sandbox is down, so the workspace must not be
            // left stranded in `Destroying`: a client would wait on a state that never
            // arrives.
            let _ = d
                .set_state(
                    id,
                    WorkspaceState::Error(format!("destroy failed: {}", e.message)),
                )
                .await;
            return Err(e);
        }
    }
    // Only now, with the worktree gone and the registry entry about to follow:
    // until this point the destroy could still have failed and left the
    // workspace behind in `Error`, and its agents' transcripts are the one thing
    // worth having out of a workspace that would not go. Reached on the forced
    // path too, where there was no layout and no worktree to remove.
    d.agents.forget_workspace(id);
    d.dirs.remove_workspace(id);
    d.registry
        .remove(id)
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(Empty {})
}

/// Says that a workspace's worktree had to be re-registered.
///
/// Worth more than a log line: the repair worked, but whatever pruned the
/// registration is still out there, and anything that was staged in the
/// worktree is unstaged now. Starts with the sentence the IDE matches on.
fn report_repaired(d: &Daemon, ws: &Workspace) {
    d.events.publish(
        Some(ws.id.clone()),
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: format!(
                "Re-registered this workspace's worktree: the repository {} had forgotten it, \
                 most likely because a git on Windows ran `git worktree prune`. Uncommitted \
                 changes are kept, anything that was staged is unstaged, and the worktree is \
                 now locked against another prune.",
                ws.repo_path.display()
            ),
            host: None,
        },
    );
}

/// Everything between "this workspace is registered" and "its sandbox is up",
/// for a workspace that already exists: the worktree directory is there, the
/// repository still lists it (put back if it can be, see
/// [`worktree::ensure_registered`]), and the sandbox starts.
///
/// Every failure comes back as a sentence a user can read, because the caller
/// puts `message` straight into `WorkspaceState::Error` and in front of them —
/// a bare `No such file or directory` from deep inside `start_sandbox` is what
/// this replaced. The code stays that of the underlying failure.
async fn bring_up(d: &Arc<Daemon>, ws: &Workspace) -> Result<(), RpcError> {
    if !ws.worktree_path.is_dir() {
        return Err(RpcError::new(
            ErrorCode::IoError,
            format!(
                "The worktree directory {} is missing. Remove the workspace to clean up.",
                ws.worktree_path.display()
            ),
        ));
    }
    let layout = layout_for(d, ws).await.map_err(|e| {
        RpcError::new(
            e.code,
            format!(
                "The repository {} could not be read: {}",
                ws.repo_path.display(),
                e.message
            ),
        )
    })?;
    if worktree::ensure_registered(&layout).await? == worktree::Registration::Repaired {
        report_repaired(d, ws);
    }
    start_sandbox(d, ws).await.map_err(|e| {
        RpcError::new(
            e.code,
            format!("The sandbox could not be started: {}", e.message),
        )
    })
}

/// Brings a workspace that exists on disk back up when the daemon starts.
///
/// A failure leaves the workspace in `Error` with the reason, never a bare
/// `SandboxDown`: that state means "the sandbox died while it was running" and
/// says nothing about what to do, which is exactly what a user looking at a
/// workspace that would not come back needs to know.
pub async fn restore(d: &Arc<Daemon>, ws: &Workspace) {
    let state = match bring_up(d, ws).await {
        Ok(()) => WorkspaceState::Ready,
        Err(e) => {
            tracing::warn!(ws = %ws.id, "restore failed: {}", e.message);
            WorkspaceState::Error(e.message)
        }
    };
    if let Err(e) = d.set_state(&ws.id, state).await {
        tracing::warn!(ws = %ws.id, "could not record the restored state: {}", e.message);
    }
}

/// One restart at a time per workspace.
///
/// Two restarts interleaved would each start a sandbox, and the second insert
/// into `Daemon::sandboxes` would drop the first handle on the floor with its
/// processes still running.
fn restart_lock(id: &WorkspaceId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .entry(id.clone())
        .or_default()
        .clone()
}

/// `workspace.restart`: stops whatever is left of the workspace's sandbox and
/// brings it up again, repairing the worktree registration on the way when it
/// can.
///
/// Allowed from `Ready` (a plain sandbox restart), `SandboxDown` and `Error`;
/// refused while the workspace is being created or destroyed, when another
/// call owns its state.
///
/// The teardown is the one `destroy` does, minus everything that touches the
/// worktree: runs and their bridges, agents, terminals, the sandbox and its
/// proxy. The agents end through their own `stop`, so each one is announced as
/// `Exited` rather than left claiming to run in a sandbox that is gone; their
/// records and transcripts stay, and the IDE starts them again (resuming their
/// sessions) once the workspace is `Ready`. The old handle leaves
/// `Daemon::sandboxes` *before* it is shut down, which is what tells its
/// `watch_sandbox` task that the death it is about to see is not news — without
/// that it would flip the restarted workspace to `SandboxDown`.
///
/// A failure leaves the workspace in `Error` with the same sentence the
/// returned error carries.
pub async fn restart(d: &Arc<Daemon>, id: &WorkspaceId) -> Result<WorkspaceInfo, RpcError> {
    let lock = restart_lock(id);
    let _guard = lock.lock().await;
    let ws = d.workspace(id)?;
    match ws.state {
        WorkspaceState::Creating | WorkspaceState::Destroying => {
            return Err(RpcError::invalid_params(format!(
                "workspace {} is being {}; it cannot be restarted now",
                ws.name,
                if ws.state == WorkspaceState::Creating {
                    "created"
                } else {
                    "destroyed"
                }
            )));
        }
        WorkspaceState::Ready | WorkspaceState::SandboxDown | WorkspaceState::Error(_) => {}
    }
    d.runs.stop_all_in(id).await;
    d.agents.stop_all_in(id).await;
    d.ptys.close_workspace(id).await;
    // The guard is dropped before the await, as in `destroy`.
    let old = d.sandboxes.lock().remove(id);
    if let Some(h) = old {
        let _ = h.shutdown().await;
    }
    d.proxies.stop(id);
    match bring_up(d, &ws).await {
        Ok(()) => Ok(d.workspace_info(&d.set_state(id, WorkspaceState::Ready).await?)),
        Err(e) => {
            tracing::warn!(ws = %id, "restart failed: {}", e.message);
            d.set_state(id, WorkspaceState::Error(e.message.clone()))
                .await?;
            Err(e)
        }
    }
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

    /// Every way the shim can fail leaves a workspace that is up and Ready with
    /// no way out of its sandbox, so each one has to reach the client rather
    /// than only the daemon's stderr.
    ///
    /// The no-sandbox backend answers `helper_exe` with a path that does not
    /// exist, which is the exact shape of the failure this guards against: a
    /// host where the daemon binary is not where the shim was looked for.
    #[tokio::test]
    async fn a_shim_that_cannot_start_is_reported_to_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let events = crate::server::broadcast::EventBus::new(16);
        let mut rx = events.subscribe();
        let d = Daemon::new(
            crate::workspace::DataDirs::new(dir.path()),
            crate::sandbox::backend_for("noop"),
            events,
        )
        .unwrap();
        let ws = Workspace {
            id: "ws_shimtest".into(),
            name: "shimtest".into(),
            repo_path: dir.path().into(),
            base_branch: "main".into(),
            branch: "bs/shimtest/work".into(),
            worktree_path: dir.path().into(),
            created_at: now_rfc3339(),
            allowlist: vec![],
            state: WorkspaceState::Ready,
            agents: vec![],
            runs: vec![],
        };
        let handle = d
            .backend
            .start(&SandboxSpec {
                id: ws.id.clone(),
                rw_binds: vec![],
                ro_binds: vec![],
                late_ro_binds: vec![],
                home: dir.path().into(),
                run_dir: dir.path().into(),
                env: vec![],
                cwd: dir.path().into(),
            })
            .await
            .unwrap();

        start_shim(&d, &ws, &handle).await;

        match rx
            .try_recv()
            .expect("a daemon.log event for this workspace")
        {
            ServerMessage::Event {
                workspace_id,
                event:
                    Event::DaemonLog {
                        level,
                        message,
                        host,
                    },
            } => {
                assert_eq!(workspace_id, Some(ws.id.clone()));
                assert_eq!(level, LogLevel::Warn);
                // The `host` field belongs to a denial; this is not one.
                assert_eq!(host, None);
                assert!(message.contains("has no network"), "{message}");
                assert!(
                    message.contains("the proxy shim did not start"),
                    "{message}"
                );
                // The path it was looked for at, so the fix is in the message.
                assert!(
                    message.contains(&handle.helper_exe().display().to_string()),
                    "{message}"
                );
            }
            other => panic!("expected a daemon.log warn, got {other:?}"),
        }
    }

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
