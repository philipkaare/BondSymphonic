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

/// The drop notice is a cross-crate contract: the daemon builds it, the IDE
/// reads it back off the wire, and neither of them writes the wording down.
/// The pair has to survive the serialisation in between, and has to refuse
/// anything that is not the notice.
#[test]
fn the_drop_notice_round_trips_through_the_wire_form() {
    let json = serde_json::to_string(&Event::events_dropped(37)).unwrap();
    let back: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(back.dropped_event_count(), Some(37));
    // The wire text is what a human reads in the log, so it is asserted once,
    // here, rather than in each crate that handles the notice.
    assert!(
        json.contains("events dropped: 37"),
        "unexpected wire text: {json}"
    );

    // An ordinary log line at the same level is not a drop notice, whatever it
    // says, and neither is the same text at another level.
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: "sandbox is down".into(),
            host: None,
        }
        .dropped_event_count(),
        None
    );
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Info,
            message: format!("{EVENT_DROP_PREFIX}5"),
            host: None,
        }
        .dropped_event_count(),
        None
    );
    assert_eq!(
        Event::PtyExit {
            pty_id: "pty_1".into(),
            code: 0,
        }
        .dropped_event_count(),
        None
    );
}

/// The denial notice is the same cross-crate contract as the drop notice: the
/// daemon's proxy builds it, the IDE turns it into a toast with an "Allow host"
/// action, and neither of them writes the wording down. The host has to survive
/// the wire in the field the IDE actually reads, and anything that is not a
/// denial has to come back as one.
#[test]
fn the_network_denial_round_trips_through_the_wire_form() {
    let json = serde_json::to_string(&Event::network_denied("evil.example")).unwrap();
    let back: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(back.denied_host(), Some("evil.example"));
    // The wire text is what a human reads in the log, asserted once, here.
    assert!(
        json.contains("network denied: evil.example"),
        "unexpected wire text: {json}"
    );

    // A plain warning is not a denial, whatever host it happens to carry, and
    // neither is the denial text at another level.
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Warn,
            message: "sandbox is down".into(),
            host: Some("evil.example".into()),
        }
        .denied_host(),
        None
    );
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Info,
            message: format!("{NETWORK_DENIED_PREFIX}evil.example"),
            host: Some("evil.example".into()),
        }
        .denied_host(),
        None
    );
    assert_eq!(Event::events_dropped(1).denied_host(), None);
    assert_eq!(
        Event::network_denied("evil.example").dropped_event_count(),
        None
    );
}

