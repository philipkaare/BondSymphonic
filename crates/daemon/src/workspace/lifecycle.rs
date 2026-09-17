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
use crate::workspace::in_place::{self, InPlaceLayout, IGNORE_SUBMODULES};
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
    // The one door to the worktree-only paths. An in-place workspace has none
    // of them, and a caller that forgot to branch on the kind must fail here
    // rather than compute a `worktrees/<id>` gitdir inside the user's `.git`.
    if ws.kind == WorkspaceKind::InPlace {
        return Err(RpcError::internal(format!(
            "workspace {} works in place and has no worktree layout",
            ws.id
        )));
    }
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
    // Read-only *after* the read-write bind of its parent gitdir, or the parent
    // would put the writable original straight back on top of it.
    let late_ro_binds = vec![same(&layout.config_worktree())];
    finish_spec(
        d,
        ws,
        rw_binds,
        vec![same(&layout.git_common)],
        late_ro_binds,
        layout.sandbox_git_env(),
    )
}

/// The sandbox of an in-place workspace: the checkout and its `.git`
/// read-write, and what a git outside the sandbox would execute read-only on
/// top. The read-only list is the snapshot's own
/// ([`in_place::ProtectedSnapshot::late_ro_binds`]), so every entry bound is
/// an entry watched. No private object directory: the agent's objects go into
/// the repository's own store.
pub fn in_place_spec_for(
    d: &Daemon,
    ws: &Workspace,
    layout: &InPlaceLayout,
    snapshot: &in_place::ProtectedSnapshot,
) -> SandboxSpec {
    let same = |p: &PathBuf| (p.clone(), p.clone());
    finish_spec(
        d,
        ws,
        layout.rw_binds().iter().map(same).collect(),
        Vec::new(),
        snapshot.late_ro_binds().iter().map(same).collect(),
        Vec::new(),
    )
}

/// What both kinds share: the cache, the Claude binary, the proxy and the
/// workspace id.
fn finish_spec(
    d: &Daemon,
    ws: &Workspace,
    mut rw_binds: Vec<(PathBuf, PathBuf)>,
    mut ro_binds: Vec<(PathBuf, PathBuf)>,
    late_ro_binds: Vec<(PathBuf, PathBuf)>,
    mut env: Vec<(String, String)>,
) -> SandboxSpec {
    let cache = d.dirs.cache(&ws.id);
    let home_in_sandbox = PathBuf::from(format!("/home/{}", sandbox_user()));
    rw_binds.push((cache, home_in_sandbox.join(".cache")));
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
    for p in [
        d.dirs.cache(&ws.id),
        d.dirs.home(&ws.id),
        d.dirs.run(&ws.id),
    ] {
        std::fs::create_dir_all(p).map_err(|e| RpcError::io(&e))?;
    }
    let (spec, protection) = match ws.kind {
        WorkspaceKind::Worktree => {
            let layout = layout_for(d, ws).await?;
            // The daemon owns `config.worktree`, empty, and the sandbox gets it
            // read-only. Written on every start rather than only at creation, so
            // that a workspace made before this existed — or one whose file an
            // agent managed to write — starts from a file the daemon put there.
            // Nothing legitimate is lost: git only writes this file for features
            // (sparse-checkout) the read-only bind rules out inside the sandbox
            // anyway.
            std::fs::write(layout.config_worktree(), b"").map_err(|e| RpcError::io(&e))?;
            (spec_for(d, ws, &layout), None)
        }
        WorkspaceKind::InPlace => {
            // Every start, like the worktree kind's `config.worktree`: what the
            // read-only binds need is put back even if something removed it
            // while the sandbox was down.
            let layout = InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks());
            // Under the repository lock, as Close's `release` is: a worktree
            // create or removal running beside this makes and deletes
            // `.git/worktrees` too.
            let snapshot = {
                let repo_lock = crate::git::repo_lock(&ws.repo_path);
                let _repo_guard = repo_lock.lock().await;
                layout.prepare(&ws.id, &d.dirs.in_place_record(&ws.id))?;
                // After `prepare`, so it is of the entries the binds are about
                // to be made on, and before the sandbox, so nothing can replace
                // one unseen in between.
                layout.snapshot().map_err(|e| RpcError::io(&e))?
            };
            (in_place_spec_for(d, ws, &layout, &snapshot), Some(snapshot))
        }
    };
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
    let handle = match d.backend.start(&spec).await {
        Ok(h) => h,
        // Nothing will ever connect to that listener now.
        Err(e) => {
            d.proxies.stop_generation(&ws.id, proxy);
            return Err(e);
        }
    };
    d.sandboxes.lock().insert(ws.id.clone(), handle.clone());
    watch_sandbox(d, &ws.id, handle.clone(), proxy);
    // Only where there are binds to lose: the no-sandbox backend protects
    // nothing, and a `git config` the user runs beside it breaches nothing.
    if let Some(snapshot) = protection.filter(|_| d.backend.name() == BWRAP_BACKEND) {
        watch_protection(d, &ws.id, handle.clone(), snapshot);
    }
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
    let sandbox = handle.clone();
    tokio::spawn(async move {
        helper.drain().await;
        let code = (&mut child.exit).await.ok();
        // A sandbox on its way down takes the shim with it, which is ordinary; a
        // shim that dies under a live sandbox has cost that workspace its
        // network and is worth saying so. Compared by handle, not by id: after
        // a restart the id has a new sandbox, and the old shim dying is not news.
        let alive = d
            .sandboxes
            .lock()
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &sandbox));
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
        // Checked and removed under one guard: a restart registering its new
        // handle between the two would otherwise have that handle removed here.
        {
            let mut live = d.sandboxes.lock();
            let still_ours = live
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &handle));
            if !still_ours {
                return;
            }
            live.remove(&id);
        }
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

