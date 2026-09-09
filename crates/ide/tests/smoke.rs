//! Offscreen end-to-end smoke test.
//!
//! Runs the real `bondsymphonic-ide` binary with `QT_QPA_PLATFORM=offscreen`
//! against an in-process fake daemon, and drives it with the two test hooks the
//! IDE carries for exactly this purpose: `BS_DAEMON_ADDR`/`BS_DAEMON_TOKEN`
//! (connect here instead of starting a daemon in WSL) and `BS_SMOKE_SCRIPT`
//! (perform these steps once connected). Both are inert when unset.
//!
//! What it proves: the window builds, the daemon connection comes up, the
//! workspace/PTY/file-tree requests reach the daemon in the right order, an
//! editor tab and a diff tab open through the same controller signals the
//! Explorer emits and fetch their contents (`fs.read_file`, `workspace.diff`)
//! and their live-update watches (`fs.watch`, `workspace.changes`), panes whose
//! process has exited are torn down without talking to the daemon about the
//! PTYs it has already reaped, the Qt event loop is still responsive at the end
//! (the `quit` step runs on it), and the process ends with status 0 well inside
//! the time limit.

use base64::Engine as _;
use bondsymphonic_proto::*;
use std::collections::HashMap;
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
const SCRIPT: &str = "create,open,tree,open_file,open_diff,close,destroy,quit";
/// The file `open_file` and `open_diff` act on, and the one entry of the fake
/// `fs.list_dir` listing that is not a directory.
const OPEN_PATH: &str = "README.md";
/// The `quit` step alone waits 2 s, `close` another 0.75 s and each of
/// `open_file` and `open_diff` another 0.75 s; the rest is a Qt startup on a
/// cold cache.
const RUN_LIMIT: Duration = Duration::from_secs(40);
/// The methods the script must produce, in this order. `fs.read_file` is the
/// editor tab loading its file and `workspace.diff` the diff tab loading its
/// alignment, so their place in the sequence is what shows the two tabs opened
/// in the order the script asked for them.
const EXPECTED: [&str; 8] = [
    "hello",
    "workspace.create",
    "pty.open",
    "fs.list_dir",
    "fs.read_file",
    "workspace.diff",
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
const NO_WARNINGS: [&str; 4] = [
    "fs.read_file failed",
    "workspace.diff failed",
    "workspace.changes failed",
    "fs.watch enable",
];

/// Every request method the fake daemon answered, in arrival order.
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
    let (addr, journal) = rt.block_on(fake_daemon());

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
    let (out, err) = (
        out.recv().unwrap_or_default(),
        err.recv().unwrap_or_default(),
    );
    let seen = journal.lock().expect("journal mutex").clone();
    let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
    // Both pipes together. `tracing_subscriber::fmt()` writes to *stdout* by
    // default and `main` does not override the writer, so every warning the IDE
    // logs arrives on stdout; only a panic message comes out on stderr. An
    // assertion that read stderr alone would pass whatever was logged, which is
    // what the three terminal-warning assertions below used to do.
    let logs = format!("{out}\n{err}");
    eprintln!("smoke: the fake daemon answered {seen:?}");

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
    // The `close` step ended every PTY the fake daemon had open, so by the time
    // `destroy` tears the panes down their processes have exited. A pane in that
    // state must not send `pty.close` or `pty.resize`: the daemon has reaped
    // those PTYs and answers `NotFound`, which used to put an error banner over
    // a pane whose only news was that its process had finished.
    let after_destroy: Vec<&String> = seen
        .iter()
        .skip_while(|m| *m != "workspace.destroy")
        .collect();
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

fn workspace(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/smoke/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}/work"),
        worktree_path: format!("/wt/{id}"),
        created_at: "2026-09-09T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        state,
        agents: Vec::new(),
        runs: Vec::new(),
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
async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded = journal.clone();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        // One workspace id per created name, so a repeated `create` is answered
        // consistently and `pty.open` can be checked against a known workspace.
        let mut workspaces: HashMap<String, String> = HashMap::new();
        let mut ptys = 0usize;
        // Every PTY handed out and the workspace it belongs to, so `pty.close`
        // can end all of them at once.
        let mut open_ptys: Vec<(WorkspaceId, PtyId)> = Vec::new();
        // The id of a `pty.close` whose reply is being held; see the arm below.
        let mut held_close: Option<u64> = None;
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
                            adapters: vec![],
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
                    let creating = workspace(&ws_id, &p.name, WorkspaceState::Creating);
                    let ready = workspace(&ws_id, &p.name, WorkspaceState::Ready);
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
                    },
                )),
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

    (addr, journal)
}

#[test]
fn request_order_is_checked_as_a_subsequence() {
    let seen: Vec<String> = [
        "hello",
        "workspace.list",
        "workspace.create",
        "fs.list_dir",
        "pty.open",
        "fs.watch",
        "workspace.changes",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "pty.close",
        "workspace.destroy",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert!(contains_in_order(&seen, &EXPECTED));
    // Order still matters: a `pty.open` before the create is not a match.
    let reordered: Vec<String> = [
        "hello",
        "pty.open",
        "workspace.create",
        "fs.list_dir",
        "fs.read_file",
        "workspace.diff",
        "pty.close",
        "workspace.destroy",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert!(!contains_in_order(&reordered, &EXPECTED));
    // Nor does the diff count as the file's own load: a run that opened the
    // diff first would put `workspace.diff` ahead of `fs.read_file`.
    let diff_first: Vec<String> = [
        "hello",
        "workspace.create",
        "pty.open",
        "fs.list_dir",
        "workspace.diff",
        "fs.read_file",
        "pty.close",
        "workspace.destroy",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert!(!contains_in_order(&diff_first, &EXPECTED));
    // A missing step is not a match either.
    let short: Vec<String> = ["hello", "workspace.create", "pty.open", "fs.list_dir"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert!(!contains_in_order(&short, &EXPECTED));
}
