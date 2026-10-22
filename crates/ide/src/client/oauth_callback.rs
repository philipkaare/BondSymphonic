//! The loopback listener an MCP server's OAuth sign-in comes back to.
//!
//! The daemon runs in WSL, and a Windows browser cannot be relied on to reach
//! a WSL localhost, so the IDE listens instead: on `127.0.0.1:<ephemeral>`, for
//! one attempt, for at most the 10 minutes the daemon keeps the pending login.
//! The first `GET /callback` whose `state` the attempt accepts is answered with
//! a "You can close this tab" page and ends the listener; the code it carries
//! goes to the daemon in `mcp.callback`, never anywhere else.
//!
//! Here rather than in C++ with a `QTcpServer`: the attempt then belongs to the
//! controller, so it outlives the Settings dialog that started it, and the
//! listener is testable without Qt.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The path the redirect URI names.
pub const CALLBACK_PATH: &str = "/callback";

/// How long a sign-in may take: the daemon's own pending-login lifetime.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(600);

/// A request line longer than this is not a callback.
const MAX_REQUEST: usize = 8 * 1024;

/// How long one connection may take to send its request line, so a browser
/// that opened a socket and said nothing cannot hold the listener.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What one request to the listener said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// The authorization server sent the browser back with a code.
    Code {
        state: String,
        code: String,
        issuer: Option<String>,
    },
    /// The user (or the server) refused: `error` is the OAuth error code.
    Denied { state: String, error: String },
    /// Anything else: a favicon request, a probe, a malformed callback.
    NotCallback,
}

impl CallbackOutcome {
    fn state(&self) -> Option<&str> {
        match self {
            CallbackOutcome::Code { state, .. } | CallbackOutcome::Denied { state, .. } => {
                Some(state)
            }
            CallbackOutcome::NotCallback => None,
        }
    }
}

/// Binds the listener on an ephemeral loopback port.
pub async fn bind() -> std::io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// The redirect URI `mcp.connect` is sent for a listener on `port`.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

/// Reads a request target (`/callback?code=...&state=...`).
pub fn parse_callback_target(target: &str) -> CallbackOutcome {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return CallbackOutcome::NotCallback;
    }
    let mut state = None;
    let mut code = None;
    let mut issuer = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(value) = percent_decode(value) else {
            return CallbackOutcome::NotCallback;
        };
        match key {
            "state" => state = Some(value),
            "code" => code = Some(value),
            "iss" => issuer = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    match (state, code, error) {
        (Some(state), _, Some(error)) if !state.is_empty() => {
            CallbackOutcome::Denied { state, error }
        }
        (Some(state), Some(code), None) if !state.is_empty() && !code.is_empty() => {
            CallbackOutcome::Code {
                state,
                code,
                issuer,
            }
        }
        _ => CallbackOutcome::NotCallback,
    }
}

/// The decoded value of `key` in `url`'s query: how an attempt learns the
/// `state` the daemon put in its authorization URL.
pub fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| percent_decode(v)).flatten()
    })
}

/// `application/x-www-form-urlencoded` decoding: `+` is a space, `%XX` a byte.
/// `None` for a broken escape or bytes that are not UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = value.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Answers requests until one carries a state `accept` takes, and returns it;
/// `None` once `timeout` has passed. A request whose state is not accepted is
/// told the sign-in is no longer active and does not end the wait. The
/// listener is dropped on return, so nothing answers a later request.
pub async fn wait_for_callback(
    listener: TcpListener,
    timeout: Duration,
    mut accept: impl FnMut(&str) -> bool,
) -> Option<CallbackOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    tracing::warn!("MCP sign-in listener failed: {e}");
                    return None;
                }
            },
            _ = tokio::time::sleep_until(deadline) => return None,
        };
        if let Some(outcome) = answer(socket, &mut accept).await {
            return Some(outcome);
        }
    }
}

/// Reads one request and answers it; the outcome when it was the attempt's.
async fn answer(
    mut socket: TcpStream,
    accept: &mut impl FnMut(&str) -> bool,
) -> Option<CallbackOutcome> {
    let target = match tokio::time::timeout(READ_TIMEOUT, request_target(&mut socket)).await {
        Ok(Some(target)) => target,
        _ => return None,
    };
    let outcome = parse_callback_target(&target);
    let accepted = outcome.state().is_some_and(accept);
    let (status, body) = match (&outcome, accepted) {
        (CallbackOutcome::NotCallback, _) => ("404 Not Found", "Not found.".to_owned()),
        (CallbackOutcome::Code { .. }, true) => (
            "200 OK",
            page("Signed in", "BondSymphonic received the sign-in. You can close this tab."),
        ),
        (CallbackOutcome::Denied { .. }, true) => (
            "200 OK",
            page(
                "Sign-in refused",
                "The sign-in was not completed. You can close this tab and try again from BondSymphonic.",
            ),
        ),
        (_, false) => (
            "200 OK",
            page(
                "Sign-in expired",
                "This sign-in attempt is no longer active. Start it again from BondSymphonic.",
            ),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    accepted.then_some(outcome)
}

/// The target of a `GET` request line, read up to its end; `None` for
/// anything else or anything too long.
async fn request_target(socket: &mut TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    let line_end = loop {
        if let Some(end) = request.windows(2).position(|w| w == b"\r\n") {
            break end;
        }
        if request.len() > MAX_REQUEST {
            return None;
        }
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    };
    let line = std::str::from_utf8(&request[..line_end]).ok()?;
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_owned()),
        _ => None,
    }
}

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family: sans-serif; margin: 3em\"><h1>{title}</h1><p>{text}</p></body></html>"
    )
}
