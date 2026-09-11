//! The per-workspace allowlisting HTTP proxy.
//!
//! Every workspace gets a listener of its own on `<run_dir>/proxy.sock`, which
//! the sandbox sees at `/run/bs/proxy.sock`. Nothing inside the sandbox can
//! reach the network directly — bubblewrap gives it a network namespace with
//! only loopback — so the shim ([`crate::net::shim`]) listening on
//! `127.0.0.1:3128` inside it, and forwarding to that socket, is the one way
//! out. The socket is per workspace, so a connection's workspace is known from
//! which socket it arrived on rather than from anything the client claims.
//!
//! Two shapes of request arrive here, both of them ordinary proxy traffic:
//! `CONNECT host:port` for TLS (everything an agent does), and an absolute-URI
//! request (`GET http://host/path`) for plain HTTP. Either way the host is
//! resolved from the request, checked against the workspace's live allowlist,
//! and the request is then either carried to that one host or refused with a
//! 403 that says which host and where to allow it.
//!
//! A `CONNECT` pins its connection: once the tunnel is up, everything the
//! client sends goes to the one host it was allowed. A plain-HTTP connection
//! is not one decision but a sequence of them: a keep-alive client sends its
//! next request on the same connection, and that request names its own host.
//! So plain HTTP is served one request at a time - head, check, connect,
//! relay, and round again - each request to the host *it* names, and each
//! upstream connection asked to close after its response, which is what tells
//! one response from the next without parsing every framing HTTP has. The
//! first version of this proxy piped the whole connection to the first host,
//! and a second request for another allowed host arrived there, credentials
//! and all.
//!
//! The allowlist is a list of *names*, and a name is not a destination: a
//! repository the user has not read can add `assets.example.test` to it and
//! point that name at `127.0.0.1` or `169.254.169.254`, reaching the host's own
//! services or a cloud metadata endpoint through the one hole the sandbox has.
//! So the check is made twice: the name against the allowlist, and then every
//! address it resolves to against [`is_private_addr`]. A private destination is
//! refused unless the allowlist entry is that literal address, which is the
//! only form in which a person can be said to have asked for it.
//!
//! The head parsing is pure and synchronous so it can be tested without a
//! socket; the allowlist lives behind an `RwLock` so `workspace.set_allowlist`
//! can change it under running connections.

use crate::net::allowlist::Allowlist;
use crate::server::broadcast::EventBus;
#[cfg(unix)]
use bondsymphonic_proto::Event;
use bondsymphonic_proto::{RpcError, WorkspaceId};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most of a request head the proxy will buffer before giving up.
///
/// A head that never ends is otherwise an unbounded allocation driven by the
/// sandbox, and 64 KiB is far past any real request line and header block.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// How long the head has to arrive. A connection that opens and says nothing
/// costs a task and a buffer, so it is not allowed to do so forever.
#[cfg(unix)]
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an allowed upstream has to accept the connection.
#[cfg(unix)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connections one workspace may have in flight through its proxy.
///
/// The sandbox is the untrusted side of this socket, and a task per connection
/// with nothing bounding the count lets a misbehaving agent pin the daemon that
/// also serves the IDE. Well past what a package install or a browser-shaped
/// client opens at once, and far short of anything the daemon cannot carry.
pub const MAX_CONNECTIONS: usize = 256;

/// The request line and headers of one proxied request.
///
/// `head_len` counts the blank line, so `&buf[head_len..]` is the body (or the
/// first bytes of a tunnel) the client had already sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub head_len: usize,
}

/// Parses a request head, or `None` while the blank line ending it has not
/// arrived yet.
///
/// A malformed request line is *not* reported as incomplete — that would leave
/// the caller waiting on a client that has already finished — but as a head
/// whose target [`target_host_port`] cannot resolve, which answers 400.
pub fn parse_request_head(buf: &[u8]) -> Option<RequestHead> {
    parse_request_head_from(buf, 0)
}