/// Stops an in-place sandbox whose protected git entries were replaced from
/// outside it (see [`InPlaceLayout::snapshot`]).
///
/// Polled rather than watched: inotify never hears about a rename Windows
/// makes on a DrvFs mount, and the check is a few `lstat` calls. Tied to one
/// sandbox handle, as [`watch_sandbox`] is, and quiet once that handle is no
/// longer the registered one -- a restart has a watcher of its own.
///
/// A breach is said out loud with what changed, and then handled as a
/// restart's teardown would be, under the workspace's gate: the agents are
/// announced as exited and the workspace goes to `Error` until Retry, which
/// prepares and snapshots again.
/// The workspace whose next protection check should panic. **A test hook, and
/// nothing outside a test ever writes it.**
///
/// It exists because the fail-closed arm below cannot be reached any other way:
/// nothing a repository can contain makes [`in_place::ProtectedSnapshot::check`]
/// panic, and an untested fail-closed path in a security control is how
/// fail-open gets put back by the next person to touch it. Keyed by workspace
/// so that two tests in one binary cannot take each other's. Writing it needs
/// code running in the daemon's own process, and what it does there is stop an
/// agent rather than free one.
#[doc(hidden)]
pub static PANIC_IN_PROTECTION_CHECK_FOR: parking_lot::Mutex<Option<WorkspaceId>> =
    parking_lot::Mutex::new(None);

/// Whether [`PANIC_IN_PROTECTION_CHECK_FOR`] names `id`, taking it if it does.
fn panic_hook_takes(id: &WorkspaceId) -> bool {
    let mut asked = PANIC_IN_PROTECTION_CHECK_FOR.lock();
    if asked.as_ref() == Some(id) {
        *asked = None;
        return true;
    }
    false
}

