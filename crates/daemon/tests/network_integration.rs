#![cfg(unix)]
//! The allowlisting proxy end to end: over the workspace's own socket on any
//! Unix host, and through the in-sandbox shim where bubblewrap works.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::{backend_for, SandboxCommand};
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{create_ws, start_daemon, Client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A stand-in for "the internet": answers any request with a 200 and `ok`, then
/// closes. Bound on loopback, so no test ever leaves the machine.
async fn ok_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                // Read the request head, so the client is not answered before
                // it has finished asking.
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = s.flush().await;
            });
        }
    });
    port
}

/// Reads until the response head is complete, then to end of stream. Returns
/// the whole thing as text, which is what every assertion here is about.
async fn read_all(s: &mut UnixStream) -> String {
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads exactly the response head (up to the blank line), leaving whatever
/// follows on the stream — a tunnel's payload, in the CONNECT case.
async fn read_head(s: &mut UnixStream) -> String {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while !out.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await.unwrap();
        assert!(
            n > 0,
            "stream closed mid-head: {:?}",
            String::from_utf8_lossy(&out)
        );
        out.push(byte[0]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn set_allowlist(c: &mut Client, id: &WorkspaceId, hosts: &[&str]) -> Result<(), RpcError> {
    c.call(Request::WorkspaceSetAllowlist(
        WorkspaceSetAllowlistParams {
            workspace_id: id.clone(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
        },
    ))
    .await
    .map(|_| ())
}

/// The proxy is reachable on the workspace's own socket whatever the backend
/// is, so this covers every host: the shim in front of it is what needs a
/// sandbox, and it gets its own test below.
#[tokio::test]
async fn the_proxy_forwards_allowed_hosts_and_refuses_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "net").await;
    let upstream = ok_server().await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    assert!(
        sock.exists(),
        "the proxy socket must exist while the workspace does"
    );

    // The workspace's own Creating/Ready events are not what this is about.
    c.drain_events();
    // The allowlist the IDE writes replaces the effective list outright, and
    // the change is announced so every client's host list stays in step.
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();
    let announced: Vec<Vec<String>> = c
        .drain_events()
        .into_iter()
        .filter_map(|(_, e)| match e {
            Event::WorkspaceStateChanged { info } if info.id == ws.id => Some(info.allowlist),
            _ => None,
        })
        .collect();
    assert_eq!(announced, vec![vec!["127.0.0.1".to_string()]]);
    let got: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(got.allowlist, vec!["127.0.0.1".to_string()]);

    // An absolute-URI request is rewritten to origin form and forwarded.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://127.0.0.1:{upstream}/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_all(&mut s).await;
    assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
    assert!(reply.ends_with("ok"), "{reply}");

    // CONNECT opens a tunnel the client then speaks HTTP over itself.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(format!("CONNECT 127.0.0.1:{upstream} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let head = read_head(&mut s).await;
    assert!(
        head.starts_with("HTTP/1.1 200 Connection Established"),
        "{head}"
    );
    s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let tunnelled = read_all(&mut s).await;
    assert!(tunnelled.ends_with("ok"), "{tunnelled}");

    // Emptying the list takes the host away again, and the refusal is both an
    // answer to the client and a notice the IDE can turn into "Allow host".
    set_allowlist(&mut c, &ws.id, &[]).await.unwrap();
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(format!("CONNECT 127.0.0.1:{upstream} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let denied = read_all(&mut s).await;
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert!(denied.contains("127.0.0.1"), "{denied}");
    assert!(denied.contains("[network] allow"), "{denied}");
    // A fully qualified spelling is denied as the host it names, without the
    // root dot: the toast's one-click Allow has to produce an entry that
    // matches the next request, however the sandbox spelled this one.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(b"CONNECT Registry.Example.:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let denied = read_all(&mut s).await;
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert!(denied.contains("registry.example"), "{denied}");
    // A round trip to the daemon, so the denial event has certainly been
    // delivered by the time the events are drained.
    let _ = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let denials: Vec<(Option<WorkspaceId>, String)> = c
        .drain_events()
        .into_iter()
        .filter_map(|(w, e)| e.denied_host().map(|h| (w, h.to_string())))
        .collect();
    assert_eq!(
        denials,
        vec![
            (Some(ws.id.clone()), "127.0.0.1".to_string()),
            (Some(ws.id.clone()), "registry.example".to_string()),
        ],
        "one denial per host, tagged with the workspace it came from, in normalised form"
    );

    // A pattern that is not a host is refused rather than silently dropped, so
    // the IDE can put the mistake in front of whoever typed it.
    let err = set_allowlist(&mut c, &ws.id, &["github.com", "a/b"])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("a/b"), "{}", err.message);
    // ... and nothing was stored: the list is replaced as a whole or not at all.
    let got: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(got.allowlist.is_empty(), "{:?}", got.allowlist);

    // Destroying the workspace takes its listener and its socket with it.
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    assert!(
        !sock.exists(),
        "the proxy socket must go with the workspace"
    );
    assert!(UnixStream::connect(&sock).await.is_err());
    cancel.cancel();
}

/// The allowlist holds names, and a name is not a destination.
///
/// A repository the user has not read can put `assets.example.test` on the list
/// at creation and point it at the host's loopback or at `169.254.169.254`.
/// `localhost` stands in for that here: an ordinary name, on the list, that
/// resolves somewhere the sandbox must not reach. The `["127.0.0.1"]` case
/// above is the other half of the rule - an address written out is a person
/// saying they meant it.
#[tokio::test]
async fn a_name_that_resolves_to_a_private_address_is_refused_and_bad_targets_are_not_denials() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "private").await;
    let upstream = ok_server().await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    c.drain_events();

    // The name is on the list, and the proxy still refuses where it points.
    set_allowlist(&mut c, &ws.id, &["localhost"]).await.unwrap();
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://localhost:{upstream}/ HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let reply = read_all(&mut s).await;
    assert!(reply.starts_with("HTTP/1.1 403 Forbidden"), "{reply}");
    assert!(
        reply.contains("private address"),
        "the body must say why, not repeat the allowlist advice: {reply}"
    );

    // The same request again inside the coalescing interval: still refused,
    // still 403, but the IDE is told once.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://localhost:{upstream}/ HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let again = read_all(&mut s).await;
    assert!(again.starts_with("HTTP/1.1 403 Forbidden"), "{again}");

    // A target that is not a hostname at all is a malformed request, not a
    // denial: `*.com` in a toast is one click from an allowlist entry covering
    // every `.com` host, and the sandbox chose that text.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(b"CONNECT *.com:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let bad = read_all(&mut s).await;
    assert!(bad.starts_with("HTTP/1.1 400 Bad Request"), "{bad}");

    // A round trip, so everything published has certainly been delivered.
    let _ = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let denials: Vec<String> = c
        .drain_events()
        .into_iter()
        .filter_map(|(_, e)| e.denied_host().map(str::to_string))
        .collect();
    assert_eq!(
        denials,
        vec!["localhost".to_string()],
        "one notice for the refused name, and nothing at all for the malformed target"
    );

    // And the allowlist itself refuses a wildcard with no registrable name
    // behind it, so the same host cannot get in by the other door.
    let err = set_allowlist(&mut c, &ws.id, &["*.com"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    // As does a list nobody could have meant to write.
    let many: Vec<String> = (0..300).map(|i| format!("h{i}.example")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let err = set_allowlist(&mut c, &ws.id, &refs).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("256"), "{}", err.message);

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A keep-alive HTTP server that remembers every request head it is sent.
///
/// Answers each request with a one-byte body and keeps the connection open
/// for the next one, closing only when the request asks it to with
/// `Connection: close` - the way every real origin behaves, and the reason the
/// proxy has to ask.
async fn recording_server() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    buf.drain(..end);
                    let close = head
                        .lines()
                        .any(|l| l.to_ascii_lowercase().trim() == "connection: close");
                    log.lock().unwrap().push(head);
                    if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                        .await
                        .is_err()
                        || close
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, seen)
}

/// Reads one complete response - the head, then as many body bytes as its
/// `Content-Length` says - and leaves the stream at the start of the next.
async fn read_response(s: &mut UnixStream) -> (String, String) {
    let head = read_head(s).await;
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().unwrap())
        })
        .unwrap_or_else(|| panic!("no Content-Length in {head:?}"));
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.unwrap();
    (head, String::from_utf8_lossy(&body).into_owned())
}

