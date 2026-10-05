//! The GitHub CLI as the daemon runs it, and the one way a repository's
//! GitHub identity is worked out.
//!
//! Shared by `workspace.create_pr` and the agents' MCP tools
//! (`crate::mcp::tools`). `gh` always runs on the host as the user, never in a
//! sandbox, and is always told `--repo` explicitly by the tools so it never
//! guesses the repository from the working directory.

use crate::git::{git_error, Git};
use bondsymphonic_proto::RpcError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// How long `gh` gets. Longer than [`crate::git::GIT_TIMEOUT`]: a round trip
/// to github.com that a slow link or a throttled API can make genuinely slow,
/// but still bounded, so a `gh` that sits waiting on something does not hold
/// the request open forever.
pub const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// The command to run instead of `gh`, as argv.
///
/// `BS_GH_BIN` is a test and development hook, read from the *daemon's* own
/// environment and never from a request. It is parsed the way a shell would
/// (`"python" "gh_stub.py"` becomes two argv entries) so a stand-in can be an
/// interpreter plus a script — parsed, not run through a shell, so nothing it
/// names is interpreted as shell syntax.
pub fn gh_argv() -> Result<Vec<String>, RpcError> {
    let Ok(raw) = std::env::var("BS_GH_BIN") else {
        return Ok(vec!["gh".to_owned()]);
    };
    crate::util::argv::split(&raw, "BS_GH_BIN").map_err(RpcError::internal)
}

/// `owner/repo` for a github.com remote URL, in any of the three spellings git
/// accepts; `None` for anything else, a look-alike host included.
pub fn github_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("ssh://"))?;
        let rest = rest.split_once('@').map_or(rest, |(_, host)| host);
        rest.strip_prefix("github.com/")?
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The GitHub `owner/repo` of `repo`'s `origin`.
pub async fn origin_slug(git: &Git, repo: &Path) -> Result<String, RpcError> {
    let url = git
        .run(repo, &["remote", "get-url", "origin"])
        .await?
        .stdout;
    github_slug(&url).ok_or_else(|| {
        RpcError::invalid_params(format!("origin is not a GitHub repository: {}", url.trim()))
    })
}

/// How a [`run_gh`] error begins when `gh` could not be started at all, which
/// on a host without the GitHub CLI is the commonest failure there is.
pub const GH_NOT_STARTED: &str = "cannot start";

pub struct GhOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Runs `gh <args>` in `cwd`. `describe` is what an error names: the
/// subcommand, never the agent's or the user's prose.
pub async fn run_gh(cwd: &Path, args: &[String], describe: &str) -> Result<GhOutput, RpcError> {
    let mut argv = gh_argv()?;
    let program = argv.remove(0);
    argv.extend(args.iter().cloned());
    let mut cmd = Command::new(&program);
    cmd.args(&argv)
        .current_dir(cwd)
        // Nothing here may stop and ask: no terminal, nobody to answer.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        // Worded so a caller can tell "could not start gh at all" (above all:
        // not installed) from a `gh` that ran and failed: see
        // `GH_NOT_STARTED`.
        Ok(Err(e)) => {
            return Err(git_error(
                describe,
                None,
                &format!("{GH_NOT_STARTED} {program}: {e}"),
            ))
        }
        Err(_) => {
            return Err(git_error(
                describe,
                None,
                &format!("timed out after {}s", GH_TIMEOUT.as_secs()),
            ))
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(git_error(describe, out.status.code(), stderr.trim()));
    }
    Ok(GhOutput { stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BS_GH_BIN` is process-wide, so every case that touches it lives in one
    /// test rather than racing the others.
    #[test]
    fn gh_argv_reads_the_hook_and_falls_back_to_gh() {
        std::env::remove_var("BS_GH_BIN");
        assert_eq!(gh_argv().unwrap(), vec!["gh".to_string()]);

        std::env::set_var("BS_GH_BIN", "\"python\" \"/tmp/gh stub.py\"");
        assert_eq!(
            gh_argv().unwrap(),
            vec!["python".to_string(), "/tmp/gh stub.py".to_string()]
        );

        // Set but empty: a hook that names nothing is a misconfiguration, not a
        // silent fall back to the real `gh` the tests must never run.
        std::env::set_var("BS_GH_BIN", "   ");
        assert!(gh_argv().is_err());

        std::env::remove_var("BS_GH_BIN");
    }

    #[test]
    fn github_urls_give_their_slug() {
        for url in [
            "https://github.com/o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r/",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "https://user@github.com/o/r.git",
        ] {
            assert_eq!(github_slug(url).as_deref(), Some("o/r"), "{url}");
        }
    }

    #[test]
    fn non_github_origins_have_no_slug() {
        for url in [
            "/tmp/origin.git",
            "https://gitlab.com/o/r.git",
            "https://github.com.evil.example/o/r.git",
            "https://github.com/o",
            "git@github.com:o/r/extra.git",
            "",
        ] {
            assert_eq!(github_slug(url), None, "{url}");
        }
    }
}
