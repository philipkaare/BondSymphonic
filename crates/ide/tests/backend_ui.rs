use bondsymphonic_proto::*;
use std::{
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[test]
fn codex_auth_failure_and_logout_leave_claude_authenticated() {
    check_backend_auth(false);
}

#[test]
fn auth_error_before_backend_identity_is_known_is_not_lost() {
    check_backend_auth(true);
}

fn check_backend_auth(early: bool) {
    if bondsymphonic_ide::testing::skip_without_qt("backend authentication isolation") {
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let journal = Arc::new(Mutex::new(Vec::new()));
    let (address,_task)=runtime.block_on(async {
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap(); let journal=journal.clone();
        let task=tokio::spawn(async move {
            let (stream,_)=listener.accept().await.unwrap();
            let (r,w)=stream.into_split(); let w=Arc::new(tokio::sync::Mutex::new(w));
            let mut lines=BufReader::new(r).lines();
            let mut logged_out=false;
            while let Some(line)=lines.next_line().await.unwrap() {
                let ClientMessage::Request{id,request}=codec::decode(&line).unwrap();
                let mut delayed=None;
                let result=match request {
                    Request::Hello(_)=>serde_json::json!({"daemon_version":"fake","protocol_version":PROTOCOL_VERSION,"capabilities":{"sandbox_backend":"noop","git_protect":false,"adapters":["claude","codex"],"backends":[]}}),
                    Request::SystemCheckPrereqs{}=>serde_json::json!({"items":[{"name":"claude_auth","ok":true,"detail":"logged in"},{"name":"codex_auth","ok":!logged_out,"detail":"logged in"}]}),
                    Request::WorkspaceList{}=>{
                        let failure=ServerMessage::event(Some(WorkspaceId("ws_backend".into())),Event::AgentStateChanged{agent_id:AgentId("ag_codex".into()),state:AgentState::Error,detail:Some("invalid_api_key".into())});
                        if early {
                            w.lock().await.write_all(codec::encode(&failure).as_bytes()).await.unwrap();
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        } else {
                            delayed=Some((Duration::from_millis(600),failure));
                        }
                        serde_json::json!({"workspaces":[{"id":"ws_backend","name":"backend","repo_path":"/repo","base_branch":"main","branch":"bs/backend","worktree_path":"/work","created_at":"2026-09-30","allowlist":[],"state":"ready","agents":["ag_codex"],"agent_records":[{"id":"ag_codex","adapter":"codex","state":"idle","session_id":"thread","permission_mode":"never"}],"runs":[]}]})
                    },
                    Request::AgentHistory(_)=>serde_json::json!({"state":"idle","messages":[],"dropped":0}),
                    Request::SystemSetupPty(p)=>{
                        logged_out=p.action==SetupAction::CodexLogout;
                        journal.lock().unwrap().push(p.action);
                        delayed=Some((Duration::from_millis(200),ServerMessage::event(None,Event::PtyExit{pty_id:PtyId("pty_codex_logout".into()),code:0})));
                        serde_json::json!({"pty_id":"pty_codex_logout"})
                    },
                    Request::SystemListModels(_)=>serde_json::json!({"models":[]}),
                    Request::FsListDir(_)=>serde_json::json!({"entries":[]}),
                    Request::WorkspaceChanges(_)=>serde_json::json!({"files":[]}),
                    Request::WorkspaceStatus(_)=>serde_json::json!({"entries":[]}),
                    Request::RepoDetectRunConfigs(_)=>serde_json::json!({"configs":[]}),
                    Request::RunList(_)=>serde_json::json!({"runs":[]}),
                    _=>serde_json::json!({}),
                };
                let reply=ServerMessage::ok(id,&result);
                w.lock().await.write_all(codec::encode(&reply).as_bytes()).await.unwrap();
                if let Some((delay,event))=delayed {let w=w.clone();tokio::spawn(async move {tokio::time::sleep(delay).await;let _=w.lock().await.write_all(codec::encode(&event).as_bytes()).await;});}
            }
        }); (address,task)
    });
    let config = std::env::temp_dir().join(format!("bs-backend-ui-{}-{early}", std::process::id()));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"),r#"{"backends":{"claude":{"enabled":true},"codex":{"enabled":true,"default_permission_mode":"never"}}}"#).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("BS_DAEMON_ADDR", address.to_string())
        .env("BS_DAEMON_TOKEN", "test")
        .env("BS_SMOKE_SCRIPT", "quit")
        .env("BS_MENU_TEST", "backend-auth")
        .env("BS_SETTINGS_PATH", config.join("settings.json"))
        .env("BS_STATE_PATH", config.join("state.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |stream: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut out = String::new();
            stream.unwrap().read_to_string(&mut out).unwrap();
            out
        })
    };
    let out = drain(child.stdout.take());
    let stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut out = String::new();
        stderr.read_to_string(&mut out).unwrap();
        out
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("backend UI test timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = out.join().unwrap();
    let err = err.join().unwrap();
    assert!(status.success(), "{out}\n{err}");
    assert!(
        out.contains("BS_MENU_TEST backend-auth target=codex-closed question=claude-open"),
        "{out}\n{err}"
    );
    assert_eq!(*journal.lock().unwrap(), vec![SetupAction::CodexLogout]);
    assert_eq!(
        out.lines()
            .rev()
            .find(|line| line.starts_with("BS_MENU_TEST backend-logout")),
        Some("BS_MENU_TEST backend-logout target=codex-closed question=claude-open"),
        "{out}\n{err}"
    );
    let _ = std::fs::remove_dir_all(&config);
}
