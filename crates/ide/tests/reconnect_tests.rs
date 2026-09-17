//! What the IDE does when the daemon it is talking to goes away.
//!
//! Two halves. The first is the pure backoff schedule, which is arithmetic and
//! needs nothing running. The second is the whole loop end to end: the real
//! `bondsymphonic-ide` binary, offscreen, against an in-process fake daemon
//! that is told to drop the connection and then accepts a second one on the
//! same port. What that proves is what a user would see -- the status bar goes
//! through "reconnecting (attempt 1)" and back to "connected", the IDE
//! re-handshakes and re-syncs its workspace list without being asked, and the
//! panes are talking to the *new* connection afterwards, because the `tree`
//! step that follows the reconnect reaches the daemon at all.
//!
//! `system.test_drop` is a method of the *fake* daemon only. It is not a
//! `Request` variant, so the real daemon's decoder sees an unknown enum
//! variant and answers `invalid_params` with the connection left up, which is
//! why the drop can only ever happen in a test.

use bondsymphonic_ide::qobjects::app_controller::{backoff_delay, next_attempt, HELD_LONG_ENOUGH};
use bondsymphonic_proto::*;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[test]
fn backoff_doubles_up_to_thirty_seconds() {
    // 1, 2, 4, 8, 16, then the cap. The plan's schedule exactly (IDE §5).
    assert_eq!(backoff_delay(1), Duration::from_secs(1));
    assert_eq!(backoff_delay(2), Duration::from_secs(2));
    assert_eq!(backoff_delay(3), Duration::from_secs(4));
    assert_eq!(backoff_delay(4), Duration::from_secs(8));
    assert_eq!(backoff_delay(5), Duration::from_secs(16));
    assert_eq!(backoff_delay(6), Duration::from_secs(30));
    assert_eq!(backoff_delay(7), Duration::from_secs(30));
}

#[test]
fn backoff_never_overflows_or_returns_zero() {
    // Attempts are unbounded: a daemon that stays down all afternoon keeps the
    // counter climbing, and the exponent must not be shifted by it.
    assert_eq!(backoff_delay(64), Duration::from_secs(30));
    assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(30));
    // There is no attempt 0, but a caller that passes one must still wait
    // rather than spin.
    assert_eq!(backoff_delay(0), Duration::from_secs(1));
}

#[test]
fn a_connection_that_held_resets_the_schedule() {
    // An ordinary daemon restart: the connection had been up for a while, so
    // the next loss starts at one second again rather than at whatever the
    // counter had climbed to hours earlier.
    assert_eq!(next_attempt(6, Duration::from_secs(3600)), 1);
    assert_eq!(next_attempt(1, HELD_LONG_ENOUGH), 1);
    // The first loss of a run: `attempt` is still 0 and the answer is 1 either
    // way, because there is no earlier attempt to climb from.
    assert_eq!(next_attempt(0, Duration::from_secs(3600)), 1);
    assert_eq!(next_attempt(0, Duration::ZERO), 1);
}

#[test]
fn a_crash_looping_daemon_keeps_the_backoff_climbing() {
    // `hello` answered and the process gone a moment later is not a recovery.
    // Without this the bar would sit on "attempt 1" and the IDE would relaunch
    // the daemon every second for as long as the window is open.
    assert_eq!(next_attempt(1, Duration::from_millis(200)), 2);
    assert_eq!(next_attempt(2, Duration::from_secs(1)), 3);
    assert_eq!(
        next_attempt(5, HELD_LONG_ENOUGH - Duration::from_millis(1)),
        6
    );
    // Which is what turns the delay into the cap rather than leaving it at one
    // second forever.
    assert_eq!(
        backoff_delay(next_attempt(5, Duration::ZERO)),
        Duration::from_secs(30)
    );
    // Unbounded attempts must not wrap.
    assert_eq!(next_attempt(u32::MAX, Duration::ZERO), u32::MAX);
}

// ---------------------------------------------------------------------------
// The end-to-end half.
// ---------------------------------------------------------------------------

