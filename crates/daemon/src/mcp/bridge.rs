//! The in-sandbox half of the tools socket: pipes an agent's stdio MCP
//! session to the workspace's Unix socket.
//!
//! An agent CLI spawns this as its MCP server (`daemon mcp-bridge`). It
//! interprets nothing: the tools are applied on the daemon side, where a
//! process inside the sandbox cannot reach the credentials or the real repo.

use std::path::Path;

/// Connects to `socket` and carries bytes both ways until the socket closes.
///
/// Stdin EOF half-closes the socket so the daemon sees the session end and
/// closes in turn. The bridge returns when the socket side ends, even if stdin
/// is still open: an agent may keep it open while the daemon goes away.
#[cfg(unix)]
pub async fn run(socket: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use tokio::io::AsyncWriteExt;

    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("{} is unreachable", socket.display()))?;
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    // Spawned, not joined: the stdin read blocks a thread and must not hold
    // the bridge open once the daemon side is done.
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_daemon).await;
        let _ = to_daemon.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    let result = tokio::io::copy(&mut from_daemon, &mut stdout).await;
    stdout.flush().await?;
    result?;
    Ok(())
}

/// The bridge exists to serve a bubblewrap sandbox, which is Linux only.
#[cfg(not(unix))]
pub async fn run(_socket: &Path) -> anyhow::Result<()> {
    anyhow::bail!("mcp-bridge needs unix sockets")
}
