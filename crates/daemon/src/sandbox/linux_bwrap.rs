//! The bubblewrap backend: one long-lived `bwrap` per workspace, running
//! [`super::init`] inside it, with every process spawned through the exec
//! socket bound at `/run/bs/exec.sock`.
//!
//! # Why every `bwrap` is forked from one thread of our own
//!
//! The argv carries `--die-with-parent`, which is the safety net that stops a
//! daemon killed with `SIGKILL` from leaving orphaned sandboxes behind: bwrap
//! implements it with `prctl(PR_SET_PDEATHSIG, SIGKILL)`.
//!
//! `prctl(2)` is explicit that the parent-death signal fires when the
//! **thread** that created the process terminates, not when the parent process
//! exits. On a thread pool that is a trap. Tokio runs each multi-threaded
//! worker as a task on its blocking pool, `tokio::task::block_in_place` hands
//! the worker's core to a sibling thread, and the thread that lost the core
//! goes back to the blocking pool — where an unused thread exits after
//! `KEEP_ALIVE`, ten seconds. A `bwrap` forked from such a thread is SIGKILLed
//! ten seconds later, taking the workspace's sandbox with it, and the only
//! thing that hides it is a live PTY whose reads keep feeding the pool.
//!
//! So the fork does not happen on whatever thread happens to be polling
//! [`BwrapBackend::start`]. [`on_the_spawner_thread`] hands it to one thread
//! the process owns, started once and never joined, and every sandbox in the
//! daemon's life is keyed to that. It costs one idle thread and it is immune
//! to any future `block_in_place` or blocking-pool use anywhere upstream of a
//! sandbox start — which is the point, because nothing about the create path
//! would otherwise tell the next author that the fork's thread matters.

use super::exec_client::ExecClient;
use super::*;
use bondsymphonic_proto::PrereqStatus;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

pub struct BwrapBackend {
    pub bwrap_path: PathBuf,
    pub self_exe: PathBuf,
    pub user: String,
}

impl Default for BwrapBackend {
    fn default() -> Self {
        Self {
            bwrap_path: "bwrap".into(),
            self_exe: daemon_exe(),
            user: whoami(),
        }
    }
}

/// The daemon binary that `bwrap` executes as `sandbox-init`.
///
/// `current_exe()` is the *test* binary under `cargo test`, so an explicit
/// `BS_DAEMON_EXE` wins, then a `bondsymphonic-daemon` next to the running
/// executable or one directory up (integration tests live in `target/*/deps`).
fn daemon_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("BS_DAEMON_EXE") {
        return PathBuf::from(p);
    }
    let current = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return "bondsymphonic-daemon".into(),
    };
    let candidates = [
        current.parent().map(|d| d.join("bondsymphonic-daemon")),
        current
            .parent()
            .and_then(|d| d.parent())
            .map(|d| d.join("bondsymphonic-daemon")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
        .unwrap_or(current)
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "bs".into())
}

/// A piece of work for the spawner thread. Boxed rather than a fixed
/// "spawn this command" message so the unit tests can ask the thread about
/// itself through the same path a real fork takes.
type SpawnerJob = Box<dyn FnOnce() + Send + 'static>;

/// The name the one spawner thread carries, so it is recognisable in `top`,
/// `/proc/<pid>/task/*/comm` and a debugger.
const SPAWNER_THREAD: &str = "bs-bwrap-spawner";

static SPAWNER: OnceLock<tokio::sync::mpsc::UnboundedSender<SpawnerJob>> = OnceLock::new();