/// A keep-alive client connection is a sequence of requests, and each one is
/// checked and routed on its own.
///
/// The first proxy pinned a connection to the upstream its first request
/// named and piped everything after it there verbatim: a second request for
/// another allowed host, credentials and all, was delivered to the first one.
/// Two hosts the workspace may reach are two trust decisions, not one.
#[tokio::test]
async fn each_plain_http_request_on_a_kept_alive_connection_goes_to_its_own_host() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "keepalive").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    let (a_port, a_seen) = recording_server().await;
    let (b_port, b_seen) = recording_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    // One connection, two requests, one after the other.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://127.0.0.1:{a_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{a_port}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let (head, body) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "a");
    s.write_all(
        format!(
            "GET http://127.0.0.1:{b_port}/secret HTTP/1.1\r\nHost: 127.0.0.1:{b_port}\r\nAuthorization: Bearer b-token\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let (head, body) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "a");
    drop(s);

    let a: Vec<String> = a_seen.lock().unwrap().clone();
    let b: Vec<String> = b_seen.lock().unwrap().clone();
    assert_eq!(a.len(), 1, "A saw exactly its own request: {a:?}");
    assert!(a[0].starts_with("GET / HTTP/1.1\r\n"), "{:?}", a[0]);
    assert!(
        !a[0].contains("b-token") && !a[0].contains("/secret"),
        "B's request must never reach A: {:?}",
        a[0]
    );
    assert_eq!(b.len(), 1, "B saw exactly its own request: {b:?}");
    assert!(b[0].starts_with("GET /secret HTTP/1.1\r\n"), "{:?}", b[0]);
    assert!(b[0].contains("Authorization: Bearer b-token"), "{:?}", b[0]);
    // Each request is sent as the last one on its upstream connection.
    for head in a.iter().chain(&b) {
        assert!(
            head.to_ascii_lowercase()
                .contains("\r\nconnection: close\r\n"),
            "{head:?}"
        );
    }

    // The same two requests pipelined - sent together, before either answer -
    // still arrive at their own hosts and are answered in order.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!(
            "GET http://127.0.0.1:{a_port}/one HTTP/1.1\r\nHost: 127.0.0.1:{a_port}\r\n\r\n\
             POST http://127.0.0.1:{b_port}/two HTTP/1.1\r\nHost: 127.0.0.1:{b_port}\r\nContent-Length: 3\r\n\r\nxyz"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let (head, body) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "a");
    let (head, body) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "a");
    drop(s);
    let a: Vec<String> = a_seen.lock().unwrap().clone();
    let b: Vec<String> = b_seen.lock().unwrap().clone();
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(a[1].starts_with("GET /one HTTP/1.1\r\n"), "{:?}", a[1]);
    assert_eq!(b.len(), 2, "{b:?}");
    assert!(b[1].starts_with("POST /two HTTP/1.1\r\n"), "{:?}", b[1]);

    // A second request for a host that is *not* allowed is refused on its own,
    // after the first was served, and the refusal closes the connection.
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://127.0.0.1:{a_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{a_port}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let (head, _) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    s.write_all(b"GET http://denied.example/ HTTP/1.1\r\nHost: denied.example\r\n\r\n")
        .await
        .unwrap();
    let denied = read_all(&mut s).await;
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert!(denied.contains("denied.example"), "{denied}");
    assert_eq!(
        a_seen.lock().unwrap().len(),
        3,
        "A served the first request only"
    );

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A bare LF inside a header is a second request, and it must not reach the
/// origin the first one was cleared for.
///
/// The proxy ends a head at the blank line and splits it on CRLF, so an LF
/// with no CR in front of it is not a line terminator there: it stays inside a
/// header value rather than ending the line it sits on.
/// An origin that treats a bare LF as a line terminator - RFC 9112 §2.2 says
/// many do - then reads two requests where the allowlist checked one, and the
/// second names its own `Host`. Both halves of the assertion matter: the
/// client is told 400, and the upstream never hears from us at all.
#[tokio::test]
async fn a_header_with_a_bare_lf_is_refused_before_any_origin_is_reached() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "smuggle").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    // Two origins, both allowed: the one the visible request names, and the
    // one the request hidden after the LF names. Neither may see a byte.
    let (a_port, a_seen) = recording_server().await;
    let (b_port, b_seen) = recording_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!(
            "GET http://127.0.0.1:{a_port}/ HTTP/1.1\r\n\
             Host: 127.0.0.1:{a_port}\r\n\
             X: v\n\nGET http://127.0.0.1:{b_port}/smuggled HTTP/1.1\nHost: 127.0.0.1:{b_port}\r\n\
             \r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let answer = read_all(&mut s).await;
    assert!(
        answer.starts_with("HTTP/1.1 400 Bad Request"),
        "a head carrying a bare LF is refused: {answer}"
    );
    drop(s);

    let a: Vec<String> = a_seen.lock().unwrap().clone();
    let b: Vec<String> = b_seen.lock().unwrap().clone();
    assert!(a.is_empty(), "the named origin saw nothing: {a:?}");
    assert!(b.is_empty(), "the smuggled origin saw nothing: {b:?}");

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A listener that accepts a connection and then drops it without a byte.
///
/// The one failure the close-delimited design creates: the proxy relays a
/// response to end of stream, so an upstream whose end of stream comes first
/// would leave the client waiting on a connection the proxy thinks is idle.
async fn silent_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            drop(s);
        }
    });
    port
}

