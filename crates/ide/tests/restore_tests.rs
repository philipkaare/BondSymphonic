//! What the IDE makes of a daemon that already has agents when it starts.
//!
//! The daemon keeps a record of every agent it has run and serves its
//! transcript afterwards, so a Claude agent outlives the daemon that started
//! it. Until the IDE read `WorkspaceInfo.agents`, none of that survived an
//! *IDE* restart: every Claude workspace came back as a terminal tab bound to
//! no agent, and no pane in the UI could reach a transcript the daemon was
//! still serving.
//!
//! This is that path end to end, and the shape of it is the point: the real
//! `bondsymphonic-ide` binary, offscreen, against a fake daemon that already
//! has the workspace and the agent when the IDE connects. Nothing in the script
//! creates or starts anything -- the only step is `quit` -- so an `agent.history`
//! in the journal can only have come from a tab the IDE rebuilt out of the
//! daemon's list.

use bondsymphonic_proto::*;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const TOKEN: &str = "restore-token";
/// Nothing but the quit, which waits two seconds first. Everything asserted
/// below has to happen without a step asking for it.
const SCRIPT: &str = "quit";
const WS_ID: &str = "ws_restore1";
const AGENT_ID: &str = "ag_restore1";
const SESSION_ID: &str = "sess-restore";
const MODEL: &str = "claude-opus-5";
const PERMISSION_MODE: &str = "acceptEdits";
/// The quit step's two seconds plus a cold Qt start.
const RUN_LIMIT: Duration = Duration::from_secs(90);

type Journal = Arc<Mutex<Vec<String>>>;

#[test]
fn a_listed_claude_agent_comes_back_as_a_claude_tab_with_its_transcript() {
    if bondsymphonic_ide::testing::skip_without_qt("restore") {
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, journal) = rt.block_on(fake_daemon());

    // Never the developer's real `%APPDATA%\BondSymphonic`.
    let config = std::env::temp_dir().join(format!("bs-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("restore config dir");

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
    eprintln!("restore: the fake daemon answered {seen:?}");

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

    // The claim. `agent.history` is only ever sent by `TranscriptModel::attach`,
    // and the model only attaches because the window built a transcript pane for
    // a tab whose adapter is Claude and whose agent id is set. Both of those came
    // out of `workspace.list`: nothing here started an agent.
    let listed = seen
        .iter()
        .position(|m| m == "workspace.list")
        .unwrap_or_else(|| panic!("the IDE never listed the daemon's workspaces\n{context}"));
    let wanted = format!("agent.history:{AGENT_ID}");
    assert!(
        seen[listed..].contains(&wanted),
        "the IDE never replayed the listed agent's transcript\n{context}"
    );
    assert!(
        !seen.iter().any(|m| m == "agent.start"),
        "the transcript must come from the listed agent, not from a new one\n{context}"
    );
    // A tab rebuilt as a terminal would have opened a shell for the pane instead.
    assert!(
        !seen.iter().any(|m| m == "pty.open"),
        "the restored tab was built as a terminal, not as a Claude pane\n{context}"
    );
    // The replay is the whole transcript, so the pane comes back with the
    // conversation and with a session to resume.
    assert!(
        !logs.contains("agent.history failed"),
        "the IDE refused the history the fake daemon served\n{context}"
    );

    let _ = std::fs::remove_dir_all(&config);
}

/// A daemon that has been running for a while: one workspace, one Claude agent
/// in it that has already ended, and the transcript still on disk. What a real
/// daemon looks like to an IDE that has just been restarted.
async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let recorded = journal.clone();

    tokio::spawn(async move {
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
                let ClientMessage::Request { id, request } =
                    codec::decode(trimmed).expect("decode");
                // The agent id travels with the method for `agent.history`: the
                // assertion is about *which* agent the pane attached to, and a
                // bare method name cannot say.
                let method = match &request {
                    Request::AgentHistory(p) => format!("agent.history:{}", p.agent_id.0),
                    other => other.method_name().to_owned(),
                };
                recorded.lock().expect("journal mutex").push(method);
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
                    Request::WorkspaceList {} => ServerMessage::ok(
                        id,
                        &WorkspaceListResult {
                            workspaces: vec![workspace()],
                        },
                    ),
                    Request::WorkspaceGet(_) => ServerMessage::ok(id, &workspace()),
                    // An agent this daemon reloaded from its records: the
                    // process is gone, the transcript is not, and the pane
                    // comes back on Restart with a session to resume.
                    Request::AgentHistory(_) => ServerMessage::ok(
                        id,
                        &HistoryResult {
                            messages: vec![AgentMessage {
                                seq: 1,
                                ts: "2026-09-10T10:00:01Z".to_owned(),
                                body: AgentMessageBody::System {
                                    subtype: "init".to_owned(),
                                    data: serde_json::json!({
                                        "session_id": SESSION_ID,
                                        "model": MODEL,
                                    }),
                                },
                            }],
                            state: AgentState::Exited,
                            detail: Some("the agent ended when the daemon restarted".to_owned()),
                        },
                    ),
                    Request::FsListDir(_) => ServerMessage::ok(
                        id,
                        &ListDirResult {
                            entries: vec![entry("src", true), entry("README.md", false)],
                        },
                    ),
                    Request::WorkspaceChanges(_) => {
                        ServerMessage::ok(id, &ChangesResult { files: vec![] })
                    }
                    Request::WorkspaceStatus(_) => {
                        ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
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
                    Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                    other => ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    ),
                };
                if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });

    (addr, journal)
}

