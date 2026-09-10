//! Offscreen end-to-end smoke test.
//!
//! Runs the real `bondsymphonic-ide` binary with `QT_QPA_PLATFORM=offscreen`
//! against an in-process fake daemon, and drives it with the two test hooks the
//! IDE carries for exactly this purpose: `BS_DAEMON_ADDR`/`BS_DAEMON_TOKEN`
//! (connect here instead of starting a daemon in WSL) and `BS_SMOKE_SCRIPT`
//! (perform these steps once connected). Both are inert when unset.
//!
//! What it proves: the window builds, the daemon connection comes up, the
//! workspace/PTY/file-tree requests reach the daemon in the right order, a
//! Claude agent's tab attaches a transcript of its own accord and carries one
//! whole turn — prompt, permission request, allow, tool call, result — with the
//! answer routed back through the window to the pane that was showing the
//! request, an editor tab and a diff tab open through the same controller
//! signals the Explorer emits and fetch their contents (`fs.read_file`,
//! `workspace.diff`) and their live-update watches (`fs.watch`,
//! `workspace.changes`), panes whose process has exited are torn down without
//! talking to the daemon about the PTYs it has already reaped, a run
//! configuration is detected and a run started and stopped on a bridged host
//! port, the network denial the daemon reports behind it becomes a toast on the
//! workspace that raised it and answering that toast sends the workspace's own
//! allowlist back with the blocked host added, the Qt event loop is still
//! responsive at the end (the `quit` step runs on it), and the process ends with
//! status 0 well inside the time limit.

use base64::Engine as _;
use bondsymphonic_proto::*;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

const TOKEN: &str = "smoke-token";
/// The Claude half comes first, on its own workspace, and the terminal half
/// second on another: a Claude tab has a transcript pane and no PTY, so the
/// `close`/`destroy` pair only means something over a workspace whose pane is a
/// terminal.
const SCRIPT: &str = "create_claude,open_agent,send,allow,tree,open_file,open_diff,stop,create,\
                      detect,run_start,allow_host,run_stop,open,close,destroy,quit";