/// `detail` was added to `run.state` after the event shipped, and it is absent
/// on every ordinary transition, so it has to be optional in both directions:
/// a daemon that does not send it must still deserialise, and a state change
/// without one must not put a null on the wire.
#[test]
fn run_state_detail_is_optional_in_both_directions() {
    let old: Event =
        serde_json::from_str(r#"{"kind":"run.state","run_id":"run_1","state":"ready"}"#).unwrap();
    assert_eq!(
        old,
        Event::RunStateChanged {
            run_id: "run_1".into(),
            state: RunState::Ready,
            url: None,
            detail: None,
        }
    );
    let v = serde_json::to_value(&old).unwrap();
    assert!(v.get("detail").is_none(), "{v}");
    assert!(v.get("url").is_none(), "{v}");

    let failed = Event::RunStateChanged {
        run_id: "run_1".into(),
        state: RunState::Failed,
        url: None,
        detail: Some("exited with status 1".into()),
    };
    let back: Event = serde_json::from_str(&serde_json::to_string(&failed).unwrap()).unwrap();
    assert_eq!(back, failed);
}

#[test]
fn diff_result_defaults_truncated_to_false() {
    // The field was added after the first daemons shipped, so a reply without
    // it has to keep deserialising rather than failing the whole diff.
    let old: DiffResult = serde_json::from_str(r#"{"base_text":"a\n","work_text":"b\n"}"#).unwrap();
    assert_eq!(old.base_text, "a\n");
    assert!(!old.truncated);

    let cut = DiffResult {
        base_text: "a\n".into(),
        work_text: "b\n".into(),
        truncated: true,
    };
    let v = serde_json::to_value(&cut).unwrap();
    assert_eq!(v["truncated"], true);
    let back: DiffResult = serde_json::from_value(v).unwrap();
    assert_eq!(back, cut);
}

/// The one struct in the protocol that carries the user's Anthropic API key
/// must not be able to print it. Nothing formats these options today, which is
/// exactly why this test exists: a `#[derive(Debug)]` added back here would
/// leak the key the first time anyone put the request in a tracing field.
#[test]
fn agent_start_options_debug_redacts_the_api_key() {
    let options = AgentStartOptions {
        command: None,
        resume_session: Some("sess-9".into()),
        model: Some("sonnet".into()),
        permission_mode: Some("default".into()),
        api_key: Some("sk-ant-secret-value".into()),
    };
    let printed = format!("{options:?}");
    assert!(
        !printed.contains("sk-ant-secret-value"),
        "the key must not survive Debug: {printed}"
    );
    assert!(printed.contains("api_key: Some(<redacted>)"), "{printed}");
    // The rest is still legible, and whether a key is set is still visible.
    assert!(printed.contains("sonnet"), "{printed}");
    let without = AgentStartOptions {
        api_key: None,
        ..options.clone()
    };
    assert!(format!("{without:?}").contains("api_key: None"));
    // Serialisation is untouched: only the human-readable form is redacted.
    let v = serde_json::to_value(&options).unwrap();
    assert_eq!(v["api_key"], "sk-ant-secret-value");
}

/// The repository's own network additions travel with the detected run
/// configurations, so the New Agent dialog can name them before the workspace
/// that would inherit them exists.
#[test]
fn detect_run_configs_carries_the_repo_network_allow() {
    // Added after the first daemons shipped: a reply without the field is a
    // repository that adds nothing, not a broken answer.
    let old: DetectRunConfigsResult = serde_json::from_str(r#"{"configs":[]}"#).unwrap();
    assert!(old.configs.is_empty());
    assert!(old.network_allow.is_empty());

    let result = DetectRunConfigsResult {
        configs: vec![RunConfig {
            name: "web".into(),
            command: "npm run dev".into(),
            port: 5173,
            port_guessed: true,
            cwd: None,
            env: Default::default(),
            ready_regex: None,
            source: RunConfigSource::Detected,
            disabled_reason: None,
        }],
        network_allow: vec!["assets.example.test".into(), "*.example.test".into()],
    };
    let v = serde_json::to_value(&result).unwrap();
    assert_eq!(v["network_allow"][0], "assets.example.test");
    let back: DetectRunConfigsResult = serde_json::from_value(v).unwrap();
    assert_eq!(back, result);
}

#[test]
fn merge_result_defaults_reason_to_none() {
    // `reason` distinguishes a conflict from the other ways a merge can come
    // back not-ok, and it was added after `MergeResult` first shipped, so a
    // reply without it has to keep deserialising.
    let old: MergeResult = serde_json::from_str(r#"{"ok":true,"conflicts":[]}"#).unwrap();
    assert!(old.ok);
    assert_eq!(old.reason, None);

    let conflicted = MergeResult {
        ok: false,
        conflicts: vec!["README.md".into()],
        reason: Some("conflict".into()),
    };
    let v = serde_json::to_value(&conflicted).unwrap();
    assert_eq!(v["reason"], "conflict");
    assert_eq!(v["conflicts"][0], "README.md");
    let back: MergeResult = serde_json::from_value(v).unwrap();
    assert_eq!(back, conflicted);
}

#[test]
fn run_start_defaults_its_port_override_to_none() {
    // `port` was added after `run.start` shipped, so a request from a client
    // that never learned about it has to keep deserialising -- and mean "the
    // port the configuration names".
    let old: Request = serde_json::from_str(
        r#"{"method":"run.start","params":{"workspace_id":"ws_1","config_name":"web"}}"#,
    )
    .unwrap();
    let Request::RunStart(p) = old else {
        panic!("expected run.start");
    };
    assert_eq!(p.port, None);
    assert_eq!(p.config_name, "web");

    let overridden = Request::RunStart(RunStartParams {
        workspace_id: WorkspaceId("ws_1".into()),
        config_name: "web".into(),
        port: Some(4321),
    });
    let v = serde_json::to_value(&overridden).unwrap();
    assert_eq!(v["params"]["port"], 4321);
    let back: Request = serde_json::from_value(v).unwrap();
    assert_eq!(back, overridden);
}
