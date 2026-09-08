pub mod repo;

use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct Git {
    env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub async fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, RpcError> {
        let command = format!("git {}", args.join(" "));
        let mut cmd = Command::new("git");
        cmd.args(args)
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
}

pub fn git_error(command: &str, exit_code: Option<i32>, stderr: &str) -> RpcError {
    RpcError::new(ErrorCode::GitError, format!("{command} failed: {stderr}")).with_data(
        serde_json::json!({ "command": command, "exit_code": exit_code, "stderr": stderr }),
    )
}
