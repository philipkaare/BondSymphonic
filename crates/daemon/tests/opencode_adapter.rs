use bondsymphonic_daemon::agents::acp::{AcpClient, AcpEvent};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[tokio::test]
async fn string_peer_id_does_not_consume_numeric_waiter() {
    let (client, peer) = tokio::io::duplex(8192);
    let (cr, cw) = tokio::io::split(client);
    let (rpc, mut events) =
        AcpClient::connect(Box::pin(cr), Box::pin(cw), vec![], Duration::from_secs(1));
    let worker = tokio::spawn(async move {
        let (pr, mut pw) = tokio::io::split(peer);
        let mut lines = BufReader::new(pr).lines();
        let call: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        pw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"method\":\"session/request_permission\",\"params\":{}}\n").await.unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], "1");
        pw.write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":call["id"],"result":{"ok":true}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    });
    let r = rpc.clone();
    let call = tokio::spawn(async move { r.call("session/prompt", json!({})).await });
    let AcpEvent::Frame(event) = events.recv().await.unwrap() else {
        panic!("expected permission request");
    };
    rpc.reply(
        event["id"].clone(),
        json!({"outcome":{"outcome":"cancelled"}}),
    )
    .await
    .unwrap();
    let AcpEvent::Response {
        answer, complete, ..
    } = events.recv().await.unwrap()
    else {
        panic!("expected ordered prompt response");
    };
    complete.send(answer).unwrap();
    assert_eq!(call.await.unwrap().unwrap()["ok"], true);
    worker.await.unwrap();
}
#[tokio::test]
async fn oversized_frame_closes_transport() {
    let (client, mut peer) = tokio::io::duplex(2048);
    let (r, w) = tokio::io::split(client);
    let (rpc, _) = AcpClient::connect(Box::pin(r), Box::pin(w), vec![], Duration::from_secs(2));
    tokio::spawn(async move {
        let _ = peer.write_all(&vec![b'x'; 1024 * 1024 + 1]).await;
    });
    assert!(rpc.call("initialize", json!({})).await.is_err());
}

