//! The host half of the port bridge: a loopback TCP port that reaches a
//! service running inside a workspace's sandbox.
//!
//! A bubblewrap sandbox has a network namespace of its own, so a web app it
//! starts binds a loopback nothing outside can reach. The bridge gives that app
//! a port on the *host's* loopback instead: every connection accepted here is
//! handed to `<run_dir>/fwd-<P>.sock`, which the forwarder inside the sandbox
//! ([`crate::net::forward`]) has bound at `/run/bs/fwd-<P>.sock` and connects on
//! to `127.0.0.1:<P>` in there.
//!
//! The forwarder writes one status byte before anything else — `1` once it has
//! the in-sandbox connection, `0` when it could not get one — which is what
//! makes [`probe`] a readiness check rather than a guess: a TCP port that
//! accepts is not proof the app behind it is up, but a status byte from the
//! far side of the sandbox is.
//!
//! Nothing here goes through the workspace proxy. The traffic is raw bytes in
//! both directions with no HTTP in it at all, which is what lets WebSockets and
//! a dev server's hot-reload channel work through the same port.

use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// The forwarder said it reached the service.
pub const STATUS_OK: u8 = 1;

/// The forwarder could not reach the service on that port.
pub const STATUS_FAILED: u8 = 0;

/// The whole budget for one [`probe`]: the socket connect *and* the status
/// byte, together. The readiness loop runs every 500 ms, so this is what keeps
/// a probe inside its own turn rather than spilling into the next.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

/// A host port standing in for a port inside one sandbox.
///
/// Dropped without [`stop`](Bridge::stop) the listener keeps running: the
/// bridge outlives the value on purpose, because the accept loop is a spawned
/// task. `stop` is what ends it.
pub struct Bridge {
    /// The port on `127.0.0.1` the host reaches the sandboxed service on.
    pub host_port: u16,
    socket: PathBuf,
    /// Ends the accept loop *and* every connection it started, so a run that
    /// goes away takes its open connections with it.
    cancel: CancellationToken,
}

impl Bridge {
    /// Binds an ephemeral host port and starts forwarding it to `socket`.
    ///
    /// The socket need not exist yet: it is created by the forwarder inside the
    /// sandbox, which is started after this. Connections that arrive before
    /// then simply fail, and [`probe`] reports the run as not ready.
    pub async fn start(socket: PathBuf) -> std::io::Result<Bridge> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let host_port = listener.local_addr()?.port();
        let cancel = CancellationToken::new();
        tokio::spawn(accept_loop(listener, socket.clone(), cancel.clone()));
        tracing::debug!(host_port, socket = %socket.display(), "port bridge listening");
        Ok(Bridge {
            host_port,
            socket,
            cancel,
        })
    }

    /// Ends the listener, its connections and the socket file.
    ///
    /// The file was created inside the sandbox but lives in the workspace's run
    /// directory on the host, so it is the host side that has to clear it: a
    /// stale socket would make the next run on the same port fail to bind.
    pub fn stop(self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.socket);
        tracing::debug!(host_port = self.host_port, "port bridge stopped");
    }
}

/// True when the forwarder at `socket` says the service inside the sandbox
/// accepts a connection right now.
///
/// One round trip: connect to the socket, read the status byte, close. Nothing
/// is sent to the service, so probing a server that logs its requests does not
/// fill the run's output with them.
///
/// Takes the socket path rather than the [`Bridge`], because the readiness loop
/// holds only the path: the bridge itself lives behind a lock that must not be
/// held across the probe's awaits.
pub async fn probe(socket: &std::path::Path) -> bool {
    // One timeout over both steps, not one each: a connect that takes the whole
    // budget leaves nothing for the byte, which is the honest reading of "this
    // did not answer in time".
    let round_trip = tokio::time::timeout(PROBE_BUDGET, async {
        let mut s = tokio::net::UnixStream::connect(socket).await.ok()?;
        let mut status = [0u8; 1];
        s.read_exact(&mut status).await.ok()?;
        Some(status[0])
    });
    matches!(round_trip.await, Ok(Some(STATUS_OK)))
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    socket: PathBuf,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => return,
            a = listener.accept() => a,
        };
        let tcp = match on_accept(accepted) {
            Control::Serve(tcp) => tcp,
            Control::Continue => {
                // Descriptor exhaustion is transient; spinning on it is not.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            Control::Stop => return,
        };
        let socket = socket.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            // No timeout on the connection as a whole: what runs through it may
            // be a WebSocket or a hot-reload stream that lives for hours. Only
            // the handshake with the forwarder is on a clock.
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = serve(tcp, socket) => {}
            }
        });
    }
}