/// The workspace the daemon has, with the agent it ran in it. `agent_records`
/// is the whole point: the adapter is what rebuilds the pane, and the id is what
/// the pane attaches to.
fn workspace() -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(WS_ID.to_owned()),
        name: "restored".to_owned(),
        repo_path: "/restore/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: "bs/restored/work".to_owned(),
        worktree_path: format!("/wt/{WS_ID}"),
        created_at: "2026-09-10T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        state: WorkspaceState::Ready,
        // Both lists, the way the daemon sends them: the bare ids an older
        // client would read, and the records this one rebuilds the tab from.
        agents: vec![AgentId(AGENT_ID.to_owned())],
        agent_records: vec![AgentSummary {
            id: AgentId(AGENT_ID.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: Some(SESSION_ID.to_owned()),
            command: None,
            model: Some(MODEL.to_owned()),
            permission_mode: Some(PERMISSION_MODE.to_owned()),
        }],
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

/// Task 9: what a restored tab remembers beyond its place in a group.
///
/// The daemon is authoritative about what a workspace *is*, but the command a
/// terminal tab was opened with and the run configuration the user picked are
/// the IDE's own and exist nowhere else. Until `state.json` carried them, every
/// restart handed back a terminal tab with no command and a Run panel that had
/// forgotten which configuration the workspace runs.
mod task_9 {
    use bondsymphonic_ide::model::app_state::{AgentTab, Workspaces};
    use bondsymphonic_ide::model::persistence::{load, save, StateFile};
    use bondsymphonic_proto::{WorkspaceId, WorkspaceInfo, WorkspaceState};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bs-restore-t9-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn info(id: &str, name: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId(id.to_owned()),
            name: name.to_owned(),
            repo_path: "/repo".to_owned(),
            base_branch: "main".to_owned(),
            branch: format!("bs/{name}/work"),
            worktree_path: format!("/wt/{name}"),
            created_at: "2026-09-11T10:00:00Z".to_owned(),
            allowlist: Vec::new(),
            state: WorkspaceState::Ready,
            agents: Vec::new(),
            agent_records: Vec::new(),
            runs: Vec::new(),
        }
    }

    #[test]
    fn a_command_and_a_run_config_come_back_after_a_save_and_load() {
        let path = temp_dir("roundtrip").join("state.json");
        let terminal = info("ws_term", "term");
        let claude = info("ws_claude", "claude");

        let mut model = Workspaces::new_default();
        let mut tab = AgentTab::from_workspace_info(&terminal);
        tab.command = Some("npm run dev".to_owned());
        model.add_tab(0, tab);
        let mut tab = AgentTab::from_workspace_info(&claude);
        tab.run_config = Some("dev".to_owned());
        model.add_tab(0, tab);

        let mut state = StateFile::default();
        state.set_groups(model.persisted_groups(), Some("ws_claude".to_owned()));
        save(&path, &state).expect("state.json written");

        let reread = load(&path);
        let restored = Workspaces::from_persisted(
            &reread.groups,
            &[terminal, claude],
            reread.active_workspace.as_deref(),
        );

        let (g, t) = restored
            .find(&WorkspaceId("ws_term".to_owned()))
            .expect("the terminal tab is back");
        assert_eq!(
            restored.groups[g].tabs[t].command.as_deref(),
            Some("npm run dev"),
            "the command the terminal tab was opened with"
        );
        let (g, t) = restored
            .find(&WorkspaceId("ws_claude".to_owned()))
            .expect("the agent tab is back");
        assert_eq!(
            restored.groups[g].tabs[t].run_config.as_deref(),
            Some("dev"),
            "the run configuration the user picked"
        );
    }

    /// A file written before the fields existed still loads, with both of them
    /// empty rather than the whole arrangement lost.
    #[test]
    fn an_old_state_file_without_the_fields_still_loads() {
        let path = temp_dir("old-format").join("state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"groups":[{"name":"Default","workspace_ids":["ws_term"]}],"active_workspace":"ws_term"}"#,
        )
        .expect("an old-format state.json");

        let reread = load(&path);
        assert_eq!(reread.groups.len(), 1, "the group survived");
        assert_eq!(reread.groups[0].workspace_ids, ["ws_term"]);

        let restored =
            Workspaces::from_persisted(&reread.groups, &[info("ws_term", "term")], Some("ws_term"));
        let tab = restored.active().expect("the restored tab");
        assert_eq!(tab.command, None);
        assert_eq!(tab.run_config, None);
    }
}

