use bondsymphonic_proto::*;

#[test]
fn ids_serialize_transparently() {
    let id = WorkspaceId("ws_0a1b2c3d".into());
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"ws_0a1b2c3d\"");
    let back: WorkspaceId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, id);
    assert_eq!(id.to_string(), "ws_0a1b2c3d");
}

#[test]
fn rpc_error_serializes_code_as_pascal_case() {
    let e =
        RpcError::new(ErrorCode::GitError, "boom").with_data(serde_json::json!({"exit_code": 128}));
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["code"], "GitError");
    assert_eq!(v["message"], "boom");
    assert_eq!(v["data"]["exit_code"], 128);
    let back: RpcError = serde_json::from_value(v).unwrap();
    assert_eq!(back.code, ErrorCode::GitError);
}

#[test]
fn request_envelope_shape() {
    let msg = ClientMessage::Request {
        id: 17,
        request: Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: "/home/bs/repo".into(),
            base_branch: "main".into(),
            name: "agent-1".into(),
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "request");
    assert_eq!(v["id"], 17);
    assert_eq!(v["method"], "workspace.create");
    assert_eq!(v["params"]["name"], "agent-1");
    let back: ClientMessage = serde_json::from_value(v).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn unit_like_request_has_empty_params() {
    let v = serde_json::to_value(Request::SystemCheckPrereqs {}).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"method": "system.check_prereqs", "params": {}})
    );
    let back: Request =
        serde_json::from_str(r#"{"method":"system.check_prereqs","params":{}}"#).unwrap();
    assert_eq!(back, Request::SystemCheckPrereqs {});
}

#[test]
fn response_and_error_envelopes() {
    let ok = ServerMessage::Response {
        id: 1,
        result: Some(serde_json::json!({"x": 1})),
        error: None,
    };
    let v = serde_json::to_value(&ok).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"type":"response","id":1,"result":{"x":1}})
    );
    let err = ServerMessage::Response {
        id: 2,
        result: None,
        error: Some(RpcError::unauthorized()),
    };
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v["error"]["code"], "Unauthorized");
    assert!(v.get("result").is_none());
}

#[test]
fn event_envelope_and_kinds() {
    let ev = ServerMessage::Event {
        workspace_id: Some("ws_1".into()),
        event: Event::AgentStateChanged {
            agent_id: "ag_1".into(),
            state: AgentState::WaitingPermission,
            detail: None,
        },
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["type"], "event");
    assert_eq!(v["workspace_id"], "ws_1");
    assert_eq!(v["event"]["kind"], "agent.state");
    assert_eq!(v["event"]["state"], "waiting_permission");
    let back: ServerMessage = serde_json::from_value(v).unwrap();
    assert_eq!(back, ev);
    let log = Event::DaemonLog {
        level: LogLevel::Warn,
        message: "denied".into(),
        host: Some("evil.example".into()),
    };
    assert_eq!(serde_json::to_value(&log).unwrap()["kind"], "daemon.log");
}

#[test]
fn codec_roundtrip_and_errors() {
    let line = codec::encode(&ClientMessage::Request {
        id: 3,
        request: Request::SystemShutdown {},
    });
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    let back: ClientMessage = codec::decode(line.trim_end()).unwrap();
    assert!(matches!(back, ClientMessage::Request { id: 3, .. }));
    assert!(codec::decode::<ClientMessage>("not json").is_err());
}

#[test]
fn every_request_variant_roundtrips() {
    for req in Request::examples() {
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req, "variant {}", req.method_name());
        assert!(json.contains(&format!("\"method\":\"{}\"", req.method_name())));
    }
}

#[test]
fn every_event_variant_roundtrips() {
    for ev in Event::examples() {
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }
}