/// An upstream that takes the connection and then says nothing is answered,
/// not waited on.
#[tokio::test]
async fn an_upstream_that_answers_nothing_is_a_502_and_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "silent").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    let upstream = silent_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!("GET http://127.0.0.1:{upstream}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let answer = read_all(&mut s).await;
    assert!(answer.starts_with("HTTP/1.1 502 Bad Gateway"), "{answer:?}");

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A connection is a sequence of trust decisions, and the allowlist it is
/// decided against is the one live at the time.
///
/// A host taken off the list while a plain-HTTP connection is open must not
/// keep being reachable on it until the client happens to hang up: the agent
/// holding that connection is the one the user just narrowed the list against.
#[tokio::test]
async fn an_allowlist_narrowed_mid_connection_binds_the_next_request_on_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "relist").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    let (upstream, seen) = recording_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    let mut s = UnixStream::connect(&sock).await.unwrap();
    let request =
        format!("GET http://127.0.0.1:{upstream}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\n\r\n");
    s.write_all(request.as_bytes()).await.unwrap();
    let (head, _) = read_response(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");

    // The user takes the host off the list while the connection is still open.
    set_allowlist(&mut c, &ws.id, &["example.test"])
        .await
        .unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let refused = read_all(&mut s).await;
    assert!(refused.starts_with("HTTP/1.1 403 Forbidden"), "{refused:?}");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the second request reached the host after it was taken off the list"
    );

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A server that answers the moment the request head arrives - before the body
/// it was promised - and then holds its connection open.
///
/// That is legal HTTP and ordinary in practice (a refusal, a redirect, a
/// `100-continue` decision), and it is the shape both tests below need: the
/// client can read the response and only then decide what to do with the rest
/// of its body.
async fn early_answering_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                // A head and part of the body it promises, so the client has a
                // response under way while its own request is still unfinished.
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel")
                    .await;
                let _ = s.flush().await;
                // Held open: the response is not over, so the proxy is still
                // relaying it when the client's body goes wrong or stops.
                std::future::pending::<()>().await;
            });
        }
    });
    port
}

