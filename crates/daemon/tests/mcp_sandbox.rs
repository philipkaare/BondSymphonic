//! User-configured MCP servers as a sandbox sees them: one socket per remote
//! server in the sandbox's run directory, brokered to the upstream by the
//! daemon, and `mcp-exec` delivering a local server's secrets through its env
//! file. The upstream is a fake Streamable HTTP server on loopback (behind
//! `BS_MCP_ALLOW_HTTP=1`), reached through the production `curl` client.
#![cfg(unix)]
mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::mcp::registry::ListenerKind;
use bondsymphonic_daemon::mcp::servers::McpServers;
use bondsymphonic_daemon::mcp::{exec, server_socket_file};
use bondsymphonic_proto::*;
use common::start_daemon;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const SENTINEL: &str = "s3cr3t-sentinel";
const DAEMON: &str = env!("CARGO_BIN_EXE_bondsymphonic-daemon");

/// What the fake upstream saw: each request's JSON-RPC method (or `reply`
/// for a client's response) and the `Mcp-Session-Id` it carried.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A minimal Streamable HTTP MCP server: every `initialize` opens a new
/// session with its own id, `tools/list` offers `echo`, and `tools/call`
/// echoes its `text` argument.
async fn upstream() -> (String, Seen) {
    std::env::set_var("BS_MCP_ALLOW_HTTP", "1");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let seen: Seen = Default::default();
    let kept = seen.clone();
    let sessions = Arc::new(std::sync::atomic::AtomicU64::new(0));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (kept, sessions) = (kept.clone(), sessions.clone());
            tokio::spawn(async move {
                let mut bytes = vec![];
                let mut chunk = [0; 4096];
                let head_end = loop {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
                let length: usize = header(&head, "content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while bytes.len() < head_end + length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                    }
                }
                let message: Value =
                    serde_json::from_slice(&bytes[head_end..]).unwrap_or(Value::Null);
                let method = message["method"].as_str().unwrap_or("reply").to_owned();
                let session = header(&head, "mcp-session-id").map(str::to_owned);
                kept.lock().unwrap().push((method.clone(), session));
                let id = message.get("id").cloned();
                let (extra, body) = match (method.as_str(), id) {
                    (_, None) | ("reply", _) => (String::new(), None),
                    ("initialize", Some(id)) => {
                        let n = sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            format!("Mcp-Session-Id: session-{n}\r\n"),
                            Some(json!({"jsonrpc":"2.0","id":id,"result":{
                                "protocolVersion":"2025-06-18",
                                "capabilities":{"tools":{}},
                                "serverInfo":{"name":"fake","version":"1"}}})),
                        )
                    }
                    ("tools/list", Some(id)) => (
                        String::new(),
                        Some(json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                            {"name":"echo","inputSchema":{"type":"object"}}]}})),
                    ),
                    ("tools/call", Some(id)) => (
                        String::new(),
                        Some(json!({"jsonrpc":"2.0","id":id,"result":{
                            "content":[{"type":"text","text":message["params"]["arguments"]["text"]}],
                            "isError":false}})),
                    ),
                    (_, Some(id)) => (
                        String::new(),
                        Some(
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no"}}),
                        ),
                    ),
                };
                let response = match body {
                    None => {
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                    Some(body) => {
                        let body = body.to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (url, seen)
}

fn remote(id: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: format!("Remote {id}"),
        transport: McpTransportConfig::Http {
            url: url.into(),
            auth: McpAuthConfig::None,
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

fn upsert(servers: &McpServers, server: McpServerConfig, secrets: Vec<McpSecretWrite>) {
    servers
        .upsert(McpUpsertParams {
            server,
            secrets,
            expected_revision: None,
        })
        .unwrap();
}

/// Starts `id`'s listener for workspace `ws` in its own run directory under
/// `root`, the way an agent start does, and returns the socket path.
fn listen(d: &Arc<Daemon>, root: &Path, ws: &str, id: &str) -> PathBuf {
    let run = root.join(format!("run-{ws}"));
    std::fs::create_dir_all(&run).unwrap();
    let id: McpServerId = id.into();
    let socket = run.join(server_socket_file(&id));
    d.mcp
        .start(d.weak(), &ws.into(), &socket, ListenerKind::Server(id))
        .unwrap();
    socket
}

struct Conn {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

impl Conn {
    async fn open(socket: &Path) -> Self {
        let (r, w) = tokio::net::UnixStream::connect(socket)
            .await
            .unwrap()
            .into_split();
        Self {
            lines: BufReader::new(r).lines(),
            w,
        }
    }

    async fn send(&mut self, msg: Value) {
        self.w
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn rpc(&mut self, msg: Value) -> Value {
        self.send(msg).await;
        let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
            .await
            .expect("a reply")
            .unwrap()
            .expect("a line");
        serde_json::from_str(&line).unwrap()
    }

    async fn initialize(&mut self) -> Value {
        let init = self
            .rpc(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},
                "clientInfo":{"name":"test","version":"1"}}}),
            )
            .await;
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        init
    }
}

async fn gone(path: &Path) {
    for _ in 0..200 {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} was not removed", path.display());
}

#[tokio::test]
async fn remote_tool_call_through_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = upstream().await;
    let (_, _, d, cancel) = start_daemon(&dir.path().join("data")).await;
    upsert(&d.mcp_servers, remote("remote", &url), vec![]);
    let socket = listen(&d, dir.path(), "ws-a", "remote");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut c = Conn::open(&socket).await;
    let init = c.initialize().await;
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "fake", "{init}");
    let list = c
        .rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    // A string id comes back as the same string.
    let call = c
        .rpc(json!({"jsonrpc":"2.0","id":"call-3","method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"hi"}}}))
        .await;
    assert_eq!(call["id"], "call-3");
    assert_eq!(call["result"]["content"][0]["text"], "hi", "{call}");
    let methods: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    cancel.cancel();
}

#[tokio::test]
async fn disable_stops_live_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _) = upstream().await;
    let (_, _, d, cancel) = start_daemon(&dir.path().join("data")).await;
    upsert(&d.mcp_servers, remote("remote", &url), vec![]);
    let socket = listen(&d, dir.path(), "ws-a", "remote");
    let mut c = Conn::open(&socket).await;
    assert!(c.initialize().await.get("result").is_some());

    let mut off = remote("remote", &url);
    off.enabled = false;
    upsert(&d.mcp_servers, off, vec![]);
    gone(&socket).await;
    // Writing may already fail: the daemon closed its end.
    let _ =
        c.w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await;
    let eof = tokio::time::timeout(Duration::from_secs(10), c.lines.next_line())
        .await
        .expect("disabling must close the open connection");
    assert!(
        matches!(eof, Ok(None) | Err(_)),
        "expected end of stream, got {eof:?}"
    );
    cancel.cancel();
}

#[tokio::test]
async fn override_disabled_stops_only_that_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _) = upstream().await;
    let (_, _, d, cancel) = start_daemon(&dir.path().join("data")).await;
    upsert(&d.mcp_servers, remote("remote", &url), vec![]);
    let a = listen(&d, dir.path(), "ws-a", "remote");
    let b = listen(&d, dir.path(), "ws-b", "remote");

    d.mcp_servers
        .set_override(McpOverrideParams {
            server_id: "remote".into(),
            workspace_id: "ws-a".into(),
            value: McpWorkspaceOverride::Disabled,
        })
        .unwrap();
    gone(&a).await;
    assert!(b.exists(), "the other workspace keeps its socket");
    let mut c = Conn::open(&b).await;
    assert_eq!(c.initialize().await["result"]["serverInfo"]["name"], "fake");
    cancel.cancel();
}