/// The file `open_file` and `open_diff` act on, and the one entry of the fake
/// `fs.list_dir` listing that is not a directory.
const OPEN_PATH: &str = "README.md";
/// The agent the fake daemon hands out, the tool it asks about and the id of
/// the request the `allow` step answers. The script agrees the request id with
/// the daemon rather than reading it off the transcript, and the window refuses
/// to route an answer to a pane that is not showing exactly that request.
const AGENT_ID: &str = "ag_smoke1";
const REQUEST_ID: &str = "req-1";
const TOOL_NAME: &str = "Bash";
/// What the turn cost, so the assertion on the result frame has a number.
const TURN_COST_USD: f64 = 0.002;
/// The one run configuration this daemon reports, and the run it starts from
/// it. The port is the one the run listens on inside the sandbox.
const RUN_CONFIG: &str = "web";
const RUN_ID: &str = "run_smoke1";
const RUN_PORT: u16 = 3000;
/// The bridged port the daemon hands back. Deliberately not [`RUN_PORT`]: the
/// URL the panel shows has to come from the daemon's reply, never be rebuilt
/// from the configuration's own port.
const HOST_PORT: u16 = 41873;
const RUN_URL: &str = "http://localhost:41873";
/// The one line the run prints before it reports itself ready.
const RUN_OUTPUT_LINE: &str = "ready on 3000";
/// The host the fake proxy refuses just after the run comes up. Not in
/// [`DEFAULT_ALLOW`], which is what makes answering the toast a change.
const DENIED_HOST: &str = "example.com";
/// The allowlist a freshly created workspace carries, in the daemon spec's
/// order (§7.1). Spelled out here rather than imported from the daemon crate:
/// it is a fixture of what a daemon reports, and the assertion below is that
/// the IDE sent this list *back* with one host added rather than replacing it
/// with a stale or empty copy.
const DEFAULT_ALLOW: [&str; 12] = [
    "api.anthropic.com",
    "*.anthropic.com",
    "registry.npmjs.org",
    "*.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
];
/// The `quit` step alone waits 2 s, each of `open_agent`, `send`, `allow`,
/// `run_start` and `allow_host` another 1.5 s, and `create`, `create_claude`,
/// `open_file`, `open_diff`, `stop`, `detect` and `close` another 0.75 s each —
/// about 16 s of deliberate waiting; the rest is a Qt startup on a cold cache.
const RUN_LIMIT: Duration = Duration::from_secs(75);
/// The methods the script must produce, in this order. `agent.history` is the
/// window's own doing — only `TranscriptModel::attach` sends it, and the model
/// only attaches because the window reacted to `agentStarted` — so its place
/// between `agent.start` and `agent.send` shows the pane was built and wired up
/// before the turn began. `fs.read_file` is the editor tab loading its file and
/// `workspace.diff` the diff tab loading its alignment, so their place in the
/// sequence is what shows the two tabs opened in the order the script asked for
/// them.
/// `repo.detect_run_configs` and `run.list` between `agent.stop` and
/// `run.start` are the Run panel's own, issued when the second workspace's tab
/// appears and the panel is pointed at its worktree; `workspace.get` and
/// `workspace.set_allowlist` are `RunPanelModel::allowHost` answering the
/// toast, in that order, because it reads the daemon's list before it sends one
/// back.
const EXPECTED: [&str; 19] = [
    "hello",
    "workspace.create",
    "agent.start",
    "agent.history",
    "agent.send",
    "agent.permission_reply",
    "fs.list_dir",
    "fs.read_file",
    "workspace.diff",
    "agent.stop",
    "repo.detect_run_configs",
    "run.list",
    "run.start",
    "workspace.get",
    "workspace.set_allowlist",
    "run.stop",
    "pty.open",
    "pty.close",
    "workspace.destroy",
];
/// Requests the window must have made after `workspace.create` on its own
/// account: the Changes tab lists the new workspace and turns on the file
/// watch. Neither is in the script, so seeing them proves the window wired the
/// workspace up rather than the script standing in for it.
const AFTER_CREATE: [&str; 2] = ["fs.watch", "workspace.changes"];
/// What `fs.read_file` returns and the base side of the diff.
const BASE_TEXT: &str = "hello\n";
/// The work side of the diff: one line added to [`BASE_TEXT`].
const WORK_TEXT: &str = "hello\nworld\n";
/// The warnings `EditorDocument`, `DiffDocument` and `ChangesModel` log when
/// one of their requests fails, matched as their exact prefixes. A run in which
/// the fake daemon answered but the reply was rejected — a body the IDE could
/// not deserialise, say — still reaches the journal, so the journal alone
/// cannot tell a served request from a served-and-refused one. These can.
/// `fs.write_file` is not among them: no step saves, so asserting on its
/// warning would assert nothing. The fake daemon answers it anyway, so a future
/// step that does save needs no change on the daemon side.
const NO_WARNINGS: [&str; 16] = [
    "fs.read_file failed",
    "workspace.diff failed",
    "workspace.changes failed",
    "fs.watch enable",
    // The transcript's own four. Each is logged by the `TranscriptModel` call
    // that issued the request, and the script drives all four of those calls
    // through the window, so every one of these is reachable: it means the IDE
    // refused a reply it was given -- a body it could not deserialise, say --
    // which the journal alone cannot show.
    "agent.history failed",
    "agent.send failed",
    "agent.permission_reply failed",
    "agent.stop failed",
    // The window's three, when a request arrives for an agent no visible
    // transcript is attached to. These are what make `send`, `allow` and `stop`
    // assertions rather than wishes: each reaches the daemon only through
    // `TranscriptModel`, and the window calls that only after it has found the
    // pane attached to the named agent -- with the request on its bar, for the
    // permission reply.
    "permission reply not routed",
    "agent send not routed",
    "agent stop not routed",
    // The Run panel's own, and the controller's. Each is logged by the call
    // that issued the request: the panel detects and lists when the tab
    // appears, the `detect` step goes through `AppController::detectRunConfigs`,
    // and `allowHost` makes both of the last two. `run.start failed` and
    // `run.stop failed` are deliberately *not* here: the script makes those two
    // on its own client, where a failure ends the step and stops the script
    // without quitting, which the exit-status assertion catches instead.
    "repo.detect_run_configs failed",
    "run.list failed",
    "workspace.get failed",
    "workspace.set_allowlist failed",
    // The window refusing to answer a denial for a workspace the Run panel is
    // not showing. This is what makes `allow_host` an assertion rather than a
    // wish: `workspace.set_allowlist` reaches the daemon only after the window
    // has matched the request against the panel's own workspace.
    "allow host not routed",
];

/// A recorder the fake daemon appends to: every request method it answered in
/// arrival order, every permission reply it received, or every allowlist it was
/// handed.
type Journal = Arc<Mutex<Vec<String>>>;

