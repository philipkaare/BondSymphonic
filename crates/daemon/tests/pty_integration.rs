//! `pty.*` over the wire: open a terminal in a workspace sandbox, type into it,
//! resize it, and see `pty.output` / `pty.exit` events arrive on the connection.

mod common;

use base64::Engine;
use bondsymphonic_proto::*;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

/// Drives one harmless request so the connection's queued events are delivered,
/// and returns them together with anything `call` buffered earlier.
#[cfg(unix)]
async fn poll_events(
    c: &mut common::Client,
    ws: &WorkspaceId,
) -> Vec<(Option<WorkspaceId>, Event)> {
    let mut events = c.drain_events();
    let id = c
        .send(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.clone(),
        }))
        .await;
    c.recv_response(id, &mut events).await.unwrap();
    events
}

#[cfg(unix)]
async fn create_workspace(
    c: &mut common::Client,
    repo: &std::path::Path,
    name: &str,
) -> WorkspaceInfo {
    serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: name.into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap()
}

#[cfg(unix)]
async fn open_pty(c: &mut common::Client, ws: &WorkspaceId, command: Option<String>) -> PtyId {
    let opened: PtyOpenResult = serde_json::from_value(
        c.call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.clone(),
            cols: 80,
            rows: 24,
            command,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    opened.pty_id
}

/// Polls until `pty.exit` for `pty` arrives, or `limit` elapses.
#[cfg(unix)]
async fn wait_for_exit(
    c: &mut common::Client,
    ws: &WorkspaceId,
    pty: &PtyId,
    limit: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        for (_, e) in poll_events(c, ws).await {
            if matches!(&e, Event::PtyExit { pty_id, .. } if pty_id == pty) {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn pty_open_echo_resize_close_over_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "p".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    let command = if cfg!(windows) {
        Some("cmd".to_string())
    } else {
        Some("sh".to_string())
    };
    let opened: PtyOpenResult = serde_json::from_value(
        c.call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let marker = "bs-marker-4242";
    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: opened.pty_id.clone(),
        data_b64: b64(&format!("echo {marker}\r\n")),
    }))
    .await
    .unwrap();
    c.call(Request::PtyResize(PtyResizeParams {
        pty_id: opened.pty_id.clone(),
        cols: 120,
        rows: 40,
    }))
    .await
    .unwrap();

    // The marker shows up twice: once as the terminal's echo of the typed line,
    // once as the shell's output. Waiting for both proves the shell really ran
    // the command rather than only echoing it back.
    //
    // A fast shell can produce both before the `pty.resize` reply above, so the
    // events those calls buffered are drained here as well; harvesting only the
    // ones that ride along with the poll below would lose them.
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && collected.matches(marker).count() < 2 {
        let mut events = c.drain_events();
        // Any request will do; the events queued for this connection ride along.
        let id = c
            .send(Request::WorkspaceGet(WorkspaceIdParams {
                workspace_id: ws.id.clone(),
            }))
            .await;
        c.recv_response(id, &mut events).await.unwrap();
        for (_, e) in events {
            if let Event::PtyOutput { data_b64, .. } = e {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap();
                collected.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(collected.contains(marker), "collected: {collected}");

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: opened.pty_id.clone(),
    }))
    .await
    .unwrap();
    let mut saw_exit = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_exit {
        // The exit can beat the `pty.close` reply, so its call's events count too.
        let mut events = c.drain_events();
        let id = c.send(Request::WorkspaceList {}).await;
        c.recv_response(id, &mut events).await.unwrap();
        saw_exit = events
            .iter()
            .any(|(_, e)| matches!(e, Event::PtyExit { pty_id, .. } if *pty_id == opened.pty_id));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saw_exit, "pty.exit event expected after close");

    // The session is gone once it has exited, so later calls fault cleanly.
    let err = c
        .call(Request::PtyWrite(PtyWriteParams {
            pty_id: opened.pty_id.clone(),
            data_b64: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

/// An unparseable `command` is the caller's mistake, not a sandbox failure.
#[tokio::test]
async fn pty_open_rejects_an_unbalanced_command() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "q".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let err = c
        .call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command: Some("echo \"unterminated".into()),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    cancel.cancel();
}

/// Destroying a workspace has to take its terminals down with it.
#[tokio::test]
async fn destroying_a_workspace_closes_its_ptys() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "r".into(),
            init_if_missing: false,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let command = if cfg!(windows) {
        Some("cmd".to_string())
    } else {
        Some("sh".to_string())
    };
    let opened: PtyOpenResult = serde_json::from_value(
        c.call(Request::PtyOpen(PtyOpenParams {
            workspace_id: ws.id.clone(),
            cols: 80,
            rows: 24,
            command,
        }))
        .await
        .unwrap(),
    )
    .unwrap();

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut gone = false;
    while tokio::time::Instant::now() < deadline && !gone {
        gone = matches!(
            c.call(Request::PtyWrite(PtyWriteParams {
                pty_id: opened.pty_id.clone(),
                data_b64: String::new(),
            }))
            .await,
            Err(e) if e.code == ErrorCode::NotFound
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(gone, "the workspace's pty should be closed and forgotten");
    cancel.cancel();
}

/// A grandchild that keeps the terminal open must not hold the pump open with it.
///
/// `sh` exits at once while the backgrounded loop inherits the terminal and
/// keeps printing, so the pump has the exit code in hand but never sees end of
/// file and never sees a quiet gap either. Draining has to stop on an absolute
/// budget, not on a per-chunk timeout that every tick restarts.
#[cfg(unix)]
#[tokio::test]
async fn a_chattering_grandchild_does_not_hold_the_exit_event() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = create_workspace(&mut c, &repo, "chatty").await;

    // Ten seconds of ticks at 100 ms, comfortably longer than the assertion
    // window, so the loop cannot end the test by finishing on its own. SIGHUP
    // is ignored first: the kernel sends it to the foreground process group
    // when the session leader exits, and an ignored disposition is inherited,
    // so the loop outlives its parent instead of dying with it.
    let command = r#"sh -c "trap '' HUP; (n=0; while [ $n -lt 100 ]; do echo tick; sleep 0.1; n=$((n+1)); done) & exit 0""#;
    let pty = open_pty(&mut c, &ws.id, Some(command.to_string())).await;

    let saw_exit = wait_for_exit(&mut c, &ws.id, &pty, std::time::Duration::from_secs(4)).await;
    assert!(
        saw_exit,
        "pty.exit must arrive on the drain budget even while output keeps coming"
    );
    cancel.cancel();
}

/// `pty.close` has to end a shell that ignores SIGTERM.
///
/// The sandbox backend's killer sends SIGTERM to the sandboxed process group,
/// and an interactive `bash -l` (the default command) ignores it. Without an
/// escalation path the session would never publish `pty.exit` and would leak.
/// This runs against the real sandbox on purpose: the noop backend's killer is
/// a hard kill, so the same test would pass there for the wrong reason.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn closing_a_sandboxed_login_shell_publishes_an_exit() {
    if !bwrap_available() {
        eprintln!("skipping: bwrap cannot create user namespaces here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, cancel) = start_bwrap_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = create_workspace(&mut c, &repo, "shell").await;

    // No command, so the default login shell runs inside the sandbox.
    let pty = open_pty(&mut c, &ws.id, None).await;

    // Wait for the shell to reach a prompt; killing it before it has installed
    // its signal handlers would not exercise the case under test.
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !collected.contains('$') {
        for (_, e) in poll_events(&mut c, &ws.id).await {
            if let Event::PtyOutput { data_b64, .. } = e {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap();
                collected.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        collected.contains('$'),
        "the login shell never reached a prompt: {collected}"
    );

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: pty.clone(),
    }))
    .await
    .unwrap();
    let saw_exit = wait_for_exit(&mut c, &ws.id, &pty, std::time::Duration::from_secs(8)).await;

    // Tear the workspace down either way, so a failure does not leave a sandbox
    // and a live shell behind.
    let destroyed = c
        .call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: true,
        }))
        .await;
    cancel.cancel();
    assert!(saw_exit, "pty.close must end a shell that ignores SIGTERM");
    destroyed.unwrap();
}

#[cfg(target_os = "linux")]
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

/// A daemon on an ephemeral port backed by the real bubblewrap sandbox.
/// `common::start_daemon` is wired to the noop backend, whose killer is a hard
/// kill, so it cannot show the SIGTERM-ignoring shell this test is about.
#[cfg(target_os = "linux")]
async fn start_bwrap_daemon(
    root: &std::path::Path,
) -> (u16, String, tokio_util::sync::CancellationToken) {
    use bondsymphonic_daemon::daemon::Daemon;
    use bondsymphonic_daemon::sandbox::backend_for;
    use bondsymphonic_daemon::server::dispatch::SystemHandler;
    use bondsymphonic_daemon::server::handlers::WorkspaceHandler;
    use bondsymphonic_daemon::server::{Server, ServerConfig};
    use bondsymphonic_daemon::workspace::DataDirs;

    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(root),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let system = SystemHandler {
        token: server.token().to_string(),
        capabilities: ServerConfig::default().capabilities,
    };
    let handler = std::sync::Arc::new(WorkspaceHandler { system, daemon });
    let (port, token) = (server.port(), server.token().to_string());
    let cancel = tokio_util::sync::CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.with_handler(handler).run(c2).await.unwrap() });
    (port, token, cancel)
}

