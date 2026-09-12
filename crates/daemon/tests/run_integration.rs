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

use bondsymphonic_daemon::sandbox::{
    backend_for, SandboxBackend, SandboxChild, SandboxCommand, SandboxHandle, SandboxSpec,
};
use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, start_daemon_with_backend, Client, PortGuard};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a run gets to reach `ready`.
///
/// A ceiling on a wait for the run's own `ready` event, not a sleep: every test
/// here returns the moment the event arrives, so raising the number costs
/// nothing on a healthy host and buys the one thing that matters — the suite
/// runs on a machine that is also building the workspace, and a cold Python
/// starting under that load took more than the 20 s this used to allow, which
/// is the flake Milestone 6 saw. Sixty seconds is the daemon's own git timeout,
/// and a run that has not bound a port in a minute is a failure worth reporting
/// as one.
const READY: Duration = Duration::from_secs(60);

/// The same ceiling for the other run events a test waits on: a line of output,
/// `stopped`, `failed`. Each of these waits also returns the moment its event
/// arrives, so the number is only ever paid by a run that is genuinely stuck,
/// and a machine that is slow enough to make a 20 s ceiling fail is a machine,
/// not a bug.
const SETTLED: Duration = READY;

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

/// How one command is chained onto the next in the shell the daemon uses for
/// this platform: `/bin/sh -c` on Unix, `cmd /C` on Windows.
fn chain() -> &'static str {
    if cfg!(windows) {
        "&"
    } else {
        ";"
    }
}

/// The spelling that prints one empty line. `cmd`'s bare `echo` prints the echo
/// state instead, and `echo.` with a space before the separator would print the
/// space, so the commands built with this are deliberately unspaced.
fn blank_line() -> &'static str {
    if cfg!(windows) {
        "echo."
    } else {
        "echo"
    }
}

/// A port nothing is listening on right now. Bound and released, so the number
/// is one the operating system just handed out rather than a guess.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// A command whose *direct child* does not go on the first signal, so a stop
/// really does spend its termination grace with the run still alive.
///
/// The direct child is the shell, and it is the shell that has to be deaf: a
/// Python process that ignores SIGTERM behind a shell that does not is no use,
/// because the shell dies, its exit is the run's exit, and the daemon is done
/// with the run in milliseconds while the Python goes on holding the port. On
/// Windows there is no signal to ignore -- the teardown kills the whole tree
/// outright -- so a plain sleep is all there is to ask for, and the window a
/// restart can land in is the teardown itself.
fn deaf_command(py: &str) -> String {
    if cfg!(windows) {
        format!("{py} hold.py")
    } else {
        "trap '' TERM; echo holding; sleep 30".to_string()
    }
}

/// A port nothing is listening on right now on IPv6 loopback, or `None` on a
/// host where `[::1]` cannot be bound at all.
fn free_port_v6() -> Option<u16> {
    let l = std::net::TcpListener::bind("[::1]:0").ok()?;
    Some(l.local_addr().ok()?.port())
}

