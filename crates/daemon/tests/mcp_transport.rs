use async_trait::async_trait;
use bondsymphonic_daemon::mcp::transport::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
struct Peer(Mutex<Vec<HttpRequest>>);
#[async_trait]
impl HttpClient for Peer {
    async fn execute(
        &self,
        request: HttpRequest,
    ) -> Result<HttpResponse, bondsymphonic_proto::RpcError> {
        self.0.lock().unwrap().push(request.clone());
        let input: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
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
    let a = RemoteMcpSession::new("https://example.com/mcp".into(), vec![], peer.clone()).unwrap();
    let b = RemoteMcpSession::new("https://example.com/mcp".into(), vec![], peer.clone()).unwrap();
    let init = json!({"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-11-25"}});
    a.send(init.clone()).await.unwrap();
    b.send(init).await.unwrap();
    a.send(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .unwrap();
    b.send(json!({"jsonrpc":"2.0","id":"eight","method":"tools/list"}))
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
fn private_addresses_and_url_credentials_rejected() {
    for u in [
        "http://example.com/mcp",
        "https://127.0.0.1/mcp",
        "https://[::1]/mcp",
        "https://user:password@example.com/mcp",
        "https://10.0.0.1/mcp",
    ] {
        assert!(HttpsUrl::parse(u).is_err(), "{u}");
    }
}
struct Redirect;
#[async_trait]
impl HttpClient for Redirect {
    async fn execute(&self, _: HttpRequest) -> Result<HttpResponse, bondsymphonic_proto::RpcError> {
        Ok(HttpResponse {
            status: 302,
            headers: vec![("location".into(), "https://other.example/mcp".into())],
            body: vec![],
        })
    }
}
#[tokio::test]
async fn redirect_cannot_escape_origin() {
    let session =
        RemoteMcpSession::new("https://example.com/mcp".into(), vec![], Arc::new(Redirect))
            .unwrap();
    assert!(session
        .send(json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .await
        .is_err());
}
#[test]
fn native_sha256_known_vector() {
    assert_eq!(
        hex::encode(native_sha256(b"abc").unwrap()),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn configured_headers_cannot_replace_transport_headers() {
    for name in [
        "Mcp-Session-Id",
        "MCP-Protocol-Version",
        "Host",
        "Content-Length",
        "Transfer-Encoding",
    ] {
        assert!(
            RemoteMcpSession::new(
                "https://example.com/mcp".into(),
                vec![(name.into(), "attacker".into())],
                Arc::new(Redirect)
            )
            .is_err(),
            "{name}"
        );
    }
}

#[test]
fn sse_mixed_delimiters_preserve_event_order() {
    let mut decoder = SseDecoder::default();
    let messages = decoder.push(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"two\",\"result\":{}}\n\n").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(messages[1]["id"], "two");
}

struct Reply {
    id: serde_json::Value,
    version: &'static str,
}
#[async_trait]
impl HttpClient for Reply {
    async fn execute(
        &self,
        request: HttpRequest,
    ) -> Result<HttpResponse, bondsymphonic_proto::RpcError> {
        let message: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        Ok(HttpResponse { status: 200, headers: vec![("Content-Type".into(), "application/json".into())], body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id": if message["method"] == "initialize" {message["id"].clone()} else {self.id.clone()},"result":{"protocolVersion":self.version}})).unwrap() })
    }
}

#[tokio::test]
async fn unrelated_response_id_is_rejected() {
    let session = RemoteMcpSession::new(
        "https://example.com/mcp".into(),
        vec![],
        Arc::new(Reply {
            id: json!("8"),
            version: "2025-11-25",
        }),
    )
    .unwrap();
    session.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await.unwrap();
    assert!(session
        .send(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .is_err());
}

#[tokio::test]
async fn older_streamable_era_is_negotiated() {
    let session = RemoteMcpSession::new(
        "https://example.com/mcp".into(),
        vec![],
        Arc::new(Reply {
            id: json!(8),
            version: "2025-06-18",
        }),
    )
    .unwrap();
    session.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await.unwrap();
    session
        .send(json!({"jsonrpc":"2.0","id":8,"method":"tools/list"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn modern_only_era_is_explicitly_rejected() {
    let session = RemoteMcpSession::new(
        "https://example.com/mcp".into(),
        vec![],
        Arc::new(Reply {
            id: json!(8),
            version: "2026-07-28",
        }),
    )
    .unwrap();
    let error = session.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await.unwrap_err();
    assert!(error.message.contains("unsupported_protocol_version"));
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

struct ExpiredSessionPeer(Mutex<Vec<HttpRequest>>);
#[async_trait]
impl HttpClient for ExpiredSessionPeer {
    async fn execute(
        &self,
        request: HttpRequest,
    ) -> Result<HttpResponse, bondsymphonic_proto::RpcError> {
        let message: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
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
    let session =
        RemoteMcpSession::new("https://example.com/mcp".into(), vec![], peer.clone()).unwrap();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}});
    session.send(init.clone()).await.unwrap();
    let error = session
        .send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mutate"}}))
        .await
        .unwrap_err();
    assert!(error.message.contains("session_expired"));
    assert_eq!(peer.0.lock().unwrap().len(), 2);
    session.send(init).await.unwrap();
    let requests = peer.0.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[2]
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id")));
    assert_eq!(
        requests
            .iter()
            .filter(
                |r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["method"]
                    == "tools/call"
            )
            .count(),
        1
    );
}
