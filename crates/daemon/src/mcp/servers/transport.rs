//! Streamable HTTP for remote MCP servers: a bounded, redirect-free HTTP
//! client on the host `curl`, an event-stream decoder, and [`RemoteSession`],
//! one independent MCP session against one configured server.
//!
//! The daemon has no TLS stack of its own, so HTTPS is the distro's `curl`,
//! hardened like `agents/opencode_providers.rs`: `-q` first (no `~/.curlrc`),
//! no redirects, `https` only, bounded size and time, the address pinned with
//! `--resolve` to the one the private-address check admitted. Nothing secret
//! reaches its argv: the URL and the headers (credentials included) go in a
//! config read from stdin (`--config -`), and a request body in a `0600` temp
//! file passed as `--data-binary @<path>`, deleted when the request ends.
use super::McpServers;
use async_trait::async_trait;
use bondsymphonic_proto::{McpServerId, McpTransportConfig, RpcError};
use serde_json::{json, Value};
use std::{
    io::Write,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// The bound on any request or response body, and curl's `--max-filesize`.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const MAX_HEADERS: usize = 32 * 1024;
pub const PROTOCOL_VERSION: &str = "2025-11-25";
/// Streamable HTTP protocol revisions this client speaks.
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-03-26", "2025-06-18", PROTOCOL_VERSION];
/// curl's `--max-time` for an ordinary request, and for a `POST` whose reply
/// may be a long-running tool call streamed as events.
const MAX_TIME: Duration = Duration::from_secs(60);
const STREAM_MAX_TIME: Duration = Duration::from_secs(600);
/// Slack over curl's own deadline before the daemon gives up on the process.
const GRACE: Duration = Duration::from_secs(5);

/// Every transport and OAuth failure is a fixed string: upstream bodies,
/// headers and URLs may carry credentials and never reach an error.
pub(crate) fn error(reason: &str) -> RpcError {
    RpcError::invalid_params(format!("MCP {reason}"))
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpRequest [REDACTED]")
    }
}

#[derive(Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[async_trait]
pub trait McpEventHandler: Send + Sync {
    /// Called for a server request (`method` and `id`) inside a streamed
    /// reply. Returns true once it has answered and consumed it.
    async fn handle_request(
        &self,
        message: &Value,
        headers: &[(String, String)],
    ) -> Result<bool, RpcError>;
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn send(
        &self,
        req: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError>;

    /// An MCP `POST`. With `id`, the reply may be an event stream the server
    /// keeps open after answering, so the client must finish at the event
    /// that answers `id`; and the server may wait on a request of its own
    /// (a `ping`) before it answers, so those go to `handler` as they arrive.
    /// This default buffers the whole response, which suits fakes and finite
    /// streams.
    async fn send_mcp(
        &self,
        req: HttpRequest,
        id: Option<Value>,
        handler: &dyn McpEventHandler,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        let _ = id;
        let mut response = self.send(req, cancel).await?;
        if is_event_stream(&response) {
            let mut decoder = SseDecoder::default();
            let messages = decoder.push(&response.body)?;
            decoder.finish()?;
            let mut remaining = vec![];
            for message in messages {
                validate_message(&message)?;
                if message.get("method").is_some()
                    && message.get("id").is_some()
                    && handler.handle_request(&message, &response.headers).await?
                {
                    continue;
                }
                remaining.push(message);
            }
            response.body = encode_events(remaining)?;
        }
        Ok(response)
    }
}

/// An upstream URL the daemon may contact: `https://` to a public address,
/// or loopback (plain or not) behind the test switch `BS_MCP_ALLOW_HTTP=1`.
#[derive(Clone, Debug)]
pub struct HttpsUrl {
    pub host: String,
    pub port: u16,
    pub origin: String,
    pub path: String,
    pub value: String,
    /// A loopback host admitted by the test switch: no DNS, and curl may use
    /// plain `http` (ruling R1).
    pub loopback: bool,
}
impl HttpsUrl {
    pub fn parse(value: &str) -> Result<Self, RpcError> {
        Self::parse_with(value, super::config::loopback_http_allowed())
    }

