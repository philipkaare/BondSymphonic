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
use bondsymphonic_proto::{RpcError, WorkspaceId, WorkspaceKind, WorkspaceState};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Weak};

/// The most text one tool result carries. An agent's context is the scarce
/// thing here: a PR with hundreds of comments or a CI log of megabytes would
/// crowd out everything else it is working with.
pub const TEXT_CAP: usize = 64 * 1024;

/// The tools that change nothing, which agent configurations allow without
/// asking (`annotations.readOnlyHint`).
pub const READ_ONLY_TOOLS: [&str; 4] = ["git_fetch", "pr_view", "ci_logs", "issue_view"];

/// What `pr_view` asks `gh` for: enough to follow a review without a browser.
const PR_FIELDS: &str = "number,url,state,title,body,isDraft,baseRefName,headRefName,\
mergeable,reviewDecision,statusCheckRollup,reviews,comments";
const ISSUE_FIELDS: &str = "number,title,body,state,labels,comments,url";

pub struct WorkspaceTools {
    /// Weak: the listener holding these tools must not keep a shutting-down
    /// daemon alive.
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

/// A git or daemon error, as the agent reads it.
fn failed(e: RpcError) -> ToolOutcome {
    ToolOutcome::Failed(e.message)
}

/// What `gh` says when it has no usable login. Specific phrases, not "auth":
/// that also matches "author" and "authorization", which a PR or a permission
/// error can mention without the login being the problem.
const NOT_LOGGED_IN: [&str; 5] = [
    "not logged in",
    "gh auth login",
    "authentication",
    "http 401",
    "bad credentials",
];

/// A `gh` error, with the way out of the commonest ones: the agent cannot log
/// in to GitHub or install software on the host from its sandbox, but it can
/// tell the user where to.
fn gh_failed(e: RpcError) -> ToolOutcome {
    let not_started = e
        .data
        .as_ref()
        .and_then(|d| d["stderr"].as_str())
        .is_some_and(|s| s.starts_with(gh::GH_NOT_STARTED));
    let mut text = e.message;
    let lower = text.to_lowercase();
    if not_started {
        text.push_str(
            " — the GitHub CLI (gh) is not installed on the host; ask the user to install it under Settings → Setup.",
        );
    } else if NOT_LOGGED_IN.iter().any(|p| lower.contains(p)) {
        text.push_str(
            " — if GitHub says you are not logged in, ask the user to log in under Settings → Setup.",
        );
    }
    ToolOutcome::Failed(text)
}

/// The largest index `<= at` that is a char boundary of `s`.
fn floor_boundary(s: &str, mut at: usize) -> usize {
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The first [`TEXT_CAP`] bytes of `s`: for JSON and prose, where the start
/// says what the thing is.
fn cap_head(s: String) -> String {
    cap_head_to(s, TEXT_CAP)
}

fn cap_head_to(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let cut = floor_boundary(&s, cap);
    format!("{}\n… [truncated]", &s[..cut])
}

/// The last `cap` bytes of `s`: for logs, where the failure is at the end.
fn cap_tail_to(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[… earlier output truncated]\n{}", &s[start..])
}

/// How much of a `ci_logs` result the failed log is always given, however
/// long the run's JSON is: the end of the log is what the agent came for.
const LOG_FLOOR: usize = 16 * 1024;

/// The run conclusions that have failed steps, and so a `--log-failed` to show.
/// `cancelled`, `skipped`, `neutral` and `success` have none.
const FAILED_CONCLUSIONS: [&str; 3] = ["failure", "timed_out", "startup_failure"];

fn invalid(msg: impl Into<String>) -> ToolOutcome {
    ToolOutcome::InvalidArguments(msg.into())
}

fn positive(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().filter(|n| *n > 0),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok().filter(|n| *n > 0)
        }
        _ => None,
    }
}

/// An optional positive-integer argument: `Ok(None)` when absent.
fn opt_number(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => positive(v)
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be a positive integer"))),
    }
}