/// A request body the proxy refuses must not put a 400 into a response that is
/// already on its way to the client.
///
/// The body and the response are relayed at the same time, so by the time a
/// malformed chunk is read the client may already have most of a 200 in hand.
/// Appending a complete 400 to that is a corrupt stream: the client reads one
/// response's head and another response's body, and has no way to tell.
#[tokio::test]
async fn a_refused_body_does_not_splice_a_400_onto_a_response_already_under_way() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "splice").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    let upstream = early_answering_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!(
            "POST http://127.0.0.1:{upstream}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    // The response is under way before anything malformed is sent.
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    // A chunk size that is not a number: the body cannot be walked any further.
    s.write_all(b"zz\r\n").await.unwrap();
    let rest = read_all(&mut s).await;
    assert!(
        !rest.contains("400 Bad Request"),
        "a 400 was spliced onto a response already under way: {rest:?}"
    );

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A client that promises a body and then stops sending it must not hold an
/// upstream socket to an allowlisted host open for ever.
///
/// Nothing else bounds this: the head timeout is spent, the connect timeout is
/// spent, and a legitimate slow upload must not be cut off - so what ends it is
/// an *idle* deadline, measured from the last byte that moved in either
/// direction rather than from the start of the request.
///
/// Time is paused once the stall is set up and then wound forward by hand, so
/// the test exercises the daemon's real deadline in milliseconds rather than
/// waiting it out - and does so the same way whether or not the machine is
/// busy, which leaving it to the runtime's own auto-advance does not.
#[tokio::test]
async fn a_request_body_that_stalls_does_not_hold_an_upstream_for_ever() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "stall").await;
    let sock = daemon.dirs.run(&ws.id).join("proxy.sock");
    let upstream = early_answering_server().await;
    set_allowlist(&mut c, &ws.id, &["127.0.0.1"]).await.unwrap();

    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(
        format!(
            "POST http://127.0.0.1:{upstream}/ HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\nContent-Length: 1000000\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    // Not one byte of the million follows. The daemon's idle deadline is a
    // minute; four of them is past two consecutive idle intervals however the
    // rounds line up, and still nothing the client has to sit through.
    tokio::time::pause();
    for _ in 0..4 {
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(30), read_all(&mut s)).await;
    assert!(
        ended.is_ok(),
        "the stalled request still held its client and its upstream four minutes later"
    );

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    cancel.cancel();
}

// ---------------------------------------------------------------------------
// The sandbox side: only where bubblewrap can create user namespaces.
// ---------------------------------------------------------------------------

fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-all",
            "--die-with-parent",
            "true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Host pids of the shims belonging to one workspace.
///
/// The command line alone would not do: every workspace's shim is spawned with
/// the same argv, and other tests run their own sandboxes in parallel. The
/// workspace id travels in the sandbox environment, so that is what identifies
/// the process.
fn shim_pids(ws: &WorkspaceId) -> Vec<i32> {
    let marker = format!("BS_WORKSPACE={ws}");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        if !cmdline.split(|b| *b == 0).any(|arg| arg == b"proxy-shim") {
            continue;
        }
        let environ = std::fs::read(e.path().join("environ")).unwrap_or_default();
        if environ.split(|b| *b == 0).any(|v| v == marker.as_bytes()) {
            out.push(pid);
        }
    }
    out
}

