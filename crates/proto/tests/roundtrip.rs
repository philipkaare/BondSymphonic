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
        backend: None,
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
            backend: None,
        }
        .dropped_event_count(),
        None
    );
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Info,
            message: format!("{EVENT_DROP_PREFIX}5"),
            host: None,
            backend: None,
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
            backend: None,
        }
        .denied_host(),
        None
    );
    assert_eq!(
        Event::DaemonLog {
            level: LogLevel::Info,
            message: format!("{NETWORK_DENIED_PREFIX}evil.example"),
            host: Some("evil.example".into()),
            backend: None,
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
        features: vec![],
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
    fn the_protocol_is_version_three_and_a_silent_peer_is_version_one() {
        const _: () = assert!(PROTOCOL_VERSION >= 3);
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

/// A denial names the sandbox it came from, so the IDE can put it in the
/// transcript of the agent that tried rather than in a toast nobody connects
/// with it. The field is additive in both directions: an older daemon's
/// denial still reads (as one from nobody in particular), and a denial with no
/// attribution puts nothing on the wire an older IDE would trip over.
#[test]
fn a_network_denial_carries_its_backend_and_stays_compatible() {
    let ev = Event::network_denied_by("evil.example", AgentAdapterKind::OpenCode);
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["backend"], "opencode");
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, ev);
    assert_eq!(back.denied_host(), Some("evil.example"));
    assert_eq!(back.denial_backend(), Some(AgentAdapterKind::OpenCode));

    // An older daemon's denial: no field, no attribution, still a denial.
    let old: Event = serde_json::from_str(
        r#"{"kind":"daemon.log","level":"warn","message":"network denied: evil.example","host":"evil.example"}"#,
    )
    .unwrap();
    assert_eq!(old.denied_host(), Some("evil.example"));
    assert_eq!(old.denial_backend(), None);
    let json = serde_json::to_string(&Event::network_denied("evil.example")).unwrap();
    assert!(!json.contains("backend"), "{json}");

    // The tunnel refusal keeps its two-line wording and offers nothing to
    // allow, but now says which host and which backend, so the IDE can show it
    // beside the agent too.
    let tunnel = Event::tunnel_refused("ollama.lan", AgentAdapterKind::OpenCode);
    let json = serde_json::to_string(&tunnel).unwrap();
    let back: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(back.denied_host(), None);
    assert_eq!(back.refused_tunnel_host(), Some("ollama.lan"));
    assert_eq!(back.denial_backend(), Some(AgentAdapterKind::OpenCode));
    match &back {
        Event::DaemonLog { level, message, .. } => {
            assert_eq!(*level, LogLevel::Warn);
            assert_eq!(
                message,
                "The agent's connection to ollama.lan was refused\n\
                 ollama.lan is not a model server in the agent's own config, \
                 and only the servers it names can be reached this way."
            );
        }
        other => panic!("{other:?}"),
    }
    // Neither recogniser mistakes the other notice, or a plain warning.
    assert_eq!(ev.refused_tunnel_host(), None);
    assert_eq!(Event::events_dropped(1).denial_backend(), None);
}

/// `agent.start_progress` is additive: a client opts in through `hello`, and
/// the event names its phase and a label the IDE can show as it stands.
mod agent_start_progress {
    use bondsymphonic_proto::*;

    #[test]
    fn the_progress_event_round_trips_in_its_envelope() {
        let msg = ServerMessage::event(
            Some("ws_1".into()),
            Event::AgentStartProgress {
                adapter: AgentAdapterKind::OpenCode,
                phase: start_phase::SANDBOX.into(),
                label: "Starting the sandbox".into(),
            },
        );
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["workspace_id"], "ws_1");
        assert_eq!(v["event"]["kind"], "agent.start_progress");
        assert_eq!(v["event"]["adapter"], "opencode");
        assert_eq!(v["event"]["phase"], "sandbox");
        assert_eq!(v["event"]["label"], "Starting the sandbox");
        let back: ServerMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
        assert!(Event::examples()
            .iter()
            .any(|e| matches!(e, Event::AgentStartProgress { .. })));
    }

    #[test]
    fn only_the_progress_event_needs_the_opt_in() {
        let progress = Event::AgentStartProgress {
            adapter: AgentAdapterKind::Codex,
            phase: start_phase::LAUNCH.into(),
            label: String::new(),
        };
        assert_eq!(
            progress.opt_in_feature(),
            Some(FEATURE_AGENT_START_PROGRESS)
        );
        for ev in Event::examples() {
            if !matches!(ev, Event::AgentStartProgress { .. }) {
                assert_eq!(ev.opt_in_feature(), None, "{ev:?}");
            }
        }
    }

    #[test]
    fn hello_features_are_optional_in_both_directions() {
        // A client that predates the field.
        let old: HelloParams =
            serde_json::from_str(r#"{"token":"t","client_version":"0.1.0","protocol_version":3}"#)
                .unwrap();
        assert!(old.features.is_empty());
        // And one that has nothing to ask for sends nothing, so an older
        // daemon sees the hello it always saw.
        let v = serde_json::to_value(&old).unwrap();
        assert!(v.get("features").is_none(), "{v}");
        let new = HelloParams {
            features: vec![FEATURE_AGENT_START_PROGRESS.into()],
            ..old
        };
        let v = serde_json::to_value(&new).unwrap();
        assert_eq!(v["features"][0], "agent.start_progress");
        assert!(new.wants(FEATURE_AGENT_START_PROGRESS));
        let back: HelloParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, new);
    }
}