    /// [`HttpsUrl::parse`] with the test switch given rather than read.
    pub fn parse_with(value: &str, allow_loopback_http: bool) -> Result<Self, RpcError> {
        if value.len() > 8192
            || value.chars().any(|c| c.is_control() || c == '\\')
            || value.contains('#')
        {
            return Err(error("invalid HTTPS URL"));
        }
        let (tail, plain) = match (
            value.strip_prefix("https://"),
            value.strip_prefix("http://"),
        ) {
            (Some(tail), _) => (tail, false),
            (None, Some(tail)) if allow_loopback_http => (tail, true),
            _ => return Err(error("HTTPS required")),
        };
        let split = tail.find(['/', '?']).unwrap_or(tail.len());
        let authority = &tail[..split];
        if authority.is_empty() || authority.contains('@') || authority.contains('%') {
            return Err(error("invalid HTTPS authority"));
        }
        let default_port = if plain { 80 } else { 443 };
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']').ok_or_else(|| error("invalid host"))?;
            let host = &authority[1..end];
            let rest = &authority[end + 1..];
            (
                host.to_string(),
                if rest.is_empty() {
                    default_port
                } else {
                    rest.strip_prefix(':')
                        .ok_or_else(|| error("invalid port"))?
                        .parse()
                        .map_err(|_| error("invalid port"))?
                },
            )
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| error("invalid port"))?),
                None => (authority.to_string(), default_port),
            }
        };
        if port == 0
            || host.is_empty()
            || host.len() > 253
            || (!host.contains(':')
                && host.split('.').any(|s| {
                    s.is_empty()
                        || s.starts_with('-')
                        || s.ends_with('-')
                        || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }))
        {
            return Err(error("invalid host"));
        }
        let host = host.to_ascii_lowercase();
        let loopback = allow_loopback_http && is_loopback_host(&host);
        if plain && !loopback {
            return Err(error("HTTPS required"));
        }
        if !loopback {
            if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
                return Err(error("private address denied"));
            }
            if let Ok(ip) = host.parse::<IpAddr>() {
                if !public_ip(ip) {
                    return Err(error("private address denied"));
                }
            }
        }
        let origin = format!(
            "{}://{}{}",
            if plain { "http" } else { "https" },
            if host.contains(':') {
                format!("[{host}]")
            } else {
                host.clone()
            },
            if port == default_port {
                String::new()
            } else {
                format!(":{port}")
            }
        );
        let path = if split == tail.len() {
            "/".to_string()
        } else if tail.as_bytes()[split] == b'?' {
            format!("/{}", &tail[split..])
        } else {
            tail[split..].to_string()
        };
        Ok(Self {
            host,
            port,
            origin,
            path,
            value: value.into(),
            loopback,
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || o[0] == 192 && o[1] == 0
                || o[0] == 198 && (o[1] == 18 || o[1] == 19)
                || o[0] == 100 && (64..=127).contains(&o[1]))
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return public_ip(v4.into());
            }
            let s = v.segments();
            !v.is_loopback()
                && !v.is_unspecified()
                && !v.is_multicast()
                && (s[0] & 0xfe00) != 0xfc00
                && (s[0] & 0xffc0) != 0xfe80
                && (s[0] & 0xe000) == 0x2000
        }
    }
}