/// What the accept loop does with the outcome of one `accept`.
enum Control {
    /// A connection arrived; serve it.
    Serve(tokio::net::TcpStream),
    /// The accept failed in a way the next one need not: log it, pause, go on.
    Continue,
    /// The listener itself is gone. Nothing will ever be accepted again.
    Stop,
}

/// Decides whether one failed `accept` ends the bridge.
///
/// Almost never: a peer that reset before the handshake finished
/// (`ECONNABORTED`), a moment of descriptor exhaustion (`EMFILE`), an
/// interrupted call - all leave the listener perfectly able to accept the
/// next connection. A loop that returned on the first of them left the port
/// published in the IDE with nobody behind it, which looks exactly like the
/// app inside the sandbox having died. Only a listener that is itself closed
/// (`EBADF`) can never accept again, and that is the one case worth stopping
/// for rather than spinning on.
fn on_accept(result: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>) -> Control {
    match result {
        Ok((tcp, _)) => Control::Serve(tcp),
        Err(e) if e.raw_os_error() == Some(libc::EBADF) => {
            tracing::warn!("port bridge listener is closed; bridge stopped: {e}");
            Control::Stop
        }
        Err(e) => {
            tracing::warn!("port bridge accept failed; continuing: {e}");
            Control::Continue
        }
    }
}

/// One host connection: hand it to the forwarder, or drop it if the forwarder
/// says the service is not there.
async fn serve(mut tcp: tokio::net::TcpStream, socket: PathBuf) {
    let mut inner = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::UnixStream::connect(&socket),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            tracing::debug!(socket = %socket.display(), "no forwarder for this bridge yet");
            return;
        }
    };
    let mut status = [0u8; 1];
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        inner.read_exact(&mut status),
    )
    .await
    {
        Ok(Ok(_)) if status[0] == STATUS_OK => {}
        _ => {
            // The service is not listening inside the sandbox. Closing rather
            // than hanging is what makes a browser say "connection refused"
            // instead of spinning.
            let _ = tcp.shutdown().await;
            return;
        }
    }
    // Errors here are ordinary: either side may hang up first.
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut inner).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed accept is ordinary - a peer that reset before the handshake
    /// finished, a moment of descriptor exhaustion - and must not take the
    /// bridge down: the port stays published in the IDE, and a listener that
    /// quietly stopped accepting would look exactly like a dead app.
    #[tokio::test]
    async fn an_ordinary_accept_error_leaves_the_loop_running() {
        for kind in [
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Other,
        ] {
            assert!(
                matches!(
                    on_accept(Err(std::io::Error::from(kind))),
                    Control::Continue
                ),
                "{kind:?}"
            );
        }
        // Too many open files, the one error worth a pause.
        let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
        assert!(matches!(on_accept(Err(emfile)), Control::Continue));
    }

    /// Only a listener that is itself gone ends the loop: nothing will ever
    /// be accepted from a closed descriptor, and spinning on it would burn a
    /// core for the life of the daemon.
    #[tokio::test]
    async fn a_closed_listener_stops_the_loop() {
        let ebadf = std::io::Error::from_raw_os_error(libc::EBADF);
        assert!(matches!(on_accept(Err(ebadf)), Control::Stop));
    }

    /// The accepted connection comes back to be served.
    #[tokio::test]
    async fn an_accepted_connection_is_handed_on() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let accepted = listener.accept().await;
        assert!(matches!(on_accept(accepted), Control::Serve(_)));
    }
}
