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

/// What the stub prints once it has stopped listening to SIGHUP and SIGTERM.
#[cfg(unix)]
const DEAF_LINE: &str = "ignoring hangups now";

/// Exit code of the tripwire programs on `PATH`. Distinctive enough that it
/// cannot be confused with a real failure. See [`fake_home`].
#[cfg(unix)]
const TRIPWIRE_EXIT: i32 = 97;

/// The two temp directories `isolate()` builds, parked for the life of the
/// process so the environment it points at stays on disk.
#[cfg(unix)]
struct Isolation {
    home: tempfile::TempDir,
    #[allow(dead_code, reason = "held only to keep the tripwire directory alive")]
    tripwire: tempfile::TempDir,
}

/// Puts the whole test binary somewhere a real login cannot be reached, and
/// returns the throwaway home.
///
/// A real login must never run from this suite: `gh auth login` opens a
/// device-code flow against GitHub and `claude auth login` writes credentials
/// into the developer's home. Two independent things keep that from happening,
/// because either one alone has failed before.
///
/// **`HOME` is moved.** The code under test prepends `$HOME/.local/bin` ahead of
/// whatever `PATH` it inherited — which is exactly where the real `claude`
/// lives, so merely prepending a stub directory to `PATH` loses to it. Moving
/// `HOME` moves that prefix, so the stub `gh` in *this* home's `.local/bin` is
/// first by construction. It also means the assertions cover the prefix itself:
/// the device-code line can only come from a stub that the prefix found.
///
/// **`PATH` is replaced with a tripwire.** If that prefix ever stops being
/// prepended, the search falls through to `PATH` — so `PATH` no longer contains
/// the real `gh` or `claude` at all. It holds a directory whose `gh` and
/// `claude` announce themselves and exit [`TRIPWIRE_EXIT`], followed by the
/// system directories the stubs' own `sleep` needs. A regression then fails
/// loudly on an exit code instead of quietly contacting GitHub or Anthropic.
///
/// Both variables are process-global and the daemon under test runs in this
/// process, so they are written once, from a `OnceLock` that every test in the
/// file enters before it starts a daemon.
#[cfg(unix)]
fn fake_home() -> std::path::PathBuf {
    static ISOLATION: std::sync::OnceLock<Isolation> = std::sync::OnceLock::new();
    ISOLATION
        .get_or_init(|| {
            let home = tempfile::tempdir().unwrap();
            let tripwire = tempfile::tempdir().unwrap();

            let bin = home.path().join(".local").join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            // Prints a device code and then stays at a prompt, taking three
            // words: `quit` to leave, `deaf` to stop listening to signals, and
            // anything else to be echoed back.
            write_program(
                &bin.join("gh"),
                &format!(
                    "#!/bin/sh\n\
                     echo '{DEVICE_CODE_LINE}'\n\
                     while IFS= read -r line; do\n\
                       case \"$line\" in\n\
                         quit) exit 0 ;;\n\
                         deaf) trap '' HUP TERM; echo '{DEAF_LINE}'\n\
                               while true; do sleep 1; done ;;\n\
                         *) echo \"got $line\" ;;\n\
                       esac\n\
                     done\n\
                     exit 0\n"
                ),
            );

            for name in ["gh", "claude"] {
                write_program(
                    &tripwire.path().join(name),
                    &format!(
                        "#!/bin/sh\n\
                         echo 'TRIPWIRE: the real {name} was reached from a test'\n\
                         exit {TRIPWIRE_EXIT}\n"
                    ),
                );
            }

            std::env::set_var("HOME", home.path());
            // The system directories stay on the path because the stubs are
            // shell scripts that call `sleep`; nothing that a setup action names
            // resolves through them any more.
            std::env::set_var(
                "PATH",
                format!("{}:/usr/bin:/bin", tripwire.path().display()),
            );
            Isolation { home, tripwire }
        })
        .home
        .path()
        .to_path_buf()
}

/// Writes an executable shell script.
#[cfg(unix)]
fn write_program(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// `pty.close` has to end a setup command that ignores the signal the backend's
/// killer sends.
///
/// A host terminal always runs on the no-sandbox backend, whose PTY killer goes
/// through `portable_pty` and is a bare SIGHUP with no escalation, sent twice.
/// A command that ignores SIGHUP would survive both, never reach the pump with
/// an exit code and never publish `pty.exit` — and the IDE waits on `pty.exit`
/// before re-checking prerequisites, so it would wait forever. This is not
/// hypothetical for the real actions: `install_claude` is a non-interactive
/// `bash -lc "curl … | bash"`.
///
/// The stub ignores SIGTERM as well, so only the last rung of the ladder can end
/// it and the whole escalation is exercised rather than just its first step.
#[cfg(unix)]
#[tokio::test]
async fn closing_a_terminal_that_ignores_hangups_still_publishes_an_exit() {
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
    // Told after it is running, so the handlers are certainly installed before
    // anything is sent to it.
    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: pty.clone(),
        data_b64: b64("deaf\r"),
    }))
    .await
    .unwrap();
    pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains(DEAF_LINE),
    )
    .await;
    assert!(out.contains(DEAF_LINE), "collected: {out}");

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: pty.clone(),
    }))
    .await
    .unwrap();
    let exit = pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(30),
        |_, exit| exit.is_some(),
    )
    .await;
    assert!(
        exit.is_some(),
        "pty.close must escalate past a signal the command ignores: {out}"
    );

    // And the session is retired, so the id is not left writable forever.
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

