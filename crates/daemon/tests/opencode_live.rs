//! The OpenCode adapter against the real CLI, with the probe's mock model
//! (`fixtures/opencode/probe-1.14.48/scripts/mockllm.py`) as its provider.
//! Ignored: it needs the pinned binary. In WSL:
//!
//! ```sh
//! BS_OPENCODE_BIN=~/.local/bin/opencode cargo test -p bondsymphonic-daemon \
//!     --test opencode_live -- --ignored --test-threads 1
//! ```
#![cfg(unix)]
use bondsymphonic_daemon::{
    agents::{
        adapter::AgentAdapter, backend::PreparedAgent, opencode::OpenCodeAdapter, AgentEntry,
        AgentSink, TranscriptStore,
    },
    sandbox::{noop::NoopBackend, SandboxBackend, SandboxSpec},
    server::broadcast::EventBus,
};
use bondsymphonic_proto::*;
use std::{
    path::Path,
    process::{Child, Command},
    sync::Arc,
    time::Duration,
};

const AGENT: &str = "bondsymphonic_ws_live";

/// The mock model, killed when dropped.
struct Mock(Child);
impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn mock(dir: &Path, status: Option<&str>) -> (Mock, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut command = Command::new("python3");
    command.args([
        format!(
            "{}/tests/fixtures/opencode/probe-1.14.48/scripts/mockllm.py",
            env!("CARGO_MANIFEST_DIR")
        ),
        port.to_string(),
        dir.join("mock.jsonl").display().to_string(),
    ]);
    if let Some(status) = status {
        command.env("MOCK_STATUS", status);
    }
    let child = Mock(command.spawn().unwrap());
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (child, port);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock model did not start");
}

struct Live {
    _mock: Mock,
    _dir: tempfile::TempDir,
    adapter: OpenCodeAdapter,
    entry: Arc<AgentEntry>,
    store: Arc<TranscriptStore>,
}

async fn live(status: Option<&str>, stall_after: Duration) -> Option<Live> {
    let Ok(bin) = std::env::var("BS_OPENCODE_BIN") else {
        eprintln!("BS_OPENCODE_BIN unset; skipping");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let (mock, port) = mock(dir.path(), status);
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&work)
        .status();
    let ws = WorkspaceId("opencode-live".into());
    let entry = Arc::new(AgentEntry::new(ws.clone(), AgentAdapterKind::OpenCode));
    let store = Arc::new(TranscriptStore::new(dir.path().join("transcripts")));
    let sink = AgentSink::new(
        EventBus::new(100),
        store.clone(),
        AgentId("agent".into()),
        ws.clone(),
        entry.clone(),
    );
    let handle = NoopBackend
        .start(&SandboxSpec {
            id: ws,
            rw_binds: vec![],
            ro_binds: vec![],
            late_ro_binds: vec![],
            home: dir.path().join("home"),
            run_dir: dir.path().join("run"),
            env: vec![],
            cwd: work.clone(),
        })
        .await
        .unwrap();
    let config = serde_json::json!({
        "agent": {AGENT: {"description":"BondSymphonic workspace agent","mode":"primary","permission":{"*":"ask"}}},
        "default_agent": AGENT,
        "provider": {"mock": {"npm":"@ai-sdk/openai-compatible","name":"Mock","options":{"baseURL":format!("http://127.0.0.1:{port}/v1"),"apiKey":"dummy-not-secret"},"models":{"m1":{"name":"M1","tool_call":true}}}},
        "model": "mock/m1",
        "small_model": "mock/m1",
    });
    let root = dir.path().join("oc");
    let mut env: Vec<(String, String)> = ["config", "data", "cache", "state"]
        .iter()
        .map(|kind| {
            let path = root.join(kind);
            std::fs::create_dir_all(&path).unwrap();
            (
                format!("XDG_{}_HOME", kind.to_uppercase()),
                path.display().to_string(),
            )
        })
        .collect();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    env.extend([
        ("HOME".into(), home.display().to_string()),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
        ("OPENCODE_CONFIG_CONTENT".into(), config.to_string()),
    ]);
    let prepared = PreparedAgent {
        argv: vec![bin, "acp".into()],
        env,
        cwd: work,
        options: serde_json::from_value(serde_json::json!({"model":"mock/m1"})).unwrap(),
        mcp_servers: vec![],
    };
    Some(Live {
        _mock: mock,
        adapter: OpenCodeAdapter::new(sink, handle, prepared)
            .with_timeout(Duration::from_secs(120))
            .with_stall_after(stall_after),
        _dir: dir,
        entry,
        store,
    })
}

impl Live {
    async fn wait_for(
        &self,
        predicate: impl Fn(&[AgentMessageBody]) -> bool,
    ) -> Vec<AgentMessageBody> {
        let waited = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let messages: Vec<_> = self
                    .store
                    .read(&AgentId("agent".into()))
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|m| m.body)
                    .collect();
                if predicate(&messages) {
                    return messages;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match waited {
            Ok(messages) => messages,
            Err(_) => panic!(
                "timed out in state {:?}; transcript: {:#?}",
                self.entry.state(),
                self.store.read(&AgentId("agent".into())).await.unwrap()
            ),
        }
    }

