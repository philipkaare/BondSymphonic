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

/// Runs git in `dir` and returns trimmed stdout, asserting it succeeded.
///
/// `output()` rather than `status()` for the reason [`init_repo`] gives: on
/// Windows git narrates every LF/CRLF rewrite on stderr, and that is worth
/// seeing only when the command actually failed.
pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// [`git_out`] for a command whose output nothing reads.
pub fn git_ok(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

/// Asserts that a refusal names the repository a folder sits inside.
///
/// Either spelling counts. The daemon prints what it canonicalised, and on
/// Windows that differs from the path a test typed in more than one way at
/// once: `git rev-parse --show-toplevel` answers with forward slashes, and a
/// `tempfile` directory under `%TEMP%` may be reached through a short name or a
/// symlink.
pub fn assert_names_enclosing_repo(message: &str, repo: &Path) {
    let canonical = bondsymphonic_daemon::git::repo::canonical_ish(repo)
        .display()
        .to_string();
    assert!(
        message.contains(&repo.display().to_string()) || message.contains(&canonical),
        "the refusal has to name the enclosing repository {}: {message}",
        repo.display()
    );
}

/// A repository with a *local* bare "origin" it tracks, for the push half of
/// `workspace.create_pr`.
///
/// Local so `git push -u origin` is a real push that a test can inspect
/// (`git --git-dir=<origin> show-ref`) without any of it leaving the machine.
/// Returns `(repo, origin)`.
/// A path as a `/bin/sh` script can carry it: git for Windows runs hooks through
/// its bundled shell, which takes `C:/...` but not backslashes.
pub fn sh_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Writes an executable `<name>` hook into `repo`'s `.git/hooks`, with `body` as
/// its script. A `#!/bin/sh` line is prepended; `body` supplies the rest,
/// including its own `exit`.
pub fn install_hook(repo: &Path, name: &str, body: &str) {
    let hooks = repo.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

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
use bondsymphonic_daemon::sandbox::{backend_for, SandboxBackend};
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
                protocol_version: Some(PROTOCOL_VERSION),
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
    start_daemon_with_backend(root, backend_for("noop")).await
}

/// The same, over a backend of the caller's choosing.
///
/// Every suite here wants the no-sandbox backend and calls `start_daemon`. This
/// is for a test that wants to wrap it: a decorator that holds one `spawn` open
/// turns a race the daemon runs in a couple of milliseconds into one the test
/// can step through, which is the difference between proving something and
/// guessing at a sleep.
pub async fn start_daemon_with_backend(
    root: &std::path::Path,
    backend: Arc<dyn SandboxBackend>,
) -> (u16, String, Arc<Daemon>, CancellationToken) {
    let server = Server::bind(ServerConfig::default()).await.unwrap();
    let daemon = Daemon::new(DataDirs::new(root), backend, server.event_bus()).unwrap();
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
            init_if_missing: false,
            in_place: false,
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

// ---------------------------------------------------------------------------
// Port guard: ending a test's own web app, and never anybody else's.
// ---------------------------------------------------------------------------

/// Kills whatever *this test's* run left listening on a port, once the test is
/// over, however it ended.
///
/// A `#[tokio::test]` whose body panics drops its runtime while the run's web
/// server is still alive, and on Windows tokio waits for a child process on a
/// blocking thread that the drop then waits for in turn — so one failed
/// assertion hangs the whole test binary instead of reporting. The guard turns
/// that back into an ordinary failure and leaves nothing behind for the next
/// test to trip over.
///
/// It kills **by process id**, and only ids it can argue belong to the run under
/// test. Killing "whatever is listening on the port" at teardown is a different
/// and much worse thing: a test port is an ephemeral number the operating system
/// hands out, and once the run has exited that number can belong to anything on
/// the developer's machine. Four rules keep this honest:
///
/// * Whatever already held the port when the guard was made is somebody else's
///   and is never killed.
/// * The port is watched rather than sampled once, because a run with a
///   `ready_regex` is `ready` before it binds, and because a run can rebind.
/// * Watching **stops** at [`PortGuard::disarm`], which each test calls as soon
///   as it has seen the run's terminal event. After that point the run is gone
///   and any new owner of the port is a stranger, so the window in which one
///   could be recorded is closed rather than left open to the end of the test.
/// * A recorded id is checked against the process table before it is killed:
///   only the images a run is started through ([`KILLABLE_IMAGES`]) are killed,
///   so even a pid that was reused between the recording and the drop cannot
///   take an unrelated program with it.
///
/// Windows only. On Unix the no-sandbox backend puts every child in a process
/// group of its own, the daemon's own teardown reaches all of it, and a runtime
/// drop does not block on a surviving child — so the guard is inert and costs a
/// struct.
pub struct PortGuard {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

/// The executable names a run of this suite is served by, lowercased.
///
/// The daemon starts a run through `cmd /C <command>` on Windows, and every
/// command these tests configure is a Python interpreter. A pid whose image is
/// not one of these is not this suite's, whatever the port said: pids are
/// reused, and a wrong guess here kills a program the user was running.
const KILLABLE_IMAGES: [&str; 4] = ["python.exe", "python3.exe", "py.exe", "cmd.exe"];

impl PortGuard {
    pub fn new(port: u16) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        if !cfg!(windows) {
            return Self {
                stop,
                seen,
                watcher: None,
            };
        }
        // Taken before the run is started, so every one of these is a process
        // that was here first.
        let theirs = listeners_on(port);
        let (s, v) = (stop.clone(), seen.clone());
        let watcher = std::thread::spawn(move || {
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                for pid in listeners_on(port) {
                    if theirs.contains(&pid) {
                        continue;
                    }
                    let mut g = v.lock().unwrap();
                    if !g.contains(&pid) {
                        g.push(pid);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
        Self {
            stop,
            seen,
            watcher: Some(watcher),
        }
    }

    /// Stops watching the port, keeping whatever has been recorded so far.
    ///
    /// Called by a test the moment the run reaches a terminal state: from then
    /// on the port belongs to whoever the operating system next hands it to, and
    /// that is never this test's business. Idempotent, and the drop calls it
    /// again for a test that ended before reaching this point.
    pub fn disarm(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.watcher.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PortGuard {
    fn drop(&mut self) {
        self.disarm();
        for pid in self.seen.lock().unwrap().iter() {
            let Some(image) = image_of(pid) else {
                // Already gone, which is the ordinary case: the test stopped its
                // own run and this guard has nothing left to do.
                continue;
            };
            if !KILLABLE_IMAGES.contains(&image.as_str()) {
                eprintln!("PortGuard: leaving pid {pid} ({image}) alone; not a run of this suite");
                continue;
            }
            // `/T` for the tree: `cmd /C python …` is a child of the shell the
            // daemon started it through.
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// The executable name behind a pid, lowercased, or `None` when the process is
/// gone or the query failed. Answering `None` is what makes a failure here a
/// reason *not* to kill.
fn image_of(pid: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let out = std::process::Command::new("tasklist")
        .args(["/NH", "/FO", "CSV", "/FI", &format!("PID eq {pid}")])
        .output()
        .ok()?;
    // A filter that matches nothing prints an informational line on stdout
    // rather than failing, and it is not CSV, so the quote is what tells the two
    // apart: `"python.exe","1234",…`.
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.starts_with('"'))?;
    Some(
        line.trim_start_matches('"')
            .split('"')
            .next()?
            .to_lowercase(),
    )
}

/// The process ids listening on `port` right now, as strings, because that is
/// what `taskkill` takes and nothing here does arithmetic on them.
fn listeners_on(port: u16) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
    else {
        return Vec::new();
    };
    let needle = format!(":{port}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // proto, local address, remote address, state, pid
            if f.len() < 5 || f[3] != "LISTENING" || !f[1].ends_with(&needle) {
                return None;
            }
            Some(f[4].to_string())
        })
        .collect()
}
