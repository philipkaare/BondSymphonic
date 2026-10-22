# General sandbox MCP Implementation Plan

> **Status, 2026-10-02:** Tasks 1–5 are implemented and reviewed in `feat/codex-adapter`. Task 6 automated results and remaining live acceptance gates are in [verification notes](notes/2026-10-02-mcp-opencode-verification.md). Original checklists below are planning detail; use the notes and SDD ledger for current status.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Let users configure arbitrary MCP servers for sandbox agents, with Jira Cloud as a removable preset shared across workspaces.
**Architecture:** A daemon registry owns configuration and isolated credentials. Sandbox stdio bridges reach a session-isolated remote broker; local servers execute within the sandbox. All backend adapters consume the same resolved registry.
**Tech Stack:** Rust, Tokio, serde, existing sandbox/RPC abstractions, HTTPS client with rustls, Qt Widgets.
**Spec:** ../specs/2026-09-30-sandbox-mcp-design.md

## Global Constraints

- Jira is a preset of general MCP functionality, never a separate integration path.
- Jira Cloud defaults to all workspaces; authentication remains explicit and missing login never blocks agent startup.
- Preserve repository MCP configuration. Product entries have stable IDs and a separate namespace.
- Remote credentials stay outside sandboxes, logs, transcripts, and configuration exports. Stdio secret environment variables go only to the selected process.
- OAuth callback arrives at the Windows IDE; daemon owns verifier, state, exchange, refresh, and token storage.
- Legacy SSE is rejected explicitly. Initial remote transport is Streamable HTTP over HTTPS.
- Disable/disconnect revokes broker sessions immediately. Backend tool-list refresh may require restart.
- Existing Claude/Codex behavior and additive protocol compatibility remain intact.

## Review Focus

1. Deleted preset must not reappear after restart (Task 2).
2. Import duplicate IDs/names and secret fields must have explicit preview decisions (Tasks 2/5).
3. Concurrent OAuth refresh and stale callbacks must not replace newer credentials (Task 3).
4. Two workspace/backend sessions must not share MCP state or socket authorization (Task 4).
5. Native repository entries must survive product generation and removal (Task 4).

## File Map

- `crates/proto/src/mcp.rs`: public serializable configuration, status, import and OAuth RPC contracts; export via lib.rs/request.rs.
- `crates/daemon/src/mcp/{mod,config,secrets,auth,transport,broker,bridge}.rs`: registry persistence, private secrets, OAuth, HTTP sessions and sandbox bridging.
- `crates/daemon/src/{daemon,main}.rs`, `server/handlers.rs`: manager ownership, helper CLI and RPC dispatch.
- `crates/daemon/src/agents/{claude_backend,codex_backend}.rs`, `workspace/agent_sandboxes.rs`: resolved supplemental configuration and authorized sockets.
- `crates/ide/src/qobjects/{app_controller,settings}.rs`, existing SettingsDialog C++ and build.rs: generic registry UI, browser callbacks and secret editing.
- New tests: daemon `mcp_config.rs`, `mcp_auth.rs`, `mcp_transport.rs`, `mcp_sandbox.rs`; proto roundtrip; IDE model/UI tests.

### Task 1: Verified protocol contracts

**Interfaces:** Produce `McpServerId(String)`, `SecretRef(String)`, `McpServerConfig { id, name, transport, enabled_backends }`, tagged `McpTransportConfig::{Stdio,Http}`, `McpAuthConfig::{None,Bearer,Headers,OAuth}`, `McpWorkspaceOverride::{Inherit,Enabled,Disabled}`, `McpRegistrySnapshot { revision, servers, overrides }`. Secret-bearing write parameters have redacted Debug; reads return references/status only. Add MCP list/upsert/remove/import-preview/import-apply/export/override/connect/callback/disconnect/test requests using existing request conventions.

- [ ] Read both current official MCP transport/authorization documents and native Claude/Codex MCP documentation; record tested versions, payload fixtures and limits in `docs/superpowers/plans/notes/2026-09-30-mcp-compatibility.md`. Pin compatible maintained HTTPS/URL/PKCE dependencies; do not handwrite TLS.
- [ ] Add failing roundtrip tests `mcp_additive_defaults`, `mcp_secret_debug_redacted`, `mcp_transport_roundtrip`; assert absent overrides inherit, unknown transport errors, and a sentinel secret never occurs in Debug/export.
- [ ] Run `cargo test -p bondsymphonic-proto`; observe new-test failure before implementation.
- [ ] Implement types/RPC variants, capabilities and redacted writes; run same command to PASS.
- [ ] Commit `feat: define general MCP configuration contracts` if git metadata is writable.

### Task 2: Registry, import and secret persistence

**Interfaces:** `McpManager::load(root: PathBuf) -> Result<Self, RpcError>`; `snapshot(&self) -> McpRegistrySnapshot`; `upsert(&self, expected_revision: u64, config: McpServerConfig, secrets: Vec<McpSecretUpdate>) -> Result<McpRegistrySnapshot,RpcError>`; `resolve(&self, workspace: WorkspaceId, backend: AgentAdapterKind) -> Vec<McpServerConfig>`. Add analogous revision-checked remove/override/import operations. `McpSecretUpdate` belongs to Task 1; secret storage never enters snapshot.

