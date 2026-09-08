#![cfg(target_os = "linux")]
//! Proves the bubblewrap backend really isolates a workspace. Skipped when
//! `bwrap` cannot create user namespaces (see `scripts/setup-wsl.sh`).

use bondsymphonic_daemon::sandbox::linux_bwrap::{bwrap_args, BwrapBackend};
use bondsymphonic_daemon::sandbox::protocol::{encode, InitRequest};
use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use std::io::Write;
use std::os::unix::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Sleep duration used as a process marker: distinctive enough that `pgrep`
/// cannot confuse this test's child with anything else on the machine.
const MARKER: &str = "2477";

/// Marker for the grandchild in the process-group kill test.
const GROUP_MARKER: &str = "2478";

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
    for host_if in host_addressed_interfaces() {
        assert!(
            !interfaces.contains(&host_if.as_str()),
            "host interface {host_if} must not exist inside the sandbox: {out}"
        );
    }
    // And nothing inside may carry an address except loopback.
    let (code, out) = run_in(&handle, "ip -o addr").await;
    assert_eq!(code, 0, "ip -o addr failed: {out}");
    let addressed: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert!(!addressed.is_empty(), "expected loopback addresses: {out}");
    assert!(
        addressed.iter().all(|n| *n == "lo"),
        "only loopback may be addressed inside the sandbox: {out}"
    );

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