fn req_number(args: &Map<String, Value>, key: &str) -> Result<u64, ToolOutcome> {
    opt_number(args, key)?.ok_or_else(|| invalid(format!("{key} is required")))
}

/// A required, non-empty string argument.
fn req_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, ToolOutcome> {
    opt_str(args, key)?.ok_or_else(|| invalid(format!("{key} is required")))
}

/// An optional string argument: absent → `None`; present, it must be a
/// non-empty string.
fn opt_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(Value::String(_)) => Err(invalid(format!("{key} must not be empty"))),
        Some(_) => Err(invalid(format!("{key} must be a string"))),
    }
}

fn opt_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, ToolOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(format!("{key} must be true or false"))),
    }
}

/// A call whose arguments have been checked, before anything is touched.
enum Call {
    Fetch,
    Push {
        force: bool,
    },
    PrCreate {
        title: String,
        body: String,
        draft: bool,
        base: Option<String>,
    },
    PrView {
        number: Option<u64>,
    },
    PrUpdate {
        title: Option<String>,
        body: Option<String>,
        ready: Option<bool>,
    },
    PrComment {
        body: String,
        number: Option<u64>,
    },
    CiLogs {
        run_id: Option<u64>,
    },
    IssueView {
        number: u64,
    },
}

/// The argument names each tool accepts; anything else is a mistake worth
/// telling the agent about rather than silently ignoring.
fn allowed(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "git_fetch" => &[],
        "git_push" => &["force"],
        "pr_create" => &["title", "body", "draft", "base"],
        "pr_view" => &["number"],
        "pr_update" => &["title", "body", "ready"],
        "pr_comment" => &["body", "number"],
        "ci_logs" => &["run_id"],
        "issue_view" => &["number"],
        _ => return None,
    })
}

fn parse(name: &str, args: &Value) -> Result<Call, ToolOutcome> {
    let keys = allowed(name).ok_or_else(|| invalid(format!("unknown tool {name}")))?;
    let empty = Map::new();
    let a = match args {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = a.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(invalid(format!("{name} takes no argument {k}")));
    }
    Ok(match name {
        "git_fetch" => Call::Fetch,
        "git_push" => Call::Push {
            force: opt_bool(a, "force")?.unwrap_or(false),
        },
        "pr_create" => Call::PrCreate {
            title: req_str(a, "title")?.to_owned(),
            // An empty body is a legitimate pull request; a missing one is not
            // a choice the agent made.
            body: match a.get("body") {
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(invalid("body must be a string")),
                None => return Err(invalid("body is required")),
            },
            draft: opt_bool(a, "draft")?.unwrap_or(false),
            base: opt_str(a, "base")?.map(str::to_owned),
        },
        "pr_view" => Call::PrView {
            number: opt_number(a, "number")?,
        },
        "pr_update" => {
            let call = Call::PrUpdate {
                title: opt_str(a, "title")?.map(str::to_owned),
                body: opt_str(a, "body")?.map(str::to_owned),
                ready: opt_bool(a, "ready")?,
            };
            if let Call::PrUpdate {
                title: None,
                body: None,
                ready: None,
            } = call
            {
                return Err(invalid("give at least one of title, body, ready"));
            }
            call
        }
        "pr_comment" => Call::PrComment {
            body: req_str(a, "body")?.to_owned(),
            number: opt_number(a, "number")?,
        },
        "ci_logs" => Call::CiLogs {
            run_id: opt_number(a, "run_id")?,
        },
        "issue_view" => Call::IssueView {
            number: req_number(a, "number")?,
        },
        _ => unreachable!("allowed() names every tool"),
    })
}

fn state_text(s: &WorkspaceState) -> String {
    match s {
        WorkspaceState::Creating => "still being created".into(),
        WorkspaceState::Ready => "ready".into(),
        WorkspaceState::SandboxDown => "its sandbox is down".into(),
        WorkspaceState::Error(e) => format!("in error: {e}"),
        WorkspaceState::Destroying => "being destroyed".into(),
    }
}

/// Builds an argv from string-likes.
fn argv<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.map(str::to_owned).to_vec()
}

