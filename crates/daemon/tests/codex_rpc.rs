use bondsymphonic_daemon::agents::codex_rpc::CodexRpc;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn calls_correlate_independently_of_server_requests_and_eof_fails_pending() {
    let (client, server) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(client);
    let (rpc, mut events) = CodexRpc::connect(
        Box::pin(reader),
        Box::pin(writer),
        vec!["sentinel".into()],
        Duration::from_secs(2),
    );
    let peer = tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(server);
        let mut lines = BufReader::new(r).lines();
        let first: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        w.write_all(b"{\"id\":0,\"method\":\"item/fileChange/requestApproval\",\"params\":{}}\n")
            .await
            .unwrap();
        w.write_all(
            format!(
                "{}\n",
                json!({"id":first["id"],"result":{"text":"sentinel"}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], 0);
        assert_eq!(reply["result"]["decision"], "accept");
        let _pending = lines.next_line().await.unwrap();
    });
    assert_eq!(
        rpc.call("initialize", json!({})).await.unwrap()["text"],
        "[redacted]"
    );
    assert_eq!(
        events.recv().await.unwrap()["method"],
        "item/fileChange/requestApproval"
    );
    rpc.reply(json!(0), json!({"decision":"accept"}))
        .await
        .unwrap();
    assert!(rpc.call("pending", json!({})).await.is_err());
    peer.await.unwrap();
    assert!(rpc.call("after_eof", json!({})).await.is_err());
}

#[tokio::test]
async fn unanswered_call_is_bounded() {
    let (client, _server) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(client);
    let (rpc, _events) = CodexRpc::connect(
        Box::pin(reader),
        Box::pin(writer),
        vec![],
        Duration::from_millis(30),
    );
    let error = rpc.call("initialize", json!({})).await.unwrap_err();
    assert!(error.message.contains("timed out"));
    rpc.close().await;
}

#[tokio::test]
async fn parallel_responses_route_by_id_and_auth_errors_are_backend_specific() {
    let (client, server) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(client);
    let (rpc, _events) = CodexRpc::connect(
        Box::pin(reader),
        Box::pin(writer),
        vec!["key-sentinel".into()],
        Duration::from_secs(2),
    );
    let peer = tokio::spawn(async move {
        let (r, mut w) = tokio::io::split(server);
        let mut lines = BufReader::new(r).lines();
        let first: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let second: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        w.write_all(
            format!(
                "{}\n",
                json!({"id":second["id"],"error":{"message":"401 Unauthorized key-sentinel"}})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        w.write_all(format!("{}\n", json!({"id":first["id"],"result":"first"})).as_bytes())
            .await
            .unwrap();
    });
    let (first, second) = tokio::join!(rpc.call("first", json!({})), rpc.call("second", json!({})));
    assert_eq!(first.unwrap(), "first");
    let failure = second.unwrap_err();
    assert_eq!(failure.code, bondsymphonic_proto::ErrorCode::Unauthorized);
    assert_eq!(failure.data.unwrap()["reason"], "codex_auth_failed");
    assert!(!failure.message.contains("key-sentinel"));
    peer.await.unwrap();
}