async fn wait_until(limit: std::time::Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline && !cond() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Runs a command inside the sandbox, merging stderr into the output so a
/// python traceback is visible when an assertion fails.
async fn run_in(
    daemon: &Daemon,
    id: &WorkspaceId,
    argv: &[&str],
    env: &[(&str, &str)],
) -> (i32, String) {
    let mut child = daemon
        .sandbox(id)
        .unwrap()
        .spawn(SandboxCommand {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .await
        .unwrap();
    let code = child.exit.await.unwrap();
    (code, format!("{out}{err}"))
}

/// The whole point of the milestone: a sandboxed process has no route to
/// anything, and the only way out is the proxy, which honours the allowlist.
#[tokio::test]
async fn a_sandboxed_process_reaches_the_host_only_through_the_allowlisting_proxy() {
    if !bwrap_available() {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        server.event_bus(),
    )
    .unwrap();
    let upstream = ok_server().await;
    let ws = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: "netbox".into(),
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

    // The environment every process in this sandbox inherits points at the shim.
    let (code, out) = run_in(
        &daemon,
        &ws.id,
        &[
            "sh",
            "-c",
            "printenv HTTP_PROXY; printenv HTTPS_PROXY; printenv NO_PROXY",
        ],
        &[],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![
            "http://127.0.0.1:3128",
            "http://127.0.0.1:3128",
            "localhost,127.0.0.1"
        ],
        "{out}"
    );

    // No route to the host at all: the upstream is on the *host's* loopback,
    // and the sandbox has a network namespace of its own.
    let (code, out) = run_in(
        &daemon,
        &ws.id,
        &[
            "bash",
            "-c",
            &format!("exec 3<>/dev/tcp/127.0.0.1/{upstream}"),
        ],
        &[],
    )
    .await;
    assert_ne!(code, 0, "the host must be unreachable directly: {out}");
    // The positive control for that probe: the same construct against the shim's
    // own port must succeed. Without it a missing bash, or a bash built without
    // `/dev/tcp`, would satisfy the assertion above just as well as a blocked
    // route does, and the probe would prove nothing.
    let (shim_host, shim_port) = lifecycle::PROXY_LISTEN
        .split_once(':')
        .expect("the shim's address is host:port");
    let probe = format!("exec 3<>/dev/tcp/{shim_host}/{shim_port}");
    let (code, out) = run_in(&daemon, &ws.id, &["bash", "-c", &probe], &[]).await;
    assert_eq!(
        code, 0,
        "bash and /dev/tcp must work in this sandbox, or the probe above proves nothing: {out}"
    );

    // Through the proxy, once the host is allowed. `NO_PROXY` is cleared for
    // this probe only: the sandbox is told to reach loopback directly so the
    // port bridge works, and the upstream here is deliberately on loopback.
    lifecycle::set_allowlist(&daemon, &ws.id, &["127.0.0.1".to_string()]).unwrap();
    let fetch = format!(
        "import urllib.request, urllib.error\n\
         try:\n    print(urllib.request.urlopen('http://127.0.0.1:{upstream}/', timeout=10).read().decode())\n\
         except urllib.error.HTTPError as e:\n    print('DENIED', e.code)\n"
    );
    let (code, out) = run_in(
        &daemon,
        &ws.id,
        &["python3", "-c", &fetch],
        &[("NO_PROXY", ""), ("no_proxy", "")],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.trim(), "ok", "{out}");

    // Take the host away again and the same fetch is refused, by the proxy.
    lifecycle::set_allowlist(&daemon, &ws.id, &[]).unwrap();
    let (code, out) = run_in(
        &daemon,
        &ws.id,
        &["python3", "-c", &fetch],
        &[("NO_PROXY", ""), ("no_proxy", "")],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.trim(), "DENIED 403", "{out}");

    // The shim is a child of the sandbox, so it goes when the sandbox goes.
    assert_eq!(shim_pids(&ws.id).len(), 1, "exactly one shim while up");
    lifecycle::destroy(&daemon, &ws.id, true).await.unwrap();
    wait_until(std::time::Duration::from_secs(10), || {
        shim_pids(&ws.id).is_empty()
    })
    .await;
    assert!(
        shim_pids(&ws.id).is_empty(),
        "the shim must die with the sandbox"
    );
}