    async fn results(&self, n: usize) -> Vec<AgentMessageBody> {
        self.wait_for(|m| {
            m.iter()
                .filter(|m| matches!(m, AgentMessageBody::Result { .. }))
                .count()
                >= n
        })
        .await
    }
}

fn notices(messages: &[AgentMessageBody]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessageBody::System { subtype, data } if subtype == "opencode_notice" => {
                data["text"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}

/// A bash call asked about, allowed and run; the reply finalised; a streaming
/// turn cancelled and reported as such.
#[tokio::test]
#[ignore = "needs BS_OPENCODE_BIN and python3"]
async fn real_opencode_runs_an_approved_tool_and_reports_a_cancel() {
    let Some(mut live) = live(None, Duration::from_secs(20)).await else {
        return;
    };
    live.adapter.start().await.unwrap();
    live.adapter
        .send("Run ls -la with bash, then say DONE.".into())
        .await
        .unwrap();
    let messages = live
        .wait_for(|m| {
            m.iter()
                .any(|m| matches!(m, AgentMessageBody::PermissionRequest { .. }))
        })
        .await;
    let request = messages
        .iter()
        .find_map(|m| match m {
            AgentMessageBody::PermissionRequest {
                request_id, input, ..
            } => {
                assert_eq!(input["command"], "ls -la", "{input}");
                Some(request_id.clone())
            }
            _ => None,
        })
        .unwrap();
    live.adapter
        .permission_reply(request, PermissionDecision::Allow, None, None)
        .await
        .unwrap();
    let messages = live.results(1).await;
    assert!(messages.iter().any(
        |m| matches!(m, AgentMessageBody::ToolUse { input, .. } if input["command"] == "ls -la")
    ));
    assert!(messages
        .iter()
        .any(|m| matches!(m, AgentMessageBody::ToolResult { is_error: false, output, .. } if output.contains(".git"))));
    assert!(messages
        .iter()
        .any(|m| matches!(m, AgentMessageBody::AssistantText { text } if text == "DONE")));
    assert_eq!(notices(&messages), Vec::<String>::new());

    live.adapter
        .send("NOTOOL SLOWSTREAM count".into())
        .await
        .unwrap();
    live.wait_for(|m| {
        m.iter()
            .any(|m| matches!(m, AgentMessageBody::AssistantDelta { text } if text == "2 "))
    })
    .await;
    live.adapter.interrupt().await.unwrap();
    let messages = live.results(2).await;
    let cancelled = messages.iter().rev().find_map(|m| match m {
        AgentMessageBody::System { subtype, data } if subtype == "opencode_turn" => {
            Some(data["stopReason"].clone())
        }
        _ => None,
    });
    assert_eq!(cancelled, Some(serde_json::json!("cancelled")));
    assert_eq!(notices(&messages), Vec::<String>::new());
    live.adapter.stop().await.unwrap();
}

/// A provider that rejects the key ends the prompt with nothing; the user is
/// told why, from OpenCode's own log.
#[tokio::test]
#[ignore = "needs BS_OPENCODE_BIN and python3"]
async fn real_opencode_explains_a_rejected_key() {
    let Some(mut live) = live(Some("401"), Duration::from_secs(20)).await else {
        return;
    };
    live.adapter.start().await.unwrap();
    live.adapter.send("NOTOOL hi".into()).await.unwrap();
    let messages = live.results(1).await;
    let notices = notices(&messages);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("Incorrect API key provided"),
        "{notices:?}"
    );
    live.adapter.stop().await.unwrap();
}

/// A provider answering 500 is retried for ever without a frame. The silence
/// is flagged with the logged cause, and an interrupt still ends the turn.
#[tokio::test]
#[ignore = "needs BS_OPENCODE_BIN and python3"]
async fn real_opencode_retrying_a_failing_provider_is_flagged_and_interruptible() {
    let Some(mut live) = live(Some("500"), Duration::from_secs(3)).await else {
        return;
    };
    live.adapter.start().await.unwrap();
    live.adapter.send("NOTOOL hi".into()).await.unwrap();
    let messages = live.wait_for(|m| !notices(m).is_empty()).await;
    let notice = &notices(&messages)[0];
    assert!(notice.contains("No response from the model"), "{notice}");
    assert!(notice.contains("HTTP 500"), "{notice}");
    live.adapter.interrupt().await.unwrap();
    let messages = live.results(1).await;
    let stop = messages.iter().rev().find_map(|m| match m {
        AgentMessageBody::System { subtype, data } if subtype == "opencode_turn" => {
            Some(data["stopReason"].clone())
        }
        _ => None,
    });
    assert_eq!(stop, Some(serde_json::json!("cancelled")));
    live.adapter.stop().await.unwrap();
}