#[test]
fn the_ide_drives_a_workspace_pty_and_file_tree_then_exits_cleanly() {
    if std::env::var_os("QMAKE").is_none() {
        eprintln!(
            "smoke: skipped because QMAKE is unset, so the Qt runtime the IDE needs is not on \
             PATH. Dot-source scripts\\env.ps1 and run again."
        );
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, journal, replies, allowlists) = rt.block_on(fake_daemon());

    let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("BS_DAEMON_ADDR", addr.to_string())
        .env("BS_DAEMON_TOKEN", TOKEN)
        .env("BS_SMOKE_SCRIPT", SCRIPT)
        .env("BS_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the IDE binary starts");

    // Drained on their own threads: a full pipe would deadlock the child long
    // before the time limit and turn a fast failure into a slow one.
    let out = drain(child.stdout.take().expect("stdout is piped"));
    let err = drain(child.stderr.take().expect("stderr is piped"));

    let status = wait_for(&mut child, RUN_LIMIT);
    // `expect`, not a default: an empty string here would vacate the panic
    // assertion and all four `NO_WARNINGS` assertions, turning a dead drain
    // thread into a green run.
    let (out, err) = (
        out.recv().expect("the stdout drain thread is alive"),
        err.recv().expect("the stderr drain thread is alive"),
    );
    let seen = journal.lock().expect("journal mutex").clone();
    let answered = replies.lock().expect("replies mutex").clone();
    let allowed = allowlists.lock().expect("allowlists mutex").clone();
    let context = format!(
        "requests: {seen:?}\npermission replies: {answered:?}\nallowlists: {allowed:?}\n\
         --- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    // Both pipes together. `tracing_subscriber::fmt()` writes to *stdout* by
    // default and `main` does not override the writer, so every warning the IDE
    // logs arrives on stdout; only a panic message comes out on stderr. An
    // assertion that read stderr alone would pass whatever was logged, which is
    // what the three terminal-warning assertions below used to do.
    let logs = format!("{out}\n{err}");
    eprintln!(
        "smoke: the fake daemon answered {seen:?}, permission replies {answered:?}, allowlists \
         {allowed:?}"
    );

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
    assert!(
        contains_in_order(&seen, &EXPECTED),
        "the fake daemon did not see {EXPECTED:?} in order\n{context}"
    );
    // The script issues one of each; the window issues its own for the tab the
    // script created — the Explorer dock lists the new workspace's root and the
    // agent pane opens and sizes its terminal. Without these the run would still
    // be green while covering none of the C++ widgets, which is exactly what
    // happened when the script raced the connect-time `workspace.list` and the
    // reconcile dropped the tab out from under them.
    for method in ["fs.list_dir", "pty.open"] {
        let count = seen.iter().filter(|m| *m == method).count();
        assert!(
            count >= 2,
            "expected the window to issue its own {method} as well as the script's, saw \
             {count}\n{context}"
        );
    }
    // The Changes tab and the editor's watch are the window's own doing, and
    // both only make sense once there is a workspace: `set_workspace` on the
    // changes model issues `workspace.changes` and turns `fs.watch` on, and
    // `EditorDocument::open` turns it on again for the file it just read. A
    // `fs.watch` before the create would be for a workspace that does not
    // exist yet, so the position in the journal is part of the claim.
    let after_create: Vec<&String> = seen
        .iter()
        .skip_while(|m| *m != "workspace.create")
        .collect();
    for method in AFTER_CREATE {
        assert!(
            after_create.iter().any(|m| *m == method),
            "the window never sent {method} after workspace.create\n{context}"
        );
    }
    // What the `allow` step actually did. The journal shows an answer was sent;
    // this shows it named the request the transcript was showing and said yes.
    assert_eq!(
        answered,
        vec![format!("{REQUEST_ID}:allow")],
        "the permission reply the fake daemon received was not a single allow for \
         {REQUEST_ID}\n{context}"
    );
    // What the `allow_host` step actually did. The journal shows a
    // `workspace.set_allowlist` was sent; this shows what was in it. The IDE
    // reads the daemon's own list at the moment of the click and appends to it,
    // so the twelve defaults have to come back untouched and in order with the
    // blocked host after them. A list that replaced them instead would silently
    // un-allow every registry an agent needs, and would still have satisfied the
    // journal.
    let expected_allowlist: Vec<String> = DEFAULT_ALLOW
        .iter()
        .chain(std::iter::once(&DENIED_HOST))
        .map(|h| (*h).to_owned())
        .collect();
    assert_eq!(
        allowed,
        vec![expected_allowlist.join(",")],
        "the allowlist the fake daemon received was not the defaults plus {DENIED_HOST}\n{context}"
    );
    // A login terminal on the host is the one thing this run must never open,
    // and nothing in the script asks for one: every prerequisite the fake
    // daemon reports passes, so the setup page never appears.
    assert!(
        !seen.iter().any(|m| m == "system.setup_pty"),
        "the IDE asked for a setup terminal\n{context}"
    );
    // The transcript detaches when its workspace goes, and stops nothing: the
    // daemon reaps a destroyed workspace's agents itself. An `agent.*` after
    // the destroy would be the IDE talking about an agent that is already gone,
    // which the daemon answers `NotFound`.
    // The Claude workspace is never destroyed in this script -- the destroy is
    // for the terminal workspace created later -- so this also covers a live
    // transcript sitting through another tab's teardown.
    let after_destroy: Vec<&String> = seen
        .iter()
        .skip_while(|m| *m != "workspace.destroy")
        .collect();
    assert!(
        !after_destroy.iter().any(|m| m.starts_with("agent.")),
        "the IDE sent an agent request after workspace.destroy\n{context}"
    );
    // The `close` step ended every PTY the fake daemon had open, so by the time
    // `destroy` tears the panes down their processes have exited. A pane in that
    // state must not send `pty.close` or `pty.resize`: the daemon has reaped
    // those PTYs and answers `NotFound`, which used to put an error banner over
    // a pane whose only news was that its process had finished.
    for method in ["pty.close", "pty.resize", "pty.write"] {
        assert!(
            !after_destroy.iter().any(|m| *m == method),
            "the window sent {method} for a PTY that had already exited\n{context}"
        );
    }
    // The same thing from the session's side: those failures are what set the
    // `error` property the terminal paints its banner from.
    for warning in ["pty.close failed", "pty.resize failed", "pty.write failed"] {
        assert!(
            !logs.contains(warning),
            "a terminal recorded {warning:?} after its process exited\n{context}"
        );
    }
    // The fake daemon answered every editor, diff and changes request, so none
    // of their documents may have logged a failure. This catches the case the
    // journal cannot: a request that arrived and was answered with something
    // the IDE then refused.
    for warning in NO_WARNINGS {
        assert!(
            !logs.contains(warning),
            "the IDE logged {warning:?} even though the fake daemon answered\n{context}"
        );
    }
}

/// Whether `wanted` appears in `seen` in order, other requests in between
/// allowed. The window issues its own `pty.open` and `fs.list_dir` for the tab
/// the script creates, so the script's requests are a subsequence of the whole,
/// not the whole.
fn contains_in_order(seen: &[String], wanted: &[&str]) -> bool {
    let mut rest = wanted.iter();
    let mut next = rest.next();
    for method in seen {
        if next == Some(&method.as_str()) {
            next = rest.next();
        }
    }
    next.is_none()
}

/// Reads a child pipe to end on its own thread.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Waits up to `limit` for `child`, killing it and returning `None` if it is
/// still running. Polling beats a wait thread here: the child has to be killed
/// on timeout, which needs the handle back.
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

fn workspace(id: &str, name: &str, state: WorkspaceState, allowlist: &[String]) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/smoke/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}/work"),
        worktree_path: format!("/wt/{id}"),
        created_at: "2026-09-09T10:00:00Z".to_owned(),
        allowlist: allowlist.to_vec(),
        state,
        agents: Vec::new(),
        runs: Vec::new(),
    }
}

