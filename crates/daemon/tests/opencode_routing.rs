//! OpenCode reaches the model servers its own config names, from its own
//! companion sandbox and nowhere else.
mod common;

use bondsymphonic_daemon::agents::{opencode_backend::opencode_home, opencode_config};
use bondsymphonic_daemon::net::allowlist::Origin;
use bondsymphonic_proto::AgentAdapterKind;

fn origins(list: &[Origin]) -> Vec<String> {
    list.iter().map(Origin::authority).collect()
}

/// Writes `name` into the mirrored config directory the companion reads.
fn mirror(root: &std::path::Path, name: &str, text: &str) {
    let dir = opencode_home(root).join("config/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), text).unwrap();
}

/// Every provider base URL in every mirrored config file is an origin, sorted,
/// each once. Comments and trailing
/// commas are JSONC and do not stop the file being read; a URL that is not a
/// literal http(s) URL grants nothing.
#[test]
fn provider_origins_come_from_every_mirrored_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mirror(
        root,
        "opencode.json",
        r#"{"provider": {
            "ollama": {"npm": "@ai-sdk/openai-compatible",
                       "options": {"baseURL": "http://localhost:11434/v1"}},
            "lan": {"options": {"baseURL": "http://192.168.1.20:8000/v1/"}},
            "v6": {"options": {"baseURL": "http://[::1]:8001/prefix/v1"}},
            "env": {"options": {"baseURL": "{env:MY_URL}"}},
            "none": {"options": {}}
        }}"#,
    );
    mirror(
        root,
        "opencode.jsonc",
        r#"// a comment
        {
          /* block "comment" with a "quote" */
          "provider": {
            "secure": {"api": "https://llm.internal.example/api",},
            "again": {"options": {"baseURL": "http://LOCALHOST:11434/other"}}, // same origin
            "odd": {"options": {"baseURL": "http://a.example/x//y"}},
          },
        }"#,
    );
    let got = opencode_config::provider_origins(&opencode_home(root).join("config"));
    assert_eq!(
        origins(&got),
        vec![
            "192.168.1.20:8000",
            "[::1]:8001",
            "a.example:80",
            "llm.internal.example:443",
            "localhost:11434",
        ]
    );
}

/// A config file that is not JSON grants nothing from that file and does not
/// take the others with it; no config at all grants nothing.
#[test]
fn an_unreadable_config_file_grants_nothing_from_that_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(opencode_config::provider_origins(&opencode_home(root).join("config")).is_empty());
    mirror(root, "config.json", "{ not json");
    mirror(
        root,
        "opencode.json",
        r#"{"provider": {"x": {"options": {"baseURL": "http://10.0.0.7:5000"}}}}"#,
    );
    let got = opencode_config::provider_origins(&opencode_home(root).join("config"));
    assert_eq!(origins(&got), vec!["10.0.0.7:5000"]);
}

/// The grants go to the OpenCode companion's proxy only: Claude and Codex in
/// the same workspace get nothing new, and the workspace's allowlist is
/// unchanged.
#[tokio::test]
async fn provider_grants_belong_to_the_opencode_companion_alone() {
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::noop::NoopBackend,
        server::broadcast::EventBus,
        workspace::{agent_sandboxes::companion_grants, lifecycle, DataDirs},
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let daemon = std::sync::Arc::new(
        Daemon::new(
            DataDirs::new(dir.path().join("data")),
            std::sync::Arc::new(NoopBackend),
            EventBus::new(100),
        )
        .unwrap(),
    );
    let info = lifecycle::create(
        &daemon,
        bondsymphonic_proto::WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "routing".into(),
            init_if_missing: false,
            in_place: true,
        },
    )
    .await
    .unwrap();
    mirror(
        &daemon.dirs.root,
        "opencode.json",
        r#"{"provider": {"ollama": {"options": {"baseURL": "http://localhost:11434/v1"}}}}"#,
    );
    let ws = daemon.workspace(&info.id).unwrap();
    let opencode = companion_grants(&daemon, AgentAdapterKind::OpenCode);
    assert_eq!(origins(&opencode), vec!["localhost:11434"]);
    for kind in [AgentAdapterKind::Claude, AgentAdapterKind::Codex] {
        assert!(companion_grants(&daemon, kind).is_empty(), "{kind:?}");
    }
    assert!(!ws.allowlist.iter().any(|h| h.contains("localhost")));
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
}