use bondsymphonic_daemon::{
    agents::{
        adapter::AgentAdapter, backend::PreparedAgent, opencode_adapter::OpenCodeAdapter,
        AgentEntry, AgentSink, TranscriptStore,
    },
    sandbox::{noop::NoopBackend, SandboxBackend, SandboxSpec},
    server::broadcast::EventBus,
};
use bondsymphonic_proto::*;
use std::sync::Arc;
const PEER: &str = r#"
import json,sys,os
mode=sys.argv[1]
fixture=json.load(open(sys.argv[2]))['examples']
pending=None
approvals=0
cwd=None
structured_output={'authorization':'unregistered-token','nested':{'apiKey':'unregistered-key','password':'unregistered-password'},'registered':os.environ.get('ESCAPED_API_KEY','')}
def emit(v): print(json.dumps(v),flush=True)
def reply(id,r):emit({'jsonrpc':'2.0','id':id,'result':r})
def update(u):emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'session-fixture','update':u}})
for line in sys.stdin:
 f=json.loads(line); m=f.get('method'); p=f.get('params',{}); id=f.get('id')
 if m=='initialize':reply(id,fixture['initializeSourceResponse']['result'])
 elif m in ['session/new','session/load']:
  cwd=p['cwd']
  assert p['mcpServers'][0]['headers'][0]['value']=='header-secret',p
  if m=='session/load':update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'historical duplicate'}})
  reply(id,{'sessionId':('session-secret-key' if mode=='opaque' else p.get('sessionId','session-fixture')),'configOptions':[{'id':('model-secret-key' if mode=='opaque' else 'model'),'category':'model','options':[{'value':('local/secret-key' if mode=='opaque' else 'local/test'),'name':'Test'}]}]})
 elif m=='session/set_config_option':
  if mode=='opaque':assert p=={'sessionId':'session-secret-key','configId':'model-secret-key','value':'local/secret-key'},p
  reply(id,{'configOptions':[]})
 elif m=='session/prompt':
  pending=id
  if mode=='opaque':
   assert p['sessionId']=='session-secret-key',p
   emit({'jsonrpc':'2.0','id':'peer-secret-key','method':'session/request_permission','params':{'toolCall':{'title':'Edit secret-key'},'options':[{'kind':'allow_once','optionId':'once-secret-key'}]}})
  elif mode=='permissions':
   for pid in ['peer',77]:emit({'jsonrpc':'2.0','id':pid,'method':'session/request_permission','params':{'sessionId':'session-fixture','toolCall':{'title':'Edit'},'options':[{'optionId':'once','kind':'allow_once'},{'optionId':'always','kind':'allow_always'},{'optionId':'reject','kind':'reject_once'}]}})
  elif mode=='structured-output':
   update({'sessionUpdate':'tool_call','toolCallId':'edit','title':'Edit','status':'in_progress','rawInput':{'password':'input-password','registered':os.environ['ESCAPED_API_KEY']}})
   emit({'jsonrpc':'2.0','id':'fs','method':'fs/write_text_file','params':{'path':os.path.join(cwd,'structured.txt'),'content':json.dumps(structured_output)}})
  elif mode=='filesystem':
   update({'sessionUpdate':'tool_call','toolCallId':'edit','title':'Edit','rawInput':{'content':'safe edit secret-key native-secret header-secret'}})
   emit({'jsonrpc':'2.0','id':'fs','method':'fs/write_text_file','params':{'path':os.path.join(cwd,'approved.txt'),'content':'safe edit secret-key native-secret header-secret'}})
  elif mode=='terminal':emit({'jsonrpc':'2.0','id':'terminal','method':'terminal/create','params':{'command':'should-never-run'}})
  elif mode=='path':emit({'jsonrpc':'2.0','id':'fs','method':'fs/write_text_file','params':{'path':'../escape','content':'BAD'}})
  elif mode!='cancel':
   update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'hello secret-key'}})
   update({'sessionUpdate':'tool_call','toolCallId':'t','title':'Edit','status':'in_progress','rawInput':{'a':1}})
   update({'sessionUpdate':'tool_call_update','toolCallId':'t','status':'completed','rawOutput':'done'})
   reply(id,{'stopReason':'end_turn'});pending=None
 elif m=='session/cancel':
  if pending is not None:reply(pending,{'stopReason':'cancelled'});pending=None
  if mode=='cancel':sys.exit(0)
 elif m is None:
  if mode=='opaque':
   assert id=='peer-secret-key' and f['result']['outcome']['optionId']=='once-secret-key',f
   reply(pending,{'stopReason':'end_turn'});pending=None
  if mode=='permissions':
   expected='always' if id==77 else 'once'
   assert f['result']['outcome']['optionId']==expected,f
   approvals+=1
   if approvals==2:reply(pending,{'stopReason':'end_turn'});pending=None
  if mode=='filesystem':
   assert f['result']=={},f
   assert open(os.path.join(cwd,'approved.txt')).read()=='safe edit secret-key native-secret header-secret'
   reply(pending,{'stopReason':'end_turn'});pending=None
  if mode=='structured-output':
   assert f['result']=={},f
   assert json.load(open(os.path.join(cwd,'structured.txt')))==structured_output
   update({'sessionUpdate':'tool_call_update','toolCallId':'edit','status':'completed','rawOutput':structured_output})
   reply(pending,{'stopReason':'end_turn'});pending=None
  if mode=='terminal':
   assert f['error']['code']==-32601,f
   reply(pending,{'stopReason':'end_turn'});pending=None
  if mode=='path':
   assert f['error']['code']==-32602,f
   reply(pending,{'stopReason':'end_turn'});pending=None