const TOKEN: &str = "reconnect-token";
/// A Claude tab with an agent attached, a terminal tab with a PTY open, then
/// the drop, then a directory listing on whatever connection the IDE has by
/// then. Two workspaces because the two panes prove different halves: the
/// transcript has to re-attach and replay, the terminal has to give up. The
/// `tree` at the end is what shows the re-sync produced a working client rather
/// than a hopeful status bar.
const SCRIPT: &str = "create_claude,open_agent,create,reconnect,tree,quit";
/// The agent the fake daemon hands out, and the one line of history it keeps,
/// so the replay after the reconnect has something to read back.
const AGENT_ID: &str = "ag_reconnect1";
/// The `reconnect` step waits for the IDE to come back (1 s of backoff plus a
/// re-sync), `create` and `quit` add their own settle times, and a cold Qt
/// start is the rest.
const RUN_LIMIT: Duration = Duration::from_secs(90);
/// What the status bar says while the IDE is trying again, and what it says
/// once it has. Both are logged by `AppController::refresh_status`, so a run
/// that never showed them to a user never logs them either.
const RECONNECTING_TEXT: &str = "daemon: reconnecting (attempt 1)";
const CONNECTED_TEXT: &str = "daemon: connected";
/// The script step that makes the fake daemon drop the connection, logged by
/// `smoke::run` before it runs. Everything after it in the log is the gap this
/// test is about, which is what lets the status lines be read in order rather
/// than merely counted.
const DROP_STEP: &str = "step: reconnect";
/// Failures that mean the reconnect happened but left something behind: a pane
/// still holding a subscription on the dead router, or a re-sync that could not
/// be read.
const NO_WARNINGS: [&str; 5] = [
    "workspace.list failed",
    "system.check_prereqs failed",
    "fs.list_dir failed",
    "agent.history failed",
    "workspace.changes failed",
];

type Journal = Arc<Mutex<Vec<String>>>;