/// A curl config string: quoted, with no line break that could start a new
/// option.
fn quote(value: &str) -> Result<String, RpcError> {
    if value.chars().any(|c| c.is_control()) {
        return Err(error("invalid HTTP input"));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub fn validate_headers(headers: &[(String, String)]) -> Result<(), RpcError> {
    if headers.len() > 128
        || headers
            .iter()
            .map(|(a, b)| a.len() + b.len())
            .sum::<usize>()
            > MAX_HEADERS
    {
        return Err(error("headers too large"));
    }
    for (n, v) in headers {
        if n.is_empty()
            || !n
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || v.chars().any(|c| c.is_control())
        {
            return Err(error("invalid header"));
        }
    }
    Ok(())
}

/// Configured credential headers may not replace the ones the transport owns.
pub fn validate_auth_headers(headers: &[(String, String)]) -> Result<(), RpcError> {
    validate_headers(headers)?;
    for (name, _) in headers {
        if [
            "Mcp-Session-Id",
            "MCP-Protocol-Version",
            "Host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Content-Type",
            "Accept",
            "Proxy-Authorization",
            "Expect",
        ]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(error("reserved transport header"));
        }
    }
    Ok(())
}

/// The production [`HttpClient`]: one `curl` process per request.
#[derive(Default)]
pub struct CurlClient;

/// One prepared curl run: its argv, the config it reads on stdin, and the
/// private body file, removed when this is dropped.
pub struct CurlInvocation {
    pub program: &'static str,
    pub args: Vec<String>,
    pub config: String,
    body: Option<tempfile::NamedTempFile>,
}
impl CurlInvocation {
    pub fn body_path(&self) -> Option<&Path> {
        self.body.as_ref().map(|f| f.path())
    }
}
impl std::fmt::Debug for CurlInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurlInvocation")
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

impl CurlClient {
    /// The curl run for `req`, connecting only to `ip`. `streamed` is a POST
    /// whose reply may be a long event stream: the 600 s deadline.
    pub fn invocation(
        req: &HttpRequest,
        ip: IpAddr,
        streamed: bool,
    ) -> Result<CurlInvocation, RpcError> {
        let url = HttpsUrl::parse(&req.url)?;
        validate_headers(&req.headers)?;
        if req.body.len() > MAX_BODY || !matches!(req.method.as_str(), "GET" | "POST" | "DELETE") {
            return Err(error("invalid HTTP request"));
        }
        let mut config = format!("url = {}\n", quote(&req.url)?);
        for (n, v) in &req.headers {
            config.push_str(&format!("header = {}\n", quote(&format!("{n}: {v}"))?));
        }
        // `NamedTempFile` is created `0600` on unix and removed on drop.
        let body = if req.body.is_empty() {
            None
        } else {
            let mut file = tempfile::Builder::new()
                .prefix("bs-mcp-body-")
                .tempfile()
                .map_err(|_| error("HTTP request body could not be staged"))?;
            file.write_all(&req.body)
                .and_then(|_| file.flush())
                .map_err(|_| error("HTTP request body could not be staged"))?;
            Some(file)
        };
        let pinned = match ip {
            IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        };
        let max_time = if streamed { STREAM_MAX_TIME } else { MAX_TIME };
        let mut args: Vec<String> = [
            // `-q` only works as the very first argument.
            "-q",
            "--silent",
            "--show-error",
            "--include",
            "--no-buffer",
            "--globoff",
            "--proto",
            if url.loopback {
                "=http,https"
            } else {
                "=https"
            },
            "--max-redirs",
            "0",
            "--noproxy",
            "*",
            "--connect-timeout",
            "10",
            "--max-filesize",
            "4194304",
            "--max-time",
        ]
        .map(str::to_owned)
        .to_vec();
        args.push(max_time.as_secs().to_string());
        args.push("--resolve".into());
        args.push(format!("{}:{}:{pinned}", url.host, url.port));
        args.push("--request".into());
        args.push(req.method.clone());
        if let Some(file) = &body {
            args.push("--data-binary".into());
            args.push(format!("@{}", file.path().display()));
        }
        args.push("--config".into());
        args.push("-".into());
        let program = if cfg!(windows) {
            "curl.exe"
        } else if Path::new("/usr/bin/curl").exists() {
            "/usr/bin/curl"
        } else {
            "curl"
        };
        Ok(CurlInvocation {
            program,
            args,
            config,
            body,
        })
    }

    /// The one address curl may connect to: loopback for the test switch,
    /// otherwise a DNS answer whose every address is public.
    async fn address(url: &HttpsUrl) -> Result<IpAddr, RpcError> {
        if url.loopback {
            return Ok(url
                .host
                .parse::<IpAddr>()
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }
        if let Ok(ip) = url.host.parse::<IpAddr>() {
            return Ok(ip);
        }
        let addresses: Vec<_> = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((url.host.as_str(), url.port)),
        )
        .await
        .map_err(|_| error("DNS deadline exceeded"))?
        .map_err(|_| error("DNS resolution failed"))?
        .collect();
        if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
            return Err(error("private address denied"));
        }
        Ok(addresses[0].ip())
    }

    async fn run(
        &self,
        req: HttpRequest,
        id: Option<Value>,
        handler: Option<&dyn McpEventHandler>,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        let url = HttpsUrl::parse(&req.url)?;
        let ip = Self::address(&url).await?;
        let streamed = id.is_some();
        let invocation = Self::invocation(&req, ip, streamed)?;
        let mut child = tokio::process::Command::new(invocation.program)
            .args(&invocation.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| error("HTTPS client unavailable"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| error("HTTPS client unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| error("HTTPS client unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| error("HTTPS client unavailable"))?;
        // curl's diagnostics may echo the URL: drained, never kept. Apart from
        // the reply, since a completed stream ends before curl does.
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
        let config = invocation.config.clone();
        let io = async {
            let write = async {
                stdin
                    .write_all(config.as_bytes())
                    .await
                    .map_err(|_| error("HTTP write failed"))?;
                drop(stdin);
                Ok::<_, RpcError>(())
            };
            let read = read_http_response_with_events(stdout, id.as_ref(), handler);
            let (_, (response, completed)) = tokio::try_join!(write, read)?;
            Ok::<_, RpcError>((response, completed))
        };
        let deadline = if streamed { STREAM_MAX_TIME } else { MAX_TIME } + GRACE;
        let outcome = tokio::select! {
            _ = cancel.cancelled() => Err(error("request cancelled")),
            r = tokio::time::timeout(deadline, io) => {
                r.map_err(|_| error("HTTP deadline exceeded")).and_then(|r| r)
            }
        };
        let result = match outcome {
            // A stream completed at its reply: the server may hold it open.
            Ok((response, true)) => Ok(response),
            Ok((response, false)) => match child.wait().await {
                Ok(status) if status.success() => Ok(response),
                _ => Err(error("HTTPS request failed")),
            },
            Err(e) => Err(e),
        };
        let _ = child.kill().await;
        // Only now, with curl gone, may the body file go.
        drop(invocation);
        result
    }
}

#[async_trait]
impl HttpClient for CurlClient {
    async fn send(
        &self,
        req: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        self.run(req, None, None, cancel).await
    }
    async fn send_mcp(
        &self,
        req: HttpRequest,
        id: Option<Value>,
        handler: &dyn McpEventHandler,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        self.run(req, id, Some(handler), cancel).await
    }
}

/// Reads curl's `--include` output. With `id`, an event-stream reply is
/// decoded as it arrives and finishes (`true`) at the event answering `id`;
/// otherwise the response is read to its end (`false`).
async fn read_http_response_with_events<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    id: Option<&Value>,
    handler: Option<&dyn McpEventHandler>,
) -> Result<(HttpResponse, bool), RpcError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let mut fed = 0;
    let mut events = Vec::new();
    let mut decoder = SseDecoder::default();
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| error("HTTP read failed"))?;
        if count == 0 {
            return Ok((parse_http(&bytes)?, false));
        }
        if bytes.len() + count > MAX_BODY + MAX_HEADERS {
            return Err(error("response too large"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(offset) = header_offset(&bytes)? else {
            continue;
        };
        let mut response = parse_http(&bytes)?;
        if response.status != 200 || !is_event_stream(&response) || id.is_none() {
            continue;
        }
        for event in decoder.push(&bytes[offset + fed..])? {
            validate_message(&event)?;
            if event.get("method").is_some() && event.get("id").is_some() {
                if let Some(handler) = handler {
                    if handler.handle_request(&event, &response.headers).await? {
                        continue;
                    }
                }
            }
            let complete = event.get("id") == id
                && (event.get("result").is_some() || event.get("error").is_some());
            events.push(event);
            if complete {
                response.body = encode_events(events)?;
                return Ok((response, true));
            }
        }
        fed = bytes.len() - offset;
    }
}

fn encode_events(events: Vec<Value>) -> Result<Vec<u8>, RpcError> {
    let mut body = Vec::new();
    for event in events {
        body.extend_from_slice(b"data: ");
        body.extend(serde_json::to_vec(&event).map_err(|_| error("invalid MCP event"))?);
        body.extend_from_slice(b"\n\n");
        if body.len() > MAX_BODY {
            return Err(error("response too large"));
        }
    }
    Ok(body)
}

fn media_type(response: &HttpResponse) -> Option<&str> {
    response
        .header("content-type")
        .map(|v| v.split(';').next().unwrap_or("").trim())
}

fn is_event_stream(response: &HttpResponse) -> bool {
    media_type(response).is_some_and(|v| v.eq_ignore_ascii_case("text/event-stream"))
}

/// Where the final (non-`1xx`) response's body starts, once its head is in.
fn header_offset(bytes: &[u8]) -> Result<Option<usize>, RpcError> {
    let mut offset = 0;
    loop {
        let Some(end) = bytes[offset..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + offset)
        else {
            if bytes.len() > MAX_HEADERS {
                return Err(error("headers too large"));
            }
            return Ok(None);
        };
        if end > MAX_HEADERS {
            return Err(error("headers too large"));
        }
        let status = std::str::from_utf8(&bytes[offset..end])
            .ok()
            .and_then(|s| s.lines().next())
            .and_then(|s| s.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| error("invalid HTTP status"))?;
        offset = end + 4;
        if !(100..200).contains(&status) {
            return Ok(Some(offset));
        }
    }
}

fn parse_http(bytes: &[u8]) -> Result<HttpResponse, RpcError> {
    let mut offset = 0;
    loop {
        let end = bytes[offset..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| error("invalid response headers"))?
            + offset;
        if end > MAX_HEADERS {
            return Err(error("headers too large"));
        }
        let text = std::str::from_utf8(&bytes[offset..end])
            .map_err(|_| error("invalid response headers"))?;
        let mut lines = text.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| error("invalid HTTP status"))?;
        let mut headers = vec![];
        for line in lines {
            let (n, v) = line
                .split_once(':')
                .ok_or_else(|| error("invalid response header"))?;
            headers.push((n.into(), v.trim().into()));
        }
        validate_headers(&headers)?;
        offset = end + 4;
        if status / 100 == 1 {
            continue;
        }
        let body = bytes[offset..].to_vec();
        if body.len() > MAX_BODY {
            return Err(error("response too large"));
        }
        return Ok(HttpResponse {
            status,
            headers,
            body,
        });
    }
}

