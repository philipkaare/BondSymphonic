use bondsymphonic_proto::*;

#[test]
fn backend_selection_is_additive_and_old_replies_still_decode() {
    let params: ListModelsParams = serde_json::from_str("{}").unwrap();
    assert_eq!(params.adapter, None);
    let capabilities: Capabilities = serde_json::from_str(
        r#"{"sandbox_backend":"noop","git_protect":false,"adapters":["claude","terminal"]}"#,
    )
    .unwrap();
    assert!(capabilities.backends.is_empty());
    let request: Request =
        serde_json::from_str(r#"{"method":"system.list_models","params":{"adapter":"codex"}}"#)
            .unwrap();
    assert!(matches!(
        request,
        Request::SystemListModels(ListModelsParams {
            adapter: Some(AgentAdapterKind::Codex),
            ..
        })
    ));
}

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
            init_if_missing: false,
            in_place: false,
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
        warnings: vec![],
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

/// `agent_records` was added after the first daemons shipped, and both
/// directions of the version skew have to keep working.
///
/// Forward: a client built against this crate reads an older daemon's
/// `WorkspaceInfo`, which has `agents` and no `agent_records`. The reply must
/// parse, with the records empty — which a client reads as "nothing is known
/// about these agents", not as "there are none".
///
/// Backward: an older client reads a newer daemon's answer. `agents` is still
/// the same array of ids it always was, so the field it looks at is unchanged,
/// and the extra key beside it is ignored the way serde ignores any unknown
/// field. That is why this is a second list rather than a changed one.
#[test]
fn workspace_info_carries_agent_records_beside_the_plain_ids() {
    let old = r#"{
        "id":"ws_1","name":"alpha","repo_path":"/r","base_branch":"main",
        "branch":"bs/alpha/work","worktree_path":"/wt","created_at":"t",
        "allowlist":[],"state":"ready","agents":["ag_1"],"runs":[]
    }"#;
    let info: WorkspaceInfo = serde_json::from_str(old).expect("an older daemon's reply parses");
    assert_eq!(info.agents, vec![AgentId("ag_1".into())]);
    assert!(
        info.agent_records.is_empty(),
        "a daemon that does not send them leaves them empty"
    );

    let full = WorkspaceInfo {
        agents: vec![AgentId("ag_1".into())],
        agent_records: vec![AgentSummary {
            id: AgentId("ag_1".into()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: Some("sess-1".into()),
            command: None,
            model: Some("claude-opus-5".into()),
            permission_mode: Some("acceptEdits".into()),
        }],
        ..info
    };
    let v = serde_json::to_value(&full).unwrap();
    // What an older client reads is untouched: the same key, the same array of
    // bare id strings.
    assert_eq!(v["agents"], serde_json::json!(["ag_1"]));
    assert_eq!(v["agent_records"][0]["adapter"], "claude");
    assert_eq!(v["agent_records"][0]["state"], "exited");
    assert_eq!(v["agent_records"][0]["session_id"], "sess-1");
    // The one field that must never appear, however the daemon built the record.
    assert!(
        v["agent_records"][0].get("api_key").is_none(),
        "the API key has no field to travel in: {v}"
    );
    let back: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(back, full);

    // An unset option is left out rather than written as null, so a record for
    // an agent started with nothing is `{id, adapter, state}`.
    let bare = WorkspaceInfo {
        agent_records: vec![AgentSummary {
            id: AgentId("ag_1".into()),
            adapter: AgentAdapterKind::Terminal,
            state: AgentState::Idle,
            session_id: None,
            command: None,
            model: None,
            permission_mode: None,
        }],
        ..full
    };
    let v = serde_json::to_value(&bare).unwrap();
    let record = v["agent_records"][0].as_object().unwrap();
    let mut keys: Vec<&str> = record.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["adapter", "id", "state"], "{v}");
    let back: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(back, bare);
}

