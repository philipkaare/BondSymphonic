# User-configured MCP servers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users add MCP servers once in Settings (remote Streamable HTTP with no auth, headers, bearer or OAuth, or sandbox-local stdio), and the tools are usable from Claude, Codex and OpenCode agents in every workspace that has them enabled. Jira (Atlassian) is a removable preset.

**Architecture:** A daemon-owned registry (`mcp/servers/`) persists configuration and private secrets. At agent start the daemon resolves the enabled servers for (workspace, backend). Each remote server gets a per-sandbox socket `<run_dir>/mcp-<id>.sock`, where a broker forwards MCP messages to the upstream server over curl-driven HTTPS and holds any OAuth tokens. Each local server gets a `mcp-exec` wrapper with a `0600` env file. Each backend receives one native MCP entry per server, next to the existing `bondsymphonic` git-tools entry.

**Tech Stack:** Rust (tokio, serde, existing `RpcError`/request conventions), host `curl` for HTTPS, cxx-qt + Qt Widgets for the IDE.

**Spec:** `docs/superpowers/specs/2026-09-30-sandbox-mcp-design.md`. Read the whole spec, especially the final section, "Rebase onto main (2026-10-22)", which overrides the earlier text where they differ.

**Archive:** `origin/wip/codex-adapter-2026-09-30` (commit `55d80c2`) holds a first implementation against an older main. Tasks name the archive files to port. Read a file with `git show wip/codex-adapter-2026-09-30:<path>`, copy it, then adapt it to the interfaces below. Never merge or cherry-pick the branch. The archive's token-based `McpBinding`/`McpLaunchLease`, its OpenCode provider work and its harness updates are out of scope.

## Global Constraints

- The server name `bondsymphonic` is reserved for the built-in git tools. A server ID matches `^[a-z0-9][a-z0-9_-]{0,31}$`.
- Jira preset: ID `jira`, name `Jira (Atlassian)`, URL `https://mcp.atlassian.com/v2/mcp`, OAuth with discovery (no hard-coded endpoints), enabled for `claude`, `codex` and `opencode`, on by default in every workspace. It is seeded once. Deleting it is permanent until the user chooses "Restore Jira preset".
- A missing or expired login never blocks an agent from starting. The server's tools just fail with a "not connected" MCP error.
- Remote credentials (tokens, bearer values, header values, client secrets) never appear in sandbox mounts, agent argv or env, the daemon log, transcripts, list/export responses or `Debug` output.
- Local-server secrets reach only that server's process, through `<run_dir>/mcp-env/<id>.env` (`0600`), and never through argv.
- Remote transport is Streamable HTTP over `https://` only. An import of legacy SSE or `http://` is reported as unsupported, never dropped silently. Loopback `http://` is the exception for tests, behind `BS_MCP_ALLOW_HTTP=1`.
- curl runs with `-q` first (no `~/.curlrc`), no `-L`, `--proto =https`, `--max-filesize 4194304`, and `--max-time 60` (or 600 for a streamed `POST`), matching `agents/opencode_providers.rs:762`.
- The server set is resolved at agent start. Disable, remove, disconnect and workspace override `disabled` stop the affected sockets at once. Additions apply at the next agent start.
- New RPCs are additive (`mcp.*` names below). The existing requests and the `bondsymphonic` entry stay byte-identical.
- Self-test hooks follow the project process rule: a throwaway workspace only, and its ID is asserted before every action.

## Review Focus

1. **A token refresh races a disconnect.** A refresh that completes after Disconnect must not bring the token back (`refresh_after_disconnect_is_discarded`, Task 3).
2. **A server disabled while an agent runs.** The agent's next tool call fails with an MCP error, and the agent keeps running (`disable_stops_live_socket`, Task 4).
3. **A user named their own native MCP server `jira` in the repo's `.mcp.json` or `~/.codex/config.toml`.** The product entry is skipped for that backend with a logged warning. The user's entry still wins and nothing is overwritten (`native_name_collision_skips_product_entry`, Task 5).
4. **An upstream that answers with an SSE stream, splits events across chunks or uses CRLF framing.** The responses are still delivered, matched to the right request ID (`sse_fragmented_crlf_events`, Task 3).
5. **The IDE closes, or the browser never returns, during an OAuth login.** The pending attempt expires after 10 minutes, and a later callback with its state is rejected (`expired_attempt_rejected`, Task 3).

---

### Task 1: Protocol contracts

