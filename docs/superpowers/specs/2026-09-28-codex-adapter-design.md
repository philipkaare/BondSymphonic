# Codex adapter — design

Date: 2026-09-28. Status: approved in chat by the user, section by section; this
spec awaits their review before planning. It is sub-project 1 of 2: the second,
an OpenCode adapter for OpenAI-compatible model servers (Ollama, vLLM, SGLang),
reuses the backend layer this one introduces and gets its own spec.

## 1. Goal

Run the OpenAI Codex CLI as an agent in BondSymphonic, beside Claude Code: in the
same workspaces, sandbox, network proxy, tabs and transcript. Choosing Codex in
the New Agent dialog gives an agent that streams its answer, shows its commands
and file changes as tool cards, can ask for approval in the amber bar, and
resumes its conversation after a restart.

Decisions made with the user:

- Local OpenAI-compatible models are **not** driven by Codex or by an agent loop
  of our own; they go through OpenCode (sub-project 2).
- Codex comes first and carries the generalisation of the adapter layer.
- Codex authenticates with **either** an OpenAI API key (wins when set) **or** a
  ChatGPT sign-in.
- The ChatGPT sign-in's rotating refresh token is handled by **one shared
  `CODEX_HOME`** bound into every Codex sandbox, with login failures surfaced the
  way Claude's are, and Setup suggesting an API key for many concurrent agents.

Out of scope: OpenCode (sub-project 2); an agent loop of our own; Codex cloud
tasks; Codex as an MCP server (removed upstream); per-command "ask every time"
approvals (Codex removed `untrusted`).

## 2. The backend layer (shared with sub-project 2)

Today `AgentAdapterKind` is `{Claude, Terminal}` (`crates/proto/src/types.rs:15`)
and `AgentManager::start` (`crates/daemon/src/agents/mod.rs:794-971`) builds
Claude unconditionally once it has refused `Terminal`.

- `AgentAdapterKind` gains `Codex` (and later `OpenCode`). Serde snake_case;
  `Capabilities.adapters` advertises it only when the daemon has a usable backend
  for it (binary found). No `PROTOCOL_VERSION` bump: the variant is additive and an
  older daemon refuses an unknown adapter with an error.
- A daemon trait `Backend` owns what differs per CLI:
  - the binary: host resolution, read-only bind path in the sandbox, version probe
    and pinned/tested version;
  - argv and the per-process environment (credentials given to the one command,
    never to the sandbox spec — as `agent_auth_env` does today);
  - the home seeding: which files are copied or bound into the workspace home;
  - the extra network hosts the agent needs (see §6);
  - the adapter constructor returning `Box<dyn AgentAdapter>`;
  - `list_models`.
- The Claude code moves behind `Backend` with **no behaviour change** (its tests
  are the proof). `agent.start` dispatches on `p.adapter`.
- Transcript: every adapter emits the existing `AgentMessageBody`
  (`UserText / AssistantText / AssistantDelta / ToolUse / ToolResult /
  PermissionRequest / Result / System`). The IDE's `TranscriptView` stays as is,
  apart from a small backend label on the pane.
- Prerequisites and Setup rows become per-backend and are only shown for a
  backend that is enabled (§5), so a Claude-only user sees no red Codex rows.
- New Agent dialog: a backend chooser (Claude, Codex; OpenCode later). The model
  and permission-mode lists follow the chosen backend; the model list comes from
  that backend's `list_models` (`system.list_models` gains an `adapter` param,
  default Claude). The preselected backend is the Settings default (§2.1).

### 2.1 Settings: the backends on equal footing

The user's requirement: Claude, Codex and OpenCode are **equals** in Settings. No
backend is the "real" one with the others bolted on.

