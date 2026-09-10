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
        vec![(Some(ws.id.clone()), "127.0.0.1".to_string())],
        "one denial, tagged with the workspace it came from"
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
