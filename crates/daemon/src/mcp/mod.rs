//! Brokered git and GitHub tools for sandboxed agents, served over MCP.

pub mod protocol;
pub mod tools;

/// The name this server announces to MCP clients.
pub const SERVER_NAME: &str = "bondsymphonic";
/// The socket's file name inside a workspace's runtime directory.
pub const SOCKET_FILE: &str = "mcp.sock";
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/bs/mcp.sock";