fn watch_protection(
    d: &Arc<Daemon>,
    id: &WorkspaceId,
    handle: Arc<dyn SandboxHandle>,
    snapshot: in_place::ProtectedSnapshot,
) {
    let d = d.clone();
    let id = id.clone();
    let snapshot = Arc::new(snapshot);
    tokio::spawn(async move {
        let is_current = |d: &Daemon| {
            d.sandboxes
                .lock()
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
        };
        // The pid the check was made with, kept so the "no pid" line is said
        // only once the breach is going to be acted on: a sandbox that has
        // already died has no pid either, and that is not worth an error.
        let mut had_pid;
        let outcome = loop {
            tokio::time::sleep(in_place::PROTECTION_POLL).await;
            if !is_current(&d) {
                return;
            }
            // Always with a pid: a replaced file can come back with the same
            // inode, so the sandbox's own mount table is the check that
            // counts. A live sandbox whose pid cannot be found has no mount
            // table to vouch for it, so it gets pid 0, which has no `/proc`
            // entry and so reads as a breach.
            let pid = handle.host_pid();
            had_pid = pid.is_some();
            let pid = pid.unwrap_or(0);
            // Off the runtime: on a DrvFs mount even an `lstat` can wait on
            // Windows.
            let check = {
                let snapshot = snapshot.clone();
                let asked_to_fail = panic_hook_takes(&id);
                tokio::task::spawn_blocking(move || {
                    assert!(!asked_to_fail, "a test asked this check to fail");
                    snapshot.check(Some(pid))
                })
            };
            match check.await {
                Ok(Some(breach)) => break Some(breach),
                Ok(None) => {}
                // Fails closed. A check that cannot answer is not an answer of
                // "nothing changed", and returning here would leave a sandbox
                // with an agent in it running unwatched for as long as the
                // workspace lives.
                Err(e) => {
                    tracing::error!(ws = %id, error = %e, "the protection check panicked");
                    break None;
                }
            }
        };
        let gate = gate(&id);
        let mut count = gate.lock().await;
        // Looked at again under the gate, and before anything is said: a
        // restart or Close that got there first owns the workspace now, and
        // the sandbox it took down is no breach.
        if !is_current(&d) || d.registry.get(&id).is_none() {
            return;
        }
        // Nor is a sandbox that died underneath the check. Its pid 1 is gone,
        // so `host_pid` answers `None` and the mount table of pid 0 says
        // nothing is mounted, which every entry then reads as unbound. An OOM
        // kill is not tampering, nothing is running in that sandbox to be
        // protected from, and `watch_sandbox` is already on its way to
        // reporting it as down.
        if handle.died().is_some_and(|rx| *rx.borrow()) {
            return;
        }
        if !had_pid {
            tracing::error!(ws = %id, "cannot find the sandbox's pid to read its mount table");
        }
        let sentence = match &outcome {
            Some(breach) => breach.sentence(),
            None => "The check that keeps this workspace's git files safe from the agent could \
                     not be made, so its sandbox was stopped. Press Retry."
                .to_string(),
        };
        match &outcome {
            Some(breach) => {
                tracing::warn!(ws = %id, entries = ?breach.entries, diff = %breach.config_diff, "protected git entries changed")
            }
            None => tracing::warn!(ws = %id, "stopping a sandbox whose protection check failed"),
        }
        let diff = outcome.map(|b| b.config_diff).unwrap_or_default();
        d.events.publish(
            Some(id.clone()),
            Event::DaemonLog {
                level: LogLevel::Warn,
                message: if diff.is_empty() {
                    sentence.clone()
                } else {
                    format!("{sentence}\n{diff}")
                },
                host: None,
            },
        );
        *count += 1;
        tear_down_sandbox(&d, &id).await;
        if let Err(e) = d.set_state(&id, WorkspaceState::Error(sentence)).await {
            tracing::warn!(ws = %id, "could not record the stopped sandbox: {}", e.message);
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
    // cleanup delete the branch the winner was checking out.
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
        if p.in_place {
            create_in_place(d, &p, &repo_path).await?
        } else {
            create_the_workspace(d, &p, &repo_path).await?
        }
    };

    seed_home(d, &ws).await;
    // Under the new workspace's gate, like every other sandbox start: a
    // protection breach found while the shim is still coming up then waits
    // for `Ready` to be written, rather than being overwritten by it.
    let gate = gate(&ws.id);
    let _count = gate.lock().await;
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
    let git_common = resolve_repository(d, p, repo_path).await?;
    if d.registry.find_by_name(repo_path, &p.name).is_some() {
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
        repo_path: repo_path.to_path_buf(),
        base_branch: p.base_branch.clone(),
        branch: Workspace::branch_for(&p.name),
        worktree_path: d.dirs.worktree(&id),
        created_at: now_rfc3339(),
        allowlist: effective_allowlist(repo_path),
        kind: WorkspaceKind::Worktree,
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
        // branch is its own is a cleanup that must not delete
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

/// [`create_the_workspace`] for `in_place: true`: the checkout is used as it
/// is. No branch, no `git worktree add`, no lock, no private objects -- so,
/// unlike a worktree create, nothing is made in the repository here and there
/// is nothing to unwind on a refusal.
async fn create_in_place(
    d: &Arc<Daemon>,
    p: &WorkspaceCreateParams,
    repo_path: &Path,
) -> Result<Workspace, RpcError> {
    // Before `resolve_repository`, which may `git init` the folder: a target
    // that is refused anyway must not be made a repository first.
    if let Some(why) = in_place::target_refusal(repo_path, &d.dirs.root) {
        return Err(RpcError::invalid_params(format!(
            "refusing to work in place in {}: {why}",
            repo_path.display()
        )));
    }
    resolve_repository(d, p, repo_path).await?;
    // Asked again after `resolve_repository`, which may just have made the
    // folder a repository of its own.
    let kind = repo::classify(&d.git, repo_path).await?;
    if let Some(why) = in_place::in_place_refusal(&kind, repo_path) {
        return Err(RpcError::invalid_params(why));
    }
    // One agent per checkout. By canonical path, so two spellings of one
    // folder are one folder, as they are for the repository lock held here.
    let target = repo::canonical_ish(repo_path);
    if let Some(other) = d.registry.list().into_iter().find(|w| {
        w.kind == WorkspaceKind::InPlace && repo::canonical_ish(&w.worktree_path) == target
    }) {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!(
                "this checkout already has an in-place workspace: {}",
                other.name
            ),
        ));
    }
    if d.registry.find_by_name(repo_path, &p.name).is_some() {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!("workspace {} already exists for this repo", p.name),
        ));
    }
    let layout = InPlaceLayout::new(repo_path, &d.dirs.no_hooks());
    // What would stop every sandbox start is a refusal now, not a workspace
    // registered only to sit in `Error`.
    layout.check_preparable()?;
    // Display only: never switched, created or deleted, and nothing merges
    // into it, so the base branch says the same.
    let branch = in_place::head_branch(&layout.git(), repo_path).await?;
    let ws = Workspace {
        id: WorkspaceId(new_id("ws_")),
        name: p.name.clone(),
        repo_path: repo_path.to_path_buf(),
        base_branch: branch.clone(),
        branch,
        worktree_path: repo_path.to_path_buf(),
        created_at: now_rfc3339(),
        allowlist: effective_allowlist(repo_path),
        kind: WorkspaceKind::InPlace,
        state: WorkspaceState::Creating,
        agents: vec![],
        runs: vec![],
    };
    d.registry
        .insert(ws.clone())
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    d.emit_state(&ws);
    Ok(ws)
}

