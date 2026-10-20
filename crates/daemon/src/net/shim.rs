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

/// Printed on stdout once a tunnel's listeners are up.
pub const TUNNEL_READY_LINE: &str = "bs-proxy-tunnel listening";

/// How long the proxy has to answer a tunnel's `CONNECT`. Longer than the
/// proxy's own resolve and connect budgets, so its answer always arrives first.
#[cfg(unix)]
const TUNNEL_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Carries every connection to any of `listen` (addresses inside the sandbox)
/// to `target` (`host:port`) outside it, as a `CONNECT` through `socket`.
///
/// This is how a URL naming the host's loopback works from inside a sandbox: a
/// client that dials `localhost:11434` there - directly, because `NO_PROXY`
/// keeps loopback off the proxy - reaches this listener on the same address,
/// and the proxy decides whether `localhost:11434` on the host may be reached.
/// It is a plain byte tunnel, so TLS, a path prefix and the `Host` header all
/// arrive untouched. An address that cannot be bound is skipped (`::1` in a
/// sandbox without IPv6); none at all is an error.
#[cfg(unix)]
pub async fn tunnel(socket: &Path, listen: &[String], target: &str) -> anyhow::Result<()> {
    let mut listeners = Vec::new();
    for addr in listen {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) => eprintln!("proxy-tunnel: cannot listen on {addr} for {target}: {e}"),
        }
    }
    if listeners.is_empty() {
        anyhow::bail!("proxy-tunnel: no address to listen on for {target}");
    }
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        writeln!(out, "{TUNNEL_READY_LINE} for {target}")?;
        out.flush()?;
    }
    let mut accepting = tokio::task::JoinSet::new();
    for listener in listeners {
        let socket = socket.to_path_buf();
        let target = target.to_string();
        accepting.spawn(async move {
            loop {
                let tcp = match listener.accept().await {
                    Ok((tcp, _)) => tcp,
                    Err(e) => {
                        eprintln!("proxy-tunnel: accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                tokio::spawn(carry(tcp, socket.clone(), target.clone()));
            }
        });
    }
    while accepting.join_next().await.is_some() {}
    Ok(())
}

#[cfg(unix)]
async fn carry(mut tcp: tokio::net::TcpStream, socket: std::path::PathBuf, target: String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let answer = async {
        let mut daemon = tokio::net::UnixStream::connect(&socket).await?;
        daemon
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await?;
        // Byte by byte up to the blank line, so nothing of the tunnel's payload
        // is read here; a refusal's body is never needed.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
            if daemon.read(&mut byte).await? == 0 {
                break;
            }
            head.push(byte[0]);
        }
        std::io::Result::Ok((daemon, head))
    };
    // Whether the proxy refused the origin outright, rather than failing to
    // reach it.
    let mut refused = false;
    let why = match tokio::time::timeout(TUNNEL_ANSWER_TIMEOUT, answer).await {
        Ok(Ok((mut daemon, head))) => {
            let status = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            let code = status.split(' ').nth(1);
            if code == Some("200") {
                // Errors here are ordinary: either side may hang up first.
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut daemon).await;
                return;
            }
            refused = code == Some("403");
            if status.is_empty() {
                "the workspace proxy closed the connection".to_string()
            } else {
                format!("the workspace proxy answered \"{status}\"")
            }
        }
        Ok(Err(e)) => format!("the workspace proxy is unreachable: {e}"),
        Err(_) => "the workspace proxy did not answer in time".to_string(),
    };
    eprintln!("proxy-tunnel: {target}: {why}");
    explain(&mut tcp, &target, &why, refused).await;
}

/// Answers a client the tunnel could not carry, saying why, when it is
/// speaking HTTP; closes when it is not (TLS), where any reply would only be a
/// protocol error.
///
/// An origin the proxy refused is a 403, which a model client takes as final:
/// asking again cannot change the answer. Anything else is a 502, which
/// OpenCode retries with backoff exactly as it retries a server that refuses
/// the connection outside any sandbox, so a model server started a moment
/// later is picked up.
#[cfg(unix)]
async fn explain(tcp: &mut tokio::net::TcpStream, target: &str, why: &str, refused: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut first = [0u8; 8];
    let read = tokio::time::timeout(std::time::Duration::from_secs(2), tcp.read(&mut first)).await;
    let Ok(Ok(n)) = read else {
        return;
    };
    let method = first[..n]
        .iter()
        .take_while(|b| b.is_ascii_uppercase())
        .count();
    if method == 0 || first[..n].get(method) != Some(&b' ') {
        return;
    }
    let (status, hint) = if refused {
        (
            "403 Forbidden",
            "Only the provider URLs in your OpenCode config are reachable, as of the agent's start",
        )
    } else {
        (
            "502 Bad Gateway",
            "Check that the server is running on the host and listening on that address",
        )
    };
    let body =
        format!("BondSymphonic could not reach {target} from this sandbox: {why}. {hint}.\n");
    let _ = tcp
        .write_all(&crate::net::http::text_response(status, &body))
        .await;
    let _ = tcp.shutdown().await;
}

#[cfg(not(unix))]
pub async fn tunnel(_socket: &Path, _listen: &[String], _target: &str) -> anyhow::Result<()> {
    anyhow::bail!("proxy-tunnel needs unix sockets")
}
