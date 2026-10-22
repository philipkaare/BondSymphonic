//! `mcp-exec`: how a sandbox-local MCP server gets its secrets without them
//! ever reaching an argv.
//!
//! At agent start the daemon writes the server's environment, secrets
//! included, to `<run_dir>/mcp-env/<id>.env` (`0600`, in a `0700`
//! directory), which the sandbox sees under `/run/bs/mcp-env/`. The agent's
//! MCP config then runs `/opt/bs/daemon mcp-exec --env-file <that file> --
//! <command> <args…>`: the wrapper reads the file, adds the variables to its
//! own environment and `exec`s the command. The agent CLI, its argv, the
//! daemon log and the transcript only ever see the file's path.
//!
//! The file is `KEY=VALUE` lines. A value is escaped so that any string fits
//! on one line: `\\` for a backslash, `\n` and `\r` for line breaks. Nothing
//! else is special, `=` included (only the first one splits).

use super::servers::McpServers;
use bondsymphonic_proto::{ErrorCode, McpServerConfig, McpServerId, McpTransportConfig, RpcError};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The env files' directory inside a sandbox's runtime directory.
pub const ENV_DIR: &str = "mcp-env";

/// `id`'s env file name inside [`ENV_DIR`].
pub fn env_file_name(id: &McpServerId) -> String {
    format!("{}.env", id.as_str())
}

/// Where `id`'s env file appears inside the sandbox.
pub fn env_file_in_sandbox(id: &McpServerId) -> String {
    format!("/run/bs/{ENV_DIR}/{}", env_file_name(id))
}

fn env_name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            _ => return None,
        }
    }
    Some(out)
}

/// The file's text for `vars`. A name outside `[A-Za-z_][A-Za-z0-9_]*` or a
/// value with a NUL (which no environment can carry) is refused; the error
/// names the variable, never the value.
fn render<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<String, RpcError> {
    let mut text = String::new();
    for (name, value) in vars {
        if !env_name_ok(name) {
            return Err(RpcError::invalid_params(
                "invalid MCP environment variable name",
            ));
        }
        if value.contains('\0') {
            return Err(RpcError::invalid_params(format!(
                "MCP environment variable {name} contains a NUL byte"
            )));
        }
        text.push_str(name);
        text.push('=');
        text.push_str(&escape(value));
        text.push('\n');
    }
    Ok(text)
}

/// Parses an env file. Errors name the line number only: the line may hold
/// a secret.
pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut vars = vec![];
    for (number, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let bad = || format!("line {} of the env file is malformed", number + 1);
        let (name, value) = line.split_once('=').ok_or_else(bad)?;
        if !env_name_ok(name) {
            return Err(bad());
        }
        vars.push((name.to_owned(), unescape(value).ok_or_else(bad)?));
    }
    Ok(vars)
}

/// Writes `<run_dir>/mcp-env/<id>.env` for the local server `cfg`: its plain
/// `env` and the stored values of its `secret_env` names. A secret with no
/// stored value is left out, so the server starts and reports it missing in
/// its own words rather than the agent failing to start.
///
/// The file is `0600` and replaced atomically; its directory is `0700`.
pub fn write_env_file(
    servers: &McpServers,
    cfg: &McpServerConfig,
    run_dir: &Path,
) -> Result<PathBuf, RpcError> {
    let McpTransportConfig::Stdio {
        env, secret_env, ..
    } = &cfg.transport
    else {
        return Err(RpcError::invalid_params(
            "an MCP env file is only for local servers",
        ));
    };
    let secrets: Vec<(&str, String)> = secret_env
        .iter()
        .filter_map(|name| {
            let value = servers.secret(&cfg.id, name);
            if value.is_none() {
                tracing::warn!(server = %cfg.id.as_str(), variable = %name, "MCP secret has no stored value");
            }
            value.map(|v| (name.as_str(), v.0))
        })
        .collect();
    let text = render(
        env.iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(secrets.iter().map(|(k, v)| (*k, v.as_str()))),
    )?;
    let io = |_| RpcError::new(ErrorCode::IoError, "MCP env file could not be written");
    let dir = run_dir.join(ENV_DIR);
    std::fs::create_dir_all(&dir).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    let path = dir.join(env_file_name(&cfg.id));
    super::servers::secrets::write_private(&path, text.as_bytes()).map_err(io)?;
    Ok(path)
}

