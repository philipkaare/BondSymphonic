pub mod repo;
pub mod worktree;

use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct Git {
    env: Vec<(String, String)>,
    /// `key=value` pairs passed as `-c key=value` before the subcommand on
    /// every invocation. Command-line config outranks every config *file*,
    /// which is what makes it usable against a config an agent can write.
    config: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Raw stdout from [`Git::run_bytes`], capped. `stdout` holds at most `cap + 1`
/// bytes; the extra byte is what tells a stream that is exactly `cap` long from
/// a longer one.
#[derive(Debug, Clone)]
pub struct GitBytes {
    pub stdout: Vec<u8>,
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// Adds a `-c key=value` override applied to every command this `Git` runs.
    pub fn with_config(mut self, key: &str, value: &str) -> Self {
        self.config.push(format!("{key}={value}"));
        self
    }

    pub async fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, RpcError> {
        // The reported command names the subcommand only: the `-c` prefix is a
        // fixed policy of this `Git`, not part of what the caller asked for.
        let command = format!("git {}", args.join(" "));
        let mut argv: Vec<&str> = Vec::with_capacity(self.config.len() * 2 + args.len());
        for c in &self.config {
            argv.push("-c");
            argv.push(c);
        }
        argv.extend_from_slice(args);
        let mut cmd = Command::new("git");
        cmd.args(&argv)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let out = match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(git_error(&command, None, &e.to_string())),
            Err(_) => return Err(git_error(&command, None, "timed out after 60s")),
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            return Err(git_error(&command, out.status.code(), stderr.trim()));
        }
        Ok(GitOutput { stdout, stderr })
    }

    /// Runs git and returns raw stdout, keeping at most `cap + 1` bytes.
    ///
    /// [`Git::run`] buffers the whole of stdout and lossily decodes it, which is
    /// wrong for object contents on both counts: a blob may be binary, and it may
    /// be arbitrarily large. Git objects are as unbounded as the files in a
    /// worktree — the reason `crate::fs::read_file` caps its own reads — so only
    /// the first `cap + 1` bytes are kept, the extra byte being what tells content
    /// exactly `cap` long from longer content.
    ///
    /// Anything past the cap is read and dropped rather than left in the pipe. Git
    /// therefore always gets to finish, so the exit status still means what it
    /// says and a caller can tell "no such object" from "here is the object".
    /// Cutting the pipe short instead would be faster on a huge blob but would
    /// trade a bounded read for a killed child and a meaningless status.
    pub async fn run_bytes(
        &self,
        cwd: &Path,
        args: &[&str],
        cap: usize,
    ) -> Result<GitBytes, RpcError> {
        use tokio::io::AsyncReadExt;

        let command = format!("git {}", args.join(" "));
        let mut argv: Vec<&str> = Vec::with_capacity(self.config.len() * 2 + args.len());
        for c in &self.config {
            argv.push("-c");
            argv.push(c);
        }
        argv.extend_from_slice(args);
        let mut cmd = Command::new("git");
        cmd.args(&argv)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let read = async {
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            // Both pipes were just configured above.
            let mut out = child.stdout.take().expect("stdout is piped");
            let mut err = child.stderr.take().expect("stderr is piped");
            let mut stdout = Vec::new();
            let mut stderr = String::new();
            // Drained together, not one after the other: a git blocked on a full
            // stderr pipe stops writing stdout, so a reader that finished stdout
            // before starting stderr would wait for an EOF that never comes.
            let (o, e) = tokio::join!(
                async {
                    (&mut out)
                        .take(cap as u64 + 1)
                        .read_to_end(&mut stdout)
                        .await?;
                    tokio::io::copy(&mut out, &mut tokio::io::sink()).await?;
                    Ok::<_, std::io::Error>(())
                },
                err.read_to_string(&mut stderr),
            );
            o.map_err(|e| e.to_string())?;
            e.map_err(|e| e.to_string())?;
            let status = child.wait().await.map_err(|e| e.to_string())?;
            Ok::<_, String>((stdout, stderr, status))
        };
        let (stdout, stderr, status) = match tokio::time::timeout(GIT_TIMEOUT, read).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(git_error(&command, None, &e)),
            Err(_) => return Err(git_error(&command, None, "timed out after 60s")),
        };
        if !status.success() {
            return Err(git_error(&command, status.code(), stderr.trim()));
        }
        Ok(GitBytes { stdout })
    }
}

pub fn git_error(command: &str, exit_code: Option<i32>, stderr: &str) -> RpcError {
    RpcError::new(ErrorCode::GitError, format!("{command} failed: {stderr}")).with_data(
        serde_json::json!({ "command": command, "exit_code": exit_code, "stderr": stderr }),
    )
}