/// The sender for the process-wide spawner thread, starting it on first use.
///
/// The thread runs until the process ends: the only sender is this `static`,
/// so the channel is never closed and `blocking_recv` never returns `None`.
/// That is the whole point — see the module documentation. Thread names are
/// capped at 15 bytes by the kernel, which [`SPAWNER_THREAD`] stays inside.
///
/// A job that panics is caught rather than allowed to end the loop. Ending it
/// would be the worst version of the bug this thread exists to prevent: every
/// live `bwrap` has armed `PR_SET_PDEATHSIG` against this one thread, so the
/// kernel would SIGKILL every sandbox in the daemon at once, and the sender in
/// the `static` would outlive its receiver so that every later
/// `workspace.create` failed until a restart. `f` is a `Command::spawn`, which
/// answers an `io::Result` rather than panicking — except that
/// `tokio::process::Command::spawn` panics when the runtime it is entered into
/// has no process driver. That is not how this daemon builds its runtime, so
/// the guard is for the panic nobody predicted rather than for a known one.
fn spawner() -> &'static tokio::sync::mpsc::UnboundedSender<SpawnerJob> {
    SPAWNER.get_or_init(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SpawnerJob>();
        std::thread::Builder::new()
            .name(SPAWNER_THREAD.to_string())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    // `AssertUnwindSafe` because the job owns everything it
                    // touches: it is a `FnOnce` that is dropped either way, and
                    // what it borrows across the boundary is the channel this
                    // loop keeps reading from.
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                    {
                        tracing::error!(
                            thread = SPAWNER_THREAD,
                            "a sandbox spawn panicked: {}; the spawner thread is carrying on, \
                             because ending it would kill every sandbox in this daemon",
                            panic_message(&payload)
                        );
                    }
                }
            })
            .expect("starting the bwrap spawner thread");
        tx
    })
}

/// What a caught panic said, for the two payload types `panic!` produces.
/// Anything else is reported as unprintable rather than dropped silently.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic payload that is not a string")
}

/// Runs `f` on the spawner thread and awaits its answer.
///
/// The caller's runtime handle is entered around `f`, because
/// `tokio::process::Command::spawn` registers the child with the runtime's
/// process driver and panics without one, and the spawner thread is not part
/// of any runtime itself. The job is pushed onto an unbounded channel, so
/// nothing here blocks the worker that is waiting for the result.
async fn on_the_spawner_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RpcError> {
    let handle = tokio::runtime::Handle::current();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job: SpawnerJob = Box::new(move || {
        let _entered = handle.enter();
        // The receiver is gone only if the awaiting task was cancelled, which
        // is not this thread's problem: `f` has already run.
        let _ = tx.send(f());
    });
    spawner()
        .send(job)
        .map_err(|_| sandbox_error("the bwrap spawner thread is gone".to_string()))?;
    // No answer means the job panicked: the spawner thread catches that and
    // carries on, and the sender went with the job it was part of.
    rx.await
        .map_err(|_| sandbox_error("the sandbox spawn panicked; see the log".to_string()))
}

/// Where the daemon binary is bound when its own path is hidden inside the
/// sandbox: under the sandbox's empty `/opt`, beside
/// `agents::claude::CLAUDE_IN_SANDBOX`, so bwrap can always create the mount
/// point and nothing a workspace writes can be in the way.
const DAEMON_IN_SANDBOX: &str = "/opt/bs/daemon";

/// The only environment variable the daemon hands `bwrap` itself: a fixed
/// lookup path, standard enough to find `bwrap` by name on any distribution.
/// It is what `/proc/1/environ` holds inside the sandbox, so it must name
/// nothing of the host's — see [`BwrapBackend::start`].
const BWRAP_LOOKUP_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The host directories the sandbox replaces with empty tmpfs mounts, on top
/// of the read-only root, before any bind puts a workspace's own paths back.
///
/// `/tmp` and `/run` because the sandbox needs writable scratch and a place
/// for `/run/bs` under a read-only root; `/home` because the daemon user's
/// own home (credentials, other workspaces' Claude state) is not a
/// workspace's business, `/opt` because that is where third-party software
/// lives on the host and where the daemon binds its own helpers, and `/mnt`
/// because on WSL that is every Windows drive.
const MASKED_ROOTS: [&str; 5] = ["/tmp", "/home", "/run", "/opt", "/mnt"];