/// The `mcp-exec` subcommand: sets the variables in `env_file` and replaces
/// this process with `argv` (looked up on `PATH` like `execvp`). Only returns
/// by exiting, with 127 when the file or the command cannot be used.
pub fn run(env_file: &Path, argv: &[OsString]) -> ! {
    let Err(why) = exec(env_file, argv);
    eprintln!("mcp-exec: {why}");
    std::process::exit(127)
}

#[cfg(unix)]
fn exec(env_file: &Path, argv: &[OsString]) -> Result<std::convert::Infallible, String> {
    use std::os::unix::process::CommandExt;
    let text =
        std::fs::read_to_string(env_file).map_err(|e| format!("{}: {e}", env_file.display()))?;
    let vars = parse(&text)?;
    let (program, args) = argv.split_first().ok_or("no command given")?;
    let error = std::process::Command::new(program)
        .args(args)
        .envs(vars)
        .exec();
    Err(format!("{}: {error}", program.to_string_lossy()))
}

/// Local servers run inside a bubblewrap sandbox, which is Linux only.
#[cfg(not(unix))]
fn exec(_env_file: &Path, _argv: &[OsString]) -> Result<std::convert::Infallible, String> {
    Err("mcp-exec needs a Unix host".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::*;

    const SENTINEL: &str = "s3cr3t-sentinel";

    #[test]
    fn values_roundtrip_through_one_line_each() {
        let tricky = "a=b\\c\nd\re \\n";
        let text = render([("ONE", tricky), ("TWO", ""), ("_3", SENTINEL)]).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(
            parse(&text).unwrap(),
            [
                ("ONE".to_string(), tricky.to_string()),
                ("TWO".to_string(), String::new()),
                ("_3".to_string(), SENTINEL.to_string()),
            ]
        );
    }

    #[test]
    fn bad_names_and_lines_are_refused_without_their_values() {
        assert!(render([("1BAD", "x")]).is_err());
        assert!(render([("NUL", "a\0b")]).is_err());
        for text in [
            format!("NOEQUALS{SENTINEL}\n"),
            format!("BAD-NAME={SENTINEL}\n"),
            format!("OK={SENTINEL}\\x\n"),
        ] {
            let err = parse(&text).unwrap_err();
            assert!(!err.contains(SENTINEL), "{err}");
        }
    }

    #[test]
    fn env_file_holds_plain_and_secret_values() {
        let dir = tempfile::tempdir().unwrap();
        let servers = McpServers::load(&dir.path().join("data")).unwrap();
        let config = McpServerConfig {
            id: "local".into(),
            name: "Local".into(),
            transport: McpTransportConfig::Stdio {
                command: "server".into(),
                args: vec![],
                env: [("PLAIN".to_string(), "x".to_string())].into(),
                secret_env: vec!["TOKEN".into(), "UNSET".into()],
            },
            enabled: true,
            default_workspace_enabled: true,
            enabled_backends: vec!["claude".into()],
        };
        servers
            .upsert(McpUpsertParams {
                server: config.clone(),
                secrets: vec![McpSecretWrite::Set {
                    reference: "TOKEN".into(),
                    value: McpSecretValue(SENTINEL.into()),
                }],
                expected_revision: None,
            })
            .unwrap();
        let run = dir.path().join("run");
        let path = write_env_file(&servers, &config, &run).unwrap();
        assert_eq!(path, run.join("mcp-env").join("local.env"));
        assert_eq!(env_file_in_sandbox(&config.id), "/run/bs/mcp-env/local.env");
        let vars = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            vars,
            [
                ("PLAIN".to_string(), "x".to_string()),
                ("TOKEN".to_string(), SENTINEL.to_string()),
            ]
        );

        let mut remote = config;
        remote.transport = McpTransportConfig::Http {
            url: "https://example.com/mcp".into(),
            auth: McpAuthConfig::None,
        };
        assert!(write_env_file(&servers, &remote, &run).is_err());
    }
}
