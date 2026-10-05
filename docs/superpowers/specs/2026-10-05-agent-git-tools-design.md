# Brokered git and GitHub tools for sandboxed agents — design

Date: 2026-10-05. Status: approved direction (option A of the 2026-10-05
discussion); implementation follows `docs/superpowers/plans/2026-10-05-agent-git-tools.md`.

## Problem

Agents run in a bubblewrap sandbox that holds none of the user's credentials,
and in a worktree workspace the repository's shared `.git` (refs,
`packed-refs`, config) is mounted read-only. Both are deliberate: they are what
stops an agent pushing to `main`, rewriting the user's refs, or leaking the
user's GitHub token, whose `repo` scope covers every repository they can reach.

The consequence is that an agent cannot `git fetch`, `git push`, open or update
a pull request, or read CI results. A `git fetch` hangs on a credential prompt;
the agent then tells the user to `gh auth login` inside a home the daemon throws
away. The host-side fetch added on 2026-10-05 (`workspace.fetch`, automatic on
`Ready`) only covers fetching, and only when the daemon decides to.

## Decision

The daemon exposes a small, fixed set of git and GitHub **tools** to every
sandboxed agent over MCP. Each tool runs on the host with the user's own
credentials, and is scoped to the one workspace whose sandbox called it. The
token never enters the sandbox; an agent can push only its own workspace branch
and open or edit only that branch's pull request.

Rejected: injecting a token and credential helper into the sandbox (option B).
It gives every agent push and API rights to every repository the user can reach
and needs per-workspace writable ref plumbing. It may come later as an explicit
per-workspace opt-in; nothing here precludes it.

## Architecture

```
agent (Claude Code / Codex)            sandbox                  host
  stdio MCP client ── spawns ──► /opt/bs/daemon mcp-bridge ──► /run/bs/mcp.sock
                                   (pipes stdin/stdout)          │ (= <run_dir>/mcp.sock)
                                                                 ▼
                                                   daemon McpRegistry listener
                                                   bound to one workspace id
                                                                 │
                                                   tool handlers (git / gh on host)
```

### The bridge

A new daemon subcommand, `bondsymphonic-daemon mcp-bridge --socket <path>`,
follows the existing `proxy-shim` / `forward` pattern: a small async program
that connects to the Unix socket and copies bytes both ways (stdin → socket,
socket → stdout) until either side closes, then exits. It interprets nothing.
Inside the sandbox it is `/opt/bs/daemon`, which every sandbox already binds
read-only, so no new binary is distributed.

### The listener

`McpRegistry` (new, `crates/daemon/src/mcp/`) mirrors `ProxyRegistry`'s
lifecycle: `start(workspace_id, socket_path) -> generation`,
`stop_generation(id, generation)`, `stop(id)`. It is started wherever a proxy
is started for a sandbox and stopped wherever that proxy is stopped:

- the workspace sandbox: `<dirs.run(id)>/mcp.sock` (`lifecycle::start_sandbox`,
  teardown, destroy, restart);
- each companion agent sandbox (Codex): `<agent-run/<backend>/<id>>/mcp.sock`
  (`agent_sandboxes::ensure` and its teardown).

Both run directories are bound at `/run/bs` inside their sandbox, so the agent
always connects to `/run/bs/mcp.sock`. The socket file is created `0600`. A
connection is served for as long as it stays open; stopping a generation closes
its listener and its open connections.

The listener needs the daemon to run tools. `Daemon` gains a `Weak<Daemon>` to
itself (set at construction) that the registry upgrades per call; a call made
after the daemon is gone answers an error.

### The protocol

MCP over the stdio transport's framing: newline-delimited JSON-RPC 2.0, one
message per line, UTF-8, lines capped at 1 MiB (a longer line closes the
connection). The daemon implements the server side directly (no MCP crate):

- `initialize` → `serverInfo {name: "bondsymphonic", version}`,
  `capabilities {tools: {}}`, and `protocolVersion` = the client's requested
  version when it is one of `2025-06-18`, `2025-03-26`, `2024-11-05`, else
  `2025-06-18`;
- `notifications/initialized`, `notifications/cancelled` (aborts the named
  in-flight call) — no reply;
- `ping` → `{}`;
- `tools/list` → the fixed tool list below, with JSON Schemas and
  `annotations.readOnlyHint` on the read-only tools;