/// An incremental `text/event-stream` decoder: any of CR, LF or CRLF ends a
/// line, even split across chunks, and only `data:` fields are kept.
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
    event_bytes: usize,
    started: bool,
    skip_lf: bool,
}
impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, RpcError> {
        if self.buffer.len() + chunk.len() + self.event_bytes > MAX_BODY {
            return Err(error("event stream too large"));
        }
        self.buffer.extend_from_slice(chunk);
        let mut output = vec![];
        if !self.started {
            const BOM: &[u8] = b"\xef\xbb\xbf";
            if self.buffer.starts_with(BOM) {
                self.buffer.drain(..3);
            } else if BOM.starts_with(&self.buffer) {
                return Ok(output);
            }
            self.started = true;
        }
        loop {
            if self.skip_lf && !self.buffer.is_empty() {
                if self.buffer[0] == b'\n' {
                    self.buffer.drain(..1);
                }
                self.skip_lf = false;
            }
            let Some(end) = self.buffer.iter().position(|b| *b == b'\r' || *b == b'\n') else {
                break;
            };
            let line = std::str::from_utf8(&self.buffer[..end])
                .map_err(|_| error("invalid event encoding"))?
                .to_owned();
            self.skip_lf = self.buffer[end] == b'\r';
            self.buffer.drain(..end + 1);
            self.event_bytes += end + 1;
            if self.event_bytes > MAX_BODY {
                return Err(error("event stream too large"));
            }
            if line.is_empty() {
                let data = self.data.join("\n");
                if !data.is_empty() {
                    output
                        .push(serde_json::from_str(&data).map_err(|_| error("invalid MCP event"))?);
                }
                self.data.clear();
                self.event_bytes = 0;
            } else {
                let (field, value) = line.split_once(':').unwrap_or((&line, ""));
                if field == "data" {
                    self.data
                        .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                }
            }
        }
        Ok(output)
    }
    pub fn finish(&self) -> Result<(), RpcError> {
        if self.buffer.iter().all(|b| b.is_ascii_whitespace()) && self.data.is_empty() {
            Ok(())
        } else {
            Err(error("incomplete event stream"))
        }
    }
}

