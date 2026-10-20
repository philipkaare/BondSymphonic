mod common;
use bondsymphonic_daemon::agents::backend::Backend;
use bondsymphonic_daemon::agents::{
    backend,
    opencode_backend::{has_login, opencode_home, process_env, OpenCodeBackend},
};
use bondsymphonic_proto::{AgentAdapterKind, SetupAction};
static PROCESS_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct EnvScope {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}
impl EnvScope {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old }
    }
}
impl Drop for EnvScope {
    fn drop(&mut self) {
        if let Some(value) = &self.old {
            std::env::set_var(self.key, value)
        } else {
            std::env::remove_var(self.key)
        }
    }
}

#[test]
fn registered_with_native_setup_and_permissions() {
    let descriptor = backend::backend_for(AgentAdapterKind::OpenCode)
        .unwrap()
        .descriptor();
    assert_eq!(descriptor.label, "OpenCode");
    assert_eq!(descriptor.default_permission_mode, "ask");
    assert!(descriptor
        .setup_actions
        .contains(&SetupAction::OpencodeLogin));
    assert!(backend::descriptors()
        .iter()
        .any(|d| d.id == AgentAdapterKind::OpenCode));
    assert!(!OpenCodeBackend.extra_hosts().contains(&"*".into()));
}

#[test]
fn credentials_and_state_are_backend_private() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!has_login(dir.path()));
    let home = opencode_home(dir.path());
    std::fs::create_dir_all(home.join("data/opencode")).unwrap();
    let path = home.join("data/opencode/auth.json");
    for invalid in ["{}", "broken", r#"{"openai":{"type":"api","key":""}}"#] {
        std::fs::write(&path, invalid).unwrap();
        assert!(!has_login(dir.path()));
    }
    std::fs::write(&path, r#"{"openai":{"type":"api","key":"sentinel"}}"#).unwrap();
    assert!(has_login(dir.path()));
    let env = process_env(dir.path(), Some("secret"));
    assert!(env
        .iter()
        .any(|(k, v)| k == "OPENAI_API_KEY" && v == "secret"));
    assert!(env
        .iter()
        .any(|(k, v)| k == "XDG_DATA_HOME" && v == &home.join("data").display().to_string()));
    assert!(!env
        .iter()
        .any(|(k, _)| k == "CODEX_HOME" || k == "ANTHROPIC_API_KEY"));
}

#[test]
fn native_login_is_imported_once_and_logout_does_not_resurrect_it() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("native-data");
    let config = dir.path().join("native-config");
    std::fs::create_dir_all(data.join("opencode")).unwrap();
    std::fs::create_dir_all(config.join("opencode")).unwrap();
    std::fs::write(
        data.join("opencode/auth.json"),
        r#"{"openai":{"type":"api","key":"native-secret"}}"#,
    )
    .unwrap();
    std::fs::write(
        config.join("opencode/opencode.json"),
        r#"{"model":"openai/example"}"#,
    )
    .unwrap();
    let root = dir.path().join("managed");
    seed_native_files(&root, &config, &data).unwrap();
    assert!(has_login(&root));
    assert!(opencode_home(&root)
        .join("config/opencode/opencode.json")
        .is_file());
    std::fs::write(opencode_home(&root).join("data/opencode/auth.json"), "{}").unwrap();
    seed_native_files(&root, &config, &data).unwrap();
    assert!(
        !has_login(&root),
        "logging out must not import the old native key again"
    );
    assert!(std::fs::read_to_string(data.join("opencode/auth.json"))
        .unwrap()
        .contains("native-secret"));
}

#[test]
fn native_config_edits_additions_deletions_and_legacy_json_are_reflected_on_refresh() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("regular/opencode");
    std::fs::create_dir_all(&source).unwrap();
    let root = dir.path().join("managed");
    let data = dir.path().join("native-data");
    let target = opencode_home(&root).join("config/opencode");
    std::fs::write(
        source.join("opencode.json"),
        br#"{"model":"local/old","provider":{"local":{"name":"before"}}}"#,
    )
    .unwrap();
    seed_native_files(&root, &dir.path().join("regular"), &data).unwrap();
    let edited = br#"{"model":"local/new","provider":{"local":{"name":"after"}}}"#;
    let jsonc = b"{\n // my provider comments\n \"model\": \"local/commented\",\n}\n";
    std::fs::write(source.join("opencode.json"), edited).unwrap();
    std::fs::write(source.join("opencode.jsonc"), jsonc).unwrap();
    std::fs::write(source.join("config.json"), b"{\"model\":\"local/legacy\"}").unwrap();
    seed_native_files(&root, &dir.path().join("regular"), &data).unwrap();
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        assert_eq!(
            std::fs::read(target.join(name)).unwrap(),
            std::fs::read(source.join(name)).unwrap()
        );
    }
    assert_eq!(std::fs::read(target.join("opencode.json")).unwrap(), edited);
    std::fs::remove_file(source.join("opencode.json")).unwrap();
    std::fs::remove_file(source.join("opencode.jsonc")).unwrap();
    seed_native_files(&root, &dir.path().join("regular"), &data).unwrap();
    assert!(!target.join("opencode.json").exists());
    assert!(!target.join("opencode.jsonc").exists());
    assert_eq!(
        std::fs::read(source.join("config.json")).unwrap(),
        b"{\"model\":\"local/legacy\"}"
    );
    std::fs::write(target.join("independent.json"), b"keep me").unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    seed_native_files(&root, &dir.path().join("regular"), &data).unwrap();
    assert!(!target.join("config.json").exists());
    assert_eq!(
        std::fs::read(target.join("independent.json")).unwrap(),
        b"keep me"
    );
}