**Files:**
- Create: `crates/proto/src/mcp.rs` (port `crates/proto/src/mcp.rs` from the archive)
- Modify: `crates/proto/src/lib.rs`, `crates/proto/src/request.rs` (add variants and `method()` names)
- Test: `crates/proto/tests/mcp.rs` (port), `crates/proto/tests/roundtrip.rs` (append)

**Interfaces:**
- Produces, keeping the archive's shapes: `McpServerId`, `McpServerConfig { id, name, transport, enabled, default_workspace_enabled, enabled_backends: Vec<String> }`, `McpTransportConfig::{Stdio { command, args, env: BTreeMap<String,String>, secret_env: Vec<String> }, Http { url, auth }}`, `McpAuthConfig::{None, Bearer {..}, Headers {..}, OAuth { client_id, client_metadata_url, client_secret, scopes }}`, `McpWorkspaceOverride::{Inherit, Enabled, Disabled}`, `McpRegistrySnapshot`, `McpSecretValue` (its Debug prints `[REDACTED]`), `McpSecretWrite::{Keep, Set, Remove}`, `McpConnectionState::{Disconnected, Connecting, Connected, Error}`, `McpServerStatus`, `McpListResult`, `McpConnectResult { authorization_url, expires_at }`, `McpImportPreviewResult`, `McpTestResult { status, tool_names }`.
- Request variants: `mcp.list`, `mcp.upsert`, `mcp.remove`, `mcp.import_preview`, `mcp.import_apply`, `mcp.export`, `mcp.restore_preset`, `mcp.override`, `mcp.connect`, `mcp.callback`, `mcp.disconnect`, `mcp.test`. These use the archive's param types. `McpTestParams` drops `workspace_id` and `backend`, because the test runs host-side against the upstream.
- Event: `Event::McpChanged { revision: u64 }`, added in `crates/proto/src/event.rs` and sent after any registry or status change.