/// The Milestone 7 version gate. `hello` carries `protocol_version` in both
/// directions, and both directions of the version skew keep working.
///
/// Forward: a pre-M7 daemon answers `hello` without the field, and a pre-M7
/// client sends one without it. Neither is a decode error; an absent field
/// means the peer speaks version 1, which is the version everything before
/// this spoke.
///
/// Backward: an older peer reading a newer one's `hello` sees one extra key
/// beside the ones it knows and ignores it, which is why this is an added
/// field rather than a changed one.
#[test]
fn hello_carries_the_protocol_version_in_both_directions() {
    let params: HelloParams =
        serde_json::from_str(r#"{"token":"t","client_version":"0.1.0"}"#).expect("pre-M7 hello");
    assert_eq!(params.protocol_version, None);
    assert_eq!(peer_protocol_version(params.protocol_version), 1);

    let result: HelloResult = serde_json::from_str(
        r#"{"daemon_version":"0.1.0","capabilities":{"sandbox_backend":"noop",
            "git_protect":false,"adapters":[]}}"#,
    )
    .expect("pre-M7 hello result");
    assert_eq!(result.protocol_version, None);
    assert_eq!(peer_protocol_version(result.protocol_version), 1);

    let params = HelloParams {
        token: "t".into(),
        client_version: "0.1.0".into(),
        protocol_version: Some(PROTOCOL_VERSION),
    };
    let v = serde_json::to_value(&params).unwrap();
    assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
    let back: HelloParams = serde_json::from_value(v).unwrap();
    assert_eq!(back, params);

    let result = HelloResult {
        protocol_version: Some(PROTOCOL_VERSION),
        ..result
    };
    let v = serde_json::to_value(&result).unwrap();
    assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
    let back: HelloResult = serde_json::from_value(v).unwrap();
    assert_eq!(back, result);
}

/// The mismatch error is built on one side and read on the other, so its shape
/// is pinned here rather than in either of them.
#[test]
fn the_protocol_mismatch_error_carries_both_versions() {
    let err = RpcError::protocol_mismatch(1, 99);
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert_eq!(
        err.message,
        "protocol version 99 is not supported (daemon speaks 1)"
    );
    let data = err.data.clone().expect("the reason travels in data");
    assert_eq!(data["reason"], PROTOCOL_MISMATCH_REASON);
    assert_eq!(data["daemon"], 1);
    assert_eq!(data["client"], 99);
    assert_eq!(protocol_mismatch_versions(&err), Some((1, 99)));

    // Any other error, however it is shaped, is not a mismatch.
    assert_eq!(protocol_mismatch_versions(&RpcError::unauthorized()), None);
    assert_eq!(
        protocol_mismatch_versions(
            &RpcError::invalid_params("nope").with_data(serde_json::json!({"reason": "other"}))
        ),
        None
    );
}