#[test]
fn legacy_import_migration_takes_back_deleted_mirrors_without_wiping_independent_config() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    for native_exists in [true, false] {
        let root = dir.path().join(if native_exists {
            "imported"
        } else {
            "independent"
        });
        let home = opencode_home(&root);
        let target = home.join("config/opencode");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(home.join(".native-imported"), b"1").unwrap();
        std::fs::write(
            target.join("opencode.json"),
            b"{\"model\":\"keep-or-delete\"}",
        )
        .unwrap();
        let config = dir.path().join(if native_exists {
            "regular"
        } else {
            "never-configured"
        });
        if native_exists {
            std::fs::create_dir_all(config.join("opencode")).unwrap();
        }
        seed_native_files(&root, &config, &dir.path().join("no-auth")).unwrap();
        assert_eq!(target.join("opencode.json").exists(), !native_exists);
    }
}

#[test]
fn config_refresh_reports_unreadable_sources_and_preserves_previous_files() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("native-config");
    let root = dir.path().join("managed");
    std::fs::create_dir_all(config.join("opencode")).unwrap();
    std::fs::write(config.join("opencode/opencode.json"), b"original").unwrap();
    seed_native_files(&root, &config, &dir.path().join("data")).unwrap();
    std::fs::remove_file(config.join("opencode/opencode.json")).unwrap();
    std::fs::create_dir(config.join("opencode/opencode.json")).unwrap();
    let error = seed_native_files(&root, &config, &dir.path().join("data"))
        .expect_err("invalid source must not silently retain stale configuration");
    assert!(error.to_string().contains("opencode.json"));
    assert_eq!(
        std::fs::read(opencode_home(&root).join("config/opencode/opencode.json")).unwrap(),
        b"original"
    );
}

#[test]
fn config_root_uses_the_windows_user_override_and_ignores_relative_xdg_paths() {
    use bondsymphonic_daemon::agents::opencode_config::{native_config_root, USER_CONFIG_ROOT_ENV};
    use std::{ffi::OsStr, path::Path};
    assert_eq!(USER_CONFIG_ROOT_ENV, "BS_OPENCODE_CONFIG_ROOT");
    let home = std::env::temp_dir().join("linux-user");
    let windows = std::env::temp_dir().join("windows-user-config");
    let xdg = std::env::temp_dir().join("xdg-config");
    assert_eq!(
        native_config_root(&home, Some(windows.as_os_str()), Some(xdg.as_os_str())),
        windows
    );
    assert_eq!(
        native_config_root(&home, Some(OsStr::new("")), Some(xdg.as_os_str())),
        xdg
    );
    assert_eq!(
        native_config_root(&home, None, Some(OsStr::new("relative"))),
        home.join(".config")
    );
    assert_eq!(
        native_config_root(&home, Some(OsStr::new("relative")), Some(OsStr::new(""))),
        home.join(".config")
    );
    assert_eq!(native_config_root(&home, None, None), home.join(".config"));
    assert!(!Path::new("relative").is_absolute());
}

#[test]
fn config_refresh_refuses_invalid_destinations_without_touching_regular_files() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("regular");
    let root = dir.path().join("managed");
    let source = config.join("opencode/opencode.json");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"native-model-and-provider").unwrap();
    let target = opencode_home(&root).join("config/opencode/opencode.json");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("occupied"), b"managed-content").unwrap();
    let error = seed_native_files(&root, &config, &dir.path().join("data")).unwrap_err();
    assert!(error.to_string().contains("opencode.json"));
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"native-model-and-provider"
    );
    assert_eq!(
        std::fs::read(target.join("occupied")).unwrap(),
        b"managed-content"
    );
}

#[test]
fn a_managed_config_used_as_native_source_is_never_rewritten_or_claimed() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    let config = opencode_home(&root).join("config");
    std::fs::create_dir_all(config.join("opencode")).unwrap();
    let path = config.join("opencode/opencode.json");
    std::fs::write(&path, b"independent-config").unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    seed_native_files(&root, &config, &dir.path().join("data")).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    seed_native_files(
        &root,
        &dir.path().join("no-native-config"),
        &dir.path().join("data"),
    )
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"independent-config");
}