#[cfg(unix)]
mod unix {
    use super::*;
    use bondsymphonic_daemon::net::{allowlist::Allowlist, proxy::ProxyRegistry, shim};
    use bondsymphonic_daemon::server::broadcast::EventBus;
    use bondsymphonic_proto::WorkspaceId;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixStream};

    /// Answers any request with `ok <path>` and closes.
    pub async fn ok_server() -> u16 {
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
                    let line = String::from_utf8_lossy(&buf);
                    let path = line.split(' ').nth(1).unwrap_or("").to_string();
                    let body = format!("ok {path}");
                    let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        port
    }

    fn grant(url: &str) -> Allowlist {
        Allowlist::from_strings(&[]).with_origins(vec![Origin::from_url(url).unwrap()])
    }

    async fn ask(sock: &std::path::Path, request: String) -> String {
        let mut s = UnixStream::connect(sock).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), s.read_to_end(&mut out))
            .await
            .expect("the proxy answers rather than hanging");
        String::from_utf8_lossy(&out).into_owned()
    }

    async fn tunnel_get(sock: &std::path::Path, authority: &str, path: &str) -> String {
        ask(
            sock,
            format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\nGET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await
    }

    /// An origin grant lets the proxy reach a loopback server - by name, by
    /// tunnel and by absolute-form request - on that port and under that
    /// spelling only, and a list without it refuses the next connection.
    #[tokio::test]
    async fn an_origin_grant_reaches_exactly_that_origin_until_it_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let id = WorkspaceId("ws_routing".into());
        let up = ok_server().await;
        let other = ok_server().await;
        let proxy = ProxyRegistry::default();
        proxy
            .start(
                &id,
                &sock,
                grant(&format!("http://localhost:{up}/v1")),
                EventBus::new(100),
            )
            .await
            .unwrap();

        let reply = tunnel_get(&sock, &format!("localhost:{up}"), "/v1/models").await;
        assert!(
            reply.starts_with("HTTP/1.1 200 Connection Established"),
            "{reply}"
        );
        assert!(reply.ends_with("ok /v1/models"), "{reply}");
        let reply = ask(
            &sock,
            format!("GET http://localhost:{up}/v1/chat HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.ends_with("ok /v1/chat"), "{reply}");

        for authority in [format!("localhost:{other}"), format!("127.0.0.1:{up}")] {
            let reply = tunnel_get(&sock, &authority, "/").await;
            assert!(reply.starts_with("HTTP/1.1 403"), "{authority}: {reply}");
        }

        proxy.set_allowlist(&id, Allowlist::from_strings(&[]));
        let reply = tunnel_get(&sock, &format!("localhost:{up}"), "/v1/models").await;
        assert!(reply.starts_with("HTTP/1.1 403"), "revoked: {reply}");
        proxy.stop(&id);
    }

    async fn free_port() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    async fn http_get(port: u16, path: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), s.read_to_end(&mut out))
            .await
            .expect("the tunnel answers rather than hanging");
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The in-sandbox half: a plain TCP listener that carries each connection
    /// to one origin through the proxy socket. When the proxy refuses, an HTTP
    /// client is told why in a 502 instead of a bare reset, and a TLS client
    /// (whose bytes are not HTTP) gets the connection closed.
    #[tokio::test]
    async fn a_tunnel_carries_a_loopback_origin_through_the_proxy_and_explains_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let id = WorkspaceId("ws_tunnel".into());
        let up = ok_server().await;
        let proxy = ProxyRegistry::default();
        proxy
            .start(
                &id,
                &sock,
                grant(&format!("http://localhost:{up}/v1")),
                EventBus::new(100),
            )
            .await
            .unwrap();
        let port = free_port().await;
        let listen = vec![format!("127.0.0.1:{port}")];
        let target = format!("localhost:{up}");
        {
            let sock = sock.clone();
            tokio::spawn(async move { shim::tunnel(&sock, &listen, &target).await });
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "tunnel never listened"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let reply = http_get(port, "/v1/models").await;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.ends_with("ok /v1/models"), "{reply}");

        // A granted origin with nothing listening: a 502, which a model client
        // retries, naming the endpoint.
        let dead = free_port().await;
        proxy.set_allowlist(
            &id,
            Allowlist::from_strings(&[]).with_origins(vec![
                Origin::new("localhost", up).unwrap(),
                Origin::new("localhost", dead).unwrap(),
            ]),
        );
        let dead_listen = free_port().await;
        {
            let sock = sock.clone();
            let listen = vec![format!("127.0.0.1:{dead_listen}")];
            let target = format!("localhost:{dead}");
            tokio::spawn(async move { shim::tunnel(&sock, &listen, &target).await });
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", dead_listen))
            .await
            .is_err()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "tunnel never listened"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let reply = http_get(dead_listen, "/v1/models").await;
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");
        assert!(reply.contains(&format!("localhost:{dead}")), "{reply}");

        // Revoked: a 403, which is final, naming the endpoint and the refusal.
        proxy.set_allowlist(&id, Allowlist::from_strings(&[]));
        let reply = http_get(port, "/v1/models").await;
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains(&format!("localhost:{up}")), "{reply}");
        assert!(reply.contains("403 Forbidden\""), "{reply}");

        let mut tls = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tls.write_all(&[0x16, 0x03, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5])
            .await
            .unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tls.read_to_end(&mut out),
        )
        .await
        .expect("closed rather than hung");
        assert!(out.is_empty(), "{out:?}");
        proxy.stop(&id);
    }
}