/// The timestamp every transcript message carries. The IDE displays it and
/// never orders by it -- `seq` does that -- so one value for the run is enough.
const TS: &str = "2026-09-09T10:00:00Z";

/// Records `body` in the fake's transcript, so `agent.history` can read it
/// back, and returns the `agent.message` event a real daemon broadcasts for it.
/// `seq` is the position in that transcript, which is what makes it monotonic.
fn emit(
    transcript: &mut Vec<AgentMessage>,
    workspace_id: &WorkspaceId,
    agent_id: &AgentId,
    body: AgentMessageBody,
) -> ServerMessage {
    let message = AgentMessage {
        seq: transcript.len() as u64 + 1,
        ts: TS.to_owned(),
        body,
    };
    transcript.push(message.clone());
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::AgentMessage {
            agent_id: agent_id.clone(),
            message,
        },
    )
}

fn agent_state(
    workspace_id: &WorkspaceId,
    agent_id: &AgentId,
    state: AgentState,
    detail: Option<&str>,
) -> ServerMessage {
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::AgentStateChanged {
            agent_id: agent_id.clone(),
            state,
            detail: detail.map(str::to_owned),
        },
    )
}

/// A `run.state` event for `run_id`. The url travels on the `ready` transition
/// and nowhere else, which is how a real daemon reports it.
fn run_state(
    workspace_id: &WorkspaceId,
    run_id: &RunId,
    state: RunState,
    url: Option<String>,
) -> ServerMessage {
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::RunStateChanged {
            run_id: run_id.clone(),
            state,
            url,
            detail: None,
        },
    )
}

/// The single run configuration this daemon detects, whatever path is asked
/// about. Its port is flagged as a guess, so the Run panel renders the label and
/// the tooltip it keeps for that case.
fn run_config() -> RunConfig {
    RunConfig {
        name: RUN_CONFIG.to_owned(),
        command: "python3 -m http.server 3000".to_owned(),
        port: RUN_PORT,
        cwd: None,
        env: BTreeMap::new(),
        ready_regex: None,
        source: RunConfigSource::Detected,
        port_guessed: true,
        disabled_reason: None,
    }
}

fn entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        is_dir,
        size,
        status: FileStatus::Unchanged,
    }
}

