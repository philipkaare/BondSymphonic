# Agent git and GitHub tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every sandboxed agent a `bondsymphonic` MCP server whose tools fetch, push the workspace branch, open/update/comment on its PR and read CI and issues — run on the host with the user's credentials, scoped to that workspace.

**Architecture:** A per-sandbox Unix socket (`<run_dir>/mcp.sock`, seen as `/run/bs/mcp.sock` inside) is served by a daemon-side MCP server (`crates/daemon/src/mcp/`). Inside the sandbox the daemon binary's new `mcp-bridge` subcommand pipes the agent's stdio to that socket. Claude Code gets it via `--mcp-config`, Codex via `-c mcp_servers.…`. Tools reuse `git::fetch`, an extracted `git::pr::push_branch`, and a new `git::gh` runner.

**Tech Stack:** Rust (tokio, serde_json), git, the GitHub CLI `gh`, bubblewrap; tests with `cargo test` in the `bondsymphonic` WSL distro.

**Spec:** `docs/superpowers/specs/2026-10-05-agent-git-tools-design.md`

## Global Constraints

- Daemon tests run in WSL: `wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo test -p bondsymphonic-daemon --release <filter>"`. Do not run the full daemon suite per task — run the named tests plus `--lib`.
- `cargo fmt -p bondsymphonic-daemon` and `cargo clippy -p bondsymphonic-daemon --release` (no new warnings in touched files) before each commit.
- The real `gh` must never run in a test: always `BS_GH_BIN` → `crates/daemon/tests/fixtures/gh_stub.py`.
- No new crate dependencies. `serde_json`, `tokio`, `tokio-util`, `futures`, `parking_lot`, `tracing` are already available to the daemon.
- MCP protocol versions accepted: `2025-06-18`, `2025-03-26`, `2024-11-05`; default `2025-06-18`. Line cap 1 MiB. Tool text cap 64 KiB (logs keep the tail).
- Socket file name `mcp.sock`; in-sandbox path `/run/bs/mcp.sock`; server name `bondsymphonic`; in-sandbox daemon path `/opt/bs/daemon`.
- Agent configuration only when the sandbox backend is `linux_bwrap`.
- Commit by path (`git commit -- <paths>`), never `git add -A`: other people's uncommitted files live in this checkout (`Theme.h`, `todo.md`, `docs/superpowers/*/2026-09-30-*`). End every commit message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Code style: match surrounding code — doc comments that explain *why*, `RpcError` constructors, `tracing` at info/warn.

## Review Focus

- A tool called after its workspace was destroyed or while it is restarting must answer `isError` with a reason, never act on a stale workspace — Task 4 test `a_tool_on_a_destroyed_workspace_is_an_error`.
- Agent-supplied text beginning with `-` (a title `--repo evil/x`) must reach `gh` as a value, not a flag — Task 4 test `a_title_that_looks_like_a_flag_stays_a_value`.
- A reply larger than the cap (huge CI log) must be cut to its tail, not break the stream — Task 4 test `ci_logs_keeps_the_tail_of_a_long_log`.
- A client that sends two requests without waiting must get both answers with the right ids, and a cancelled call must not answer — Task 3 tests `concurrent_requests_both_answer` and `a_cancelled_call_sends_no_reply`.
- An origin that is not GitHub (local path, GitLab) must make GitHub tools say so plainly, while `git_fetch`/`git_push` still work — Task 1 test `non_github_origins_have_no_slug` and Task 4 test `github_tools_refuse_a_non_github_origin`.

---

### Task 1: `git::gh` — origin slug and the `gh` runner

**Files:**
- Create: `crates/daemon/src/git/gh.rs`
- Modify: `crates/daemon/src/git/mod.rs` (add `pub mod gh;`)
- Modify: `crates/daemon/src/git/pr.rs` (use `gh::run_gh`, delete its own `gh_argv`, `GH_TIMEOUT` moves to `gh.rs`)

**Interfaces:**
- Produces:
  - `pub const GH_TIMEOUT: Duration` (120 s, moved from `pr.rs`)
  - `pub fn gh_argv() -> Result<Vec<String>, RpcError>` (moved verbatim from `pr.rs`, now `pub`)
  - `pub fn github_slug(url: &str) -> Option<String>` — `"owner/repo"` for github.com URLs
  - `pub async fn origin_slug(git: &Git, repo: &Path) -> Result<String, RpcError>` — reads `git remote get-url origin`; `Err(invalid_params("origin is not a GitHub repository: <url>"))` otherwise
  - `pub struct GhOutput { pub stdout: String, pub stderr: String }`
  - `pub async fn run_gh(cwd: &Path, args: &[String], describe: &str) -> Result<GhOutput, RpcError>` — runs `gh_argv()` + args, env `GH_PROMPT_DISABLED=1`, `GIT_TERMINAL_PROMPT=0`, `NO_COLOR=1`, stdin null, `GH_TIMEOUT`, `kill_on_drop`; non-zero exit → `git_error(describe, code, stderr.trim())`