impl WorkspaceTools {
    /// The daemon and the workspace as they are now, or the reason the call
    /// cannot go ahead.
    fn current(&self) -> Result<(Arc<Daemon>, Workspace), Refusal> {
        let d = self.daemon.upgrade().ok_or_else(|| {
            ToolOutcome::Failed("the BondSymphonic daemon is shutting down".into())
        })?;
        let ws = d.workspace(&self.workspace).map_err(failed)?;
        if ws.state != WorkspaceState::Ready {
            return Err(ToolOutcome::Failed(format!(
                "workspace {} is not ready: {}; try again once it is running",
                ws.name,
                state_text(&ws.state)
            )));
        }
        Ok((d, ws))
    }

    /// [`pr::push_branch`] on a task of its own, awaited.
    ///
    /// A tool call's future is dropped whenever the agent gives up on it —
    /// MCP `notifications/cancelled`, a client that hangs up, a listener
    /// stopped by a restart — and dropping `push_branch` between the push and
    /// its `absorb_objects` would release the repository lock and kill git
    /// (`kill_on_drop`), leaving `refs/remotes/origin/<branch>` naming objects
    /// that live only in the workspace's private directory, which destroy
    /// deletes. A spawned task is not cancelled with its waiter, so the push
    /// and the copy always finish together. It holds a strong `Arc<Daemon>`
    /// for that long, bounded by git's own timeouts.
    async fn push_detached(d: &Arc<Daemon>, ws: &Workspace, force: bool) -> Result<(), Refusal> {
        let (d, ws) = (Arc::clone(d), ws.clone());
        tokio::spawn(async move { pr::push_branch(&d, &ws, force).await })
            .await
            .map_err(|e| ToolOutcome::Failed(format!("the push task failed: {e}")))?
            .map_err(failed)
    }

    async fn slug(d: &Daemon, ws: &Workspace) -> Result<String, Refusal> {
        gh::origin_slug(&d.git, &ws.repo_path).await.map_err(failed)
    }

    /// Runs `gh`; `what` is the subcommand an error names, never the
    /// agent's prose.
    async fn gh(ws: &Workspace, args: Vec<String>, what: &str) -> Result<String, Refusal> {
        gh::run_gh(&ws.repo_path, &args, what)
            .await
            .map(|o| o.stdout)
            .map_err(gh_failed)
    }