/// The daemon's data directory, as far as the spec tells: `homes/<id>` and
/// `run/<id>` are two entries of it, so the directory the two share two
/// levels up is it. `None` for a spec whose home and run dir do not share
/// that layout, which only hand-built specs in tests do.
fn data_dir(spec: &SandboxSpec) -> Option<&Path> {
    let from_home = spec.home.parent()?.parent()?;
    let from_run = spec.run_dir.parent()?.parent()?;
    (from_home == from_run && from_home != Path::new("/")).then_some(from_home)
}

/// Every host path the sandbox masks with a tmpfs: [`MASKED_ROOTS`] plus the
/// daemon's data directory, which holds every other workspace's worktree,
/// home, objects and exec socket. The data directory is left out when one of
/// the fixed roots already hides it (`~/.bondsymphonic` under `/home`, a
/// test's tempdir under `/tmp`).
fn masked_roots(spec: &SandboxSpec) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = MASKED_ROOTS.iter().map(PathBuf::from).collect();
    if let Some(data) = data_dir(spec) {
        if !roots.iter().any(|r| data.starts_with(r)) {
            roots.push(data.to_path_buf());
        }
    }
    roots
}

/// Whether the daemon binary lives under a path the sandbox masks, and so
/// cannot be executed at its own path inside: a cargo target directory in the
/// developer's home, a checkout on a Windows drive, `<data dir>/bin`.
fn exe_is_hidden(spec: &SandboxSpec, self_exe: &Path) -> bool {
    masked_roots(spec).iter().any(|r| self_exe.starts_with(r))
}

/// Where the daemon binary can be executed from inside the sandbox: the path it
/// is bound in at when its own is hidden, and otherwise its own.
///
/// This is the single answer [`bwrap_args`] builds the `sandbox-init` argv from
/// and [`SandboxHandle::helper_exe`] hands to everything else, so the two can
/// never disagree about where the binary is.
pub fn exe_in_sandbox(spec: &SandboxSpec, self_exe: &Path) -> PathBuf {
    if exe_is_hidden(spec, self_exe) {
        PathBuf::from(DAEMON_IN_SANDBOX)
    } else {
        self_exe.to_path_buf()
    }
}