- [ ] Write `preset_removed_stays_removed`, `revision_conflict_preserves_config`, `import_requires_collision_decision`, `export_omits_secret_values`, `workspace_override_inherits` in mcp_config.rs, asserting one-time Jira seeding at `https://mcp.atlassian.com/v2/mcp`, atomic persistence and no raw secrets.
- [ ] Run `cargo test -p bondsymphonic-daemon --test mcp_config`, expect FAIL.
- [ ] Implement atomic registry persistence, private secret file permissions, strict validation, import preview/apply, backend/workspace resolution, explicit restore-preset and redacted errors.
- [ ] Run the focused target to PASS; commit `feat: persist general MCP server registry` where permitted.

### Task 3: Generic HTTP and OAuth

**Interfaces:** `McpManager::connect(server: McpServerId) -> Result<McpConnectResult,RpcError>` returns browser URL and opaque attempt ID; `complete_oauth(attempt_id: String, callback_url: String) -> Result<(),RpcError>`; `disconnect(server: McpServerId) -> Result<(),RpcError>`. `RemoteMcpSession` owns negotiated headers/session ID and cancellation; broker consumes it in Task 4.

- [ ] Add fake-server tests `oauth_wrong_state_rejected`, `oauth_wrong_issuer_rejected`, `refresh_single_flight`, `disconnect_invalidates_pending_exchange`, `http_session_headers_isolated`, `redirect_cannot_escape_origin`, `step_up_scope_challenge`. Assert PKCE/resource parameters, metadata discovery, exact issuer validation and bounded errors.
- [ ] Run mcp_auth and mcp_transport targets to observe FAIL.
- [ ] Implement maintained-client HTTPS, origin/private-address validation, MCP protocol negotiation/Streamable HTTP, generic no-auth/header/bearer modes, OAuth discovery and configured client ID/client metadata with supported registration fallback. Keep token generation checks across refresh/disconnect; expose no token in public errors.
- [ ] Run both targets to PASS; commit `feat: add generic remote MCP transport and OAuth` where permitted.

### Task 4: Sandbox bridge and existing backends

**Interfaces:** `McpManager::prepare(workspace: WorkspaceId, backend: AgentAdapterKind, handle: Arc<dyn SandboxHandle>) -> Result<McpLaunchConfig,RpcError>` returns supplemental server declarations and binds/env without remote secrets. CLI `mcp-bridge` connects only to its assigned workspace/backend/generation socket. Local server launches use `SandboxHandle::spawn` and per-process environment.

- [ ] Write `cross_workspace_bridge_rejected`, `disable_closes_live_session`, `stdio_uses_sandbox_environment`, `native_config_preserved`, `unauthenticated_preset_does_not_block_start`; assert distinct sessions and no host command execution.
- [ ] Run mcp_sandbox target to observe FAIL.
- [ ] Implement bounded stdio framing, per-session forwarding/cancellation, runtime socket authorization/revocation, sandbox helper bind and backend-native supplemental config using Task 1 probe findings. Preserve native entries and reject collisions explicitly.
- [ ] Run WSL real bwrap/socket tests via `scripts/test-daemon.ps1 --test mcp_sandbox`; run focused backend regression tests; commit where permitted.

### Task 5: Generic Settings and Windows callback

**Interfaces:** IDE invokes Task 1 RPCs and renders snapshots/status; callback listener passes attempt ID and exact callback URL to Task 3. Secret editor only submits changed values, never reads existing values.

- [ ] Add failing tests `mcp_import_preview_before_apply`, `mcp_secret_editor_is_write_only`, `mcp_status_ignores_stale_reply`, `callback_bound_to_active_attempt` in appropriate existing Qt/pure model test conventions.
- [ ] Run focused IDE tests with `--features require-qt`; observe FAIL.
- [ ] Add MCP page: add/edit/remove, stdio/HTTP/auth forms, import/export preview, connect/disconnect/test, backend selection and workspace overrides. Add Jira preset button with ordinary editor semantics. Launch Windows browser and own localhost callback lifecycle; surface actionable safe errors.
- [ ] Run focused tests to PASS and packaged Qt smoke; commit where permitted.

### Task 6: Integrated verification and review

- [ ] Run fmt check, workspace Clippy with warnings denied, Windows workspace require-qt tests and WSL daemon tests.
- [ ] Verify a generic stdio and generic fake remote OAuth server through both existing backends; verify startup with disconnected Jira. Live Jira read-only check requires existing account access; never invent successful live evidence or mutate user issues.
- [ ] Request independent task/final review, fix material findings, record evidence and environmental limitations in notes. No merge/push implied by this plan.

## Execution decisions

User explicitly requested automatic implementation with subagents and safe parallelism. Task 1 contracts and OpenCode compatibility probe may run in parallel; shared proto/backend/UI mutations must be serialized. Review each implementation task before dependent work. Self-review: all design sections mapped, shared signatures matched, five review inputs assigned above; no implementation-dependent wire assumptions before probes.