/// A loopback grant is listened for at the same address and port inside the
/// sandbox; `localhost` on both loopbacks. Grants that are not loopback go
/// through the proxy as they are, an address already claimed is not claimed
/// again, and the proxy shim's own port is never taken.
#[test]
fn loopback_grants_are_listened_for_at_their_own_address() {
    use bondsymphonic_daemon::workspace::agent_sandboxes::tunnel_listens;
    let grants: Vec<Origin> = [
        "http://localhost:11434/v1",
        "http://127.0.0.1:11434/v1",
        "http://127.0.0.2:9000",
        "http://[::1]:8001/prefix",
        "http://127.0.0.1:3128",
        "http://192.168.1.20:8000/v1",
        "https://api.example.com/v1",
    ]
    .iter()
    .map(|u| Origin::from_url(u).unwrap())
    .collect();
    let got: Vec<(String, Vec<String>)> = tunnel_listens(&grants)
        .into_iter()
        .map(|(o, listen)| (o.authority(), listen))
        .collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        got,
        vec![
            (
                "localhost:11434".to_string(),
                s(&["127.0.0.1:11434", "[::1]:11434"])
            ),
            ("127.0.0.2:9000".to_string(), s(&["127.0.0.2:9000"])),
            ("[::1]:8001".to_string(), s(&["[::1]:8001"])),
        ]
    );
}