struct SessionState {
    /// The credential headers of the latest request.
    headers: Vec<(String, String)>,
    session: Option<String>,
    initialized: bool,
    protocol: String,
    epoch: u64,
}

/// One MCP client's session with one remote server. Separate clients get
/// separate sessions, so `Mcp-Session-Id`s never mix. Credentials are read
/// from the registry for every request (refreshing an OAuth token when due),
/// and the server must still exist, be enabled and keep the URL it had when
/// the session opened.
pub struct RemoteSession {
    servers: Arc<McpServers>,
    id: McpServerId,
    url: Option<String>,
    http: Arc<dyn HttpClient>,
    /// Set for Settings' connection test, which may try a disabled server.
    allow_disabled: bool,
    state: parking_lot::Mutex<SessionState>,
    cancel: CancellationToken,
    initialize_lock: tokio::sync::Mutex<()>,
}

impl RemoteSession {
    pub fn open(servers: Arc<McpServers>, id: McpServerId, http: Arc<dyn HttpClient>) -> Self {
        let url = servers.server(&id).and_then(|s| match s.transport {
            McpTransportConfig::Http { url, .. } => Some(url),
            McpTransportConfig::Stdio { .. } => None,
        });
        Self {
            servers,
            id,
            url,
            http,
            allow_disabled: false,
            state: parking_lot::Mutex::new(SessionState {
                headers: vec![],
                session: None,
                initialized: false,
                protocol: String::new(),
                epoch: 0,
            }),
            cancel: CancellationToken::new(),
            initialize_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn allowing_disabled(mut self) -> Self {
        self.allow_disabled = true;
        self
    }

    /// Ends the session: in-flight and later requests fail.
    pub fn close(&self) {
        self.cancel.cancel();
    }

    /// Whether the session has a live upstream initialization: false before
    /// `initialize` and again after the server expired the session.
    pub fn is_initialized(&self) -> bool {
        self.state.lock().initialized
    }

    /// The URL to post to, after checking the server still allows it.
    fn current_url(&self) -> Result<String, RpcError> {
        let pinned = self
            .url
            .as_ref()
            .ok_or_else(|| error("not a remote server"))?;
        let server = self
            .servers
            .server(&self.id)
            .ok_or_else(|| error("server removed"))?;
        if !server.enabled && !self.allow_disabled {
            return Err(error("server disabled"));
        }
        match &server.transport {
            McpTransportConfig::Http { url, .. } if url == pinned => Ok(url.clone()),
            _ => Err(error("server configuration changed")),
        }
    }

    /// Sends one JSON-RPC message; returns the reply (for a request) plus any
    /// notifications the server sent with it.
    pub async fn request(&self, message: Value) -> Result<Vec<Value>, RpcError> {
        validate_message(&message)?;
        if self.cancel.is_cancelled() {
            return Err(error("session closed"));
        }
        let url = self.current_url()?;
        HttpsUrl::parse(&url)?;
        let auth = self
            .servers
            .auth_headers_for(&self.id, self.allow_disabled)
            .await?;
        validate_auth_headers(&auth)?;
        let expects_response = message.get("method").is_some() && message.get("id").is_some();
        let init = message.get("method").and_then(Value::as_str) == Some("initialize");
        let _initial = if init {
            Some(self.initialize_lock.lock().await)
        } else {
            None
        };
        let (headers, request_epoch, had_session) = {
            let mut state = self.state.lock();
            if !init && !state.initialized {
                return Err(error("session not initialized"));
            }
            if init && state.initialized {
                return Err(error("session already initialized"));
            }
            state.headers = auth;
            let mut h = state.headers.clone();
            h.push(("Content-Type".into(), "application/json".into()));
            h.push((
                "Accept".into(),
                "application/json, text/event-stream".into(),
            ));
            if let Some(s) = &state.session {
                h.push(("Mcp-Session-Id".into(), s.clone()));
            }
            if state.initialized {
                h.push(("MCP-Protocol-Version".into(), state.protocol.clone()));
            }
            (h, state.epoch, state.session.is_some())
        };
        let body = serde_json::to_vec(&message).map_err(|_| error("invalid MCP message"))?;
        if body.len() > MAX_BODY {
            return Err(error("message too large"));
        }
        let request = HttpRequest {
            method: "POST".into(),
            url,
            headers,
            body,
        };
        let responder = SessionRequestResponder {
            session: self,
            request_epoch,
            initial_protocol: message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION),
        };
        let id = if expects_response {
            message.get("id").cloned()
        } else {
            None
        };
        let deadline = if expects_response {
            STREAM_MAX_TIME
        } else {
            MAX_TIME
        } + GRACE;
        let response = tokio::select! {
            _ = self.cancel.cancelled() => return Err(error("session closed")),
            r = tokio::time::timeout(
                deadline,
                self.http.send_mcp(request, id, &responder, self.cancel.child_token()),
            ) => r.map_err(|_| error("HTTP deadline exceeded"))??
        };
        if response.body.len() > MAX_BODY {
            return Err(error("response too large"));
        }
        validate_headers(&response.headers)?;
        if response.status == 404 && had_session {
            self.expire(request_epoch)?;
            return Err(error("session_expired"));
        }
        if response.status == 401 || response.status == 403 {
            let step = response
                .header("www-authenticate")
                .is_some_and(|h| h.contains("insufficient_scope"));
            let reason = if step {
                "step_up_scope_required"
            } else {
                "authentication_required"
            };
            self.servers.note_failure(&self.id, &error(reason));
            return Err(error(reason));
        }
        if response.status == 202 {
            if expects_response {
                return Err(error("request response missing"));
            }
            return Ok(vec![]);
        }
        if !(200..300).contains(&response.status) {
            return Err(error("upstream HTTP rejected"));
        }
        let messages = if is_event_stream(&response) {
            let mut decoder = SseDecoder::default();
            let result = decoder.push(&response.body)?;
            decoder.finish()?;
            result
        } else {
            vec![serde_json::from_slice(&response.body).map_err(|_| error("invalid MCP JSON"))?]
        };
        for incoming in &messages {
            validate_message(incoming)?;
        }
        if expects_response {
            let responses: Vec<_> = messages
                .iter()
                .filter(|m| m.get("result").is_some() || m.get("error").is_some())
                .collect();
            if responses.len() != 1 || responses[0].get("id") != message.get("id") {
                return Err(error("response ID mismatch or missing"));
            }
        }
        if init {
            let matching = messages
                .iter()
                .find(|m| m.get("id") == message.get("id"))
                .ok_or_else(|| error("initialize response missing"))?;
            let version = matching
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                .ok_or_else(|| error("initialize failed"))?;
            if !SUPPORTED_VERSIONS.contains(&version) {
                return Err(error("unsupported_protocol_version; modern 2026-07-28 operations require newer transport"));
            }
            let session = response.header("mcp-session-id").map(str::to_owned);
            if session.as_ref().is_some_and(|s| !valid_session_id(s)) {
                return Err(error("invalid session ID"));
            }
            let mut state = self.state.lock();
            if self.cancel.is_cancelled() {
                return Err(error("session closed"));
            }
            if state.epoch != request_epoch {
                return Err(error("session_expired"));
            }
            state.initialized = true;
            state.session = session;
            state.protocol = version.to_owned();
            state.epoch = state
                .epoch
                .checked_add(1)
                .ok_or_else(|| error("session generation exhausted"))?;
        } else if self.state.lock().epoch != request_epoch {
            return Err(error("session_expired"));
        }
        Ok(messages)
    }

    fn expire(&self, request_epoch: u64) -> Result<(), RpcError> {
        let mut state = self.state.lock();
        // A late 404 from an old request cannot erase a replacement session.
        if state.epoch == request_epoch {
            state.epoch = state
                .epoch
                .checked_add(1)
                .ok_or_else(|| error("session generation exhausted"))?;
            state.initialized = false;
            state.session = None;
            state.protocol.clear();
        }
        Ok(())
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 1024 && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Answers a server's own requests inside a streamed reply: `ping`, and an
/// explicit refusal for anything else (sampling, elicitation), which this
/// client never advertised.
struct SessionRequestResponder<'a> {
    session: &'a RemoteSession,
    request_epoch: u64,
    initial_protocol: &'a str,
}
#[async_trait]
impl McpEventHandler for SessionRequestResponder<'_> {
    async fn handle_request(
        &self,
        message: &Value,
        response_headers: &[(String, String)],
    ) -> Result<bool, RpcError> {
        validate_message(message)?;
        if message.get("method").is_none() || message.get("id").is_none() {
            return Ok(false);
        }
        let reply = if message["method"] == "ping" {
            json!({"jsonrpc":"2.0","id":message["id"],"result":{}})
        } else {
            json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Unsupported MCP server request"}})
        };
        let (mut headers, had_session) = {
            let state = self.session.state.lock();
            if state.epoch != self.request_epoch {
                return Err(error("session_expired"));
            }
            let session = state.session.as_deref().or_else(|| {
                response_headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
                    .map(|(_, value)| value.as_str())
            });
            if session.is_some_and(|id| !valid_session_id(id)) {
                return Err(error("invalid session ID"));
            }
            let mut headers = state.headers.clone();
            if let Some(id) = session {
                headers.push(("Mcp-Session-Id".into(), id.into()));
            }
            headers.push((
                "MCP-Protocol-Version".into(),
                if state.initialized {
                    state.protocol.clone()
                } else {
                    self.initial_protocol.into()
                },
            ));
            (headers, session.is_some())
        };
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push((
            "Accept".into(),
            "application/json, text/event-stream".into(),
        ));
        let request = HttpRequest {
            method: "POST".into(),
            url: self
                .session
                .url
                .clone()
                .ok_or_else(|| error("not a remote server"))?,
            headers,
            body: serde_json::to_vec(&reply).map_err(|_| error("invalid MCP response"))?,
        };
        let response = tokio::select! {
            _ = self.session.cancel.cancelled() => return Err(error("session closed")),
            response = tokio::time::timeout(
                MAX_TIME + GRACE,
                self.session.http.send(request, self.session.cancel.child_token()),
            ) => response.map_err(|_| error("HTTP deadline exceeded"))??,
        };
        validate_headers(&response.headers)?;
        if response.body.len() > MAX_BODY {
            return Err(error("response too large"));
        }
        if response.status == 404 && had_session {
            self.session.expire(self.request_epoch)?;
            return Err(error("session_expired"));
        }
        if response.status != 202 {
            return Err(error("server_request_reply_rejected"));
        }
        Ok(true)
    }
}

fn validate_message(message: &Value) -> Result<(), RpcError> {
    if !message.is_object() || message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error("invalid JSON-RPC message"));
    }
    if message
        .get("id")
        .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
    {
        return Err(error("invalid JSON-RPC ID"));
    }
    let method = message.get("method");
    let result = message.get("result");
    let failure = message.get("error");
    if let Some(method) = method {
        if !method.as_str().is_some_and(|m| !m.is_empty()) || result.is_some() || failure.is_some()
        {
            return Err(error("invalid JSON-RPC request"));
        }
    } else if message.get("id").is_none() || result.is_some() == failure.is_some() {
        return Err(error("invalid JSON-RPC response"));
    }
    Ok(())
}