    async fn run(&self, name: &str, args: Value) -> Result<String, Refusal> {
        // Arguments first: a malformed call is the agent's to fix whatever
        // state the workspace is in.
        let call = parse(name, &args)?;
        let (d, ws) = self.current()?;
        let b = ws.branch.as_str();
        match call {
            Call::Fetch => {
                let r = fetch::fetch_repo(&ws.repo_path, &d.dirs.no_hooks())
                    .await
                    .map_err(failed)?;
                Ok(if !r.has_origin {
                    "This repository has no origin remote.".into()
                } else if r.updated == 0 {
                    "Fetched origin: already up to date.".into()
                } else {
                    format!("Fetched origin: {} refs updated.", r.updated)
                })
            }
            Call::Push { force } => {
                Self::push_detached(&d, &ws, force).await?;
                Ok(format!("Pushed {b} to origin."))
            }
            Call::PrCreate {
                title,
                body,
                draft,
                base,
            } => {
                // Refused in place before the slug, so the agent hears the
                // reason that applies rather than one about the remote.
                if ws.kind == WorkspaceKind::InPlace {
                    return Err(failed(crate::workspace::in_place::nothing_to_merge()));
                }
                // The slug before the push: nothing is published for a
                // pull request that cannot be opened.
                let slug = Self::slug(&d, &ws).await?;
                Self::push_detached(&d, &ws, false).await?;
                let base = base.unwrap_or_else(|| ws.base_branch.clone());
                let mut args = argv([
                    "pr", "create", "--repo", &slug, "--title", &title, "--body", &body, "--head",
                    b, "--base", &base,
                ]);
                if draft {
                    args.push("--draft".into());
                }
                match gh::run_gh(&ws.repo_path, &args, "gh pr create").await {
                    Ok(o) => Ok(cap_head(o.stdout)),
                    Err(e) if e.message.contains("already exists") => {
                        let url = Self::gh(
                            &ws,
                            argv([
                                "pr", "view", b, "--repo", &slug, "--json", "url", "--jq", ".url",
                            ]),
                            "gh pr view",
                        )
                        .await?;
                        Ok(format!(
                            "A pull request for {b} already exists: {}",
                            url.trim()
                        ))
                    }
                    Err(e) => Err(gh_failed(e)),
                }
            }
            Call::PrView { number } => {
                let slug = Self::slug(&d, &ws).await?;
                let target = number.map_or_else(|| b.to_owned(), |n| n.to_string());
                let out = Self::gh(
                    &ws,
                    argv(["pr", "view", &target, "--repo", &slug, "--json", PR_FIELDS]),
                    "gh pr view",
                )
                .await?;
                Ok(cap_head(out))
            }
            Call::PrUpdate { title, body, ready } => {
                if ws.kind == WorkspaceKind::InPlace {
                    return Err(failed(crate::workspace::in_place::nothing_to_merge()));
                }
                let slug = Self::slug(&d, &ws).await?;
                let mut done = Vec::new();
                if title.is_some() || body.is_some() {
                    let mut args = argv(["pr", "edit", b, "--repo", &slug]);
                    if let Some(t) = &title {
                        args.extend(argv(["--title", t]));
                        done.push("title");
                    }
                    if let Some(t) = &body {
                        args.extend(argv(["--body", t]));
                        done.push("body");
                    }
                    Self::gh(&ws, args, "gh pr edit").await?;
                }
                match ready {
                    Some(true) => {
                        Self::gh(
                            &ws,
                            argv(["pr", "ready", b, "--repo", &slug]),
                            "gh pr ready",
                        )
                        .await?;
                        done.push("marked ready for review");
                    }
                    Some(false) => {
                        Self::gh(
                            &ws,
                            argv(["pr", "ready", b, "--repo", &slug, "--undo"]),
                            "gh pr ready --undo",
                        )
                        .await?;
                        done.push("converted to draft");
                    }
                    None => {}
                }
                Ok(format!(
                    "Updated the pull request for {b}: {}.",
                    done.join(", ")
                ))
            }
            Call::PrComment { body, number } => {
                let slug = Self::slug(&d, &ws).await?;
                let target = number.map_or_else(|| b.to_owned(), |n| n.to_string());
                let out = Self::gh(
                    &ws,
                    argv(["pr", "comment", &target, "--repo", &slug, "--body", &body]),
                    "gh pr comment",
                )
                .await?;
                let out = out.trim();
                Ok(if out.is_empty() {
                    format!("Commented on pull request {target}.")
                } else {
                    cap_head(out.to_owned())
                })
            }
            Call::CiLogs { run_id } => {
                let slug = Self::slug(&d, &ws).await?;
                let id = match run_id {
                    Some(id) => id.to_string(),
                    None => {
                        let out = Self::gh(
                            &ws,
                            argv([
                                "run",
                                "list",
                                "--repo",
                                &slug,
                                "--branch",
                                b,
                                "--limit",
                                "1",
                                "--json",
                                "databaseId,status,conclusion,name,url",
                            ]),
                            "gh run list",
                        )
                        .await?;
                        let runs: Value = serde_json::from_str(out.trim()).map_err(|e| {
                            ToolOutcome::Failed(format!(
                                "gh run list printed something unexpected: {e}"
                            ))
                        })?;
                        match runs.get(0).and_then(|r| r.get("databaseId")) {
                            None => return Ok(format!("No CI runs for {b} yet.")),
                            Some(Value::Number(n)) => n.to_string(),
                            Some(other) => {
                                return Err(ToolOutcome::Failed(format!(
                                    "gh run list gave an unexpected run id: {other}"
                                )))
                            }
                        }
                    }
                };
                let view = Self::gh(
                    &ws,
                    argv([
                        "run",
                        "view",
                        &id,
                        "--repo",
                        &slug,
                        "--json",
                        "status,conclusion,name,url,jobs",
                    ]),
                    "gh run view",
                )
                .await?;
                let failure = serde_json::from_str::<Value>(view.trim())
                    .ok()
                    .is_some_and(|v| {
                        v["conclusion"]
                            .as_str()
                            .is_some_and(|c| FAILED_CONCLUSIONS.contains(&c))
                    });
                // One budget for the whole result: the JSON leaves the log at
                // least LOG_FLOOR, and the log gets whatever the JSON left.
                let view_cap = if failure {
                    TEXT_CAP - LOG_FLOOR
                } else {
                    TEXT_CAP
                };
                let mut text = cap_head_to(view, view_cap);
                if failure {
                    let log = Self::gh(
                        &ws,
                        argv(["run", "view", &id, "--repo", &slug, "--log-failed"]),
                        "gh run view --log-failed",
                    )
                    .await?;
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str("\nLog of the failed steps:\n");
                    let room = TEXT_CAP.saturating_sub(text.len());
                    text.push_str(&cap_tail_to(log, room));
                }
                Ok(text)
            }
            Call::IssueView { number } => {
                let slug = Self::slug(&d, &ws).await?;
                let out = Self::gh(
                    &ws,
                    argv([
                        "issue",
                        "view",
                        &number.to_string(),
                        "--repo",
                        &slug,
                        "--json",
                        ISSUE_FIELDS,
                    ]),
                    "gh issue view",
                )
                .await?;
                Ok(cap_head(out))
            }
        }
    }
}

