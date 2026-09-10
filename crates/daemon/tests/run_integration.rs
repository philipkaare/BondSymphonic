//! `run.*` end to end over the no-sandbox backend, which is what Windows and
//! the WSL noop suite both use: a real web app started from a repo's
//! `bondsymphonic.toml`, its readiness, its output, and every way it can end.
//!
//! There is no bridge on this backend — processes are plain children of the
//! daemon and already share its loopback — so `host_port` is the configured
//! port and readiness is a direct TCP connect. The bridged story lives in
//! `sandbox_integration.rs`, where bubblewrap gives the workspace a network
//! namespace of its own.
//!
//! The web app is `python -m http.server` bound to loopback. Nothing here
//! reaches the network.

mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a `python -m http.server` gets to bind its port. The daemon probes
/// every 500 ms, so this is generous even on a cold interpreter.
const READY: Duration = Duration::from_secs(20);

/// The interpreter to run the test web app with, or `None` on a host with
/// none. Windows ships a `python3` App Execution Alias that is not an
/// interpreter, so the real name is tried first there.
fn python() -> Option<&'static str> {
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

/// A port nothing is listening on right now. Bound and released, so the number
/// is one the operating system just handed out rather than a guess.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Writes `bondsymphonic.toml` and the helper script into `repo` and commits
/// them, so the workspace's worktree carries both.
fn write_repo_config(repo: &Path, toml: &str) {
    std::fs::write(repo.join("bondsymphonic.toml"), toml).unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(
        repo.join("sub").join("envprint.py"),
        "import os\nprint(os.environ['PORT'], os.environ['HOST'], os.environ['BS_TEST'])\n",
    )
    .unwrap();
    common::commit_all(repo, &[], "config");
}

fn run_of(ev: &Event) -> Option<&RunId> {
    match ev {
        Event::RunOutput { run_id, .. } | Event::RunStateChanged { run_id, .. } => Some(run_id),
        _ => None,
    }
}

/// Collects `run.*` events for `run` until `done` is satisfied or `limit`
/// elapses. `Client` only surfaces events it read while waiting for a response,
/// so a cheap request is what delivers them.
async fn run_events(
    c: &mut Client,
    run: &RunId,
    limit: Duration,
    mut done: impl FnMut(&[Event]) -> bool,
) -> Vec<Event> {
    let start = Instant::now();
    let mut out = Vec::new();
    loop {
        for (_, ev) in c.drain_events() {
            if run_of(&ev) == Some(run) {
                out.push(ev);
            }
        }
        if done(&out) || start.elapsed() >= limit {
            return out;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        let _ = c.call(Request::WorkspaceList {}).await;
    }
}

fn states(events: &[Event]) -> Vec<(RunState, Option<String>, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::RunStateChanged {
                state, url, detail, ..
            } => Some((*state, url.clone(), detail.clone())),
            _ => None,
        })
        .collect()
}

fn output(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::RunOutput { line, .. } => Some(line.clone()),
            _ => None,
        })
        .collect()
}

fn has_state(events: &[Event], want: RunState) -> bool {
    states(events).iter().any(|(s, _, _)| *s == want)
}

