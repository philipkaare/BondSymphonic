use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn spec(dir: &std::path::Path) -> SandboxSpec {
    SandboxSpec {
        id: "ws_test".into(),
        rw_binds: vec![],
        ro_binds: vec![],
        home: dir.join("home"),
        run_dir: dir.join("run"),
        env: vec![("BS_TEST_VAR".into(), "from-spec".into())],
        cwd: dir.to_path_buf(),
    }
}

#[tokio::test]
async fn noop_spawns_with_pipes_env_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let argv = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "echo %BS_TEST_VAR%".into()]
    } else {
        vec!["sh".into(), "-c".into(), "echo $BS_TEST_VAR".into()]
    };
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
    assert_eq!(out.trim(), "from-spec");
    assert_eq!(child.exit.await.unwrap(), 0);

    let argv = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), "exit 3".into()]
    } else {
        vec!["sh".into(), "-c".into(), "exit 3".into()]
    };
    let child = handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    assert_eq!(child.exit.await.unwrap(), 3);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn noop_pty_echoes_input() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let argv = if cfg!(windows) {
        vec!["cmd".into()]
    } else {
        vec!["sh".into()]
    };
    let mut child = handle
        .spawn(SandboxCommand {
            argv,
            env: vec![],
            cwd: None,
            pty: Some(PtySize { cols: 80, rows: 24 }),
        })
        .await
        .unwrap();
    let mut pty = child.pty.take().unwrap();
    pty.writer
        .write_all(b"echo bs-pty-marker\r\n")
        .await
        .unwrap();
    (pty.resizer)(PtySize {
        cols: 100,
        rows: 30,
    })
    .unwrap();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut chunk = [0u8; 1024];
        let n = tokio::time::timeout_at(deadline, pty.reader.read(&mut chunk))
            .await
            .expect("pty output")
            .unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if String::from_utf8_lossy(&buf)
            .matches("bs-pty-marker")
            .count()
            >= 2
        {
            break; // echo of input + output
        }
    }
    assert!(String::from_utf8_lossy(&buf).contains("bs-pty-marker"));
    pty.writer.write_all(b"exit\r\n").await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.exit)
        .await
        .expect("shell exits");
    handle.shutdown().await.unwrap();
}

/// A long-running child, used to prove the kill paths actually reach it.
///
/// The Unix form deliberately runs two sleeps, one backgrounded, so the shell
/// cannot `exec` itself into a single command: killing only the shell's pid
/// would strand the backgrounded `sleep`, whereas signalling the process group
/// takes down all three. On Windows `ping` is spawned directly rather than under
/// `cmd`, so the direct child is the sleeper itself and nothing is orphaned.
fn sleeper() -> Vec<String> {
    if cfg!(windows) {
        vec!["ping".into(), "-n".into(), "31".into(), "127.0.0.1".into()]
    } else {
        vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()]
    }
}

fn sleeping_command() -> SandboxCommand {
    SandboxCommand {
        argv: sleeper(),
        env: vec![],
        cwd: None,
        pty: None,
    }
}

#[tokio::test]
async fn killer_terminates_a_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let child = handle.spawn(sleeping_command()).await.unwrap();

    (child.killer)();
    tokio::time::timeout(std::time::Duration::from_secs(10), child.exit)
        .await
        .expect("killed child reports its exit")
        .expect("exit code is delivered");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_terminates_every_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_for("noop");
    let handle = backend.start(&spec(dir.path())).await.unwrap();
    let first = handle.spawn(sleeping_command()).await.unwrap();
    let second = handle.spawn(sleeping_command()).await.unwrap();

    handle.shutdown().await.unwrap();
    for (n, child) in [first, second].into_iter().enumerate() {
        tokio::time::timeout(std::time::Duration::from_secs(10), child.exit)
            .await
            .unwrap_or_else(|_| panic!("child {n} was not terminated by shutdown"))
            .expect("exit code is delivered");
    }
}
