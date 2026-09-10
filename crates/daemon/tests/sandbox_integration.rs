#![cfg(target_os = "linux")]
//! Proves the bubblewrap backend really isolates a workspace. Skipped when
//! `bwrap` cannot create user namespaces (see `scripts/setup-wsl.sh`).

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::linux_bwrap::{bwrap_args, BwrapBackend};
use bondsymphonic_daemon::sandbox::protocol::{encode, InitRequest};
use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::{
    AgentAdapterKind, AgentMessageBody, AgentStartOptions, AgentStartParams, AgentState, Event,
    ServerMessage, WorkspaceCreateParams, WorkspaceState,
};
use std::io::Write;
use std::os::unix::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Sleep duration used as a process marker: distinctive enough that `pgrep`
/// cannot confuse this test's child with anything else on the machine.
const MARKER: &str = "2477";

/// Marker for the grandchild in the process-group kill test.
const GROUP_MARKER: &str = "2478";

/// Marker for the process a sandboxed attacker must not be able to kill.
const HOSTILE_MARKER: &str = "2479";

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-all",
            "--die-with-parent",
            "true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

type Handle = std::sync::Arc<dyn bondsymphonic_daemon::sandbox::SandboxHandle>;

/// True while any process on the host has `pattern` in its command line.
fn pgrep(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .arg("-af")
        .arg(pattern)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Interface names carrying an IP address on the host, loopback aside. None of
/// them may be visible from inside a sandbox.
fn host_addressed_interfaces() -> Vec<String> {
    let out = match std::process::Command::new("ip")
        .args(["-o", "addr"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|n| *n != "lo")
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

async fn wait_until(limit: std::time::Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline && !cond() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn run_in(handle: &Handle, script: &str) -> (i32, String) {
    run_argv(handle, vec!["sh".into(), "-c".into(), script.into()]).await
}

/// dash has no `/dev/tcp`, so probes that need it run under bash or they would
/// fail for the wrong reason.
async fn run_bash(handle: &Handle, script: &str) -> (i32, String) {
    run_argv(handle, vec!["bash".into(), "-c".into(), script.into()]).await
}

async fn run_argv(handle: &Handle, argv: Vec<String>) -> (i32, String) {
    let mut child = handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let code = child.exit.await.unwrap();
    (code, out)
}

#[tokio::test]
async fn bwrap_isolates_filesystem_pids_and_network() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_t".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: dir.path().join("run"),
        env: vec![],
        cwd: work.clone(),
    };
    let backend = backend_for("linux_bwrap");
    let handle = backend.start(&spec).await.unwrap();

    let (code, _) = run_in(&handle, "touch /etc/bs-should-fail").await;
    assert_ne!(code, 0, "root filesystem must be read-only");
    let (code, _) = run_in(&handle, &format!("echo ok > {}/inside.txt", work.display())).await;
    assert_eq!(code, 0, "worktree must be writable");
    assert!(work.join("inside.txt").exists());
    let (code, out) = run_in(&handle, "echo $HOME; touch $HOME/x && echo home-ok").await;
    assert_eq!(code, 0);
    assert!(out.contains("home-ok"), "home must be writable: {out}");
    let (_, out) = run_in(&handle, "echo $$").await;
    assert!(
        out.trim().parse::<i32>().unwrap() < 200,
        "own PID namespace expected, got {out}"
    );
    let (code, out) = run_bash(&handle, "exec 3<>/dev/tcp/1.1.1.1/80").await;
    assert_ne!(code, 0, "network must be unreachable: {out}");
    // A failed connect could also mean a missing route, so check the network
    // namespace itself. It cannot be asserted to hold loopback alone: the
    // kernel creates a `tunl0` and `sit0` stub in every new namespace where the
    // ipip and sit modules are loaded. What it must not hold is any interface
    // the host actually uses.
    let (code, out) = run_in(&handle, "cat /proc/net/dev").await;
    assert_eq!(code, 0);
    let interfaces: Vec<&str> = out
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    assert!(interfaces.contains(&"lo"), "loopback expected: {out}");
    for name in &interfaces {
        assert!(
            !["eth", "en", "wl", "docker", "veth"]
                .iter()
                .any(|p| name.starts_with(p)),
            "real network interface {name} must not exist inside the sandbox: {out}"
        );
    }
    // Belt and braces: whatever the host is actually using must be absent too,
    // whatever it happens to be called.
    for host_if in host_addressed_interfaces() {
        assert!(
            !interfaces.contains(&host_if.as_str()),
            "host interface {host_if} must not exist inside the sandbox: {out}"
        );
    }

    let mut child = handle
        .spawn(SandboxCommand {
            argv: vec!["sh".into()],
            env: vec![],
            cwd: None,
            pty: Some(PtySize { cols: 80, rows: 24 }),
        })
        .await
        .unwrap();
    let mut pty = child.pty.take().unwrap();
    pty.writer.write_all(b"stty size; exit\n").await.unwrap();
    pty.writer.flush().await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(Ok(n)) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        pty.reader.read(&mut chunk),
    )
    .await
    {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    assert!(
        String::from_utf8_lossy(&buf).contains("24 80"),
        "pty size should be applied: {}",
        String::from_utf8_lossy(&buf)
    );
    assert_eq!(child.exit.await.unwrap(), 0);
    handle.shutdown().await.unwrap();
}

/// End-to-end through the real workspace lifecycle (not a hand-built
/// `SandboxSpec`): a bwrap-backed workspace lets the agent commit on its own
/// branch, but the shared ref store and object database stay read-only, and
/// the daemon still sees the commit through the object alternate.
#[tokio::test]
async fn bwrap_workspace_protects_main_branch_and_shared_objects() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "sb".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ws.state,
        WorkspaceState::Ready,
        "sandbox must come up: {:?}",
        ws.state
    );
    let handle = daemon.sandbox(&ws.id).unwrap();
    let wt = ws.worktree_path.clone();

    // Agent commits on its own branch: allowed.
    let (code, out) = run_in(
        &handle,
        &format!(
            "cd '{wt}' && git -c user.name=a -c user.email=a@a commit -q --allow-empty -m sandboxed && git rev-parse --abbrev-ref HEAD"
        ),
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.trim(), "bs/sb/work");

    // Moving main: denied (refs/heads is read-only).
    let (code, out) = run_in(
        &handle,
        &format!("cd '{wt}' && git update-ref refs/heads/main HEAD 2>&1"),
    )
    .await;
    assert_ne!(code, 0, "main must be protected: {out}");

    // Writing into the shared object store: denied.
    let git_common = bondsymphonic_daemon::git::repo::common_dir(&daemon.git, &repo)
        .await
        .unwrap();
    let (code, _) = run_in(
        &handle,
        &format!("touch '{}/objects/should-fail'", git_common.display()),
    )
    .await;
    assert_ne!(code, 0);

    // The daemon sees the commit through alternates.
    let layout = lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap())
        .await
        .unwrap();
    let subject = layout
        .daemon_git()
        .run(&repo, &["log", "-1", "--format=%s", "bs/sb/work"])
        .await
        .unwrap();
    assert_eq!(subject.stdout.trim(), "sandboxed");

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

