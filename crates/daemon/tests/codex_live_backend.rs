#![cfg(target_os = "linux")]
mod common;
use bondsymphonic_daemon::{daemon::Daemon,sandbox::backend_for,server::broadcast::EventBus,workspace::{DataDirs,lifecycle}};
use bondsymphonic_proto::*;
use std::{sync::Arc,time::Duration};

async fn turn(d:&Arc<Daemon>,agent:&AgentId,prompt:&str) -> String {
    d.agents.send(AgentSendParams{agent_id:agent.clone(),text:prompt.into()}).await.unwrap();
    tokio::time::timeout(Duration::from_secs(120),async {
        loop {
            let history=d.agents.history(AgentIdParams{agent_id:agent.clone()}).await.unwrap();
            assert!(!matches!(history.state,AgentState::Error|AgentState::Exited),"Codex state {:?}: {:?}",history.state,history.detail);
            if let Some(session)=history.messages.iter().find_map(|m|match &m.body {AgentMessageBody::Result{session_id,..}=>Some(session_id.clone()),_=>None}) {return session;}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.expect("live Codex turn timed out")
}

#[tokio::test(flavor="multi_thread")]
#[ignore="requires BS_CODEX_BIN, BS_CODEX_CODE_MODE_HOST, BS_CODEX_HOME and live ChatGPT access"]
async fn chatgpt_backend_runs_tools_and_resumes_in_a_new_process() {
    let dir=tempfile::tempdir().unwrap();
    let repo=common::init_repo(dir.path());
    let root=dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(std::env::var_os("BS_CODEX_HOME").expect("set BS_CODEX_HOME"),root.join("codex-home")).unwrap();
    let d=Daemon::new(DataDirs::new(root),backend_for("linux_bwrap"),EventBus::new(256)).unwrap();
    let ws=lifecycle::create(&d,WorkspaceCreateParams{repo_path:repo.display().to_string(),name:"live-codex".into(),base_branch:"main".into(),in_place:false,init_if_missing:false}).await.unwrap();
    let options=serde_json::from_value(serde_json::json!({"permission_mode":"never"})).unwrap();
    let first=d.agents.start(&d,AgentStartParams{workspace_id:ws.id.clone(),adapter:AgentAdapterKind::Codex,options}).await.unwrap().agent_id;
    let session=turn(&d,&first,"In this disposable compatibility-test project, run a shell command that writes ADAPTER_OK and a newline to adapter-proof.txt. Read the file to verify it. End with ADAPTER_DONE.").await;
    assert_eq!(std::fs::read_to_string(std::path::Path::new(&ws.worktree_path).join("adapter-proof.txt")).unwrap(),"ADAPTER_OK\n");
    d.agents.stop(AgentIdParams{agent_id:first}).await.unwrap();
    let options=serde_json::from_value(serde_json::json!({"permission_mode":"never","resume_session":session})).unwrap();
    let second=d.agents.start(&d,AgentStartParams{workspace_id:ws.id.clone(),adapter:AgentAdapterKind::Codex,options}).await.unwrap().agent_id;
    let resumed=turn(&d,&second,"Reply with RESUMED_OK. Do not use tools.").await;
    assert_eq!(session,resumed);
    let history=d.agents.history(AgentIdParams{agent_id:second.clone()}).await.unwrap();
    assert!(history.messages.iter().any(|m|matches!(&m.body,AgentMessageBody::AssistantText{text} if text.contains("RESUMED_OK"))));
    d.agents.stop(AgentIdParams{agent_id:second}).await.unwrap();
    lifecycle::destroy(&d,&ws.id,true).await.unwrap();
}
