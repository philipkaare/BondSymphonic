//! Remote MCP transport: Streamable HTTP sessions over a fake `HttpClient`,
//! the event-stream decoder, the URL policy, and the production `CurlClient`
//! against loopback fake servers (behind `BS_MCP_ALLOW_HTTP=1`).
use async_trait::async_trait;
use bondsymphonic_daemon::mcp::servers::{transport::*, McpServers};
use bondsymphonic_proto::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "s3cr3t-sentinel";

fn remote(id: &str, auth: McpAuthConfig) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: id.into(),
        transport: McpTransportConfig::Http {
            url: "https://example.com/mcp".into(),
            auth,
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

/// A registry holding one remote server `remote` with `auth` and `secrets`.
fn registry(
    auth: McpAuthConfig,
    secrets: Vec<McpSecretWrite>,
    http: Arc<dyn HttpClient>,
) -> (tempfile::TempDir, Arc<McpServers>) {
    let dir = tempfile::tempdir().unwrap();
    let servers = McpServers::load(dir.path()).unwrap().with_http(http);
    servers
        .upsert(McpUpsertParams {
            server: remote("remote", auth),
            secrets,
            expected_revision: None,
        })
        .unwrap();
    (dir, Arc::new(servers))
}

fn open(servers: &Arc<McpServers>, http: Arc<dyn HttpClient>) -> RemoteSession {
    RemoteSession::open(servers.clone(), "remote".into(), http)
}

fn init() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})
}

struct Peer(Mutex<Vec<HttpRequest>>);
#[async_trait]
impl HttpClient for Peer {
    async fn send(
        &self,
        request: HttpRequest,
        _cancel: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        self.0.lock().unwrap().push(request.clone());
        let input: Value = serde_json::from_slice(&request.body).unwrap();
        Ok(HttpResponse {
            status: 200,
            headers: vec![(
                "Mcp-Session-Id".into(),
                format!("session-{}", self.0.lock().unwrap().len()),
            )],
            body: serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":input["id"],"result":{"protocolVersion":"2025-11-25"}}),
            )
            .unwrap(),
        })
    }
}

