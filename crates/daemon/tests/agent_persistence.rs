//! Agents outlive the daemon that started them.
//!
//! A daemon restart ends every agent process, but the transcript on disk and
//! the record beside it are what let the IDE go on showing the tab, replay what
//! was said, and offer to resume the session. These tests drive that through
//! the real RPC surface with the fake `claude` the agent tests use, restarting
//! the daemon over the same data directory the way `main.rs` does.
//!
//! `BS_CLAUDE_BIN` and `FAKE_CLAUDE_FIXTURE` are process-wide, so the tests
//! here take [`ENV`] for their whole duration and run one at a time.

mod common;

use bondsymphonic_daemon::agents::persist::AgentRecords;
use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Serialises the tests: each one points `BS_CLAUDE_BIN` and
/// `FAKE_CLAUDE_FIXTURE` at what it needs, and both are process-wide.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The interpreter to run the fake with, or `None` when this host has none.
fn python() -> Option<&'static str> {
    // Windows ships a `python3` App Execution Alias that is not an interpreter,
    // so the real name is tried first there.
    let candidates: [&str; 2] = if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    };
    candidates.into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A path as `BS_CLAUDE_BIN` can carry it. `shell_words` reads backslashes as
/// escapes, so a Windows path goes in with forward slashes, which every Windows
/// API accepts.
fn arg_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn use_fake_claude(py: &str, fixture: &str) {
    std::env::set_var(
        "BS_CLAUDE_BIN",
        format!(
            "\"{py}\" \"{}\"",
            arg_path(&fixture_dir().join("fake_claude.py"))
        ),
    );
    std::env::set_var(
        "FAKE_CLAUDE_FIXTURE",
        fixture_dir().join("claude-stream").join(fixture),
    );
    std::env::remove_var("FAKE_CLAUDE_ECHO_DELAY");
}

fn options() -> AgentStartOptions {
    AgentStartOptions {
        command: None,
        resume_session: None,
        model: None,
        permission_mode: None,
        api_key: None,
    }
}

async fn start_agent(
    c: &mut Client,
    ws: &WorkspaceId,
    options: AgentStartOptions,
) -> Result<AgentId, RpcError> {
    let v = c
        .call(Request::AgentStart(AgentStartParams {
            workspace_id: ws.clone(),
            adapter: AgentAdapterKind::Claude,
            options,
        }))
        .await?;
    Ok(serde_json::from_value::<AgentStartResult>(v)
        .unwrap()
        .agent_id)
}

async fn history(c: &mut Client, ag: &AgentId) -> HistoryResult {
    let v = c
        .call(Request::AgentHistory(AgentIdParams {
            agent_id: ag.clone(),
        }))
        .await
        .unwrap();
    serde_json::from_value(v).unwrap()
}

