#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    // `output()` rather than `status()`: on Windows git narrates every LF/CRLF
    // rewrite on stderr, and inheriting that buries the suite's real output in
    // warnings. Kept and printed only when the command actually fails.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    // Identity and signing in the repository's own config, not in the ambient
    // environment: the daemon commits here itself (`merge --no-ff`, `rebase`,
    // `merge --squash` + `commit`) and does not inherit the `GIT_AUTHOR_*` the
    // helper above sets, so without this those commits fail on a host with no
    // global identity and hang on one that signs by default.
    git(&["config", "user.name", "t"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    repo
}

/// A repository with a *local* bare "origin" it tracks, for the push half of
/// `workspace.create_pr`.
///
/// Local so `git push -u origin` is a real push that a test can inspect
/// (`git --git-dir=<origin> show-ref`) without any of it leaving the machine.
/// Returns `(repo, origin)`.
pub fn init_repo_with_origin(dir: &Path) -> (PathBuf, PathBuf) {
    let repo = init_repo(dir);
    let origin = dir.join("origin.git");
    let st = Command::new("git")
        .args(["init", "--bare", "-q", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();
    assert!(st.success(), "git init --bare");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["remote", "add", "origin", &origin.to_string_lossy()]);
    git(&["push", "-q", "-u", "origin", "main"]);
    (repo, origin)
}

// ---------------------------------------------------------------------------
// Daemon test harness: a line-protocol client and a server wired to a
// `WorkspaceHandler` over the noop sandbox backend.
// ---------------------------------------------------------------------------

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::dispatch::SystemHandler;
use bondsymphonic_daemon::server::handlers::WorkspaceHandler;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::DataDirs;
use bondsymphonic_proto::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

pub struct Client {
    r: BufReader<tokio::net::tcp::OwnedReadHalf>,
    w: tokio::net::tcp::OwnedWriteHalf,
    next: u64,
    /// Events that arrived while `call` was waiting for a response. They are
    /// kept rather than dropped: the daemon delivers everything queued for a
    /// connection ahead of the reply, so a test that only used `call` would
    /// otherwise lose events published while its request was in flight.
    pending: Vec<(Option<WorkspaceId>, Event)>,
}

impl Client {
    pub async fn connect(port: u16, token: &str) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (r, w) = s.into_split();
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
            pending: Vec::new(),
        };
        let v = c
            .call(Request::Hello(HelloParams {
                token: token.into(),
                client_version: "t".into(),
            }))
            .await
            .unwrap();
        assert!(v["daemon_version"].is_string());
        c
    }

    pub async fn send(&mut self, req: Request) -> u64 {
        let id = self.next;
        self.next += 1;
        self.w
            .write_all(codec::encode(&ClientMessage::Request { id, request: req }).as_bytes())
            .await
            .unwrap();
        id
    }

    /// Reads until the response for `id` arrives; returns intermediate events.
    pub async fn recv_response(
        &mut self,
        id: u64,
        events: &mut Vec<(Option<WorkspaceId>, Event)>,
    ) -> Result<serde_json::Value, RpcError> {
        loop {
            let mut line = String::new();
            assert!(
                self.r.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            match codec::decode::<ServerMessage>(line.trim_end()).unwrap() {
                ServerMessage::Response {
                    id: rid,
                    result,
                    error,
                } if rid == id => {
                    return match error {
                        Some(e) => Err(e),
                        None => Ok(result.unwrap_or(serde_json::Value::Null)),
                    }
                }
                ServerMessage::Response { .. } => {}
                ServerMessage::Event {
                    workspace_id,
                    event,
                } => events.push((workspace_id, event)),
            }
        }
    }

    pub async fn call(&mut self, req: Request) -> Result<serde_json::Value, RpcError> {
        let id = self.send(req).await;
        let mut ev = Vec::new();
        let out = self.recv_response(id, &mut ev).await;
        self.pending.extend(ev);
        out
    }

    /// Takes the events `call` has buffered since the last drain.
    pub fn drain_events(&mut self) -> Vec<(Option<WorkspaceId>, Event)> {
        std::mem::take(&mut self.pending)
    }
}

/// Binds a server on an ephemeral port with a workspace handler over `root`.
pub async fn start_daemon(root: &std::path::Path) -> (u16, String, Arc<Daemon>, CancellationToken) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(DataDirs::new(root), backend_for("noop"), server.event_bus()).unwrap();
    let system = SystemHandler {
        token: server.token().to_string(),
        capabilities: ServerConfig::default().capabilities,
    };
    let handler = Arc::new(WorkspaceHandler {
        system,
        daemon: daemon.clone(),
    });
    let (port, token) = (server.port(), server.token().to_string());
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move { server.with_handler(handler).run(c2).await.unwrap() });
    (port, token, daemon, cancel)
}

/// Creates a workspace on `main` and returns its info.
pub async fn create_ws(c: &mut Client, repo: &std::path::Path, name: &str) -> WorkspaceInfo {
    serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: "main".into(),
            name: name.into(),
        }))
        .await
        .unwrap(),
    )
    .unwrap()
}

/// Commits whatever is in a working tree. With `env` set to
/// `Layout::sandbox_git_env()` this commits inside a workspace worktree the way
/// the sandbox does — new objects land in the workspace's private object
/// directory, so the commit is only reachable from the main store through that
/// directory. With an empty `env` it is a plain commit in an ordinary repo.
pub fn commit_all(worktree: &Path, env: &[(String, String)], message: &str) {
    for args in [
        ["add", "-A"].as_slice(),
        ["commit", "-q", "-m", message].as_slice(),
    ] {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(worktree)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