/// The real Claude Code binary has to be reachable *inside* a workspace
/// sandbox, which is the one thing the fake `claude` the rest of the suite runs
/// can never show.
///
/// The sandbox mounts a tmpfs over `/home` and the workspace's own home over
/// `/home/<user>`, so the daemon user's `~/.local/bin/claude` is not there
/// however the `PATH` is arranged; `spec_for` binds the resolved binary in at
/// `CLAUDE_IN_SANDBOX` and `claude_argv` names that path. This runs the real
/// binary through the real workspace lifecycle and asserts the version it
/// prints, so a bind that is missing, masked or pointed at the wrong file fails
/// here rather than at a user's first `agent.start`.
///
/// `--version` is the only invocation this suite makes of the real CLI: it
/// prints and exits, contacts nothing, and cannot log anyone in or out.
#[tokio::test]
async fn the_real_claude_binary_runs_inside_a_bwrap_workspace() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let Some(host_claude) = bondsymphonic_daemon::agents::claude::host_claude_bin() else {
        eprintln!("SKIP: Claude Code is not installed for this user");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "claudebin".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);
    let handle = daemon.sandbox(&ws.id).unwrap();

    // The daemon user's own install is masked, whatever the sandbox `PATH`
    // says: this is the failure the bind exists to fix.
    let (code, _) = run_in(&handle, &format!("test -e '{}'", host_claude.display())).await;
    assert_ne!(
        code, 0,
        "the daemon user's home must not be visible inside the sandbox"
    );

    // The bind the workspace spec was built from, which is also what
    // `claude_argv` names for this backend (pinned by the unit test in
    // `agents::claude`, where `BS_CLAUDE_BIN` can be controlled -- it is
    // process-wide, and another test in this binary sets it).
    let (bound_host, bound_in_sandbox) =
        bondsymphonic_daemon::agents::claude::claude_ro_bind().unwrap();
    assert_eq!(bound_host, host_claude);
    assert_eq!(
        bound_in_sandbox,
        std::path::Path::new(bondsymphonic_daemon::agents::claude::CLAUDE_IN_SANDBOX)
    );
    // Spawned by absolute path, not by name, so nothing here depends on the
    // sandbox `PATH` either.
    let (code, out) = run_argv(
        &handle,
        vec![
            bondsymphonic_daemon::agents::claude::CLAUDE_IN_SANDBOX.to_owned(),
            "--version".into(),
        ],
    )
    .await;
    assert_eq!(code, 0, "`claude --version` inside the sandbox: {out}");
    let printed = out.trim();
    assert!(
        printed.starts_with(bondsymphonic_daemon::agents::claude::TESTED_CLAUDE_VERSION)
            || printed
                .split_whitespace()
                .next()
                .is_some_and(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())),
        "expected a version from inside the sandbox, got {printed:?}"
    );
    eprintln!("claude inside the sandbox: {printed}");

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

