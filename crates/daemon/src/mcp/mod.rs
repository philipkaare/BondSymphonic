//! Brokered git and GitHub tools for sandboxed agents, served over MCP.

pub mod bridge;
pub mod exec;
pub mod native;
pub mod protocol;
pub mod registry;
pub mod servers;
pub mod tools;

use bondsymphonic_proto::{
    AgentAdapterKind, McpServerConfig, McpServerId, McpTransportConfig, WorkspaceId,
};

/// The name this server announces to MCP clients.
pub const SERVER_NAME: &str = "bondsymphonic";
/// The socket's file name inside a workspace's runtime directory.
pub const SOCKET_FILE: &str = "mcp.sock";
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/bs/mcp.sock";

/// A user-configured remote server's socket file inside a sandbox's runtime
/// directory, next to [`SOCKET_FILE`]. The server id's alphabet
/// (`[a-z0-9_-]`) keeps it one path component, and the name alone binds a
/// connection to its sandbox and server: no token or preamble is needed.
pub fn server_socket_file(id: &bondsymphonic_proto::McpServerId) -> String {
    format!("mcp-{}.sock", id.as_str())
}

/// Where [`server_socket_file`] appears inside the sandbox.
pub fn server_socket_in_sandbox(id: &bondsymphonic_proto::McpServerId) -> String {
    format!("/run/bs/{}", server_socket_file(id))
}

/// Where the daemon binary appears inside the sandbox: the path
/// `sandbox::linux_bwrap` binds it at, and the command the agents' MCP config
/// runs as the bridge.
pub const DAEMON_IN_SANDBOX: &str = "/opt/bs/daemon";

/// The arguments that make the daemon binary act as the stdio-to-socket bridge.
pub fn bridge_args() -> [&'static str; 3] {
    ["mcp-bridge", "--socket", SOCKET_IN_SANDBOX]
}

/// A user-configured server as an agent's MCP config names it: under its
/// server id, run as a stdio command inside the sandbox. Built from one
/// resolved list per (workspace, backend) at agent start, so all three CLIs
/// get the same servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    pub id: McpServerId,
    pub command: String,
    pub args: Vec<String>,
}

impl ResolvedServer {
    /// What the agent runs for `cfg`: the bridge to the server's socket for a
    /// remote server, and the server's own command under `mcp-exec` for a
    /// local one, so its secrets come from the env file and never an argv.
    pub fn for_config(cfg: &McpServerConfig) -> Self {
        let args = match &cfg.transport {
            McpTransportConfig::Http { .. } => vec![
                "mcp-bridge".into(),
                "--socket".into(),
                server_socket_in_sandbox(&cfg.id),
            ],
            McpTransportConfig::Stdio { command, args, .. } => [
                "mcp-exec".into(),
                "--env-file".into(),
                exec::env_file_in_sandbox(&cfg.id),
                "--".into(),
                command.clone(),
            ]
            .into_iter()
            .chain(args.iter().cloned())
            .collect(),
        };
        Self {
            id: cfg.id.clone(),
            command: DAEMON_IN_SANDBOX.into(),
            args,
        }
    }
}

/// Claude Code's `--mcp-config` value: one stdio server running the bridge,
/// and one per user-configured server.
pub fn claude_mcp_config(servers: &[ResolvedServer]) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        SERVER_NAME.into(),
        serde_json::json!({
            "type": "stdio",
            "command": DAEMON_IN_SANDBOX,
            "args": bridge_args(),
        }),
    );
    for s in servers {
        map.insert(
            s.id.as_str().into(),
            serde_json::json!({"type": "stdio", "command": s.command, "args": s.args}),
        );
    }
    serde_json::json!({ "mcpServers": map }).to_string()
}