/// A daemon just real enough for one IDE session: it answers the handshake, the
/// connect-time calls, and the ones the script makes, and it emits the events a
/// real daemon would (Creating then Ready for the new workspace, one line of
/// output for each PTY, an exit for each PTY it ends). Everything else is an
/// explicit error, so an unexpected request shows up in the journal rather than
/// hanging the IDE.
async fn fake_daemon() -> (std::net::SocketAddr, Journal, Journal, Journal) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded = journal.clone();
    // Every `agent.permission_reply` as `<request id>:<decision>`. The method
    // journal shows that an answer was sent; this shows which request it
    // answered and what it said, which is the whole point of the `allow` step.
    let replies: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded_replies = replies.clone();
    // Every `workspace.set_allowlist` as its comma-joined host list. The method
    // journal shows one was sent; this shows what the IDE put in it, which is
    // the whole point of the `allow_host` step.
    let allowlists: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded_allowlists = allowlists.clone();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        // One workspace id per created name, so a repeated `create` is answered
        // consistently and `pty.open` can be checked against a known workspace.
        let mut workspaces: HashMap<String, String> = HashMap::new();
        // The other direction, plus each workspace's allowlist as it stands, so
        // `workspace.get` answers with what the last `workspace.set_allowlist`
        // left behind rather than with the fixture.
        let mut names: HashMap<String, String> = HashMap::new();
        let mut allowed: HashMap<String, Vec<String>> = HashMap::new();
        // The runs this daemon has handed out and the workspace they belong to,
        // so `run.list` and the `run.state` events have somewhere to come from.
        let mut runs: Vec<RunInfo> = Vec::new();
        let mut run_workspace: Option<WorkspaceId> = None;
        let mut ptys = 0usize;
        // Every PTY handed out and the workspace it belongs to, so `pty.close`
        // can end all of them at once.
        let mut open_ptys: Vec<(WorkspaceId, PtyId)> = Vec::new();
        // The id of a `pty.close` whose reply is being held; see the arm below.
        let mut held_close: Option<u64> = None;
        // The one agent this daemon hands out, the workspace it belongs to, and
        // everything it has broadcast, which `agent.history` reads back.
        let mut agent: Option<(WorkspaceId, AgentId)> = None;
        let mut transcript: Vec<AgentMessage> = Vec::new();
        loop {
            line.clear();
            // The `quit` step ends the process, which resets this socket rather
            // than closing it politely, so a read error ends the session too.
            match r.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let ClientMessage::Request { id, request } =
                codec::decode(line.trim_end()).expect("decode");
            recorded
                .lock()
                .expect("journal mutex")
                .push(request.method_name().to_owned());

            let mut follow_ups: Vec<ServerMessage> = Vec::new();
            let reply: Option<ServerMessage> = match request {
                Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                    id,
                    &HelloResult {
                        daemon_version: "0.0.0-fake".into(),
                        capabilities: Capabilities {
                            sandbox_backend: "noop".into(),
                            git_protect: false,
                            // What a daemon with a `claude` on PATH advertises.
                            // The New Agent dialog opens on Claude only when it
                            // sees this, so the list is part of the fixture.
                            adapters: vec![AgentAdapterKind::Terminal, AgentAdapterKind::Claude],
                        },
                    },
                )),
                Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                Request::SystemCheckPrereqs {} => Some(ServerMessage::ok(
                    id,
                    &CheckPrereqsResult {
                        items: vec![PrereqStatus {
                            name: "git".into(),
                            ok: true,
                            detail: "git version 2.43".into(),
                            fix_hint: None,
                        }],
                    },
                )),
                Request::WorkspaceList {} => Some(ServerMessage::ok(
                    id,
                    &WorkspaceListResult { workspaces: vec![] },
                )),
                Request::WorkspaceCreate(p) => {
                    let ws_id = format!("ws_smoke{}", workspaces.len() + 1);
                    workspaces.insert(p.name.clone(), ws_id.clone());
                    names.insert(ws_id.clone(), p.name.clone());
                    // A real daemon gives a new workspace the default list,
                    // extended by the repository's `bondsymphonic.toml`. There
                    // is no toml here, so it is the twelve defaults exactly.
                    let hosts: Vec<String> =
                        DEFAULT_ALLOW.iter().map(|h| (*h).to_owned()).collect();
                    allowed.insert(ws_id.clone(), hosts.clone());
                    let creating = workspace(&ws_id, &p.name, WorkspaceState::Creating, &hosts);
                    let ready = workspace(&ws_id, &p.name, WorkspaceState::Ready, &hosts);
                    // A real daemon answers while still creating and reports the
                    // rest through events; the tab has to survive both.
                    for info in [creating.clone(), ready] {
                        follow_ups.push(ServerMessage::event(
                            Some(WorkspaceId(ws_id.clone())),
                            Event::WorkspaceStateChanged { info },
                        ));
                    }
                    Some(ServerMessage::ok(id, &creating))
                }
                Request::PtyOpen(p) => {
                    ptys += 1;
                    let pty_id = PtyId(format!("pty_smoke{ptys}"));
                    open_ptys.push((p.workspace_id.clone(), pty_id.clone()));
                    follow_ups.push(ServerMessage::event(
                        Some(p.workspace_id.clone()),
                        Event::PtyOutput {
                            pty_id: pty_id.clone(),
                            data_b64: BASE64.encode("prompt$ "),
                        },
                    ));
                    Some(ServerMessage::ok(id, &PtyOpenResult { pty_id }))
                }
                // A real daemon would exit only the PTY named here. This one
                // ends every PTY it has handed out, because the script closes
                // its own and has no way to name the ones the window opened for
                // its panes — and those are the ones the steps after this have
                // to find already exited.
                //
                // The reply is held until such a pane exists: the window opens
                // its terminal only once Qt has laid it out, which is later
                // than the script's first steps, so answering straight away
                // would end the script's PTY and leave the window's untouched.
                Request::PtyClose(_) => {
                    held_close = Some(id);
                    None
                }
                Request::WorkspaceDestroy(_) => Some(ServerMessage::ok(id, &Empty {})),
                // One agent, started once. A real daemon reports the process
                // coming up as a state change rather than in the reply, so the
                // IDE has to survive an `agent.state` that arrives before its
                // transcript has subscribed -- which is exactly what the
                // router's early buffer is for.
                Request::AgentStart(p) => {
                    let agent_id = AgentId(AGENT_ID.to_owned());
                    agent = Some((p.workspace_id.clone(), agent_id.clone()));
                    follow_ups.push(agent_state(
                        &p.workspace_id,
                        &agent_id,
                        AgentState::Working,
                        None,
                    ));
                    Some(ServerMessage::ok(id, &AgentStartResult { agent_id }))
                }
                // A file read on a real daemon. Here it is whatever has been
                // broadcast so far, which is what a reopened tab would replay.
                Request::AgentHistory(_) => Some(ServerMessage::ok(
                    id,
                    &HistoryResult {
                        messages: transcript.clone(),
                        state: AgentState::Idle,
                        detail: None,
                    },
                )),
                // The prompt, then the turn stalling on a tool the user has to
                // allow: the same shape as the `permission_turn.ndjson` fixture
                // the daemon's parser tests run on.
                Request::AgentSend(p) => match agent.clone() {
                    Some((ws, ag)) => {
                        follow_ups.push(emit(
                            &mut transcript,
                            &ws,
                            &ag,
                            AgentMessageBody::UserText { text: p.text },
                        ));
                        follow_ups.push(emit(
                            &mut transcript,
                            &ws,
                            &ag,
                            AgentMessageBody::System {
                                subtype: "init".to_owned(),
                                data: serde_json::json!({
                                    "session_id": "sess-3",
                                    "model": "claude-opus-5",
                                    "tools": [TOOL_NAME],
                                }),
                            },
                        ));
                        follow_ups.push(emit(
                            &mut transcript,
                            &ws,
                            &ag,
                            AgentMessageBody::PermissionRequest {
                                request_id: REQUEST_ID.to_owned(),
                                tool_name: TOOL_NAME.to_owned(),
                                input: serde_json::json!({ "command": "rm -rf build" }),
                                suggestions: Vec::new(),
                            },
                        ));
                        follow_ups.push(agent_state(
                            &ws,
                            &ag,
                            AgentState::WaitingPermission,
                            Some(TOOL_NAME),
                        ));
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    None => Some(ServerMessage::err(
                        id,
                        RpcError::internal("agent.send before agent.start"),
                    )),
                },
                // The answer, and the rest of the turn it unblocks.
                Request::AgentPermissionReply(p) => {
                    let decision = match p.decision {
                        PermissionDecision::Allow => "allow",
                        PermissionDecision::Deny => "deny",
                    };
                    recorded_replies
                        .lock()
                        .expect("replies mutex")
                        .push(format!("{}:{decision}", p.request_id));
                    match agent.clone() {
                        Some((ws, ag)) => {
                            follow_ups.push(agent_state(&ws, &ag, AgentState::Working, None));
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::ToolUse {
                                    id: "toolu_2".to_owned(),
                                    name: TOOL_NAME.to_owned(),
                                    input: serde_json::json!({ "command": "rm -rf build" }),
                                },
                            ));
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::ToolResult {
                                    id: "toolu_2".to_owned(),
                                    output: "removed 'build'".to_owned(),
                                    is_error: false,
                                },
                            ));
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::Result {
                                    cost_usd: TURN_COST_USD,
                                    duration_ms: 500,
                                    num_turns: 1,
                                    session_id: "sess-3".to_owned(),
                                },
                            ));
                            follow_ups.push(agent_state(&ws, &ag, AgentState::Idle, None));
                            Some(ServerMessage::ok(id, &Empty {}))
                        }
                        None => Some(ServerMessage::err(
                            id,
                            RpcError::internal("agent.permission_reply before agent.start"),
                        )),
                    }
                }
                Request::AgentInterrupt(_) => Some(ServerMessage::ok(id, &Empty {})),
                Request::AgentStop(_) => {
                    if let Some((ws, ag)) = agent.clone() {
                        follow_ups.push(agent_state(
                            &ws,
                            &ag,
                            AgentState::Exited,
                            Some("exit code 0"),
                        ));
                    }
                    Some(ServerMessage::ok(id, &Empty {}))
                }
                // Answered, and answered with a failure: a login terminal on
                // the host is the one thing this run must never open, and an
                // error here would show up in the journal rather than starting
                // one. The assertions below require it never to be asked for.
                Request::SystemSetupPty(_) => Some(ServerMessage::err(
                    id,
                    RpcError::internal("the smoke run never logs in"),
                )),
                // The terminal widget restates its size once the PTY exists.
                Request::PtyResize(_) | Request::PtyWrite(_) => {
                    Some(ServerMessage::ok(id, &Empty {}))
                }
                Request::FsListDir(_) => Some(ServerMessage::ok(
                    id,
                    &ListDirResult {
                        entries: vec![entry("src", true, 0), entry(OPEN_PATH, false, 42)],
                    },
                )),
                // The editor tab's load. Small, valid UTF-8 and not truncated,
                // so the document opens editable rather than as a notice.
                Request::FsReadFile(_) => Some(ServerMessage::ok(
                    id,
                    &ReadFileResult {
                        content: BASE_TEXT.to_owned(),
                        encoding: "utf-8".to_owned(),
                        truncated: false,
                    },
                )),
                // Ctrl+S and the watch the editor and the Changes tab both ask
                // for. Nothing here has to do anything: the assertions are that
                // the requests were made and that neither was reported failed.
                Request::FsWriteFile(_) | Request::FsWatch(_) => {
                    Some(ServerMessage::ok(id, &Empty {}))
                }
                // One modified file, so the Changes tab has a row to build and
                // the counts have somewhere to land.
                Request::WorkspaceChanges(_) => Some(ServerMessage::ok(
                    id,
                    &ChangesResult {
                        files: vec![ChangedFile {
                            path: OPEN_PATH.to_owned(),
                            status: FileStatus::Modified,
                            additions: 1,
                            deletions: 0,
                        }],
                    },
                )),
                // One added line, which aligns to one equal row and one insert
                // row: enough for `DiffWidget` to build both panes, tint a row
                // and size its gutter from two different line-number columns.
                Request::WorkspaceDiff(_) => Some(ServerMessage::ok(
                    id,
                    &DiffResult {
                        base_text: BASE_TEXT.to_owned(),
                        work_text: WORK_TEXT.to_owned(),
                        truncated: false,
                    },
                )),
                // What `RunPanelModel::allowHost` reads before it writes.
                Request::WorkspaceGet(p) => {
                    let ws = p.workspace_id.0.clone();
                    match names.get(&ws) {
                        Some(name) => {
                            let hosts = allowed.get(&ws).cloned().unwrap_or_default();
                            let info = workspace(&ws, name, WorkspaceState::Ready, &hosts);
                            Some(ServerMessage::ok(id, &info))
                        }
                        None => Some(ServerMessage::err(id, RpcError::not_found(ws))),
                    }
                }
                // The other half of the click. A real daemon persists the list
                // and reports the new one as a `workspace.state` event, which is
                // what refreshes every client; the assertion is on what arrived
                // here.
                Request::WorkspaceSetAllowlist(p) => {
                    let ws = p.workspace_id.0.clone();
                    recorded_allowlists
                        .lock()
                        .expect("allowlists mutex")
                        .push(p.hosts.join(","));
                    allowed.insert(ws.clone(), p.hosts.clone());
                    if let Some(name) = names.get(&ws) {
                        let info = workspace(&ws, name, WorkspaceState::Ready, &p.hosts);
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::WorkspaceStateChanged { info },
                        ));
                    }
                    Some(ServerMessage::ok(id, &Empty {}))
                }
                // One configuration, whatever path is asked about: the New Agent
                // dialog asks about the repository and the Run panel about the
                // worktree, and both have to get a list they can render.
                Request::RepoDetectRunConfigs(_) => Some(ServerMessage::ok(
                    id,
                    &DetectRunConfigsResult {
                        configs: vec![run_config()],
                        network_allow: vec![],
                    },
                )),
                // The run, on a bridged port, then the three events a real
                // daemon reports it with -- and behind them the proxy refusing a
                // host the run reached for, built with the same proto helper the
                // daemon builds it with, so the IDE recognises it the same way.
                Request::RunStart(p) => {
                    let run_id = RunId(RUN_ID.to_owned());
                    run_workspace = Some(p.workspace_id.clone());
                    runs.push(RunInfo {
                        run_id: run_id.clone(),
                        config_name: p.config_name.clone(),
                        state: RunState::Ready,
                        host_port: HOST_PORT,
                        url: RUN_URL.to_owned(),
                    });
                    follow_ups.push(run_state(
                        &p.workspace_id,
                        &run_id,
                        RunState::Starting,
                        None,
                    ));
                    follow_ups.push(ServerMessage::event(
                        Some(p.workspace_id.clone()),
                        Event::RunOutput {
                            run_id: run_id.clone(),
                            line: RUN_OUTPUT_LINE.to_owned(),
                        },
                    ));
                    follow_ups.push(run_state(
                        &p.workspace_id,
                        &run_id,
                        RunState::Ready,
                        Some(RUN_URL.to_owned()),
                    ));
                    follow_ups.push(ServerMessage::event(
                        Some(p.workspace_id.clone()),
                        Event::network_denied(DENIED_HOST),
                    ));
                    Some(ServerMessage::ok(
                        id,
                        &RunStartResult {
                            run_id,
                            host_port: HOST_PORT,
                            url: RUN_URL.to_owned(),
                        },
                    ))
                }
                Request::RunStop(p) => {
                    runs.retain(|r| r.run_id != p.run_id);
                    if let Some(ws) = run_workspace.clone() {
                        follow_ups.push(run_state(&ws, &p.run_id, RunState::Stopped, None));
                    }
                    Some(ServerMessage::ok(id, &Empty {}))
                }
                Request::RunList(_) => {
                    Some(ServerMessage::ok(id, &RunListResult { runs: runs.clone() }))
                }
                other => Some(ServerMessage::err(
                    id,
                    RpcError::internal(format!("not implemented: {}", other.method_name())),
                )),
            };

            // The held `pty.close` is answered as soon as the window has a pane
            // of its own, and every PTY ends with it.
            if let Some(close_id) = held_close {
                if open_ptys.len() >= 2 {
                    held_close = None;
                    for (workspace_id, pty_id) in open_ptys.drain(..) {
                        follow_ups.push(ServerMessage::event(
                            Some(workspace_id),
                            Event::PtyExit { pty_id, code: 0 },
                        ));
                    }
                    follow_ups.push(ServerMessage::ok(close_id, &Empty {}));
                }
            }

            let mut batch = reply.as_ref().map(codec::encode).unwrap_or_default();
            for message in &follow_ups {
                batch.push_str(&codec::encode(message));
            }
            if w.write_all(batch.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    (addr, journal, replies, allowlists)
}