#[cfg(unix)]
#[test]
fn config_refresh_refuses_linked_destination_directories_and_files() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("native-config");
    std::fs::create_dir_all(config.join("opencode")).unwrap();
    std::fs::write(config.join("opencode/opencode.json"), b"incoming").unwrap();
    let victim = dir.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("opencode.json"), b"user-host-content").unwrap();
    for point in ["config", "config/opencode", "config/opencode/opencode.json"] {
        let root = dir.path().join(point.replace('/', "-"));
        let home = opencode_home(&root);
        let target = home.join(point);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        symlink(
            if point.ends_with(".json") {
                victim.join("opencode.json")
            } else {
                victim.clone()
            },
            &target,
        )
        .unwrap();
        assert!(
            seed_native_files(&root, &config, &dir.path().join("data")).is_err(),
            "{point}"
        );
        assert_eq!(
            std::fs::read(victim.join("opencode.json")).unwrap(),
            b"user-host-content"
        );
    }
}

#[cfg(unix)]
#[test]
fn native_config_source_fifo_is_refused_without_waiting_for_a_writer() {
    use bondsymphonic_daemon::agents::opencode_backend::seed_native_files;
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::{io::Write, os::unix::fs::OpenOptionsExt, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("regular");
    std::fs::create_dir_all(config.join("opencode")).unwrap();
    let path = config.join("opencode/opencode.json");
    mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let root = dir.path().join("managed");
    let data = dir.path().join("data");
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(seed_native_files(&root, &config, &data))
            .unwrap();
    });
    let answer = receiver.recv_timeout(Duration::from_secs(1));
    if answer.is_err() {
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        writer.write_all(b"{}").unwrap();
        drop(writer);
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    worker.join().unwrap();
    assert!(
        answer.is_ok(),
        "config refresh blocked on a nonregular source"
    );
    assert!(
        answer.unwrap().is_err(),
        "a FIFO is not a native configuration file"
    );
}

#[tokio::test]
async fn companion_state_is_isolated_and_removed_with_workspace() {
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::noop::NoopBackend,
        server::broadcast::EventBus,
        workspace::{agent_sandboxes::companion_spec, lifecycle, DataDirs},
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let daemon = std::sync::Arc::new(
        Daemon::new(
            DataDirs::new(dir.path().join("data")),
            std::sync::Arc::new(NoopBackend),
            EventBus::new(100),
        )
        .unwrap(),
    );
    let info = lifecycle::create(
        &daemon,
        bondsymphonic_proto::WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "opencode".into(),
            init_if_missing: false,
            in_place: true,
        },
    )
    .await
    .unwrap();
    let workspace = daemon.workspace(&info.id).unwrap();
    let spec = companion_spec(&daemon, &workspace, &OpenCodeBackend)
        .await
        .unwrap();
    assert_ne!(spec.home, daemon.dirs.home(&info.id));
    assert!(spec
        .home
        .ends_with(format!("agent-homes/opencode/{}", info.id)));
    let shared = opencode_home(&daemon.dirs.root);
    assert!(spec.rw_binds.contains(&(shared.clone(), shared.clone())));
    assert!(!spec
        .rw_binds
        .iter()
        .any(|(path, _)| path == &daemon.dirs.root.join("codex-home")));
    for path in [
        &spec.home,
        &spec.run_dir,
        &daemon
            .dirs
            .root
            .join("agent-caches/opencode")
            .join(info.id.as_str()),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
    assert!(!spec.home.exists());
    assert!(!spec.run_dir.exists());
    assert!(repo.join("README.md").exists());
}

