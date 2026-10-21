//! `system.setup_pty` refusing an action before it opens anything.
//!
//! Its own test binary: it points `BS_CODEX_BIN` at the fake Codex, and an
//! environment variable is process-wide.
mod common;
use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};

/// An uninstall is refused while an agent of that backend is running, with
/// the reason a client can act on, and nothing is opened -- the check sits
/// ahead of the PTY and of every `before_setup` side effect.
#[tokio::test(flavor = "multi_thread")]
async fn an_uninstall_is_refused_while_an_agent_of_that_backend_runs() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let fake = fixtures
        .join("fake_codex.py")
        .display()
        .to_string()
        .replace('\\', "/");
    std::env::set_var("BS_CODEX_BIN", format!("{python} \"{fake}\" normal"));

    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "uninstall-refusal").await;
    let options: AgentStartOptions =
        serde_json::from_value(serde_json::json!({"api_key": "refusal-key"})).unwrap();
    let started = d
        .agents
        .start(
            &d,
            AgentStartParams {
                workspace_id: ws.id.clone(),
                adapter: AgentAdapterKind::Codex,
                options,
            },
        )
        .await
        .unwrap();
    assert_eq!(d.agents.running(AgentAdapterKind::Codex), 1);

    let err = c
        .call(Request::SystemSetupPty(SetupPtyParams {
            action: SetupAction::UninstallCodex,
            cols: 80,
            rows: 24,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert_eq!(err.data.unwrap()["reason"], "agents_running");

    d.agents
        .stop(AgentIdParams {
            agent_id: started.agent_id,
        })
        .await
        .unwrap();
    std::env::remove_var("BS_CODEX_BIN");
    cancel.cancel();
}