/// The repository's own `[network] allow` on top of the defaults. A
/// `bondsymphonic.toml` that will not parse is worth a warning and nothing
/// more: refusing to create the workspace over it would leave the user unable
/// to open the very repo they need a workspace in to fix the file.
fn effective_allowlist(repo_path: &Path) -> Vec<String> {
    let repo_config = match crate::runs::config::load_repo_config(repo_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "using the default allowlist");
            None
        }
    };
    crate::net::allowlist::effective(repo_config.as_ref())
}

/// Identifies the repository at `repo_path`, initialising it first when the
/// client asked for that, and answers its common git directory. Shared by both
/// kinds of create, so both refuse and initialise the same folders the same way.
async fn resolve_repository(
    d: &Arc<Daemon>,
    p: &WorkspaceCreateParams,
    repo_path: &Path,
) -> Result<PathBuf, RpcError> {
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
    Ok(git_common)
}

pub async fn destroy(d: &Daemon, id: &WorkspaceId, force: bool) -> Result<Empty, RpcError> {
    // For the whole destroy: a restart or startup restore of this workspace
    // that is starting a sandbox finishes first, and its sandbox is then torn
    // down below like any other, rather than registered after the teardown for
    // a workspace that no longer exists. The workspace is read under it.
    let gate = gate(id);
    let mut count = gate.lock().await;
    *count += 1;
    let ws = d.workspace(id)?;
    if ws.kind == WorkspaceKind::InPlace {
        // `force` means nothing here, and there is no dirty or unmerged check:
        // the work is in the user's own checkout and none of it is deleted.
        close_in_place(d, &ws).await?;
        gates().lock().remove(id);
        return Ok(Empty {});
    }
    // From here on the destroy deletes a directory tree, and the only tree it
    // may ever delete is the one the daemon made for this workspace. A registry
    // entry is a file on disk: a daemon build from before in-place workspaces
    // existed drops the `kind` field when it rewrites `workspaces.json`, and the
    // entry then reads back as a worktree workspace whose `worktree_path` is the
    // user's own checkout. Everything below -- `worktree remove --force`, and
    // `remove_dir_all` after it -- would take that checkout, `.git` and all.
    // There is no legitimate case to weigh against: `workspace.create` puts
    // every worktree it makes under `worktrees/<id>`, so a path that is not
    // there is a corrupted entry, whatever `force` says.
    let expected = d.dirs.worktree(id);
    if ws.worktree_path != expected {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!(
                "workspace {id} records a worktree at {}, which is not the {} BondSymphonic \
                 would have made; refusing to remove it. Its registry entry is damaged: edit \
                 {} by hand to remove the entry.",
                ws.worktree_path.display(),
                expected.display(),
                d.dirs.registry_file().display()
            ),
        ));
    }
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
        // workspace's private object dir, which the branch deletion in `worktree::remove`
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
                // A registration a prune took leaves git nothing to answer
                // with. Not knowing is treated as dirty: the user is asked to
                // discard, which is what a forced destroy is for.
                if !layout.worktree_gitdir().join("HEAD").is_file() {
                    return Ok(true);
                }
                // `IGNORE_SUBMODULES`: see there.
                Ok(!layout
                    .worktree_git()
                    .run(
                        &ws.worktree_path,
                        &["status", "--porcelain", IGNORE_SUBMODULES],
                    )
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
    // Ids are never reused, so the gate is one entry nobody will ask for
    // again. Anyone already waiting on it holds a clone and finds the
    // workspace gone.
    gates().lock().remove(id);
    Ok(Empty {})
}