#[tokio::test]
async fn selected_permissions_use_a_private_primary_agent_despite_native_build_override() {
    let _environment = PROCESS_ENV.lock().await;
    use bondsymphonic_daemon::{
        agents::{
            adapter::AgentAdapter, opencode::OpenCodeAdapter, AgentEntry, AgentSink,
            TranscriptStore,
        },
        daemon::Daemon,
        sandbox::{backend_for, SandboxSpec},
        server::broadcast::EventBus,
        workspace::{lifecycle, DataDirs},
    };
    use bondsymphonic_proto::*;
    use std::{sync::Arc, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let _config_source = EnvScope::set(
        "BS_OPENCODE_CONFIG_ROOT",
        &dir.path().join("regular-config"),
    );
    let repo = common::init_repo(dir.path());
    let config = r#"{"model":"local/existing","provider":{"local":{"name":"local"}},"mcp":{"existing":{"enabled":true}},"agent":{"build":{"permission":{"bash":"allow"}}}}"#;
    std::fs::write(repo.join("opencode.json"), config).unwrap();
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("noop"),
        EventBus::new(100),
    )
    .unwrap();
    let info = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "permissions".into(),
            in_place: true,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let workspace = daemon.workspace(&info.id).unwrap();
    let old_binary = std::env::var_os("BS_OPENCODE_BIN");
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(value) = &self.0 {
                std::env::set_var("BS_OPENCODE_BIN", value)
            } else {
                std::env::remove_var("BS_OPENCODE_BIN")
            }
        }
    }
    let _restore = Restore(old_binary);
    let program = format!(
        "{}/tests/fixtures/fake_opencode.py",
        env!("CARGO_MANIFEST_DIR")
    )
    .replace('\\', "/");
    let python = if cfg!(windows) { "python" } else { "python3" };
    for mode in ["ask", "yolo"] {
        let fixture = if mode == "ask" {
            "permission-ask"
        } else {
            "permission-yolo"
        };
        std::env::set_var(
            "BS_OPENCODE_BIN",
            format!("{python} \"{program}\" {fixture}"),
        );
        let prepared = OpenCodeBackend
            .prepare(
                &daemon,
                &workspace,
                &serde_json::from_value(
                    serde_json::json!({"permission_mode":mode,"resume_session":"saved-session"}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let launch: serde_json::Value = serde_json::from_str(
            &prepared
                .env
                .iter()
                .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
                .unwrap()
                .1,
        )
        .unwrap();
        assert!(
            launch["default_agent"]
                .as_str()
                .is_some_and(|name| name.starts_with("bondsymphonic_ws_")),
            "permission selection must use its own primary agent"
        );
        let entry = Arc::new(AgentEntry::new(info.id.clone(), AgentAdapterKind::OpenCode));
        let store = Arc::new(TranscriptStore::new(
            dir.path().join(format!("transcripts-{mode}")),
        ));
        let sink = AgentSink::new(
            EventBus::new(100),
            store.clone(),
            AgentId("agent".into()),
            info.id.clone(),
            entry.clone(),
        );
        let handle = backend_for("noop")
            .start(&SandboxSpec {
                id: info.id.clone(),
                rw_binds: vec![],
                ro_binds: vec![],
                late_ro_binds: vec![],
                home: dir.path().join("home"),
                run_dir: dir.path().join("run"),
                env: vec![],
                cwd: repo.clone(),
            })
            .await
            .unwrap();
        let mut adapter =
            OpenCodeAdapter::new(sink, handle, prepared).with_timeout(Duration::from_secs(2));
        adapter.start().await.unwrap();
        adapter.send("check permissions".into()).await.unwrap();
        let expected = if mode == "ask" {
            "ask-checked"
        } else {
            "allow-checked"
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let messages = store.read(&AgentId("agent".into())).await.unwrap();
                if messages.iter().any(
                    |m| matches!(&m.body,AgentMessageBody::AssistantDelta{text}if text==expected),
                ) {
                    break;
                }
                assert_ne!(
                    entry.state().0,
                    AgentState::Exited,
                    "fake peer rejected launch permissions"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        adapter.stop().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("opencode.json")).unwrap(),
            config
        );
    }
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
}

#[tokio::test]
async fn config_read_errors_stop_prepare_and_model_discovery_with_actionable_errors() {
    use bondsymphonic_daemon::{
        daemon::Daemon,
        sandbox::backend_for,
        server::broadcast::EventBus,
        workspace::{lifecycle, DataDirs},
    };
    use bondsymphonic_proto::*;
    let _environment = PROCESS_ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("regular");
    std::fs::create_dir_all(source.join("opencode/opencode.json")).unwrap();
    let _config_source = EnvScope::set("BS_OPENCODE_CONFIG_ROOT", &source);
    let repo = common::init_repo(dir.path());
    let daemon = Daemon::new(
        DataDirs::new(dir.path().join("data")),
        backend_for("noop"),
        EventBus::new(100),
    )
    .unwrap();
    let info = lifecycle::create(
        &daemon,
        WorkspaceCreateParams {
            repo_path: repo.display().to_string(),
            base_branch: "main".into(),
            name: "config-error".into(),
            in_place: true,
            init_if_missing: false,
        },
    )
    .await
    .unwrap();
    let workspace = daemon.workspace(&info.id).unwrap();
    let prepare = OpenCodeBackend
        .prepare(
            &daemon,
            &workspace,
            &serde_json::from_value(serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap_err();
    let models = OpenCodeBackend
        .list_models(&daemon, None)
        .await
        .unwrap_err();
    for error in [prepare, models] {
        assert_eq!(error.code, ErrorCode::IoError);
        assert!(error.message.contains("config could not be refreshed"));
        assert!(error.message.contains("opencode.json"));
    }
    lifecycle::destroy(&daemon, &info.id, false).await.unwrap();
}
