//! The agent CLIs' own MCP configuration, read only to keep a user-configured
//! server from silently replacing an entry of the same name there.
//!
//! Product-managed servers are given to a CLI under their server id, next to
//! whatever the CLI loads natively. When a native file already names that id,
//! one of the two would win and the other vanish without a word, so the
//! product entry is left out instead (and logged): the user's own file is the
//! more specific choice, and renaming the product server is one edit away.

use bondsymphonic_proto::AgentAdapterKind;
use std::path::{Path, PathBuf};

/// The most of a native file that is read. These are hand-written configs;
/// anything bigger is not one, and is treated as unreadable.
const READ_LIMIT: usize = 1024 * 1024;

/// One native config file and the table in it that names MCP servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeConfig {
    pub path: PathBuf,
    /// The top-level key whose object holds the servers by name:
    /// `mcpServers` (Claude), `mcp_servers` (Codex), `mcp` (OpenCode).
    pub table: &'static str,
}

/// The native files an agent of `backend` reads its own MCP servers from:
///
/// - Claude: the worktree's `.mcp.json` (project scope).
/// - Codex: `config.toml` in the `CODEX_HOME` the daemon gives it.
/// - OpenCode: the mirrored config under the daemon's OpenCode home, which is
///   what the sandboxed `opencode` loads, and the worktree's own
///   `opencode.json`/`opencode.jsonc`, which it loads as project config.
pub fn native_configs(
    root: &Path,
    worktree: &Path,
    backend: AgentAdapterKind,
) -> Vec<NativeConfig> {
    let at = |path: PathBuf, table| NativeConfig { path, table };
    match backend {
        AgentAdapterKind::Claude => vec![at(worktree.join(".mcp.json"), "mcpServers")],
        AgentAdapterKind::Codex => vec![at(
            crate::agents::codex_backend::codex_home(root).join("config.toml"),
            "mcp_servers",
        )],
        AgentAdapterKind::OpenCode => {
            let mirror = crate::agents::opencode_backend::opencode_home(root)
                .join("config")
                .join("opencode");
            crate::agents::opencode_config::CONFIGS
                .iter()
                .map(|name| at(mirror.join(name), "mcp"))
                .chain(
                    ["opencode.json", "opencode.jsonc"]
                        .into_iter()
                        .map(|name| at(worktree.join(name), "mcp")),
                )
                .collect()
        }
        AgentAdapterKind::Terminal => vec![],
    }
}

/// `Err` when one of `files` names `name` in its server table, or cannot be
/// read or parsed -- then whether it does is unknown, and the product entry is
/// left out rather than risk replacing the user's. A missing file names
/// nothing. The error says which file, never what is in it.
///
/// Links are not followed: the worktree is the agent's to write, and a link
/// there could point the daemon at anything on the host.
pub fn check_native_collision(files: &[NativeConfig], name: &str) -> Result<(), String> {
    for file in files {
        let shown = file.path.display();
        let bytes = match crate::agents::opencode_config::read_regular_file(
            &file.path, READ_LIMIT, false,
        ) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{shown} could not be read: {e}")),
        };
        let unparsed = || format!("{shown} could not be parsed");
        let text = std::str::from_utf8(&bytes).map_err(|_| unparsed())?;
        let named = if file.path.extension().is_some_and(|e| e == "toml") {
            let value: toml::Value = toml::from_str(text).map_err(|_| unparsed())?;
            value
                .get(file.table)
                .and_then(|t| t.as_table())
                .is_some_and(|t| t.contains_key(name))
        } else {
            // Claude's `.mcp.json` is plain JSON, which JSONC is a superset of.
            let text = crate::agents::opencode_config::strip_jsonc(text);
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| unparsed())?;
            value
                .get(file.table)
                .and_then(|t| t.as_object())
                .is_some_and(|t| t.contains_key(name))
        };
        if named {
            return Err(format!("{shown} already has an MCP server named {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configs(dir: &Path, backend: AgentAdapterKind) -> Vec<NativeConfig> {
        native_configs(&dir.join("data"), &dir.join("worktree"), backend)
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_files_name_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for backend in [
            AgentAdapterKind::Claude,
            AgentAdapterKind::Codex,
            AgentAdapterKind::OpenCode,
        ] {
            assert_eq!(
                check_native_collision(&configs(dir.path(), backend), "jira"),
                Ok(())
            );
        }
    }

    #[test]
    fn each_backend_reads_its_own_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("worktree/.mcp.json"),
            r#"{"mcpServers":{"jira":{"command":"x"}},"other":{"codex":1}}"#,
        );
        write(
            &root.join("data/codex-home/config.toml"),
            "[mcp_servers.codex]\ncommand = \"x\"\n[other.jira]\n",
        );
        write(
            &root.join("data/opencode-host/config/opencode/opencode.jsonc"),
            "{ // mine\n \"mcp\": {\"oc\": {\"type\":\"local\"},},\n}",
        );
        let claude = configs(root, AgentAdapterKind::Claude);
        assert!(check_native_collision(&claude, "jira").is_err());
        assert_eq!(
            check_native_collision(&claude, "codex"),
            Ok(()),
            "only mcpServers counts"
        );
        let codex = configs(root, AgentAdapterKind::Codex);
        assert!(check_native_collision(&codex, "codex").is_err());
        assert_eq!(check_native_collision(&codex, "jira"), Ok(()));
        let opencode = configs(root, AgentAdapterKind::OpenCode);
        assert!(check_native_collision(&opencode, "oc").is_err());
        assert_eq!(check_native_collision(&opencode, "jira"), Ok(()));
    }

    #[test]
    fn an_unreadable_file_counts_as_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("worktree/.mcp.json"), "{ not json");
        let err = check_native_collision(&configs(dir.path(), AgentAdapterKind::Claude), "jira")
            .unwrap_err();
        assert!(err.contains(".mcp.json"), "{err}");
        assert!(!err.contains("not json"), "never the contents: {err}");
    }
}