- [ ] **Step 1:** Port `crates/proto/tests/mcp.rs`. Keep `mcp_additive_defaults`, `mcp_secret_debug_redacted` (a sentinel `"s3cr3t-sentinel"` must not appear in `format!("{:?}")` of any params or the result) and `mcp_transport_roundtrip`. Add `reserved_and_invalid_ids_rejected_by_validate` only if validation lives in proto. Otherwise it goes to Task 2. Append a roundtrip of `Event::McpChanged` to `roundtrip.rs`.
- [ ] **Step 2:** Run `cargo test -p bondsymphonic-proto` and expect a compile FAIL.
- [ ] **Step 3:** Port the types and add the variants and event.
- [ ] **Step 4:** Run `cargo test -p bondsymphonic-proto` and expect PASS. Also run `cargo clippy -p bondsymphonic-proto --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit `feat(proto): MCP server registry contracts`.

### Task 2: Registry, secrets and import/export

**Files:**
- Create: `crates/daemon/src/mcp/servers/mod.rs` (`McpServers`, ported from the archive's `mcp/mod.rs` registry half), `servers/config.rs` (port), `servers/secrets.rs` (port)
- Modify: `crates/daemon/src/mcp/mod.rs` (`pub mod servers;`), `crates/daemon/src/daemon.rs` (field `pub mcp_servers: Arc<McpServers>`, loaded from `<data_dir>/mcp`), `crates/daemon/src/server/handlers.rs` (list/upsert/remove/import/export/restore_preset/override)
- Test: `crates/daemon/tests/mcp_config.rs` (port and extend)

**Interfaces:**
- Produces: `McpServers::load(data_dir: &Path) -> Result<Self, RpcError>`, `snapshot() -> McpRegistrySnapshot`, `upsert(McpUpsertParams) -> Result<McpRegistrySnapshot, RpcError>`, `remove(&McpServerId)`, `set_override(McpOverrideParams)`, `import_preview(&McpImportParams) -> Result<McpImportPreviewResult, RpcError>`, `import_apply(McpImportParams)`, `export() -> String` (JSON with secret references only), `restore_preset()`, `resolve(ws: &WorkspaceId, backend: AgentAdapterKind) -> Vec<McpServerConfig>`, `secret(&McpServerId, name: &str) -> Option<McpSecretValue>` (crate-private), `subscribe() -> watch::Receiver<u64>`.
- `validate` rejects the ID `bondsymphonic`, IDs outside the pattern, a non-`https` URL (unless `BS_MCP_ALLOW_HTTP=1` and the host is loopback) and an empty stdio command.

- [ ] **Step 1:** Port the archive's `mcp_config.rs` tests. They must include `preset_removed_stays_removed`, `revision_conflict_preserves_config`, `import_requires_collision_decision`, `export_omits_secret_values`, `workspace_override_inherits` and `secrets_file_is_0600` (unix). Add `reserved_id_rejected` (`bondsymphonic` gives `InvalidParams`) and `sse_import_reported_unsupported` (an `mcpServers` entry `{"type":"sse","url":…}` shows up in `issues` with field `type`, not in `servers`).
- [ ] **Step 2:** Run `cargo test -p bondsymphonic-daemon --test mcp_config` and expect FAIL.
- [ ] **Step 3:** Implement the ported code with the changes above. Persist with `crate::util::atomic::write_atomic`. Handlers send `Event::McpChanged` after every successful mutation.
- [ ] **Step 4:** Run the same test target and expect PASS. Also run `cargo test -p bondsymphonic-daemon --lib`.
- [ ] **Step 5:** Commit `feat(daemon): persist user MCP server registry`.

### Task 3: Remote transport and OAuth

**Files:**
- Create: `crates/daemon/src/mcp/servers/transport.rs` (port), `servers/auth.rs` (port)
- Modify: `servers/mod.rs` (status map, `connect`/`callback`/`disconnect`/`test`), `server/handlers.rs`
- Test: `crates/daemon/tests/mcp_transport.rs` and `crates/daemon/tests/mcp_auth.rs` (port; their loopback fake servers need `BS_MCP_ALLOW_HTTP=1`)

**Interfaces:**
- Consumes: Task 2's `McpServers` and secrets.
- Produces: `trait HttpClient: Send + Sync { async fn send(&self, req: HttpRequest, cancel: CancellationToken) -> Result<HttpResponse, RpcError> }` with `CurlClient` as the production implementation. `RemoteSession::open(servers: Arc<McpServers>, id: McpServerId, http: Arc<dyn HttpClient>) -> RemoteSession`. `RemoteSession::request(&self, msg: serde_json::Value) -> Result<Vec<serde_json::Value>, RpcError>` returns the reply plus any notifications. `RemoteSession::close()`. `McpServers::connect(id, redirect_uri) -> McpConnectResult`, `callback(McpCallbackParams)`, `disconnect(&id)`, `test(&id) -> McpTestResult` (initialize plus `tools/list`), `status(&id) -> McpServerStatus`.
- The curl flags are the Global Constraints values. Headers, including auth, go in a curl config read from stdin (`--config -`). The request body goes in a `0600` temp file passed as `--data-binary @<path>` and is deleted afterwards. Neither ever goes through argv.

- [ ] **Step 1:** Port the tests. They must include `oauth_wrong_state_rejected`, `oauth_wrong_issuer_rejected`, `refresh_single_flight`, `disconnect_invalidates_pending_exchange`, `http_session_headers_isolated`, `redirect_cannot_escape_origin`, plus Review Focus `refresh_after_disconnect_is_discarded`, `sse_fragmented_crlf_events` and `expired_attempt_rejected` (the attempt TTL is 600 s, with the clock injected or paused through `tokio::time::pause`). Add `curl_argv_carries_no_secret`: build the `CurlClient` command for a bearer request with value `"s3cr3t-sentinel"` and assert that no argv element contains it.
- [ ] **Step 2:** Run `cargo test -p bondsymphonic-daemon --test mcp_transport --test mcp_auth` and expect FAIL.
- [ ] **Step 3:** Implement from the ported code. The token is stored through Task 2's secrets under the server's ID, with a generation counter that Disconnect bumps.
- [ ] **Step 4:** Run the same command and expect PASS.
- [ ] **Step 5:** Commit `feat(daemon): remote MCP transport and OAuth`.

### Task 4: Per-sandbox sockets, broker and `mcp-exec`

**Files:**
- Create: `crates/daemon/src/mcp/servers/broker.rs` (new; reuse the archive's `broker.rs` `serve` loop logic only), `crates/daemon/src/mcp/exec.rs`
- Modify: `crates/daemon/src/mcp/registry.rs` (generalise `McpRegistry::start` to take a `ListenerKind::{Tools, Server(McpServerId)}`, and add `stop_server(&McpServerId)` and `stop_server_in(&WorkspaceId, &McpServerId)`), `crates/daemon/src/main.rs` (subcommand `mcp-exec --env-file <path> -- <cmd> <args…>`), `crates/daemon/src/daemon.rs` (a task that watches `McpServers::subscribe()` and stops the sockets of servers that became disabled, removed, disconnected or overridden off)
- Test: `crates/daemon/tests/mcp_sandbox.rs` (new; a fake HTTPS upstream on loopback with `BS_MCP_ALLOW_HTTP=1`)

**Interfaces:**
- Produces: `McpRegistry::start(&self, daemon: Weak<Daemon>, ws: &WorkspaceId, socket: &Path, kind: ListenerKind) -> Result<(), RpcError>`. `pub fn server_socket_file(id: &McpServerId) -> String` returns `mcp-<id>.sock`. `pub fn server_socket_in_sandbox(id) -> String` returns `/run/bs/mcp-<id>.sock`. `exec::run(env_file: &Path, argv: &[OsString]) -> !` reads `KEY=VALUE` lines and `execvp`s the command.
- The broker opens one `RemoteSession` per accepted connection and forwards every line-delimited JSON-RPC message, preserving request IDs. If the server isn't connected, every request is answered with JSON-RPC error `-32001` and message `"<name> is not connected — connect it in Settings › MCP servers"`.

- [ ] **Step 1:** Write the tests:
  - `remote_tool_call_through_socket`: initialize, then `tools/list`, then `tools/call` through the socket reach the fake upstream.
  - `disable_stops_live_socket`: after `upsert` with `enabled:false`, the next request on an open connection gets EOF, and the socket file is gone.
  - `override_disabled_stops_only_that_workspace`.
  - `unconnected_server_answers_32001`.
  - `two_connections_have_independent_sessions`: the upstream sees two different `Mcp-Session-Id` values.
  - `exec_sets_env_and_keeps_argv_clean`: `mcp-exec` runs `/usr/bin/env`, and the output contains `TOKEN=s3cr3t-sentinel`.
  - `env_file_is_0600`.
  - Gate the socket tests `#[cfg(unix)]`.
