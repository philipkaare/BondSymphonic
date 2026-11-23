//! A connection to a host the workspace has not allowed is held while the
//! user is asked, rather than refused before anyone could answer. An Allow in
//! that time lets the very request through; a Block ends it at once; no
//! answer ends it with a refusal that says the user was asked.
#![cfg(unix)]

use bondsymphonic_daemon::net::allowlist::Allowlist;
use bondsymphonic_daemon::net::proxy::ProxyRegistry;
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_proto::{ServerMessage, WorkspaceId};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A proxy for one workspace that holds refused connections for `wait`, an
/// upstream on loopback for it to reach once allowed, and the request that
/// names that upstream.
struct Fixture {
    _dir: tempfile::TempDir,
    proxy: ProxyRegistry,
    id: WorkspaceId,
    sock: std::path::PathBuf,
    events: tokio::sync::broadcast::Receiver<ServerMessage>,
    request: String,
}

async fn fixture(wait: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let id = WorkspaceId("ws_approval".into());
    let bus = EventBus::new(100);
    let events = bus.subscribe();
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = upstream.local_addr().unwrap().port();
    // Accepts and holds whatever arrives; only the tunnel's 200 matters here.
    tokio::spawn(async move {
        while let Ok((conn, _)) = upstream.accept().await {
            tokio::spawn(async move {
                let _held = conn;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    let proxy = ProxyRegistry::default().with_approval_wait(wait);
    proxy
        .start(&id, &sock, Allowlist::from_strings(&[]), bus)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        proxy,
        id,
        sock,
        events,
        request: format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    }
}

/// Sends the request and reads the proxy's first answer: the tunnel's 200, or
/// a refusal.
async fn ask(f: &Fixture) -> tokio::task::JoinHandle<(String, Duration)> {
    let sock = f.sock.clone();
    let request = f.request.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let mut c = tokio::net::UnixStream::connect(&sock).await.unwrap();
        c.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = c.read(&mut buf).await.unwrap();
        (
            String::from_utf8_lossy(&buf[..n]).into_owned(),
            started.elapsed(),
        )
    })
}

/// Waits until the denial for the held connection has been published: the
/// user is being asked from that moment.
async fn denial_published(f: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Ok(message) = f.events.try_recv() {
            if let ServerMessage::Event { event, .. } = message {
                if event.denied_host() == Some("127.0.0.1") {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the held connection was never announced");
}

#[tokio::test]
async fn an_allow_during_the_wait_lets_the_held_request_through() {
    let mut f = fixture(Duration::from_secs(20)).await;
    let reply = ask(&f).await;
    denial_published(&mut f).await;
    assert!(
        !reply.is_finished(),
        "the request was answered before the user"
    );
    f.proxy
        .set_allowlist(&f.id, Allowlist::from_strings(&["127.0.0.1".into()]));
    let (head, took) = reply.await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200 Connection Established"),
        "{head}"
    );
    assert!(
        took < Duration::from_secs(10),
        "the allow did not wake it: {took:?}"
    );
    f.proxy.stop(&f.id);
}

#[tokio::test]
async fn a_block_during_the_wait_refuses_at_once() {
    let mut f = fixture(Duration::from_secs(20)).await;
    let reply = ask(&f).await;
    denial_published(&mut f).await;
    f.proxy.set_allowlist(
        &f.id,
        Allowlist::from_strings(&[]).with_blocks(&["127.0.0.1".into()]),
    );
    let (head, took) = reply.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(
        head.contains("blocked by a workspace network rule"),
        "{head}"
    );
    assert!(
        took < Duration::from_secs(10),
        "the block did not wake it: {took:?}"
    );
    f.proxy.stop(&f.id);
}

#[tokio::test]
async fn no_answer_refuses_when_the_wait_runs_out_and_says_so() {
    let mut f = fixture(Duration::from_millis(600)).await;
    let reply = ask(&f).await;
    denial_published(&mut f).await;
    let (head, took) = reply.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(head.contains("the user was asked to allow it"), "{head}");
    assert!(
        took >= Duration::from_millis(500),
        "refused before the wait: {took:?}"
    );
    f.proxy.stop(&f.id);
}

/// A change to the rules that does not decide this host wakes the connection
/// and puts it back to waiting, rather than refusing it.
#[tokio::test]
async fn an_unrelated_change_keeps_the_connection_waiting() {
    let mut f = fixture(Duration::from_secs(20)).await;
    let reply = ask(&f).await;
    denial_published(&mut f).await;
    f.proxy
        .set_allowlist(&f.id, Allowlist::from_strings(&["example.com".into()]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !reply.is_finished(),
        "an unrelated change answered the request"
    );
    f.proxy
        .set_allowlist(&f.id, Allowlist::from_strings(&["127.0.0.1".into()]));
    let (head, _) = reply.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    f.proxy.stop(&f.id);
}