/// The same, resuming the search for the blank line at `from`.
///
/// A reader that appends `n` bytes and re-scans from byte zero every time is
/// quadratic in the size of the head, and the client driving it sits inside the
/// sandbox: one byte per read over a 64 KiB budget is billions of comparisons
/// for a single connection. Resuming from `len - n - 3` — three bytes back, so
/// a terminator straddling the join is still seen — makes the whole read
/// linear. `from` may safely be any value: it is clamped, and passing 0 is
/// always correct, just slower.
pub fn parse_request_head_from(buf: &[u8], from: usize) -> Option<RequestHead> {
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
pub fn target_host_port(head: &RequestHead) -> Option<(String, u16)> {
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
pub fn origin_form(head: &RequestHead) -> Vec<u8> {
    let path = match strip_http_scheme(&head.target) {
        Some(rest) => match rest.find('/') {
            Some(i) => &rest[i..],
            // `http://host` with no path at all still asks for the root.
            None => "/",
        },
        None => head.target.as_str(),
    };
    // A chunked body is walked as chunked and passed on as chunked, so a
    // `Content-Length` sitting beside it is not framing the proxy believes -
    // and must not be framing the upstream believes either. An origin that
    // read the length instead would stop short and take the rest of the body
    // for a second request to that host. RFC 9112 lets an intermediary drop
    // the length rather than refuse the whole request, which is what this does.
    let chunked = body_framing(head) == Some(BodyFraming::Chunked);
    let mut out = format!("{} {} {}\r\n", head.method, path, head.version);
    for (name, value) in &head.headers {
        // Hop-by-hop, addressed to this proxy: forwarding them would leak the
        // client's proxy credentials to the upstream server and confuse its
        // connection handling. `Connection` and `Keep-Alive` describe the
        // client's connection to the proxy, not the proxy's to the server,
        // whose lifetime is decided below.
        if is_hop_by_hop(name) {
            continue;
        }
        if chunked && name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
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
pub fn wants_close(head: &RequestHead) -> bool {
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
pub enum BodyFraming {
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
pub fn body_framing(head: &RequestHead) -> Option<BodyFraming> {
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

/// The refusal sent to a client that asked for a host the workspace may not
/// reach.
///
/// It names the host and the two places it can be allowed, and nothing else:
/// this text ends up in build logs, so it must carry no path, no workspace id
/// and no hint of what else the allowlist holds.
pub fn denied_response(host: &str) -> Vec<u8> {
    text_response(
        "403 Forbidden",
        &format!(
            "host {host} is not in this workspace's allowlist; add it to bondsymphonic.toml [network] allow or use Allow host in the IDE\n"
        ),
    )
}

/// The refusal sent to a client whose host resolved somewhere the sandbox may
/// not go, however the name got onto the allowlist.
///
/// A separate text from [`denied_response`] on purpose: "add it to the
/// allowlist" is the wrong advice here, because the name already *is* on the
/// allowlist and adding it again changes nothing.
pub fn private_response(host: &str) -> Vec<u8> {
    text_response(
        "403 Forbidden",
        &format!(
            "host {host} resolves to a private address; this workspace may not reach loopback, link-local, private or unique-local networks. Allow the address itself, written out, if that is really what was meant\n"
        ),
    )
}

/// An IPv4-mapped IPv6 address as the IPv4 address it is.
///
/// `::ffff:127.0.0.1` is loopback whichever family it is written in, and a
/// classifier that only looked at the v6 form would call it public.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// Whether `addr` is somewhere a sandboxed workspace has no business reaching
/// through its proxy: the host's own loopback, the machine's private network,
/// the link-local range that carries the cloud metadata endpoint
/// (`169.254.169.254`), a range that is routed only inside some operator's
/// network, or an address that is not a destination at all.
///
/// The proxy exists to let the sandbox reach *the internet* under an allowlist.
/// Everything here is on this side of the boundary the sandbox was built to
/// keep it behind, so a name resolving to one of these is treated as the
/// attempt to cross it that it is - unless the allowlist entry is that literal
/// address, which only a person can have written.
///
/// The ranges, by family. IPv4: `0/8` (this network; Linux routes it to the
/// host itself), `127/8`, `10/8`, `172.16/12`, `192.168/16`, `169.254/16`,
/// `100.64/10` (carrier-grade NAT: the operator's network, not the internet),
/// `192.0.0/24` (IETF protocol assignments, `192.0.0.8` among them),
/// `198.18/15` (benchmarking), `224/4` (multicast) and `240/4` (reserved,
/// which includes broadcast). IPv6: `::/96` (loopback, the unspecified
/// address, and the withdrawn IPv4-compatible spelling of an IPv4 address),
/// `fc00::/7`, `fe80::/10`, `ff00::/8`, and `64:ff9b::/96`, the NAT64 prefix,
/// which names an IPv4 destination through a translator on the local network.
/// An IPv4-mapped address (`::ffff:a.b.c.d`) is classified as the IPv4 address
/// it is.
pub fn is_private_addr(addr: &IpAddr) -> bool {
    match canonical_ip(*addr) {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                // This network, 0.0.0.0/8.
                || a == 0
                // Carrier-grade NAT, 100.64.0.0/10.
                || (a == 100 && (b & 0xc0) == 64)
                // IETF protocol assignments, 192.0.0.0/24.
                || (a == 192 && b == 0 && c == 0)
                // Benchmarking, 198.18.0.0/15.
                || (a == 198 && (b & 0xfe) == 18)
                // Reserved, 240.0.0.0/4, broadcast included.
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let head = seg[0];
            v6.is_multicast()
                // Unique local, fc00::/7 - the IPv6 answer to 10/8.
                || (head & 0xfe00) == 0xfc00
                // Link local, fe80::/10.
                || (head & 0xffc0) == 0xfe80
                // `::/96`: loopback (`::1`), the unspecified address, and the
                // withdrawn IPv4-compatible spelling of an IPv4 address
                // (`::127.0.0.1`), which no stack routes and which a
                // classifier that only unmaps `::ffff:` would call public.
                || seg[..6] == [0, 0, 0, 0, 0, 0]
                // NAT64, 64:ff9b::/96.
                || seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
        }
    }
}

/// Whether `host` is a plain ASCII hostname or an IP literal, and so fit to be
/// published as a denial.
///
/// A denied host travels into the IDE's toast and, one click later, into the
/// workspace's allowlist. The sandbox chooses that text: `CONNECT *.com:443`
/// would otherwise offer "Allow host *.com". Labels are 1-63 bytes of letters,
/// digits and hyphens, the whole name at most 253; anything else is a malformed
/// request rather than a denial.
pub fn is_valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    // The trailing dot of a fully qualified name is the root label, not an
    // empty one.
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// How long one workspace's denial of one host stays quiet after it has been
/// reported.
///
/// A page in the sandbox fetching a blocked host in a loop would otherwise put
/// one `daemon.log` event on the bus per request, which is an unbounded stream
/// the IDE has to queue and the user cannot answer. The 403 is still sent every
/// time; only the *notice* is coalesced.
pub const DENIAL_INTERVAL: Duration = Duration::from_secs(5);

/// How many (workspace, host) pairs the gate remembers before it prunes.
const DENIAL_GATE_CAP: usize = 1024;

/// Remembers which denials have already been reported, so a flood becomes one
/// notice per host per [`DENIAL_INTERVAL`].
#[derive(Default)]
pub struct DenialGate {
    seen: Mutex<HashMap<(WorkspaceId, String), Instant>>,
}

impl DenialGate {
    /// Whether this denial should be reported now, remembering it if so.
    pub fn admit(&self, ws: &WorkspaceId, host: &str) -> bool {
        self.admit_at(ws, host, Instant::now())
    }

    /// The same at a caller-chosen instant, so the interval can be tested
    /// without sleeping through it.
    pub fn admit_at(&self, ws: &WorkspaceId, host: &str, now: Instant) -> bool {
        let mut seen = self.seen.lock();
        let key = (ws.clone(), host.to_string());
        if let Some(at) = seen.get(&key) {
            if now.saturating_duration_since(*at) < DENIAL_INTERVAL {
                return false;
            }
        }
        // Pruning only at the cap, rather than on every denial, keeps a flood
        // of *distinct* hosts linear instead of quadratic.
        if seen.len() >= DENIAL_GATE_CAP {
            seen.retain(|_, at| now.saturating_duration_since(*at) < DENIAL_INTERVAL);
            if seen.len() >= DENIAL_GATE_CAP {
                // Every entry is still live: the sandbox is naming a thousand
                // hosts a second, and remembering more of them costs more than
                // the coalescing saves.
                seen.clear();
            }
        }
        seen.insert(key, now);
        true
    }
}

/// A complete, self-describing plain-text response. Built rather than written
/// out, so a `Content-Length` can never drift away from its body.
fn text_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Sent when the request is not one a proxy can serve at all.
#[cfg(unix)]
fn bad_request() -> Vec<u8> {
    text_response(
        "400 Bad Request",
        "not a proxy request this daemon can serve\n",
    )
}

/// Sent when an allowed host would not take the connection. The reason stays in
/// the daemon's log: the client is inside the sandbox.
#[cfg(unix)]
fn bad_gateway() -> Vec<u8> {
    text_response("502 Bad Gateway", "could not connect to the host\n")
}

/// One workspace's live listener.
struct Entry {
    /// Swapped under running connections by `workspace.set_allowlist`.
    allow: Arc<RwLock<Allowlist>>,
    socket: PathBuf,
    /// Which call to [`ProxyRegistry::start`] this listener belongs to. A
    /// workspace whose sandbox dies and is restarted has a watcher for each
    /// start, and only the one whose generation still matches may stop it.
    generation: u64,
    /// Ends the accept loop *and* every connection it started, so a workspace
    /// that goes away takes its open tunnels with it.
    cancel: tokio_util::sync::CancellationToken,
}

/// The proxies, one per live workspace.
#[derive(Default)]
pub struct ProxyRegistry {
    entries: Mutex<HashMap<WorkspaceId, Entry>>,
    /// Handed out by [`ProxyRegistry::start`], never reused.
    next_generation: std::sync::atomic::AtomicU64,
    /// One per daemon, keyed by workspace *and* host, so a workspace cannot
    /// drown the event bus in denials of the same host. Only the connection
    /// path reads it, and that path needs a Unix socket.
    #[cfg_attr(not(unix), allow(dead_code))]
    denials: Arc<DenialGate>,
}

/// What one connection needs to decide where it may go and to say so.
#[cfg(unix)]
#[derive(Clone)]
struct ConnCtx {
    allow: Arc<RwLock<Allowlist>>,
    events: EventBus,
    workspace: WorkspaceId,
    /// Shared with every other workspace's connections, so one host denied in a
    /// loop is one notice per [`DENIAL_INTERVAL`] rather than one per request.
    denials: Arc<DenialGate>,
}

impl ProxyRegistry {
    /// Starts (or, for a workspace that already has one, refreshes) the proxy
    /// listening on `socket`.
    ///
    /// Called before the sandbox starts, so the socket is already there when
    /// the shim inside it connects. Returns the generation of the listener the
    /// caller now owns, for [`ProxyRegistry::stop_generation`].
    pub async fn start(
        &self,
        id: &WorkspaceId,
        socket: &Path,
        allow: Allowlist,
        events: EventBus,
    ) -> Result<u64, RpcError> {
        self.start_now(id, socket, allow, events)
    }

    #[cfg(unix)]
    fn start_now(
        &self,
        id: &WorkspaceId,
        socket: &Path,
        allow: Allowlist,
        events: EventBus,
    ) -> Result<u64, RpcError> {
        // One lock for the whole thing: nothing in here awaits, and a second
        // `start` for the same workspace must not be able to bind over a live
        // listener between the lookup and the insert.
        let mut entries = self.entries.lock();
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(existing) = entries.get_mut(id) {
            // A listener on the path the caller asked for is the one it wanted,
            // whoever started it; it takes over the new allowlist and the new
            // generation, so the previous owner's watcher can no longer stop it.
            if existing.socket.as_path() == socket {
                *existing.allow.write() = allow;
                existing.generation = generation;
                return Ok(generation);
            }
            // A different path means this is a different listener. Silently
            // handing back the old one would leave the caller's socket file
            // missing and its sandbox with no way out.
            tracing::warn!(
                ws = %id,
                old = %existing.socket.display(),
                new = %socket.display(),
                "the workspace proxy moved; replacing the listener"
            );
            let old = entries.remove(id).expect("just looked it up");
            old.cancel.cancel();
            let _ = std::fs::remove_file(&old.socket);
        }
        // A socket file left by a daemon that did not exit cleanly would make
        // the bind fail; nothing else may live at this path.
        let _ = std::fs::remove_file(socket);
        let listener = tokio::net::UnixListener::bind(socket).map_err(|e| RpcError::io(&e))?;
        // The socket is this workspace's way out, so nobody else on the host
        // gets to borrow it: bind leaves it world-connectable under the usual
        // umask. The sandbox runs as the daemon's own user, so 0600 is no
        // obstacle to the shim.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
        }
        let allow = Arc::new(RwLock::new(allow));
        let cancel = tokio_util::sync::CancellationToken::new();
        let ctx = ConnCtx {
            allow: allow.clone(),
            events,
            workspace: id.clone(),
            denials: self.denials.clone(),
        };
        tokio::spawn(accept_loop(listener, ctx, cancel.clone()));
        entries.insert(
            id.clone(),
            Entry {
                allow,
                socket: socket.to_path_buf(),
                generation,
                cancel,
            },
        );
        tracing::debug!(ws = %id, socket = %socket.display(), "workspace proxy listening");
        Ok(generation)
    }

    /// Without Unix sockets there is no sandbox to proxy for either, so this is
    /// a no-op rather than an error: the daemon still runs on Windows for the
    /// tests, over the no-sandbox backend.
    #[cfg(not(unix))]
    fn start_now(
        &self,
        _id: &WorkspaceId,
        _socket: &Path,
        _allow: Allowlist,
        _events: EventBus,
    ) -> Result<u64, RpcError> {
        tracing::debug!("the workspace proxy needs a unix socket; not started on this host");
        Ok(self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }

    /// Replaces the live allowlist. A tunnel already established keeps the
    /// host it was allowed to: it is pinned to one upstream, so nothing it can
    /// do afterwards reaches anywhere else. A plain-HTTP connection's next
    /// request is checked against the new list.
    pub fn set_allowlist(&self, id: &WorkspaceId, allow: Allowlist) {
        if let Some(entry) = self.entries.lock().get(id) {
            *entry.allow.write() = allow;
        }
    }

    /// Ends the listener, its connections and the socket file. For a workspace
    /// that is going away, where whatever is listening should stop whoever
    /// started it.
    pub fn stop(&self, id: &WorkspaceId) {
        let Some(entry) = self.entries.lock().remove(id) else {
            return;
        };
        entry.cancel.cancel();
        let _ = std::fs::remove_file(&entry.socket);
    }

    /// The same, but only for the listener [`ProxyRegistry::start`] handed back
    /// this `generation`.
    ///
    /// A sandbox that dies while a restart is already under way has a watcher
    /// holding a stale view of the workspace: without this it would cancel the
    /// listener the *new* sandbox is about to use and unlink its socket, and
    /// the workspace would come up Ready with no way out. Mirrors the
    /// `Arc::ptr_eq` check the sandbox map gets for the same reason.
    pub fn stop_generation(&self, id: &WorkspaceId, generation: u64) {
        let mut entries = self.entries.lock();
        if entries.get(id).is_none_or(|e| e.generation != generation) {
            return;
        }
        let entry = entries.remove(id).expect("just looked it up");
        drop(entries);
        entry.cancel.cancel();
        let _ = std::fs::remove_file(&entry.socket);
    }
}

#[cfg(unix)]
async fn accept_loop(
    listener: tokio::net::UnixListener,
    ctx: ConnCtx,
    cancel: tokio_util::sync::CancellationToken,
) {
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        // The permit is taken *before* accepting, so a workspace that has run
        // out simply stops taking connections off the socket. The kernel's
        // backlog then holds them, which is back-pressure rather than a refusal
        // the client would have to understand.
        if limit.available_permits() == 0 {
            tracing::warn!(
                ws = %ctx.workspace,
                limit = MAX_CONNECTIONS,
                "proxy connection limit reached; further connections wait"
            );
        }
        let permit = tokio::select! {
            _ = cancel.cancelled() => return,
            p = Arc::clone(&limit).acquire_owned() => match p {
                Ok(p) => p,
                // Only when the semaphore is closed, which nothing does.
                Err(_) => return,
            },
        };
        let accepted = tokio::select! {
            _ = cancel.cancelled() => return,
            a = listener.accept() => a,
        };
        match accepted {
            Ok((stream, _)) => {
                let ctx = ctx.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    // Held for the life of the connection, so the count falls
                    // again however this task ends.
                    let _permit = permit;
                    // No timeout on the connection as a whole: a tunnel may
                    // legitimately be a WebSocket that lives for hours. Only
                    // the head, before anything is allowed, is on a clock.
                    let served = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = serve(stream, &ctx) => r,
                    };
                    if let Err(e) = served {
                        // A client that hangs up mid-transfer is ordinary.
                        tracing::debug!(ws = %ctx.workspace, "proxy connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(ws = %ctx.workspace, "proxy accept failed: {e}");
                // Descriptor exhaustion is transient; spinning on it is not.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Reads until the head is complete. `None` means the client hung up first or
/// sent more than [`MAX_HEAD_BYTES`] without finishing.
#[cfg(unix)]
async fn read_head(
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

/// Refuses one connection: warns, publishes the denial unless this host has
/// already been reported for this workspace inside [`DENIAL_INTERVAL`], and
/// writes `body`.
///
/// The 403 is always sent; only the notice is coalesced. A page fetching a
/// blocked host in a loop must get its refusal every time - it is what makes
/// the failure legible in the build log - while the user gets one toast.
#[cfg(unix)]
async fn refuse(
    client: &mut tokio::net::UnixStream,
    ctx: &ConnCtx,
    host: &str,
    body: Vec<u8>,
    why: &str,
) {
    use tokio::io::AsyncWriteExt;
    if ctx.denials.admit(&ctx.workspace, host) {
        tracing::warn!(ws = %ctx.workspace, host = %host, "{why}");
        // Published before the refusal is written, so a client that reacts to
        // the 403 by asking the daemon something cannot outrun its own event.
        ctx.events
            .publish(Some(ctx.workspace.clone()), Event::network_denied(host));
    } else {
        tracing::debug!(ws = %ctx.workspace, host = %host, "{why} (already reported)");
    }
    let _ = client.write_all(&body).await;
}

/// One client connection, from its first request to its last.
///
/// A `CONNECT` becomes a tunnel to the one host it named and this returns when
/// the tunnel ends. Plain HTTP loops: one request at a time, each checked
/// against the allowlist and carried to the host *it* names on an upstream
/// connection of its own, until the client closes, asks to close, or sends
/// something that cannot be served.
#[cfg(unix)]
async fn serve(mut client: tokio::net::UnixStream, ctx: &ConnCtx) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut buf = Vec::with_capacity(4096);
    let mut first = true;
    loop {
        let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client, &mut buf)).await
        {
            Ok(Ok(Some(head))) => head,
            // A kept-alive client that closes, or falls silent, between
            // requests is finished rather than malformed.
            Ok(Ok(None)) | Err(_) if !first && buf.is_empty() => return Ok(()),
            // Nothing usable arrived: an empty connection, an oversized
            // head, or a client that took longer than the timeout to say
            // anything.
            Ok(Ok(None)) | Err(_) => {
                let _ = client.write_all(&bad_request()).await;
                return Ok(());
            }
            Ok(Err(e)) => return Err(e),
        };
        first = false;
        let Some((host, port)) = target_host_port(&head) else {
            let _ = client.write_all(&bad_request()).await;
            return Ok(());
        };
        // One spelling for the check, the denial and the connect: `GitHub.com.`
        // is `github.com`, and the entry a denial's one-click Allow writes back
        // has to be the one the next request matches.
        let host = crate::net::allowlist::normalize_host(&host);
        // The sandbox writes this text, and a denial carries it into the IDE's
        // toast and from there into the workspace allowlist. `CONNECT *.com:443`
        // is a malformed request, not a denial: answering 400 keeps it out of
        // the event stream entirely.
        if !is_valid_host(&host) {
            tracing::debug!(
                ws = %ctx.workspace,
                "proxy target is not a hostname or an address; refused"
            );
            let _ = client.write_all(&bad_request()).await;
            return Ok(());
        }
        let Some(mut upstream) = open_upstream(&mut client, ctx, &host, port).await? else {
            // Refused or unreachable; the answer has been written and says
            // `Connection: close`.
            return Ok(());
        };
        // Whatever followed the head is already in hand: a TLS ClientHello sent
        // without waiting for the tunnel's 200, a request body, or the next
        // request of a pipelining client.
        buf.drain(..head.head_len);
        if head.method.eq_ignore_ascii_case("CONNECT") {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            if !buf.is_empty() {
                upstream.write_all(&buf).await?;
            }
            // From here the connection is pinned to the one host it was
            // allowed: anything else the client sends still goes there.
            tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
            return Ok(());
        }
        let Some(framing) = body_framing(&head) else {
            tracing::debug!(ws = %ctx.workspace, host = %host, "request body framing is not usable; refused");
            let _ = client.write_all(&bad_request()).await;
            return Ok(());
        };
        upstream.write_all(&origin_form(&head)).await?;
        // The body goes up and the response comes down at the same time: a
        // server may answer (a `100 Continue`, or a refusal) before it has
        // read the body, and a client waiting on that answer would otherwise
        // never send it. The upstream was asked to close after its response,
        // so its end of stream is the end of the response.
        let (mut from_client, mut to_client) = client.split();
        let (mut from_upstream, mut to_upstream) = upstream.split();
        let relayed = tokio::try_join!(
            relay_body(&mut from_client, &mut to_upstream, &mut buf, framing),
            tokio::io::copy(&mut from_upstream, &mut to_client),
        );
        match relayed {
            // An upstream that closed without a byte of response would leave
            // the client waiting on a connection the proxy thinks is idle.
            Ok((_, 0)) => {
                tracing::debug!(ws = %ctx.workspace, host = %host, "upstream closed without responding");
                let _ = client.write_all(&bad_gateway()).await;
                return Ok(());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::debug!(ws = %ctx.workspace, host = %host, "request body is malformed: {e}");
                let _ = client.write_all(&bad_request()).await;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        drop(upstream);
        if wants_close(&head) {
            client.shutdown().await?;
            return Ok(());
        }
    }
}

/// The upstream connection for one request, or `None` once the client has
/// been answered with why there is not going to be one: the host is not on the
/// allowlist, it resolves only to private addresses, or it would not take the
/// connection.
///
/// One function for both `CONNECT` and plain HTTP, so there is exactly one
/// place a host is let through.
#[cfg(unix)]
async fn open_upstream(
    client: &mut tokio::net::UnixStream,
    ctx: &ConnCtx,
    host: &str,
    port: u16,
) -> std::io::Result<Option<tokio::net::TcpStream>> {
    use tokio::io::AsyncWriteExt;
    // The guard is a temporary of this statement alone: an `RwLock` read held
    // across the awaits below would block every `set_allowlist` behind a tunnel.
    let allowed = ctx.allow.read().allows(host);
    if !allowed {
        refuse(client, ctx, host, denied_response(host), "network denied").await;
        return Ok(None);
    }
    // Resolved here rather than inside `connect`, because what the allowlist
    // cleared was a *name*: the destination it stands for is checked next, and
    // a name that resolves nowhere is a gateway failure like any other.
    let resolved: Vec<std::net::SocketAddr> = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(addrs)) => addrs.collect(),
        // The reason stays in the daemon log: the client is inside the sandbox
        // and has no business learning about the host's DNS or routing.
        Ok(Err(e)) => {
            tracing::debug!(ws = %ctx.workspace, host = %host, port, "upstream resolve failed: {e}");
            let _ = client.write_all(&bad_gateway()).await;
            return Ok(None);
        }
        Err(_) => {
            tracing::debug!(ws = %ctx.workspace, host = %host, port, "upstream resolve timed out");
            let _ = client.write_all(&bad_gateway()).await;
            return Ok(None);
        }
    };
    // A private destination is refused whatever name led to it, unless the
    // allowlist entry is that literal address: a repository can add a *name* to
    // the list at creation, but only a person writes `127.0.0.1` down.
    let reachable: Vec<std::net::SocketAddr> = {
        let allow = ctx.allow.read();
        resolved
            .iter()
            .copied()
            .filter(|a| {
                !is_private_addr(&a.ip()) || allow.allows_literal_addr(&canonical_ip(a.ip()))
            })
            .collect()
    };
    if reachable.is_empty() {
        refuse(
            client,
            ctx,
            host,
            private_response(host),
            "network denied: private destination",
        )
        .await;
        return Ok(None);
    }
    // One deadline over the whole list, not one per address: a name with four
    // addresses behind it must not cost four times the budget, which is what
    // `TcpStream::connect(host)` used to give without being told.
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    let mut connected = None;
    for addr in &reachable {
        match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                connected = Some(s);
                break;
            }
            Ok(Err(e)) => {
                tracing::debug!(ws = %ctx.workspace, host = %host, port, "upstream connect failed: {e}")
            }
            Err(_) => {
                tracing::debug!(ws = %ctx.workspace, host = %host, port, "upstream connect timed out")
            }
        }
    }
    match connected {
        Some(upstream) => Ok(Some(upstream)),
        None => {
            let _ = client.write_all(&bad_gateway()).await;
            Ok(None)
        }
    }
}

/// The longest line the chunked-body relay will buffer looking for its end:
/// a chunk-size line or a trailer. Real ones are a few bytes.
const MAX_LINE_BYTES: usize = 8 * 1024;

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
pub async fn relay_body<R, W>(
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
                upstream.write_all(&line).await?;
                let size = chunk_size(&line)?;
                if size == 0 {
                    break;
                }
                // The chunk's data and the CRLF that ends it.
                let with_crlf = size.checked_add(2).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size overflows")
                })?;
                relay_exact(client, upstream, buf, with_crlf).await?;
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
        parse_request_head(text.as_bytes()).expect("a complete head")
    }

    /// The head is parsed off a stream, so it must say "not yet" rather than
    /// guess: a request line without its blank line may still be growing.
    #[test]
    fn a_head_parses_only_once_the_blank_line_has_arrived() {
        assert!(parse_request_head(b"GET http://a/ HTTP/1.1\r\nHost: a\r\n").is_none());
        assert!(parse_request_head(b"CONNECT a:443 HTTP/1.1\r\n").is_none());
        assert!(parse_request_head(b"").is_none());
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
        // request on the client's connection can be checked on its own.
        assert_eq!(
            String::from_utf8(origin_form(&h)).unwrap(),
            "GET /a/b?c=d HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nConnection: close\r\n\r\n"
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

    /// The refusal has to tell whoever reads the build log which host was
    /// refused and where to allow it — and nothing else about the daemon.
    #[test]
    fn the_denial_names_the_host_and_where_to_allow_it() {
        let text = String::from_utf8(denied_response("evil.example")).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
        assert!(head.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{head}");
        assert!(head.contains("Content-Type: text/plain"), "{head}");
        assert!(
            head.contains(&format!("Content-Length: {}", body.len())),
            "{head}"
        );
        assert!(body.contains("evil.example"), "{body}");
        assert!(body.contains("bondsymphonic.toml"), "{body}");
        assert!(body.contains("[network] allow"), "{body}");
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
        assert_eq!(parse_request_head(text.as_bytes()), Some(head));
    }

    /// A client that never sends the blank line gets no request out of the
    /// proxy however much it sends; the read loop stops it at the cap.
    #[test]
    fn a_head_that_never_ends_is_never_parsed() {
        let junk = vec![b'x'; MAX_HEAD_BYTES + 1];
        assert!(parse_request_head(&junk).is_none());
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("an address")
    }

    /// The whole point of the sandbox is that what runs in it cannot reach the
    /// host. Every address family's way of naming "here" or "this network" has
    /// to be recognised, or the proxy is the way back in.
    #[test]
    fn every_private_destination_is_recognised_as_one() {
        for private in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            // The cloud metadata endpoint, and the link-local range it sits in.
            "169.254.169.254",
            "169.254.0.1",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            // An IPv4 address wearing an IPv6 coat is still that address.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_addr(&ip(private)), "{private} must be refused");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            // Just outside 172.16/12 on either side.
            "172.15.0.1",
            "172.32.0.1",
            "192.167.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private_addr(&ip(public)), "{public} must be allowed");
        }
    }

    /// Writing an address down is how a person says they meant it. A *name*
    /// that resolves there - which a repository can add to the list at creation
    /// without anyone reading it - is not the same permission.
    #[test]
    fn a_literal_address_entry_permits_that_address_and_nothing_else() {
        let list = Allowlist::from_strings(&["127.0.0.1".to_string(), "localhost".to_string()]);
        // The integration test's own case: `["127.0.0.1"]` still reaches the
        // local server.
        let permitted = |addr: &str| {
            let a = ip(addr);
            !is_private_addr(&a) || list.allows_literal_addr(&canonical_ip(a))
        };
        assert!(permitted("127.0.0.1"));
        assert!(
            permitted("::ffff:127.0.0.1"),
            "the mapped form is the same address"
        );
        assert!(permitted("8.8.8.8"));
        // `localhost` is on the list as a name, and resolves to both of these.
        assert!(!permitted("::1"));
        assert!(!permitted("127.0.0.2"));
        assert!(!permitted("169.254.169.254"));
        // An empty list permits nothing private, and everything public.
        let none = Allowlist::default();
        assert!(!none.allows_literal_addr(&ip("127.0.0.1")));
    }

    /// The denied host is text the sandbox chose, and one click turns it into
    /// an allowlist entry. Only a plain hostname or an address may get that far.
    #[test]
    fn only_a_hostname_or_an_address_may_be_published_as_a_denial() {
        for ok in [
            "github.com",
            "GitHub.com",
            "a",
            "a-b.example.com",
            "example.com.",
            "127.0.0.1",
            "::1",
            &"a".repeat(63),
        ] {
            assert!(is_valid_host(ok), "{ok} is a host");
        }
        for bad in [
            "",
            // The wildcard that would become an allowlist entry.
            "*.com",
            "*",
            "a_b.example.com",
            "a b.com",
            "a/b",
            "a..b",
            ".a",
            "exämple.com",
            "a\u{0}b",
            // Labels are at most 63 bytes, the name at most 253.
            &"a".repeat(64),
            &vec!["a"; 200].join("."),
        ] {
            assert!(!is_valid_host(bad), "{bad:?} is not a host");
        }
        // 253 is the limit, not 254.
        let label = "a".repeat(63);
        let long = format!("{label}.{label}.{label}.{}", "a".repeat(61));
        assert_eq!(long.len(), 253);
        assert!(is_valid_host(&long));
        assert!(!is_valid_host(&format!("a{long}")));
    }

    /// A page in the sandbox fetching a blocked host in a loop is one notice,
    /// not one per request - and a *different* host is still its own notice.
    #[test]
    fn repeated_denials_of_one_host_are_coalesced_per_workspace() {
        let gate = DenialGate::default();
        let a = WorkspaceId("ws_a".into());
        let b = WorkspaceId("ws_b".into());
        let t0 = Instant::now();
        assert!(gate.admit_at(&a, "evil.example", t0));
        assert!(!gate.admit_at(&a, "evil.example", t0));
        assert!(!gate.admit_at(
            &a,
            "evil.example",
            t0 + DENIAL_INTERVAL - Duration::from_millis(1)
        ));
        // Another host, and the same host in another workspace, are their own
        // notices: the user has a separate decision to make about each.
        assert!(gate.admit_at(&a, "other.example", t0));
        assert!(gate.admit_at(&b, "evil.example", t0));
        // Once the interval is past, the host is reported again: a denial the
        // user dismissed and then hit again has to be visible.
        assert!(gate.admit_at(&a, "evil.example", t0 + DENIAL_INTERVAL));
        assert!(!gate.admit_at(&a, "evil.example", t0 + DENIAL_INTERVAL));
    }

    /// A flood of *distinct* hosts must not grow the gate without bound.
    #[test]
    fn the_denial_gate_is_bounded() {
        let gate = DenialGate::default();
        let ws = WorkspaceId("ws_a".into());
        let t0 = Instant::now();
        for i in 0..(DENIAL_GATE_CAP * 3) {
            assert!(gate.admit_at(&ws, &format!("h{i}.example"), t0));
        }
        assert!(gate.seen.lock().len() <= DENIAL_GATE_CAP);
    }

    /// The private-destination refusal has to say what happened, and must not
    /// repeat the "add it to the allowlist" advice: the name is already on it.
    #[test]
    fn the_private_refusal_names_the_host_and_the_reason() {
        let text = String::from_utf8(private_response("assets.example.test")).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
        assert!(head.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{head}");
        assert!(
            head.contains(&format!("Content-Length: {}", body.len())),
            "{head}"
        );
        assert!(body.contains("assets.example.test"), "{body}");
        assert!(body.contains("private address"), "{body}");
    }

    /// The ranges that are not "the internet" either but that the first list
    /// missed: carrier-grade NAT, the IETF protocol-assignment block, the
    /// benchmarking range, the reserved class E block, and the NAT64 prefix
    /// through which an IPv6-only host names an IPv4 destination.
    #[test]
    fn the_wider_private_ranges_are_recognised_too() {
        for private in [
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "64:ff9b::1.2.3.4",
            "64:ff9b::7f00:1",
            // IPv4-compatible IPv6, `::/96`: the withdrawn spelling of an
            // IPv4 address, `::127.0.0.1` among them, which no classifier
            // that only knows `::ffff:` recognises.
            "::1.2.3.4",
            "::7f00:1",
            "::ffff:10.0.0.1",
            "::ffff:100.64.0.1",
        ] {
            assert!(is_private_addr(&ip(private)), "{private} must be refused");
        }
        for public in [
            "8.8.8.8",
            "2606:4700::1111",
            // Just outside each new range.
            "100.63.255.255",
            "100.128.0.0",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.0",
            // Below 224/4 (multicast) and 240/4 alike.
            "223.255.255.255",
            "64:ff9c::1",
        ] {
            assert!(!is_private_addr(&ip(public)), "{public} must be allowed");
        }
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
        result.map(|()| (received, buf))
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
}
