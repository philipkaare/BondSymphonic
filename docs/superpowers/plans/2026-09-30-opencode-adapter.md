# OpenCode backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Implement sandboxed OpenCode agents against user-configured OpenAI-compatible model servers and the shared general MCP registry.
**Architecture:** One pinned OpenCode ACP stdio process per agent uses the existing backend/adapter lifecycle. Provider routing and credentials belong to its isolated companion sandbox and proxy. General MCP declarations are consumed from the preceding MCP plan exactly once.
**Tech Stack:** Rust/Tokio, ACP JSON-RPC, existing transcript, bwrap, per-backend network proxy, Qt settings.
**Spec:** ../specs/2026-09-30-opencode-adapter-design.md

## Global Constraints

- OpenCode starts disabled for existing users and never falls back automatically to a cloud model.
- Multiple named providers support Ollama, vLLM, SGLang and custom compatible endpoints.
- API keys remain Windows-owned and appear only through per-process secret environment references.
- Sandbox localhost is not assumed to be WSL or Windows localhost. Grants are exact host/port and OpenCode-only.
- ACP peer filesystem and terminal operations stay within sandbox/workspace protections.
- Session resume must survive process and daemon restarts; overlapping permissions follow the existing approval queue.
- General MCP registry serves custom servers and Jira identically; preserve native repository configuration.
- HTTP serve is a fallback only after recorded ACP incompatibility and design revision.

## Review Focus

1. Endpoint URL contains a path prefix or IPv6 literal (Task 2).
2. Provider edit races an older model discovery response (Tasks 2/5).
3. ACP sends overlapping approvals or exit during cancellation (Task 4).
4. Saved session uses a removed provider or model (Task 4).
5. Malicious ACP filesystem/terminal request escapes workspace (Task 4).

## File Map

- `docs/superpowers/plans/notes/2026-09-30-opencode-compatibility.md`: verified release/protocol fixtures.
- `crates/proto/src/opencode.rs`, types.rs/request.rs: provider and backend contracts.
- `crates/daemon/src/agents/{opencode_backend,opencode_adapter,acp}.rs`: preparation, adapter and protocol parsing.
- `crates/daemon/src/opencode/{mod,providers}.rs`: provider persistence/discovery.
- `crates/daemon/src/net/proxy.rs`, workspace/agent_sandboxes.rs, agents/backend.rs, prereqs.rs/setup.rs: exact endpoint grants, companion sandbox and registry/setup.
- IDE existing settings/controller/transcript and SettingsDialog: providers, installation, chooser and session interaction.
- Tests: `opencode_providers.rs`, `opencode_backend.rs`, `opencode_adapter.rs`, protocol roundtrip and existing IDE tests.

### Task 1: ACP compatibility probe

**Interfaces:** Recorded fixtures define exact ACP methods/fields and CLI release used by Task 4. No shared product file mutations; safe parallel work with MCP contracts.

- [ ] Inspect official OpenCode ACP/provider/MCP documentation and locally installed binaries/cache. Obtain explicit release version where network permits; capture init, new/load, prompt/update, permission, cancel, model selection, supplemental MCP and sandbox peer requests.
- [ ] Add sanitized replay fixtures under daemon tests/fixtures/opencode and a notes matrix of supported/unsupported capabilities with actual command evidence. Never label documentation-only observations as live probes.
- [ ] Confirm ACP supports required resume/tools/permissions/MCP. If a binary cannot be obtained, implement the documented protocol behind explicit version/capability checks and retain unverified live tests as a completion limitation. If observed ACP cannot implement requirements, record evidence and revise transport design before implementation.

### Task 2: Providers and restricted routing

**Interfaces:** `OpenCodeProvider { id: String, name: String, base_url: String, endpoint_target: EndpointTarget, models: Vec<OpenCodeModel> }`; `EndpointTarget::{Wsl,Windows,Remote}`; `OpenCodeModel { id, name, context_limit: Option<u64>, output_limit: Option<u64> }`. `ProviderStore::upsert(expected_revision: u64, provider: OpenCodeProvider) -> Result<ProviderSnapshot,RpcError>`; `ProviderStore::resolve(id: &str) -> Result<OpenCodeProvider,RpcError>`. Write-only key updates remain IDE-owned. Extend model-query context with optional provider ID across Backend implementations without changing existing default results.