/// One `GET /` over a fresh connection. `None` when the port refuses it.
async fn http_get(port: u16) -> Option<String> {
    let mut s = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .ok()?;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

async fn start_run(
    c: &mut Client,
    ws: &WorkspaceId,
    name: &str,
) -> Result<RunStartResult, RpcError> {
    let v = c
        .call(Request::RunStart(RunStartParams {
            workspace_id: ws.clone(),
            config_name: name.into(),
        }))
        .await?;
    Ok(serde_json::from_value(v).unwrap())
}

async fn list_runs(c: &mut Client, ws: &WorkspaceId) -> Vec<RunInfo> {
    let v = c
        .call(Request::RunList(WorkspaceIdParams {
            workspace_id: ws.clone(),
        }))
        .await
        .unwrap();
    serde_json::from_value::<RunListResult>(v).unwrap().runs
}

/// The whole life of a web app run: start, ready, output, list, the second
/// start refused, stop, and the port gone with it.
#[tokio::test]
async fn a_web_run_becomes_ready_streams_its_output_and_stops() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"web\"\ncommand = \"{py} -m http.server {port} --bind 127.0.0.1\"\nport = {port}\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "runs").await;

    let started = start_run(&mut c, &ws.id, "web").await.unwrap();
    assert_eq!(started.host_port, port, "the noop backend does not bridge");
    assert_eq!(started.url, format!("http://localhost:{port}"));

    let evs = run_events(&mut c, &started.run_id, READY, |e| {
        has_state(e, RunState::Ready)
    })
    .await;
    let seen = states(&evs);
    assert_eq!(
        seen.first().map(|(s, _, _)| *s),
        Some(RunState::Starting),
        "{seen:?}"
    );
    let ready = seen
        .iter()
        .find(|(s, _, _)| *s == RunState::Ready)
        .unwrap_or_else(|| panic!("the run never became ready: {evs:?}"));
    assert_eq!(ready.1.as_deref(), Some(started.url.as_str()), "{seen:?}");

    // `run.list` agrees with the events.
    let runs = list_runs(&mut c, &ws.id).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].run_id, started.run_id);
    assert_eq!(runs[0].config_name, "web");
    assert_eq!(runs[0].state, RunState::Ready);
    assert_eq!(runs[0].host_port, port);
    // And so does the workspace.
    let info: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceGet(WorkspaceIdParams {
            workspace_id: ws.id.clone(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(info.runs, vec![started.run_id.clone()]);

    // The app answers, and the request it served reaches the client as output.
    let body = http_get(port)
        .await
        .unwrap_or_else(|| panic!("no answer on {port}"));
    assert!(body.contains("200"), "{body}");
    let evs = run_events(&mut c, &started.run_id, Duration::from_secs(10), |e| {
        output(e).iter().any(|l| l.contains("GET /"))
    })
    .await;
    assert!(
        output(&evs).iter().any(|l| l.contains("GET /")),
        "the request must appear in the run's output: {:?}",
        output(&evs)
    );

    // One run per config per workspace.
    let again = start_run(&mut c, &ws.id, "web").await.unwrap_err();
    assert_eq!(again.code, ErrorCode::Conflict, "{again:?}");

    // Stop: the terminal state, the port gone, and the run out of the list.
    c.call(Request::RunStop(RunIdParams {
        run_id: started.run_id.clone(),
    }))
    .await
    .unwrap();
    let evs = run_events(&mut c, &started.run_id, Duration::from_secs(10), |e| {
        has_state(e, RunState::Stopped)
    })
    .await;
    assert!(
        has_state(&evs, RunState::Stopped),
        "the run must announce that it stopped: {:?}",
        states(&evs)
    );
    assert!(
        !has_state(&evs, RunState::Failed),
        "a stopped run is not a failed one: {:?}",
        states(&evs)
    );
    assert!(
        http_get(port).await.is_none(),
        "the web app must be gone once the run stopped"
    );
    assert!(list_runs(&mut c, &ws.id).await.is_empty());

    // Stopping it again changes nothing and is not an error.
    c.call(Request::RunStop(RunIdParams {
        run_id: started.run_id.clone(),
    }))
    .await
    .unwrap();

    cancel.cancel();
}

/// The three ways `run.start` refuses.
#[tokio::test]
async fn run_start_refuses_an_unknown_config_a_disabled_one_and_a_bad_regex() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"bad\"\ncommand = \"true\"\nport = {port}\nready_regex = \"(\"\n"
        ),
    );
    // Detection would otherwise never see a compose file next to a toml that
    // declares runs, so the disabled case is checked on its own repo below.
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "refusals").await;

    let missing = start_run(&mut c, &ws.id, "nope").await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound, "{missing:?}");
    assert!(missing.message.contains("nope"), "{missing:?}");

    let bad = start_run(&mut c, &ws.id, "bad").await.unwrap_err();
    assert_eq!(bad.code, ErrorCode::InvalidParams, "{bad:?}");
    assert!(
        bad.message.contains("ready_regex"),
        "the message must name the regex: {bad:?}"
    );

    // A repo whose only config is a disabled one.
    let dir2 = tempfile::tempdir().unwrap();
    let repo2 = init_repo(dir2.path());
    std::fs::write(repo2.join("docker-compose.yml"), "services: {}\n").unwrap();
    common::commit_all(&repo2, &[], "compose");
    let ws2 = create_ws(&mut c, &repo2, "compose").await;
    let disabled = start_run(&mut c, &ws2.id, "compose").await.unwrap_err();
    assert_eq!(disabled.code, ErrorCode::InvalidParams, "{disabled:?}");
    assert!(disabled.message.contains("Docker"), "{disabled:?}");

    cancel.cancel();
}