/// `workspace.destroy` for an in-place workspace: everything that runs in its
/// sandbox stops, the daemon's own directories and records for it go, and the
/// checkout is left exactly as it was. Never `worktree::remove`, never a
/// branch, never a write to the repository beyond taking back what
/// [`InPlaceLayout::prepare`] put there.
async fn close_in_place(d: &Daemon, ws: &Workspace) -> Result<(), RpcError> {
    let id = &ws.id;
    d.set_state(id, WorkspaceState::Destroying).await?;
    d.watchers.disable(id);
    tear_down_sandbox(d, id).await;
    {
        // The same lock every other write to this repository takes.
        let repo_lock = crate::git::repo_lock(&ws.repo_path);
        let _repo_guard = repo_lock.lock().await;
        InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks())
            .release(id, &d.dirs.in_place_record(id));
    }
    d.agents.forget_workspace(id);
    // `remove_workspace` removes `<data>/worktrees/<id>`, never `worktree_path`.
    d.dirs.remove_workspace(id);
    d.registry
        .remove(id)
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(())
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
                "Re-registered this workspace's worktree: the repository {} had forgotten it \
                 or its registration was incomplete, most likely because a git on Windows ran \
                 `git worktree prune`. Uncommitted changes are kept, anything that was staged \
                 is unstaged, a detached HEAD is back on {}, and the worktree is now locked \
                 against another prune.",
                ws.repo_path.display(),
                ws.branch
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
    match ws.kind {
        // No worktree to repair and no registration to put back: only the
        // checkout itself has to still be there.
        WorkspaceKind::InPlace => {
            InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).check_repository()?
        }
        WorkspaceKind::Worktree => worktree_bring_up(d, ws).await?,
    }
    start_sandbox(d, ws).await.map_err(|e| {
        RpcError::new(
            e.code,
            format!("The sandbox could not be started: {}", e.message),
        )
    })
}