/// Proves init tears its sandbox down when the daemon's connection closes.
///
/// bwrap is started directly here, without `kill_on_drop` and without an
/// `ExecClient`, because both would hide the behaviour under test: dropping a
/// `BwrapHandle` kills bwrap outright, and `ExecClient`'s reader thread holds
/// the client alive for as long as init is. A raw socket that is simply dropped
/// leaves init's own EOF handling as the only thing that can stop the sandbox.
#[tokio::test]
async fn init_tears_down_when_the_exec_socket_closes() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let run_dir = dir.path().join("run");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    let spec = SandboxSpec {
        id: "ws_teardown".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: run_dir.clone(),
        env: vec![],
        cwd: work.clone(),
    };

    let backend = BwrapBackend::default();
    let args = bwrap_args(
        &spec,
        std::path::Path::new("/run/bs/exec.sock"),
        &backend.self_exe,
        &backend.user,
    );
    let mut bwrap = std::process::Command::new(&backend.bwrap_path)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let sock = run_dir.join("exec.sock");
    wait_until(std::time::Duration::from_secs(5), || sock.exists()).await;
    assert!(sock.exists(), "sandbox-init never bound its socket");

    let mut conn = UnixStream::connect(&sock).unwrap();
    conn.write_all(&encode(&InitRequest::Spawn {
        id: 1,
        argv: vec!["/bin/sleep".into(), MARKER.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: Some(work.to_string_lossy().into_owned()),
        pty: None,
    }))
    .unwrap();

    let sleeper = format!("/bin/sleep {MARKER}");
    wait_until(std::time::Duration::from_secs(5), || pgrep(&sleeper)).await;
    assert!(pgrep(&sleeper), "the sandboxed child never started");

    drop(conn);

    let budget = std::time::Duration::from_secs(8);
    wait_until(budget, || !pgrep(&sleeper)).await;
    assert!(!pgrep(&sleeper), "child survived the exec socket closing");
    // bwrap's own command line carries this run directory, so it identifies this
    // sandbox exactly even while other tests run their own in parallel. bwrap
    // exiting means its pid namespace is gone, and with it sandbox-init.
    let marker = run_dir.to_string_lossy().into_owned();
    wait_until(budget, || !pgrep(&marker)).await;
    assert!(!pgrep(&marker), "bwrap survived the exec socket closing");
    wait_until(budget, || {
        bwrap.try_wait().map(|s| s.is_some()).unwrap_or(false)
    })
    .await;
    assert!(
        bwrap.try_wait().unwrap().is_some(),
        "bwrap did not exit after the exec socket closed"
    );
}

/// A `Kill` must reach the whole process group. A shell that backgrounded a
/// child is the case that catches signalling only the leader: kill just the
/// shell and the grandchild keeps running.
#[tokio::test]
async fn killing_a_child_takes_its_process_group_with_it() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_kill".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: dir.path().join("run"),
        env: vec![],
        cwd: work.clone(),
    };
    let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();
    let child = handle
        .spawn(SandboxCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("sleep {GROUP_MARKER} & wait"),
            ],
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();

    let grandchild = format!("sleep {GROUP_MARKER}");
    wait_until(std::time::Duration::from_secs(5), || pgrep(&grandchild)).await;
    assert!(
        pgrep(&grandchild),
        "the backgrounded grandchild never started"
    );

    (child.killer)();

    wait_until(std::time::Duration::from_secs(8), || !pgrep(&grandchild)).await;
    assert!(
        !pgrep(&grandchild),
        "the backgrounded grandchild outlived the kill"
    );
    handle.shutdown().await.unwrap();
}