/// End to end under bubblewrap: a server on the WSL host's loopback, named by
/// `localhost` in the OpenCode config, answers a client inside the OpenCode
/// companion sandbox that dials `localhost` there, with the path intact. The
/// Codex companion of the same workspace cannot reach it, and once the config
/// no longer names it the OpenCode companion cannot either.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_opencode_sandbox_reaches_a_host_loopback_server_its_config_names() {
    use bondsymphonic_daemon::agents::{
        backend::Backend, codex_backend::CodexBackend, opencode_backend::OpenCodeBackend,
    };
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::{backend_for, SandboxCommand, SandboxHandle},
        server::broadcast::EventBus,
        workspace::{lifecycle, DataDirs},
    };
    use tokio::io::AsyncReadExt;
    let bwrap = std::process::Command::new("bwrap")
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
        .unwrap_or(false);
    if !bwrap {
        eprintln!("SKIP: bwrap unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        EventBus::new(100),
    )
    .unwrap();
    let up = unix::ok_server().await;
    let info = lifecycle::create(
        &daemon,
        bondsymphonic_proto::WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "routing".into(),
            init_if_missing: false,
            in_place: false,
        },
    )
    .await
    .unwrap();
    let ws = daemon.workspace(&info.id).unwrap();
    let configure = |url: &str| {
        mirror(
            &daemon.dirs.root,
            "opencode.json",
            &format!(r#"{{"provider": {{"local": {{"options": {{"baseURL": "{url}"}}}}}}}}"#),
        )
    };
    configure(&format!("http://localhost:{up}/v1"));

    async fn fetch(handle: &std::sync::Arc<dyn SandboxHandle>, url: &str) -> String {
        let script = format!(
            "import urllib.request\n\
             try:\n    print(urllib.request.urlopen('{url}', timeout=20).read().decode())\n\
             except Exception as e:\n    print('FAILED', e)\n"
        );
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["python3".into(), "-c".into(), script],
                env: vec![],
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
        let _ = child.exit.await;
        format!("{out}{err}")
    }

    let url = format!("http://localhost:{up}/v1/models");
    let opencode = daemon
        .agent_sandboxes
        .ensure(&daemon, &ws, &OpenCodeBackend as &dyn Backend)
        .await
        .unwrap();
    let out = fetch(&opencode, &url).await;
    assert_eq!(out.trim(), "ok /v1/models", "{out}");

    let codex = daemon
        .agent_sandboxes
        .ensure(&daemon, &ws, &CodexBackend as &dyn Backend)
        .await
        .unwrap();
    let out = fetch(&codex, &url).await;
    assert!(out.starts_with("FAILED"), "Codex reached it: {out}");

    // Another server in the config: the old origin is revoked and its tunnel
    // gone, the new one reachable, from the same live sandbox.
    let other = unix::ok_server().await;
    configure(&format!("http://127.0.0.1:{other}/prefix/v1"));
    let again = daemon
        .agent_sandboxes
        .ensure(&daemon, &ws, &OpenCodeBackend as &dyn Backend)
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&again, &opencode));
    let out = fetch(&opencode, &url).await;
    assert!(
        out.starts_with("FAILED"),
        "revoked origin still reached: {out}"
    );
    let out = fetch(
        &opencode,
        &format!("http://127.0.0.1:{other}/prefix/v1/models"),
    )
    .await;
    assert_eq!(out.trim(), "ok /prefix/v1/models", "{out}");

    daemon.agent_sandboxes.stop_workspace(&info.id).await;
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
}

