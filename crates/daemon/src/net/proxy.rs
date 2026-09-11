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
//! taken from the request line - the authority of the absolute-form URI, or
//! the authority a `CONNECT` names - checked against the workspace's live
//! allowlist, and the request is then either carried to that one host or
//! refused with a 403 that says which host and where to allow it.
//!
//! There is deliberately no fallback to the `Host` header. An origin-form
//! request (`GET /path`) has no URI authority, so it is a 400 rather than a
//! request routed by a header: `Host` is the one field a smuggled or confused
//! request can most easily disagree with the URI about, and the authority the
//! allowlist cleared must be the authority the socket was opened to. For the
//! same reason the `Host` sent upstream is rewritten from the URI rather than
//! copied from the client.
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
//! What the proxy decides lives here; what an HTTP message *is* lives in
//! [`crate::net::http`] beside it - head parsing, body framing, the origin-form
//! rewrite, and the relays that carry one request and its response. The
//! allowlist lives behind an `RwLock` so `workspace.set_allowlist` can change
//! it under running connections.

use crate::net::allowlist::Allowlist;
use crate::net::http::text_response;
// Everything else this module borrows from `http` is used by the connection
// loop, which needs a Unix socket.
#[cfg(unix)]
use crate::net::http::{
    bad_gateway, bad_request, body_framing, head_is_well_formed, origin_form, read_head,
    relay_exchange, request_timeout, target_host_port, wants_close, Relayed,
};
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
        // Before anything is decided about this request: a header carrying a
        // bare LF, a lone CR or a NUL is a second request hiding inside the
        // first, and it is refused here rather than checked and relayed. See
        // `head_is_well_formed`. Nothing has been connected yet, so the origin
        // never sees a byte of it.
        if !head_is_well_formed(&head) {
            tracing::debug!(
                ws = %ctx.workspace,
                "request head carries a line terminator or a bad field name; refused"
            );
            let _ = client.write_all(&bad_request()).await;
            return Ok(());
        }
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
        let relayed = {
            let (mut from_client, mut to_client) = client.split();
            let (mut from_upstream, mut to_upstream) = upstream.split();
            relay_exchange(
                &mut from_client,
                &mut to_client,
                &mut from_upstream,
                &mut to_upstream,
                &mut buf,
                framing,
            )
            .await?
        };
        match relayed {
            Relayed::Done => {}
            // An upstream that closed without a byte of response would leave
            // the client waiting on a connection the proxy thinks is idle.
            Relayed::NoResponse => {
                tracing::debug!(ws = %ctx.workspace, host = %host, "upstream closed without responding");
                let _ = client.write_all(&bad_gateway()).await;
                return Ok(());
            }
            // An answer only while there is still a response to be the answer
            // *to*. Once any of the upstream's response has reached the client,
            // appending a complete 400 or 408 to it is not an error message but
            // a second response spliced onto the first, and nothing on the
            // other end can tell the two apart. The connection closes either
            // way, so nothing can be confused with a later response.
            Relayed::BadBody { relayed } => {
                tracing::debug!(ws = %ctx.workspace, host = %host, relayed, "request body is malformed");
                if relayed == 0 {
                    let _ = client.write_all(&bad_request()).await;
                }
                return Ok(());
            }
            Relayed::Stalled { relayed } => {
                tracing::debug!(ws = %ctx.workspace, host = %host, relayed, "request body stalled; connection ended");
                if relayed == 0 {
                    let _ = client.write_all(&request_timeout()).await;
                }
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