fn spec(name: &'static str, description: &'static str, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name,
        description,
        input_schema,
        read_only: READ_ONLY_TOOLS.contains(&name),
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[async_trait]
impl ToolHost for WorkspaceTools {
    fn tools(&self) -> Vec<ToolSpec> {
        let number = |what: &str| json!({"type": "integer", "minimum": 1, "description": what});
        vec![
            spec(
                "git_fetch",
                "Fetch origin on the host so this workspace sees the latest remote branches \
                 (origin/*). The sandbox cannot fetch for itself.",
                schema(json!({}), &[]),
            ),
            spec(
                "git_push",
                "Push this workspace's branch to origin. Only this workspace's own branch can be \
                 pushed. Set force to overwrite after a rebase (uses --force-with-lease).",
                schema(json!({"force": {"type": "boolean"}}), &[]),
            ),
            spec(
                "pr_create",
                "Push this workspace's branch and open a GitHub pull request for it. Only this \
                 workspace's own branch can be the head. Returns the URL; if a pull request for \
                 the branch already exists, returns that one's URL.",
                schema(
                    json!({
                        "title": {"type": "string"},
                        "body": {"type": "string"},
                        "draft": {"type": "boolean"},
                        "base": {"type": "string", "description": "Base branch; defaults to the workspace's base branch"},
                    }),
                    &["title", "body"],
                ),
            ),
            spec(
                "pr_view",
                "Show a GitHub pull request as JSON: state, reviews, comments and checks. \
                 Defaults to the pull request for this workspace's branch.",
                schema(
                    json!({"number": number("Pull request number; defaults to this workspace's branch's PR")}),
                    &[],
                ),
            ),
            spec(
                "pr_update",
                "Edit the title or body of the pull request for this workspace's branch, or mark \
                 it ready for review (ready: true) or back to draft (ready: false). Only this \
                 workspace's own pull request can be changed.",
                schema(
                    json!({
                        "title": {"type": "string"},
                        "body": {"type": "string"},
                        "ready": {"type": "boolean"},
                    }),
                    &[],
                ),
            ),
            spec(
                "pr_comment",
                "Comment on a GitHub pull request. Defaults to the pull request for this \
                 workspace's branch.",
                schema(
                    json!({
                        "body": {"type": "string"},
                        "number": number("Pull request number; defaults to this workspace's branch's PR"),
                    }),
                    &["body"],
                ),
            ),
            spec(
                "ci_logs",
                "Show the status of a GitHub Actions run and, when it failed, the log of its \
                 failed steps (the end of it). Defaults to the newest run on this workspace's \
                 branch.",
                schema(
                    json!({"run_id": number("Run id; defaults to the newest run on this workspace's branch")}),
                    &[],
                ),
            ),
            spec(
                "issue_view",
                "Show a GitHub issue of this workspace's repository as JSON, with its comments.",
                schema(json!({"number": number("Issue number")}), &["number"]),
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value) -> ToolOutcome {
        let result = self.run(name, args).await;
        // The tool and the outcome only: titles, bodies and comments are the
        // agent's prose and stay out of the log.
        tracing::info!(ws = %self.workspace, tool = name, ok = result.is_ok(), "agent tool");
        match result {
            Ok(t) => ToolOutcome::Ok(t),
            Err(o) => o,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_not_capped() {
        assert_eq!(cap_head("abc".into()), "abc");
        assert_eq!(cap_tail_to("abc".into(), TEXT_CAP), "abc");
    }

    #[test]
    fn caps_cut_on_char_boundaries() {
        // 3-byte chars, so TEXT_CAP falls inside one.
        let s = "€".repeat(TEXT_CAP);
        let h = cap_head(s.clone());
        assert!(h.ends_with("\n… [truncated]"));
        assert!(h.len() <= TEXT_CAP + 20);
        let t = cap_tail_to(s, TEXT_CAP);
        assert!(t.starts_with("[… earlier output truncated]\n"));
        assert!(t.len() <= TEXT_CAP + 40);
    }

    #[test]
    fn numbers_are_positive_integers() {
        for ok in [json!(1), json!("42")] {
            assert!(positive(&ok).is_some(), "{ok}");
        }
        for bad in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("abc"),
            json!(""),
            json!("-1"),
            json!(true),
        ] {
            assert!(positive(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn unknown_arguments_are_refused() {
        assert!(matches!(
            parse("git_push", &json!({"branch": "main"})),
            Err(ToolOutcome::InvalidArguments(_))
        ));
        assert!(matches!(parse("git_fetch", &Value::Null), Ok(Call::Fetch)));
    }

    #[test]
    fn auth_failures_point_at_setup() {
        let ToolOutcome::Failed(t) = gh_failed(RpcError::invalid_params("gh: not logged in"))
        else {
            panic!()
        };
        assert!(t.contains("Settings → Setup"));
        let ToolOutcome::Failed(t) = gh_failed(RpcError::invalid_params("HTTP 404")) else {
            panic!()
        };
        assert!(!t.contains("Setup"));
        for login in [
            "HTTP 401: Bad credentials",
            "To get started with GitHub CLI, please run:  gh auth login",
            "authentication required",
        ] {
            let ToolOutcome::Failed(t) = gh_failed(RpcError::invalid_params(login)) else {
                panic!()
            };
            assert!(t.contains("log in under Settings → Setup"), "{login}");
        }
        // "auth" inside other words is not a login problem.
        for other in [
            "GraphQL: author is not a collaborator",
            "Resource not accessible by integration: authorization denied for this action",
        ] {
            let ToolOutcome::Failed(t) = gh_failed(RpcError::invalid_params(other)) else {
                panic!()
            };
            assert!(!t.contains("Setup"), "{other}");
        }
    }

    #[test]
    fn a_gh_that_cannot_start_points_at_installing_it() {
        let e = crate::git::git_error(
            "gh pr view",
            None,
            &format!(
                "{} gh: No such file or directory (os error 2)",
                gh::GH_NOT_STARTED
            ),
        );
        let ToolOutcome::Failed(t) = gh_failed(e) else {
            panic!()
        };
        assert!(t.contains("not installed on the host"), "{t}");
        assert!(t.contains("Settings → Setup"), "{t}");
        // A `gh` that ran and failed is not "not installed".
        let ToolOutcome::Failed(t) = gh_failed(crate::git::git_error(
            "gh pr view",
            Some(1),
            "no pull requests found",
        )) else {
            panic!()
        };
        assert!(!t.contains("not installed"), "{t}");
    }
}
