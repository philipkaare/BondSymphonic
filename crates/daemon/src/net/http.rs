//! The HTTP message plumbing the proxy is built on: parsing a request head,
//! reading how its body is framed, rewriting it for the origin, and carrying
//! one request and its response between two streams.
//!
//! Split out of [`super::proxy`], which keeps the policy - which hosts a
//! workspace may reach, which addresses a name is allowed to resolve to, the
//! listener and its connection loop. Nothing here knows what an allowlist is;
//! everything here is about getting one HTTP message from one socket to
//! another without the two ends disagreeing about where it ends. That
//! disagreement is what request smuggling is made of, so the rules are strict
//! and the refusals are 400s rather than guesses.
//!
//! The parsing is pure and synchronous so it can be tested without a socket,
//! and the relays are generic over their streams so they can be tested over a
//! pair of in-memory pipes.

// The connection loop that drives all of this needs a Unix socket, so on
// Windows - where the daemon exists only for the test suite - most of this
// module is reached only from the tests at the end of it.
#![cfg_attr(not(unix), allow(dead_code))]

/// The most of a request head the proxy will buffer before giving up.
///
/// A head that never ends is otherwise an unbounded allocation driven by the
/// sandbox, and 64 KiB is far past any real request line and header block.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// How long a request body may go without a byte moving in either direction
/// before the exchange is abandoned.
///
/// A client that promises a body and then stops otherwise holds its own task,
/// its socket and an open connection to an allowlisted host for as long as it
/// likes, and nothing else bounds that: the head timeout is spent by then, and
/// so is the connect timeout. An *idle* deadline rather than a total one,
/// because a slow upload that keeps sending is a legitimate thing to do and
/// must not be cut off in the middle. It stops applying once the body is
/// through: an origin that thinks about a request for a long while before
/// answering is also legitimate, and the CONNECT tunnel beside this has no
/// deadline at all for the same reason.
#[cfg(unix)]
const BODY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The request line and headers of one proxied request.
///
/// `head_len` counts the blank line, so `&buf[head_len..]` is the body (or the
/// first bytes of a tunnel) the client had already sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestHead {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub head_len: usize,
}

/// Parses a request head, or `None` while the blank line ending it has not
/// arrived yet, resuming the search for that blank line at `from`.
///
/// A malformed request line is *not* reported as incomplete — that would leave
/// the caller waiting on a client that has already finished — but as a head
/// whose target [`target_host_port`] cannot resolve, which answers 400.
///
/// A reader that appends `n` bytes and re-scans from byte zero every time is
/// quadratic in the size of the head, and the client driving it sits inside the
/// sandbox: one byte per read over a 64 KiB budget is billions of comparisons
/// for a single connection. Resuming from `len - n - 3` — three bytes back, so
/// a terminator straddling the join is still seen — makes the whole read
/// linear. `from` may safely be any value: it is clamped, and passing 0 is
/// always correct, just slower.
fn parse_request_head_from(buf: &[u8], from: usize) -> Option<RequestHead> {
    let from = from.min(buf.len());
    let end = from + buf[from..].windows(4).position(|w| w == b"\r\n\r\n")?;
    // Lossy rather than strict UTF-8: header values are bytes, and a request
    // that is complete must be answered rather than waited on.
    let text = String::from_utf8_lossy(&buf[..end]);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default().to_string();
    let version = first.next().unwrap_or("HTTP/1.1").to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Some(RequestHead {
        method,
        target,
        version,
        headers,
        head_len: end + 4,
    })
}

/// The host and port this request wants to reach, or `None` when it is not
/// something a proxy can serve.
///
/// The host comes back bare: an IPv6 literal's brackets belong to the URI
/// syntax, not to the name the allowlist matches or the name a socket connects
/// to. An `https://` absolute URI has no answer here on purpose — serving it
/// would mean terminating TLS, and clients use `CONNECT` for that instead.
pub(super) fn target_host_port(head: &RequestHead) -> Option<(String, u16)> {
    if head.method.eq_ignore_ascii_case("CONNECT") {
        // CONNECT carries an authority and nothing else; 443 is the only port
        // a client that omits it can mean.
        return split_authority(&head.target, 443);
    }
    let rest = strip_http_scheme(&head.target)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    split_authority(authority, 80)
}