/// A command that exits before it is ready fails, and says why.
#[tokio::test]
async fn a_run_that_exits_before_it_is_ready_fails_with_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!("[[run]]\nname = \"boom\"\ncommand = \"exit 3\"\nport = {port}\n"),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "boom").await;

    let started = start_run(&mut c, &ws.id, "boom").await.unwrap();
    let evs = run_events(&mut c, &started.run_id, Duration::from_secs(20), |e| {
        has_state(e, RunState::Failed)
    })
    .await;
    let seen = states(&evs);
    assert_eq!(
        seen.first().map(|(s, _, _)| *s),
        Some(RunState::Starting),
        "{seen:?}"
    );
    let failed = seen
        .iter()
        .find(|(s, _, _)| *s == RunState::Failed)
        .unwrap_or_else(|| panic!("the run must fail: {seen:?}"));
    assert!(
        failed.2.as_deref().unwrap_or_default().contains('3'),
        "the detail must carry the exit code: {failed:?}"
    );
    assert!(
        !has_state(&evs, RunState::Ready),
        "a command that exits at once was never ready: {seen:?}"
    );
    // A run that ended on its own is no longer listed.
    assert!(list_runs(&mut c, &ws.id).await.is_empty());

    cancel.cancel();
}

/// The command's environment is the config's, plus `PORT` and `HOST`, and it
/// runs in the config's `cwd` under the worktree.
#[tokio::test]
async fn a_run_gets_port_host_and_its_configured_environment_in_its_cwd() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"envcheck\"\ncommand = \"{py} envprint.py\"\nport = {port}\ncwd = \"sub\"\nenv = {{ BS_TEST = \"seven\" }}\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "envcheck").await;

    let started = start_run(&mut c, &ws.id, "envcheck").await.unwrap();
    let evs = run_events(&mut c, &started.run_id, Duration::from_secs(20), |e| {
        !output(e).is_empty() && has_state(e, RunState::Failed)
    })
    .await;
    let lines = output(&evs);
    assert!(
        lines
            .iter()
            .any(|l| l.trim() == format!("{port} 0.0.0.0 seven")),
        "the script must see PORT, HOST and the config's env: {lines:?}"
    );

    cancel.cancel();
}

/// `workspace.destroy` ends the run before it answers, so a client never sees a
/// workspace disappear with a run still claiming to be ready.
#[tokio::test]
async fn destroy_stops_a_ready_run_before_it_replies() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"web\"\ncommand = \"{py} -m http.server {port} --bind 127.0.0.1\"\nport = {port}\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "destroyed").await;

    let started = start_run(&mut c, &ws.id, "web").await.unwrap();
    run_events(&mut c, &started.run_id, READY, |e| {
        has_state(e, RunState::Ready)
    })
    .await;
    assert!(http_get(port).await.is_some(), "the app must be up first");

    let id = c
        .send(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: true,
        }))
        .await;
    let mut during = Vec::new();
    c.recv_response(id, &mut during).await.unwrap();
    let stopped = during
        .iter()
        .filter_map(|(_, e)| match e {
            Event::RunStateChanged { run_id, state, .. } if run_id == &started.run_id => {
                Some(*state)
            }
            _ => None,
        })
        .any(|s| s == RunState::Stopped);
    assert!(
        stopped,
        "destroy must stop the run before it answers: {:?}",
        during.iter().map(|(_, e)| e).collect::<Vec<_>>()
    );
    assert!(
        http_get(port).await.is_none(),
        "the web app must be gone with the workspace"
    );

    cancel.cancel();
}