#[test]
fn request_order_is_checked_as_a_subsequence() {
    /// A run the way the journal really comes out: the connect-time calls, the
    /// window's own requests for each tab, and the script's, interleaved.
    fn journal(methods: &[&str]) -> Vec<String> {
        methods.iter().map(|s| (*s).to_owned()).collect()
    }

    let seen = journal(&[
        "hello",
        "workspace.list",
        "system.check_prereqs",
        "workspace.create",
        "fs.list_dir",
        "fs.watch",
        "workspace.changes",
        "repo.detect_run_configs",
        "run.list",
        "agent.start",
        "agent.history",
        "agent.send",
        "agent.permission_reply",
        "fs.list_dir",
        "fs.read_file",
        "fs.watch",
        "workspace.diff",
        "agent.stop",
        "workspace.create",
        "fs.list_dir",
        "repo.detect_run_configs",
        "run.list",
        "pty.open",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "pty.open",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(contains_in_order(&seen, &EXPECTED));
    // Order still matters: a `pty.open` before the first create is not a match.
    let reordered = journal(&[
        "hello",
        "pty.open",
        "workspace.create",
        "agent.start",
        "agent.history",
        "agent.send",
        "agent.permission_reply",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "agent.stop",
        "repo.detect_run_configs",
        "run.list",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(!contains_in_order(&reordered, &EXPECTED));
    // Nor does the diff count as the file's own load: a run that opened the
    // diff first would put `workspace.diff` ahead of `fs.read_file`.
    let diff_first = journal(&[
        "hello",
        "workspace.create",
        "agent.start",
        "agent.history",
        "agent.send",
        "agent.permission_reply",
        "fs.list_dir",
        "workspace.diff",
        "fs.read_file",
        "agent.stop",
        "repo.detect_run_configs",
        "run.list",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "pty.open",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(!contains_in_order(&diff_first, &EXPECTED));
    // A transcript that replayed its history only after the turn had started
    // would be a pane attached too late to have shown the permission bar.
    let history_late = journal(&[
        "hello",
        "workspace.create",
        "agent.start",
        "agent.send",
        "agent.history",
        "agent.permission_reply",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "agent.stop",
        "repo.detect_run_configs",
        "run.list",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "pty.open",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(!contains_in_order(&history_late, &EXPECTED));
    // A turn that never asked for permission, or was never answered, is not a
    // match: the whole point of the Claude half is that one reply went out.
    let unanswered = journal(&[
        "hello",
        "workspace.create",
        "agent.start",
        "agent.history",
        "agent.send",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "agent.stop",
        "repo.detect_run_configs",
        "run.list",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "pty.open",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(!contains_in_order(&unanswered, &EXPECTED));
    // An "Allow host" that sent a list without reading the daemon's first would
    // put `workspace.set_allowlist` ahead of `workspace.get`. The order is the
    // claim: the IDE extends the workspace's own allowlist rather than
    // replacing it with whatever it happened to be holding.
    let allowlist_unread = journal(&[
        "hello",
        "workspace.create",
        "agent.start",
        "agent.history",
        "agent.send",
        "agent.permission_reply",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "agent.stop",
        "repo.detect_run_configs",
        "run.list",
        "run.start",
        "workspace.set_allowlist",
        "workspace.get",
        "run.stop",
        "pty.open",
        "pty.close",
        "workspace.destroy",
    ]);
    assert!(!contains_in_order(&allowlist_unread, &EXPECTED));
    // A missing step is not a match either.
    let short = journal(&["hello", "workspace.create", "agent.start", "fs.list_dir"]);
    assert!(!contains_in_order(&short, &EXPECTED));
}