/// Actionable OpenCode notices, queued prompts and token usage, 2026-10-21:
/// all additive, so a daemon and an IDE of either vintage still read each
/// other.
mod notices_queue_usage {
    use bondsymphonic_proto::*;
    use serde_json::json;

    #[test]
    fn a_notice_carries_its_kind_actions_and_log_and_an_old_one_is_plain_text() {
        let notice = AgentNotice {
            text: "No response".into(),
            kind: NoticeKind::Stall,
            actions: vec![NoticeAction::Interrupt, NoticeAction::ShowLog],
            log: vec!["ERROR provider".into()],
        };
        let wire = serde_json::to_value(&notice).unwrap();
        assert_eq!(
            wire,
            json!({"text":"No response","kind":"stall","actions":["interrupt","show_log"],"log":["ERROR provider"]})
        );
        assert_eq!(serde_json::from_value::<AgentNotice>(wire).unwrap(), notice);
        // What a daemon before this change wrote.
        let old: AgentNotice = serde_json::from_value(json!({"text":"x"})).unwrap();
        assert_eq!(old.kind, NoticeKind::Other);
        assert!(old.actions.is_empty() && old.log.is_empty());
        // A kind or an action from a newer daemon is not an error.
        let newer: AgentNotice = serde_json::from_value(
            json!({"text":"x","kind":"brand_new","actions":["open_config_folder","teleport"]}),
        )
        .unwrap();
        assert_eq!(newer.kind, NoticeKind::Other);
        assert_eq!(
            newer.actions,
            vec![NoticeAction::OpenConfigFolder, NoticeAction::Unknown]
        );
        let body = notice.clone().into_message();
        assert!(
            matches!(&body, AgentMessageBody::System { subtype, .. } if subtype == NOTICE_SUBTYPE)
        );
        assert_eq!(AgentNotice::from_message(&body), Some(notice));
    }

    #[test]
    fn queued_prompts_and_their_outcomes_round_trip() {
        let queued = QueuedPrompt {
            id: "q1".into(),
            text: "two".into(),
        };
        let body = queued.clone().into_message();
        assert_eq!(QueuedPrompt::from_message(&body), Some(queued));
        let done = DequeuedPrompt {
            id: "q1".into(),
            outcome: DequeueOutcome::Discarded,
        };
        let wire = serde_json::to_value(done.clone().into_message()).unwrap();
        assert_eq!(
            wire,
            json!({"type":"system","subtype":"prompt_dequeued","data":{"id":"q1","outcome":"discarded"}})
        );
        let back: AgentMessageBody = serde_json::from_value(wire).unwrap();
        assert_eq!(DequeuedPrompt::from_message(&back), Some(done));
        let request: Request = serde_json::from_value(json!({
            "method":"agent.queued_prompt",
            "params":{"agent_id":"ag_1","prompt_id":"q1","action":"send_now"}
        }))
        .unwrap();
        assert_eq!(request.method_name(), "agent.queued_prompt");
        assert!(matches!(
            request,
            Request::AgentQueuedPrompt(AgentQueuedPromptParams {
                action: QueuedPromptAction::SendNow,
                ..
            })
        ));
    }