- Settings → **Agents** has a small common part — *New agents use* (the default
  backend, among the enabled ones) and *Show turn cost and system lines* — and
  below it **one section per backend, all built from the same template**, shown
  as tabs *Claude | Codex | OpenCode*:
  1. **Enabled** checkbox;
  2. **Sign-in**: the backend's credential(s) — Claude: Anthropic API key and
     the Claude sign-in/long-lived token; Codex: OpenAI API key and the ChatGPT
     sign-in; OpenCode: its providers (sub-project 2) — each with its Setup
     buttons (install, log in, log out) right there;
  3. **Default model** (from that backend's `list_models`);
  4. **Default permission mode** (that backend's own modes).
- The template is data-driven from what the daemon reports per backend, so the
  OpenCode tab arrives with sub-project 2 without restructuring. Until then the
  dialog shows the tabs of the backends the daemon knows.
- Settings storage becomes per backend: `backends.<id>.{enabled,
  default_model, default_permission_mode}` plus `default_backend`. The existing
  top-level `default_permission_mode` migrates to `backends.claude` on first
  load; Claude is enabled by default, Codex and OpenCode are not.
- The "no mode named → the Settings default" rule (`start_options`, commit
  `f22c844`) uses the chosen backend's default mode; the same rule is added for
  the default model.
- The Setup page keeps the machine-wide rows (git, bwrap, userns, sandbox, gh)
  in a *System* group and shows one group per **enabled** backend with that
  backend's rows — Claude's too, so disabling Claude hides its rows exactly as a
  disabled Codex hides Codex's.
- Keys stay in the Windows credential store, one entry per backend
  (`anthropic_api_key`, `openai_api_key`).

## 3. The Codex adapter

### 3.1 Process and protocol
- One `codex app-server` per agent over stdio (JSON-RPC 2.0), inside the
  workspace sandbox. The binary is bound read-only at `/opt/bs/codex`, like
  `CLAUDE_IN_SANDBOX`. The version is pinned and probed like Claude's, because
  app-server is documented as experimental.
- Start: `initialize` → `initialized` → `thread/start` with the worktree as cwd,
  the model, the approval policy (§3.3) and sandbox `danger-full-access` — Codex's
  own sandbox is bubblewrap too and does not nest reliably; our bwrap and proxy
  are the boundary, which is also OpenAI's guidance for an outer sandbox.
- Resume: `thread/resume` with the stored `sessionId`, kept where Claude's session
  id is kept today, so Restart and the automatic resume behave the same.

### 3.2 Mapping to the transcript

| Codex | `AgentMessageBody` |
|---|---|
| `item/agentMessage/delta`, `item/completed` (agentMessage) | `AssistantDelta`, `AssistantText` |
| `commandExecution` started / outputDelta / completed | `ToolUse` "Bash" (command) / `ToolResult` (output, exit code) |
| `fileChange` | `ToolUse` "Edit" with the changed paths and diff / `ToolResult` |
| `mcpToolCall` | `ToolUse` / `ToolResult` |
| `reasoning`, `plan` deltas | not shown |
| `turn/completed` | `Result` (tokens; status completed / interrupted / failed) |
| failure with `codexErrorInfo` `Unauthorized` | detail that the IDE's auth-failure rule matches: Codex is marked not logged in |

Unrecognised notifications degrade to `System { subtype: "raw" }`, as
`claude_stream.rs` does.

### 3.3 Turns, interrupt, approvals
- A message starts `turn/start`; a message sent while a turn runs becomes
  `turn/steer`. The stop button sends `turn/interrupt`; stopping the agent closes
  stdin and kills the process as for Claude.
- `item/commandExecution/requestApproval` and `item/fileChange/requestApproval`
  become `PermissionRequest` in the amber bar; `agent.permission_reply` answers
  with `accept` / `decline` (`acceptForSession` for "always").
- Permission modes for Codex: **YOLO** (approval policy `never`, the default, as
  for Claude) and **"Ask when Codex wants to"** (`on-request`). The dialog says
  plainly that "ask before every command" is not available for Codex.

## 4. Authentication

- **API key**: Settings → Agents → Codex gets "OpenAI API key", stored in the
  Windows credential store (`openai_api_key`), sent per start like the
  Anthropic key and given to the agent process only. Because a bare
  `OPENAI_API_KEY` does not authenticate Codex, the daemon passes a provider
  config whose `env_key` names it (to be verified, §7).
- **ChatGPT sign-in**: Setup "Log in to Codex" runs `codex login --device-auth` in
  a host setup terminal; the device link is detected and opened as for Claude.
  "Log out of Codex" runs `codex logout`.
- **Shared `CODEX_HOME`**: `~/.bondsymphonic/codex-home/` holds `auth.json`, is the
  `CODEX_HOME` of the setup terminal, and is bound read-write into every Codex
  sandbox as its `CODEX_HOME`. A refresh by one agent is then on disk for all.
  A directory, not a single file, because a rename onto a bind-mounted file fails
  (to be verified, §7). Cost, accepted: Codex's own session files live there too,
  so a Codex agent can read other workspaces' Codex history.
- A login failure (turn failed `Unauthorized`, or exit detail naming the refresh)
  sets the IDE's auth-failure override for Codex, as for Claude; its fix hint
  names Log in to Codex and suggests an API key when several Codex agents run.
- Precedence: API key (if set) > ChatGPT sign-in > none (the agent start is
  refused with the prerequisite's message).

## 5. Setup and prerequisites

- New prerequisite rows `codex` (binary present, version) and `codex_auth` (API
  key set, or `auth.json` present in the shared `CODEX_HOME`).
- New setup actions: `InstallCodex` (`curl -fsSL https://chatgpt.com/codex/install.sh | sh`,
  literal argv like the others), `CodexLogin`, `CodexLogout`.
- Shown only when Codex is enabled in Settings → Agents → Codex (§2.1); the
  same rule now applies to Claude's rows.

## 6. Network

- The allowlist becomes backend-aware: a Codex agent's sandbox additionally
  allows `api.openai.com`, `auth.openai.com` and `chatgpt.com` (WebSocket
  upgrade included). Claude sandboxes are unchanged.
- The documented host list is incomplete, so the §7 check captures the real set
  from the proxy's denial log during a trial run, and the defaults are set from
  that.

## 7. Pre-implementation check (blocking)

Done in the distro before the plan is executed, like the token work's §3:

1. Install the pinned Codex; record the app-server message shapes actually
   emitted for a small turn with a command, a file change, an approval and an
   interrupt — these become the adapter's test fixtures.
2. API key via a provider `env_key` authenticates without `auth.json`.
3. How `auth.json` is written (rename vs in-place) — decides directory vs file
   bind.
4. The hosts a sign-in and a turn contact, from the proxy denial log.
5. `danger-full-access` inside our bwrap runs commands and applies patches.

If 2 fails, the API key is written with `codex login --with-api-key` into a
per-key `CODEX_HOME` instead; if 5 fails, the design comes back to the user.

## 8. Testing

- The Claude backend behind the trait: the existing Claude tests unchanged and
  green.
- Codex adapter: the §7 fixtures replayed through the mapper (every row of §3.2),
  approval request → reply round trip, steer vs start, interrupt, resume with a
  session id, unauthorized → auth-failure detail.
- Credentials: API key never on argv, in a log line or in the sandbox spec; the
  shared `CODEX_HOME` bind present only for Codex sandboxes.
- Allowlist: OpenAI hosts reachable only from Codex sandboxes.
- IDE: backend chooser, per-backend model and mode lists, the Settings tabs built
  from one template (same controls per backend), the settings migration of the
  old top-level permission mode, per-backend defaults applied by `start_options`,
  Setup groups hidden for disabled backends (Claude included).
- Daemon tests on Windows and in the distro; `cargo fmt --check` clean.