#[test]
fn the_ide_reconnects_after_the_daemon_drops_the_connection() {
    if bondsymphonic_ide::testing::skip_without_qt("reconnect") {
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, journal) = rt.block_on(fake_daemon());

    // Never the developer's real `%APPDATA%\BondSymphonic`: this run would
    // otherwise rewrite their groups and their layout.
    let config = std::env::temp_dir().join(format!("bs-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("reconnect config dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
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
        .expect("the IDE binary starts");

    let out = drain(child.stdout.take().expect("stdout is piped"));
    let err = drain(child.stderr.take().expect("stderr is piped"));
    let status = wait_for(&mut child, RUN_LIMIT);
    let (out, err) = (
        out.recv().expect("the stdout drain thread is alive"),
        err.recv().expect("the stderr drain thread is alive"),
    );
    let seen = journal.lock().expect("journal mutex").clone();
    let logs = format!("{out}\n{err}");
    let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
    eprintln!("reconnect: the fake daemon answered {seen:?}");

    let status =
        status.unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
    assert!(
        status.success(),
        "the IDE exited with {status}, expected 0\n{context}"
    );
    assert!(
        !logs.contains("panicked at"),
        "the IDE logged a panic\n{context}"
    );

    // The handshake happened twice: once at start, once after the drop. This
    // is the claim the whole test exists for -- nothing in the script connects,
    // so a second `hello` can only be the controller's own reconnect loop.
    let hellos = seen.iter().filter(|m| *m == "hello").count();
    assert_eq!(hellos, 2, "expected exactly two handshakes\n{context}");
    assert!(
        seen.iter().any(|m| m == "system.test_drop"),
        "the reconnect step never asked the fake daemon to drop\n{context}"
    );

    // The re-sync: everything after the second `hello`. `workspace.list` is
    // what reconciles the tabs against the daemon that came back, and
    // `system.check_prereqs` is the other half of the connect-time pair.
    let last_hello = seen
        .iter()
        .rposition(|m| m == "hello")
        .expect("the two-handshake assertion above already ran");
    let after = &seen[last_hello..];
    for method in [
        // The controller's own re-sync.
        "workspace.list",
        "system.check_prereqs",
        // Each pane re-attaching on the new connection, unasked: the file tree
        // re-reads its root, the Changes tab re-subscribes and re-lists, the
        // Run panel re-detects, and the transcript replays its history. Only
        // `fs.list_dir` is also something the script does, and it does it after
        // all of these.
        "fs.list_dir",
        "fs.watch",
        "workspace.changes",
        "repo.detect_run_configs",
        "run.list",
        "agent.history",
    ] {
        assert!(
            after.iter().any(|m| m == method),
            "the IDE never sent {method} on the second connection\n{context}"
        );
    }
    // The terminal's PTY did not survive, and the pane must know it: the new
    // daemon has never heard of that id, so a `pty.close` or `pty.resize` for
    // it would come back `NotFound` and put an error banner over a pane whose
    // only news is that its shell is gone.
    for method in ["pty.close", "pty.resize", "pty.write"] {
        assert!(
            !after.iter().any(|m| m == method),
            "the IDE sent {method} for a PTY the restarted daemon never had\n{context}"
        );
    }

    // What the user would have read in the status bar, in order. The second
    // `find` starts where the first left off, so a "connected" printed only
    // before the drop does not satisfy it.
    let reconnecting = logs
        .find(RECONNECTING_TEXT)
        .unwrap_or_else(|| panic!("the status bar never said {RECONNECTING_TEXT:?}\n{context}"));
    assert!(
        logs[reconnecting..].contains(CONNECTED_TEXT),
        "the status bar never went back to connected after reconnecting\n{context}"
    );

    // IC5. The same three lines read as a sequence rather than a set, anchored
    // on the script step that caused the drop: after the connection ends, the
    // next thing the status bar says has to be that it is reconnecting. A bar
    // that went straight from "connected" to "connected" would mean the loss
    // was never published, and every operation attempted in between was
    // already failing with "daemon connection lost" behind a bar claiming a
    // connection. `refresh_status` logs only on a change, so the absence of a
    // reconnecting line is exactly that bug and not a missing print.
    let dropped = logs
        .find(DROP_STEP)
        .unwrap_or_else(|| panic!("the script never reached the drop step\n{context}"));
    let after_drop = &logs[dropped..];
    let said = |text: &str| after_drop.find(text);
    let next_status = [RECONNECTING_PREFIX, CONNECTED_TEXT]
        .into_iter()
        .filter_map(|text| said(text).map(|at| (at, text)))
        .min();
    assert_eq!(
        next_status.map(|(_, text)| text),
        Some(RECONNECTING_PREFIX),
        "after the drop the status bar must say it is reconnecting before it \
         says anything else\n{context}"
    );

    for warning in NO_WARNINGS {
        assert!(
            !logs.contains(warning),
            "the IDE logged {warning:?} even though the fake daemon answered\n{context}"
        );
    }

    let _ = std::fs::remove_dir_all(&config);
}

/// A daemon just real enough for a reconnect: it answers the handshake, the
/// connect-time calls and the script's, it remembers the workspaces it has
/// handed out across connections the way a restarted daemon reads them back off
/// disk, and it drops the connection when asked to with `system.test_drop`.
///
/// The listener stays bound for the whole run, so the second connection lands
/// on the same port the first did -- which is what makes this a daemon
/// restarting rather than the IDE finding a different one.
async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded = journal.clone();

    tokio::spawn(async move {
        // Outside the accept loop: a daemon that restarts still knows the
        // workspaces it created and the agents it recorded, and the IDE's
        // re-sync has to find both.
        let mut names: HashMap<String, String> = HashMap::new();
        let mut ptys = 0usize;
        // The one agent, and whether the daemon has "restarted" since it was
        // started. A restarted daemon reloads its agents as `exited` records
        // whose history still reads, which is what the transcript pane comes
        // back showing.
        let mut agent: Option<AgentId> = None;
        let mut restarted = false;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end();
                // The method is read before the body is typed: `system.test_drop`
                // is not a `Request` the proto crate knows, so decoding it as one
                // would fail rather than reaching the arm below.
                let method = serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|v| v.get("method")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                recorded.lock().expect("journal mutex").push(method.clone());
                if method == TEST_DROP {
                    // No reply at all: the socket simply goes, which is what a
                    // daemon that has died looks like from the IDE's side. The
                    // daemon that accepts the next connection is a restarted
                    // one, so its agents come back as records.
                    restarted = true;
                    break;
                }
                let ClientMessage::Request { id, request } =
                    codec::decode(trimmed).expect("decode");
                let mut follow_ups: Vec<ServerMessage> = Vec::new();
                let reply = match request {
                    Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                        id,
                        &HelloResult {
                            daemon_version: "0.0.0-fake".into(),
                            capabilities: Capabilities {
                                sandbox_backend: "noop".into(),
                                git_protect: false,
                                adapters: vec![
                                    AgentAdapterKind::Terminal,
                                    AgentAdapterKind::Claude,
                                ],
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
                    // Every workspace this daemon has ever made, so the
                    // reconcile after the reconnect keeps the tab instead of
                    // dropping it.
                    Request::WorkspaceList {} => {
                        let workspaces = names
                            .iter()
                            .map(|(id, name)| workspace(id, name))
                            .collect::<Vec<_>>();
                        ServerMessage::ok(id, &WorkspaceListResult { workspaces })
                    }
                    Request::WorkspaceCreate(p) => {
                        let ws_id = format!("ws_reconnect{}", names.len() + 1);
                        names.insert(ws_id.clone(), p.name.clone());
                        let info = workspace(&ws_id, &p.name);
                        follow_ups.push(ServerMessage::event(
                            Some(WorkspaceId(ws_id.clone())),
                            Event::WorkspaceStateChanged {
                                info: Box::new(info.clone()),
                            },
                        ));
                        ServerMessage::ok(id, &info)
                    }
                    Request::WorkspaceGet(p) => match names.get(&p.workspace_id.0) {
                        Some(name) => ServerMessage::ok(id, &workspace(&p.workspace_id.0, name)),
                        None => ServerMessage::err(id, RpcError::not_found(p.workspace_id.0)),
                    },
                    Request::AgentStart(p) => {
                        let agent_id = AgentId(AGENT_ID.to_owned());
                        agent = Some(agent_id.clone());
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::AgentStateChanged {
                                agent_id: agent_id.clone(),
                                state: AgentState::Idle,
                                detail: None,
                            },
                        ));
                        ServerMessage::ok(id, &AgentStartResult { agent_id })
                    }
                    // The transcript that survives the restart. Before the drop
                    // the agent is idle; after it the daemon has reloaded it
                    // from its records, so it reads back `exited` with the same
                    // messages -- which is what puts the pane on Restart with a
                    // session to resume.
                    Request::AgentHistory(_) if agent.is_some() => {
                        let (state, detail) = if restarted {
                            (AgentState::Exited, Some("daemon restarted".to_owned()))
                        } else {
                            (AgentState::Idle, None)
                        };
                        ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![AgentMessage {
                                    seq: 1,
                                    ts: "2026-09-10T10:00:01Z".to_owned(),
                                    body: AgentMessageBody::System {
                                        subtype: "init".to_owned(),
                                        data: serde_json::json!({
                                            "session_id": "sess-reconnect",
                                            "model": "claude-opus-5",
                                        }),
                                    },
                                }],
                                state,
                                detail,
                            },
                        )
                    }
                    Request::FsListDir(_) => ServerMessage::ok(
                        id,
                        &ListDirResult {
                            entries: vec![entry("src", true), entry("README.md", false)],
                        },
                    ),
                    Request::PtyOpen(p) => {
                        ptys += 1;
                        let pty_id = PtyId(format!("pty_reconnect{ptys}"));
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::PtyOutput {
                                pty_id: pty_id.clone(),
                                data_b64: String::new(),
                            },
                        ));
                        ServerMessage::ok(id, &PtyOpenResult { pty_id })
                    }
                    Request::WorkspaceChanges(_) => {
                        ServerMessage::ok(id, &ChangesResult { files: vec![] })
                    }
                    Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                        id,
                        &DetectRunConfigsResult {
                            configs: vec![],
                            network_allow: vec![],
                            warnings: vec![],
                        },
                    ),
                    Request::RunList(_) => ServerMessage::ok(id, &RunListResult { runs: vec![] }),
                    Request::FsWatch(_)
                    | Request::PtyResize(_)
                    | Request::PtyClose(_)
                    | Request::AgentStop(_) => ServerMessage::ok(id, &Empty {}),
                    other => ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    ),
                };
                let mut batch = codec::encode(&reply);
                for message in &follow_ups {
                    batch.push_str(&codec::encode(message));
                }
                if w.write_all(batch.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });

    (addr, journal)
}

