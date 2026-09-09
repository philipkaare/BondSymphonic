//! `system.setup_pty` over the wire.
//!
//! A setup terminal runs one of four fixed commands on the host, outside every
//! sandbox, because logging in to Claude or GitHub has to write to the daemon
//! user's real home and reach the network. The events it publishes carry no
//! workspace id, and the returned id is an ordinary PTY id: `pty.write`,
//! `pty.resize` and `pty.close` work on it exactly as they do on a workspace
//! terminal.

mod common;

// Only the Unix tests decode terminal output; on Windows the suite gets no
// further than the spawn.
#[cfg(unix)]
use base64::Engine;
use bondsymphonic_proto::*;

#[cfg(unix)]
fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

/// The device-code line the stub `gh` prints, standing in for the one a real
/// `gh auth login` shows.
#[cfg(unix)]
const DEVICE_CODE_LINE: &str = "Open https://github.com/login/device and enter code ABCD-1234";

/// Gives the daemon a throwaway home whose `.local/bin` holds a stub `gh`, and
/// returns it.
///
/// A real login must never run from this suite: `gh auth login` would open a
/// device-code flow against GitHub and `claude auth login` would write
/// credentials into the developer's home. Shadowing the real programs by
/// prepending a directory to `PATH` is *not* enough to guarantee that, because
/// the code under test prepends `$HOME/.local/bin` ahead of whatever it
/// inherited — which is exactly where `claude` installs itself, so the real one
/// would win. Moving `HOME` moves that prefix, so the stub is first by
/// construction and no real program can be reached under either name.
///
/// It also means the assertions cover the `PATH` prefix itself: the stub is
/// only ever found because `open_host` put `$HOME/.local/bin` in front.
///
/// `HOME` is process-global and the daemon under test runs in this process, so
/// it is set once; the temp directory is parked in the `OnceLock` so it lives as
/// long as the process. Nothing else in this binary spawns a program or reads a
/// home directory.
#[cfg(unix)]
fn fake_home() -> std::path::PathBuf {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Prints a device code, then stays at a prompt: enough for one test to
        // watch it exit on command and another to type into it and close it.
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\n\
                 echo '{DEVICE_CODE_LINE}'\n\
                 while IFS= read -r line; do\n\
                   if [ \"$line\" = quit ]; then exit 0; fi\n\
                   echo \"got $line\"\n\
                 done\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("HOME", dir.path());
        dir
    })
    .path()
    .to_path_buf()
}

#[cfg(unix)]
async fn open_setup(
    c: &mut common::Client,
    action: SetupAction,
) -> Result<PtyId, bondsymphonic_proto::RpcError> {
    c.call(Request::SystemSetupPty(SetupPtyParams {
        action,
        cols: 80,
        rows: 24,
    }))
    .await
    .map(|v| serde_json::from_value::<PtyOpenResult>(v).unwrap().pty_id)
}

/// Drives one harmless request so the connection's queued events are delivered,
/// appends this terminal's output to `out` and returns its exit code once the
/// `pty.exit` event has arrived. Every event for the terminal is checked to
/// carry no workspace id: a host terminal belongs to no workspace.
#[cfg(unix)]
async fn pump(c: &mut common::Client, out: &mut String, pty: &PtyId) -> Option<i32> {
    let mut events = c.drain_events();
    let id = c.send(Request::WorkspaceList {}).await;
    c.recv_response(id, &mut events).await.unwrap();
    let mut exit = None;
    for (ws, e) in events {
        match e {
            Event::PtyOutput { pty_id, data_b64 } if &pty_id == pty => {
                assert_eq!(ws, None, "a host pty.output carries no workspace id");
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap();
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            Event::PtyExit { pty_id, code } if &pty_id == pty => {
                assert_eq!(ws, None, "a host pty.exit carries no workspace id");
                exit = Some(code);
            }
            _ => {}
        }
    }
    exit
}

/// Pumps until `out` satisfies `done` or `limit` elapses, returning the exit
/// code if one arrived on the way.
#[cfg(unix)]
async fn pump_until(
    c: &mut common::Client,
    out: &mut String,
    pty: &PtyId,
    limit: std::time::Duration,
    done: impl Fn(&str, Option<i32>) -> bool,
) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut exit = None;
    loop {
        exit = pump(c, out, pty).await.or(exit);
        if done(out, exit) || tokio::time::Instant::now() >= deadline {
            return exit;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The headline case: `gh auth login` runs on the host and the device-code line
/// it prints reaches the client as `pty.output` with no workspace id. Typing
/// into the terminal ends it, and the session publishes one clean `pty.exit`.
#[cfg(unix)]
#[tokio::test]
async fn gh_login_streams_its_device_code_from_a_host_terminal() {
    let _home = fake_home();
    let dir = tempfile::tempdir().unwrap();
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;

    let pty = open_setup(&mut c, SetupAction::GhLogin).await.unwrap();
    let mut out = String::new();
    pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains(DEVICE_CODE_LINE),
    )
    .await;
    assert!(
        out.contains("https://github.com/login/device"),
        "collected: {out}"
    );
    assert!(out.contains("ABCD-1234"), "collected: {out}");

    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: pty.clone(),
        data_b64: b64("quit\r"),
    }))
    .await
    .unwrap();
    let exit = pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |_, exit| exit.is_some(),
    )
    .await;
    assert_eq!(exit, Some(0), "collected: {out}");
    cancel.cancel();
}