/// Daemon shutdown has to end a setup terminal that ignores hangups, and within
/// a bounded time.
///
/// The host handle's own `shutdown` is the same bare SIGHUP its killers send, so
/// a command that ignores it would outlive the daemon that started it — an
/// unsandboxed login or half-finished installer with nothing left to stop it.
/// The brief's "a login left open does not outlive the daemon" is this.
#[cfg(unix)]
#[tokio::test]
async fn daemon_shutdown_ends_a_terminal_that_ignores_hangups() {
    let _home = fake_home();
    let dir = tempfile::tempdir().unwrap();
    let (port, token, daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
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
    c.call(Request::PtyWrite(PtyWriteParams {
        pty_id: pty.clone(),
        data_b64: b64("deaf\r"),
    }))
    .await
    .unwrap();
    pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains(DEAF_LINE),
    )
    .await;
    assert!(out.contains(DEAF_LINE), "collected: {out}");

    let started = tokio::time::Instant::now();
    daemon.shutdown_host().await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "shutdown must not wait on a process that will never go"
    );

    // The session retires as soon as the pump sees the child exit, so a write
    // that faults is proof the process is really gone rather than merely asked.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
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
    assert!(gone, "the setup terminal must not outlive the daemon");
    cancel.cancel();
}

/// A setup terminal runs a command on the host with no sandbox around it, so it
/// must be behind the same authentication as everything else.
#[tokio::test]
async fn setup_pty_before_hello_is_unauthorized() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    // This test spawns nothing, but it does start a daemon, and `fake_home`
    // writes process-global environment variables. Entering the isolation here
    // too means every test in this file has passed through it before any daemon
    // exists, so the writes can never race a running one.
    #[cfg(unix)]
    fake_home();
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

/// A setup terminal belongs to the connection that opened it. It runs on the
/// host with no sandbox around it, so when its client goes away — the IDE
/// crashed, or was closed mid-login — nothing else is going to close it, and
/// an unsandboxed login left running with nobody watching is precisely what
/// the host path must never leave behind. The daemon closes it when the
/// connection drops; a second connection can watch it go.
#[cfg(unix)]
#[tokio::test]
async fn a_host_terminal_is_closed_when_the_connection_that_opened_it_drops() {
    let _home = fake_home();
    let dir = tempfile::tempdir().unwrap();
    let (port, token, _daemon, cancel) = common::start_daemon(&dir.path().join("data")).await;
    let mut opener = common::Client::connect(port, &token).await;

    let pty = open_setup(&mut opener, SetupAction::GhLogin).await.unwrap();
    let mut out = String::new();
    pump_until(
        &mut opener,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |o, _| o.contains(DEVICE_CODE_LINE),
    )
    .await;
    assert!(out.contains(DEVICE_CODE_LINE), "collected: {out}");

    // A second client, connected before the first goes, sees the same session:
    // the id is an ordinary PTY id and the session is daemon-wide.
    let mut watcher = common::Client::connect(port, &token).await;
    watcher
        .call(Request::PtyResize(PtyResizeParams {
            pty_id: pty.clone(),
            cols: 100,
            rows: 30,
        }))
        .await
        .unwrap();

    drop(opener);

    // Twice the ladder, not the length of it. Losing the connection fires the
    // backend's killer, waits one `SIGNAL_GRACE`, then sends SIGTERM and waits
    // another before SIGKILL -- about a second, plus however long the process
    // takes to die. A deadline set to what the ladder costs is a deadline this
    // test fails on a loaded machine for no reason at all.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut gone = false;
    while !gone && tokio::time::Instant::now() < deadline {
        gone = matches!(
            watcher
                .call(Request::PtyWrite(PtyWriteParams {
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
        "a setup terminal must not outlive the connection that opened it"
    );
    cancel.cancel();
}

/// `pty.close` takes the id `system.setup_pty` returned, and the session is
/// retired once the command is gone.
#[cfg(unix)]
#[tokio::test]
async fn pty_close_accepts_a_host_terminal_id() {
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

    c.call(Request::PtyClose(PtyIdParams {
        pty_id: pty.clone(),
    }))
    .await
    .expect("pty.close takes a host terminal id");
    let exit = pump_until(
        &mut c,
        &mut out,
        &pty,
        std::time::Duration::from_secs(20),
        |_, exit| exit.is_some(),
    )
    .await;
    assert!(exit.is_some(), "pty.exit expected after close: {out}");
    cancel.cancel();
}