/// I1: the daemon's environment is the shell the user started it from, and it
/// reaches init through bwrap. Nothing in it may reach a sandboxed process.
#[tokio::test]
async fn a_sandboxed_process_gets_only_the_environment_the_backend_gives_it() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    // Stands in for ANTHROPIC_API_KEY, GH_TOKEN, SSH_AUTH_SOCK and the rest.
    std::env::set_var("BS_TEST_CANARY", "leaked-secret-2477");
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_env".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: dir.path().join("run"),
        env: vec![("BS_WORKSPACE".into(), "ws_env".into())],
        cwd: work.clone(),
    };
    let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();
    let (code, out) = run_argv(&handle, vec!["env".into()]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("BS_TEST_CANARY") && !out.contains("leaked-secret-2477"),
        "the daemon's environment reached the sandbox: {out}"
    );
    // What the backend does pass has to survive the clearing.
    for expected in ["PATH=", "HOME=", "USER=", "BS_WORKSPACE=ws_env"] {
        assert!(
            out.lines().any(|l| l.starts_with(expected)),
            "{expected} missing from the sandbox environment: {out}"
        );
    }
    handle.shutdown().await.unwrap();
}

/// Pids whose command line matches `pattern`.
fn pgrep_pids(pattern: &str) -> Vec<i32> {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// I2: a sandbox that dies has to be reported. Killing bwrap takes its pid
/// namespace, and with it init, so the exec socket closes exactly as it would
/// on an OOM or a crash.
#[tokio::test]
async fn a_workspace_whose_sandbox_dies_is_reported_as_sandbox_down() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "dies".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

    let mut events = server.event_bus().subscribe();
    // bwrap's command line carries this run directory, so it names exactly this
    // sandbox even while other tests run their own in parallel.
    let run_dir = daemon.dirs.run(&ws.id).to_string_lossy().into_owned();
    let pids = pgrep_pids(&run_dir);
    assert!(!pids.is_empty(), "no bwrap process for {run_dir}");
    for pid in pids {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    let saw_event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(msg) = events.recv().await {
            if let bondsymphonic_proto::ServerMessage::Event {
                event: bondsymphonic_proto::Event::WorkspaceStateChanged { info },
                ..
            } = msg
            {
                if info.id == ws.id && info.state == WorkspaceState::SandboxDown {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_event, "no SandboxDown event within 5s");
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::SandboxDown
    );
    assert!(
        daemon.sandbox(&ws.id).is_err(),
        "the dead sandbox must be dropped from the registry of live handles"
    );
}

/// I3: the exec socket is bound in `/run/bs`, which is mounted read-write into
/// the sandbox, so anything running inside can talk to init. It must not be
/// able to shut the sandbox down or signal a pid init did not start.
#[tokio::test]
async fn a_sandboxed_process_cannot_shut_init_down_or_kill_arbitrary_pids() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_hostile".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: dir.path().join("run"),
        env: vec![],
        cwd: work.clone(),
    };
    let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();

    // A process the daemon started, which the hostile requests must not reach.
    let victim = handle
        .spawn(SandboxCommand {
            argv: vec!["/bin/sleep".into(), HOSTILE_MARKER.into()],
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    let sleeper = format!("/bin/sleep {HOSTILE_MARKER}");
    wait_until(std::time::Duration::from_secs(5), || pgrep(&sleeper)).await;
    assert!(pgrep(&sleeper), "the victim never started");

    // `pid: 1` becomes kill(-1, ...) — every process in the namespace — and
    // `pid: 0` signals init's own process group.
    let attack = r#"
import json, socket, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/run/bs/exec.sock")
for m in ({"op": "shutdown"},
          {"op": "kill", "pid": 1, "signal": 15},
          {"op": "kill", "pid": 0, "signal": 9},
          {"op": "kill", "pid": 1, "signal": 9}):
    s.sendall((json.dumps(m) + "\n").encode())
time.sleep(1)
print("sent")
"#;
    let (code, out) = run_argv(&handle, vec!["python3".into(), "-c".into(), attack.into()]).await;
    assert_eq!(code, 0, "the attack script must run: {out}");
    assert!(out.contains("sent"), "{out}");

    // init is still serving, and the daemon's own process was untouched.
    assert!(
        pgrep(&sleeper),
        "a sandboxed process killed the daemon's child"
    );
    let (code, out) = run_in(&handle, "echo still-alive").await;
    assert_eq!(code, 0, "init stopped serving: {out}");
    assert_eq!(out.trim(), "still-alive");

    (victim.killer)();
    wait_until(std::time::Duration::from_secs(8), || !pgrep(&sleeper)).await;
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// C1 round 2: `config.worktree` is repository config, and repository config is
// arbitrary code. It lives in the per-worktree gitdir, which the agent needs
// write access to for `HEAD`, the index and their locks, so the single file is
// bound read-only over the writable directory instead.
// ---------------------------------------------------------------------------

/// Creates a bwrap-backed workspace and returns the daemon, its info and the
/// layout.
async fn bwrap_workspace(
    dir: &std::path::Path,
    repo: &std::path::Path,
    name: &str,
) -> (
    std::sync::Arc<Daemon>,
    bondsymphonic_proto::WorkspaceInfo,
    bondsymphonic_daemon::git::worktree::Layout,
) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: name.into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);
    let layout = lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap())
        .await
        .unwrap();
    (daemon, ws, layout)
}