/// The fake daemon's own control method. Not in `Request`: the real daemon
/// cannot decode it and answers `invalid_params` without dropping anything, so
/// a drop can only be asked for where a fake is listening.
const TEST_DROP: &str = "system.test_drop";

fn workspace(id: &str, name: &str) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/reconnect/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}/work"),
        worktree_path: format!("/wt/{id}"),
        created_at: "2026-09-10T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        kind: bondsymphonic_proto::WorkspaceKind::Worktree,
        state: WorkspaceState::Ready,
        agents: Vec::new(),
        agent_records: Vec::new(),
        runs: Vec::new(),
    }
}

fn entry(name: &str, is_dir: bool) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        is_dir,
        size: 0,
        status: FileStatus::Unchanged,
    }
}

/// Reads a child pipe to end on its own thread, so a full pipe cannot deadlock
/// the child before the time limit.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

fn wait_for(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

// ---------------------------------------------------------------------------
// The version gate. A daemon the IDE cannot talk to is not a daemon that is
// down: retrying it is retrying the same refusal every second for the rest of
// the session, so the loop stops and says which two pieces do not match.
// ---------------------------------------------------------------------------

/// What the status bar must say when the pair does not match, and the text
/// that must never appear beside it. The hint is part of it: the IDE stops
/// here rather than trying to repair anything, so the sentence has to say what
/// the user does next.
const MISMATCH_TEXT: &str =
    "daemon: protocol mismatch (daemon 3, IDE 2) \u{2014} rebuild the daemon \
     (scripts\\build-daemon.ps1) or reinstall the package";
const RECONNECTING_PREFIX: &str = "daemon: reconnecting (attempt";
/// The mismatch is decided by the handshake, so a cold Qt start is all this
/// waits for.
const MISMATCH_LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_protocol_mismatch_stops_the_connection_loop_instead_of_backing_off() {
    if bondsymphonic_ide::testing::skip_without_qt("protocol mismatch") {
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, journal) = rt.block_on(fake_daemon_speaking_the_next_protocol());

    let config = std::env::temp_dir().join(format!("bs-mismatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("mismatch config dir");

    // `BS_DAEMON_ADDR` is set, so no daemon of ours is launched -- and it would
    // make no difference if one were: the IDE installs the daemon it ships with
    // before every spawn, so a refusal is a pair that cannot talk and the first
    // one is the last word.
    let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("BS_DAEMON_ADDR", addr.to_string())
        .env("BS_DAEMON_TOKEN", TOKEN)
        .env("BS_SETTINGS_PATH", config.join("settings.json"))
        .env("BS_STATE_PATH", config.join("state.json"))
        .env("BS_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the IDE binary starts");

    let out = drain_live(child.stdout.take().expect("stdout is piped"));
    let err = drain_live(child.stderr.take().expect("stderr is piped"));
    let logs = |()| format!("{}\n{}", read(&out), read(&err));
    // No script can run without a connection, so this run never quits by
    // itself: its log is watched until the status text appears there -- the
    // supervisor logs the same sentence it puts in the status bar, which is the
    // only way to read the bar from outside an offscreen process -- then given
    // a moment to prove nothing follows, and stopped.
    let saw = wait_for_text(&mut child, &out, &err, MISMATCH_TEXT, MISMATCH_LIMIT);
    std::thread::sleep(Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    let logs = logs(());
    let seen = journal.lock().expect("journal mutex").clone();
    let context = format!("requests: {seen:?}\n--- logs ---\n{logs}");

    assert!(
        saw,
        "the status bar never said {MISMATCH_TEXT:?} within {MISMATCH_LIMIT:?}\n{context}"
    );
    assert!(
        !logs.contains("panicked at"),
        "the IDE logged a panic\n{context}"
    );
    // The whole point: no schedule was started, so the user is not watching a
    // counter climb against a daemon that will refuse every attempt alike.
    assert!(
        !logs.contains(RECONNECTING_PREFIX),
        "the IDE entered the backoff loop after a protocol mismatch\n{context}"
    );
    let hellos = seen.iter().filter(|m| *m == "hello").count();
    assert_eq!(
        hellos, 1,
        "the handshake was tried more than once against a daemon that cannot match\n{context}"
    );

    let _ = std::fs::remove_dir_all(&config);
}

/// A daemon that answers the handshake claiming the next protocol version.
/// Everything else it refuses: nothing should get that far.
async fn fake_daemon_speaking_the_next_protocol() -> (std::net::SocketAddr, Journal) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded = journal.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    recorded
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![],
                                },
                                protocol_version: Some(PROTOCOL_VERSION + 1),
                            },
                        ),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, journal)
}