/// Waits until `ag`'s transcript holds at least `want` messages, so a test does
/// not race the fake's replay.
async fn wait_for_messages(c: &mut Client, ag: &AgentId, want: usize) -> HistoryResult {
    let start = Instant::now();
    loop {
        let h = history(c, ag).await;
        if h.messages.len() >= want || start.elapsed() > Duration::from_secs(20) {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn records_of(root: &Path) -> Vec<bondsymphonic_daemon::agents::persist::AgentRecord> {
    AgentRecords::new(root.join("agents.json")).load()
}

fn transcript_of(root: &Path, ag: &AgentId) -> PathBuf {
    root.join("transcripts").join(format!("{ag}.ndjson"))
}

/// The whole of it: an agent is recorded while it runs, the record is closed
/// when it stops, and a second daemon over the same data directory brings it
/// back as an exited agent whose history still reads.
#[tokio::test]
async fn an_agent_survives_a_daemon_restart_as_exited_history() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    use_fake_claude(py, "simple_turn.ndjson");

    let (port, token, _d, cancel) = start_daemon(&root).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let opts = AgentStartOptions {
        api_key: Some("sk-ant-not-a-real-key".into()),
        model: Some("claude-opus-5".into()),
        ..options()
    };
    let ag = start_agent(&mut c, &ws.id, opts).await.unwrap();

    // init, assistant text, result.
    wait_for_messages(&mut c, &ag, 3).await;
    c.call(Request::AgentSend(AgentSendParams {
        agent_id: ag.clone(),
        text: "hello".into(),
    }))
    .await
    .unwrap();
    let turn = wait_for_messages(&mut c, &ag, 6).await;
    assert!(
        turn.messages
            .iter()
            .any(|m| matches!(&m.body, AgentMessageBody::UserText { text } if text == "hello")),
        "the fake must have taken the turn: {:?}",
        turn.messages
    );

    c.call(Request::AgentStop(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();
    // Read after the stop, not before it: the adapter drains what the process
    // wrote on its way out, so anything read earlier can still grow.
    let before = history(&mut c, &ag).await;

    // The record is on disk, closed, with the session the agent reported and
    // without the key it was started with.
    let recs = records_of(&root);
    assert_eq!(recs.len(), 1, "{recs:?}");
    let rec = &recs[0];
    assert_eq!(rec.agent_id, ag);
    assert_eq!(rec.workspace_id, ws.id);
    assert_eq!(rec.adapter, AgentAdapterKind::Claude);
    assert_eq!(rec.session_id.as_deref(), Some("sess-1"));
    assert_eq!(rec.options.model.as_deref(), Some("claude-opus-5"));
    assert!(rec.started_at.starts_with("20"), "{:?}", rec.started_at);
    assert!(rec.ended_at.is_some(), "a stopped agent's record is closed");
    assert_eq!(rec.options.api_key, None, "the key must never be stored");
    let raw = std::fs::read_to_string(root.join("agents.json")).unwrap();
    assert!(
        !raw.contains("sk-ant-not-a-real-key"),
        "the key must not reach the file in any shape: {raw}"
    );

    // A second daemon over the same data directory, the way `main.rs` starts.
    cancel.cancel();
    drop(c);
    let (port, token, d2, cancel2) = start_daemon(&root).await;
    d2.restore().await;
    let mut c = Client::connect(port, &token).await;

    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(
        info.agents,
        vec![ag.clone()],
        "the restored agent must still belong to its workspace"
    );
    // And its record, which is what lets a client that restarted alongside the
    // daemon rebuild this workspace's tab as the Claude tab it was: the adapter
    // it ran, the state it came back in, and the session a Restart resumes.
    assert_eq!(info.agent_records.len(), 1);
    assert_eq!(info.agent_records[0].id, ag);
    assert_eq!(info.agent_records[0].adapter, AgentAdapterKind::Claude);
    assert_eq!(info.agent_records[0].state, AgentState::Exited);
    assert!(
        info.agent_records[0].session_id.is_some(),
        "a resumable agent must name its session"
    );
    // And through `workspace.list`, which is what the IDE re-syncs with after a
    // reconnect. Same code path as `workspace.get`, but it is the one the
    // requirement names.
    let listed: WorkspaceListResult =
        serde_json::from_value(c.call(Request::WorkspaceList {}).await.unwrap()).unwrap();
    let mine = listed
        .workspaces
        .iter()
        .find(|w| w.id == ws.id)
        .expect("the workspace must still be listed");
    assert_eq!(mine.agents, vec![ag.clone()]);
    assert_eq!(mine.agent_records.len(), 1);
    assert_eq!(mine.agent_records[0].adapter, AgentAdapterKind::Claude);

    let after = history(&mut c, &ag).await;
    assert_eq!(
        after.messages, before.messages,
        "the transcript must survive verbatim"
    );
    assert_eq!(after.state, AgentState::Exited);
    let detail = after.detail.clone().unwrap_or_default();
    assert!(
        detail.contains("restarted"),
        "the detail must say why the agent is gone: {detail:?}"
    );

    // It cannot be talked to: the process is gone and the client is told to
    // start a new agent instead.
    let e = c
        .call(Request::AgentSend(AgentSendParams {
            agent_id: ag.clone(),
            text: "still there?".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
    assert!(
        e.message.contains("resume"),
        "the message must point at the way forward: {}",
        e.message
    );
    // Stopping something already stopped is a no-op, not an error.
    c.call(Request::AgentStop(AgentIdParams {
        agent_id: ag.clone(),
    }))
    .await
    .unwrap();

    // Destroying the workspace takes the record and the transcript with it.
    assert!(transcript_of(&root, &ag).exists());
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
        workspace_id: ws.id.clone(),
        force: true,
    }))
    .await
    .unwrap();
    assert!(records_of(&root).is_empty(), "the record must be gone");
    assert!(
        !transcript_of(&root, &ag).exists(),
        "the transcript must be gone"
    );

    cancel2.cancel();
}

/// A start that never got a process leaves no record behind.
///
/// The record is written *before* the spawn, which is what keeps the session id
/// (see the unit tests in `agents::tests`), so the failure path has to take it
/// back: an id nothing ever ran under must not come back from a restart looking
/// like an agent that ended.
#[tokio::test]
async fn a_failed_start_leaves_no_record() {
    let _guard = ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    // A program that is not there: the spawn fails, nothing runs.
    std::env::set_var(
        "BS_CLAUDE_BIN",
        arg_path(&dir.path().join("no-such-claude-binary")),
    );
    std::env::remove_var("FAKE_CLAUDE_FIXTURE");

    let (port, token, _d, cancel) = start_daemon(&root).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    start_agent(&mut c, &ws.id, options())
        .await
        .expect_err("a missing program cannot start an agent");
    assert!(
        records_of(&root).is_empty(),
        "a start that failed must not leave a record: {:?}",
        records_of(&root)
    );

    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert!(info.agents.is_empty());

    cancel.cancel();
}

/// An agent that was still running when the daemon died. Its record has no
/// `ended_at`, and restoring is what closes it.
#[tokio::test]
async fn an_agent_live_at_shutdown_comes_back_closed() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    use_fake_claude(py, "simple_turn.ndjson");

    let (port, token, _d, cancel) = start_daemon(&root).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let ag = start_agent(&mut c, &ws.id, options()).await.unwrap();
    wait_for_messages(&mut c, &ag, 3).await;

    let open = records_of(&root);
    assert_eq!(open.len(), 1);
    assert_eq!(
        open[0].ended_at, None,
        "a running agent's record stays open"
    );

    // The daemon goes without stopping the agent, which is what a crash or a
    // `pkill` looks like.
    cancel.cancel();
    drop(c);
    let (port, token, d2, cancel2) = start_daemon(&root).await;
    d2.restore().await;
    let mut c = Client::connect(port, &token).await;

    let h = history(&mut c, &ag).await;
    assert_eq!(h.state, AgentState::Exited);
    assert!(
        h.detail.clone().unwrap_or_default().contains("restarted"),
        "{:?}",
        h.detail
    );
    assert!(
        !h.messages.is_empty(),
        "the transcript from before the restart must still be there"
    );

    let closed = records_of(&root);
    assert_eq!(closed.len(), 1);
    assert!(
        closed[0].ended_at.is_some(),
        "restoring must close the record it found open"
    );
    assert_eq!(closed[0].session_id.as_deref(), Some("sess-1"));

    // Restarting again must not re-open it or lose it.
    cancel2.cancel();
    drop(c);
    let (port, token, d3, cancel3) = start_daemon(&root).await;
    d3.restore().await;
    let mut c = Client::connect(port, &token).await;
    assert_eq!(records_of(&root).len(), 1);
    assert_eq!(history(&mut c, &ag).await.state, AgentState::Exited);

    // A new agent started after the restore joins the restored one rather than
    // replacing it, and gets an id of its own.
    let fresh = start_agent(&mut c, &ws.id, options()).await.unwrap();
    assert_ne!(fresh, ag);
    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(
        info.agents,
        vec![ag.clone(), fresh.clone()],
        "the restored agent comes first, the new one after it"
    );
    // The two lists are the same agents in the same order, which is the whole
    // contract between them.
    assert_eq!(
        info.agent_records
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        info.agents
    );
    assert_eq!(records_of(&root).len(), 2);

    cancel3.cancel();
}

/// A records file that will not parse is not a reason to refuse to start: the
/// workspaces are the important state, and they are in a different file.
#[tokio::test]
async fn a_corrupt_records_file_does_not_stop_the_daemon() {
    let _guard = ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("agents.json"), "{ this is not json").unwrap();

    let (port, token, d, cancel) = start_daemon(&root).await;
    d.restore().await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    let v = c
        .call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert!(info.agents.is_empty());
    assert!(
        root.join("agents.json.corrupt").exists(),
        "the unreadable file is kept aside rather than silently dropped"
    );

    cancel.cancel();
}

/// `[claude] settings` in the repo's own `bondsymphonic.toml` wins over the
/// daemon user's copy, and a path that climbs out of the worktree is refused.
#[tokio::test]
async fn the_repos_claude_settings_reach_the_workspace_home() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    use_fake_claude(py, "simple_turn.ndjson");

    std::fs::create_dir_all(repo.join("tools")).unwrap();
    std::fs::write(
        repo.join("tools").join("claude-settings.json"),
        "{\"permissions\":{\"allow\":[\"Read\"]}}",
    )
    .unwrap();
    std::fs::write(
        repo.join("bondsymphonic.toml"),
        "[claude]\nsettings = \"tools/claude-settings.json\"\n",
    )
    .unwrap();
    common::commit_all(&repo, &[], "claude settings");

    let (port, token, d, cancel) = start_daemon(&root).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let ag = start_agent(&mut c, &ws.id, options()).await.unwrap();
    wait_for_messages(&mut c, &ag, 1).await;

    let home = d.dirs.home(&ws.id);
    assert_eq!(
        std::fs::read_to_string(home.join(".claude").join("settings.json")).unwrap(),
        "{\"permissions\":{\"allow\":[\"Read\"]}}",
        "the repo's settings must be the ones in the sandbox home"
    );

    // A path out of the worktree is refused, and the agent does not start.
    std::fs::write(
        Path::new(&ws.worktree_path).join("bondsymphonic.toml"),
        "[claude]\nsettings = \"../../escape.json\"\n",
    )
    .unwrap();
    let e = start_agent(&mut c, &ws.id, options()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");
    assert!(
        e.message.contains("settings"),
        "the message must name what was refused: {}",
        e.message
    );

    // A settings path that does not exist is refused just as plainly.
    std::fs::write(
        Path::new(&ws.worktree_path).join("bondsymphonic.toml"),
        "[claude]\nsettings = \"tools/absent.json\"\n",
    )
    .unwrap();
    let e = start_agent(&mut c, &ws.id, options()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");

    cancel.cancel();
}

/// Once a client has seen what the agent said after naming its session, the
/// session id is on disk: a daemon that dies at that moment comes back with a
/// session to resume. Every record write is made a second slower here, which a
/// reader that only queued the write would overtake.
#[tokio::test]
async fn the_session_id_is_on_disk_before_the_lines_after_it_are_seen() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    use_fake_claude(py, "simple_turn.ndjson");

    let (port, token, d, cancel) = start_daemon(&root).await;
    d.agents
        .delay_every_record_write_for_tests(Duration::from_secs(1));
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "durable").await;
    let ag = start_agent(&mut c, &ws.id, options()).await.unwrap();
    wait_for_messages(&mut c, &ag, 3).await;

    assert_eq!(
        records_of(&root)[0].session_id.as_deref(),
        Some("sess-1"),
        "the client has seen the turn, and the session id is still not on disk"
    );
    cancel.cancel();
}

/// What a daemon on its way out waits for: every record write still queued,
/// within a bound. A write slower than the bound is reported as not done.
#[tokio::test]
async fn flushing_the_records_waits_for_queued_writes_within_a_bound() {
    let _guard = ENV.lock().await;
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let root = dir.path().join("data");
    use_fake_claude(py, "simple_turn.ndjson");

    let (port, token, d, cancel) = start_daemon(&root).await;
    // Longer than the reader waits for the session id, so the write is still
    // queued when the turn has been seen.
    d.agents
        .delay_every_record_write_for_tests(Duration::from_secs(4));
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "flushed").await;
    let ag = start_agent(&mut c, &ws.id, options()).await.unwrap();
    wait_for_messages(&mut c, &ag, 3).await;

    assert!(
        !d.agents.flush_records(Duration::from_millis(100)).await,
        "a flush shorter than the write cannot report it done"
    );
    assert!(d.agents.flush_records(Duration::from_secs(10)).await);
    assert_eq!(records_of(&root)[0].session_id.as_deref(), Some("sess-1"));
    cancel.cancel();
}