- [ ] **Step 2:** Run `scripts/test-daemon.ps1 --test mcp_sandbox` in WSL and expect FAIL.
- [ ] **Step 3:** Implement. Existing `Tools` behaviour and `mcp.sock` stay identical. `stop_workspace` still stops every listener of that workspace.
- [ ] **Step 4:** Run the same command and expect PASS. Also run `scripts/test-daemon.ps1 --test registry` to check that the existing git-tools listener still behaves.
- [ ] **Step 5:** Commit `feat(daemon): per-sandbox MCP server sockets and mcp-exec`.

### Task 5: Backend wiring

**Files:**
- Modify: `crates/daemon/src/mcp/mod.rs` (`claude_mcp_config`, `codex_config` and a new `opencode_mcp_servers` take `&[ResolvedServer]`), `crates/daemon/src/agents/claude.rs:374`, `agents/codex_backend.rs:335,349`, `agents/opencode.rs:403`, `workspace/agent_sandboxes.rs:315`, `workspace/lifecycle.rs:282` (start one `Server` listener per resolved remote server and write the env files)
- Test: the unit tests in `crates/daemon/src/mcp/mod.rs`, plus append to `crates/daemon/tests/mcp_sandbox.rs`

**Interfaces:**
- Consumes: `McpServers::resolve` (Task 2), `McpRegistry::start(.., ListenerKind::Server(id))` (Task 4).
- Produces: `pub struct ResolvedServer { pub id: McpServerId, pub command: String, pub args: Vec<String> }`. For a remote server this is `DAEMON_IN_SANDBOX` with `mcp-bridge --socket /run/bs/mcp-<id>.sock`. For a local server it is `DAEMON_IN_SANDBOX` with `mcp-exec --env-file /run/bs/mcp-env/<id>.env -- <command> <args…>`. `pub fn prepare_servers(d: &Daemon, ws: &WorkspaceId, backend: AgentAdapterKind, run_dir: &Path) -> Vec<ResolvedServer>` starts the listeners and writes the env files. It never fails the agent start: a failure for one server is logged and that server skipped.
- With an empty `&[ResolvedServer]`, each backend's output is byte-identical to today's (the existing unit test strings stay unchanged).

- [ ] **Step 1:** Write the tests:
  - `claude_config_lists_user_servers`: a remote `jira` gives `mcpServers.jira.args == ["mcp-bridge","--socket","/run/bs/mcp-jira.sock"]`, and `claude_allowed_tools()` is unchanged.
  - `codex_config_lists_user_servers`.
  - `opencode_servers_list_user_servers`.
  - `empty_servers_unchanged`.
  - `native_name_collision_skips_product_entry`: a worktree `.mcp.json` with `mcpServers.jira` means Claude gets no product `jira`.
  - `unauthenticated_preset_does_not_block_start`: append to `mcp_sandbox.rs` an agent start with the default registry against a fake CLI.