    #[test]
    fn turn_usage_leaves_out_what_the_backend_did_not_report() {
        let usage = TurnUsage {
            input_tokens: Some(5),
            output_tokens: Some(3),
            ..TurnUsage::default()
        };
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"input_tokens":5,"output_tokens":3})
        );
        let empty: TurnUsage = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, TurnUsage::default());
        assert!(empty.is_empty());
        assert!(!usage.is_empty());
        // Read off the row a Codex or OpenCode turn ends with.
        let row = AgentMessageBody::System {
            subtype: "opencode_turn".into(),
            data: json!({"stopReason":"end_turn","turn_usage":{"input_tokens":5,"output_tokens":3}}),
        };
        assert_eq!(TurnUsage::from_turn_row(&row), Some(usage));
        let old_row = AgentMessageBody::System {
            subtype: "codex_turn".into(),
            data: json!({"status":"completed","usage":{"total":{}}}),
        };
        assert_eq!(TurnUsage::from_turn_row(&old_row), None);
    }
}

/// Send now or Discard on a queued message the worker has just taken is a
/// race, not a failure: the refusal says so in `data.reason`, which survives
/// the wire, so the IDE can let it pass without parsing the message.
#[test]
fn a_settled_queued_prompt_is_recognisable_after_the_wire() {
    let err = RpcError::queued_prompt_settled();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    let back: RpcError = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
    assert_eq!(
        back.data.as_ref().unwrap()["reason"],
        QUEUED_PROMPT_SETTLED_REASON
    );
    assert!(is_queued_prompt_settled(&back));
    assert!(!is_queued_prompt_settled(&RpcError::invalid_params(
        "That message was already sent or discarded"
    )));
    assert!(!is_queued_prompt_settled(&RpcError::protocol_mismatch(
        4, 3
    )));
}

#[test]
fn network_rule_edits_and_block_decisions_round_trip() {
    for (method, params) in [
        (
            "workspace.set_network_rules",
            serde_json::json!({
                "workspace_id": "ws_1", "allow": ["*.example.com"], "block": ["api.example.com"]
            }),
        ),
        (
            "workspace.block_host",
            serde_json::json!({"workspace_id": "ws_1", "host": "api.example.com"}),
        ),
    ] {
        let wire = serde_json::json!({"method": method, "params": params});
        let request: Request = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(request.method_name(), method);
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
    }
}

#[test]
fn commit_and_ignore_contracts_roundtrip() {
    let id: bondsymphonic_proto::WorkspaceId = "ws_edit".into();
    let requests = [
        bondsymphonic_proto::Request::WorkspacePrepareCommit(
            bondsymphonic_proto::WorkspaceIdParams {
                workspace_id: id.clone(),
            },
        ),
        bondsymphonic_proto::Request::WorkspaceCommit(bondsymphonic_proto::WorkspaceCommitParams {
            workspace_id: id.clone(),
            paths: vec!["[literal]*.rs".into()],
            message: "Commit selected".into(),
        }),
        bondsymphonic_proto::Request::WorkspaceIgnoreOptions(
            bondsymphonic_proto::WorkspaceIgnoreOptionsParams {
                workspace_id: id.clone(),
                path: "src/file.rs".into(),
                directory: false,
            },
        ),
        bondsymphonic_proto::Request::WorkspaceIgnore(bondsymphonic_proto::WorkspaceIgnoreParams {
            workspace_id: id,
            path: ".".into(),
            target: bondsymphonic_proto::IgnoreTarget::Directory,
        }),
    ];
    for request in requests {
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["method"], request.method_name());
        assert_eq!(
            serde_json::from_value::<bondsymphonic_proto::Request>(wire).unwrap(),
            request
        );
    }
    assert_eq!(
        serde_json::to_value(bondsymphonic_proto::IgnoreTarget::FileType).unwrap(),
        "file_type"
    );
}

#[test]
fn mcp_changed_event_roundtrips_with_its_revision() {
    let event = bondsymphonic_proto::Event::McpChanged { revision: 7 };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["kind"], "mcp.changed");
    assert_eq!(wire["revision"], 7);
    assert_eq!(
        serde_json::from_value::<bondsymphonic_proto::Event>(wire).unwrap(),
        event
    );
}