- [ ] Write failing `prefix_and_ipv6_preserved`, `endpoint_grant_is_backend_and_port_scoped`, `model_discovery_generation_guard`, `manual_models_without_metadata`, `provider_export_has_no_key`; run opencode_providers target to observe FAIL.
- [ ] Implement provider RPC/storage and explicit namespace routing using the existing proxy. Reject unsafe endpoint credentials/fragments, preserve base path, resolve Windows target from reachable configured routing, expire grants on edit/restart; provide actionable failures without broad private-network permission.
- [ ] Run focused tests to PASS and proxy regressions; commit where permitted.

### Task 3: Backend and companion

**Interfaces:** `OpenCodeBackend` implements existing `Backend`; `AgentAdapterKind::OpenCode` serializes as `opencode`. Start options include optional provider ID; selected model/provider persists in session data. Preparation emits `opencode acp` argv and supplemental config from verified release, per-process key references and MCP Task 4 `McpLaunchConfig`.

- [ ] Write `opencode_disabled_by_default`, `opencode_has_own_home_and_proxy`, `provider_key_debug_redacted`, `missing_provider_is_actionable`, `mcp_attached_once`; run opencode_backend target to observe FAIL.
- [ ] Register backend/prerequisites/setup, add exhaustive enum handling and private companion runtime. Generate provider model configuration and MCP declarations while retaining native entries. Do not grant proxy access to other backends.
- [ ] Run focused target and Claude/Codex backend regressions to PASS; commit where permitted.

### Task 4: ACP agent lifecycle

**Interfaces:** `OpenCodeAdapter` implements `AgentAdapter`; `AcpClient` owns request IDs, pending responses, framing, cancellation and bounded stderr. Consume exact fixtures from Task 1; map transcript events using existing AgentSink conventions.

- [ ] Write failing `acp_prompt_tool_and_result`, `overlapping_permissions_preserved`, `cancel_then_exit_completes_once`, `resume_after_daemon_restart`, `removed_provider_blocks_resume_safely`, `peer_path_escape_rejected`; assert sink events and correct native approval replies rather than implementation mirrors.
- [ ] Run opencode_adapter target to observe FAIL.
- [ ] Implement initialization/capability negotiation, session new/load, prompt queue, streaming/tool events, permission replies, cancel/stop and bounded process errors. Store backend-native session IDs; reject unsupported version/capability clearly. Execute supported peer operations through sandbox handles and workspace-safe paths only.
- [ ] Run fake-process fixtures to PASS; run real ACP tool/edit/resume checks when binary/provider available; commit where permitted.

### Task 5: Provider settings and chooser

**Interfaces:** Existing backend descriptors expose OpenCode enabled/install/credential status; provider UI consumes Task 2 revisioned snapshots and provider ID model queries.

- [ ] Add failing IDE tests `opencode_enable_install_choose`, `provider_key_replacement_isolated`, `stale_provider_models_ignored`, `missing_resume_provider_explained`; run focused require-qt tests.
- [ ] Implement multiple provider CRUD, endpoint target selector, presets/manual model limits, per-provider Windows credentials, default selection and OpenCode setup guidance. Use existing transcript approvals and general MCP page.
- [ ] Run focused tests and Qt smoke to PASS; commit where permitted.

### Task 6: Integration and review

- [ ] Run fmt, workspace Clippy warnings denied, Windows require-qt tests and WSL daemon tests.
- [ ] Verify local provider agent tool/edit and resume through process/daemon restart, alongside generic MCP stdio/remote use. Report unavailable live prerequisites honestly.
- [ ] Independently review task diffs and whole change, fix material findings and document checks. Do not merge or push without authorization specific to this work.

## Execution decisions

User authorized immediate subagent implementation and safe parallelism. ACP probe can run beside MCP contracts; provider work can run beside MCP transport only with disjoint file ownership. Serialize proto, backend registry and IDE changes. Self-review: design behavior mapped to tasks, interfaces consistent, five failure classes assigned tests; ACP wire fields deliberately determined by the probe, not guessed in planning.
