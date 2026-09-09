//! The bubblewrap backend: one long-lived `bwrap` per workspace, running
//! [`super::init`] inside it, with every process spawned through the exec
//! socket bound at `/run/bs/exec.sock`.

use super::exec_client::ExecClient;
use super::*;
use bondsymphonic_proto::PrereqStatus;
use std::path::Path;
use std::process::Stdio;

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

/// Where the daemon binary is bound when its own path is hidden inside the
/// sandbox. `/tmp` is a fresh tmpfs, so bwrap can always create the mount point.
const INIT_EXE_IN_SANDBOX: &str = "/tmp/.bs-init";

/// The sandbox replaces `/home` and `/tmp` with empty tmpfs mounts, so a daemon
/// binary living under either — a cargo target directory in the developer's
/// home, typically — cannot be executed at its own path inside.
fn exe_is_hidden(self_exe: &Path) -> bool {
    self_exe.starts_with("/home") || self_exe.starts_with("/tmp")
}

/// Builds the `bwrap` argument vector for one workspace sandbox.
///
/// Read-only binds are emitted before read-write ones so a writable subpath of
/// a read-only tree wins.
pub fn bwrap_args(spec: &SandboxSpec, socket_in_sandbox: &Path, self_exe: &Path) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let home_in = format!("/home/{}", whoami());
    // `/run` is a tmpfs because the root is bound read-only: without it bwrap
    // cannot create the `/run/bs` mount point.
    let mut a: Vec<String> = [
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        "/tmp",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/home",
        "--tmpfs",
        "/run",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    a.extend(["--bind".into(), s(&spec.home), home_in]);
    let exe_in = if exe_is_hidden(self_exe) {
        a.extend([
            "--ro-bind".into(),
            s(self_exe),
            INIT_EXE_IN_SANDBOX.to_string(),
        ]);
        PathBuf::from(INIT_EXE_IN_SANDBOX)
    } else {
        self_exe.to_path_buf()
    };
    for (h, sb) in &spec.ro_binds {
        a.extend(["--ro-bind".into(), s(h), s(sb)]);
    }
    for (h, sb) in &spec.rw_binds {
        a.extend(["--bind".into(), s(h), s(sb)]);
    }
    a.extend(["--bind".into(), s(&spec.run_dir), "/run/bs".into()]);
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
        let args = bwrap_args(spec, Path::new("/run/bs/exec.sock"), &self.self_exe);
        let mut child = tokio::process::Command::new(&self.bwrap_path)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
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

        let home_in = format!("/home/{}", self.user);
        let mut base_env = vec![
            ("HOME".to_string(), home_in.clone()),
            ("USER".to_string(), self.user.clone()),
            (
                "PATH".to_string(),
                format!("{home_in}/.local/bin:{home_in}/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
            ),
            ("TERM".to_string(), "xterm-256color".into()),
            ("LANG".to_string(), "C.UTF-8".into()),
        ];
        base_env.extend(spec.env.iter().cloned());
        Ok(Arc::new(BwrapHandle {
            spec: spec.clone(),
            client,
            base_env,
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

    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        // The exec socket closes when init exits, whatever killed it: an OOM, a
        // crash, bwrap being killed, or the whole sandbox being torn down.
        Some(self.client.died())
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

    #[test]
    fn bwrap_args_follow_the_spec_layout() {
        let spec = SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![("/data/worktrees/ws_1".into(), "/data/worktrees/ws_1".into())],
            ro_binds: vec![("/repo/.git".into(), "/repo/.git".into())],
            home: "/data/homes/ws_1".into(),
            run_dir: "/data/run/ws_1".into(),
            env: vec![],
            cwd: "/data/worktrees/ws_1".into(),
        };
        let args = bwrap_args(
            &spec,
            Path::new("/run/bs/exec.sock"),
            Path::new("/usr/bin/bondsymphonic-daemon"),
        );
        let s = args.join(" ");
        assert!(s.starts_with("--ro-bind / / --tmpfs /tmp --proc /proc --dev /dev"));
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
        // A daemon outside /home and /tmp is reachable at its own path.
        assert!(!s.contains(INIT_EXE_IN_SANDBOX));
    }

    #[test]
    fn a_daemon_under_home_is_bound_in_and_executed_from_tmp() {
        let spec = SandboxSpec {
            id: "ws_1".into(),
            rw_binds: vec![],
            ro_binds: vec![],
            home: "/data/homes/ws_1".into(),
            run_dir: "/data/run/ws_1".into(),
            env: vec![],
            cwd: "/data/worktrees/ws_1".into(),
        };
        let exe = Path::new("/home/bs/.bondsymphonic/target/debug/bondsymphonic-daemon");
        let s = bwrap_args(&spec, Path::new("/run/bs/exec.sock"), exe).join(" ");
        assert!(s.contains(&format!(
            "--ro-bind {} {INIT_EXE_IN_SANDBOX}",
            exe.display()
        )));
        assert!(s.ends_with(&format!(
            "-- {INIT_EXE_IN_SANDBOX} sandbox-init --socket /run/bs/exec.sock"
        )));
        // The bind lands after `--tmpfs /tmp`, or it would be masked by it.
        assert!(s.find("--tmpfs /tmp").unwrap() < s.find(INIT_EXE_IN_SANDBOX).unwrap());
    }
}