/// The read-only bind has to hold against every way of replacing a file, while
/// leaving the rest of the gitdir writable — git cannot work otherwise.
#[tokio::test]
async fn a_sandboxed_process_cannot_write_the_worktree_config() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (daemon, ws, layout) = bwrap_workspace(dir.path(), &repo, "roconf").await;
    let handle = daemon.sandbox(&ws.id).unwrap();

    let probe = PROBE_SCRIPT.replace("@GD@", &layout.worktree_gitdir().to_string_lossy());
    let (_, out) = run_in(&handle, &probe).await;
    for expected in [
        "write: refused",
        "unlink: refused",
        "rename-over: refused",
        "sibling: written",
        "index: writable",
    ] {
        assert!(
            out.lines().any(|l| l == expected),
            "expected {expected:?} in:\n{out}"
        );
    }
    assert_eq!(
        std::fs::read(layout.config_worktree()).unwrap(),
        Vec::<u8>::new(),
        "the daemon's empty config.worktree must survive the sandbox"
    );
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

const PROBE_SCRIPT: &str = r#"
gd='@GD@'
if printf x > "$gd/config.worktree" 2>/dev/null; then echo "write: SUCCEEDED"; else echo "write: refused"; fi
if rm -f "$gd/config.worktree" 2>/dev/null; then echo "unlink: SUCCEEDED"; else echo "unlink: refused"; fi
if mv "$gd/gitdir" "$gd/config.worktree" 2>/dev/null; then echo "rename-over: SUCCEEDED"; else echo "rename-over: refused"; fi
if printf x > "$gd/sibling" 2>/dev/null; then echo "sibling: written"; else echo "sibling: REFUSED"; fi
if [ -w "$gd/index" ]; then echo "index: writable"; else echo "index: READ-ONLY"; fi
"#;

