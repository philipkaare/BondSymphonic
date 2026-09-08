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
