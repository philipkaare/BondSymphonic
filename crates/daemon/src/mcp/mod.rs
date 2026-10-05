//! Brokered git and GitHub tools for sandboxed agents, served over MCP.

pub mod bridge;
pub mod protocol;
pub mod registry;
pub mod tools;

/// The name this server announces to MCP clients.
pub const SERVER_NAME: &str = "bondsymphonic";
/// The socket's file name inside a workspace's runtime directory.
pub const SOCKET_FILE: &str = "mcp.sock";
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/bs/mcp.sock";

/// Where the daemon binary appears inside the sandbox. Must agree with
/// `sandbox::linux_bwrap::DAEMON_IN_SANDBOX`.
pub const DAEMON_IN_SANDBOX: &str = "/opt/bs/daemon";

/// The arguments that make the daemon binary act as the stdio-to-socket bridge.
pub fn bridge_args() -> [&'static str; 3] {
    ["mcp-bridge", "--socket", SOCKET_IN_SANDBOX]
}

/// Claude Code's `--mcp-config` value: one stdio server running the bridge.
pub fn claude_mcp_config() -> String {
    serde_json::json!({"mcpServers": {SERVER_NAME: {
        "type": "stdio",
        "command": DAEMON_IN_SANDBOX,
        "args": bridge_args(),
    }}})
    .to_string()
}

/// Claude Code's `--allowedTools` value: the read-only tools run without a
/// permission prompt; the ones that write to a remote still ask.
pub fn claude_allowed_tools() -> String {
    tools::READ_ONLY_TOOLS
        .iter()
        .map(|t| format!("mcp__{SERVER_NAME}__{t}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Codex's `-c` overrides that register the same bridge server. Values are
/// TOML, so strings are quoted and the args are an array.
pub fn codex_config() -> Vec<String> {
    let args = bridge_args()
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "-c".into(),
        format!("mcp_servers.{SERVER_NAME}.command=\"{DAEMON_IN_SANDBOX}\""),
        "-c".into(),
        format!("mcp_servers.{SERVER_NAME}.args=[{args}]"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_names_the_bridge() {
        let v: serde_json::Value = serde_json::from_str(&claude_mcp_config()).unwrap();
        let s = &v["mcpServers"]["bondsymphonic"];
        assert_eq!(s["command"], DAEMON_IN_SANDBOX);
        assert_eq!(s["args"], serde_json::json!(bridge_args()));
        assert_eq!(
            claude_mcp_config(),
            r#"{"mcpServers":{"bondsymphonic":{"args":["mcp-bridge","--socket","/run/bs/mcp.sock"],"command":"/opt/bs/daemon","type":"stdio"}}}"#
        );
        assert_eq!(
            claude_allowed_tools(),
            "mcp__bondsymphonic__git_fetch,mcp__bondsymphonic__pr_view,mcp__bondsymphonic__ci_logs,mcp__bondsymphonic__issue_view"
        );
    }

    #[test]
    fn codex_config_is_toml_overrides() {
        assert_eq!(
            codex_config(),
            [
                "-c",
                "mcp_servers.bondsymphonic.command=\"/opt/bs/daemon\"",
                "-c",
                "mcp_servers.bondsymphonic.args=[\"mcp-bridge\",\"--socket\",\"/run/bs/mcp.sock\"]"
            ]
        );
    }
}
