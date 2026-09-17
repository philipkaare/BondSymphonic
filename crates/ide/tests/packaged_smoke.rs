//! Smoke test for the *packaged* IDE, the folder `scripts\package.ps1` builds.
//!
//! `tests/smoke.rs` runs the binary cargo just built, which finds its Qt DLLs
//! on the PATH a developer shell sets up. That proves nothing about a package
//! handed to somebody else: what `windeployqt` copied into `dist\BondSymphonic`
//! is a different, smaller set of files, and the first thing a user would see
//! if one were missing is a silent failure to start. So this suite runs the
//! deployed exe itself, from its own folder: `--version`, which proves the
//! statically imported libraries load (the five Qt DLLs and the Visual C++
//! runtime) without building a window, and then a whole connect-and-quit cycle,
//! which proves the plugins do too.
//!
//! It is skipped, loudly, unless `BS_PACKAGED_EXE` points at a built
//! `dist\BondSymphonic\bondsymphonic-ide.exe`:
//!
//! ```powershell
//! . .\scripts\env.ps1
//! .\scripts\package.ps1
//! $env:BS_PACKAGED_EXE = "$pwd\dist\BondSymphonic\bondsymphonic-ide.exe"
//! cargo test -p bondsymphonic-ide --test packaged_smoke
//! ```
//!
//! The daemon it talks to is the minimal fake at the bottom of this file, not
//! the real one: the claim under test is that the deployed Qt runtime loads and
//! the window comes up, and a fake keeps the run off the user's WSL distro and
//! their workspaces entirely. `tests/smoke.rs` is left alone; the fake here
//! answers only the three requests a `quit`-only run makes.

use bondsymphonic_proto::*;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Points at the deployed exe. Unset in an ordinary `cargo test` run.
const PACKAGED_EXE_ENV: &str = "BS_PACKAGED_EXE";
const TOKEN: &str = "packaged-token";
/// Connect, let the window settle, exit 0. Deliberately the shortest script
/// there is: this suite is about the deployed runtime, not about features, and
/// a longer script would be a second copy of `tests/smoke.rs` to maintain.
const SCRIPT: &str = "quit";
/// The whole run: process start, Qt initialisation off a cold DLL load, the
/// handshake, and the smoke script's own two-second settle before it quits.
const RUN_LIMIT: Duration = Duration::from_secs(90);
/// One workspace the fake daemon reports, so the list the IDE reconciles
/// against is a real one rather than empty.
const WORKSPACE_ID: &str = "ws_packaged1";
const WORKSPACE_NAME: &str = "packaged-smoke";

