pub mod merge;
pub mod pr;
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

    /// Runs git with `stdin` fed to it on standard input.
    ///
    /// For the handful of plumbing commands that take their arguments there and
    /// nowhere else — `pack-objects --revs` is the one the daemon needs. The
    /// input is written in full before anything is read back, which is only
    /// safe because these inputs are a few short lines: a caller that fed in
    /// more than a pipe buffer could deadlock against a child blocked on a full
    /// stdout, so this is not the method for bulk input.
    pub async fn run_with_stdin(
        &self,
        cwd: &Path,
        args: &[&str],
        stdin: &str,
    ) -> Result<GitOutput, RpcError> {
        use tokio::io::AsyncWriteExt;

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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let run = async {
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            {
                // Dropped at the end of this block, which is what closes the
                // pipe; git waits for end-of-input before it does anything.
                let mut si = child.stdin.take().expect("stdin is piped");
                si.write_all(stdin.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                si.flush().await.map_err(|e| e.to_string())?;
            }
            child.wait_with_output().await.map_err(|e| e.to_string())
        };
        let out = match tokio::time::timeout(GIT_TIMEOUT, run).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(git_error(&command, None, &e)),
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

/// Copies into the main repository's object store every object reachable from
/// `include` but not from `exclude`.
///
/// A workspace's commits are written to a private object directory that is
/// deleted with the workspace (daemon design §5.2). The daemon reads them
/// through `GIT_ALTERNATE_OBJECT_DIRECTORIES`, so a merge or a push leaves the
/// main repository holding refs — the base branch, `refs/remotes/origin/...` —
/// that point at objects the user's own git cannot see and that disappear the
/// moment that workspace is destroyed. This is what makes those refs stand on
/// their own.
///
/// **Not `git repack -a -d`**, which §5.2 suggests: `repack -a` walks *every*
/// ref, so a second workspace whose commits live in a *different* private
/// directory makes it fail — and it fails after deleting the loose objects it
/// had already packed, leaving the repository unreadable. Measured on git 2.52.
/// `pack-objects` over an explicit revision range touches only the objects
/// asked for, which also makes it proportional to the merge rather than to the
/// repository.
///
/// Best effort, and loud when it fails: the merge or push it follows has
/// already happened, so this cannot turn into a failed request. A warning says
/// the refs still borrow.
pub async fn absorb_objects(
    git: &Git,
    repo: &Path,
    git_common: &Path,
    include: &str,
    exclude: &str,
) {
    if include == exclude {
        return;
    }
    // Written straight into `objects/pack`, named the way git names its own
    // packs: `pack-objects` builds each file under a temporary name and renames
    // it into place, which is exactly how `git repack` puts packs here.
    let pack_dir = git_common.join("objects").join("pack");
    if let Err(e) = std::fs::create_dir_all(&pack_dir) {
        tracing::warn!(dir = %pack_dir.display(), "cannot create the pack directory: {e}");
        return;
    }
    let prefix = pack_dir.join("pack").to_string_lossy().into_owned();
    let revs = format!("{include}\n^{exclude}\n");
    if let Err(e) = git
        .run_with_stdin(
            repo,
            &[
                "pack-objects",
                "--revs",
                "--delta-base-offset",
                "-q",
                &prefix,
            ],
            &revs,
        )
        .await
    {
        tracing::warn!(
            repo = %repo.display(),
            "packing {include} failed; this repository still borrows objects from a workspace and will lose them when it is destroyed: {}",
            e.message
        );
    }
}

pub fn git_error(command: &str, exit_code: Option<i32>, stderr: &str) -> RpcError {
    RpcError::new(ErrorCode::GitError, format!("{command} failed: {stderr}")).with_data(
        serde_json::json!({ "command": command, "exit_code": exit_code, "stderr": stderr }),
    )
}