"#;
async fn fixture(
    mode: &str,
    resume: Option<&str>,
    model: &str,
) -> (
    tempfile::TempDir,
    OpenCodeAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
    EventBus,
) {
    fixture_with_handle(mode, resume, model, None).await
}
async fn fixture_with_handle(
    mode: &str,
    resume: Option<&str>,
    model: &str,
    handle_override: Option<Arc<dyn bondsymphonic_daemon::sandbox::SandboxHandle>>,
) -> (
    tempfile::TempDir,
    OpenCodeAdapter,
    Arc<AgentEntry>,
    Arc<TranscriptStore>,
    EventBus,
) {
    let dir = tempfile::tempdir().unwrap();
    let ws: WorkspaceId = "ws_acp".into();
    let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::Codex));
    let store = Arc::new(TranscriptStore::new(dir.path().join("transcripts")));
    let events = EventBus::new(128);
    let records = Arc::new(bondsymphonic_daemon::agents::persist::AgentRecords::new(
        dir.path().join("records.json"),
    ));
    let sink = AgentSink::new(
        events.clone(),
        store.clone(),
        "ag_acp".into(),
        ws.clone(),
        entry.clone(),
    )
    .with_records(records.clone());
    let handle = NoopBackend
        .start(&SandboxSpec {
            id: ws,
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
    let options: AgentStartOptions = serde_json::from_value(
        json!({"resume_session":resume,"model":model,"api_key":"secret-key"}),
    )
    .unwrap();
    records.upsert(bondsymphonic_daemon::agents::persist::AgentRecord {
        agent_id: "ag_acp".into(),
        workspace_id: "ws_acp".into(),
        adapter: AgentAdapterKind::Codex,
        session_id: None,
        options: options.clone(),
        started_at: "2026-09-30T00:00:00Z".into(),
        ended_at: None,
    });
    let prepared = PreparedAgent {
        mcp_servers: vec![
            json!({"type":"http","name":"fixture","url":"http://fixture.invalid","headers":[{"name":"Authorization","value":"header-secret"}]}),
        ],
        provider_endpoints: vec![],
        argv: vec![
            if cfg!(windows) { "python" } else { "python3" }.into(),
            "-u".into(),
            "-c".into(),
            PEER.into(),
            mode.into(),
            format!(
                "{}/tests/fixtures/opencode/documented-acp-v1.json",
                env!("CARGO_MANIFEST_DIR")
            ),
        ],
        env: vec![
            ("NATIVE_API_KEY".into(), "native-secret".into()),
            ("ESCAPED_API_KEY".into(), r#"escaped-"token\slash"#.into()),
        ],
        cwd: dir.path().into(),
        options,
    };
    (
        dir,
        OpenCodeAdapter::new(sink, handle_override.unwrap_or(handle), prepared)
            .with_timeout(Duration::from_secs(2)),
        entry,
        store,
        events,
    )
}
async fn wait(entry: &AgentEntry, wanted: AgentState) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while entry.state().0 != wanted {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn acp_prompt_tool_and_result() {
    let (_d, mut a, e, s, _) = fixture("normal", None, "local/test").await;
    a.start().await.unwrap();
    a.send("go secret-key".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    let ms = s.read(&"ag_acp".into()).await.unwrap();
    assert!(!serde_json::to_string(&ms).unwrap().contains("secret-key"));
    assert!(ms.iter().any(
        |m| matches!(&m.body,AgentMessageBody::AssistantDelta{text} if text=="hello [redacted]")
    ));
    assert!(ms.iter().any(|m|matches!(&m.body,AgentMessageBody::ToolResult{output,is_error,..} if output.contains("done") && !is_error)));
    assert_eq!(
        ms.iter()
            .filter(|m| matches!(m.body, AgentMessageBody::Result { .. }))
            .count(),
        1
    );
    a.stop().await.unwrap();
}
#[tokio::test]
async fn overlapping_permissions_preserved() {
    let (_d, mut a, e, s, _) = fixture("permissions", None, "local/test").await;
    a.start().await.unwrap();
    a.send("go".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let ms = s.read(&"ag_acp".into()).await.unwrap();
            if ms
                .iter()
                .filter(|m| matches!(m.body, AgentMessageBody::PermissionRequest { .. }))
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    a.permission_reply(
        "acp:permission:1".into(),
        PermissionDecision::Allow,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(e.state().0, AgentState::WaitingPermission);
    a.permission_reply(
        "acp:permission:2".into(),
        PermissionDecision::AllowForSession,
        None,
        None,
    )
    .await
    .unwrap();
    wait(&e, AgentState::Idle).await;
    a.stop().await.unwrap();
}
#[tokio::test]
async fn cancel_then_exit_completes_once() {
    let (_d, mut a, e, _, events) = fixture("cancel", None, "local/test").await;
    let mut rx = events.subscribe();
    a.start().await.unwrap();
    a.send("go".into()).await.unwrap();
    a.interrupt().await.unwrap();
    wait(&e, AgentState::Exited).await;
    a.stop().await.unwrap();
    let mut n = 0;
    while let Ok(m) = rx.try_recv() {
        if matches!(
            m,
            ServerMessage::Event {
                event: Event::AgentStateChanged {
                    state: AgentState::Exited,
                    ..
                },
                ..
            }
        ) {
            n += 1;
        }
    }
    assert_eq!(n, 1);
}
#[tokio::test]
async fn resume_after_daemon_restart() {
    let (d, mut a, e, store, _) = fixture("normal", Some("persisted-session"), "local/test").await;
    a.start().await.unwrap();
    assert_eq!(e.session_id.lock().as_deref(), Some("persisted-session"));
    a.send("continue".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    let persisted =
        bondsymphonic_daemon::agents::persist::AgentRecords::new(d.path().join("records.json"))
            .load();
    assert_eq!(
        persisted[0].session_id.as_deref(),
        Some("persisted-session")
    );
    let messages = store.read(&"ag_acp".into()).await.unwrap();
    assert!(!messages.iter().any(|m|matches!(&m.body,AgentMessageBody::AssistantDelta{text} if text.contains("historical duplicate"))));
    a.stop().await.unwrap();
}
#[tokio::test]
async fn removed_provider_blocks_resume_safely() {
    let (_d, mut a, e, _, _) = fixture("normal", Some("persisted-session"), "removed/model").await;
    assert!(a.start().await.unwrap_err().message.contains("unavailable"));
    assert_eq!(e.state().0, AgentState::Exited);
}
#[tokio::test]
async fn peer_path_escape_rejected() {
    let (d, mut a, e, _, _) = fixture("path", None, "local/test").await;
    a.start().await.unwrap();
    a.send("edit".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    assert!(!d.path().parent().unwrap().join("escape").exists());
    a.stop().await.unwrap();
}

#[tokio::test]
async fn approved_edit_uses_scoped_filesystem() {
    let (d, mut a, e, s, _) = fixture("filesystem", None, "local/test").await;
    a.start().await.unwrap();
    a.send("edit".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    assert_eq!(
        std::fs::read_to_string(d.path().join("approved.txt")).unwrap(),
        "safe edit secret-key native-secret header-secret"
    );
    let transcript = serde_json::to_string(&s.read(&"ag_acp".into()).await.unwrap()).unwrap();
    for secret in ["secret-key", "native-secret", "header-secret"] {
        assert!(!transcript.contains(secret), "leaked {secret}");
    }
    a.stop().await.unwrap();
}
#[tokio::test]
async fn terminal_peer_is_explicitly_unsupported() {
    let (_d, mut a, e, _, _) = fixture("terminal", None, "local/test").await;
    a.start().await.unwrap();
    a.send("command".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    a.stop().await.unwrap();
}
#[tokio::test]
async fn overlapping_prompts_are_serialized() {
    let (_d, mut a, e, s, _) = fixture("normal", None, "local/test").await;
    a.start().await.unwrap();
    a.send("first".into()).await.unwrap();
    a.send("second".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    let ms = s.read(&"ag_acp".into()).await.unwrap();
    assert_eq!(
        ms.iter()
            .filter(|m| matches!(m.body, AgentMessageBody::Result { .. }))
            .count(),
        2
    );
    a.stop().await.unwrap();
}

#[tokio::test]
async fn all_registered_secrets_are_redacted_from_user_transcripts() {
    let (_d, mut a, e, s, _) = fixture("normal", None, "local/test").await;
    a.start().await.unwrap();
    a.send("secret-key native-secret header-secret".into())
        .await
        .unwrap();
    wait(&e, AgentState::Idle).await;
    let text = serde_json::to_string(&s.read(&"ag_acp".into()).await.unwrap()).unwrap();
    for secret in ["secret-key", "native-secret", "header-secret"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    a.stop().await.unwrap();
}
#[tokio::test]
async fn opaque_session_model_and_permission_values_preserved() {
    let (_d, mut a, e, s, _) = fixture("opaque", None, "local/secret-key").await;
    a.start().await.unwrap();
    assert_eq!(e.session_id.lock().as_deref(), Some("session-secret-key"));
    a.send("edit".into()).await.unwrap();
    wait(&e, AgentState::WaitingPermission).await;
    let ms = s.read(&"ag_acp".into()).await.unwrap();
    let request = ms
        .iter()
        .find_map(|m| {
            if let AgentMessageBody::PermissionRequest { request_id, .. } = &m.body {
                Some(request_id.clone())
            } else {
                None
            }
        })
        .unwrap();
    a.permission_reply(request, PermissionDecision::Allow, None, None)
        .await
        .unwrap();
    wait(&e, AgentState::Idle).await;
    assert!(
        !serde_json::to_string(&s.read(&"ag_acp".into()).await.unwrap())
            .unwrap()
            .contains("secret-key")
    );
    a.stop().await.unwrap();
}
#[tokio::test]
async fn prompt_completion_waits_for_delayed_event_pump() {
    let (client, peer) = tokio::io::duplex(8192);
    let (r, w) = tokio::io::split(client);
    let (rpc, mut events) =
        AcpClient::connect(Box::pin(r), Box::pin(w), vec![], Duration::from_secs(2));
    let release = Arc::new(tokio::sync::Notify::new());
    let hold = release.clone();
    let (sent, received) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(peer);
        let mut lines = BufReader::new(r).lines();
        let request: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        w.write_all(format!("{}\n{}\n",json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"before result"}}}}),json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}})).as_bytes()).await.unwrap();
        let _ = sent.send(());
        hold.notified().await;
    });
    let r = rpc.clone();
    let mut call = tokio::spawn(async move { r.call("session/prompt", json!({})).await });
    received.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut call)
            .await
            .is_err(),
        "prompt completed while prior event remained unprocessed"
    );
    let _ = events.recv().await.unwrap();
    let AcpEvent::Response {
        answer, complete, ..
    } = events.recv().await.unwrap()
    else {
        panic!("expected prompt completion barrier");
    };
    complete.send(answer).unwrap();
    assert_eq!(call.await.unwrap().unwrap()["stopReason"], "end_turn");
    rpc.close().await;
    release.notify_one();
    worker.await.unwrap();
}