/// Task 10: the arrangement a daemon that has not answered `workspace.list`
/// yet must not be allowed to destroy, and the prerequisite check that retries
/// itself.
///
/// Both are about the same few seconds. Between connecting and the first
/// successful list the IDE knows nothing about the user's groups, and the
/// model on screen is the one-group placeholder; anything it reports about the
/// arrangement in that window is a guess, and writing the guess back costs the
/// user every group they had. The prerequisite check lands in the same window,
/// and a daemon that was not ready to answer it left the setup page empty for
/// the session.
mod task_10 {
    use super::{drain, entry, wait_for, Journal, TOKEN};
    use bondsymphonic_ide::model::persistence::StateStore;
    use bondsymphonic_ide::qobjects::app_controller::restore_workspaces;
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// Create one workspace, then quit. Nothing in the script touches a group,
    /// a prerequisite or the setup page, so everything asserted below happened
    /// because the IDE decided it.
    const SCRIPT: &str = "create,quit";
    /// The two workspaces the saved arrangement names, and the one the script
    /// makes.
    const WS_A: &str = "ws_feature_a";
    const WS_B: &str = "ws_feature_b";
    const WS_NEW: &str = "ws_created";
    const GROUP_A: &str = "Feature-A";
    const GROUP_B: &str = "Feature-B";
    /// How long the fake daemon sits on the first `workspace.create` before
    /// answering. It is what keeps the run alive long enough for the
    /// prerequisite retry to fall due without a step asking for it;
    /// `workspace.create` has a 120 s client timeout, so this is nowhere near
    /// it.
    const CREATE_DELAY: Duration = Duration::from_secs(4);
    /// The create delay, the script settle and quit times, and a cold Qt start.
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    /// The saved arrangement, written before the IDE starts: two named groups
    /// that have to survive a run in which `workspace.list` never answers.
    const SAVED_STATE: &str = concat!(
        r#"{"version":1,"groups":["#,
        r#"{"name":"Feature-A","workspace_ids":["ws_feature_a"]},"#,
        r#"{"name":"Feature-B","workspace_ids":["ws_feature_b"]}],"#,
        r#""active_workspace":"ws_feature_a"}"#
    );