- [ ] **Step 1: Write the failing tests** (bottom of `gh.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
```

- [ ] **Step 2: Run to see them fail** — `cargo test -p bondsymphonic-daemon --release --lib git::gh` → compile error (module missing).

- [ ] **Step 3: Implement `gh.rs`**

```rust
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
/// but still bounded.
pub const GH_TIMEOUT: Duration = Duration::from_secs(120);

// (move `gh_argv` here from pr.rs unchanged, with its doc comment, as `pub fn`)

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
        RpcError::invalid_params(format!(
            "origin is not a GitHub repository: {}",
            url.trim()
        ))
    })
}

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
        Ok(Err(e)) => return Err(git_error(describe, None, &e.to_string())),
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
```

Move `gh_argv`'s unit test (`gh_argv_reads_the_hook_and_falls_back_to_gh`) from `pr.rs` into `gh.rs`'s test module unchanged.

- [ ] **Step 4: Make `pr.rs` use it.** In `create_pr`, replace the block from `let mut argv = gh_argv()?;` through the `if !out.status.success() { … }` check with:

```rust
    let mut args: Vec<String> = [
        "pr", "create", "--title", title, "--body", body, "--head", &ws.branch, "--base",
        &ws.base_branch,
    ]
    .map(str::to_owned)
    .to_vec();
    if draft {
        args.push("--draft".into());
    }
    // What goes in the error: never the title or body, which are the user's
    // own prose and say nothing about why `gh` failed.
    let program = crate::git::gh::gh_argv()?.remove(0);
    let command = {
        let mut c = format!("{program} pr create --head {} --base {}", ws.branch, ws.base_branch);
        if draft {
            c.push_str(" --draft");
        }
        c
    };
    let out = crate::git::gh::run_gh(&ws.repo_path, &args, &command).await?;
    let stdout = out.stdout;
```

Keep the URL-finding code after it (it reads `stdout`; replace `out.status.code()` in its error with `None`). Delete `pr.rs`'s `GH_TIMEOUT`, `gh_argv`, its test module, and now-unused imports (`Stdio`, `Duration`, `Command`).

- [ ] **Step 5: Run** `cargo test -p bondsymphonic-daemon --release --lib git::` and `--test pr_integration` → all pass (the existing PR tests prove the refactor kept behaviour).

- [ ] **Step 6: Commit** — `git commit -m "refactor: share the gh runner and add GitHub origin parsing" -- crates/daemon/src/git/gh.rs crates/daemon/src/git/mod.rs crates/daemon/src/git/pr.rs` (after `git add crates/daemon/src/git/gh.rs`).

---

### Task 2: `git::pr::push_branch` — the push half of `create_pr`, with force

**Files:**
- Modify: `crates/daemon/src/git/pr.rs`
- Test: `crates/daemon/tests/pr_integration.rs` (append)

**Interfaces:**
- Consumes: Task 1 (`gh::run_gh`).
- Produces: `pub async fn push_branch(d: &Daemon, ws: &Workspace, force: bool) -> Result<(), RpcError>` — refuses in-place (`in_place::nothing_to_merge()`) and non-`Ready`; takes `repo_lock`; `git push origin <branch>` (with `--force-with-lease` when `force`) through `layout.daemon_push_git()`; then `absorb_objects` exactly as `create_pr` does today. `create_pr` calls it.

- [ ] **Step 1: Write the failing integration tests** (append to `pr_integration.rs`; reuse its helpers):

```rust
/// The push half alone, as the agents' `git_push` tool runs it.
#[tokio::test]
async fn push_branch_pushes_and_force_with_lease_overwrites_a_rewritten_branch() {
    let _guard = ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "pusher").await;
    commit_in_ws(&daemon, &ws, "a.txt", "first").await;

    let w = daemon.workspace(&ws.id).unwrap();
    bondsymphonic_daemon::git::pr::push_branch(&daemon, &w, false).await.unwrap();
    assert_eq!(
        common::git_out(&origin, &["log", "-1", "--format=%s", "refs/heads/bs/pusher/work"]),
        "first"
    );

    // Rewrite the branch: amend, so the remote is no longer an ancestor.
    let env = lifecycle::layout_for(&daemon, &w).await.unwrap().sandbox_git_env();
    let wt = Path::new(&ws.worktree_path);
    let st = std::process::Command::new("git")
        .args(["commit", "-q", "--amend", "-m", "rewritten"])
        .current_dir(wt)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(st.success());

    let plain = bondsymphonic_daemon::git::pr::push_branch(&daemon, &w, false).await;
    assert_eq!(plain.unwrap_err().code, ErrorCode::GitError, "a non-fast-forward is refused");
    bondsymphonic_daemon::git::pr::push_branch(&daemon, &w, true).await.unwrap();
    assert_eq!(
        common::git_out(&origin, &["log", "-1", "--format=%s", "refs/heads/bs/pusher/work"]),
        "rewritten"
    );
    cancel.cancel();
}
```

(If `common::commit_all`'s env lacks author identity, mirror whatever `commit_all` sets.)

- [ ] **Step 2: Run** `--test pr_integration push_branch` → fails to compile (`push_branch` missing).

- [ ] **Step 3: Implement.** In `pr.rs`, move the state checks and the `{ lock … push … absorb_objects }` block out of `create_pr` into:

```rust
/// Pushes the workspace's own branch to `origin`, under the repository lock,
/// and copies the objects it carries into the shared store.
///
/// The one push the daemon makes, for `workspace.create_pr` and for an agent's
/// `git_push` tool alike: the branch is always `ws.branch`, never one the
/// caller names. `force` is `--force-with-lease`, for an agent that rebased its
/// branch; it still refuses to overwrite commits it has not seen.
pub async fn push_branch(d: &Daemon, ws: &Workspace, force: bool) -> Result<(), RpcError> {
    if ws.kind == bondsymphonic_proto::WorkspaceKind::InPlace {
        return Err(crate::workspace::in_place::nothing_to_merge());
    }
    if ws.state != WorkspaceState::Ready {
        return Err(RpcError::invalid_params(format!("workspace {} is not ready", ws.id)));
    }
    let layout = layout_for(d, ws).await?;
    let git = layout.daemon_push_git();
    let lock = crate::git::repo_lock(&ws.repo_path);
    let _guard = lock.lock().await;
    let mut args = vec!["push"];
    if force {
        args.push("--force-with-lease");
    }
    args.extend(["origin", ws.branch.as_str()]);
    git.run(&ws.repo_path, &args).await?;
    crate::git::absorb_objects(&git, &ws.repo_path, &layout.git_common, &ws.branch, &ws.base_branch)
        .await
        .map_err(|e| crate::git::objects_stranded("push", "pushed", &e.message))
}
```

Keep the existing comments (move them with the code). `create_pr` becomes: `let ws = d.workspace(id)?; push_branch(d, &ws, false).await?;` then the `gh` half. Add `use crate::workspace::Workspace;`.

- [ ] **Step 4: Run** `--test pr_integration` → all pass.

- [ ] **Step 5: Commit** — `refactor: extract push_branch from create_pr, with force-with-lease` (paths: `pr.rs`, `pr_integration.rs`).

---

### Task 3: MCP protocol core

**Files:**
- Create: `crates/daemon/src/mcp/mod.rs`, `crates/daemon/src/mcp/protocol.rs`
- Modify: `crates/daemon/src/lib.rs` (`pub mod mcp;`)

**Interfaces:**
- Produces (in `mcp::protocol`):
  - `pub struct ToolSpec { pub name: &'static str, pub description: &'static str, pub input_schema: serde_json::Value, pub read_only: bool }`
  - `pub enum ToolOutcome { Ok(String), Failed(String), InvalidArguments(String) }`
  - `#[async_trait] pub trait ToolHost: Send + Sync + 'static { fn tools(&self) -> Vec<ToolSpec>; async fn call(&self, name: &str, args: serde_json::Value) -> ToolOutcome; }` (an unknown `name` is the host's job: return `InvalidArguments("unknown tool …")`)
  - `pub const MAX_LINE: usize = 1 << 20;`
  - `pub async fn serve<R, W>(reader: R, writer: W, host: Arc<dyn ToolHost>)` where `R: AsyncRead + Unpin + Send + 'static`, `W: AsyncWrite + Unpin + Send + 'static` — returns when the reader closes or a line exceeds `MAX_LINE`; in-flight calls are aborted on return.
- `mcp/mod.rs`: `pub mod protocol;` (Tasks 4–5 add `tools`, `registry`, Task 6 `bridge`), plus `pub const SERVER_NAME: &str = "bondsymphonic"; pub const SOCKET_FILE: &str = "mcp.sock"; pub const SOCKET_IN_SANDBOX: &str = "/run/bs/mcp.sock";`

- [ ] **Step 1: Write the failing tests** (in `protocol.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct Fake;
    #[async_trait::async_trait]
    impl ToolHost for Fake {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec { name: "echo", description: "echo", input_schema: json!({"type":"object"}), read_only: true }]
        }
        async fn call(&self, name: &str, args: Value) -> ToolOutcome {
            match name {
                "echo" => ToolOutcome::Ok(args["text"].as_str().unwrap_or("").into()),
                "slow" => { tokio::time::sleep(std::time::Duration::from_secs(30)).await; ToolOutcome::Ok("late".into()) }
                "fail" => ToolOutcome::Failed("it broke".into()),
                other => ToolOutcome::InvalidArguments(format!("unknown tool {other}")),
            }
        }
    }

    /// A client end and the lines it reads back.
    async fn session() -> (tokio::io::DuplexStream, tokio::io::Lines<BufReader<tokio::io::DuplexStream>>) {
        let (client_w, server_r) = tokio::io::duplex(1 << 16);
        let (server_w, client_r) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(server_r, server_w, Arc::new(Fake)));
        (client_w, BufReader::new(client_r).lines())
    }

    async fn send(w: &mut tokio::io::DuplexStream, v: Value) {
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }
    async fn recv(r: &mut tokio::io::Lines<BufReader<tokio::io::DuplexStream>>) -> Value {
        serde_json::from_str(&r.next_line().await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn initialize_echoes_a_supported_version_and_offers_tools() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "bondsymphonic");
        assert!(v["result"]["capabilities"]["tools"].is_object());
        send(&mut w, json!({"jsonrpc":"2.0","id":"x","method":"initialize","params":{"protocolVersion":"1999-01-01"}})).await;
        assert_eq!(recv(&mut r).await["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn tools_list_and_call() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        send(&mut w, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["id"], 2, "a notification gets no reply, so this is the first line");
        assert_eq!(v["result"]["tools"][0]["name"], "echo");
        assert_eq!(v["result"]["tools"][0]["annotations"]["readOnlyHint"], true);
        assert!(v["result"]["tools"][0]["inputSchema"].is_object());
        send(&mut w, json!({"jsonrpc":"2.0","id":"s","method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["id"], "s");
        assert_eq!(v["result"]["content"][0], json!({"type":"text","text":"hi"}));
        assert_eq!(v["result"]["isError"], false);
        send(&mut w, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fail","arguments":{}}})).await;
        let v = recv(&mut r).await;
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(v["result"]["content"][0]["text"], "it broke");
        send(&mut w, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}})).await;
        assert_eq!(recv(&mut r).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_methods_ping_and_malformed_lines() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":5,"method":"resources/list"})).await;
        assert_eq!(recv(&mut r).await["error"]["code"], -32601);
        send(&mut w, json!({"jsonrpc":"2.0","id":6,"method":"ping"})).await;
        assert_eq!(recv(&mut r).await["result"], json!({}));
        w.write_all(b"{not json\n").await.unwrap();
        let v = recv(&mut r).await;
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["id"].is_null());
    }

    #[tokio::test]
    async fn concurrent_requests_both_answer() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"slow","arguments":{}}})).await;
        send(&mut w, json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"fast"}}})).await;
        // The fast one answers while the slow one is still running.
        let v = tokio::time::timeout(std::time::Duration::from_secs(5), recv(&mut r)).await.unwrap();
        assert_eq!(v["id"], 11);
    }

    #[tokio::test]
    async fn a_cancelled_call_sends_no_reply() {
        let (mut w, mut r) = session().await;
        send(&mut w, json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"slow","arguments":{}}})).await;
        send(&mut w, json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":20}})).await;
        send(&mut w, json!({"jsonrpc":"2.0","id":21,"method":"ping"})).await;
        assert_eq!(recv(&mut r).await["id"], 21);
        // Nothing else arrives for id 20.
        assert!(tokio::time::timeout(std::time::Duration::from_millis(300), r.next_line()).await.is_err());
    }

    #[tokio::test]
    async fn an_oversize_line_ends_the_session() {
        let (mut w, mut r) = session().await;
        let big = vec![b'x'; MAX_LINE + 10];
        w.write_all(&big).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        assert!(r.next_line().await.unwrap().is_none(), "the server hangs up");
    }
}
```

- [ ] **Step 2: Run** `--lib mcp::protocol` → compile errors.

- [ ] **Step 3: Implement `protocol.rs`:**

```rust
//! The server side of MCP over the stdio transport's framing:
//! newline-delimited JSON-RPC 2.0, one message per line.
//!
//! Hand-written because the daemon needs five methods of it and no MCP crate
//! is a dependency. Transport-agnostic: `serve` takes any reader and writer,
//! which in production is one connection to a workspace's `mcp.sock` and in
//! tests is an in-memory pipe.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub const MAX_LINE: usize = 1 << 20;
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub read_only: bool,
}

pub enum ToolOutcome {
    Ok(String),
    Failed(String),
    InvalidArguments(String),
}

#[async_trait]
pub trait ToolHost: Send + Sync + 'static {
    fn tools(&self) -> Vec<ToolSpec>;
    async fn call(&self, name: &str, args: Value) -> ToolOutcome;
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let version = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": super::SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
    })
}

fn list(host: &dyn ToolHost) -> Value {
    let tools: Vec<Value> = host
        .tools()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
                "annotations": {"readOnlyHint": t.read_only},
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// The key a request id is remembered under for cancellation: its JSON text,
/// so `1` and `"1"` stay different ids.
fn key(id: &Value) -> String {
    id.to_string()
}

pub async fn serve<R, W>(reader: R, mut writer: W, host: Arc<dyn ToolHost>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let calls: Arc<parking_lot::Mutex<HashMap<String, tokio::task::AbortHandle>>> =
        Arc::default();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Bounded read: `take` stops a line from growing past the cap.
        let n = match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 || buf.len() > MAX_LINE {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                let _ = out_tx.send(error(&Value::Null, -32700, "parse error"));
                continue;
            }
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            ("notifications/cancelled", None) => {
                if let Some(h) = calls.lock().remove(&key(&params["requestId"])) {
                    h.abort();
                }
            }
            (_, None) => {} // any other notification: nothing to say
            ("initialize", Some(id)) => {
                let _ = out_tx.send(reply(&id, initialize(&params)));
            }
            ("ping", Some(id)) => {
                let _ = out_tx.send(reply(&id, json!({})));
            }
            ("tools/list", Some(id)) => {
                let _ = out_tx.send(reply(&id, list(&*host)));
            }
            ("tools/call", Some(id)) => {
                let Some(name) = params["name"].as_str().map(str::to_owned) else {
                    let _ = out_tx.send(error(&id, -32602, "tools/call needs a name"));
                    continue;
                };
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => json!({}),
                    Some(v) => v.clone(),
                };
                let (host, out_tx, calls2, k) = (host.clone(), out_tx.clone(), calls.clone(), key(&id));
                let task = tokio::spawn(async move {
                    let msg = match host.call(&name, args).await {
                        ToolOutcome::Ok(text) => reply(&id, json!({"content":[{"type":"text","text":text}],"isError":false})),
                        ToolOutcome::Failed(text) => reply(&id, json!({"content":[{"type":"text","text":text}],"isError":true})),
                        ToolOutcome::InvalidArguments(why) => error(&id, -32602, &why),
                    };
                    calls2.lock().remove(&key(&id));
                    let _ = out_tx.send(msg);
                });
                calls.lock().insert(k, task.abort_handle());
            }
            (_, Some(id)) => {
                let _ = out_tx.send(error(&id, -32601, "method not found"));
            }
        }
    }
    for (_, h) in calls.lock().drain() {
        h.abort();
    }
    drop(out_tx);
    let _ = writer_task.await;
}
```

Note the race in `tools/call`: the task may finish and remove its key before `insert` runs, leaving a stale entry. Fix by inserting **before** spawning work can complete: create the task with `tokio::spawn`, then insert, and have the task's first action be a `tokio::task::yield_now().await` — or simpler, insert under the same lock the task takes to remove (lock `calls`, spawn, insert, unlock). Use the latter:

```rust
let mut guard = calls.lock();
let task = tokio::spawn(/* … as above … */);
guard.insert(k, task.abort_handle());
drop(guard);
```

(The task's `calls2.lock()` then waits for the guard; `parking_lot::Mutex` is fine to hold across a non-await spawn.)

`async-trait` is already a daemon dependency (used by `agents::backend`); confirm with `grep async-trait crates/daemon/Cargo.toml`.

- [ ] **Step 4: Run** `--lib mcp::protocol` → 6 pass.

- [ ] **Step 5: Commit** — `feat: MCP stdio protocol server core` (paths: `crates/daemon/src/mcp/mod.rs`, `protocol.rs`, `lib.rs`).

---

### Task 4: The workspace tools

**Files:**
- Create: `crates/daemon/src/mcp/tools.rs`
- Modify: `crates/daemon/src/mcp/mod.rs` (`pub mod tools;`)
- Modify: `crates/daemon/tests/fixtures/gh_stub.py` (canned answers)
- Test: create `crates/daemon/tests/mcp_tools.rs`

**Interfaces:**
- Consumes: Task 1 (`gh::{origin_slug, run_gh, gh_argv}`), Task 2 (`pr::push_branch`), Task 3 (`ToolHost`, `ToolSpec`, `ToolOutcome`), `git::fetch::fetch_repo(repo, no_hooks)`.
- Produces:
  - `pub struct WorkspaceTools { daemon: std::sync::Weak<Daemon>, workspace: WorkspaceId }` with `pub fn new(daemon: Weak<Daemon>, workspace: WorkspaceId) -> Self`, implementing `ToolHost`.
  - `pub const TEXT_CAP: usize = 64 * 1024;`
  - `pub const READ_ONLY_TOOLS: [&str; 4] = ["git_fetch", "pr_view", "ci_logs", "issue_view"];`

Tool behaviour (exact `gh` argv; `<slug>` from `origin_slug`, `<b>` = `ws.branch`):

| tool | argv |
|---|---|
| `pr_create` | push_branch(force=false), then `pr create --repo <slug> --title T --body B --head <b> --base <base or ws.base_branch> [--draft]`. If `gh` fails with stderr containing `already exists`, run `pr view <b> --repo <slug> --json url --jq .url` and return `"A pull request for <b> already exists: <url>"`. Success text: the URL line. |
| `pr_view` | `pr view <number or b> --repo <slug> --json number,url,state,title,body,isDraft,baseRefName,headRefName,mergeable,reviewDecision,statusCheckRollup,reviews,comments` → stdout |
| `pr_update` | title/body: `pr edit <b> --repo <slug> [--title T] [--body B]`; `ready: true` → `pr ready <b> --repo <slug>`; `ready: false` → `pr ready <b> --repo <slug> --undo`. At least one argument required → else `InvalidArguments`. |
| `pr_comment` | `pr comment <number or b> --repo <slug> --body B` |
| `ci_logs` | no `run_id`: `run list --repo <slug> --branch <b> --limit 1 --json databaseId,status,conclusion,name,url` → if empty: `"No CI runs for <b> yet."`; take `databaseId`. Then `run view <id> --repo <slug> --json status,conclusion,name,url,jobs` and, when `conclusion == "failure"`, append `run view <id> --repo <slug> --log-failed` (tail-capped). |
| `issue_view` | `issue view <number> --repo <slug> --json number,title,body,state,labels,comments,url` |

`git_fetch` → `fetch_repo(&ws.repo_path, &d.dirs.no_hooks())`: text `"Fetched origin: N refs updated."` / `"Fetched origin: already up to date."` / `"This repository has no origin remote."`. `git_push {force}` → `push_branch`, text `"Pushed <b> to origin."`.

Every call: upgrade the `Weak` (gone → `Failed("the BondSymphonic daemon is shutting down")`); `d.workspace(&id)` (error → `Failed(message)`); require `state == Ready` (else `Failed("workspace … is not ready: <state>")`); errors from git/gh → `Failed(e.message)`; `gh` errors get `" — if GitHub says you are not logged in, log in under Settings → Setup."` appended when stderr mentions `auth` or `logged in`. Log `tracing::info!(ws, tool, ok)` — never arguments.

Argument validation (→ `InvalidArguments`): `pr_create` needs non-empty string `title` and string `body`; `draft` bool if present; `base` non-empty string if present. `number`/`run_id` must be positive integers (JSON number or digit string). `body` for `pr_comment` non-empty. Unknown tool → `InvalidArguments("unknown tool <name>")`.

`TEXT_CAP`: `fn cap_head(s) ` keeps the first 64 KiB + `"\n… [truncated]"`; `fn cap_tail(s)` keeps the last 64 KiB prefixed `"[… earlier output truncated]\n"`. Logs use `cap_tail`, everything else `cap_head`. Cut on a char boundary.

Input schemas: JSON Schema objects with `"additionalProperties": false`, e.g. `pr_create`: `{"type":"object","properties":{"title":{"type":"string"},"body":{"type":"string"},"draft":{"type":"boolean"},"base":{"type":"string","description":"Base branch; defaults to the workspace's base branch"}},"required":["title","body"],"additionalProperties":false}`. Descriptions must say what the tool does *and* that it acts only on this workspace's branch, e.g. `git_push`: "Push this workspace's branch to origin. Only this workspace's own branch can be pushed. Set force to overwrite after a rebase (uses --force-with-lease)."

- [ ] **Step 1: Extend the stub.** In `gh_stub.py`, after the failure check, answer by subcommand (stdout), so tools have something to parse:

```python
if args[:2] == ["pr", "create"]:
    if os.environ.get("GH_STUB_PR_EXISTS") == "1":
        sys.stderr.write('a pull request for branch "x" into branch "main" already exists:\nhttps://github.com/example/repo/pull/7\n')
        sys.exit(1)
    sys.stdout.write("https://github.com/example/repo/pull/42\n")
elif args[:2] == ["pr", "view"]:
    if "--jq" in args:
        sys.stdout.write("https://github.com/example/repo/pull/7\n")
    else:
        sys.stdout.write('{"number":42,"state":"OPEN","title":"T"}\n')
elif args[:2] == ["run", "list"]:
    sys.stdout.write(os.environ.get("GH_STUB_RUNS", '[{"databaseId":99,"status":"completed","conclusion":"failure","name":"ci","url":"u"}]') + "\n")
elif args[:2] == ["run", "view"]:
    if "--log-failed" in args:
        sys.stdout.write(os.environ.get("GH_STUB_LOG_TEXT", "step failed: boom\n"))
    else:
        sys.stdout.write('{"status":"completed","conclusion":"failure","name":"ci"}\n')
elif args[:2] == ["issue", "view"]:
    sys.stdout.write('{"number":5,"title":"Bug"}\n')
```

(Replace the existing single `pr create` branch; keep logging and `GH_STUB_FAIL` first.)

- [ ] **Step 2: Write the failing integration tests** in `crates/daemon/tests/mcp_tools.rs`. Copy `python()`, `fixture_dir()`, `arg_path()`, `use_gh_stub()` and the `ENV` mutex from `pr_integration.rs` (each test file is its own crate). Call tools directly through the `ToolHost` trait — the socket is Task 5's.

```rust
mod common;

use bondsymphonic_daemon::mcp::protocol::{ToolHost, ToolOutcome};
use bondsymphonic_daemon::mcp::tools::{WorkspaceTools, READ_ONLY_TOOLS};
use bondsymphonic_proto::*;
use common::{create_ws, init_repo_with_origin, start_daemon, Client};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

// … python(), fixture_dir(), arg_path(), use_gh_stub(), ENV copied here …

fn text(o: ToolOutcome) -> Result<String, String> {
    match o {
        ToolOutcome::Ok(t) => Ok(t),
        ToolOutcome::Failed(t) => Err(format!("failed: {t}")),
        ToolOutcome::InvalidArguments(t) => Err(format!("invalid: {t}")),
    }
}

/// A repository whose origin *reads* as GitHub (so `--repo` resolves) but
/// pushes to the local bare repository, so nothing leaves the machine.
fn github_looking_origin(repo: &Path, bare: &Path) {
    common::git_out(repo, &["remote", "set-url", "origin", "https://github.com/example/repo.git"]);
    common::git_out(repo, &["remote", "set-url", "--push", "origin", &bare.display().to_string()]);
}

async fn setup(name: &str) -> (tempfile::TempDir, Arc<bondsymphonic_daemon::daemon::Daemon>, WorkspaceInfo, std::path::PathBuf, tokio_util::sync::CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let (repo, origin) = init_repo_with_origin(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, name).await;
    github_looking_origin(&repo, &origin);
    (dir, daemon, ws, origin, cancel)
}

#[tokio::test]
async fn the_tool_list_marks_the_read_only_ones() {
    let (_dir, d, ws, _o, cancel) = setup("list").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone()).tools();
    let names: Vec<_> = tools.iter().map(|t| t.name).collect();
    assert_eq!(names, ["git_fetch", "git_push", "pr_create", "pr_view", "pr_update", "pr_comment", "ci_logs", "issue_view"]);
    for t in &tools {
        assert_eq!(t.read_only, READ_ONLY_TOOLS.contains(&t.name), "{}", t.name);
        assert_eq!(t.input_schema["type"], "object");
    }
    cancel.cancel();
}

#[tokio::test]
async fn pr_create_pushes_and_names_the_repo_explicitly() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, origin, cancel) = setup("prc").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(tools.call("pr_create", json!({"title":"T","body":"B"})).await).unwrap();
    assert_eq!(out.trim(), "https://github.com/example/repo/pull/42");
    assert!(common::git_try(&origin, &["rev-parse", "refs/heads/bs/prc/work"]).is_ok());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().trim(),
        "pr create --repo example/repo --title T --body B --head bs/prc/work --base main"
    );
    cancel.cancel();
}

#[tokio::test]
async fn a_title_that_looks_like_a_flag_stays_a_value() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("flag").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    text(tools.call("pr_create", json!({"title":"--repo evil/x","body":"B"})).await).unwrap();
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("--repo example/repo --title --repo evil/x --body"), "{logged}");
    cancel.cancel();
}

#[tokio::test]
async fn an_existing_pr_is_reported_not_failed() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("exists").await;
    use_gh_stub(py, &dir.path().join("gh.log"), false);
    std::env::set_var("GH_STUB_PR_EXISTS", "1");
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(tools.call("pr_create", json!({"title":"T","body":"B"})).await);
    std::env::remove_var("GH_STUB_PR_EXISTS");
    assert!(out.unwrap().contains("already exists: https://github.com/example/repo/pull/7"));
    cancel.cancel();
}

#[tokio::test]
async fn read_tools_call_gh_with_the_branch_and_repo() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("read").await;
    let log = dir.path().join("gh.log");
    use_gh_stub(py, &log, false);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    text(tools.call("pr_view", json!({})).await).unwrap();
    text(tools.call("issue_view", json!({"number": 5})).await).unwrap();
    text(tools.call("pr_comment", json!({"body":"hello", "number": "42"})).await).unwrap();
    text(tools.call("pr_update", json!({"ready": true})).await).unwrap();
    let logged = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = logged.lines().collect();
    assert!(lines[0].starts_with("pr view bs/read/work --repo example/repo --json number,url,state"), "{logged}");
    assert!(lines[1].starts_with("issue view 5 --repo example/repo --json"), "{logged}");
    assert_eq!(lines[2], "pr comment 42 --repo example/repo --body hello");
    assert_eq!(lines[3], "pr ready bs/read/work --repo example/repo");
    cancel.cancel();
}

#[tokio::test]
async fn ci_logs_keeps_the_tail_of_a_long_log() {
    let _g = ENV.lock().await;
    let Some(py) = python() else { return };
    let (dir, d, ws, _o, cancel) = setup("ci").await;
    use_gh_stub(py, &dir.path().join("gh.log"), false);
    let long = format!("{}THE END\n", "noise line\n".repeat(20_000));
    std::env::set_var("GH_STUB_LOG_TEXT", &long);
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let out = text(tools.call("ci_logs", json!({})).await);
    std::env::remove_var("GH_STUB_LOG_TEXT");
    let out = out.unwrap();
    assert!(out.len() <= bondsymphonic_daemon::mcp::tools::TEXT_CAP + 4096, "{}", out.len());
    assert!(out.trim_end().ends_with("THE END"));
    assert!(out.contains("earlier output truncated"));
    cancel.cancel();
}

#[tokio::test]
async fn github_tools_refuse_a_non_github_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _origin) = init_repo_with_origin(dir.path()); // origin is a local path
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "local").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let err = text(tools.call("pr_view", json!({})).await).unwrap_err();
    assert!(err.contains("not a GitHub repository"), "{err}");
    // Fetch and push still work: they only need git.
    text(tools.call("git_fetch", json!({})).await).unwrap();
    text(tools.call("git_push", json!({})).await).unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn bad_arguments_are_invalid_not_failed() {
    let (_dir, d, ws, _o, cancel) = setup("args").await;
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    for (tool, args) in [
        ("pr_create", json!({"body":"no title"})),
        ("pr_create", json!({"title":"", "body":"x"})),
        ("issue_view", json!({"number": -1})),
        ("issue_view", json!({"number": "abc"})),
        ("pr_update", json!({})),
        ("pr_comment", json!({"body": ""})),
        ("no_such_tool", json!({})),
    ] {
        assert!(matches!(tools.call(tool, args.clone()).await, ToolOutcome::InvalidArguments(_)), "{tool} {args}");
    }
    cancel.cancel();
}

#[tokio::test]
async fn a_tool_on_a_destroyed_workspace_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "gone").await;
    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams { workspace_id: ws.id.clone(), force: true })).await.unwrap();
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    assert!(matches!(tools.call("git_fetch", json!({})).await, ToolOutcome::Failed(_)));
    cancel.cancel();
}

#[tokio::test]
async fn push_is_refused_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            repo_path: repo.to_string_lossy().into(),
            base_branch: String::new(),
            name: "inplace".into(),
            init_if_missing: false,
            in_place: true,
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let tools = WorkspaceTools::new(Arc::downgrade(&d), ws.id.clone());
    let err = text(tools.call("git_push", json!({})).await).unwrap_err();
    assert!(err.contains("in-place"), "{err}");
    cancel.cancel();
}
```

- [ ] **Step 3: Run** `--test mcp_tools` → compile errors.

- [ ] **Step 4: Implement `tools.rs`.** Structure:

```rust
//! The tools every sandboxed agent's `bondsymphonic` MCP server offers.
//!
//! Each runs on the host, as the user, against the one workspace whose
//! socket the call arrived on (see `mcp::registry`). The agent never names a
//! branch it may push or a PR it may edit: those are always the workspace's
//! own. See docs/superpowers/specs/2026-10-05-agent-git-tools-design.md.

use super::protocol::{ToolHost, ToolOutcome, ToolSpec};
use crate::daemon::Daemon;
use crate::git::{fetch, gh, pr};
use crate::workspace::Workspace;
use async_trait::async_trait;
use bondsymphonic_proto::{RpcError, WorkspaceId, WorkspaceState};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};

pub const TEXT_CAP: usize = 64 * 1024;
pub const READ_ONLY_TOOLS: [&str; 4] = ["git_fetch", "pr_view", "ci_logs", "issue_view"];

pub struct WorkspaceTools {
    daemon: Weak<Daemon>,
    workspace: WorkspaceId,
}

impl WorkspaceTools {
    pub fn new(daemon: Weak<Daemon>, workspace: WorkspaceId) -> Self {
        Self { daemon, workspace }
    }
}

/// Why a call cannot run, already worded for the agent.
type Refusal = ToolOutcome;

fn failed(e: RpcError) -> ToolOutcome {
    let mut text = e.message;
    if text.contains("auth") || text.contains("logged in") {
        text.push_str(" — if GitHub says you are not logged in, ask the user to log in under Settings → Setup.");
    }
    ToolOutcome::Failed(text)
}

fn cap_head(s: String) -> String { /* first TEXT_CAP bytes on a char boundary + "\n… [truncated]" */ }
fn cap_tail(s: String) -> String { /* "[… earlier output truncated]\n" + last TEXT_CAP bytes on a char boundary */ }

fn positive(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().filter(|n| *n > 0),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok().filter(|n| *n > 0),
        _ => None,
    }
}
/// An optional positive-integer argument: `Ok(None)` when absent.
fn opt_number(args: &Value, key: &str) -> Result<Option<u64>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => positive(v).map(Some).ok_or_else(|| ToolOutcome::InvalidArguments(format!("{key} must be a positive integer"))),
    }
}
fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> { /* non-empty string or InvalidArguments */ }
fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolOutcome> { /* absent → None; non-string or empty → InvalidArguments */ }
fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolOutcome> { /* … */ }

impl WorkspaceTools {
    /// The daemon and the workspace as they are now, or the reason the call
    /// cannot go ahead.
    fn current(&self) -> Result<(Arc<Daemon>, Workspace), Refusal> {
        let d = self.daemon.upgrade().ok_or_else(|| ToolOutcome::Failed("the BondSymphonic daemon is shutting down".into()))?;
        let ws = d.workspace(&self.workspace).map_err(failed)?;
        if ws.state != WorkspaceState::Ready {
            return Err(ToolOutcome::Failed(format!("workspace {} is not ready ({:?}); try again once it is running", ws.name, ws.state)));
        }
        Ok((d, ws))
    }

    async fn slug(d: &Daemon, ws: &Workspace) -> Result<String, Refusal> {
        gh::origin_slug(&d.git, &ws.repo_path).await.map_err(failed)
    }

    async fn gh(ws: &Workspace, args: Vec<String>, what: &str) -> Result<String, Refusal> {
        gh::run_gh(&ws.repo_path, &args, what).await.map(|o| o.stdout).map_err(failed)
    }
}
```

Then `tools()` returns the eight `ToolSpec`s in the order the test expects, and `call` validates arguments **before** `current()` (so bad arguments are `InvalidArguments` even on a broken workspace) and dispatches. Use a private `async fn run(&self, name, args) -> Result<String, ToolOutcome>` and map `Ok(t)` → `ToolOutcome::Ok(t)`, `Err(o)` → `o`; wrap with `tracing::info!(ws = %self.workspace, tool = name, ok, "agent tool")`. Write each arm with the exact argv from the table above, building `Vec<String>` in the order `[subcommand…, target, "--repo", slug, flags…]`. For `pr_create` with an existing PR: on `Err(ToolOutcome::Failed(t))` where `t.contains("already exists")`, call `pr view <b> --repo <slug> --json url --jq .url` and return the sentence.

- [ ] **Step 5: Run** `--test mcp_tools` and `--lib mcp` → all pass.

- [ ] **Step 6: Commit** — `feat: workspace-scoped git and GitHub tools for agents` (paths: `tools.rs`, `mcp/mod.rs`, `gh_stub.py`, `mcp_tools.rs`).

---

### Task 5: Per-sandbox listeners and lifecycle wiring

**Files:**
- Create: `crates/daemon/src/mcp/registry.rs`
- Modify: `crates/daemon/src/mcp/mod.rs` (`pub mod registry;`)
- Modify: `crates/daemon/src/daemon.rs` (fields `pub mcp: crate::mcp::registry::McpRegistry`, `me: std::sync::Weak<Daemon>`; `pub fn weak(&self) -> Weak<Daemon>`; construct with `Arc::new_cyclic`)
- Modify: `crates/daemon/src/workspace/lifecycle.rs` (`start_sandbox`: start the MCP listener next to `proxies.start`; `tear_down_sandbox` and the destroy path: `d.mcp.stop_workspace(id)` next to `d.proxies.stop(id)`)
- Modify: `crates/daemon/src/workspace/agent_sandboxes.rs` (`ensure`: start a listener on `spec.run_dir.join(mcp::SOCKET_FILE)` before `d.backend.start`)
- Test: create `crates/daemon/tests/mcp_socket.rs` (`#![cfg(unix)]`)

**Interfaces:**
- Consumes: Task 3 `protocol::serve`, Task 4 `tools::WorkspaceTools`.
- Produces: `McpRegistry` (`Default`) with `pub fn start(&self, daemon: Weak<Daemon>, id: &WorkspaceId, socket: &Path) -> Result<(), RpcError>` (replaces any listener already on that socket path; removes a stale socket file first; binds `UnixListener`; `chmod 0600`; spawns the accept loop serving each connection with `serve(read_half, write_half, Arc::new(WorkspaceTools::new(daemon, id)))` under a `CancellationToken` child that also aborts open connections), and `pub fn stop_workspace(&self, id: &WorkspaceId)` (cancels every listener for `id` and deletes its socket files). On non-unix, `start` is a no-op `Ok(())`. `Daemon::weak()`.

- [ ] **Step 1: Write the failing test** `tests/mcp_socket.rs`:

```rust
#![cfg(unix)]
mod common;

use bondsymphonic_proto::*;
use common::{create_ws, init_repo_with_origin, start_daemon, Client};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn rpc(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>, w: &mut tokio::net::unix::OwnedWriteHalf, msg: Value) -> Value {
    w.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn a_workspace_serves_its_tools_on_its_run_dir_socket_until_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "sock").await;
    let socket = d.dirs.run(&ws.id).join(bondsymphonic_daemon::mcp::SOCKET_FILE);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);

    let (r, mut w) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
    let mut lines = BufReader::new(r).lines();
    let init = rpc(&mut lines, &mut w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "bondsymphonic");
    let list = rpc(&mut lines, &mut w, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 8);
    let fetched = rpc(&mut lines, &mut w, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"git_fetch","arguments":{}}})).await;
    assert_eq!(fetched["result"]["isError"], false, "{fetched}");

    c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams { workspace_id: ws.id.clone(), force: true })).await.unwrap();
    assert!(!socket.exists(), "destroy removes the socket");
    cancel.cancel();
}

#[tokio::test]
async fn a_restart_serves_a_fresh_listener() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _o) = init_repo_with_origin(dir.path());
    let (port, token, d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "again").await;
    c.call(Request::WorkspaceRestart(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap();
    let socket = d.dirs.run(&ws.id).join(bondsymphonic_daemon::mcp::SOCKET_FILE);
    let (r, mut w) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
    let mut lines = BufReader::new(r).lines();
    let v = rpc(&mut lines, &mut w, json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await;
    assert_eq!(v["result"], json!({}));
    cancel.cancel();
}
```

- [ ] **Step 2: Run** `--test mcp_socket` → fails (no socket).

- [ ] **Step 3: Implement `registry.rs`:**

```rust
//! One MCP listener per sandbox, on `<run_dir>/mcp.sock`, which the sandbox
//! sees as `/run/bs/mcp.sock`. Each is bound to its workspace for its whole
//! life; every tool call re-reads the workspace, so a listener that outlives
//! its sandbox for a moment can do nothing to a stopped or destroyed one.

use super::{protocol, tools::WorkspaceTools};
use crate::daemon::Daemon;
use bondsymphonic_proto::{RpcError, WorkspaceId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

struct Listener {
    workspace: WorkspaceId,
    cancel: CancellationToken,
}

#[derive(Default)]
pub struct McpRegistry {
    listeners: parking_lot::Mutex<HashMap<PathBuf, Listener>>,
}

impl McpRegistry {
    #[cfg(unix)]
    pub fn start(&self, daemon: Weak<Daemon>, id: &WorkspaceId, socket: &Path) -> Result<(), RpcError> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(old) = self.listeners.lock().remove(socket) {
            old.cancel.cancel();
        }
        let _ = std::fs::remove_file(socket);
        let listener = tokio::net::UnixListener::bind(socket).map_err(|e| RpcError::io(&e))?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| RpcError::io(&e))?;
        let cancel = CancellationToken::new();
        self.listeners.lock().insert(socket.to_path_buf(), Listener { workspace: id.clone(), cancel: cancel.clone() });
        let id = id.clone();
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((s, _)) => s,
                        Err(e) => { tracing::warn!(ws = %id, "mcp accept failed: {e}"); continue; }
                    },
                };
                let (r, w) = stream.into_split();
                let host = Arc::new(WorkspaceTools::new(daemon.clone(), id.clone()));
                let conn_cancel = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = conn_cancel.cancelled() => {}
                        _ = protocol::serve(r, w, host) => {}
                    }
                });
            }
        });
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn start(&self, _daemon: Weak<Daemon>, _id: &WorkspaceId, _socket: &Path) -> Result<(), RpcError> {
        Ok(())
    }

    pub fn stop_workspace(&self, id: &WorkspaceId) {
        let mut listeners = self.listeners.lock();
        let gone: Vec<PathBuf> = listeners.iter().filter(|(_, l)| &l.workspace == id).map(|(p, _)| p.clone()).collect();
        for path in gone {
            if let Some(l) = listeners.remove(&path) {
                l.cancel.cancel();
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}
```

- [ ] **Step 4: Wire it.**
  - `daemon.rs`: add the two fields; in `new`, `Ok(Arc::new_cyclic(|me| Self { me: me.clone(), mcp: Default::default(), … }))`; add `pub fn weak(&self) -> std::sync::Weak<Daemon> { self.me.clone() }`. Any other place constructing `Daemon { … }` (tests) must add the fields — `grep -rn "Daemon {" crates/daemon` and fix.
  - `lifecycle::start_sandbox`, right after `proxies.start(...)` succeeds: `if let Err(e) = d.mcp.start(d.weak(), &ws.id, &d.dirs.run(&ws.id).join(crate::mcp::SOCKET_FILE)) { tracing::warn!(ws = %ws.id, "agent tools unavailable: {}", e.message); }` — a failure costs the tools, never the workspace.
  - `lifecycle::tear_down_sandbox` and the destroy teardown (both call `d.proxies.stop(id)`): add `d.mcp.stop_workspace(id);` immediately after.
  - `agent_sandboxes::ensure`, after the `proxy.start(...)` call: the same warn-on-error `d.mcp.start(d.weak(), &ws.id, &spec.run_dir.join(crate::mcp::SOCKET_FILE))`.

- [ ] **Step 5: Run** `--test mcp_socket`, `--test workspace_integration`, `--test agent_integration`, `--lib` → pass.

- [ ] **Step 6: Commit** — `feat: serve agent tools on each sandbox's mcp.sock` (paths: `registry.rs`, `mcp/mod.rs`, `daemon.rs`, `lifecycle.rs`, `agent_sandboxes.rs`, `mcp_socket.rs`, plus any test files fixed for the new fields).

---

### Task 6: The `mcp-bridge` subcommand

**Files:**
- Create: `crates/daemon/src/mcp/bridge.rs`
- Modify: `crates/daemon/src/mcp/mod.rs` (`pub mod bridge;`), `crates/daemon/src/main.rs` (subcommand)
- Test: create `crates/daemon/tests/mcp_bridge.rs` (`#![cfg(unix)]`)

**Interfaces:**
- Produces: `pub async fn run(socket: &Path) -> anyhow::Result<()>` — connect (error → `anyhow` with the path), then `tokio::io::copy` stdin→socket write half and socket read half→stdout concurrently; when stdin hits EOF, `shutdown()` the socket write half and keep forwarding replies until the socket closes; return when the socket side ends. Non-unix: `bail!("mcp-bridge needs unix sockets")`. `main.rs`: `Cmd::McpBridge { #[arg(long)] socket: PathBuf }` → `tokio::runtime::Runtime::new()?.block_on(mcp::bridge::run(&socket))`, documented `/// Internal: connect an agent's stdio MCP client to the workspace's tools. Not for direct use.`

- [ ] **Step 1: Write the failing test:**

```rust
#![cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const DAEMON: &str = env!("CARGO_BIN_EXE_bondsymphonic-daemon");

#[test]
fn the_bridge_carries_a_request_and_its_reply() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("mcp.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"ping\":1}\n");
        let mut w = conn;
        w.write_all(b"{\"pong\":1}\n").unwrap();
        // Closing our end is what ends the bridge.
    });
    let mut child = Command::new(DAEMON)
        .args(["mcp-bridge", "--socket"])
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{\"ping\":1}\n").unwrap(); // stdin dropped → EOF
    let mut out = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut out).unwrap();
    assert_eq!(out, "{\"pong\":1}\n");
    server.join().unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_missing_socket_fails_with_its_path() {
    let out = Command::new(DAEMON)
        .args(["mcp-bridge", "--socket", "/nonexistent/mcp.sock"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/mcp.sock"));
}
```

- [ ] **Step 2: Run** `--test mcp_bridge` → fails (unknown subcommand).

- [ ] **Step 3: Implement** `bridge.rs` per the interface (mirror `net/shim.rs` style and its module doc: "makes no decisions; the tools are applied on the daemon side").

```rust
#[cfg(unix)]
pub async fn run(socket: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("{} is unreachable", socket.display()))?;
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let upstream = async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_daemon).await;
        use tokio::io::AsyncWriteExt;
        let _ = to_daemon.shutdown().await;
    };
    let downstream = async move {
        let mut stdout = tokio::io::stdout();
        let r = tokio::io::copy(&mut from_daemon, &mut stdout).await;
        use tokio::io::AsyncWriteExt;
        let _ = stdout.flush().await;
        r
    };
    let (_, down) = tokio::join!(upstream, downstream);
    down?;
    Ok(())
}
```

Note: `tokio::io::stdin()` reads on a blocking thread; with `join!` the process waits for stdin EOF even after the socket closes. The agent closes stdin when it ends the session, which is the normal end; acceptable. If the test hangs, switch to `tokio::select!` that returns when `downstream` completes.

- [ ] **Step 4: Run** `--test mcp_bridge` → pass.

- [ ] **Step 5: Commit** — `feat: mcp-bridge subcommand connecting agent stdio to the tools socket`.

---

### Task 7: Agent configuration and instructions

**Files:**
- Modify: `crates/daemon/src/mcp/mod.rs` (config helpers)
- Modify: `crates/daemon/src/agents/claude.rs` (`claude_argv`), `crates/daemon/src/agents/codex_backend.rs` (`prepare`), `crates/daemon/src/agents/mod.rs` (`SANDBOX_GIT_NOTE` text)

**Interfaces:**
- Produces in `mcp/mod.rs`:
  - `pub const DAEMON_IN_SANDBOX: &str = "/opt/bs/daemon";` (same value as `sandbox::linux_bwrap::DAEMON_IN_SANDBOX`; note in a comment that they must agree)
  - `pub fn bridge_args() -> [&'static str; 3]` → `["mcp-bridge", "--socket", SOCKET_IN_SANDBOX]`
  - `pub fn claude_mcp_config() -> String` → `{"mcpServers":{"bondsymphonic":{"type":"stdio","command":"/opt/bs/daemon","args":["mcp-bridge","--socket","/run/bs/mcp.sock"]}}}`
  - `pub fn claude_allowed_tools() -> String` → `"mcp__bondsymphonic__git_fetch,mcp__bondsymphonic__pr_view,mcp__bondsymphonic__ci_logs,mcp__bondsymphonic__issue_view"` (built from `tools::READ_ONLY_TOOLS`)
  - `pub fn codex_config() -> Vec<String>` → `["-c", "mcp_servers.bondsymphonic.command=\"/opt/bs/daemon\"", "-c", "mcp_servers.bondsymphonic.args=[\"mcp-bridge\",\"--socket\",\"/run/bs/mcp.sock\"]"]`

- [ ] **Step 1: Write the failing tests.**
  - In `mcp/mod.rs` tests: `claude_mcp_config()` parses as JSON with `mcpServers.bondsymphonic.command == DAEMON_IN_SANDBOX` and `args == bridge_args()`; `codex_config()` values parse as TOML-ish exactly as written (compare strings).
  - In `claude.rs`'s argv test (where `--append-system-prompt` is already asserted for `BWRAP`): assert the bwrap argv contains `--mcp-config` followed by `claude_mcp_config()` and `--allowedTools` followed by `claude_allowed_tools()`, and the `NOOP` argv contains neither.
  - In `codex_backend.rs`, if `prepare` has no unit test seam, add a small pure function `fn sandbox_args(backend: &str) -> Vec<String>` returning `codex_config()` for `"linux_bwrap"` and empty otherwise, test it, and call it from `prepare` (`argv.extend(sandbox_args(d.backend.name()))` before `app-server`).

- [ ] **Step 2: Run** `--lib mcp agents::claude agents::codex_backend` → failures.

- [ ] **Step 3: Implement.** In `claude_argv`, inside the existing `if backend == SANDBOXED_BACKEND { … }` block, push `"--mcp-config"`, `crate::mcp::claude_mcp_config()`, `"--allowedTools"`, `crate::mcp::claude_allowed_tools()` **before** `--append-system-prompt` (the existing test asserts the prompt is last). Rewrite `SANDBOX_GIT_NOTE`:

```rust
pub const SANDBOX_GIT_NOTE: &str = "You are running inside a BondSymphonic sandbox that holds no Git \
or GitHub credentials, so plain `git fetch`, `git pull`, `git push` and `gh` cannot authenticate, and \
logging in from inside the sandbox does not help. Use the `bondsymphonic` MCP tools instead: git_fetch \
(update origin/*), git_push (push this workspace's branch; force uses --force-with-lease), pr_create, \
pr_view, pr_update, pr_comment (this branch's pull request), ci_logs (CI status and failed logs for \
this branch) and issue_view. They run on the host as the user and can only act on this workspace's \
own branch. If a tool says GitHub is not logged in, tell the user to log in under Settings > Setup.";
```

Update the comment above it to mention the tools. Update `agents::claude` test expectations that compare the note if any.

- [ ] **Step 4: Run** the same filters plus `--test codex_backend --test codex_integration` → pass.

- [ ] **Step 5: Commit** — `feat: give sandboxed Claude and Codex agents the bondsymphonic tools`.

---

### Task 8: End to end in a real sandbox, and docs

**Files:**
- Test: append to `crates/daemon/tests/sandbox_integration.rs` (already `#![cfg(target_os = "linux")]` with `bwrap_available()`)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` (the `workspace.fetch` bullet gains a sentence pointing at the tools spec), `docs/user-guide.md` (a short "What agents can do with git and GitHub" section — find the section on agents/permissions with `grep -n "^## " docs/user-guide.md` and add it after)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the test.** Find how existing tests in `sandbox_integration.rs` start a daemon with the `linux_bwrap` backend and run a command in a workspace sandbox (search for `backend_for("linux_bwrap")` and `.spawn(SandboxCommand`). Reusing that setup, create a workspace, then spawn inside its sandbox:

```rust
argv: vec!["/bin/sh".into(), "-c".into(), format!(
    "printf '%s\\n' '{init}' '{list}' | {exe} mcp-bridge --socket /run/bs/mcp.sock",
    init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    exe = handle.helper_exe().display(),
)],
```

Collect stdout until both replies (two lines) arrive or 10 s pass; assert line 1 has `"serverInfo":{"name":"bondsymphonic"` and line 2 lists `git_push`. Gate on `bwrap_available()` like its neighbours (print SKIP and return).

Because the bridge exits only when the socket closes, close stdin after the two lines (the `printf |` pipe does) and accept that the process then waits for the daemon side: the server closes the connection when its reader hits EOF (Task 3 `serve` returns on EOF), so the bridge ends. If it does not, the test's timeout catches it — fix `serve`/`bridge`, do not lengthen the timeout.

- [ ] **Step 2: Run** `--test sandbox_integration mcp` → pass (or SKIP on a host without bwrap; this distro has it).

- [ ] **Step 3: Docs.** User guide section, four or five sentences: agents can fetch, push their own branch, open/update/comment on its PR, read CI logs and issues through the built-in BondSymphonic tools; nothing gives them your GitHub token; read-only tools run without asking, the rest follow the agent's permission mode; plain `git push`/`gh` inside an agent will not work by design; log in to GitHub under Settings → Setup.

- [ ] **Step 4: Full daemon suite once** (`cargo test -p bondsymphonic-daemon --release --no-fail-fast`) → only the known flaky `mixed_backends` may fail; rerun it alone to confirm.

- [ ] **Step 5: Commit** — `test: agent tools reachable from inside a real sandbox; document them`.