/// [`bring_up`]'s checks for a worktree workspace: the directory is there and
/// the repository lists it, re-registered when it can be.
async fn worktree_bring_up(d: &Arc<Daemon>, ws: &Workspace) -> Result<(), RpcError> {
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
    Ok(())
}

/// One workspace's gate: held by whichever of restore, restart and destroy is
/// working on its sandbox, around a count of the restarts and destroys that
/// have run.
///
/// Without it the three interleave. A destroy that lands while a restart is
/// starting the sandbox tears down before the new handle exists, and the
/// restart then registers a live sandbox for a workspace that is gone; a Retry
/// that lands while the startup restore is bringing the same workspace up
/// starts a second sandbox and drops the first one's handle with its processes
/// still running.
///
/// The count is how the startup restore, which works from a list taken before
/// the first request was accepted, tells that somebody got to a workspace
/// before it did. Lock order is gate, then repository lock, everywhere.
type Gate = Arc<tokio::sync::Mutex<u64>>;

fn gates() -> &'static parking_lot::Mutex<std::collections::HashMap<WorkspaceId, Gate>> {
    static GATES: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<WorkspaceId, Gate>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

fn gate(id: &WorkspaceId) -> Gate {
    gates().lock().entry(id.clone()).or_default().clone()
}

/// A workspace as the startup restore found it, with its gate's count at that
/// moment.
pub struct RestoreEntry {
    ws: Workspace,
    /// `None` when the gate was held at the time, which only a restart or a
    /// destroy already under way can do: the restore then leaves the workspace
    /// to it.
    ops: Option<u64>,
}

/// The workspaces the startup restore will bring up. `main` takes this before
/// the server accepts its first request, so nothing a client does can be in it.
pub fn restore_snapshot(d: &Daemon) -> Vec<RestoreEntry> {
    d.registry
        .list()
        .into_iter()
        .map(|ws| {
            let ops = gate(&ws.id).try_lock().ok().map(|count| *count);
            RestoreEntry { ws, ops }
        })
        .collect()
}

/// Brings a workspace that exists on disk back up when the daemon starts.
///
/// A failure leaves the workspace in `Error` with the reason, never a bare
/// `SandboxDown`: that state means "the sandbox died while it was running" and
/// says nothing about what to do, which is exactly what a user looking at a
/// workspace that would not come back needs to know.
///
/// Runs alongside the accept loop, so it takes the workspace's gate and looks
/// again under it. A workspace that is gone, or that a restart or destroy has
/// touched since the snapshot, is left alone: whoever touched it owns its
/// state now. A workspace the last daemon left `Creating` or `Destroying` is
/// not brought up either — nobody is going to finish that, and a half-removed
/// worktree must not come back `Ready` — but put in `Error` with what to do.
pub async fn restore(d: &Arc<Daemon>, entry: RestoreEntry) {
    let id = entry.ws.id.clone();
    let gate = gate(&id);
    let count = gate.lock().await;
    let Some(ws) = d.registry.get(&id) else {
        return;
    };
    if entry.ops != Some(*count) {
        tracing::debug!(ws = %id, "restore skipped: the workspace was restarted or destroyed meanwhile");
        return;
    }
    // What the IDE calls taking a workspace away, which for an in-place one
    // deletes nothing.
    let verb = if ws.kind == WorkspaceKind::InPlace {
        "Close"
    } else {
        "Remove"
    };
    let state = match ws.state {
        WorkspaceState::Creating => WorkspaceState::Error(format!(
            "Creating this workspace was interrupted when the daemon stopped. {verb} the \
             workspace to clean up."
        )),
        WorkspaceState::Destroying => WorkspaceState::Error(format!(
            "Removing this workspace was interrupted when the daemon stopped. {verb} the \
             workspace again to finish."
        )),
        WorkspaceState::Ready | WorkspaceState::SandboxDown | WorkspaceState::Error(_) => {
            // A sandbox already registered is not this restore's to leak.
            tear_down_sandbox(d, &id).await;
            match bring_up(d, &ws).await {
                Ok(()) => WorkspaceState::Ready,
                Err(e) => {
                    tracing::warn!(ws = %id, "restore failed: {}", e.message);
                    WorkspaceState::Error(e.message)
                }
            }
        }
    };
    if let Err(e) = d.set_state(&id, state).await {
        tracing::warn!(ws = %id, "could not record the restored state: {}", e.message);
    }
}

