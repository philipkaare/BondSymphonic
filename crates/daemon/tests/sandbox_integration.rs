#![cfg(target_os = "linux")]
//! Proves the bubblewrap backend really isolates a workspace. Skipped when
//! `bwrap` cannot create user namespaces (see `scripts/setup-wsl.sh`).

use bondsymphonic_daemon::sandbox::{backend_for, PtySize, SandboxCommand, SandboxSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

async fn run_in(
    handle: &std::sync::Arc<dyn bondsymphonic_daemon::sandbox::SandboxHandle>,
    script: &str,
) -> (i32, String) {
    let mut child = handle
        .spawn(SandboxCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
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
    let (code, _) = run_in(
        &handle,
        "cat /etc/hostname >/dev/null; (exec 3<>/dev/tcp/1.1.1.1/80) 2>/dev/null",
    )
    .await;
    assert_ne!(code, 0, "network must be unreachable");

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
