//! A network denial says whose sandbox asked, so the IDE can put it in the
//! transcript of the agent that tried instead of a toast nobody connects with
//! it. Each proxy knows which sandbox it serves -- the workspace's own, which
//! Claude agents share with terminals and runs, or one backend's companion --
//! and every notice it publishes carries that.

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::net::proxy::ProxyRegistry;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{agent_sandboxes::companion_proxy, DataDirs};
use bondsymphonic_proto::AgentAdapterKind;

/// The workspace's own proxy serves the workspace sandbox, where Claude agents
/// run; each companion's proxy serves its backend. Nothing else does.
#[test]
fn every_proxy_knows_whose_sandbox_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("noop"),
        EventBus::new(10),
    )
    .unwrap();
    assert_eq!(d.proxies.backend(), Some(AgentAdapterKind::Claude));
    for kind in [AgentAdapterKind::Codex, AgentAdapterKind::OpenCode] {
        assert_eq!(companion_proxy(kind).backend(), Some(kind));
    }
    assert_eq!(ProxyRegistry::default().backend(), None);
}

/// Sends `request` to a proxy for `backend` (or for nobody) holding `allow`,
/// and returns the events that caused.
#[cfg(unix)]
async fn refused(
    backend: Option<AgentAdapterKind>,
    allow: bondsymphonic_daemon::net::allowlist::Allowlist,
    request: &str,
) -> Vec<bondsymphonic_proto::Event> {
    use bondsymphonic_proto::{ServerMessage, WorkspaceId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let id = WorkspaceId("ws_attr".into());
    let events = EventBus::new(100);
    let mut rx = events.subscribe();
    let proxy = match backend {
        Some(kind) => ProxyRegistry::for_backend(kind),
        None => ProxyRegistry::default(),
    };
    proxy.start(&id, &sock, allow, events).await.unwrap();
    let mut c = tokio::net::UnixStream::connect(&sock).await.unwrap();
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    let reply = String::from_utf8_lossy(&out);
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    proxy.stop(&id);
    let mut seen = vec![];
    while let Ok(ServerMessage::Event { event, .. }) = rx.try_recv() {
        seen.push(event);
    }
    seen
}

/// A denial from each kind of proxy names that proxy's backend, and a proxy
/// that was never told publishes the denial as it always did.
#[cfg(unix)]
#[tokio::test]
async fn a_denial_carries_the_backend_of_the_proxy_that_refused_it() {
    use bondsymphonic_daemon::net::allowlist::Allowlist;
    for backend in [
        Some(AgentAdapterKind::Claude),
        Some(AgentAdapterKind::Codex),
        Some(AgentAdapterKind::OpenCode),
        None,
    ] {
        let events = refused(
            backend,
            Allowlist::from_strings(&[]),
            "CONNECT blocked.example:443 HTTP/1.1\r\n\r\n",
        )
        .await;
        let denials: Vec<_> = events
            .iter()
            .filter_map(|e| e.denied_host().map(|h| (h.to_owned(), e.denial_backend())))
            .collect();
        assert_eq!(
            denials,
            vec![("blocked.example".to_owned(), backend)],
            "{events:?}"
        );
    }
}

/// The OpenCode companion's refused tunnel keeps its no-Allow wording, and now
/// says which host and which backend, so the IDE can show it beside the agent.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_tunnel_names_its_host_and_backend_without_offering_allow() {
    use bondsymphonic_daemon::net::allowlist::{Allowlist, Origin};
    use bondsymphonic_daemon::net::shim;
    let allow =
        Allowlist::from_strings(&[]).with_origins(vec![Origin::new("localhost", 11434).unwrap()]);
    let events = refused(
        Some(AgentAdapterKind::OpenCode),
        allow,
        &format!(
            "CONNECT exfil.example:443 HTTP/1.1\r\n{}: 1\r\n\r\n",
            shim::TUNNEL_HEADER
        ),
    )
    .await;
    assert!(
        events.iter().all(|e| e.denied_host().is_none()),
        "{events:?}"
    );
    let tunnels: Vec<_> = events
        .iter()
        .filter_map(|e| {
            e.refused_tunnel_host()
                .map(|h| (h.to_owned(), e.denial_backend()))
        })
        .collect();
    assert_eq!(
        tunnels,
        vec![("exfil.example".to_owned(), Some(AgentAdapterKind::OpenCode))],
        "{events:?}"
    );
}