/// SHA-256 for the PKCE challenge, delegated to the operating system's
/// implementation rather than a hand-rolled or newly added crate.
pub fn sha256(input: &[u8]) -> Result<[u8; 32], RpcError> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptOpenAlgorithmProvider(
                h: *mut *mut c_void,
                id: *const u16,
                implementation: *const u16,
                flags: u32,
            ) -> i32;
            fn BCryptHash(
                h: *mut c_void,
                secret: *const u8,
                secret_len: u32,
                input: *const u8,
                input_len: u32,
                output: *mut u8,
                output_len: u32,
            ) -> i32;
            fn BCryptCloseAlgorithmProvider(h: *mut c_void, flags: u32) -> i32;
        }
        let mut h = std::ptr::null_mut();
        let name: Vec<u16> = "SHA256\0".encode_utf16().collect();
        let mut output = [0; 32];
        let len: u32 = input
            .len()
            .try_into()
            .map_err(|_| error("hash input too large"))?;
        // SAFETY: plain CNG calls on buffers this function owns, with their
        // true lengths; the provider handle is closed on every path.
        unsafe {
            if BCryptOpenAlgorithmProvider(&mut h, name.as_ptr(), std::ptr::null(), 0) < 0 {
                return Err(error("SHA256 unavailable"));
            }
            let status = BCryptHash(
                h,
                std::ptr::null(),
                0,
                input.as_ptr(),
                len,
                output.as_mut_ptr(),
                32,
            );
            BCryptCloseAlgorithmProvider(h, 0);
            if status < 0 {
                return Err(error("SHA256 failed"));
            }
        }
        Ok(output)
    }
    #[cfg(not(windows))]
    {
        let mut child = std::process::Command::new("sha256sum")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| error("SHA256 unavailable"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| error("SHA256 unavailable"))?
            .write_all(input)
            .map_err(|_| error("SHA256 failed"))?;
        let output = child
            .wait_with_output()
            .map_err(|_| error("SHA256 failed"))?;
        let digest = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| hex::decode(s).ok())
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| error("SHA256 failed"))?;
        Ok(digest)
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use bondsymphonic_proto::{McpAuthConfig, McpServerConfig, McpUpsertParams};

    #[tokio::test]
    async fn post_sse_completes_before_server_closes_stream() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n").await.unwrap();
        let id = json!(7);
        let (response, completed) = tokio::time::timeout(
            Duration::from_millis(100),
            read_http_response_with_events(reader, Some(&id), None),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(completed);
        let mut decoder = SseDecoder::default();
        let messages = decoder.push(&response.body).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["id"], 7);
        drop(writer);
    }

    struct PingPeer {
        replies: parking_lot::Mutex<Vec<HttpRequest>>,
        answered: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl HttpClient for PingPeer {
        async fn send(
            &self,
            request: HttpRequest,
            _: CancellationToken,
        ) -> Result<HttpResponse, RpcError> {
            let message: Value = serde_json::from_slice(&request.body).unwrap();
            if message["method"] == "initialize" {
                return Ok(HttpResponse { status: 200, headers: vec![("Mcp-Session-Id".into(), "ping-session".into())], body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"2025-11-25"}})).unwrap() });
            }
            self.replies.lock().push(request);
            self.answered.notify_one();
            Ok(HttpResponse {
                status: 202,
                headers: vec![],
                body: vec![],
            })
        }
        async fn send_mcp(
            &self,
            request: HttpRequest,
            id: Option<Value>,
            handler: &dyn McpEventHandler,
            cancel: CancellationToken,
        ) -> Result<HttpResponse, RpcError> {
            let message: Value = serde_json::from_slice(&request.body).unwrap();
            if message["method"] == "initialize" {
                return self.send(request, cancel).await;
            }
            let (mut writer, reader) = tokio::io::duplex(4096);
            writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"health\",\"method\":\"ping\"}\n\n").await.unwrap();
            let answered = self.answered.clone();
            let id_for_writer = id.clone().unwrap();
            tokio::spawn(async move {
                answered.notified().await;
                writer.write_all(format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":id_for_writer,"result":{"content":[{"type":"text","text":"complete"}]}})).as_bytes()).await.unwrap();
            });
            read_http_response_with_events(reader, id.as_ref(), Some(handler))
                .await
                .map(|(response, _)| response)
        }
    }

    #[tokio::test]
    async fn pending_post_ping_is_answered_before_tool_response() {
        let peer = Arc::new(PingPeer {
            replies: Default::default(),
            answered: Arc::new(tokio::sync::Notify::new()),
        });
        let dir = tempfile::tempdir().unwrap();
        let servers = Arc::new(McpServers::load(dir.path()).unwrap());
        servers
            .upsert(McpUpsertParams {
                server: McpServerConfig {
                    id: "remote".into(),
                    name: "remote".into(),
                    transport: McpTransportConfig::Http {
                        url: "https://example.com/mcp".into(),
                        auth: McpAuthConfig::Bearer {
                            token: "tok".into(),
                        },
                    },
                    enabled: true,
                    default_workspace_enabled: true,
                    enabled_backends: vec![],
                },
                secrets: vec![bondsymphonic_proto::McpSecretWrite::Set {
                    reference: "tok".into(),
                    value: bondsymphonic_proto::McpSecretValue("current".into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        let session = RemoteSession::open(servers, "remote".into(), peer.clone());
        session
            .request(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .await
            .unwrap();
        let messages = tokio::time::timeout(
            Duration::from_millis(500),
            session.request(json!({"jsonrpc":"2.0","id":77,"method":"tools/call"})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], 77);
        let replies = peer.replies.lock();
        assert_eq!(replies.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&replies[0].body).unwrap(),
            json!({"jsonrpc":"2.0","id":"health","result":{}})
        );
        assert!(replies[0]
            .headers
            .contains(&("Mcp-Session-Id".into(), "ping-session".into())));
        assert!(replies[0]
            .headers
            .contains(&("MCP-Protocol-Version".into(), "2025-11-25".into())));
        assert!(replies[0]
            .headers
            .contains(&("Authorization".into(), "Bearer current".into())));
    }
}