#[tokio::test]
async fn unconnected_server_answers_32001() {
    let dir = tempfile::tempdir().unwrap();
    let (_, _, d, cancel) = start_daemon(&dir.path().join("data")).await;
    // The Jira preset is seeded on first start and has no login yet.
    let socket = listen(&d, dir.path(), "ws-a", "jira");
    let mut c = Conn::open(&socket).await;
    let expected = json!({
        "code": -32001,
        "message": "Jira (Atlassian) is not connected — connect it in Settings › MCP servers",
    });
    let init = c.initialize().await;
    assert_eq!(init["id"], 1);
    assert_eq!(init["error"], expected, "{init}");
    let list = c
        .rpc(json!({"jsonrpc":"2.0","id":"two","method":"tools/list"}))
        .await;
    assert_eq!(list["id"], "two");
    assert_eq!(list["error"], expected, "{list}");
    // A missing login is a failed tool, not a closed socket.
    assert!(socket.exists());
    cancel.cancel();
}

#[tokio::test]
async fn two_connections_have_independent_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = upstream().await;
    let (_, _, d, cancel) = start_daemon(&dir.path().join("data")).await;
    upsert(&d.mcp_servers, remote("remote", &url), vec![]);
    let socket = listen(&d, dir.path(), "ws-a", "remote");
    let mut one = Conn::open(&socket).await;
    let mut two = Conn::open(&socket).await;
    one.initialize().await;
    two.initialize().await;
    for c in [&mut one, &mut two] {
        let list = c
            .rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await;
        assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    }
    let sessions: std::collections::BTreeSet<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "tools/list")
        .map(|(_, s)| s.clone().expect("a session id"))
        .collect();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    cancel.cancel();
}

/// A registry with one local server whose `TOKEN` is the sentinel.
fn local_server(root: &Path) -> (McpServers, McpServerConfig) {
    let servers = McpServers::load(root).unwrap();
    let config = McpServerConfig {
        id: "local".into(),
        name: "Local".into(),
        transport: McpTransportConfig::Stdio {
            command: "/usr/bin/env".into(),
            args: vec![],
            env: BTreeMap::from([("PLAIN".to_string(), "visible value".to_string())]),
            secret_env: vec!["TOKEN".into()],
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    };
    upsert(
        &servers,
        config.clone(),
        vec![McpSecretWrite::Set {
            reference: "TOKEN".into(),
            value: McpSecretValue(SENTINEL.into()),
        }],
    );
    (servers, config)
}

#[test]
fn env_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (servers, config) = local_server(&dir.path().join("data"));
    let run = dir.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let path = exec::write_env_file(&servers, &config, &run).unwrap();
    assert_eq!(path, run.join("mcp-env").join("local.env"));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&run.join("mcp-env")), 0o700);
    assert!(std::fs::read_to_string(&path).unwrap().contains("TOKEN="));
}

#[test]
fn exec_sets_env_and_keeps_argv_clean() {
    let dir = tempfile::tempdir().unwrap();
    let (servers, config) = local_server(&dir.path().join("data"));
    let run = dir.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let env_file = exec::write_env_file(&servers, &config, &run).unwrap();
    let argv: Vec<String> = vec![
        "mcp-exec".into(),
        "--env-file".into(),
        env_file.to_string_lossy().into(),
        "--".into(),
        "/usr/bin/env".into(),
    ];
    // What reaches the process table is the file's path, never its contents.
    assert!(argv.iter().any(|a| Path::new(a) == env_file));
    assert!(!argv.iter().any(|a| a.contains(SENTINEL)));
    let out = std::process::Command::new(DAEMON)
        .args(&argv)
        .env_remove("TOKEN")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().any(|l| l == format!("TOKEN={SENTINEL}")),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l == "PLAIN=visible value"),
        "{stdout}"
    );
}