/// The returned id is an ordinary PTY id, so the three session methods work on
/// it and it is forgotten once the session has ended.
#[cfg(unix)]
#[tokio::test]
async fn a_setup_terminal_takes_writes_resizes_and_closes() {
    let _home = fake_home();
    let dir = tempfile::tempdir().unwrap();
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;

    let pty = open_setup(&mut c, SetupAction::GhLogin).await.unwrap();
    let mut out = String::new();
    pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains(DEVICE_CODE_LINE),
    )
    .await;
    assert!(out.contains(DEVICE_CODE_LINE), "collected: {out}");

    c.call(Request::PtyResize(PtyResizeParams {
        pty_id: pty.clone(),
        cols: 120,
        rows: 40,
    }))
    .await
    .unwrap();
    // A carriage return is what a terminal sends for Enter; the line discipline
    // turns it into the newline the stub's `read` is waiting for.
    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: pty.clone(),
        data_b64: b64("hello\r"),
    }))
    .await
    .unwrap();
    pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains("got hello"),
    )
    .await;
    assert!(out.contains("got hello"), "collected: {out}");

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: pty.clone(),
    }))
    .await
    .unwrap();
    let exit = pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |_, exit| exit.is_some(),
    )
    .await;
    assert!(exit.is_some(), "pty.exit expected after close: {out}");

    let err = c
        .call(Request::PtyWrite(PtyWriteParams {
            pty_id: pty.clone(),
            data_b64: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    cancel.cancel();
}

/// A setup terminal runs a command on the host with no sandbox around it, so it
/// must be behind the same authentication as everything else.
#[tokio::test]
async fn setup_pty_before_hello_is_unauthorized() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let (port, _token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (r, mut w) = s.into_split();
    let mut r = tokio::io::BufReader::new(r);
    w.write_all(
        codec::encode(&ClientMessage::Request {
            id: 1,
            request: Request::SystemSetupPty(SetupPtyParams {
                action: SetupAction::InstallGh,
                cols: 80,
                rows: 24,
            }),
        })
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut line = String::new();
    assert!(r.read_line(&mut line).await.unwrap() > 0);
    match codec::decode::<ServerMessage>(line.trim_end()).unwrap() {
        ServerMessage::Response {
            id: 1,
            error: Some(e),
            ..
        } => assert_eq!(e.code, ErrorCode::Unauthorized),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}

/// The setup commands are for the Linux distro the daemon really runs in; on
/// Windows the daemon exists only for this test suite, and whether any of the
/// four programs resolves depends on the machine — `apt-get` never does, while
/// Windows 11 does ship a `sudo`. So the assertion is the one thing that must
/// hold either way: the request is answered rather than panicking or hanging,
/// and whatever comes back leaves the connection and the daemon usable.
#[cfg(windows)]
#[tokio::test]
async fn on_windows_a_setup_terminal_is_answered_cleanly_either_way() {
    let dir = tempfile::tempdir().unwrap();
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;

    let res = c
        .call(Request::SystemSetupPty(SetupPtyParams {
            action: SetupAction::InstallGh,
            cols: 80,
            rows: 24,
        }))
        .await;
    match res {
        // The program did not resolve: an error the client can show, not a panic.
        Err(err) => assert!(
            matches!(
                err.code,
                ErrorCode::SandboxError | ErrorCode::InvalidParams | ErrorCode::Internal
            ),
            "unexpected error: {err:?}"
        ),
        // It did resolve and will fail on its own terms; the session it opened
        // is a real one, so `pty.close` takes it, and nothing is left running.
        Ok(v) => {
            let opened: PtyOpenResult = serde_json::from_value(v).unwrap();
            c.call(Request::PtyClose(PtyIdParams {
                pty_id: opened.pty_id,
            }))
            .await
            .unwrap();
        }
    }
    // The connection is still usable, so neither outcome took the daemon or the
    // connection loop down with it.
    c.call(Request::WorkspaceList {}).await.unwrap();
    cancel.cancel();
}
