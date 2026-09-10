//! The in-sandbox half of the port bridge: a Unix socket that reaches a port on
//! the sandbox's own loopback.
//!
//! Started as `bondsymphonic-daemon forward --socket /run/bs/fwd-<P>.sock
//! --port <P>` inside the workspace's sandbox, where `/run/bs` is the
//! workspace's run directory bound in from the host. The host side
//! ([`crate::net::bridge`]) connects to the same file from outside.
//!
//! Every connection begins with one status byte written by this end: `1` when
//! the service on `127.0.0.1:<P>` accepted, `0` when it did not. That byte is
//! the whole protocol, and it exists because the host cannot otherwise tell
//! "the sandbox is reachable" from "the app inside it is up": the socket is
//! bound as soon as this process starts, long before a dev server has finished
//! compiling.

use std::path::Path;

/// Printed on stdout once the socket is bound, so whoever started the forwarder
/// can wait for it rather than racing the first connection against the bind.
pub const READY_LINE: &str = "bs-forward listening";

/// How long the service inside the sandbox has to accept a connection before it
/// is reported as not there.
#[cfg(unix)]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Binds `socket` and forwards every connection to `127.0.0.1:port`. Never
/// returns while the listener is healthy.
#[cfg(unix)]
pub async fn run(socket: &Path, port: u16) -> anyhow::Result<()> {
    use crate::net::bridge::{STATUS_FAILED, STATUS_OK};
    use tokio::io::AsyncWriteExt;

    // A socket file left by an earlier run of the same port would make the bind
    // fail; nothing else may live at this path.
    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket)?;
    // Bind leaves the file world-connectable under the usual umask, and it sits
    // in the workspace's run directory on the host: 0600 keeps another account
    // on the machine from reaching into this sandbox's loopback. The sandbox
    // runs as the daemon's own user, so it costs the bridge nothing.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    // Line-buffered stdout is not guaranteed when stdout is a pipe, so the
    // readiness line is flushed explicitly.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        writeln!(out, "{READY_LINE} on {port}")?;
        out.flush()?;
    }
    loop {
        let (mut unix, _) = match listener.accept().await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("forward: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        tokio::spawn(async move {
            let connected = tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect(("127.0.0.1", port)),
            )
            .await;
            match connected {
                Ok(Ok(mut app)) => {
                    if unix.write_all(&[STATUS_OK]).await.is_err() {
                        return;
                    }
                    // Errors here are ordinary: either side may hang up first.
                    let _ = tokio::io::copy_bidirectional(&mut unix, &mut app).await;
                }
                _ => {
                    // The host end drops the connection when it reads this, so
                    // a browser gets a refusal rather than a hang.
                    let _ = unix.write_all(&[STATUS_FAILED]).await;
                }
            }
        });
    }
}

/// The forwarder exists to serve a sandbox with a network namespace of its own,
/// which needs Unix sockets; the subcommand refuses rather than pretending
/// elsewhere.
#[cfg(not(unix))]
pub async fn run(_socket: &Path, _port: u16) -> anyhow::Result<()> {
    anyhow::bail!("forward needs unix sockets")
}