/// The first terminal state in `events`, with its detail.
fn terminal(events: &[Event]) -> Option<(RunState, Option<String>)> {
    states(events)
        .into_iter()
        .find(|(s, _, _)| matches!(s, RunState::Stopped | RunState::Failed))
        .map(|(s, _, detail)| (s, detail))
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
    // Enough output that the daemon's reader cannot possibly have drained the
    // pipe by the time the exit code arrives, so the detail can only carry the
    // final marker if it is built after the drain rather than at the exit.
    std::fs::write(
        repo.join("sub").join("boom.py"),
        "import sys\nfor i in range(20000):\n    print('filler', i)\nprint('boom-marker')\nsys.stdout.flush()\nsys.exit(3)\n",
    )
    .unwrap();
    // A process that is "ready" without binding anything, so two runs can be
    // alive at once on one port.
    std::fs::write(
        repo.join("hold.py"),
        "import sys, time\nprint('holding', flush=True)\ntime.sleep(60)\n",
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
    start_run_on(c, ws, name, None).await
}

/// `run.start` with an explicit port for this start, or the configuration's own
/// when `port` is `None`.
async fn start_run_on(
    c: &mut Client,
    ws: &WorkspaceId,
    name: &str,
    port: Option<u16>,
) -> Result<RunStartResult, RpcError> {
    let v = c
        .call(Request::RunStart(RunStartParams {
            workspace_id: ws.clone(),
            config_name: name.into(),
            port,
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
    let mut guard = PortGuard::new(port);

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
    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
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
    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
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
    // The run is over, so the port is nobody's business from here on.
    guard.disarm();
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
            "[[run]]\nname = \"bad\"\ncommand = \"true\"\nport = {port}\nready_regex = \"(\"\n\n[[run]]\nname = \"escape\"\ncommand = \"true\"\nport = {port}\ncwd = \"/etc\"\n"
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

    // An absolute `cwd` would replace the worktree base rather than extend it,
    // so the repository's own file could point a run at any path on the host.
    let escape = start_run(&mut c, &ws.id, "escape").await.unwrap_err();
    assert_eq!(escape.code, ErrorCode::InvalidParams, "{escape:?}");
    assert!(
        escape.message.contains("relative"),
        "the message must say what is wrong with it: {escape:?}"
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

/// Two configurations that share a port are two runs that do not touch each
/// other.
///
/// Detection produces exactly this shape - `dev`, `start` and `serve` out of one
/// `package.json`, all with the same guessed port - so it is the ordinary case,
/// not a contrived one. The bridged half of the claim (one forwarder socket per
/// run) is asserted in `sandbox_integration.rs`, where there is a sandbox to
/// bridge into; here the point is that nothing in the manager is keyed on the
/// port.
#[tokio::test]
async fn two_configs_sharing_a_port_are_two_independent_runs() {
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
            "[[run]]\nname = \"dev\"\ncommand = \"{py} hold.py\"\nport = {port}\nready_regex = \"holding\"\n\n[[run]]\nname = \"serve\"\ncommand = \"{py} hold.py\"\nport = {port}\nready_regex = \"holding\"\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "sameport").await;

    let dev = start_run(&mut c, &ws.id, "dev").await.unwrap();
    let serve = start_run(&mut c, &ws.id, "serve").await.unwrap();
    assert_ne!(
        dev.run_id, serve.run_id,
        "the second start must be its own run, not a refusal or a reuse"
    );

    // Both runs in one pass. `run_events` keeps only the run it was asked
    // about and drops the rest, so waiting for these one after the other would
    // throw away whichever `ready` arrived while the other was being waited on.
    let wanted = [dev.run_id.clone(), serve.run_id.clone()];
    let mut ready: Vec<RunId> = Vec::new();
    let deadline = Instant::now() + READY;
    while ready.len() < wanted.len() && Instant::now() < deadline {
        for (_, ev) in c.drain_events() {
            if let Event::RunStateChanged {
                run_id,
                state: RunState::Ready,
                ..
            } = &ev
            {
                if wanted.contains(run_id) && !ready.contains(run_id) {
                    ready.push(run_id.clone());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        let _ = c.call(Request::WorkspaceList {}).await;
    }
    for run in &wanted {
        assert!(ready.contains(run), "{run} never became ready: {ready:?}");
    }
    let runs = list_runs(&mut c, &ws.id).await;
    assert_eq!(runs.len(), 2, "{runs:?}");

    // Stopping one leaves the other exactly as it was: a stop keyed on the port
    // would have taken both.
    c.call(Request::RunStop(RunIdParams {
        run_id: dev.run_id.clone(),
    }))
    .await
    .unwrap();
    let evs = run_events(&mut c, &dev.run_id, SETTLED, |e| {
        has_state(e, RunState::Stopped)
    })
    .await;
    assert!(has_state(&evs, RunState::Stopped), "{:?}", states(&evs));

    let runs = list_runs(&mut c, &ws.id).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].run_id, serve.run_id);
    assert_eq!(runs[0].state, RunState::Ready, "{runs:?}");

    c.call(Request::RunStop(RunIdParams {
        run_id: serve.run_id.clone(),
    }))
    .await
    .unwrap();
    cancel.cancel();
}

/// A command that exits before it is ready fails, and says why.
#[tokio::test]
async fn a_run_that_exits_before_it_is_ready_fails_with_its_exit_code() {
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
            "[[run]]\nname = \"boom\"\ncommand = \"{py} boom.py\"\nport = {port}\ncwd = \"sub\"\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "boom").await;

    let started = start_run(&mut c, &ws.id, "boom").await.unwrap();
    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
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
    let detail = failed.2.as_deref().unwrap_or_default();
    assert!(
        detail.contains("exit code 3"),
        "the detail must carry the exit code: {detail:?}"
    );
    // The detail is built after the readers drain, so the *last* lines the run
    // printed are in it — not whatever the reader happened to have consumed
    // when the exit code arrived.
    assert!(
        detail.contains("boom-marker"),
        "the detail must carry the run's last output: {detail:?}"
    );
    assert!(
        detail.lines().count() <= 21,
        "the detail is the exit code plus at most 20 lines: {} lines",
        detail.lines().count()
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
    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
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

/// A port given on the start replaces the one the configuration names: the
/// command sees it as `PORT`, and it is the port the caller is handed back.
///
/// This is what makes a *guessed* port usable: detection reads `3000` out of a
/// `package.json` it never ran, the user corrects it in the Run panel, and
/// nothing is written back to the repository.
#[tokio::test]
async fn run_start_honours_a_port_override() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let configured = free_port();
    let overridden = free_port();
    assert_ne!(configured, overridden);
    write_repo_config(
        &repo,
        &format!(
            "[[run]]
name = \"envcheck\"
command = \"{py} envprint.py\"
port = {configured}
cwd = \"sub\"
env = {{ BS_TEST = \"seven\" }}

[[run]]
name = \"held\"
command = \"{py} hold.py\"
port = {configured}
"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "override").await;

    let started = start_run_on(&mut c, &ws.id, "envcheck", Some(overridden))
        .await
        .unwrap();
    assert_eq!(
        started.host_port, overridden,
        "the caller is told the port it asked for"
    );
    assert_eq!(started.url, format!("http://localhost:{overridden}"));

    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
        !output(e).is_empty() && has_state(e, RunState::Failed)
    })
    .await;
    let lines = output(&evs);
    assert!(
        lines
            .iter()
            .any(|l| l.trim() == format!("{overridden} 0.0.0.0 seven")),
        "the command must see the overridden port as PORT: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(&configured.to_string())),
        "the configured port must not reach the command: {lines:?}"
    );

    // Zero is not a port a run can be bound to; it is the operating system's
    // "pick one", and a run whose port nobody knows cannot be reached.
    let e = start_run_on(&mut c, &ws.id, "envcheck", Some(0))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e:?}");

    // Still one run per configuration, and the override does not buy a second
    // one: the claim is keyed by the configuration's name, so a start on a
    // different port is the same run asking twice. `held` sleeps rather than
    // exiting, so the claim is genuinely held while the second start is made.
    let held = start_run_on(&mut c, &ws.id, "held", Some(free_port()))
        .await
        .unwrap();
    let e = start_run_on(&mut c, &ws.id, "held", Some(free_port()))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict, "{e:?}");
    c.call(Request::RunStop(RunIdParams {
        run_id: held.run_id.clone(),
    }))
    .await
    .unwrap();

    cancel.cancel();
}

/// A dev server frames its banner with blank lines. One of those must not be
/// read as end of stream: it would take the rest of the run's output with it,
/// and with a `ready_regex` set the run could then never become ready at all.
#[tokio::test]
async fn a_blank_output_line_does_not_end_the_stream_or_strand_a_ready_regex() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    let (sep, blank) = (chain(), blank_line());
    // A blank line, then the marker, then the server: the marker is only ever
    // seen by a reader that survived the blank line before it.
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"banner\"\ncommand = \"{blank}{sep}echo READY{sep}{py} -m http.server {port} --bind 127.0.0.1\"\nport = {port}\nready_regex = \"READY\"\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "banner").await;
    let mut guard = PortGuard::new(port);

    let started = start_run(&mut c, &ws.id, "banner").await.unwrap();
    let evs = run_events(&mut c, &started.run_id, READY, |e| {
        has_state(e, RunState::Ready)
    })
    .await;
    assert!(
        has_state(&evs, RunState::Ready),
        "the regex after the blank line must still make the run ready: {:?}",
        states(&evs)
    );
    let lines = output(&evs);
    assert!(
        lines.iter().any(String::is_empty),
        "the blank line itself must reach the client: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.trim() == "READY"),
        "the line after the blank one must reach the client: {lines:?}"
    );

    c.call(Request::RunStop(RunIdParams {
        run_id: started.run_id.clone(),
    }))
    .await
    .unwrap();
    // Waited for rather than assumed: the guard must stop watching the port at
    // the run's terminal event, and there is no such event until it arrives.
    let evs = run_events(&mut c, &started.run_id, SETTLED, |e| {
        has_state(e, RunState::Stopped)
    })
    .await;
    assert!(has_state(&evs, RunState::Stopped), "{:?}", states(&evs));
    guard.disarm();
    cancel.cancel();
}

/// `run.stop` is idempotent for a run this daemon started, and honest about an
/// id it never minted.
#[tokio::test]
async fn run_stop_refuses_an_id_that_never_existed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    write_repo_config(&repo, "");
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let _ws = create_ws(&mut c, &repo, "nosuchrun").await;

    let err = c
        .call(Request::RunStop(RunIdParams {
            run_id: "run_deadbeef".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");

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
    let mut guard = PortGuard::new(port);

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
    guard.disarm();
    assert!(
        http_get(port).await.is_none(),
        "the web app must be gone with the workspace"
    );

    cancel.cancel();
}

/// Stopping a run that has not become ready yet is a stop, not a failure.
///
/// The supervisor is sitting on the run's exit while `stop` kills it, so both
/// paths see the same death and race to announce it. The supervisor used to win
/// that race about as often as it lost it and call a run the user had just
/// stopped `failed`, with an exit code and the last of its output as the
/// explanation for something that needed none.
#[tokio::test]
async fn stopping_a_run_that_is_still_starting_reports_it_as_stopped() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    // `hold.py` binds nothing, so the run never leaves `starting` on its own and
    // every stop below lands in exactly the window this is about.
    write_repo_config(
        &repo,
        &format!("[[run]]\nname = \"held\"\ncommand = \"{py} hold.py\"\nport = {port}\n"),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "stopwhilestarting").await;

    for attempt in 1..=20 {
        let started = start_run(&mut c, &ws.id, "held").await.unwrap();
        c.call(Request::RunStop(RunIdParams {
            run_id: started.run_id.clone(),
        }))
        .await
        .unwrap();
        let evs = run_events(&mut c, &started.run_id, SETTLED, |e| terminal(e).is_some()).await;
        let (state, detail) = terminal(&evs)
            .unwrap_or_else(|| panic!("attempt {attempt}: no terminal state: {:?}", states(&evs)));
        assert_eq!(
            state,
            RunState::Stopped,
            "attempt {attempt}: a run the user stopped is stopped, not failed: {:?}",
            states(&evs)
        );
        assert_eq!(
            detail, None,
            "attempt {attempt}: a stop needs no explanation"
        );
        assert!(
            list_runs(&mut c, &ws.id).await.is_empty(),
            "attempt {attempt}: the stopped run must be out of the list"
        );
    }

    cancel.cancel();
}

/// A start that arrives while the previous run of the same configuration is
/// still being killed is refused, rather than handed the port the old process
/// has not let go of yet.
///
/// `stop` takes the run out of the list before it signals anything, so for the
/// whole termination grace the manager looked as though nothing was running.
#[tokio::test]
async fn a_restart_during_the_stop_grace_is_refused() {
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
            "[[run]]\nname = \"deaf\"\ncommand = \"{}\"\nport = {port}\n",
            deaf_command(py)
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut a = Client::connect(p, &token).await;
    let mut b = Client::connect(p, &token).await;
    let ws = create_ws(&mut a, &repo, "restartgrace").await;

    let started = start_run(&mut a, &ws.id, "deaf").await.unwrap();
    // The command goes deaf before it prints, so its line is the proof that a
    // stop arriving from here finds a process that will not simply go.
    let up = run_events(&mut a, &started.run_id, SETTLED, |e| {
        output(e).iter().any(|l| l.contains("holding"))
    })
    .await;
    assert!(
        output(&up).iter().any(|l| l.contains("holding")),
        "the run never got going: {:?}",
        states(&up)
    );
    // The stop is left in flight on its own connection: the second client asks
    // for the same configuration while it is still running.
    let stop = a
        .send(Request::RunStop(RunIdParams {
            run_id: started.run_id.clone(),
        }))
        .await;
    // Taking the run out of the list is the first thing `stop` does, so once it
    // is gone from here the stop is inside its termination grace and the racing
    // start below is the one this test is about.
    let deadline = Instant::now() + SETTLED;
    while !list_runs(&mut b, &ws.id).await.is_empty() {
        assert!(Instant::now() < deadline, "the stop never began");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let again = start_run(&mut b, &ws.id, "deaf").await.unwrap_err();
    assert_eq!(
        again.code,
        ErrorCode::Conflict,
        "a restart during the grace must be refused: {again:?}"
    );
    assert_eq!(
        again
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str()),
        Some("run_stopping"),
        "and say which of the two conflicts it is: {again:?}"
    );

    let mut ignored = Vec::new();
    a.recv_response(stop, &mut ignored).await.unwrap();
    // With the stop finished the configuration is startable again.
    let third = start_run(&mut b, &ws.id, "deaf").await.unwrap();
    b.call(Request::RunStop(RunIdParams {
        run_id: third.run_id,
    }))
    .await
    .unwrap();

    cancel.cancel();
}

/// A service that listens on IPv6 loopback alone is ready when it answers
/// there. Probing only `127.0.0.1` left such a run in `starting` for ever, and
/// `localhost` resolves to `::1` first on a modern host, so the URL the daemon
/// handed back worked in the browser while the daemon said the run was not up.
#[tokio::test]
async fn a_service_on_ipv6_loopback_becomes_ready() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let Some(port) = free_port_v6() else {
        eprintln!("SKIP: no IPv6 loopback on this host");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    write_repo_config(
        &repo,
        &format!(
            "[[run]]\nname = \"v6\"\ncommand = \"{py} -m http.server {port} --bind ::1\"\nport = {port}\n"
        ),
    );
    let (p, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "ipv6").await;

    let started = start_run(&mut c, &ws.id, "v6").await.unwrap();
    let evs = run_events(&mut c, &started.run_id, READY, |e| {
        has_state(e, RunState::Ready)
    })
    .await;
    let became_ready = has_state(&evs, RunState::Ready);
    let seen = states(&evs);

    // Stopped before a single assertion, and no `PortGuard` here: the guard
    // reads `netstat -p tcp`, which on Windows lists IPv4 alone, so an
    // IPv6-only server is invisible to it. An assertion that fired first would
    // leave that server running, its pipes open, and the test runtime waiting
    // on the reader task holding them for ever -- a hung suite instead of a
    // failed test. The daemon's own stop is what ends this run, on every host.
    c.call(Request::RunStop(RunIdParams {
        run_id: started.run_id.clone(),
    }))
    .await
    .unwrap();
    let after = run_events(&mut c, &started.run_id, SETTLED, |e| {
        has_state(e, RunState::Stopped)
    })
    .await;
    cancel.cancel();

    assert!(
        became_ready,
        "a run reachable only on ::1 must still become ready: {seen:?}"
    );
    assert!(has_state(&after, RunState::Stopped), "{:?}", states(&after));
}

/// A `run.start` that is already past the workspace's readiness check when a
/// `workspace.destroy` sweeps that workspace's runs must not leave one behind.
///
/// The destroy stops every run it can see and then takes the sandbox and the
/// worktree away. A start that registers its run a moment later was never seen,
/// so its process outlived the workspace, went on holding the port, and
/// announced state changes for a workspace the client had already been told was
/// gone.
#[tokio::test]
async fn a_start_that_races_a_destroy_leaves_no_run_behind() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (p, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut a = Client::connect(p, &token).await;
    let mut b = Client::connect(p, &token).await;

    // The window is however long the start spends between its readiness check
    // and registering the run, which is a process spawn; the delays sweep the
    // destroy across it.
    for attempt in 0..8u64 {
        let home = dir.path().join(format!("race{attempt}"));
        std::fs::create_dir_all(&home).unwrap();
        let repo = init_repo(&home);
        let port = free_port();
        write_repo_config(
            &repo,
            &format!(
                "[[run]]\nname = \"web\"\ncommand = \"{py} -m http.server {port} --bind 127.0.0.1\"\nport = {port}\n"
            ),
        );
        let ws = create_ws(&mut a, &repo, &format!("race{attempt}")).await;
        let mut guard = PortGuard::new(port);

        let start = a
            .send(Request::RunStart(RunStartParams {
                workspace_id: ws.id.clone(),
                config_name: "web".into(),
                port: None,
            }))
            .await;
        tokio::time::sleep(Duration::from_millis(attempt * 5)).await;
        b.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force: true,
        }))
        .await
        .unwrap();
        let mut events = Vec::new();
        let started = a
            .recv_response(start, &mut events)
            .await
            .map(|v| serde_json::from_value::<RunStartResult>(v).unwrap());

        // Whatever the start answered, the workspace is gone and nothing of it
        // may still be running.
        assert!(
            d.runs.runs_of(&ws.id).is_empty(),
            "attempt {attempt}: a run outlived its workspace: {started:?}"
        );
        // A moment for anything the losing path would still have published.
        let quiet = Instant::now() + Duration::from_millis(750);
        let mut after: Vec<Event> = events.into_iter().map(|(_, e)| e).collect();
        while Instant::now() < quiet {
            let _ = a.call(Request::WorkspaceList {}).await;
            after.extend(a.drain_events().into_iter().map(|(_, e)| e));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Ok(started) = &started {
            let failed: Vec<&Event> = after
                .iter()
                .filter(|e| run_of(e) == Some(&started.run_id))
                .filter(|e| {
                    matches!(
                        e,
                        Event::RunStateChanged {
                            state: RunState::Failed,
                            ..
                        }
                    )
                })
                .collect();
            assert!(
                failed.is_empty(),
                "attempt {attempt}: a workspace that is gone must not report a failed run: {failed:?}"
            );
        }
        guard.disarm();
        assert!(
            http_get(port).await.is_none(),
            "attempt {attempt}: the web app must be gone with its workspace ({started:?})"
        );
    }

    cancel.cancel();
}

// ---------------------------------------------------------------------------
// Holding the start's window open.
// ---------------------------------------------------------------------------

/// One spawn's worth of hold on the sandbox: armed by a test, entered by the
/// daemon, released by the test.
///
/// `run.start` reads the workspace state, spawns the process, registers the run
/// and reads the state again. The window the test below is about lies between
/// those two reads, and the daemon crosses it in the couple of milliseconds a
/// config read and a spawn take. Sleeping for a guessed number of microseconds
/// and hoping to land inside it is how that test used to be written; on a CI
/// runner busy with the rest of the suite the whole window sat past the end of
/// a 40 ms sweep, every attempt was refused for being too early, and the test
/// failed having proved nothing. Holding the spawn makes the window last as
/// long as the test needs it to and costs nothing on any host.
#[derive(Default)]
struct SpawnGate {
    /// Whether the *next* spawn waits. One spawn only: a workspace spawns other
    /// things, and only the one this test starts may be held.
    armed: std::sync::atomic::AtomicBool,
    /// The daemon has reached the held spawn, so the start is past its own
    /// readiness check and has not registered its run.
    entered: tokio::sync::Notify,
    /// The test is done with the window.
    release: tokio::sync::Notify,
}

impl SpawnGate {
    fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Waits for the daemon to reach the held spawn. Bounded, so a start that
    /// never gets there fails the test instead of hanging it.
    async fn entered(&self) {
        tokio::time::timeout(SETTLED, self.entered.notified())
            .await
            .expect("the start never reached the spawn the gate holds");
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

/// The backend that answers a [`SpawnGate`]: the no-sandbox backend in every
/// respect but the one spawn it holds.
struct GatedBackend {
    inner: Arc<dyn SandboxBackend>,
    gate: Arc<SpawnGate>,
}

impl GatedBackend {
    fn over_noop(gate: Arc<SpawnGate>) -> Arc<dyn SandboxBackend> {
        Arc::new(Self {
            inner: backend_for("noop"),
            gate,
        })
    }
}

#[async_trait::async_trait]
impl SandboxBackend for GatedBackend {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn check(&self) -> Vec<PrereqStatus> {
        self.inner.check().await
    }

    async fn start(&self, spec: &SandboxSpec) -> Result<Arc<dyn SandboxHandle>, RpcError> {
        Ok(Arc::new(GatedHandle {
            inner: self.inner.start(spec).await?,
            gate: self.gate.clone(),
        }))
    }
}

struct GatedHandle {
    inner: Arc<dyn SandboxHandle>,
    gate: Arc<SpawnGate>,
}

#[async_trait::async_trait]
impl SandboxHandle for GatedHandle {
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        // Disarmed by the same read that takes the hold, so the second spawn
        // through this handle -- and every one after it -- goes straight to the
        // backend underneath.
        if self
            .gate
            .armed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.spawn(cmd).await
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        self.inner.shutdown().await
    }

    fn helper_exe(&self) -> std::path::PathBuf {
        self.inner.helper_exe()
    }

    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.inner.died()
    }
}

/// A `run.start` that is already past the workspace's readiness check when that
/// workspace is marked for destruction is refused, and leaves nothing behind.
///
/// The end-to-end race above is the bug as a user meets it; this is the same
/// window held open on purpose. `workspace.destroy` marks the workspace
/// `Destroying` and *then* sweeps its runs, so a start that registers its run
/// after the sweep was never seen by it: the process outlived the workspace,
/// went on holding the port, and announced state changes for a workspace the
/// client had already been told was gone. Marking the state here is that first
/// half of a destroy on its own, at a moment the start cannot have seen it.
///
/// The window is held by the sandbox rather than aimed at with a sleep: see
/// [`SpawnGate`]. The daemon is stopped inside the spawn, which is past the
/// start's own readiness check and before the run is registered, so the mark
/// lands where this test needs it every time and on every host.
#[tokio::test]
async fn a_start_whose_workspace_is_marked_for_destruction_is_refused() {
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
            "[[run]]
name = \"web\"
command = \"{py} -m http.server {port} --bind 127.0.0.1\"
port = {port}
"
        ),
    );
    let gate = Arc::new(SpawnGate::default());
    let (p, token, d, cancel) = start_daemon_with_backend(
        &dir.path().join("data"),
        GatedBackend::over_noop(gate.clone()),
    )
    .await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "marked").await;
    let mut guard = PortGuard::new(port);

    // Armed after the workspace exists, so the spawn this holds is the run's
    // and not something the create needed.
    gate.arm();
    let start = c
        .send(Request::RunStart(RunStartParams {
            workspace_id: ws.id.clone(),
            config_name: "web".into(),
            port: None,
        }))
        .await;
    gate.entered().await;
    // Inside the window: the start read `Ready` at the top of the call and has
    // not registered its run.
    d.set_state(&ws.id, WorkspaceState::Destroying)
        .await
        .unwrap();
    gate.release();

    let mut events = Vec::new();
    let answer = c.recv_response(start, &mut events).await;
    let refusal = match answer {
        Err(e) => e,
        Ok(v) => {
            // The failure this test exists to catch, and it has to be reported
            // rather than panicked straight out of: the run it was told about
            // is alive, and a live run left behind by a panicking test keeps
            // the runtime waiting on its pipes instead of letting the test
            // fail. Stopped first, then failed.
            let started: RunStartResult = serde_json::from_value(v).unwrap();
            let _ = c
                .call(Request::RunStop(RunIdParams {
                    run_id: started.run_id.clone(),
                }))
                .await;
            let _ = run_events(&mut c, &started.run_id, SETTLED, |e| {
                has_state(e, RunState::Stopped)
            })
            .await;
            guard.disarm();
            cancel.cancel();
            panic!("a start marked mid-spawn was allowed to run: {started:?}");
        }
    };
    let reason = refusal
        .data
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(|r| r.as_str());
    assert_eq!(reason, Some("workspace_not_ready"), "{refusal:?}");
    // Which of the two checks refused it, said in the one place the two differ.
    // The first cannot have: the workspace was `Ready` until the spawn was
    // already running.
    assert!(
        refusal.message.contains("no longer ready"),
        "the refusal must come from the check after the insert: {refusal:?}"
    );
    assert!(
        d.runs.runs_of(&ws.id).is_empty(),
        "a run outlived the workspace it belonged to"
    );

    // Nothing of that run may be left -- and since the client was told the
    // start did not happen, not one word may be said about a run whose id it
    // was never given.
    let mut seen: Vec<Event> = events.into_iter().map(|(_, e)| e).collect();
    let quiet = Instant::now() + Duration::from_millis(500);
    while Instant::now() < quiet {
        let _ = c.call(Request::WorkspaceList {}).await;
        seen.extend(c.drain_events().into_iter().map(|(_, e)| e));
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let announced: Vec<&Event> = seen
        .iter()
        .filter(|e| matches!(e, Event::RunStateChanged { .. }))
        .collect();
    assert!(
        announced.is_empty(),
        "a start that was refused announced a run: {announced:?}"
    );
    // Its output counts as a word said about it. The readers are attached
    // before the run is registered -- they have to be, or the first lines of a
    // run that starts normally are lost -- so on this path they were publishing
    // `run.output` for a run id the client was never given and could not have
    // subscribed to, unsubscribed from, or stopped.
    let said = output(&seen);
    assert!(
        said.is_empty(),
        "a start that was refused streamed a run's output: {said:?}"
    );
    assert!(
        http_get(port).await.is_none(),
        "the web app must not have been left running"
    );

    guard.disarm();
    cancel.cancel();
}

/// A `run.start` refused because the workspace is not ready says so in
/// `data.reason`, whichever of the two checks refused it.
///
/// There are two. The one at the top of the call reads the workspace the client
/// named; the one after the run is registered catches a destroy that began
/// while the start was spawning. The second has always carried a reason a
/// client can match on and the first carried a sentence and nothing else, so an
/// IDE wanting to tell "that workspace is going away" from "no such run
/// configuration" had to read English to do it.
#[tokio::test]
async fn a_start_refused_because_the_workspace_is_not_ready_says_which_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let port = free_port();
    write_repo_config(
        &repo,
        &format!("[[run]]\nname = \"web\"\ncommand = \"true\"\nport = {port}\n"),
    );
    let (p, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(p, &token).await;
    let ws = create_ws(&mut c, &repo, "notready").await;

    // The first half of a destroy, on its own: the workspace is still in the
    // registry and is no longer somewhere a run may be started.
    d.set_state(&ws.id, WorkspaceState::Destroying)
        .await
        .unwrap();

    let refused = start_run(&mut c, &ws.id, "web").await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidParams, "{refused:?}");
    assert_eq!(
        refused
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str()),
        Some("workspace_not_ready"),
        "the refusal must be one a client can act on without reading it: {refused:?}"
    );
    assert!(d.runs.runs_of(&ws.id).is_empty());

    cancel.cancel();
}