    #[test]
    fn a_create_before_the_first_list_leaves_the_saved_groups_alone() {
        if bondsymphonic_ide::testing::skip_without_qt("restore/task-10") {
            return;
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config = std::env::temp_dir().join(format!("bs-restore-groups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        std::fs::write(&state_path, SAVED_STATE).expect("the saved arrangement is written");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
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
        let written = std::fs::read_to_string(&state_path).unwrap_or_default();
        let context = format!(
            "requests: {seen:?}\n--- state.json ---\n{written}\n--- stdout ---\n{out}\n\
             --- stderr ---\n{err}"
        );

        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !logs.contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        // The premise: the daemon really did refuse both attempts, so no
        // restore ran and the model on screen was the placeholder throughout.
        assert_eq!(
            seen.iter().filter(|m| *m == "workspace.list").count(),
            2,
            "the IDE must ask twice and then give up\n{context}"
        );
        assert!(
            seen.iter().any(|m| m == "workspace.create"),
            "the script never created a workspace\n{context}"
        );

        // IC6. The file still names the user's groups: an arrangement reported
        // before the first list describes the placeholder, not the user.
        let state: serde_json::Value = serde_json::from_str(&written)
            .unwrap_or_else(|e| panic!("state.json parses: {e}\n{context}"));
        let empty = Vec::new();
        let names: Vec<String> = state["groups"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .map(|g| g["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == GROUP_A) && names.iter().any(|n| n == GROUP_B),
            "the saved groups were overwritten: {names:?}\n{context}"
        );

        // And the point of keeping them: the next restore, against a daemon
        // that is answering again, gives the user their groups back.
        let store = StateStore::load(Some(state_path.clone()));
        let list = vec![workspace(WS_A, "alpha"), workspace(WS_B, "beta")];
        let (model, _) = restore_workspaces(&store, &list);
        let restored: Vec<&str> = model.groups.iter().map(|g| g.name.as_str()).collect();
        assert!(
            restored.contains(&GROUP_A) && restored.contains(&GROUP_B),
            "the restore no longer yields the groups: {restored:?}\n{context}"
        );

        // IC7. The first `system.check_prereqs` was refused and nothing in the
        // script asks again, so a second one can only be the IDE retrying.
        assert!(
            seen.iter().filter(|m| *m == "system.check_prereqs").count() >= 2,
            "a refused prerequisite check was never retried\n{context}"
        );
        assert!(
            logs.contains("prerequisites checked"),
            "the retry never produced a prerequisite list\n{context}"
        );

        let _ = std::fs::remove_dir_all(&config);
    }

    /// A daemon that cannot list its workspaces and refuses the first
    /// prerequisite check: the two failures the IDE has to survive without
    /// spending the user's arrangement on them.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();
        let prereq_calls = Arc::new(AtomicUsize::new(0));
        let creates = Arc::new(AtomicUsize::new(0));

        tokio::spawn(async move {
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
                                    adapters: vec![
                                        AgentAdapterKind::Terminal,
                                        AgentAdapterKind::Claude,
                                    ],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        // Refused once. Nothing in the script re-checks, so a
                        // second call is the IDE asking again by itself.
                        Request::SystemCheckPrereqs {} => {
                            if prereq_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                ServerMessage::err(
                                    id,
                                    RpcError::internal("the prerequisite check is not ready"),
                                )
                            } else {
                                ServerMessage::ok(
                                    id,
                                    &CheckPrereqsResult {
                                        items: vec![PrereqStatus {
                                            name: "git".into(),
                                            ok: true,
                                            detail: "git version 2.43".into(),
                                            fix_hint: None,
                                        }],
                                    },
                                )
                            }
                        }
                        // Never answered: the IDE asks twice and gives up, so
                        // no restore runs and the model on screen stays the
                        // placeholder for the whole run.
                        Request::WorkspaceList {} => {
                            ServerMessage::err(id, RpcError::internal("no list for you"))
                        }
                        Request::WorkspaceCreate(p) => {
                            // The first create is slow on purpose: it is what
                            // keeps the run alive past the prerequisite retry.
                            if creates.fetch_add(1, Ordering::SeqCst) == 0 {
                                tokio::time::sleep(CREATE_DELAY).await;
                            }
                            ServerMessage::ok(id, &workspace(WS_NEW, &p.name))
                        }
                        Request::WorkspaceGet(_) => {
                            ServerMessage::ok(id, &workspace(WS_NEW, "smoke"))
                        }
                        Request::PtyOpen(_) => ServerMessage::ok(
                            id,
                            &PtyOpenResult {
                                pty_id: PtyId("pty_fake".to_owned()),
                            },
                        ),
                        Request::FsListDir(_) => ServerMessage::ok(
                            id,
                            &ListDirResult {
                                entries: vec![entry("src", true), entry("README.md", false)],
                            },
                        ),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    fn workspace(id: &str, name: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId(id.to_owned()),
            name: name.to_owned(),
            repo_path: "/restore/repo".to_owned(),
            base_branch: "main".to_owned(),
            branch: format!("bs/{name}/work"),
            worktree_path: format!("/wt/{id}"),
            created_at: "2026-09-11T10:00:00Z".to_owned(),
            allowlist: Vec::new(),
            state: WorkspaceState::Ready,
            agents: Vec::new(),
            agent_records: Vec::new(),
            runs: Vec::new(),
        }
    }
}