#[tokio::test]
async fn delayed_pump_keeps_two_tool_turns_before_their_results() {
    let (_d, mut a, e, s, _) = fixture("normal", None, "local/test").await;
    a = a.with_event_delay_for_tests(Duration::from_millis(25));
    a.start().await.unwrap();
    a.send("first".into()).await.unwrap();
    a.send("second".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;
    let bodies = s.read(&"ag_acp".into()).await.unwrap();
    let mut tool_turns = Vec::new();
    for message in bodies {
        match message.body {
            AgentMessageBody::AssistantDelta { .. } => tool_turns.push("text"),
            AgentMessageBody::ToolUse { id, .. } => {
                assert_eq!(id, "t");
                tool_turns.push("tool");
            }
            AgentMessageBody::ToolResult { id, .. } => {
                assert_eq!(id, "t");
                tool_turns.push("output");
            }
            AgentMessageBody::Result { .. } => tool_turns.push("result"),
            _ => {}
        }
    }
    assert_eq!(
        tool_turns,
        ["text", "tool", "output", "result", "text", "tool", "output", "result"]
    );
    a.stop().await.unwrap();
}

struct FailingSpawn;
#[async_trait::async_trait]
impl bondsymphonic_daemon::sandbox::SandboxHandle for FailingSpawn {
    async fn spawn(
        &self,
        _cmd: bondsymphonic_daemon::sandbox::SandboxCommand,
    ) -> Result<bondsymphonic_daemon::sandbox::SandboxChild, RpcError> {
        let mut error = RpcError::new(
            ErrorCode::SandboxError,
            "spawn failed secret-key native-secret header-secret",
        );
        error.data = Some(json!({"detail":"native-secret header-secret"}));
        Err(error)
    }
    async fn shutdown(&self) -> Result<(), RpcError> {
        Ok(())
    }
    fn helper_exe(&self) -> std::path::PathBuf {
        "unused".into()
    }
}

#[tokio::test]
async fn spawn_errors_redact_every_registered_secret() {
    let (_d, mut a, _e, _s, _) =
        fixture_with_handle("normal", None, "local/test", Some(Arc::new(FailingSpawn))).await;
    let error = a.start().await.unwrap_err();
    assert_eq!(error.code, ErrorCode::SandboxError);
    let recorded = serde_json::to_string(&error).unwrap();
    for secret in ["secret-key", "native-secret", "header-secret"] {
        assert!(!recorded.contains(secret), "leaked {secret}");
    }
}

#[tokio::test]
async fn structured_tool_results_redact_fields_and_escaped_secrets_before_serializing() {
    let (d, mut a, e, s, events) = fixture("structured-output", None, "local/test").await;
    let mut rx = events.subscribe();
    a.start().await.unwrap();
    a.send("edit".into()).await.unwrap();
    wait(&e, AgentState::Idle).await;

    let exact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.path().join("structured.txt")).unwrap())
            .unwrap();
    assert_eq!(exact["authorization"], "unregistered-token");
    assert_eq!(exact["nested"]["apiKey"], "unregistered-key");
    assert_eq!(exact["nested"]["password"], "unregistered-password");
    assert_eq!(exact["registered"], r#"escaped-"token\slash"#);

    let stored = s.read(&"ag_acp".into()).await.unwrap();
    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerMessage::Event {
            event: Event::AgentMessage { message, .. },
            ..
        } = event
        {
            published.push(message);
        }
    }
    assert_eq!(published, stored);
    let result = stored
        .iter()
        .find_map(|message| match &message.body {
            AgentMessageBody::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .expect("completed tool result");
    let presented: serde_json::Value = serde_json::from_str(result).unwrap();
    assert_eq!(presented["rawOutput"]["authorization"], "[redacted]");
    assert_eq!(presented["rawOutput"]["nested"]["apiKey"], "[redacted]");
    assert_eq!(presented["rawOutput"]["nested"]["password"], "[redacted]");
    assert_eq!(presented["rawOutput"]["registered"], "[redacted]");
    assert_eq!(presented["rawInput"]["password"], "[redacted]");
    assert_eq!(presented["rawInput"]["registered"], "[redacted]");
    a.stop().await.unwrap();
}
