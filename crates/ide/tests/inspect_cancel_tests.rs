//! Cancelling a `repo.inspect` the New Agent dialog is waiting on.
//!
//! On a repository reached through `/mnt/c` the inspection can take minutes,
//! and the dialog offers a Stop beside "Reading repository…". What Stop has to
//! guarantee is on this side of the wire: the wait ends at once, and the answer
//! the daemon sends later -- it has no way to be told to stop -- is dropped
//! rather than delivered to whatever the dialog is showing by then.
//!
//! The controller runs every inspection through one [`LatestOnly`]; these
//! drive the same thing against a fake daemon that holds `repo.inspect` for as
//! long as the test says.

use bondsymphonic_ide::client::{DaemonClient, LatestOnly};
use bondsymphonic_proto::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

/// The path the fake daemon sits on until `release` is notified.
const SLOW: &str = "/mnt/c/slow";

fn info_for(path: &str) -> RepoInfo {
    RepoInfo {
        default_branch: "main".into(),
        // The path itself, so an answer can be traced back to its question.
        branches: vec![path.to_owned()],
        is_dirty: false,
        remotes: vec![],
        is_repo: true,
        exists: true,
        head_branch: Some("main".into()),
        in_place_refusal: None,
        hooks_path_in_tree: None,
    }
}

/// Answers `hello`, answers `repo.inspect` of any path but [`SLOW`] at once,
/// and answers [`SLOW`] only once `release` is notified -- each request on a
/// task of its own, the way the real daemon handles them, so a held inspection
/// does not hold up the ones behind it.
async fn fake_daemon(release: Arc<Notify>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = stream.into_split();
        let w = Arc::new(Mutex::new(w));
        let mut r = BufReader::new(r);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let ClientMessage::Request { id, request } = codec::decode(line.trim_end()).unwrap();
            let w = w.clone();
            let release = release.clone();
            tokio::spawn(async move {
                let msg = match request {
                    Request::Hello(_) => ServerMessage::ok(
                        id,
                        &HelloResult {
                            daemon_version: "9.9.9".into(),
                            capabilities: Capabilities {
                                backends: vec![],
                                sandbox_backend: "noop".into(),
                                git_protect: false,
                                adapters: vec![],
                                opencode_providers: false,
                            },
                            protocol_version: Some(PROTOCOL_VERSION),
                        },
                    ),
                    Request::RepoInspect(p) => {
                        if p.path == SLOW {
                            release.notified().await;
                        }
                        ServerMessage::ok(id, &info_for(&p.path))
                    }
                    other => ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    ),
                };
                let _ = w
                    .lock()
                    .await
                    .write_all(codec::encode(&msg).as_bytes())
                    .await;
            });
        }
    });
    addr
}

fn inspect(client: &DaemonClient, path: &str) -> impl std::future::Future<Output = RepoInfo> {
    let client = client.clone();
    let path = path.to_owned();
    async move {
        client
            .request::<RepoInfo>(Request::RepoInspect(RepoPathParams { path }))
            .await
            .expect("the fake daemon answers every inspection")
    }
}

/// Stop ends the wait now, not when the daemon gets round to answering, and
/// the answer that does come later reaches nobody.
#[tokio::test]
async fn a_cancelled_inspection_ends_at_once_and_its_late_answer_is_dropped() {
    let release = Arc::new(Notify::new());
    let addr = fake_daemon(release.clone()).await;
    let (client, _hello, _events) = DaemonClient::connect(addr, "t", "0.1.0").await.unwrap();

    let latest = LatestOnly::default();
    let ticket = latest.begin();
    let waiting = tokio::spawn(ticket.run(inspect(&client, SLOW)));

    // Long enough for the request to be on the wire and the daemon sitting on it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished(), "the slow inspection is still out");

    latest.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("a cancelled inspection must stop waiting at once")
        .unwrap();
    assert!(outcome.is_none(), "a cancelled inspection delivers nothing");

    // The daemon answers anyway -- it cannot be told to stop. That answer must
    // not turn up as the answer to the next question.
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let next = latest.begin();
    let answer = tokio::time::timeout(Duration::from_secs(2), next.run(inspect(&client, "/fast")))
        .await
        .expect("the connection is still usable after a cancel")
        .expect("the newest inspection is delivered");
    assert_eq!(answer.branches, vec!["/fast".to_owned()]);
    assert!(client.is_connected());
}

/// Picking another folder while the first is still being read: the first
/// answer, whenever it lands, is not the dialog's any more.
#[tokio::test]
async fn a_newer_inspection_supersedes_the_one_still_out() {
    let release = Arc::new(Notify::new());
    let addr = fake_daemon(release.clone()).await;
    let (client, _hello, _events) = DaemonClient::connect(addr, "t", "0.1.0").await.unwrap();

    let latest = LatestOnly::default();
    let first = tokio::spawn(latest.begin().run(inspect(&client, SLOW)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = latest.begin();

    let stale = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("a superseded inspection stops waiting at once")
        .unwrap();
    assert!(stale.is_none());

    let answer = second.run(inspect(&client, "/other")).await.unwrap();
    assert_eq!(answer.branches, vec!["/other".to_owned()]);
    release.notify_one();
}

/// An answer that is already in hand when the cancel lands is dropped too:
/// the check is made after the answer, not only while waiting for it. This is
/// also what the controller re-checks on the Qt thread before emitting, where
/// the dialog's Stop runs.
#[tokio::test]
async fn an_answer_that_lands_with_the_cancel_is_still_dropped() {
    let latest = LatestOnly::default();
    let ticket = latest.begin();
    assert!(ticket.is_current());
    latest.cancel();
    assert!(!ticket.is_current());
    assert!(ticket.run(async { 7 }).await.is_none());

    let fresh = latest.begin();
    assert_eq!(fresh.run(async { 8 }).await, Some(8));
}