/// Added by Task 1 so the daemon can fill it in Task 2: a reply from a daemon
/// that predates the field carries no warnings, which is "nothing was ignored"
/// rather than a broken answer.
#[test]
fn detect_run_configs_defaults_its_warnings_to_empty() {
    let old: DetectRunConfigsResult =
        serde_json::from_str(r#"{"configs":[],"network_allow":[]}"#).unwrap();
    assert!(old.warnings.is_empty());

    let result = DetectRunConfigsResult {
        warnings: vec!["bondsymphonic.toml: [[run]] \"web\" has no port".into()],
        ..old
    };
    let v = serde_json::to_value(&result).unwrap();
    assert_eq!(
        v["warnings"][0],
        "bondsymphonic.toml: [[run]] \"web\" has no port"
    );
    let back: DetectRunConfigsResult = serde_json::from_value(v).unwrap();
    assert_eq!(back, result);
}

/// Both halves of the additive change this pass makes, checked the only way
/// that matters: against the bytes a peer of the *previous* vintage sends.
///
/// A client built before `init_if_missing` existed sends `workspace.create`
/// without the key, and the daemon must read that as "do not create anything" —
/// the conservative half, since the other reading would have an old IDE
/// initialising repositories nobody asked for.
#[test]
fn workspace_create_from_a_client_that_predates_init_if_missing() {
    let back: Request = serde_json::from_str(
        r#"{"method":"workspace.create","params":{"repo_path":"/r","base_branch":"main","name":"a"}}"#,
    )
    .unwrap();
    let Request::WorkspaceCreate(p) = back else {
        panic!("wrong variant")
    };
    assert!(
        !p.init_if_missing,
        "an old client asks for a workspace in a repository that already exists"
    );
}

/// And a daemon built before `repo.inspect` could answer for a non-repository
/// only ever answered *about* one: it failed outright otherwise. So its answer
/// has to decode as `is_repo: true`, or a new IDE reading an old daemon would
/// offer to initialise every repository the user picked.
///
/// `exists` defaults the other way on purpose: it is only meaningful together
/// with `is_repo: false`, and `false` is what an old daemon knew about it.
#[test]
fn repo_info_from_a_daemon_that_predates_the_non_repo_answer() {
    let info: RepoInfo = serde_json::from_str(
        r#"{"default_branch":"main","branches":["main"],"is_dirty":false,"remotes":[]}"#,
    )
    .unwrap();
    assert!(info.is_repo, "an old daemon only ever answered for a repo");
    assert!(!info.exists);

    let v = serde_json::to_value(RepoInfo {
        default_branch: "main".into(),
        branches: vec![],
        is_dirty: false,
        remotes: vec![],
        is_repo: false,
        exists: true,
        head_branch: None,
        in_place_refusal: None,
        hooks_path_in_tree: None,
    })
    .unwrap();
    assert_eq!(v["is_repo"], false);
    assert_eq!(v["exists"], true);
}

/// In-place workspaces, 2026-09-17: the second kind of workspace, and the
/// version bump that keeps an older daemon from quietly making a worktree.
mod in_place_protocol {
    use bondsymphonic_proto::*;

    #[test]
    fn the_protocol_is_version_two_and_a_silent_peer_is_version_one() {
        assert_eq!(PROTOCOL_VERSION, 2);
        assert_eq!(peer_protocol_version(None), 1);
    }

    #[test]
    fn a_workspace_kind_is_spelled_in_snake_case_and_defaults_to_worktree() {
        assert_eq!(
            serde_json::to_value(WorkspaceKind::InPlace).unwrap(),
            "in_place"
        );
        assert_eq!(
            serde_json::to_value(WorkspaceKind::Worktree).unwrap(),
            "worktree"
        );
        assert_eq!(WorkspaceKind::default(), WorkspaceKind::Worktree);
        // A registry or a reply written before the field existed.
        let old = r#"{
            "id":"ws_1","name":"alpha","repo_path":"/r","base_branch":"main",
            "branch":"bs/alpha/work","worktree_path":"/wt","created_at":"t",
            "allowlist":[],"state":"ready","agents":[],"runs":[]
        }"#;
        let info: WorkspaceInfo = serde_json::from_str(old).unwrap();
        assert_eq!(info.kind, WorkspaceKind::Worktree);
        let v = serde_json::to_value(WorkspaceInfo {
            kind: WorkspaceKind::InPlace,
            ..info
        })
        .unwrap();
        assert_eq!(v["kind"], "in_place");
    }

    #[test]
    fn a_create_without_in_place_is_a_worktree_create() {
        let p: WorkspaceCreateParams =
            serde_json::from_str(r#"{"repo_path":"/r","base_branch":"main","name":"a"}"#).unwrap();
        assert!(!p.in_place);
        let p: WorkspaceCreateParams = serde_json::from_str(
            r#"{"repo_path":"/r","base_branch":"","name":"a","in_place":true}"#,
        )
        .unwrap();
        assert!(p.in_place);
    }

    #[test]
    fn repo_info_without_the_in_place_fields_reads_as_none() {
        let info: RepoInfo = serde_json::from_str(
            r#"{"default_branch":"main","branches":["main"],"is_dirty":false,"remotes":[]}"#,
        )
        .unwrap();
        assert_eq!(info.head_branch, None);
        assert_eq!(info.in_place_refusal, None);
        assert_eq!(info.hooks_path_in_tree, None);
    }
}