/// The exe under test, or `None` when this suite does not apply.
fn packaged_exe() -> Option<PathBuf> {
    let raw = std::env::var_os(PACKAGED_EXE_ENV)?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

#[test]
fn the_packaged_exe_reports_its_version_and_runs_offscreen() {
    let Some(exe) = packaged_exe() else {
        let reason = format!(
            "{PACKAGED_EXE_ENV} unset, so there is no packaged build to test. Run \
             scripts\\package.ps1 and set it to dist\\BondSymphonic\\bondsymphonic-ide.exe."
        );
        // The same bargain `bondsymphonic_ide::testing::skip_without_qt` makes
        // for the Qt-less suites, on this suite's own variable: a developer
        // gets a loud skip, and CI's Windows job -- which turns `require-qt` on
        // -- gets a failure. Without this, a job that forgot to build the
        // package would report `1 passed` and nobody would learn the package
        // went untested, which is precisely the regression this suite exists
        // to catch.
        if cfg!(feature = "require-qt") {
            panic!("{reason}");
        }
        // stdout, and prefixed `SKIP:`, so it reads the same way as the other
        // suites' skips. Cargo captures it unless the run asks for --nocapture.
        println!("SKIP: {reason}");
        println!("skipped: 1");
        return;
    };
    assert!(
        exe.is_file(),
        "{PACKAGED_EXE_ENV}={} does not name a file",
        exe.display()
    );

    version_loads_the_qt_libraries(&exe);
    connects_and_quits_against_a_fake_daemon(&exe);
}

/// `--version` returns before `QApplication` is constructed, so no plugin is
/// loaded and no window is built. It is not a test that runs "without Qt": the
/// five Qt DLLs are static imports of the exe, so the loader resolves them
/// before `main` — which is exactly why this is a useful first question to ask
/// of a package. It proves those DLLs and the Visual C++ runtime beside them
/// are present and loadable. The plugin folders are proven by the run below.
fn version_loads_the_qt_libraries(exe: &PathBuf) {
    let out = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("the packaged exe starts");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    assert!(
        out.status.success(),
        "--version exited with {}, expected 0\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    let expected = format!(
        "bondsymphonic-ide {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        PROTOCOL_VERSION
    );
    assert_eq!(
        stdout, expected,
        "--version printed the wrong line\nstderr: {stderr}"
    );
    eprintln!("packaged_smoke: --version said {stdout:?}");
}

/// The real claim: the deployed DLLs load, the window builds, the daemon
/// handshake completes and the process ends by itself with status 0.
fn connects_and_quits_against_a_fake_daemon(exe: &PathBuf) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, seen) = rt.block_on(fake_daemon());

    // The IDE writes its settings and its layout on exit. Both are pointed at a
    // directory of this run's own: a test must never touch the user's real
    // `%APPDATA%\BondSymphonic`.
    let config = std::env::temp_dir().join(format!("bs-packaged-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("packaged smoke config dir");

    let mut child = Command::new(exe)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("BS_DAEMON_ADDR", addr.to_string())
        .env("BS_DAEMON_TOKEN", TOKEN)
        .env("BS_SMOKE_SCRIPT", SCRIPT)
        .env("BS_SETTINGS_PATH", config.join("settings.json"))
        .env("BS_STATE_PATH", config.join("state.json"))
        .env("BS_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the packaged exe starts");

    // Drained on their own threads: a full pipe would deadlock the child long
    // before the time limit and turn a fast failure into a slow one.
    let out = drain(child.stdout.take().expect("stdout is piped"));
    let err = drain(child.stderr.take().expect("stderr is piped"));
    let status = wait_for(&mut child, RUN_LIMIT);
    let out = out.recv().expect("the stdout drain thread is alive");
    let err = err.recv().expect("the stderr drain thread is alive");
    let methods = seen.lock().expect("journal mutex").clone();
    let logs = format!("{out}\n{err}");
    let context = format!("requests: {methods:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");

    let status = status
        .unwrap_or_else(|| panic!("the packaged IDE did not exit within {RUN_LIMIT:?}\n{context}"));
    assert!(
        status.success(),
        "the packaged IDE exited with {status}, expected 0\n{context}"
    );
    assert!(
        !logs.contains("panicked at"),
        "the packaged IDE logged a panic\n{context}"
    );
    // The handshake and the first re-sync. `hello` alone would pass on a
    // process that connected and then died before building anything.
    for method in ["hello", "workspace.list"] {
        assert!(
            methods.iter().any(|m| m == method),
            "the fake daemon never saw {method}\n{context}"
        );
    }
    eprintln!("packaged_smoke: the fake daemon answered {methods:?}, the exe left with {status}");
    let _ = std::fs::remove_dir_all(&config);
}

/// Reads a pipe to EOF on its own thread and hands the whole text back once.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Waits up to `limit` for the child, killing it if it overruns so the suite
/// never leaves a window process behind.
fn wait_for(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// The three requests a `quit`-only run makes, and nothing else. Anything the
/// IDE asks for beyond them comes back as an error it reports and survives, so
/// a fourth request added elsewhere in the IDE cannot silently break this
/// suite — but it will show up in the journal the failure prints.
async fn fake_daemon() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let journal = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let journal = journal.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Ok(ClientMessage::Request { id, request }) = codec::decode(line.trim_end())
                    else {
                        continue;
                    };
                    journal
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: env!("CARGO_PKG_VERSION").into(),
                                capabilities: Capabilities {
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "git".into(),
                                    ok: true,
                                    detail: "git version 2.43".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => ServerMessage::ok(
                            id,
                            &WorkspaceListResult {
                                workspaces: vec![workspace()],
                            },
                        ),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!(
                                "packaged_smoke does not answer {}",
                                other.method_name()
                            )),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (addr, seen)
}

/// The one workspace the fake reports. Its fields are shaped like a real
/// daemon's answer; nothing in a `quit` run reads more than the id and name.
fn workspace() -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(WORKSPACE_ID.to_owned()),
        name: WORKSPACE_NAME.to_owned(),
        repo_path: "/packaged/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{WORKSPACE_NAME}/work"),
        worktree_path: format!("/packaged/worktrees/{WORKSPACE_ID}"),
        created_at: "2026-09-10T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        kind: bondsymphonic_proto::WorkspaceKind::Worktree,
        state: WorkspaceState::Ready,
        agents: Vec::new(),
        agent_records: Vec::new(),
        runs: Vec::new(),
    }
}