#[tokio::test]
async fn http_session_headers_isolated() {
    let peer = Arc::new(Peer(Mutex::new(vec![])));
    let (_d, servers) = registry(McpAuthConfig::None, vec![], peer.clone());
    let a = open(&servers, peer.clone());
    let b = open(&servers, peer.clone());
    a.request(init()).await.unwrap();
    b.request(init()).await.unwrap();
    a.request(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .unwrap();
    b.request(json!({"jsonrpc":"2.0","id":"eight","method":"tools/list"}))
        .await
        .unwrap();
    let r = peer.0.lock().unwrap();
    assert!(r[2]
        .headers
        .contains(&("Mcp-Session-Id".into(), "session-1".into())));
    assert!(r[3]
        .headers
        .contains(&("Mcp-Session-Id".into(), "session-2".into())));
}

#[tokio::test]
async fn bearer_comes_from_the_secret_store_and_never_from_debug() {
    let peer = Arc::new(Peer(Mutex::new(vec![])));
    let (_d, servers) = registry(
        McpAuthConfig::Bearer {
            token: "tok".into(),
        },
        vec![McpSecretWrite::Set {
            reference: "tok".into(),
            value: McpSecretValue(SENTINEL.into()),
        }],
        peer.clone(),
    );
    open(&servers, peer.clone()).request(init()).await.unwrap();
    let requests = peer.0.lock().unwrap();
    assert!(requests[0]
        .headers
        .contains(&("Authorization".into(), format!("Bearer {SENTINEL}"))));
    assert!(!format!("{:?}", requests[0]).contains(SENTINEL));
}

#[tokio::test]
async fn disabled_or_removed_server_rejects_requests() {
    let peer = Arc::new(Peer(Mutex::new(vec![])));
    let (_d, servers) = registry(McpAuthConfig::None, vec![], peer.clone());
    let session = open(&servers, peer.clone());
    session.request(init()).await.unwrap();
    let mut disabled = remote("remote", McpAuthConfig::None);
    disabled.enabled = false;
    servers
        .upsert(McpUpsertParams {
            server: disabled,
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    assert!(session
        .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .is_err());
    servers.remove(&"remote".into()).unwrap();
    assert!(session
        .request(json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .await
        .is_err());
    assert_eq!(peer.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn closed_session_rejects_requests() {
    let peer = Arc::new(Peer(Mutex::new(vec![])));
    let (_d, servers) = registry(McpAuthConfig::None, vec![], peer.clone());
    let session = open(&servers, peer.clone());
    session.close();
    assert!(session.request(init()).await.is_err());
    assert!(peer.0.lock().unwrap().is_empty());
}

#[test]
fn sse_fragmentation_and_bounds() {
    let mut decoder = SseDecoder::default();
    assert!(decoder
        .push(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"x")
        .unwrap()
        .is_empty());
    let messages = decoder.push(b"\",\"result\":{}}\r\n\r\n").unwrap();
    assert_eq!(messages[0]["id"], "x");
    assert!(decoder.push(&vec![b'a'; MAX_BODY + 1]).is_err());
}

#[test]
fn sse_fragmented_crlf_events() {
    // Two events and a comment, with every CRLF pair split across chunks and
    // one event's data split mid-JSON over two `data:` lines.
    let stream: &[&[u8]] = &[
        b": keep-alive\r",
        b"\ndata: {\"jsonrpc\":\"2.0\",",
        b"\r",
        b"\ndata: \"method\":\"notifications/progress\"}\r",
        b"\n\r",
        b"\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"res",
        b"ult\":{}}\r",
        b"\n",
        b"\r",
        b"\n",
    ];
    let mut decoder = SseDecoder::default();
    let mut messages = vec![];
    for chunk in stream {
        messages.extend(decoder.push(chunk).unwrap());
    }
    decoder.finish().unwrap();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0]["method"], "notifications/progress");
    assert_eq!(messages[1]["id"], 9);
}

#[test]
fn sse_mixed_delimiters_preserve_event_order() {
    let mut decoder = SseDecoder::default();
    let messages = decoder.push(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"two\",\"result\":{}}\n\n").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(messages[1]["id"], "two");
}

#[test]
fn sse_accepts_bom_cr_lf_and_split_crlf() {
    for newline in ["\r", "\n", "\r\n"] {
        let frame = format!(
            "\u{feff}data: {{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{{}}}}{newline}{newline}"
        );
        // Byte-at-a-time also splits BOM and CRLF across chunks.
        let mut decoder = SseDecoder::default();
        let mut messages = vec![];
        for byte in frame.as_bytes() {
            messages.extend(decoder.push(&[*byte]).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(messages.len(), 1, "newline {newline:?}");
        assert_eq!(messages[0]["id"], 7);
    }
}

#[test]
fn private_addresses_and_url_credentials_rejected() {
    for u in [
        "http://example.com/mcp",
        "http://127.0.0.1:8080/mcp",
        "https://127.0.0.1/mcp",
        "https://[::1]/mcp",
        "https://localhost/mcp",
        "https://user:password@example.com/mcp",
        "https://10.0.0.1/mcp",
        "https://169.254.169.254/latest",
    ] {
        assert!(HttpsUrl::parse_with(u, false).is_err(), "{u}");
    }
    assert!(HttpsUrl::parse_with("https://example.com/mcp", false).is_ok());
    // The test switch admits loopback only, plain or not.
    assert!(HttpsUrl::parse_with("http://127.0.0.1:8080/mcp", true).is_ok());
    assert!(HttpsUrl::parse_with("http://example.com/mcp", true).is_err());
    assert!(HttpsUrl::parse_with("http://10.0.0.1/mcp", true).is_err());
}

struct Redirect;
#[async_trait]
impl HttpClient for Redirect {
    async fn send(&self, _: HttpRequest, _: CancellationToken) -> Result<HttpResponse, RpcError> {
        Ok(HttpResponse {
            status: 302,
            headers: vec![("location".into(), "https://other.example/mcp".into())],
            body: vec![],
        })
    }
}

#[tokio::test]
async fn redirect_cannot_escape_origin() {
    let (_d, servers) = registry(McpAuthConfig::None, vec![], Arc::new(Redirect));
    let session = open(&servers, Arc::new(Redirect));
    assert!(session.request(init()).await.is_err());
}

#[test]
fn sha256_known_vector() {
    assert_eq!(
        hex::encode(sha256(b"abc").unwrap()),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[tokio::test]
async fn configured_headers_cannot_replace_transport_headers() {
    for name in [
        "Mcp-Session-Id",
        "MCP-Protocol-Version",
        "Host",
        "Content-Length",
        "Transfer-Encoding",
    ] {
        let peer = Arc::new(Peer(Mutex::new(vec![])));
        let (_d, servers) = registry(
            McpAuthConfig::Headers {
                headers: [(name.to_string(), SecretRef("h".into()))].into(),
            },
            vec![McpSecretWrite::Set {
                reference: "h".into(),
                value: McpSecretValue("attacker".into()),
            }],
            peer.clone(),
        );
        assert!(
            open(&servers, peer.clone()).request(init()).await.is_err(),
            "{name}"
        );
        assert!(peer.0.lock().unwrap().is_empty(), "{name}");
    }
}

struct Reply {
    id: Value,
    version: &'static str,
}
#[async_trait]
impl HttpClient for Reply {
    async fn send(
        &self,
        request: HttpRequest,
        _: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        let message: Value = serde_json::from_slice(&request.body).unwrap();
        Ok(HttpResponse { status: 200, headers: vec![("Content-Type".into(), "application/json".into())], body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id": if message["method"] == "initialize" {message["id"].clone()} else {self.id.clone()},"result":{"protocolVersion":self.version}})).unwrap() })
    }
}

fn reply_session(id: Value, version: &'static str) -> (tempfile::TempDir, RemoteSession) {
    let http: Arc<dyn HttpClient> = Arc::new(Reply { id, version });
    let (dir, servers) = registry(McpAuthConfig::None, vec![], http.clone());
    (dir, open(&servers, http))
}

#[tokio::test]
async fn unrelated_response_id_is_rejected() {
    let (_d, session) = reply_session(json!("8"), "2025-11-25");
    session.request(init()).await.unwrap();
    assert!(session
        .request(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .is_err());
}

#[tokio::test]
async fn older_streamable_era_is_negotiated() {
    let (_d, session) = reply_session(json!(8), "2025-06-18");
    session.request(init()).await.unwrap();
    session
        .request(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn modern_only_era_is_explicitly_rejected() {
    let (_d, session) = reply_session(json!(8), "2026-07-28");
    let error = session.request(init()).await.unwrap_err();
    assert!(error.message.contains("unsupported_protocol_version"));
}

struct ExpiredSessionPeer(Mutex<Vec<HttpRequest>>);
#[async_trait]
impl HttpClient for ExpiredSessionPeer {
    async fn send(
        &self,
        request: HttpRequest,
        _: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        let message: Value = serde_json::from_slice(&request.body).unwrap();
        let mut requests = self.0.lock().unwrap();
        requests.push(request);
        let init = message["method"] == "initialize";
        let expires = !init && requests.len() == 2;
        Ok(HttpResponse {
            status: if expires { 404 } else { 200 },
            headers: if init {
                vec![(
                    "Mcp-Session-Id".into(),
                    format!("session-{}", requests.len()),
                )]
            } else {
                vec![]
            },
            body: if expires {
                vec![]
            } else {
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":"2025-11-25"}})).unwrap()
            },
        })
    }
}

#[tokio::test]
async fn expired_session_reinitializes_without_replaying_tool_call() {
    let peer = Arc::new(ExpiredSessionPeer(Mutex::new(vec![])));
    let (_d, servers) = registry(McpAuthConfig::None, vec![], peer.clone());
    let session = open(&servers, peer.clone());
    session.request(init()).await.unwrap();
    let error = session
        .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mutate"}}))
        .await
        .unwrap_err();
    assert!(error.message.contains("session_expired"));
    assert_eq!(peer.0.lock().unwrap().len(), 2);
    session.request(init()).await.unwrap();
    let requests = peer.0.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[2]
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id")));
    assert_eq!(
        requests
            .iter()
            .filter(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["method"] == "tools/call")
            .count(),
        1
    );
}

/// Answers `initialize`, accepts `notifications/initialized`, and lists two
/// tools over two pages; or fails every request with `fail`.
struct ToolsPeer {
    fail: Option<u16>,
}
#[async_trait]
impl HttpClient for ToolsPeer {
    async fn send(
        &self,
        request: HttpRequest,
        _: CancellationToken,
    ) -> Result<HttpResponse, RpcError> {
        if let Some(status) = self.fail {
            return Ok(HttpResponse {
                status,
                headers: vec![],
                body: format!("upstream said {SENTINEL}").into_bytes(),
            });
        }
        let message: Value = serde_json::from_slice(&request.body).unwrap();
        let result = match message["method"].as_str().unwrap() {
            "initialize" => json!({"protocolVersion":"2025-06-18"}),
            "tools/list" if message["params"]["cursor"] == "p2" => {
                json!({"tools":[{"name":"second"}]})
            }
            "tools/list" => json!({"tools":[{"name":"first"}],"nextCursor":"p2"}),
            _ => {
                return Ok(HttpResponse {
                    status: 202,
                    headers: vec![],
                    body: vec![],
                })
            }
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":message["id"],"result":result}))
                .unwrap(),
        })
    }
}

#[tokio::test]
async fn test_lists_tools_and_sets_status() {
    let (_d, servers) = registry(
        McpAuthConfig::None,
        vec![],
        Arc::new(ToolsPeer { fail: None }),
    );
    let id: McpServerId = "remote".into();
    assert_eq!(servers.status(&id).state, McpConnectionState::Disconnected);
    let result = servers.test(&id).await.unwrap();
    assert_eq!(result.tool_names, ["first", "second"]);
    assert_eq!(result.status.state, McpConnectionState::Connected);
    let listed = servers.list().unwrap();
    let status = listed.statuses.iter().find(|s| s.server_id == id).unwrap();
    assert_eq!(status.state, McpConnectionState::Connected);

    // An edit of the server forgets the old result.
    let mut renamed = remote("remote", McpAuthConfig::None);
    renamed.name = "Renamed".into();
    servers
        .upsert(McpUpsertParams {
            server: renamed,
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    assert_eq!(servers.status(&id).state, McpConnectionState::Disconnected);
}

#[tokio::test]
async fn failed_test_is_an_error_status_without_upstream_text() {
    for status in [401, 500] {
        let (_d, servers) = registry(
            McpAuthConfig::Bearer {
                token: "tok".into(),
            },
            vec![McpSecretWrite::Set {
                reference: "tok".into(),
                value: McpSecretValue(SENTINEL.into()),
            }],
            Arc::new(ToolsPeer { fail: Some(status) }),
        );
        let result = servers.test(&"remote".into()).await.unwrap();
        assert_eq!(result.status.state, McpConnectionState::Error);
        let reason = result.status.reason.clone().unwrap();
        assert!(!reason.is_empty());
        assert!(!format!("{result:?}").contains(SENTINEL));
        assert!(!serde_json::to_string(&servers.list().unwrap())
            .unwrap()
            .contains(SENTINEL));
    }
}

#[tokio::test]
async fn test_of_a_local_server_reports_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let servers = Arc::new(McpServers::load(dir.path()).unwrap());
    servers
        .upsert(McpUpsertParams {
            server: McpServerConfig {
                id: "local".into(),
                name: "local".into(),
                transport: McpTransportConfig::Stdio {
                    command: "node".into(),
                    args: vec![],
                    env: Default::default(),
                    secret_env: vec![],
                },
                enabled: true,
                default_workspace_enabled: true,
                enabled_backends: vec!["claude".into()],
            },
            secrets: vec![],
            expected_revision: None,
        })
        .unwrap();
    let result = servers.test(&"local".into()).await.unwrap();
    assert_eq!(result.status.state, McpConnectionState::Error);
    assert!(result.tool_names.is_empty());
}

#[tokio::test]
async fn degraded_registry_refuses_transport_operations() {
    let dir = tempfile::tempdir().unwrap();
    McpServers::load(dir.path()).unwrap();
    std::fs::write(dir.path().join("mcp/registry.json"), "{ nope").unwrap();
    let servers = Arc::new(McpServers::load(dir.path()).unwrap());
    let jira: McpServerId = "jira".into();
    assert_eq!(
        servers.test(&jira).await.unwrap_err().code,
        ErrorCode::IoError
    );
    assert_eq!(
        servers.disconnect(&jira).await.unwrap_err().code,
        ErrorCode::IoError
    );
    assert_eq!(
        servers
            .connect(McpConnectParams {
                server_id: jira.clone(),
                redirect_uri: "http://127.0.0.1:5000/callback".into(),
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::IoError
    );
}

// ---- The production client: curl ----

fn bearer_request(url: &str) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: vec![
            ("Authorization".into(), format!("Bearer {SENTINEL}")),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: format!("{{\"secret\":\"{SENTINEL}\"}}").into_bytes(),
    }
}

#[test]
fn curl_argv_carries_no_secret() {
    let request = bearer_request("https://example.com/mcp");
    let invocation =
        CurlClient::invocation(&request, "203.0.113.7".parse().unwrap(), false).unwrap();
    for arg in &invocation.args {
        assert!(!arg.contains(SENTINEL), "argv leaks: {arg}");
    }
    assert_eq!(invocation.args[0], "-q");
    for flag in ["--max-redirs", "--max-filesize", "--max-time", "--proto"] {
        assert!(invocation.args.iter().any(|a| a == flag), "{flag}");
    }
    let value = |flag: &str| {
        let at = invocation.args.iter().position(|a| a == flag).unwrap();
        invocation.args[at + 1].clone()
    };
    assert_eq!(value("--proto"), "=https");
    assert_eq!(value("--max-filesize"), "4194304");
    assert_eq!(value("--max-time"), "60");
    assert_eq!(value("--max-redirs"), "0");
    assert_eq!(value("--resolve"), "example.com:443:203.0.113.7");
    assert_eq!(value("--config"), "-");
    assert!(!invocation
        .args
        .iter()
        .any(|a| a == "-L" || a == "--location"));
    // The header rides on stdin, the body in a private file.
    assert!(invocation.config.contains(SENTINEL));
    let body = invocation.body_path().unwrap().to_owned();
    assert_eq!(value("--data-binary"), format!("@{}", body.display()));
    assert!(std::fs::read_to_string(&body).unwrap().contains(SENTINEL));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&body).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(invocation);
    assert!(!body.exists(), "the body file outlived the request");

    let streamed = CurlClient::invocation(&request, "203.0.113.7".parse().unwrap(), true).unwrap();
    let at = streamed
        .args
        .iter()
        .position(|a| a == "--max-time")
        .unwrap();
    assert_eq!(streamed.args[at + 1], "600");
}

/// What a loopback fake server saw: the request head and body.
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Serves every connection with `response`, then holds the connection open
/// for `hold` (a server that keeps its event stream open).
async fn loopback(response: Vec<u8>, hold: std::time::Duration) -> (u16, Seen) {
    std::env::set_var("BS_MCP_ALLOW_HTTP", "1");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Default::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let kept = kept.clone();
            let response = response.clone();
            tokio::spawn(async move {
                let mut bytes = vec![];
                let mut chunk = [0; 4096];
                let head_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while bytes.len() < head_end + length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                kept.lock()
                    .unwrap()
                    .push((head, bytes[head_end..].to_vec()));
                socket.write_all(&response).await.unwrap();
                tokio::time::sleep(hold).await;
            });
        }
    });
    (port, seen)
}

#[tokio::test]
async fn curl_client_posts_headers_and_body_over_loopback() {
    let (port, seen) = loopback(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}".to_vec(),
        std::time::Duration::ZERO,
    )
    .await;
    let response = CurlClient
        .send(
            bearer_request(&format!("http://127.0.0.1:{port}/mcp")),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"{\"ok\":true}");
    let seen = seen.lock().unwrap();
    let (head, body) = &seen[0];
    assert!(head.starts_with("POST /mcp "), "{head}");
    assert!(head.contains(&format!("Authorization: Bearer {SENTINEL}")));
    assert_eq!(body, format!("{{\"secret\":\"{SENTINEL}\"}}").as_bytes());
}

#[tokio::test]
async fn curl_client_does_not_follow_redirects() {
    let (port, seen) = loopback(
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        std::time::Duration::ZERO,
    )
    .await;
    let response = CurlClient
        .send(
            HttpRequest {
                method: "GET".into(),
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: vec![],
                body: vec![],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 302);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

struct NoServerRequests;
#[async_trait]
impl McpEventHandler for NoServerRequests {
    async fn handle_request(&self, _: &Value, _: &[(String, String)]) -> Result<bool, RpcError> {
        Ok(false)
    }
}

#[tokio::test]
async fn curl_post_stream_completes_at_the_matching_reply() {
    // The server never closes its stream: the call must end at reply 7.
    let (port, _seen) = loopback(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\r\n\r\n".to_vec(),
        std::time::Duration::from_secs(120),
    )
    .await;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        CurlClient.send_mcp(
            HttpRequest {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: br#"{"jsonrpc":"2.0","id":7,"method":"tools/call"}"#.to_vec(),
            },
            Some(json!(7)),
            &NoServerRequests,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the stream was not completed at its reply")
    .unwrap();
    let mut decoder = SseDecoder::default();
    let messages = decoder.push(&response.body).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["id"], 7);
}

#[tokio::test]
async fn curl_request_is_cancellable() {
    let (port, _seen) = loopback(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec(),
        std::time::Duration::from_secs(120),
    )
    .await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        CurlClient.send_mcp(
            HttpRequest {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#.to_vec(),
            },
            Some(json!(1)),
            &NoServerRequests,
            cancel,
        ),
    )
    .await
    .expect("cancellation did not end the request");
    assert!(result.is_err());
}