/// Like [`drain`], but the text read so far can be looked at while the child is
/// still running, which is what a run that never exits by itself needs.
fn drain_live(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = text.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                return;
            }
            sink.lock()
                .expect("log mutex")
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    text
}

fn read(text: &Arc<Mutex<String>>) -> String {
    text.lock().expect("log mutex").clone()
}

/// Waits until `needle` appears in either pipe, the child exits, or the limit
/// passes. Returns whether it was seen.
fn wait_for_text(
    child: &mut std::process::Child,
    out: &Arc<Mutex<String>>,
    err: &Arc<Mutex<String>>,
    needle: &str,
    limit: Duration,
) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if read(out).contains(needle) || read(err).contains(needle) {
            return true;
        }
        if child.try_wait().expect("try_wait").is_some() {
            return read(out).contains(needle) || read(err).contains(needle);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Task 10: what the status bar says the moment a connection ends.
mod task_10 {
    use bondsymphonic_ide::model::app_state::ConnectionState;
    use bondsymphonic_ide::qobjects::app_controller::{
        next_attempt, on_connection_lost, require_connection, state_after_loss, CONNECTION_LOST,
    };
    use std::time::Duration;

    /// The gap between a connection ending and the next attempt starting is not
    /// instant: the old daemon is reaped first, and `shutdown` closes its stdin
    /// and waits for the process to go. For all of that window the shared
    /// client is already gone and every operation fails with "daemon connection
    /// lost" -- so a status bar still reading "connected" is the IDE claiming a
    /// connection it is refusing to use. The state moves in the same step that
    /// drops the client, before anything slow happens.
    #[test]
    fn a_lost_connection_reads_as_reconnecting_before_the_old_daemon_is_reaped() {
        // An ordinary loss: the loop is going to try again, and the number it
        // reports is the attempt it is about to make, so the bar does not jump
        // when the backoff starts counting it.
        let next = next_attempt(0, Duration::from_secs(3600));
        assert_eq!(
            state_after_loss(false, false, next),
            ConnectionState::Reconnecting { attempt: next }
        );
        assert_eq!(
            state_after_loss(false, false, 4),
            ConnectionState::Reconnecting { attempt: 4 }
        );
        // Whatever else it is, it is never still connected.
        for attempt in [1, 2, 30] {
            assert_ne!(
                state_after_loss(false, false, attempt),
                ConnectionState::Connected
            );
        }

        // Nothing will be tried again: an IDE on its way out says so plainly,
        // and a daemon whose data directory belongs to another daemon is an
        // error the user has to go and fix.
        assert_eq!(state_after_loss(true, false, 1), ConnectionState::Lost);
        assert_eq!(state_after_loss(true, true, 1), ConnectionState::Lost);
        assert_eq!(state_after_loss(false, true, 1), ConnectionState::Error);
    }

    /// The invariant the finding is actually worded as: the IDE never says it
    /// is connected while its own operations are refusing for want of a
    /// connection.
    ///
    /// Driven through the two process-wide functions `supervise` itself calls,
    /// in the order it calls them, rather than through a copy of them:
    /// `on_connection_lost` empties the shared slot and arms the "was lost"
    /// latch, and from that instant `require_connection` -- which every
    /// invokable goes through -- answers with the sentence the user gets. The
    /// state published in the same step must therefore not be `Connected`, for
    /// any of the three endings and any attempt number.
    #[test]
    fn nothing_the_loss_publishes_says_connected_while_operations_are_refused() {
        on_connection_lost();
        assert_eq!(
            require_connection().err(),
            Some(CONNECTION_LOST),
            "every operation must already be failing with a reason"
        );
        for quitting in [false, true] {
            for fatal in [false, true] {
                for attempt in [1, 2, 30] {
                    let state = state_after_loss(quitting, fatal, attempt);
                    assert_ne!(
                        state,
                        ConnectionState::Connected,
                        "quitting={quitting} fatal={fatal} attempt={attempt}"
                    );
                }
            }
        }
    }
}
