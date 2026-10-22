//! The Qt-free half of the MCP servers settings: which `mcp.list` reply the
//! page shows, how the server editor's write-only secret fields become
//! `McpSecretWrite`s, and the loopback listener an OAuth sign-in comes back to.

use bondsymphonic_ide::client::oauth_callback::{
    self, parse_callback_target, CallbackOutcome, CALLBACK_PATH,
};
use bondsymphonic_ide::model::mcp::{
    effective_in_workspace, upsert_params, McpView, OAuthAttempts, SecretField, ServerForm,
};
use bondsymphonic_proto::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn server(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: id.into(),
        transport: McpTransportConfig::Http {
            url: "https://example.com/mcp".into(),
            auth: McpAuthConfig::Bearer {
                token: "token".into(),
            },
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

fn listing(revision: u64, state: McpConnectionState) -> McpListResult {
    McpListResult {
        registry: McpRegistrySnapshot {
            revision,
            servers: vec![server("jira")],
            overrides: Default::default(),
        },
        statuses: vec![McpServerStatus {
            server_id: "jira".into(),
            state,
            reason: None,
        }],
    }
}

#[test]
fn mcp_status_ignores_stale_reply() {
    let mut view = McpView::default();
    // Two `mcp.changed` events (revision 2, then a duplicate or reordered 1)
    // each re-list; the replies can come back in either order.
    let first = view.begin_list();
    let second = view.begin_list();
    assert!(view.apply_list(second, listing(2, McpConnectionState::Connected)));
    assert!(
        !view.apply_list(first, listing(1, McpConnectionState::Disconnected)),
        "a reply with an older revision is dropped"
    );
    assert_eq!(view.revision(), Some(2));
    assert_eq!(
        view.listing().unwrap().statuses[0].state,
        McpConnectionState::Connected
    );

    // A status-only change keeps the revision: a later request's reply with
    // the same revision still updates the statuses...
    let third = view.begin_list();
    assert!(view.apply_list(third, listing(2, McpConnectionState::Error)));
    assert_eq!(
        view.listing().unwrap().statuses[0].state,
        McpConnectionState::Error
    );
    // ...but an earlier request's reply with that revision, arriving late,
    // does not put the older statuses back.
    let fourth = view.begin_list();
    let fifth = view.begin_list();
    assert!(view.apply_list(fifth, listing(2, McpConnectionState::Connected)));
    assert!(!view.apply_list(fourth, listing(2, McpConnectionState::Error)));
    assert_eq!(
        view.listing().unwrap().statuses[0].state,
        McpConnectionState::Connected
    );

    // A degraded registry answers with an error naming registry.json: the page
    // shows it instead of the list, and a later good listing replaces it.
    let failed = view.begin_list();
    assert!(view.apply_error(failed, "io: cannot read registry.json".into()));
    let json: serde_json::Value = serde_json::from_str(&view.to_json()).unwrap();
    assert_eq!(json["error"], "io: cannot read registry.json");
    assert!(json.get("registry").is_none());
    let recovered = view.begin_list();
    assert!(view.apply_list(recovered, listing(3, McpConnectionState::Connected)));
    let json: serde_json::Value = serde_json::from_str(&view.to_json()).unwrap();
    assert!(json.get("error").is_none());
    assert_eq!(json["registry"]["revision"], 3);

    // A new connection starts over: the next daemon's revision is the truth.
    view.reset();
    let after = view.begin_list();
    assert!(view.apply_list(after, listing(1, McpConnectionState::Disconnected)));
    assert_eq!(view.revision(), Some(1));
}

#[test]
fn mcp_secret_editor_is_write_only() {
    let form = |secrets: Vec<SecretField>| ServerForm {
        server: server("jira"),
        secrets,
        expected_revision: Some(4),
    };
    // Blank is Keep: the editor never had the stored value to show.
    let params = upsert_params(form(vec![SecretField {
        reference: "token".into(),
        value: String::new(),
        remove: false,
    }]))
    .unwrap();
    assert_eq!(
        params.secrets,
        vec![McpSecretWrite::Keep {
            reference: "token".into()
        }]
    );
    assert_eq!(params.expected_revision, Some(4));
    // Whitespace is blank too, as the daemon reads it.
    let params = upsert_params(form(vec![SecretField {
        reference: "token".into(),
        value: "  ".into(),
        remove: false,
    }]))
    .unwrap();
    assert!(matches!(params.secrets[0], McpSecretWrite::Keep { .. }));
    // A typed value is Set, and Remove wins over a typed value.
    let params = upsert_params(form(vec![SecretField {
        reference: "token".into(),
        value: "s3cr3t-sentinel".into(),
        remove: false,
    }]))
    .unwrap();
    assert!(matches!(&params.secrets[0],
        McpSecretWrite::Set { reference, value } if reference.as_str() == "token" && value.0 == "s3cr3t-sentinel"));
    assert!(
        !format!("{params:?}").contains("s3cr3t-sentinel"),
        "Debug never prints a secret"
    );
    let params = upsert_params(form(vec![SecretField {
        reference: "token".into(),
        value: "s3cr3t-sentinel".into(),
        remove: true,
    }]))
    .unwrap();
    assert_eq!(
        params.secrets,
        vec![McpSecretWrite::Remove {
            reference: "token".into()
        }]
    );

    // The listing the page is drawn from carries references, never values.
    let mut view = McpView::default();
    let ticket = view.begin_list();
    view.apply_list(ticket, listing(1, McpConnectionState::Connected));
    assert!(!view.to_json().contains("s3cr3t-sentinel"));

    // The checks the daemon would make, made before anything is sent.
    let mut bad = server("Bad Id");
    let error = upsert_params(ServerForm {
        server: bad.clone(),
        secrets: vec![],
        expected_revision: None,
    })
    .unwrap_err();
    assert!(error.contains("ID"), "{error}");
    bad.id = "bondsymphonic".into();
    let error = upsert_params(ServerForm {
        server: bad,
        secrets: vec![],
        expected_revision: None,
    })
    .unwrap_err();
    assert!(error.contains("reserved"), "{error}");
    let mut stdio = server("local");
    stdio.transport = McpTransportConfig::Stdio {
        command: "npx".into(),
        args: vec![],
        env: Default::default(),
        secret_env: vec!["NOT-VALID".into()],
    };
    let error = upsert_params(ServerForm {
        server: stdio,
        secrets: vec![],
        expected_revision: None,
    })
    .unwrap_err();
    assert!(error.contains("NOT-VALID"), "{error}");
}

#[test]
fn workspace_override_resolves_like_the_daemon() {
    let mut on = server("on");
    on.default_workspace_enabled = true;
    let mut off = server("off");
    off.default_workspace_enabled = false;
    assert!(effective_in_workspace(&on, McpWorkspaceOverride::Inherit));
    assert!(!effective_in_workspace(&off, McpWorkspaceOverride::Inherit));
    assert!(effective_in_workspace(&off, McpWorkspaceOverride::Enabled));
    assert!(!effective_in_workspace(&on, McpWorkspaceOverride::Disabled));
    // Globally off is off everywhere, whatever the workspace says.
    on.enabled = false;
    assert!(!effective_in_workspace(&on, McpWorkspaceOverride::Enabled));
}

#[test]
fn callback_target_parses_code_state_and_issuer() {
    assert_eq!(CALLBACK_PATH, "/callback");
    assert_eq!(
        parse_callback_target("/callback?code=a%2Bb&state=xyz&iss=https%3A%2F%2Fauth.example"),
        CallbackOutcome::Code {
            state: "xyz".into(),
            code: "a+b".into(),
            issuer: Some("https://auth.example".into()),
        }
    );
    assert_eq!(
        parse_callback_target("/callback?error=access_denied&state=xyz"),
        CallbackOutcome::Denied {
            state: "xyz".into(),
            error: "access_denied".into(),
        }
    );
    assert_eq!(
        parse_callback_target("/favicon.ico"),
        CallbackOutcome::NotCallback
    );
    assert_eq!(
        parse_callback_target("/callback?code=only"),
        CallbackOutcome::NotCallback
    );
}

#[test]
fn callback_bound_to_active_attempt() {
    // The pure half: an attempt takes exactly one callback, with its state.
    let mut attempts = OAuthAttempts::default();
    let first = attempts.begin("jira");
    assert!(attempts.set_state(first, "state-1".into()));
    assert_eq!(attempts.take(first, "unknown"), None, "unknown state");
    assert_eq!(attempts.take(first, "state-1").as_deref(), Some("jira"));
    assert_eq!(attempts.take(first, "state-1"), None, "second callback");
    // A newer attempt replaces an older one, whose callback is then ignored.
    let old = attempts.begin("jira");
    attempts.set_state(old, "old".into());
    let new = attempts.begin("other");
    attempts.set_state(new, "new".into());
    assert!(!attempts.is_active(old));
    assert_eq!(attempts.take(old, "old"), None);
    assert!(!attempts.set_state(old, "late".into()));
    assert_eq!(attempts.take(new, "new").as_deref(), Some("other"));
    // Cancelled is gone.
    let cancelled = attempts.begin("jira");
    attempts.set_state(cancelled, "c".into());
    attempts.cancel(cancelled);
    assert_eq!(attempts.take(cancelled, "c"), None);

    // The listener: a request with an unknown state is answered and ignored,
    // the attempt's own is answered with the close-this-tab page and ends it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (listener, port) = oauth_callback::bind().await.unwrap();
        assert_eq!(
            oauth_callback::redirect_uri(port),
            format!("http://127.0.0.1:{port}/callback")
        );
        let waiting = tokio::spawn(oauth_callback::wait_for_callback(
            listener,
            Duration::from_secs(20),
            |state: &str| state == "good",
        ));
        let ask = |target: &'static str| async move {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            socket
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).await.unwrap();
            reply
        };
        let reply = ask("/callback?code=c&state=bad").await;
        assert!(reply.contains("no longer active"), "{reply}");
        let reply = ask("/favicon.ico").await;
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        let reply = ask("/callback?code=c&state=good").await;
        assert!(reply.contains("You can close this tab"), "{reply}");
        let outcome = waiting.await.unwrap();
        assert_eq!(
            outcome,
            Some(CallbackOutcome::Code {
                state: "good".into(),
                code: "c".into(),
                issuer: None,
            })
        );
        // The listener is closed after the callback: nothing answers a second.
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err());

        // And it gives up at the deadline.
        let (listener, _) = oauth_callback::bind().await.unwrap();
        let outcome =
            oauth_callback::wait_for_callback(listener, Duration::from_millis(50), |_| true).await;
        assert_eq!(outcome, None);
    });
}