/// Live, opt-in (`BS_OPENCODE_LIVE=1`, real OpenCode installed, bubblewrap):
/// `opencode run` inside the OpenCode companion sandbox, configured exactly as
/// a user would configure Ollama, gets its answer from an OpenAI-compatible
/// server on the host's loopback.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs the real OpenCode CLI and bubblewrap; set BS_OPENCODE_LIVE=1"]
async fn live_opencode_run_reaches_a_host_loopback_model_server() {
    use bondsymphonic_daemon::agents::{backend::Backend, opencode_backend::OpenCodeBackend};
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::{backend_for, SandboxCommand},
        server::broadcast::EventBus,
        workspace::{lifecycle, DataDirs},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if std::env::var_os("BS_OPENCODE_LIVE").is_none() {
        eprintln!("SKIP: BS_OPENCODE_LIVE is not set");
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let seen = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let line = head.lines().next().unwrap_or_default().to_string();
                    seen.lock().unwrap().push(line.clone());
                    let reply = if line.contains("/chat/completions") {
                        let chunk = |delta: &str, finish: &str| {
                            format!(
                                "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
                            )
                        };
                        let body = format!(
                            "{}{}data: [DONE]\n\n",
                            chunk(
                                r#"{"role":"assistant","content":"hello from the host"}"#,
                                "null"
                            ),
                            chunk("{}", "\"stop\"")
                        );
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    } else {
                        let body = r#"{"object":"list","data":[{"id":"m","object":"model"}]}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _ = s.write_all(reply.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // The user's own config, as the daemon would find it on Windows.
    let native = dir.path().join("native");
    std::fs::create_dir_all(native.join("opencode")).unwrap();
    std::fs::write(
        native.join("opencode/opencode.json"),
        format!(
            r#"{{"provider": {{"local": {{"npm": "@ai-sdk/openai-compatible", "name": "Local",
                "options": {{"baseURL": "http://localhost:{port}/v1"}},
                "models": {{"m": {{"name": "M"}}}}}}}}}}"#
        ),
    )
    .unwrap();
    std::env::set_var(
        bondsymphonic_daemon::agents::opencode_config::USER_CONFIG_ROOT_ENV,
        &native,
    );
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("linux_bwrap"),
        EventBus::new(100),
    )
    .unwrap();
    let info = lifecycle::create(
        &daemon,
        bondsymphonic_proto::WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "live".into(),
            init_if_missing: false,
            in_place: false,
        },
    )
    .await
    .unwrap();
    let ws = daemon.workspace(&info.id).unwrap();
    let prepared = OpenCodeBackend
        .prepare(
            &daemon,
            &ws,
            &serde_json::from_value(serde_json::json!({"permission_mode": "yolo"})).unwrap(),
        )
        .await
        .unwrap();
    let handle = daemon
        .agent_sandboxes
        .ensure(&daemon, &ws, &OpenCodeBackend as &dyn Backend)
        .await
        .unwrap();
    let mut argv = prepared.argv.clone();
    argv.pop(); // acp
    argv.extend(["run".into(), "-m".into(), "local/m".into(), "say hi".into()]);
    let mut child = handle
        .spawn(SandboxCommand {
            argv,
            env: prepared.env.clone(),
            cwd: Some(prepared.cwd.clone()),
            pty: None,
        })
        .await
        .unwrap();
    let collect = |mut pipe: bondsymphonic_daemon::sandbox::ChildReader| {
        let text = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = text.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        text
    };
    // `opencode run` appends piped stdin to the message, and waits for its end.
    drop(child.stdin.take());
    let out = collect(child.stdout.take().unwrap());
    let err = collect(child.stderr.take().unwrap());
    let code =
        match tokio::time::timeout(std::time::Duration::from_secs(120), &mut child.exit).await {
            Ok(code) => code.unwrap_or(-1),
            Err(_) => {
                (child.killer)();
                -999
            }
        };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let out = String::from_utf8_lossy(&out.lock().unwrap()).to_string();
    let err = String::from_utf8_lossy(&err.lock().unwrap()).to_string();
    let seen = seen.lock().unwrap().clone();
    eprintln!("exit {code}\nstdout:\n{out}\nstderr:\n{err}\nserver saw: {seen:?}");
    assert!(
        seen.iter()
            .any(|l| l.starts_with("POST /v1/chat/completions")),
        "{seen:?}"
    );
    assert!(out.contains("hello from the host"), "{out}\n{err}");
    daemon.agent_sandboxes.stop_workspace(&info.id).await;
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
}

/// A tunnel refusal is an OpenCode endpoint the config no longer names, so
/// "Allow host localhost" would be the wrong advice: no denial notice is
/// published for it. An ordinary refusal on the same proxy still is.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_tunnel_does_not_offer_to_allow_the_host() {
    use bondsymphonic_daemon::net::{allowlist::Allowlist, proxy::ProxyRegistry, shim};
    use bondsymphonic_daemon::server::broadcast::EventBus;
    use bondsymphonic_proto::{ServerMessage, WorkspaceId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let id = WorkspaceId("ws_quiet".into());
    let events = EventBus::new(100);
    let mut rx = events.subscribe();
    let proxy = ProxyRegistry::default();
    proxy
        .start(&id, &sock, Allowlist::from_strings(&[]), events)
        .await
        .unwrap();
    let port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    {
        let sock = sock.clone();
        let listen = vec![format!("127.0.0.1:{port}")];
        tokio::spawn(async move { shim::tunnel(&sock, &listen, "localhost:9").await });
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut s = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => break s,
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "tunnel never listened"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    };
    s.write_all(b"GET /v1/models HTTP/1.1\r\nHost: localhost:9\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let reply = String::from_utf8_lossy(&out);
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");

    // The control: a plain CONNECT for another host is still reported.
    let mut c = tokio::net::UnixStream::connect(&sock).await.unwrap();
    c.write_all(b"CONNECT other.example:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    let mut denied = Vec::new();
    while let Ok(ServerMessage::Event { event, .. }) = rx.try_recv() {
        denied.extend(event.denied_host().map(str::to_string));
    }
    assert_eq!(denied, vec!["other.example".to_string()]);
    proxy.stop(&id);
}