/// The `http://` prefix, case-insensitively, or `None` for anything else.
fn strip_http_scheme(target: &str) -> Option<&str> {
    let prefix = target.get(..7)?;
    prefix.eq_ignore_ascii_case("http://").then(|| &target[7..])
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port` — with any userinfo
/// dropped — into a bare host and a port.
fn split_authority(auth: &str, default_port: u16) -> Option<(String, u16)> {
    // Userinfo is part of the URI, not of the host being reached.
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    if let Some(rest) = auth.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail {
            "" => default_port,
            t => t.strip_prefix(':')?.parse().ok()?,
        };
        return Some((host.to_string(), port));
    }
    if auth.is_empty() {
        return None;
    }
    match auth.rsplit_once(':') {
        // An unbracketed address with several colons is a bare IPv6 literal,
        // where the last colon is part of the address rather than a port. It is
        // malformed in a URI, and guessing a port out of it would connect
        // somewhere the client never named.
        Some(_) if auth.matches(':').count() > 1 => None,
        Some((host, port)) if !host.is_empty() => Some((host.to_string(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((auth.to_string(), default_port)),
    }
}

/// The authority of an absolute-form target, userinfo dropped, exactly as the
/// URI spells it - port and all. `None` for anything that is not absolute
/// form.
///
/// This is what the request was *checked* against, so it is what the origin
/// must be told in `Host`; [`target_host_port`] is the same authority split up
/// and normalised for the allowlist and the socket.
fn uri_authority(target: &str) -> Option<String> {
    let rest = strip_http_scheme(target)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    (!authority.is_empty()).then(|| authority.to_string())
}

/// The header names the client listed in its `Connection` header, which are
/// hop-by-hop by the client's own declaration (RFC 9110 §7.6.1). The two
/// connection *options* are not header names and are dropped here; the
/// `Connection` header itself never goes upstream either way.
fn connection_named(head: &RequestHead) -> Vec<String> {
    head.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && t != "close" && t != "keep-alive")
        .collect()
}

/// The request as the upstream server expects it: an origin-form request line,
/// without the hop-by-hop headers, and with `Connection: close` in their place.
///
/// The close is the proxy's own: whatever the client asked for, the upstream
/// is told to end its connection after this one response. Its end of stream is
/// then the end of the response, so the response can be relayed without the
/// proxy understanding its framing, and the client's next request - which may
/// name a different host - gets checked and connected on its own. Every origin
/// honours it. The proxy never closes the client's own connection over it, but
/// the response carries the `Connection: close` back, so a well-behaved client
/// usually closes anyway and opens a fresh connection for its next request:
/// the loop is there for the clients that do *not* - a pipelining one, or one
/// that ignores the header - because those are the ones whose second request
/// used to be delivered to the first request's host.
///
/// The client's HTTP version is kept rather than forced to 1.1: an HTTP/1.0
/// client told the server it speaks 1.1 would be sent chunked responses it
/// cannot read.
pub(super) fn origin_form(head: &RequestHead) -> Vec<u8> {
    let path = match strip_http_scheme(&head.target) {
        Some(rest) => match rest.find('/') {
            Some(i) => &rest[i..],
            // `http://host` with no path at all still asks for the root.
            None => "/",
        },
        None => head.target.as_str(),
    };
    // The framing this proxy believes is the framing the upstream is given, in
    // the proxy's own words rather than the client's. A `Content-Length`
    // beside a chunked body is dropped: the body is walked as chunked and
    // passed on as chunked, and an origin that read the length instead would
    // stop short and take the rest of the body for a second request to that
    // host. RFC 9112 lets an intermediary drop the length rather than refuse
    // the whole request. A length that *is* the framing is re-emitted as the
    // number that was parsed, because `u64::from_str` accepts spellings an
    // origin may not: `+5` frames five bytes here and nothing at all there,
    // and those five bytes would be read as a request of their own.
    let framing = body_framing(head);
    let mut out = format!("{} {} {}\r\n", head.method, path, head.version);
    // Absolute form carries the host twice: in the URI, which is what the
    // allowlist checked and what the socket was opened to, and in `Host`,
    // which is what the origin routes on. The URI wins, so a request cleared
    // for one host cannot arrive there naming another. An origin-form request
    // has no URI authority and keeps the `Host` it came with.
    let authority = uri_authority(&head.target);
    if let Some(authority) = &authority {
        out.push_str(&format!("Host: {authority}\r\n"));
    }
    let named = connection_named(head);
    for (name, value) in &head.headers {
        // Hop-by-hop, addressed to this proxy: forwarding them would leak the
        // client's proxy credentials to the upstream server and confuse its
        // connection handling. `Connection` and `Keep-Alive` describe the
        // client's connection to the proxy, not the proxy's to the server,
        // whose lifetime is decided below. Whatever else `Connection` names is
        // hop-by-hop because the client said so.
        if is_hop_by_hop(name) || named.iter().any(|t| name.eq_ignore_ascii_case(t)) {
            continue;
        }
        if authority.is_some() && name.eq_ignore_ascii_case("host") {
            continue;
        }
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(BodyFraming::Length(n)) = framing {
        out.push_str(&format!("Content-Length: {n}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// Whether a request header is about the client's connection to this proxy
/// rather than about the request, and so must not be passed on.
fn is_hop_by_hop(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("proxy-"))
}

/// Whether the client wants its connection closed once this request has been
/// answered: it said so, or it speaks HTTP/1.0, where staying open has to be
/// asked for.
///
/// This and the body-relay helpers below are reached only from the connection
/// loop, which needs a Unix socket; the Windows build keeps them for the tests
/// at the end of this file, which is what the dead-code allowance is for.
pub(super) fn wants_close(head: &RequestHead) -> bool {
    let tokens: Vec<String> = head
        .headers
        .iter()
        .filter(|(k, _)| {
            k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("proxy-connection")
        })
        .flat_map(|(_, v)| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    if tokens.iter().any(|t| t == "close") {
        return true;
    }
    head.version.eq_ignore_ascii_case("HTTP/1.0") && !tokens.iter().any(|t| t == "keep-alive")
}

/// How the body of a request is delimited, read off its head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyFraming {
    /// No body at all: neither `Content-Length` nor `Transfer-Encoding`.
    None,
    /// Exactly this many bytes follow the head.
    Length(u64),
    /// Chunked transfer coding; the body ends with its zero-size chunk and
    /// trailers.
    Chunked,
}

/// The body framing of `head`, or `None` for a head that does not say
/// consistently: a `Transfer-Encoding` other than chunked (the proxy would not
/// know where the body ends, and so where the next request begins), a
/// `Content-Length` that is not a number, or two that disagree. Every one of
/// those is a 400; RFC 9112 calls them request smuggling vectors.
pub(super) fn body_framing(head: &RequestHead) -> Option<BodyFraming> {
    let mut codings = head
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, v)| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .peekable();
    if codings.peek().is_some() {
        // Chunked must be the final coding applied, and it is the only one
        // the proxy can find the end of. It also takes precedence over any
        // `Content-Length`, which is then ignored rather than believed.
        return (codings.last().as_deref() == Some("chunked")).then_some(BodyFraming::Chunked);
    }
    let mut length = None;
    for (_, v) in head
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        // A list value (`5, 5`) is one header repeated, and treated as such.
        for item in v.split(',') {
            let n: u64 = item.trim().parse().ok()?;
            if length.replace(n).is_some_and(|prev| prev != n) {
                return None;
            }
        }
    }
    Some(match length {
        Some(n) => BodyFraming::Length(n),
        None => BodyFraming::None,
    })
}

/// A complete, self-describing plain-text response. Built rather than written
/// out, so a `Content-Length` can never drift away from its body.
pub(super) fn text_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Sent when the request is not one a proxy can serve at all.
#[cfg(unix)]
pub(super) fn bad_request() -> Vec<u8> {
    text_response(
        "400 Bad Request",
        "not a proxy request this daemon can serve\n",
    )
}

/// Sent when a client promised a body and then stopped sending it. See
/// [`BODY_IDLE_TIMEOUT`].
#[cfg(unix)]
pub(super) fn request_timeout() -> Vec<u8> {
    text_response("408 Request Timeout", "the request body stopped arriving\n")
}

/// Sent when an allowed host would not take the connection. The reason stays in
/// the daemon's log: the client is inside the sandbox.
#[cfg(unix)]
pub(super) fn bad_gateway() -> Vec<u8> {
    text_response("502 Bad Gateway", "could not connect to the host\n")
}

/// Reads until the head is complete. `None` means the client hung up first or
/// sent more than [`MAX_HEAD_BYTES`] without finishing.
#[cfg(unix)]
pub(super) async fn read_head(
    client: &mut tokio::net::UnixStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<RequestHead>> {
    use tokio::io::AsyncReadExt;
    // How much of the buffer has already been searched for the blank line. Kept
    // across reads so the scan is linear in the head rather than quadratic; see
    // [`parse_request_head_from`].
    let mut scanned = 0usize;
    loop {
        if let Some(head) = parse_request_head_from(buf, scanned) {
            return Ok(Some(head));
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Ok(None);
        }
        // Three bytes back from the end, so a terminator split across this read
        // and the next is still found.
        scanned = buf.len().saturating_sub(3);
        let mut chunk = [0u8; 4096];
        match client.read(&mut chunk).await? {
            0 => return Ok(None),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The longest line the chunked-body relay will buffer looking for its end:
/// a chunk-size line or a trailer. Real ones are a few bytes.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// A writer that counts what it has passed on.
///
/// Two things need the count. The idle deadline needs to know whether anything
/// moved during the last interval, and the error path needs to know whether any
/// of the response has already reached the client - because once it has, a 400
/// written after it is not an error message but a second response spliced onto
/// the first, which no client can tell apart from the body it was reading.
#[cfg(unix)]
struct Counted<'a, W> {
    inner: &'a mut W,
    count: &'a std::sync::atomic::AtomicU64,
}

#[cfg(unix)]
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Counted<'_, W> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let n = std::task::ready!(std::pin::Pin::new(&mut *this.inner).poll_write(cx, buf))?;
        this.count
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        std::task::Poll::Ready(Ok(n))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

/// How relaying one request body up and one response down ended.
#[cfg(unix)]
pub(super) enum Relayed {
    /// The response was relayed to its end. Nothing is owed to the client.
    Done,
    /// The upstream closed without a byte of response.
    NoResponse,
    /// The request body could not be walked any further. `relayed` is how much
    /// of the response had already reached the client.
    BadBody { relayed: u64 },
    /// Nothing moved either way for [`BODY_IDLE_TIMEOUT`] while the body was
    /// still owed. `relayed` is how much of the response had already reached
    /// the client.
    Stalled { relayed: u64 },
}

/// Carries one request body up and the response back down at the same time.
///
/// At the same time, because a server may answer - a `100 Continue`, or a
/// refusal - before it has read the body, and a client waiting on that answer
/// would otherwise never send it. The upstream was asked to close after its
/// response, so its end of stream is the end of the response and nothing here
/// has to understand response framing.
///
/// Both directions are counted, which is what makes the two failures above
/// answerable: an idle interval is one in which neither count moved, and a
/// half-written response is one whose count is not zero.
#[cfg(unix)]
pub(super) async fn relay_exchange<CR, CW, UR, UW>(
    from_client: &mut CR,
    to_client: &mut CW,
    from_upstream: &mut UR,
    to_upstream: &mut UW,
    buf: &mut Vec<u8>,
    framing: BodyFraming,
) -> std::io::Result<Relayed>
where
    CR: tokio::io::AsyncRead + Unpin,
    CW: tokio::io::AsyncWrite + Unpin,
    UR: tokio::io::AsyncRead + Unpin,
    UW: tokio::io::AsyncWrite + Unpin,
{
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let up = AtomicU64::new(0);
    let down = AtomicU64::new(0);
    // Once the body is through, the deadline stops applying; see
    // [`BODY_IDLE_TIMEOUT`].
    let body_done = AtomicBool::new(false);
    let mut counted_up = Counted {
        inner: to_upstream,
        count: &up,
    };
    let mut counted_down = Counted {
        inner: to_client,
        count: &down,
    };
    let exchange = async {
        tokio::try_join!(
            async {
                let r = relay_body(from_client, &mut counted_up, buf, framing).await;
                body_done.store(true, Ordering::Relaxed);
                r
            },
            tokio::io::copy(from_upstream, &mut counted_down),
        )
    };
    // Pinned rather than dropped and rebuilt on each interval: a timeout that
    // dropped this future would lose whatever `copy` had read into its own
    // buffer and not yet written.
    tokio::pin!(exchange);
    let mut seen = (0u64, 0u64);
    let outcome = loop {
        match tokio::time::timeout(BODY_IDLE_TIMEOUT, &mut exchange).await {
            Ok(outcome) => break outcome,
            Err(_) if body_done.load(Ordering::Relaxed) => continue,
            Err(_) => {
                let moved = (up.load(Ordering::Relaxed), down.load(Ordering::Relaxed));
                if moved == seen {
                    return Ok(Relayed::Stalled { relayed: moved.1 });
                }
                seen = moved;
            }
        }
    };
    match outcome {
        Ok((_, 0)) => Ok(Relayed::NoResponse),
        Ok(_) => Ok(Relayed::Done),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Ok(Relayed::BadBody {
            relayed: down.load(Ordering::Relaxed),
        }),
        Err(e) => Err(e),
    }
}

/// Carries one request body from the client to the upstream, exactly as
/// delimited by `framing`, and leaves `buf` holding whatever followed it - the
/// next request of a pipelining client. `buf` holds the bytes already read
/// past the head when this starts.
///
/// The bytes are passed on verbatim: a chunked body is not re-coded, only
/// walked, so the proxy knows where it ends. A body that cannot be walked - a
/// chunk size that is not a number, a line without end - is `InvalidData`,
/// which the caller turns into a 400; any other error is the connection.
///
/// Generic over the streams so it can be tested on a pair of in-memory pipes.
async fn relay_body<R, W>(
    client: &mut R,
    upstream: &mut W,
    buf: &mut Vec<u8>,
    framing: BodyFraming,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    match framing {
        BodyFraming::None => Ok(()),
        BodyFraming::Length(n) => relay_exact(client, upstream, buf, n).await,
        BodyFraming::Chunked => {
            loop {
                let line = read_line(client, buf).await?;
                // Read before it is written: a size the proxy cannot read is a
                // 400 to the client, and an origin that had already been handed
                // it would be framing this body by a number the proxy never
                // agreed to.
                let size = chunk_size(&line)?;
                upstream.write_all(&line).await?;
                if size == 0 {
                    break;
                }
                relay_exact(client, upstream, buf, size).await?;
                // Every chunk's data ends with CRLF. Passing those two bytes
                // through unchecked lets a client end a chunk with something a
                // lenient origin accepts and this proxy does not, and the body
                // then ends in two different places - which is where request
                // smuggling starts.
                relay_chunk_end(client, upstream, buf).await?;
            }
            // Trailers, up to and including the blank line that ends the body.
            loop {
                let line = read_line(client, buf).await?;
                upstream.write_all(&line).await?;
                if line == b"\r\n" {
                    return Ok(());
                }
            }
        }
    }
}

/// Passes exactly `remaining` bytes from `client` to `upstream`, taking what
/// is already in `buf` first and leaving any surplus there.
async fn relay_exact<R, W>(
    client: &mut R,
    upstream: &mut W,
    buf: &mut Vec<u8>,
    mut remaining: u64,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fits = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
    let take = buf.len().min(fits(remaining));
    if take > 0 {
        upstream.write_all(&buf[..take]).await?;
        buf.drain(..take);
        remaining -= take as u64;
    }
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = chunk.len().min(fits(remaining));
        let n = client.read(&mut chunk[..want]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed inside the request body",
            ));
        }
        upstream.write_all(&chunk[..n]).await?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Checks and forwards the CRLF that ends one chunk's data.
///
/// Anything else there is `InvalidData`, which the caller turns into a 400: the
/// chunk was as long as it said, and what follows it is not the next chunk.
async fn relay_chunk_end<R, W>(
    client: &mut R,
    upstream: &mut W,
    buf: &mut Vec<u8>,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    while buf.len() < 2 {
        let mut chunk = [0u8; 1024];
        match client.read(&mut chunk).await? {
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed at the end of a chunk",
                ))
            }
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
    if &buf[..2] != b"\r\n" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk is not ended by CRLF",
        ));
    }
    upstream.write_all(b"\r\n").await?;
    buf.drain(..2);
    Ok(())
}