/// Closing a workspace's terminals has to end a command that ignores the
/// backend's polite signal, the way `pty.close` does.
///
/// `close_workspace` runs from `workspace.destroy`, and on the no-sandbox
/// backend the killer it fires is a bare SIGHUP. A command that traps SIGHUP
/// survived it, kept its session registered and its process running, and the
/// destroy went on to delete the worktree from under it. `pty.close` already
/// escalates past that; the workspace path has to take the same ladder.
#[cfg(unix)]
#[tokio::test]
async fn closing_a_workspace_ends_a_terminal_that_ignores_hangups() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = create_workspace(&mut c, &repo, "hup").await;

    let pty = open_pty(
        &mut c,
        &ws.id,
        Some(r#"sh -c "trap '' HUP; sleep 60""#.to_string()),
    )
    .await;
    // Let the shell install its trap before anything is sent to it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    daemon.ptys.close_workspace(&ws.id).await;

    // The session retires as soon as the pump sees the child exit, so a write
    // that faults is proof the process is really gone rather than merely asked.
    //
    // Twice the ladder, not the length of it: `close_workspace` returns before
    // the ladder has even started, and the ladder is a killer this command
    // ignores, one `SIGNAL_GRACE`, a SIGTERM it also ignores, another, and then
    // SIGKILL. A deadline set to what that costs is one a loaded machine fails
    // for no reason at all.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut gone = false;
    while !gone && tokio::time::Instant::now() < deadline {
        gone = matches!(
            c.call(Request::PtyWrite(PtyWriteParams {
                pty_id: pty.clone(),
                data_b64: String::new(),
            }))
            .await,
            Err(e) if e.code == ErrorCode::NotFound
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        gone,
        "close_workspace must escalate past a signal the command ignores"
    );
    cancel.cancel();
}
