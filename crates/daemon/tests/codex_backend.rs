mod common;
use bondsymphonic_daemon::agents::codex_backend::{has_login, process_config};

#[test]
fn api_key_is_only_in_the_process_environment_and_overrides_shared_login() {
    let dir = tempfile::tempdir().unwrap();
    let (args, env) = process_config(dir.path(), Some("key-sentinel"));
    assert!(!args.join(" ").contains("key-sentinel"));
    assert!(args
        .iter()
        .any(|a| a.contains("env_key") && a.contains("OPENAI_API_KEY")));
    assert!(args
        .iter()
        .any(|a| a.contains("requires_openai_auth=false")));
    assert!(env
        .iter()
        .any(|(k, v)| k == "OPENAI_API_KEY" && v == "key-sentinel"));
    assert!(env.iter().any(
        |(k, v)| k == "CODEX_HOME" && v == &dir.path().join("codex-home").display().to_string()
    ));
    let (args, env) = process_config(dir.path(), None);
    assert!(!args.iter().any(|a| a.contains("bondsymphonic_api")));
    assert!(!env
        .iter()
        .any(|(k, _)| k == "ANTHROPIC_API_KEY" || k == "OPENAI_API_KEY"));
}

#[test]
fn missing_malformed_and_empty_auth_are_not_a_login() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!has_login(dir.path()));
    for invalid in ["not json", "{}", r#"{"tokens":{"access_token":""}}"#] {
        std::fs::write(dir.path().join("auth.json"), invalid).unwrap();
        assert!(!has_login(dir.path()));
    }
    std::fs::write(
        dir.path().join("auth.json"),
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"sentinel","refresh_token":"refresh"}}"#,
    )
    .unwrap();
    assert!(has_login(dir.path()));
}

#[test]
fn setup_login_and_logout_use_the_same_shared_home() {
    use bondsymphonic_daemon::setup::setup_env;
    use bondsymphonic_proto::SetupAction;
    let root = std::path::Path::new("/test/data");
    let login = setup_env(root, SetupAction::CodexLogin);
    assert_eq!(login, setup_env(root, SetupAction::CodexLogout));
    assert_eq!(
        login,
        vec![(
            "CODEX_HOME".into(),
            root.join("codex-home").display().to_string()
        )]
    );
    assert!(setup_env(root, SetupAction::ClaudeLogin).is_empty());
}

#[tokio::test]
async fn model_discovery_paginates_and_does_not_reuse_another_accounts_models() {
    use bondsymphonic_daemon::{
        agents::codex_backend::list_models_process,
        sandbox::{noop::NoopBackend, SandboxBackend, SandboxCommand, SandboxSpec},
    };
    use bondsymphonic_proto::WorkspaceId;
    let dir = tempfile::tempdir().unwrap();
    let handle = NoopBackend
        .start(&SandboxSpec {
            id: WorkspaceId("models".into()),
            rw_binds: vec![],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: dir.path().join("home"),
            run_dir: dir.path().join("run"),
            env: vec![],
            cwd: dir.path().into(),
        })
        .await
        .unwrap();
    for mode in ["models", "models-other", "models-empty", "models-error"] {
        let reply = list_models_process(
            &handle,
            SandboxCommand {
                argv: vec![
                    if cfg!(windows) { "python" } else { "python3" }.into(),
                    format!(
                        "{}/tests/fixtures/fake_codex.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    mode.into(),
                ],
                env: vec![],
                cwd: None,
                pty: None,
            },
        )
        .await;
        match mode {
            "models" => assert_eq!(
                reply
                    .unwrap()
                    .models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second"]
            ),
            "models-other" => assert_eq!(reply.unwrap().models[0].id, "other-account"),
            "models-empty" => assert!(reply.unwrap().models.is_empty()),
            _ => assert!(reply.is_err()),
        }
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn manager_never_persists_codex_keys_or_seeds_claude_into_its_home() {
    use bondsymphonic_daemon::{
        agents::{backend::Backend, codex_backend::CodexBackend},
        daemon::Daemon,
        sandbox::backend_for,
        server::broadcast::EventBus,
        workspace::{lifecycle, DataDirs},
    };
    use bondsymphonic_proto::*;
    let program = format!(
        "{}/tests/fixtures/fake_codex.py",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "/");
    let python = if cfg!(windows) { "python" } else { "python3" };
    std::env::set_var("BS_CODEX_BIN", format!("{python} \"{program}\" normal"));
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let d = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("noop"),
        EventBus::new(100),
    )
    .unwrap();
    let ws = lifecycle::create(
        &d,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            name: "codex".into(),
            base_branch: "main".into(),
            in_place: false,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let options: AgentStartOptions =
        serde_json::from_value(serde_json::json!({"api_key":"key-sentinel"})).unwrap();
    let prepared = CodexBackend
        .prepare(&d, &d.workspace(&ws.id).unwrap(), &options)
        .await
        .unwrap();
    assert!(!format!("{prepared:?}").contains("key-sentinel"));
    assert!(!prepared.argv.join(" ").contains("key-sentinel"));
    let agent = d
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
    d.agents
        .stop(AgentIdParams {
            agent_id: agent.agent_id,
        })
        .await
        .unwrap();
    assert!(!std::fs::read_to_string(d.dirs.agents_file())
        .unwrap()
        .contains("key-sentinel"));
    assert!(
        !serde_json::to_string(&d.workspace_info(&d.workspace(&ws.id).unwrap()))
            .unwrap()
            .contains("key-sentinel")
    );
    assert!(!d
        .dirs
        .root
        .join("agent-homes/codex")
        .join(ws.id.as_str())
        .join(".claude")
        .exists());
    lifecycle::destroy(&d, &ws.id, true).await.unwrap();
    std::env::remove_var("BS_CODEX_BIN");
}