/// Stops everything that runs in a workspace's sandbox, and the sandbox.
///
/// The teardown `destroy` does, minus everything that touches the worktree:
/// runs and their bridges, agents, terminals, the sandbox and its proxy. The
/// agents end through their own `stop`, so each one is announced as `Exited`
/// rather than left claiming to run in a sandbox that is gone; their records
/// and transcripts stay. The handle leaves `Daemon::sandboxes` *before* it is
/// shut down, which is what tells its `watch_sandbox` task that the death it is
/// about to see is not news.
async fn tear_down_sandbox(d: &Daemon, id: &WorkspaceId) {
    d.runs.stop_all_in(id).await;
    d.agents.stop_all_in(id).await;
    d.ptys.close_workspace(id).await;
    // The guard is dropped before the await, as in `destroy`.
    let old = d.sandboxes.lock().remove(id);
    if let Some(h) = old {
        let _ = h.shutdown().await;
    }
    d.proxies.stop(id);
}

/// `workspace.restart`: stops whatever is left of the workspace's sandbox and
/// brings it up again, repairing the worktree registration on the way when it
/// can.
///
/// Allowed from `Ready` (a plain sandbox restart), `SandboxDown` and `Error`;
/// refused while the workspace is being created or destroyed, when another
/// call owns its state.
///
/// The teardown is [`tear_down_sandbox`]; the IDE starts the agents again
/// (resuming their sessions) once the workspace is `Ready`. Under the
/// workspace's [`Gate`], so it waits for — and is waited for by — a destroy or
/// the startup restore of the same workspace.
///
/// A failure leaves the workspace in `Error` with the same sentence the
/// returned error carries.
pub async fn restart(d: &Arc<Daemon>, id: &WorkspaceId) -> Result<WorkspaceInfo, RpcError> {
    let gate = gate(id);
    let mut count = gate.lock().await;
    // Read under the gate: a destroy that held it may have removed the
    // workspace, or left it in `Error`.
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
    *count += 1;
    tear_down_sandbox(d, id).await;
    match bring_up(d, &ws).await {
        Ok(()) => Ok(d.workspace_info(&d.set_state(id, WorkspaceState::Ready).await?)),
        Err(e) => {
            tracing::warn!(ws = %id, "restart failed: {}", e.message);
            // The reason is what the caller needs; a registry that would not
            // take the write is the daemon's own problem and goes to the log.
            if let Err(s) = d
                .set_state(id, WorkspaceState::Error(e.message.clone()))
                .await
            {
                tracing::warn!(ws = %id, "could not record the failed restart: {}", s.message);
            }
            Err(e)
        }
    }
}

pub async fn status(d: &Daemon, id: &WorkspaceId) -> Result<WorkspaceStatusResult, RpcError> {
    let ws = d.workspace(id)?;
    // Both pin git's discovery (`worktree_git`, not `daemon_git`): the tree is
    // agent-writable either way, so git must not be allowed to find its
    // repository (and its config) from it.
    let git = match ws.kind {
        WorkspaceKind::InPlace => InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).git(),
        WorkspaceKind::Worktree => layout_for(d, &ws).await?.worktree_git(),
    };
    // `IGNORE_SUBMODULES`: see there.
    let out = git
        .run(
            &ws.worktree_path,
            &[
                "status",
                "--porcelain=v2",
                "--untracked-files=all",
                IGNORE_SUBMODULES,
            ],
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
            kind: WorkspaceKind::Worktree,
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
