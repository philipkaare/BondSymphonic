//! The in-sandbox half of the proxy: a TCP listener that forwards to the
//! workspace's Unix socket.
//!
//! A sandbox has a network namespace of its own with nothing but loopback, so
//! `HTTP_PROXY=http://127.0.0.1:3128` can only be answered from inside it. This
//! runs there — the same daemon binary, under `proxy-shim` — and hands every
//! connection to `/run/bs/proxy.sock`, which is the workspace's own socket bound
//! in from the host. It makes no decisions: the allowlist is applied on the
//! daemon side, where a process inside the sandbox cannot reach it.

use std::path::Path;

/// Printed on stdout once the listener is up, so whoever started the shim can
/// wait for it rather than racing the first request against the bind.
pub const READY_LINE: &str = "bs-proxy-shim listening";

/// Listens on `listen` and forwards every connection to `socket`. Never returns
/// while the listener is healthy.
#[cfg(unix)]
pub async fn run(socket: &Path, listen: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    // Line-buffered stdout is not guaranteed when stdout is a pipe, so the
    // readiness line is flushed explicitly.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        writeln!(out, "{READY_LINE} on {listen}")?;
        out.flush()?;
    }
    loop {
        let (mut tcp, _) = match listener.accept().await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("proxy-shim: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let socket = socket.to_path_buf();
        tokio::spawn(async move {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(mut daemon) => {
                    // Errors here are ordinary: either side may hang up first.
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut daemon).await;
                }
                Err(e) => eprintln!("proxy-shim: {} is unreachable: {e}", socket.display()),
            }
        });
    }
}

/// The shim exists to serve a bubblewrap sandbox, which is Linux only; the
/// subcommand refuses rather than pretending elsewhere.
#[cfg(not(unix))]
pub async fn run(_socket: &Path, _listen: &str) -> anyhow::Result<()> {
    anyhow::bail!("proxy-shim needs unix sockets")
}