- [ ] **Step 2:** Run `cargo test -p bondsymphonic-daemon --lib mcp` and `scripts/test-daemon.ps1 --test mcp_sandbox`, and expect FAIL.
- [ ] **Step 3:** Implement. For collision detection, port `check_native_collision` from the archive's `broker.rs`, reading only the worktree's `.mcp.json` for Claude and `~/.codex/config.toml` keys for Codex.
- [ ] **Step 4:** Run the same commands plus `--test codex_integration --test opencode_integration --test agent_integration`, and expect PASS.
- [ ] **Step 5:** Commit `feat(daemon): give agents the user's MCP servers`.

### Task 6: IDE settings page, workspace dialog and OAuth callback

**Files:**
- Create: `crates/ide/cpp/McpPage.{h,cpp}` (port and adapt from the archive), `crates/ide/cpp/WorkspaceMcpDialog.{h,cpp}`, `crates/ide/src/model/mcp.rs` (a pure model: the snapshot plus statuses, reply-staleness by revision)
- Modify: `crates/ide/src/client/mod.rs` (`mcp.*` calls), `crates/ide/src/qobjects/app_controller.rs` (invokables `mcpList`, `mcpUpsert`, `mcpRemove`, `mcpImportPreview`, `mcpImportApply`, `mcpExport`, `mcpRestorePreset`, `mcpSetOverride`, `mcpConnect`, `mcpDisconnect`, `mcpTest`, and signal `mcpChanged`), `crates/ide/cpp/SettingsDialog.cpp` (tab "MCP servers"), `crates/ide/cpp/MainWindow.cpp:401` (Workspace › "MCP servers…"), `crates/ide/build.rs`, `docs/user-guide.md` (an "MCP servers" section)
- Test: `crates/ide/tests/mcp_model_tests.rs` (pure), `crates/ide/tests/mcp_widgets.rs` (offscreen: never `show()` a top-level window; resize and activate instead)

**Interfaces:**
- Consumes: the Task 1 RPCs and `Event::McpChanged`.
- OAuth: `mcpConnect(id)` starts a `QTcpServer` on `127.0.0.1:0`, sends `mcp.connect` with `redirect_uri = http://127.0.0.1:<port>/callback`, opens `authorization_url` with `QDesktopServices::openUrl`, answers the first `GET /callback` with a small "You can close this tab" page, and sends `mcp.callback { server_id, state, code, issuer }`. It closes after the callback or after 10 minutes.

- [ ] **Step 1:** Write the tests:
  - `mcp_status_ignores_stale_reply`: a reply with revision < the current one is dropped.
  - `mcp_secret_editor_is_write_only`: the editor never shows existing secret values, and blank means `Keep`.
  - `mcp_import_preview_before_apply`: Apply is disabled until collisions are resolved.
  - `callback_bound_to_active_attempt`: a second callback, or one with an unknown state, is ignored.
  - `workspace_dialog_shows_inherit`.
  - `settings_has_mcp_tab`.
  - The widget tests use `bs_widget_test_*` hooks.
- [ ] **Step 2:** Run `cargo test -p bondsymphonic-ide --features require-qt --test mcp_model_tests --test mcp_widgets` and expect FAIL.
- [ ] **Step 3:** Implement. The page lists the servers with a status dot, has Add / Edit / Remove / Connect / Disconnect / Test / Import / Export buttons, and offers "Restore Jira preset" when Jira is missing. It also says that new or enabled servers apply when an agent next starts.
- [ ] **Step 4:** Run the same command plus the full `cargo test -p bondsymphonic-ide --features require-qt`, and expect PASS.
- [ ] **Step 5:** Commit `feat(ide): MCP servers settings, workspace overrides and OAuth login`.

### Task 7: Integrated verification

- [ ] `cargo fmt --all -- --check`, plus `cargo clippy --workspace --all-targets -- -D warnings` on Windows with `--features bondsymphonic-ide/require-qt` and in WSL for the daemon.
- [ ] Run the full `scripts/test-daemon.ps1` and the full Windows IDE suite. Record pass counts and any known flakes.
- [ ] End to end in a throwaway workspace: add a local stdio server (`npx -y @modelcontextprotocol/server-everything` if the network allows it, otherwise a fixture script) and confirm that a Claude agent lists and calls its `echo` tool. Start an agent while Jira is disconnected and confirm the start succeeds.
- [ ] Live Jira OAuth and a read-only search are the user's to run, because they need the user's Atlassian account. Never claim them without that.
- [ ] Final whole-branch review. Fix the findings, then merge to `main` and push.