/// Claude Code's `--allowedTools` value: the read-only tools run without a
/// permission prompt; the ones that write to a remote still ask. A
/// user-configured server's tools are never listed: the CLI's own permission
/// prompt governs them.
pub fn claude_allowed_tools() -> String {
    tools::READ_ONLY_TOOLS
        .iter()
        .map(|t| format!("mcp__{SERVER_NAME}__{t}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Codex's `-c` overrides that register the same bridge server, and the
/// user-configured ones. Values are TOML, so strings are quoted and the args
/// are an array; a server id (`[a-z0-9_-]`) is a valid bare key as it is.
pub fn codex_config(servers: &[ResolvedServer]) -> Vec<String> {
    let args = bridge_args()
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(",");
    let mut config = vec![
        "-c".into(),
        format!("mcp_servers.{SERVER_NAME}.command=\"{DAEMON_IN_SANDBOX}\""),
        "-c".into(),
        format!("mcp_servers.{SERVER_NAME}.args=[{args}]"),
    ];
    for s in servers {
        // A local server's command and args are the user's own strings, so
        // they go through the TOML encoder rather than a format string.
        let command = toml::Value::String(s.command.clone());
        let args = toml::Value::Array(s.args.iter().cloned().map(toml::Value::String).collect());
        let id = s.id.as_str();
        config.extend([
            "-c".into(),
            format!("mcp_servers.{id}.command={command}"),
            "-c".into(),
            format!("mcp_servers.{id}.args={args}"),
        ]);
    }
    config
}

/// OpenCode's ACP `session/new` `mcpServers`: the bridge server first, then
/// the user-configured ones. Their environment is always empty: a local
/// server's comes from its env file.
pub fn opencode_mcp_servers(servers: &[ResolvedServer]) -> serde_json::Value {
    let builtin = serde_json::json!({
        "name": SERVER_NAME,
        "command": DAEMON_IN_SANDBOX,
        "args": bridge_args(),
        "env": [],
    });
    serde_json::Value::Array(
        std::iter::once(builtin)
            .chain(servers.iter().map(|s| {
                serde_json::json!({"name": s.id.as_str(), "command": s.command, "args": s.args, "env": []})
            }))
            .collect(),
    )
}

/// `configs` without those a native config file of the agent's already names
/// (or that one could not be checked against): see [`native`].
fn without_native_collisions(
    configs: Vec<McpServerConfig>,
    native: &[native::NativeConfig],
) -> Vec<McpServerConfig> {
    configs
        .into_iter()
        .filter(|cfg| match native::check_native_collision(native, cfg.id.as_str()) {
            Ok(()) => true,
            Err(why) => {
                tracing::warn!(server = %cfg.id.as_str(), "MCP server not given to the agent: {why}");
                false
            }
        })
        .collect()
}

/// Gets the user's MCP servers ready for an agent of `backend` in `ws` whose
/// sandbox has its runtime directory at `run_dir`, and returns them for the
/// agent's MCP config. The set is resolved here, at agent start: a remote
/// server gets its socket (`mcp-<id>.sock`) started, a local one its env file
/// written.
///
/// Never fails the start. A server that cannot be prepared -- its socket will
/// not bind, its env file will not write, a native config already uses its
/// name -- is logged and left out, and the agent starts without it. A server
/// with no login yet is prepared like any other: its tools answer "not
/// connected" until the user connects it.
pub fn prepare_servers(
    d: &crate::daemon::Daemon,
    ws: &WorkspaceId,
    backend: AgentAdapterKind,
    run_dir: &std::path::Path,
) -> Vec<ResolvedServer> {
    let configs = d.mcp_servers.resolve(ws, backend);
    let configs = match d.workspace(ws) {
        Ok(w) => without_native_collisions(
            configs,
            &native::native_configs(&d.dirs.root, &w.worktree_path, backend),
        ),
        Err(e) => {
            tracing::warn!(ws = %ws, "MCP servers not given to the agent: {}", e.message);
            vec![]
        }
    };
    remove_stale_env_files(run_dir, &configs);
    if configs.is_empty() {
        return vec![];
    }
    if let Err(e) = std::fs::create_dir_all(run_dir) {
        tracing::warn!(ws = %ws, "MCP servers not given to the agent: {e}");
        return vec![];
    }
    configs
        .iter()
        .filter_map(|cfg| {
            let prepared = match &cfg.transport {
                McpTransportConfig::Http { .. } => d.mcp.start(
                    d.weak(),
                    ws,
                    &run_dir.join(server_socket_file(&cfg.id)),
                    registry::ListenerKind::Server(cfg.id.clone()),
                ),
                McpTransportConfig::Stdio { .. } => {
                    exec::write_env_file(&d.mcp_servers, cfg, run_dir).map(|_| ())
                }
            };
            match prepared {
                Ok(()) => Some(ResolvedServer::for_config(cfg)),
                Err(e) => {
                    tracing::warn!(ws = %ws, server = %cfg.id.as_str(), "MCP server not given to the agent: {}", e.message);
                    None
                }
            }
        })
        .collect()
}

/// [`prepare_servers`] for an agent of `backend` in `ws`, in the run
/// directory of the sandbox it runs in. Only a sandboxed agent gets the
/// user's servers: the bridge and `mcp-exec` exist only inside a sandbox, and
/// an unsandboxed agent has the user's own CLI config anyway.
pub fn prepare_agent_servers(
    d: &crate::daemon::Daemon,
    ws: &WorkspaceId,
    backend: AgentAdapterKind,
) -> Vec<ResolvedServer> {
    if d.backend.name() != crate::agents::claude::SANDBOXED_BACKEND {
        return vec![];
    }
    let run_dir = crate::workspace::agent_sandboxes::agent_run_dir(d, ws, backend);
    prepare_servers(d, ws, backend, &run_dir)
}

/// Deletes the env files in `run_dir` of local servers this agent start does
/// not give out -- removed, disabled, or no longer local -- so a secret does
/// not outlive its server's configuration in a sandbox's runtime directory.
/// Only this sandbox's own run directory, which only this backend uses.
fn remove_stale_env_files(run_dir: &std::path::Path, keep: &[McpServerConfig]) {
    let Ok(entries) = std::fs::read_dir(run_dir.join(exec::ENV_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let kept = keep.iter().any(|cfg| {
            matches!(cfg.transport, McpTransportConfig::Stdio { .. })
                && name.to_str() == Some(exec::env_file_name(&cfg.id).as_str())
        });
        if !kept {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::McpAuthConfig;

    #[test]
    fn claude_config_names_the_bridge() {
        let v: serde_json::Value = serde_json::from_str(&claude_mcp_config(&[])).unwrap();
        let s = &v["mcpServers"]["bondsymphonic"];
        assert_eq!(s["command"], DAEMON_IN_SANDBOX);
        assert_eq!(s["args"], serde_json::json!(bridge_args()));
        assert_eq!(
            claude_mcp_config(&[]),
            r#"{"mcpServers":{"bondsymphonic":{"args":["mcp-bridge","--socket","/run/bs/mcp.sock"],"command":"/opt/bs/daemon","type":"stdio"}}}"#
        );
        assert_eq!(
            claude_allowed_tools(),
            "mcp__bondsymphonic__git_fetch,mcp__bondsymphonic__pr_view,mcp__bondsymphonic__ci_logs,mcp__bondsymphonic__issue_view"
        );
    }

    #[test]
    fn server_sockets_sit_beside_the_tools_socket() {
        let id: bondsymphonic_proto::McpServerId = "jira".into();
        assert_eq!(server_socket_file(&id), "mcp-jira.sock");
        assert_eq!(server_socket_in_sandbox(&id), "/run/bs/mcp-jira.sock");
    }

    #[test]
    fn codex_config_is_toml_overrides() {
        assert_eq!(
            codex_config(&[]),
            [
                "-c",
                "mcp_servers.bondsymphonic.command=\"/opt/bs/daemon\"",
                "-c",
                "mcp_servers.bondsymphonic.args=[\"mcp-bridge\",\"--socket\",\"/run/bs/mcp.sock\"]"
            ]
        );
    }

    fn remote_jira() -> McpServerConfig {
        McpServerConfig {
            id: "jira".into(),
            name: "Jira".into(),
            transport: McpTransportConfig::Http {
                url: "https://mcp.atlassian.com/v2/mcp".into(),
                auth: McpAuthConfig::None,
            },
            enabled: true,
            default_workspace_enabled: true,
            enabled_backends: vec!["claude".into()],
        }
    }

    fn local_notes() -> McpServerConfig {
        McpServerConfig {
            id: "notes".into(),
            name: "Notes".into(),
            transport: McpTransportConfig::Stdio {
                command: "node".into(),
                args: vec!["server.js".into(), "say \"hi\"".into()],
                env: Default::default(),
                secret_env: vec!["TOKEN".into()],
            },
            enabled: true,
            default_workspace_enabled: true,
            enabled_backends: vec!["claude".into()],
        }
    }

    fn user_servers() -> Vec<ResolvedServer> {
        [remote_jira(), local_notes()]
            .iter()
            .map(ResolvedServer::for_config)
            .collect()
    }

    #[test]
    fn resolved_servers_run_the_bridge_or_mcp_exec() {
        let [jira, notes] = &user_servers()[..] else {
            panic!()
        };
        assert_eq!(jira.command, DAEMON_IN_SANDBOX);
        assert_eq!(
            jira.args,
            ["mcp-bridge", "--socket", "/run/bs/mcp-jira.sock"]
        );
        assert_eq!(notes.command, DAEMON_IN_SANDBOX);
        assert_eq!(
            notes.args,
            [
                "mcp-exec",
                "--env-file",
                "/run/bs/mcp-env/notes.env",
                "--",
                "node",
                "server.js",
                "say \"hi\""
            ]
        );
    }

    #[test]
    fn claude_config_lists_user_servers() {
        let v: serde_json::Value =
            serde_json::from_str(&claude_mcp_config(&user_servers())).unwrap();
        let jira = &v["mcpServers"]["jira"];
        assert_eq!(jira["type"], "stdio");
        assert_eq!(jira["command"], DAEMON_IN_SANDBOX);
        assert_eq!(
            jira["args"],
            serde_json::json!(["mcp-bridge", "--socket", "/run/bs/mcp-jira.sock"])
        );
        assert_eq!(v["mcpServers"]["notes"]["args"][4], "node");
        assert_eq!(
            v["mcpServers"]["bondsymphonic"]["args"],
            serde_json::json!(bridge_args())
        );
        // Discovered tools are the CLI's to prompt for.
        assert_eq!(
            claude_allowed_tools(),
            "mcp__bondsymphonic__git_fetch,mcp__bondsymphonic__pr_view,mcp__bondsymphonic__ci_logs,mcp__bondsymphonic__issue_view"
        );
    }

    #[test]
    fn codex_config_lists_user_servers() {
        let config = codex_config(&user_servers());
        assert_eq!(&config[..4], &codex_config(&[])[..]);
        // Each value parses back to what it says, quotes and all.
        let mut doc = String::new();
        for pair in config.chunks(2) {
            assert_eq!(pair[0], "-c");
            doc.push_str(&pair[1]);
            doc.push('\n');
        }
        let parsed: toml::Value = toml::from_str(&doc).unwrap();
        let servers = &parsed["mcp_servers"];
        assert_eq!(servers["jira"]["command"].as_str(), Some(DAEMON_IN_SANDBOX));
        assert_eq!(
            servers["jira"]["args"],
            toml::Value::try_from(["mcp-bridge", "--socket", "/run/bs/mcp-jira.sock"]).unwrap()
        );
        assert_eq!(servers["notes"]["args"][6].as_str(), Some("say \"hi\""));
    }

    #[test]
    fn opencode_servers_list_user_servers() {
        let servers = opencode_mcp_servers(&user_servers());
        assert_eq!(servers[0], opencode_mcp_servers(&[])[0]);
        assert_eq!(
            servers[1],
            serde_json::json!({"name":"jira","command":DAEMON_IN_SANDBOX,
                "args":["mcp-bridge","--socket","/run/bs/mcp-jira.sock"],"env":[]})
        );
        assert_eq!(servers[2]["name"], "notes");
        assert_eq!(servers.as_array().unwrap().len(), 3);
    }

    #[test]
    fn empty_servers_unchanged() {
        assert_eq!(
            claude_mcp_config(&[]),
            r#"{"mcpServers":{"bondsymphonic":{"args":["mcp-bridge","--socket","/run/bs/mcp.sock"],"command":"/opt/bs/daemon","type":"stdio"}}}"#
        );
        assert_eq!(codex_config(&[]).len(), 4);
        assert_eq!(
            opencode_mcp_servers(&[]).to_string(),
            r#"[{"args":["mcp-bridge","--socket","/run/bs/mcp.sock"],"command":"/opt/bs/daemon","env":[],"name":"bondsymphonic"}]"#
        );
    }

    #[test]
    fn native_name_collision_skips_product_entry() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".mcp.json"),
            r#"{"mcpServers":{"jira":{"command":"my-own-jira"}}}"#,
        )
        .unwrap();
        let native = native::native_configs(
            &dir.path().join("data"),
            &worktree,
            AgentAdapterKind::Claude,
        );
        let kept = without_native_collisions(vec![remote_jira(), local_notes()], &native);
        let ids: Vec<&str> = kept.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["notes"]);
    }
}