/// The attack the read-only bind exists for, run end to end: a sandboxed agent
/// plants a `clean` filter in `config.worktree`, marks every file as using it,
/// and touches a tracked file so the daemon's next `status` has to re-hash it.
/// No key deny-list can stop this one — the driver name is the agent's to pick
/// — so the write itself has to fail.
///
/// Linux only: `noop` has no mounts and therefore no defence here. It is a
/// development and test backend and does not sandbox anything.
#[tokio::test]
async fn a_sandboxed_agent_cannot_make_status_run_a_filter_driver() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // The extension is what makes git read config.worktree at all. Enabling it
    // is an ordinary thing for a repository owner to have done.
    assert!(std::process::Command::new("git")
        .args(["config", "extensions.worktreeConfig", "true"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let (daemon, ws, layout) = bwrap_workspace(dir.path(), &repo, "filter").await;
    let handle = daemon.sandbox(&ws.id).unwrap();
    let marker = dir.path().join("pwned-filter.txt");

    let attack = FILTER_ATTACK
        .replace("@GD@", &layout.worktree_gitdir().to_string_lossy())
        .replace("@WT@", &ws.worktree_path)
        .replace("@MARKER@", &marker.to_string_lossy());
    let (_, out) = run_in(&handle, &attack).await;
    assert!(
        out.lines().any(|l| l == "config: refused"),
        "the agent wrote config.worktree:\n{out}"
    );
    // The rest of the attack must still have worked, or the test would pass for
    // the wrong reason.
    for expected in ["attrs: written", "touched"] {
        assert!(
            out.lines().any(|l| l == expected),
            "expected {expected:?} in:\n{out}"
        );
    }

    lifecycle::status(&daemon, &ws.id).await.unwrap();
    assert!(
        !marker.exists(),
        "workspace.status ran a filter driver the agent planted"
    );
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

const FILTER_ATTACK: &str = r#"
gd='@GD@'; wt='@WT@'
if printf '[filter "evil"]\n\tclean = sh -c "printf pwned > @MARKER@; cat"\n' > "$gd/config.worktree" 2>/dev/null; then echo "config: WRITTEN"; else echo "config: refused"; fi
if printf '* filter=evil\n' > "$wt/.gitattributes" 2>/dev/null; then echo "attrs: written"; else echo "attrs: REFUSED"; fi
if touch "$wt/README.md" 2>/dev/null; then echo "touched"; else echo "TOUCH-REFUSED"; fi
"#;

/// The same signal-number path over the real sandbox: `InitRequest::Kill`
/// carries the number the caller asked for, and init delivers it to the
/// sandboxed process group rather than substituting a termination.
#[tokio::test]
async fn signal_delivers_the_requested_signal_in_the_sandbox() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let spec = SandboxSpec {
        id: "ws_signal".into(),
        rw_binds: vec![(work.clone(), work.clone())],
        ro_binds: vec![],
        late_ro_binds: vec![],
        home: dir.path().join("home"),
        run_dir: dir.path().join("run"),
        env: vec![],
        cwd: work.clone(),
    };
    let handle = backend_for("linux_bwrap").start(&spec).await.unwrap();
    let child = handle
        .spawn(SandboxCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "trap 'exit 42' INT; sleep 30".into(),
            ],
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    // The trap has to be installed before the signal arrives, or the shell dies
    // of the default action instead of running it.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (child.signal)(libc::SIGINT);

    let code = tokio::time::timeout(std::time::Duration::from_secs(10), child.exit)
        .await
        .expect("the interrupted shell reports its exit")
        .expect("exit code is delivered");
    assert_eq!(code, 42, "SIGINT reached the sandboxed shell's trap");
    handle.shutdown().await.unwrap();
}

/// The Claude adapter over the real sandbox rather than the noop backend.
///
/// The fake `claude` is copied into the worktree, which is the one directory
/// bound read-write at its own path, so it is visible from inside; its fixture
/// goes beside it under the name the fake falls back to when
/// `FAKE_CLAUDE_FIXTURE` is not in its environment, which it cannot be here —
/// bwrap gives a sandboxed process only the environment the backend built.
#[tokio::test]
async fn a_claude_agent_streams_a_turn_from_inside_the_sandbox() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let python = std::path::Path::new("/usr/bin/python3");
    if !python.exists() {
        eprintln!("SKIP: /usr/bin/python3 is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let mut events = server.event_bus().subscribe();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "agent".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let worktree = std::path::PathBuf::from(&ws.worktree_path);
    let fake = worktree.join("fake_claude.py");
    std::fs::copy(fixtures.join("fake_claude.py"), &fake).unwrap();
    std::fs::copy(
        fixtures.join("claude-stream/tool_use_turn.ndjson"),
        worktree.join("fixture.ndjson"),
    )
    .unwrap();
    // Process-wide, and deliberately unguarded: this is the only test in this
    // binary that reads or writes `BS_CLAUDE_BIN`, so there is nothing to
    // serialise against -- `the_real_claude_binary_runs_inside_a_bwrap_workspace`
    // deliberately does not consult it. `agent_integration.rs` does take a lock,
    // because every test in that binary points the variable somewhere different.
    //
    // The fake lives *inside the worktree*, which is what makes it reachable:
    // under bwrap the hook has to name something bound into the sandbox, and the
    // worktree is bound read-write.
    std::env::set_var(
        "BS_CLAUDE_BIN",
        format!("/usr/bin/python3 {}", fake.display()),
    );

    let started = daemon
        .agents
        .start(
            &daemon,
            AgentStartParams {
                workspace_id: ws.id.clone(),
                adapter: AgentAdapterKind::Claude,
                options: AgentStartOptions {
                    command: None,
                    resume_session: None,
                    model: None,
                    permission_mode: None,
                    api_key: None,
                },
            },
        )
        .await
        .unwrap();
    let ag = started.agent_id;

    // init, Working, two assistant texts, tool_use, tool_result, result, Idle.
    let mut seen: Vec<Event> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while seen.len() < 8 && tokio::time::Instant::now() < deadline {
        let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            break;
        };
        if let ServerMessage::Event { event, .. } = msg {
            let mine = match &event {
                Event::AgentMessage { agent_id, .. }
                | Event::AgentStateChanged { agent_id, .. } => agent_id == &ag,
                _ => false,
            };
            if mine {
                seen.push(event);
            }
        }
    }
    let bodies: Vec<&AgentMessageBody> = seen
        .iter()
        .filter_map(|e| match e {
            Event::AgentMessage { message, .. } => Some(&message.body),
            _ => None,
        })
        .collect();
    assert!(
        bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::ToolUse { name, .. } if name == "Bash")),
        "the sandboxed agent must stream its tool call: {bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|b| matches!(b, AgentMessageBody::Result { .. })),
        "the turn must finish: {bodies:?}"
    );
    let states: Vec<AgentState> = seen
        .iter()
        .filter_map(|e| match e {
            Event::AgentStateChanged { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
    assert_eq!(states.first(), Some(&AgentState::Working), "{states:?}");
    assert_eq!(states.last(), Some(&AgentState::Idle), "{states:?}");

    assert_eq!(daemon.agents.agents_of(&ws.id), vec![ag]);
    // `destroy` stops the agent before the sandbox goes; the worktree carries
    // the copied fake, so it needs the forced path.
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    assert!(daemon.agents.agents_of(&ws.id).is_empty());
    std::env::remove_var("BS_CLAUDE_BIN");
}

// ---------------------------------------------------------------------------
// The port bridge: a web app started inside a bwrap workspace answers on the
// host, while the host port stays invisible from inside the sandbox.
// ---------------------------------------------------------------------------

/// A port nothing is listening on right now, handed out by the operating
/// system rather than guessed.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One `GET /` over a fresh connection to the host's loopback. `None` when the
/// port refuses it.
async fn http_get(port: u16) -> Option<String> {
    let mut s = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .ok()?;
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Waits for `run` to reach `want`, returning the state events seen on the way.
async fn wait_for_run_state(
    events: &mut tokio::sync::broadcast::Receiver<ServerMessage>,
    run: &bondsymphonic_proto::RunId,
    want: bondsymphonic_proto::RunState,
    limit: std::time::Duration,
) -> Vec<(bondsymphonic_proto::RunState, Option<String>)> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut seen = Vec::new();
    loop {
        let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            return seen;
        };
        if let ServerMessage::Event {
            event:
                Event::RunStateChanged {
                    run_id,
                    state,
                    url,
                    detail,
                },
            ..
        } = msg
        {
            if &run_id != run {
                continue;
            }
            seen.push((state, url.or(detail)));
            if state == want {
                return seen;
            }
        }
    }
}

/// Two configurations that share a port get a forwarder socket each.
///
/// Detection emits `dev`, `start` and `serve` from one `package.json`, all with
/// the same guessed port, so this is the ordinary case. A socket named for the
/// port would have the second run unlink the first one's live socket, leaving
/// run 1's bridge pointing at run 2's forwarder and either stop killing both.
#[tokio::test]
async fn two_bwrap_runs_on_one_port_get_a_socket_and_a_bridge_each() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    if !std::path::Path::new("/usr/bin/python3").exists() {
        eprintln!("SKIP: /usr/bin/python3 is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // One port, two configurations. Only one of them can actually bind it
    // inside the sandbox; the other is ready by its output, which is exactly
    // how a repository with three aliases for one dev server behaves.
    let port = free_port();
    std::fs::write(
        repo.join("hold.py"),
        "import sys, time\nprint('holding', flush=True)\ntime.sleep(60)\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("bondsymphonic.toml"),
        format!(
            "[[run]]\nname = \"dev\"\ncommand = \"python3 hold.py\"\nport = {port}\nready_regex = \"holding\"\n\n[[run]]\nname = \"serve\"\ncommand = \"python3 hold.py\"\nport = {port}\nready_regex = \"holding\"\n"
        ),
    )
    .unwrap();
    common::commit_all(&repo, &[], "config");

    let (daemon, ws, _layout) = bwrap_workspace(dir.path(), &repo, "twoports").await;
    let mut events = daemon.events.subscribe();
    let mut started = Vec::new();
    for name in ["dev", "serve"] {
        started.push(
            daemon
                .runs
                .start(
                    &daemon,
                    bondsymphonic_proto::RunStartParams {
                        workspace_id: ws.id.clone(),
                        config_name: name.into(),
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("{name} did not start: {e}")),
        );
    }
    assert_ne!(started[0].run_id, started[1].run_id);
    assert_ne!(
        started[0].host_port, started[1].host_port,
        "each run is reached on a host port of its own"
    );

    let sockets: Vec<std::path::PathBuf> = started
        .iter()
        .map(|r| {
            daemon
                .dirs
                .run(&ws.id)
                .join(format!("fwd-{}.sock", r.run_id))
        })
        .collect();
    assert_ne!(
        sockets[0], sockets[1],
        "the two forwarders must not share a socket path"
    );
    for socket in &sockets {
        assert!(socket.exists(), "{} was never bound", socket.display());
    }

    // Both runs off one pass over the stream. `wait_for_run_state` drops every
    // message that is not the run it was asked about, so waiting for these one
    // after the other would throw away whichever `ready` landed first.
    let wanted: Vec<bondsymphonic_proto::RunId> =
        started.iter().map(|r| r.run_id.clone()).collect();
    let mut ready: Vec<bondsymphonic_proto::RunId> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while ready.len() < wanted.len() {
        let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            break;
        };
        if let ServerMessage::Event {
            event:
                Event::RunStateChanged {
                    run_id,
                    state: bondsymphonic_proto::RunState::Ready,
                    ..
                },
            ..
        } = msg
        {
            if wanted.contains(&run_id) && !ready.contains(&run_id) {
                ready.push(run_id);
            }
        }
    }
    for run in &wanted {
        assert!(ready.contains(run), "{run} never became ready: {ready:?}");
    }

    // Stopping the first leaves the second's socket, forwarder and bridge
    // exactly as they were.
    daemon.runs.stop(&started[0].run_id).await.unwrap();
    wait_until(std::time::Duration::from_secs(5), || !sockets[0].exists()).await;
    assert!(
        !sockets[0].exists(),
        "the stopped run must take its own socket"
    );
    assert!(
        sockets[1].exists(),
        "and must leave the other run's socket alone"
    );
    let survivor = format!("fwd-{}[.]sock", started[1].run_id);
    assert!(
        pgrep(&survivor),
        "the other forwarder must still be running"
    );
    let live = daemon.runs.list(&ws.id);
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0].run_id, started[1].run_id);
    assert_eq!(live[0].state, bondsymphonic_proto::RunState::Ready);

    daemon.runs.stop(&started[1].run_id).await.unwrap();
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}

#[tokio::test]
async fn a_bwrap_run_answers_on_the_host_through_its_bridge() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    if !std::path::Path::new("/usr/bin/python3").exists() {
        eprintln!("SKIP: /usr/bin/python3 is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let port = free_port();
    std::fs::write(
        repo.join("bondsymphonic.toml"),
        format!(
            "[[run]]\nname = \"web\"\ncommand = \"python3 -m http.server {port} --bind 127.0.0.1\"\nport = {port}\n"
        ),
    )
    .unwrap();
    common::commit_all(&repo, &[], "config");

    let (daemon, ws, _layout) = bwrap_workspace(dir.path(), &repo, "bridged").await;
    let mut events = daemon.events.subscribe();
    let started = daemon
        .runs
        .start(
            &daemon,
            bondsymphonic_proto::RunStartParams {
                workspace_id: ws.id.clone(),
                config_name: "web".into(),
            },
        )
        .await
        .unwrap();
    assert_ne!(
        started.host_port, port,
        "a sandboxed run is reached through a host port of its own"
    );
    assert_eq!(
        started.url,
        format!("http://localhost:{}", started.host_port)
    );
    let socket = daemon
        .dirs
        .run(&ws.id)
        .join(format!("fwd-{}.sock", started.run_id));

    let seen = wait_for_run_state(
        &mut events,
        &started.run_id,
        bondsymphonic_proto::RunState::Ready,
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        seen.last().map(|(s, _)| *s),
        Some(bondsymphonic_proto::RunState::Ready),
        "readiness must come from the bridge probe: {seen:?}"
    );
    assert!(
        socket.exists(),
        "the forwarder must bind {}",
        socket.display()
    );
    // The socket path carries this run's own id, so it names exactly one
    // process on the machine however many sandboxes other suites are running.
    let forwarder = format!("fwd-{}[.]sock", started.run_id);
    assert!(pgrep(&forwarder), "the forwarder must be running");

    // Through the bridge, from the host, exactly as the Windows browser does.
    let body = http_get(started.host_port)
        .await
        .unwrap_or_else(|| panic!("no answer on the bridged port {}", started.host_port));
    assert!(body.contains("200"), "{body}");

    // The host port is not reachable from inside the sandbox: the bridge runs
    // on the host, and the sandbox has a network namespace of its own.
    let handle: Handle = daemon.sandbox(&ws.id).unwrap();
    let (code, _) = run_bash(
        &handle,
        &format!("exec 3<>/dev/tcp/127.0.0.1/{}", started.host_port),
    )
    .await;
    assert_ne!(code, 0, "the sandbox must not see the host's bridge port");

    daemon.runs.stop(&started.run_id).await.unwrap();
    assert!(
        http_get(started.host_port).await.is_none(),
        "the bridge must be gone with the run"
    );
    wait_until(std::time::Duration::from_secs(5), || !pgrep(&forwarder)).await;
    assert!(
        !pgrep(&forwarder),
        "the forwarder must die with the run it served"
    );
    assert!(
        !socket.exists(),
        "the forwarder socket must be removed with the run"
    );
    assert!(daemon.runs.list(&ws.id).is_empty());

    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
}