/// One line of `buf`, CRLF included, reading more from `client` until it is
/// complete. A line longer than [`MAX_LINE_BYTES`] is `InvalidData`; a client
/// that closes mid-line is `UnexpectedEof`.
async fn read_line<R>(client: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut scanned = 0usize;
    loop {
        if let Some(i) = buf[scanned..].windows(2).position(|w| w == b"\r\n") {
            let end = scanned + i + 2;
            return Ok(buf.drain(..end).collect());
        }
        if buf.len() >= MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk line too long",
            ));
        }
        scanned = buf.len().saturating_sub(1);
        let mut chunk = [0u8; 1024];
        match client.read(&mut chunk).await? {
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed inside a chunked body",
                ))
            }
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The size a chunk-size line announces: hex, with any `;ext=…` after it
/// ignored. Anything else is `InvalidData`.
fn chunk_size(line: &[u8]) -> std::io::Result<u64> {
    let text = std::str::from_utf8(line)
        .ok()
        .and_then(|t| t.strip_suffix("\r\n"))
        .map(|t| t.split(';').next().unwrap_or_default().trim())
        .filter(|t| !t.is_empty());
    text.and_then(|t| u64::from_str_radix(t, 16).ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> RequestHead {
        parse_request_head_from(text.as_bytes(), 0).expect("a complete head")
    }

    /// The head is parsed off a stream, so it must say "not yet" rather than
    /// guess: a request line without its blank line may still be growing.
    #[test]
    fn a_head_parses_only_once_the_blank_line_has_arrived() {
        assert!(parse_request_head_from(b"GET http://a/ HTTP/1.1\r\nHost: a\r\n", 0).is_none());
        assert!(parse_request_head_from(b"CONNECT a:443 HTTP/1.1\r\n", 0).is_none());
        assert!(parse_request_head_from(b"", 0).is_none());
        let h = head("GET http://a/x HTTP/1.1\r\nHost: a\r\nAccept: */*\r\n\r\nbody");
        assert_eq!(
            (h.method.as_str(), h.target.as_str()),
            ("GET", "http://a/x")
        );
        assert_eq!(h.version, "HTTP/1.1");
        assert_eq!(
            h.headers,
            vec![
                ("Host".to_string(), "a".to_string()),
                ("Accept".to_string(), "*/*".to_string())
            ]
        );
        // `head_len` stops at the blank line, so the caller keeps the body.
        assert_eq!(
            &"GET http://a/x HTTP/1.1\r\nHost: a\r\nAccept: */*\r\n\r\nbody".as_bytes()
                [h.head_len..],
            b"body"
        );
    }

    /// A CONNECT target is an authority, and an IPv6 literal wears brackets
    /// there that the allowlist and `TcpStream::connect` must not see.
    #[test]
    fn connect_targets_yield_a_bare_host_and_a_port() {
        assert_eq!(
            target_host_port(&head("CONNECT github.com:443 HTTP/1.1\r\n\r\n")),
            Some(("github.com".into(), 443))
        );
        assert_eq!(
            target_host_port(&head("CONNECT [::1]:8443 HTTP/1.1\r\n\r\n")),
            Some(("::1".into(), 8443))
        );
        // No port at all: the only thing CONNECT is ever used for is TLS.
        assert_eq!(
            target_host_port(&head("CONNECT github.com HTTP/1.1\r\n\r\n")),
            Some(("github.com".into(), 443))
        );
        assert_eq!(
            target_host_port(&head("CONNECT github.com:notaport HTTP/1.1\r\n\r\n")),
            None
        );
    }

    #[test]
    fn an_absolute_uri_defaults_to_port_80_and_keeps_an_explicit_one() {
        assert_eq!(
            target_host_port(&head("GET http://example.com/x?y HTTP/1.1\r\n\r\n")),
            Some(("example.com".into(), 80))
        );
        assert_eq!(
            target_host_port(&head("POST http://example.com:8080/ HTTP/1.1\r\n\r\n")),
            Some(("example.com".into(), 8080))
        );
        // No path at all is still a valid absolute URI.
        assert_eq!(
            target_host_port(&head("GET http://example.com HTTP/1.1\r\n\r\n")),
            Some(("example.com".into(), 80))
        );
        // Userinfo belongs to the URI, not to the host being checked.
        assert_eq!(
            target_host_port(&head("GET http://u:p@example.com/ HTTP/1.1\r\n\r\n")),
            Some(("example.com".into(), 80))
        );
    }

    /// Everything else — an origin-form request sent to the proxy by mistake,
    /// or an `https://` absolute URI, which a proxy cannot serve without
    /// terminating TLS — has no target rather than a guessed one.
    #[test]
    fn a_request_that_is_not_proxyable_has_no_target() {
        assert_eq!(target_host_port(&head("GET / HTTP/1.1\r\n\r\n")), None);
        assert_eq!(
            target_host_port(&head("GET https://example.com/ HTTP/1.1\r\n\r\n")),
            None
        );
        assert_eq!(
            target_host_port(&head("GET http:// HTTP/1.1\r\n\r\n")),
            None
        );
    }

    #[test]
    fn origin_form_rewrites_the_request_line_and_drops_the_proxy_headers() {
        let h = head(
            "GET http://example.com:8080/a/b?c=d HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\nproxy-authorization: Basic x\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
        );
        // The upstream is asked to close after this response, whatever the
        // client asked for: that is what delimits the response, so the next
        // request on the client's connection can be checked on its own. `Host`
        // is the URI's authority, which is the host that was checked; see
        // `an_absolute_uri_names_upstream_the_host_it_was_checked_against`.
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "GET /a/b?c=d HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        // A URI with no path becomes the root.
        let h = head("GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        );
    }

    /// A body cannot be delimited two ways at once. The proxy walks a chunked
    /// body as chunked, so the upstream has to read it as chunked too: an
    /// origin that believed a `Content-Length` beside it would stop short and
    /// take the rest of the body for a second, smuggled request to that same
    /// host. RFC 9112 gives an intermediary two options here, and dropping the
    /// length is the one that still serves the request.
    #[test]
    fn a_chunked_request_is_forwarded_without_a_content_length_beside_it() {
        let h = head(
            "POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "POST / HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        // Without a transfer coding the length *is* the framing, and stays.
        let h = head(
            "POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\n",
        );
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
        );
    }

    /// The read loop resumes its search where the last one stopped, so a head
    /// that dribbles in one byte at a time must still be found — exactly once,
    /// at the byte that completes the blank line.
    #[test]
    fn a_head_arriving_one_byte_at_a_time_is_found_where_it_ends() {
        let text = "GET http://a/x HTTP/1.1\r\nHost: a\r\n\r\nbody";
        let mut buf: Vec<u8> = Vec::new();
        let mut found = None;
        for (i, byte) in text.bytes().enumerate() {
            // Mirrors `read_head`: the offset is taken before the bytes land.
            let scanned = buf.len().saturating_sub(3);
            buf.push(byte);
            if let Some(head) = parse_request_head_from(&buf, scanned) {
                found = Some((i, head));
                break;
            }
        }
        let (i, head) = found.expect("the head is complete before the body");
        // The byte that completed it is the last of the blank line, and nothing
        // of the body had arrived yet.
        assert_eq!(i + 1, text.find("body").unwrap());
        assert_eq!(head.head_len, text.find("body").unwrap());
        assert_eq!(head.target, "http://a/x");
        assert_eq!(head.headers, vec![("Host".to_string(), "a".to_string())]);
        // Resuming from an offset can never find *more* than starting at zero.
        assert_eq!(parse_request_head_from(text.as_bytes(), 0), Some(head));
    }

    /// A client that never sends the blank line gets no request out of the
    /// proxy however much it sends; the read loop stops it at the cap.
    #[test]
    fn a_head_that_never_ends_is_never_parsed() {
        let junk = vec![b'x'; MAX_HEAD_BYTES + 1];
        assert!(parse_request_head_from(&junk, 0).is_none());
    }

    /// Where one request's body ends is where the next request begins, so the
    /// head has to say it unambiguously or the request is not served.
    #[test]
    fn body_framing_is_read_off_the_head_or_refused() {
        let framing = |h: &str| body_framing(&head(h));
        assert_eq!(
            framing("GET http://a/ HTTP/1.1\r\nHost: a\r\n\r\n"),
            Some(BodyFraming::None)
        );
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nContent-Length: 12\r\n\r\n"),
            Some(BodyFraming::Length(12))
        );
        // The same length twice is one header repeated; two lengths are an
        // attack on whichever side believes the other one.
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n"),
            Some(BodyFraming::Length(5))
        );
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"),
            None
        );
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nContent-Length: five\r\n\r\n"),
            None
        );
        // Chunked wins over a length, and is the only coding that can be
        // walked to its end.
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n"),
            Some(BodyFraming::Chunked)
        );
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nTransfer-Encoding: gzip, Chunked\r\n\r\n"),
            Some(BodyFraming::Chunked)
        );
        assert_eq!(
            framing("POST http://a/ HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n"),
            None
        );
    }

    /// The client's connection to the proxy outlives a request unless the
    /// client says otherwise, or is too old to have said anything.
    #[test]
    fn the_client_decides_whether_its_connection_stays_open() {
        assert!(!wants_close(&head("GET http://a/ HTTP/1.1\r\n\r\n")));
        assert!(wants_close(&head(
            "GET http://a/ HTTP/1.1\r\nConnection: close\r\n\r\n"
        )));
        assert!(wants_close(&head(
            "GET http://a/ HTTP/1.1\r\nConnection: keep-alive, Close\r\n\r\n"
        )));
        assert!(wants_close(&head(
            "GET http://a/ HTTP/1.1\r\nProxy-Connection: close\r\n\r\n"
        )));
        assert!(wants_close(&head("GET http://a/ HTTP/1.0\r\n\r\n")));
        assert!(!wants_close(&head(
            "GET http://a/ HTTP/1.0\r\nProxy-Connection: Keep-Alive\r\n\r\n"
        )));
    }

    /// Runs the relay over in-memory pipes: `already` is what had arrived with
    /// the head, `later` what the client sends afterwards. Returns what the
    /// upstream received and what was left over for the next request, whether
    /// that stayed in the buffer or was never read off the client.
    async fn relay(
        already: &[u8],
        later: &[u8],
        framing: BodyFraming,
    ) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        let (result, received, rest) = relay_full(already, later, framing).await;
        result.map(|()| (received, rest))
    }

    /// The same, but keeping what reached the upstream even when the relay
    /// failed: a body the proxy refuses must not have been passed on first.
    async fn relay_full(
        already: &[u8],
        later: &[u8],
        framing: BodyFraming,
    ) -> (std::io::Result<()>, Vec<u8>, Vec<u8>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client_side, mut proxy_side) = tokio::io::duplex(64);
        let (mut proxy_out, mut upstream_side) = tokio::io::duplex(64);
        let later = later.to_vec();
        let sender = tokio::spawn(async move {
            client_side.write_all(&later).await.unwrap();
            drop(client_side);
        });
        let mut received = Vec::new();
        let receiver = tokio::spawn(async move {
            upstream_side.read_to_end(&mut received).await.unwrap();
            received
        });
        let mut buf = already.to_vec();
        let result = relay_body(&mut proxy_side, &mut proxy_out, &mut buf, framing).await;
        drop(proxy_out);
        sender.await.unwrap();
        let received = receiver.await.unwrap();
        proxy_side.read_to_end(&mut buf).await.unwrap();
        (result, received, buf)
    }

    /// A body of known length is passed on whole and nothing past it: what
    /// follows is the next request, and stays with the client's connection.
    #[tokio::test]
    async fn a_body_of_known_length_is_relayed_and_the_rest_kept() {
        let (got, rest) = relay(b"abc", b"defNEXT", BodyFraming::Length(6))
            .await
            .unwrap();
        assert_eq!(got, b"abcdef");
        assert_eq!(rest, b"NEXT");
        // Entirely in hand already, with the next request behind it.
        let (got, rest) = relay(b"abcdefGET", b"", BodyFraming::Length(6))
            .await
            .unwrap();
        assert_eq!(got, b"abcdef");
        assert_eq!(rest, b"GET");
        // No body: nothing moves, nothing is lost.
        let (got, rest) = relay(b"GET", b"", BodyFraming::None).await.unwrap();
        assert!(got.is_empty());
        assert_eq!(rest, b"GET");
        // A client that hangs up mid-body is a broken connection, not a 400.
        let err = relay(b"ab", b"c", BodyFraming::Length(6))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// A chunked body is walked to its terminating chunk and trailers, passed
    /// on byte for byte, and what follows it is kept.
    #[tokio::test]
    async fn a_chunked_body_is_relayed_verbatim_to_its_end() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nTrailer: x\r\n\r\n";
        let (already, later) = body.split_at(7);
        let (got, rest) = relay(already, &[later, b"NEXT"].concat(), BodyFraming::Chunked)
            .await
            .unwrap();
        assert_eq!(got, body);
        assert_eq!(rest, b"NEXT");
        // The client's framing is what is checked, so a size that is not a
        // number is refused rather than guessed.
        let err = relay(b"zz\r\nab\r\n0\r\n\r\n", b"", BodyFraming::Chunked)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let err = relay(b"", &vec![b'a'; MAX_LINE_BYTES + 1], BodyFraming::Chunked)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A chunk-size line the proxy cannot read is a 400 to the client, and the
    /// origin must never have seen it. Writing the line upstream before
    /// checking it leaves the origin framing the body one way and the proxy
    /// another, which is the disagreement every check in here exists to
    /// prevent.
    #[tokio::test]
    async fn a_chunk_size_reaches_the_origin_only_once_it_has_been_read() {
        let (result, upstream, _) =
            relay_full(b"zz\r\nab\r\n0\r\n\r\n", b"", BodyFraming::Chunked).await;
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "a size that is not hex is refused"
        );
        assert!(
            upstream.is_empty(),
            "the origin was handed a size the proxy then refused: {:?}",
            String::from_utf8_lossy(&upstream)
        );
    }

    /// Every chunk's data is followed by CRLF, and those two bytes are checked
    /// rather than passed through. An origin lenient about the terminator
    /// would find the body ending somewhere the proxy does not, and the bytes
    /// in between would be a request it attributes to this client.
    #[tokio::test]
    async fn a_chunk_that_does_not_end_in_crlf_is_refused() {
        let (result, _, _) = relay_full(b"4\r\nWikiXX0\r\n\r\n", b"", BodyFraming::Chunked).await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        // The well-formed body of the same shape still goes through.
        let (got, _) = relay(b"4\r\nWiki\r\n0\r\n\r\n", b"", BodyFraming::Chunked)
            .await
            .unwrap();
        assert_eq!(got, b"4\r\nWiki\r\n0\r\n\r\n");
    }

    /// The `Content-Length` sent upstream is the number the proxy parsed, not
    /// the text it accepted. `u64::from_str` takes a leading `+`, so
    /// `Content-Length: +5` frames the body as five bytes here while an origin
    /// that refused the `+` would read no body at all and take those five
    /// bytes for a request of their own.
    #[test]
    fn the_content_length_sent_on_is_the_number_the_proxy_read() {
        for value in ["+5", "5, 5", " 5 "] {
            let h = head(&format!(
                "POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nContent-Length: {value}\r\n\r\n"
            ));
            assert_eq!(
                String::from_utf8(origin_form(&h)).unwrap(),
                "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
                "{value:?}"
            );
        }
    }

    /// `Connection` lists the headers that belong to this hop, and RFC 9110
    /// §7.6.1 says an intermediary drops every one it names. Passing them on
    /// hands the origin a header the client meant for the proxy alone.
    #[test]
    fn a_header_the_client_named_in_connection_is_hop_by_hop_too() {
        let h = head(
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close, X-Internal\r\nX-Internal: secret\r\nAccept: */*\r\n\r\n",
        );
        let out = String::from_utf8(origin_form(&h)).unwrap();
        assert!(!out.contains("X-Internal"), "{out}");
        assert!(!out.contains("secret"), "{out}");
        // Only what it named: the rest of the request is untouched.
        assert!(out.contains("Accept: */*\r\n"), "{out}");
    }

    /// An absolute-form request carries the host twice - in the URI, which is
    /// what the allowlist checked and what the proxy connected to, and in
    /// `Host`, which is what the origin routes on. They must agree, or a
    /// request cleared for one host arrives at it naming another.
    #[test]
    fn an_absolute_uri_names_upstream_the_host_it_was_checked_against() {
        let h = head(
            "GET http://example.com:8080/a HTTP/1.1\r\nHost: evil.example\r\nAccept: */*\r\n\r\n",
        );
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "GET /a HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        // Userinfo is part of the URI, not of the host being named.
        let h = head("GET http://u:p@example.com/a HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "GET /a HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        );
    }
}