- `tools/call` → `{content: [{type: "text", text}], isError}`. A tool failure
  is `isError: true` with the reason as text (MCP's convention), never a
  JSON-RPC error; unknown tool or invalid arguments is JSON-RPC `-32602`;
- anything else with an id → `-32601`; malformed JSON → `-32700` with id null.

Requests on one connection run concurrently; responses carry the request's id
exactly as sent (number or string).

## The tools

All act on the workspace bound to the socket. Each re-reads the workspace at
call time and refuses unless it is `Ready`. GitHub tools need `origin` to be a
`github.com` remote (`https://github.com/o/r(.git)`, `git@github.com:o/r(.git)`,
`ssh://git@github.com/o/r(.git)`); they pass `--repo o/r` explicitly so `gh`
never guesses. Results are text, capped at 64 KiB (logs keep their tail).

| Tool | Arguments | What it does | Read-only |
|---|---|---|---|
| `git_fetch` | — | `git::fetch::fetch_repo` (existing host fetch). Says how many refs moved. | yes |
| `git_push` | `force?: bool` | Pushes the workspace branch `ws.branch` to `origin/<same name>` with `daemon_push_git`, under `repo_lock`, then `absorb_objects` (the push half of `create_pr`, extracted). `force` uses `--force-with-lease`. | no |
| `pr_create` | `title`, `body`, `draft?`, `base?` | Push as above, then `gh pr create --head <branch> --base <base or ws.base_branch>`. Returns the URL. If a PR for the branch already exists, returns its URL and says so. | no |
| `pr_view` | `number?` | `gh pr view` (default: the workspace branch's PR) with `--json number,url,state,title,body,isDraft,baseRefName,headRefName,mergeable,reviewDecision,statusCheckRollup,reviews,comments`. | yes |
| `pr_update` | `title?`, `body?`, `ready?` | Only the workspace branch's own PR: `gh pr edit` for title/body, `gh pr ready` / `gh pr ready --undo` for `ready`. | no |
| `pr_comment` | `body`, `number?` | `gh pr comment` (default: the branch's PR). | no |
| `ci_logs` | `run_id?` | Default: the newest run on the workspace branch (`gh run list --branch --limit 1`). Returns its status and, when it failed, `gh run view --log-failed` (tail). | yes |
| `issue_view` | `number` | `gh issue view --json number,title,body,state,labels,comments,url`. | yes |

In-place workspaces: `git_fetch` and the read-only GitHub tools work (they use
`ws.repo_path`); `git_push`, `pr_create`, `pr_update` answer the existing
`in_place::nothing_to_merge` refusal as text, as `workspace.create_pr` does.

`gh` and `git` run with the existing guards: `GIT_TERMINAL_PROMPT=0`,
`GH_PROMPT_DISABLED=1`, stdin null, a timeout (`GIT_TIMEOUT` / `GH_TIMEOUT`),
`kill_on_drop`. `gh` is found via the existing `gh_argv()` (`BS_GH_BIN` test
hook). Arguments from the agent are passed as separate argv entries, never
through a shell; a value starting with `-` is passed after its flag, which
`gh` treats as a value. Titles and bodies are not logged; tool name, workspace
and outcome are.

## Agent configuration

Only under the `linux_bwrap` backend (the `noop` backend runs agents with the
user's own credentials and needs none of this):

- **Claude Code**: `--mcp-config '{"mcpServers":{"bondsymphonic":{"type":"stdio","command":"/opt/bs/daemon","args":["mcp-bridge","--socket","/run/bs/mcp.sock"]}}}'`
  (adds to, never replaces, the user's own MCP servers — no
  `--strict-mcp-config`), and `--allowedTools` listing the four read-only
  tools (`mcp__bondsymphonic__git_fetch`, `…pr_view`, `…ci_logs`,
  `…issue_view`) so they never prompt. The write tools prompt or not according
  to the agent's permission mode, exactly like any other tool.
- **Codex**: `-c mcp_servers.bondsymphonic.command="/opt/bs/daemon"` and
  `-c mcp_servers.bondsymphonic.args=["mcp-bridge","--socket","/run/bs/mcp.sock"]`
  in the companion process config; approval follows its `approvalPolicy`.
- **Instructions**: `SANDBOX_GIT_NOTE` is rewritten to say: no credentials in
  the sandbox; use the `bondsymphonic` tools to fetch, push the workspace
  branch, open/update/comment on its PR and read CI and issues; plain
  `git push` / `gh` will not work; do not try to log in.

## What stays

`workspace.fetch`, the automatic fetch on `Ready`, the IDE's Fetch, Merge and
Create PR all stay; `pr_create` and the IDE's Create PR share one push
implementation.

## Failure behaviour

- Socket missing or daemon gone: the bridge exits non-zero; the agent CLI shows
  the server as failed and carries on without the tools.
- Workspace not `Ready`, destroyed, restarted: tools answer `isError` with the
  reason; a restart's new generation gets a fresh listener.
- `gh` missing or not logged in: `isError` with `gh`'s message plus "log in
  under Settings → Setup".
- Network down: the git/gh error text, `isError`.

## Testing

- Unit: JSON-RPC framing and dispatch (initialize version negotiation, string
  and numeric ids, unknown method, malformed line, oversize line, cancellation,
  concurrent calls); origin-URL parsing; argument validation per tool; Claude
  argv (`--mcp-config`, `--allowedTools` only under bwrap); Codex config.
- Integration (daemon, Linux): a workspace with a local bare `origin` — call
  `tools/call` over the real socket for `git_fetch` and `git_push` and see the
  remote branch move; `pr_create` / `pr_view` / `pr_comment` / `ci_logs` /
  `issue_view` against the `BS_GH_BIN` stub used by `pr_integration.rs`,
  asserting the exact `gh` argv including `--repo`; push refused for in-place.
- Bridge: `mcp-bridge` against a test listener carries a request and its reply
  both ways and exits when either side closes.
- Sandbox (bwrap, Linux): from inside a real workspace sandbox,
  `/opt/bs/daemon mcp-bridge --socket /run/bs/mcp.sock` answers `initialize`
  and `tools/list`; the socket is the only new path in the spec and no
  credential directory is mounted.
- Live (ignored test / manual): a real Claude Code agent in a sandbox lists the
  `bondsymphonic` tools and `git_fetch` succeeds.