/// The environment every process in the sandbox starts from, init included:
/// a fixed `HOME`, `USER`, `PATH`, `TERM` and `LANG` for the workspace's own
/// home, plus whatever the spec adds (git object paths, the proxy, the
/// workspace id). Nothing from the daemon's own environment, which is the
/// shell the user started it from.
fn base_env(spec: &SandboxSpec, user: &str) -> Vec<(String, String)> {
    let home_in = format!("/home/{user}");
    let mut env = vec![
        ("HOME".to_string(), home_in.clone()),
        ("USER".to_string(), user.to_string()),
        (
            "PATH".to_string(),
            format!("{home_in}/.local/bin:{home_in}/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        ),
        ("TERM".to_string(), "xterm-256color".into()),
        ("LANG".to_string(), "C.UTF-8".into()),
    ];
    env.extend(spec.env.iter().cloned());
    env
}

/// Builds the `bwrap` argument vector for one workspace sandbox.
///
/// The read-only root comes first, then the tmpfs mounts over
/// [`masked_roots`], then the binds: read-only before read-write so a writable
/// subpath of a read-only tree wins, and every bind after every tmpfs so a
/// workspace's own paths come back through the masks.
pub fn bwrap_args(
    spec: &SandboxSpec,
    socket_in_sandbox: &Path,
    self_exe: &Path,
    user: &str,
) -> Vec<String> {
    // One spelling of "a path as an argument", shared with the git layer.
    let s = crate::git::path_arg;
    // The caller's user, not the ambient `$USER`: the two differ on CI runners and
    // whenever the daemon is started under a different account than it was configured for.
    let home_in = format!("/home/{user}");
    let mut a: Vec<String> = ["--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev"]
        .into_iter()
        .map(String::from)
        .collect();
    // Every mask before every bind, so a bind into a masked tree has its
    // mount point created in the tmpfs rather than being covered by it.
    for root in masked_roots(spec) {
        a.extend(["--tmpfs".into(), s(&root)]);
    }
    a.extend(["--bind".into(), s(&spec.home), home_in]);
    let exe_in = exe_in_sandbox(spec, self_exe);
    if exe_is_hidden(spec, self_exe) {
        a.extend(["--ro-bind".into(), s(self_exe), s(&exe_in)]);
    }
    for (h, sb) in &spec.ro_binds {
        a.extend(["--ro-bind".into(), s(h), s(sb)]);
    }
    for (h, sb) in &spec.rw_binds {
        a.extend(["--bind".into(), s(h), s(sb)]);
    }
    // After the read-write binds, or the bind of the containing directory would
    // put the writable original back on top of them.
    for (h, sb) in &spec.late_ro_binds {
        a.extend(["--ro-bind".into(), s(h), s(sb)]);
    }
    a.extend(["--bind".into(), s(&spec.run_dir), "/run/bs".into()]);
    // bwrap hands its own environment to the command it runs, and the daemon's
    // is the shell the user started it from. init gets exactly the base
    // environment its children get, and nothing else.
    a.push("--clearenv".into());
    for (k, v) in base_env(spec, user) {
        a.extend(["--setenv".into(), k, v]);
    }
    a.extend(
        [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup",
            "--unshare-net",
            "--die-with-parent",
            "--new-session",
            "--chdir",
        ]
        .into_iter()
        .map(String::from),
    );
    a.push(s(&spec.cwd));
    a.extend([
        "--".into(),
        s(&exe_in),
        "sandbox-init".into(),
        "--socket".into(),
        s(socket_in_sandbox),
    ]);
    a
}

struct BwrapHandle {
    spec: SandboxSpec,
    client: Arc<ExecClient>,
    base_env: Vec<(String, String)>,
    /// The daemon binary as this sandbox sees it, from the same
    /// [`exe_in_sandbox`] the argv was built with.
    helper_exe: PathBuf,
    /// The pid of the `bwrap` the daemon started. It stays the daemon's
    /// unreaped child until `shutdown` waits on it, so the number cannot be
    /// reused before then.
    bwrap_pid: Option<u32>,
    bwrap: tokio::sync::Mutex<tokio::process::Child>,
}

#[async_trait]
impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str {
        "linux_bwrap"
    }

    async fn check(&self) -> Vec<PrereqStatus> {
        let ok = tokio::process::Command::new(&self.bwrap_path)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-all",
                "--die-with-parent",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        vec![PrereqStatus {
            name: "sandbox".into(),
            ok,
            detail: if ok {
                "bubblewrap user namespaces work".into()
            } else {
                "bwrap --unshare-all failed".into()
            },
            fix_hint: if ok {
                None
            } else {
                Some(
                    "sudo apt-get install -y bubblewrap; see setup-wsl.sh for the AppArmor sysctl"
                        .into(),
                )
            },
        }]
    }

    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        std::fs::create_dir_all(&spec.home).map_err(|e| RpcError::io(&e))?;
        std::fs::create_dir_all(&spec.run_dir).map_err(|e| RpcError::io(&e))?;
        let host_sock = spec.run_dir.join("exec.sock");
        let _ = std::fs::remove_file(&host_sock);
        let args = bwrap_args(
            spec,
            Path::new("/run/bs/exec.sock"),
            &self.self_exe,
            &self.user,
        );
        // `--clearenv` only reaches the command bwrap runs. bwrap's own
        // process stays inside the sandbox as its pid 1 with the environment
        // it was started with, readable from `/proc/1/environ` by everything
        // in there, so the daemon's environment is not handed to it at all.
        // Not even its `PATH`: that one names the daemon user's home and, on
        // WSL, every Windows directory on the user's path. `bwrap_path`
        // defaults to the bare name `bwrap`, so a fixed lookup path is all
        // the spawn needs to resolve it.
        let mut cmd = tokio::process::Command::new(&self.bwrap_path);
        cmd.args(&args)
            .env_clear()
            .env("PATH", BWRAP_LOOKUP_PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Not `cmd.spawn()` here: `--die-with-parent` keys the sandbox's life
        // to the thread that forks it, and a tokio worker is not a thread this
        // process controls the lifetime of. See the module documentation.
        let mut child = on_the_spawner_thread(move || cmd.spawn())
            .await?
            .map_err(|e| sandbox_error(format!("bwrap: {e}")))?;
        // bwrap's stderr is the only diagnostic when the sandbox refuses to
        // start, so it is drained into the log rather than left to fill a pipe,
        // and the first lines are kept so a startup failure can say why.
        let diagnostics: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        if let Some(stderr) = child.stderr.take() {
            let id = spec.id.clone();
            let kept = diagnostics.clone();
            tokio::spawn(async move {
                let mut lines =
                    tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stderr));
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(workspace = %id, "bwrap: {line}");
                    let mut kept = kept.lock().unwrap();
                    if kept.len() < 20 {
                        kept.push(line);
                    }
                }
            });
        }

        // Wait for the socket (init is up) — 5 s.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !host_sock.exists() {
            if tokio::time::Instant::now() > deadline {
                let why = diagnostics.lock().unwrap().join("; ");
                return Err(sandbox_error(format!(
                    "sandbox init did not start within 5s: {}",
                    if why.is_empty() { "no output" } else { &why }
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let client = tokio::task::spawn_blocking({
            let p = host_sock.clone();
            move || ExecClient::connect(&p)
        })
        .await
        .map_err(|e| sandbox_error(e.to_string()))?
        .map_err(|e| sandbox_error(format!("connect exec.sock: {e}")))?;

        Ok(Arc::new(BwrapHandle {
            spec: spec.clone(),
            client,
            base_env: base_env(spec, &self.user),
            helper_exe: exe_in_sandbox(spec, &self.self_exe),
            bwrap_pid: child.id(),
            bwrap: tokio::sync::Mutex::new(child),
        }))
    }
}

#[async_trait]
impl SandboxHandle for BwrapHandle {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        self.client
            .spawn(&cmd, &self.base_env, &self.spec.cwd)
            .await
    }

    fn helper_exe(&self) -> PathBuf {
        self.helper_exe.clone()
    }

    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        // The exec socket closes when init exits, whatever killed it: an OOM, a
        // crash, bwrap being killed, or the whole sandbox being torn down.
        Some(self.client.died())
    }

    fn host_pid(&self) -> Option<u32> {
        // The `bwrap` the daemon started stays in the host's mount namespace;
        // with `--unshare-user` it forks a second `bwrap` into the new
        // namespaces, which becomes the sandbox's pid 1 and is its only child.
        // Read fresh each time: that child is not ours to keep from being
        // reaped, and its pid could belong to anything once it is gone.
        let pid = self.bwrap_pid?;
        std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        let _ = self.client.shutdown();
        let mut b = self.bwrap.lock().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(7), b.wait()).await;
        let _ = b.kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheap guard for what the module documentation explains: every fork
    /// goes to one thread this process owns, whichever task asked and whatever
    /// the scheduler was doing with it. A tokio worker would fail all three
    /// assertions -- it is unnamed, it differs between tasks, and it can be
    /// the very thread that is awaiting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_bwrap_is_forked_from_one_thread_the_daemon_owns() {
        let here = std::thread::current().id();
        let first = on_the_spawner_thread(|| std::thread::current().id())
            .await
            .unwrap();
        // From a spawned task, which is polled on a worker rather than on the
        // thread `block_on` is running.
        let second = tokio::spawn(async {
            on_the_spawner_thread(|| std::thread::current().id())
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(first, second, "every fork must use the same thread");
        assert_ne!(first, here, "the fork must leave the calling thread");
        let name = on_the_spawner_thread(|| std::thread::current().name().map(str::to_string))
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some(SPAWNER_THREAD));
    }

    /// Every live sandbox has armed `PR_SET_PDEATHSIG` against the spawner
    /// thread, so a panic that ended it would SIGKILL all of them at once and
    /// leave every later `workspace.create` with a sender whose receiver is
    /// gone. The thread has to outlive a job that panics.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_job_does_not_take_the_spawner_thread_with_it() {
        let before = on_the_spawner_thread(|| std::thread::current().id())
            .await
            .unwrap();

        // The default hook would print this test's deliberate panic and read as
        // a failure in the suite's output, so it is silenced for the one call.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let answer = on_the_spawner_thread(|| panic!("a sandbox spawn went wrong")).await;
        std::panic::set_hook(hook);
        assert!(
            answer.is_err(),
            "a job that panicked must not be reported as an answer"
        );

        let after = on_the_spawner_thread(|| std::thread::current().id())
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "the spawner thread must be the same one after a job panicked"
        );
    }

    /// The spawner thread is not part of any runtime, so a
    /// `tokio::process::Command` spawned on it needs the caller's runtime
    /// entered around it or it panics on the missing process driver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_spawner_thread_can_spawn_a_tokio_process() {
        let mut cmd = tokio::process::Command::new("true");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = on_the_spawner_thread(move || cmd.spawn())
            .await
            .unwrap()
            .unwrap();
        assert!(child.wait().await.unwrap().success());
    }

    #[test]
    fn bwrap_args_follow_the_spec_layout() {
        let spec = SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![("/data/worktrees/ws_1".into(), "/data/worktrees/ws_1".into())],
            ro_binds: vec![("/repo/.git".into(), "/repo/.git".into())],
            late_ro_binds: vec![(
                "/repo/.git/worktrees/ws_1/config.worktree".into(),
                "/repo/.git/worktrees/ws_1/config.worktree".into(),
            )],
            home: "/data/homes/ws_1".into(),
            run_dir: "/data/run/ws_1".into(),
            env: vec![],
            cwd: "/data/worktrees/ws_1".into(),
        };
        let args = bwrap_args(
            &spec,
            Path::new("/run/bs/exec.sock"),
            Path::new("/usr/bin/bondsymphonic-daemon"),
            "bs",
        );
        let s = args.join(" ");
        assert!(s.starts_with("--ro-bind / / --proc /proc --dev /dev --tmpfs /tmp"));
        assert!(s.contains("--tmpfs /home"));
        assert!(s.contains("--bind /data/homes/ws_1 /home/bs"));
        assert!(s.contains("--ro-bind /repo/.git /repo/.git"));
        assert!(s.contains("--bind /data/worktrees/ws_1 /data/worktrees/ws_1"));
        assert!(s.contains("--bind /data/run/ws_1 /run/bs"));
        assert!(s.contains(
            "--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup --unshare-net"
        ));
        assert!(s.contains("--die-with-parent --new-session"));
        assert!(
            s.ends_with("-- /usr/bin/bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock")
        );
        // ro binds come before rw binds so rw subpaths override.
        assert!(
            s.find("--ro-bind /repo/.git").unwrap() < s.find("--bind /data/worktrees").unwrap()
        );
        // A daemon on the read-only root is reachable at its own path.
        assert!(!s.contains(DAEMON_IN_SANDBOX));
    }

    /// What `bwrap_args` executes and what a handle reports as its helper are
    /// the same path in both branches, or the shim is spawned from somewhere
    /// that does not exist and the workspace silently loses its network.
    #[test]
    fn the_helper_path_is_the_one_the_argv_executes_in_both_branches() {
        let spec = SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: "/data/homes/ws_1".into(),
            run_dir: "/data/run/ws_1".into(),
            env: vec![],
            cwd: "/data/worktrees/ws_1".into(),
        };
        for exe in [
            // Hidden: under the tmpfs bwrap puts over /home, which is where the
            // daemon is installed and where cargo builds it.
            Path::new("/home/bs/.bondsymphonic/bin/bondsymphonic-daemon"),
            Path::new("/tmp/staged/bondsymphonic-daemon"),
            // Reachable at its own path.
            Path::new("/usr/local/bin/bondsymphonic-daemon"),
            Path::new("/usr/lib/bondsymphonic/bondsymphonic-daemon"),
        ] {
            let helper = exe_in_sandbox(&spec, exe);
            let args = bwrap_args(&spec, Path::new("/run/bs/exec.sock"), exe, "bs").join(" ");
            assert!(
                args.ends_with(&format!(
                    "-- {} sandbox-init --socket /run/bs/exec.sock",
                    helper.display()
                )),
                "{exe:?} is executed as {helper:?}: {args}"
            );
        }
        assert_eq!(
            exe_in_sandbox(&spec, Path::new("/home/bs/x/bondsymphonic-daemon")),
            Path::new(DAEMON_IN_SANDBOX)
        );
        assert_eq!(
            exe_in_sandbox(&spec, Path::new("/usr/bin/bondsymphonic-daemon")),
            Path::new("/usr/bin/bondsymphonic-daemon")
        );
    }

    fn spec_under(data: &str) -> SandboxSpec {
        SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![(
                format!("{data}/worktrees/ws_1").into(),
                format!("{data}/worktrees/ws_1").into(),
            )],
            ro_binds: vec![("/repo/.git".into(), "/repo/.git".into())],
            late_ro_binds: vec![],
            home: format!("{data}/homes/ws_1").into(),
            run_dir: format!("{data}/run/ws_1").into(),
            env: vec![("BS_WORKSPACE".into(), "ws_1".into())],
            cwd: format!("{data}/worktrees/ws_1").into(),
        }
    }

    fn args_for(spec: &SandboxSpec, exe: &str) -> Vec<String> {
        bwrap_args(spec, Path::new("/run/bs/exec.sock"), Path::new(exe), "bs")
    }

    /// Position of the first argument that starts a `needle` run, as a joined
    /// string offset; `None` when absent.
    fn pos(args: &[String], needle: &str) -> Option<usize> {
        args.join(" ").find(needle)
    }

    /// NT1: the daemon is hidden wherever a tmpfs sits over its path -- not
    /// only under `/home` and `/tmp` -- and is then bound read-only at a fixed
    /// place under `/opt/bs`, which is also what the argv executes.
    #[test]
    fn a_daemon_under_any_masked_path_is_bound_at_opt_bs_daemon_and_executed_from_there() {
        let spec = spec_under("/data");
        for exe in [
            "/opt/x/bondsymphonic-daemon",
            "/home/bs/.bondsymphonic/bin/bondsymphonic-daemon",
            "/tmp/staged/bondsymphonic-daemon",
            "/mnt/c/git/target/debug/bondsymphonic-daemon",
            "/run/bondsymphonic-daemon",
            "/data/bin/bondsymphonic-daemon",
        ] {
            let args = args_for(&spec, exe);
            let s = args.join(" ");
            assert!(
                s.contains(&format!("--ro-bind {exe} /opt/bs/daemon")),
                "{exe} must be bound in: {s}"
            );
            assert!(
                s.ends_with("-- /opt/bs/daemon sandbox-init --socket /run/bs/exec.sock"),
                "{exe} must be executed from the bind: {s}"
            );
            // The bind lands after `--tmpfs /opt`, or it would be masked by it.
            assert!(pos(&args, "--tmpfs /opt ").unwrap() < pos(&args, "/opt/bs/daemon").unwrap());
        }
        // A daemon on the read-only root is reachable at its own path.
        let s = args_for(&spec, "/usr/local/bin/bondsymphonic-daemon").join(" ");
        assert!(!s.contains("/opt/bs/daemon"), "{s}");
        assert!(s.ends_with(
            "-- /usr/local/bin/bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock"
        ));
    }

    /// NT3: `/mnt` (the Windows drives on WSL) and the daemon's data directory
    /// are masked before any bind, so only this workspace's own paths come
    /// back through the binds.
    #[test]
    fn mnt_and_the_data_dir_are_masked_before_the_binds() {
        let spec = spec_under("/data");
        let args = args_for(&spec, "/usr/bin/bondsymphonic-daemon");
        let s = args.join(" ");
        for masked in ["/tmp", "/home", "/run", "/opt", "/mnt", "/data"] {
            assert!(s.contains(&format!("--tmpfs {masked} ")), "{masked}: {s}");
        }
        let first_bind = pos(&args, " --bind ").unwrap();
        let first_ro = pos(&args, " --ro-bind ").unwrap();
        for masked in ["/mnt", "/data"] {
            let at = pos(&args, &format!("--tmpfs {masked} ")).unwrap();
            assert!(
                at < first_bind && at < first_ro,
                "{masked} after a bind: {s}"
            );
        }
        assert!(s.contains("--bind /data/worktrees/ws_1 /data/worktrees/ws_1"));
        assert!(s.contains("--bind /data/homes/ws_1 /home/bs"));
        assert!(s.contains("--bind /data/run/ws_1 /run/bs"));
    }

    /// A data directory under an already masked root (a test's tempdir under
    /// `/tmp`, or `~/.bondsymphonic` under `/home`) is hidden by that root and
    /// gets no tmpfs of its own.
    #[test]
    fn a_data_dir_under_a_masked_root_is_not_masked_twice() {
        let s = args_for(&spec_under("/tmp/t/data"), "/usr/bin/bondsymphonic-daemon").join(" ");
        assert!(!s.contains("--tmpfs /tmp/t/data"), "{s}");
        assert_eq!(s.matches("--tmpfs /tmp ").count(), 1, "{s}");
        let s = args_for(
            &spec_under("/home/bs/.bondsymphonic"),
            "/usr/bin/bondsymphonic-daemon",
        )
        .join(" ");
        assert!(!s.contains("--tmpfs /home/bs/.bondsymphonic"), "{s}");
        // A hand-built spec whose home and run dir share no data directory
        // masks nothing beyond the fixed roots.
        let spec = SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: "/srv/a/home".into(),
            run_dir: "/var/lib/b/run".into(),
            env: vec![],
            cwd: "/srv/a".into(),
        };
        let s = args_for(&spec, "/usr/bin/bondsymphonic-daemon").join(" ");
        assert_eq!(s.matches("--tmpfs ").count(), 5, "{s}");
    }

    /// NT2: bwrap execs init with a cleared environment and exactly the base
    /// environment its children get, so nothing the daemon inherited from the
    /// user's shell is in the sandbox at all.
    #[test]
    fn init_starts_with_a_cleared_environment_and_the_base_env() {
        let spec = spec_under("/data");
        let args = args_for(&spec, "/usr/bin/bondsymphonic-daemon");
        let s = args.join(" ");
        assert!(s.contains(" --clearenv "), "{s}");
        // After the last bind, before the namespace flags.
        let clear = pos(&args, " --clearenv ").unwrap();
        assert!(pos(&args, "--bind /data/run/ws_1 /run/bs").unwrap() < clear);
        assert!(clear < pos(&args, "--unshare-user").unwrap());
        let set = |k: &str| {
            args.windows(3)
                .find(|w| w[0] == "--setenv" && w[1] == k)
                .map(|w| w[2].clone())
        };
        assert_eq!(set("HOME").as_deref(), Some("/home/bs"), "{s}");
        assert_eq!(set("USER").as_deref(), Some("bs"), "{s}");
        assert!(
            set("PATH").is_some_and(|p| p.starts_with("/home/bs/.local/bin:")),
            "{s}"
        );
        assert_eq!(set("TERM").as_deref(), Some("xterm-256color"), "{s}");
        assert_eq!(set("LANG").as_deref(), Some("C.UTF-8"), "{s}");
        // The spec's own variables (git, proxy, workspace id) reach init too.
        assert_eq!(set("BS_WORKSPACE").as_deref(), Some("ws_1"), "{s}");
    }
}
