# Codex and multiple agent backends Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run Codex beside Claude with resumable transcripts, approvals, and equal per-backend controls in Settings.

**Architecture:** A daemon agent-backend registry owns CLI-specific preparation and constructs adapters behind the existing process interface. Backend descriptors drive the IDE's Settings tabs, model and permission choices, and setup actions. Workspace files are shared, with separate sandboxes and proxies per backend.

**Tech Stack:** Rust, Tokio, serde JSON, bubblewrap in WSL2, Qt Widgets and cxx-qt, Codex app-server over stdio.

**Spec:** `docs/superpowers/specs/2026-09-28-codex-adapter-design.md`.

**Status:** Ready for plan review. Repository inspected at `6e1b2bd`; the user confirmed separate sandboxes per backend during planning. No implementation or live Codex validation has been performed. Spec section 7 checks are execution gates, not assumed successes.

## Global constraints

- Codex comes first; OpenCode is sub-project 2. Do not advertise an unimplemented OpenCode adapter.
- No `PROTOCOL_VERSION` bump for the additive backend fields and variant. Missing `system.list_models.adapter` means Claude.
- Claude behavior, including credential precedence, long-lived tokens, settings migration, and transcript restoration, stays covered by its existing tests.
- Credentials go to one process, never the sandbox environment, command arguments, persisted agent options, or logs.
- Windows credential entries remain `anthropic_api_key` and `openai_api_key`.
- Codex uses shared `~/.bondsymphonic/codex-home/`; its directory bind is read-write and exclusive to Codex sandboxes. Concurrent Codex agents can read one another's Codex history, as accepted in the spec.
- Codex modes are `never` (label `YOLO`, default) and `on-request` (label `Ask when Codex wants to`). State that asking before every command is unavailable.
- Backend settings are `backends.<id>.{enabled,default_model,default_permission_mode}` and `default_backend`. Claude starts enabled; Codex starts disabled.
- Only available binaries appear in `Capabilities.adapters`; known but uninstalled backends must still appear in Settings so users can install them.
- Windows tests, real WSL sandbox tests, and `cargo fmt --all -- --check` are required before completion.

## Confirmed decision: sandbox ownership

The spec requires OpenAI network defaults and the shared Codex login directory to be accessible only from Codex sandboxes. The current implementation has one `SandboxHandle` and one proxy per `WorkspaceId` (`daemon.rs`, `net/proxy.rs`), created before any agent starts. Adding Codex mounts to that sandbox would expose them to Claude, terminals, and runs. Adding a host to its proxy would allow every process in it to use that host.

**User-confirmed approach:** retain the existing workspace sandbox for Claude, terminals, files, and runs. Lazily create a companion sandbox per `(WorkspaceId, Codex)` with its own home, run directory, proxy listener, and generation. It binds the same worktree and git protection mounts, plus the shared Codex home and Codex binary. Multiple Codex agents in that workspace reuse it. Generalize this companion registry for OpenCode later without implementing OpenCode now. Claude's sandbox and credential seeding remain unchanged.

Task 4 implements this decision, now recorded in the spec. Adapter interfaces consume a selected `SandboxHandle` so process protocol handling remains independent of sandbox ownership.

## Review focus

1. A user disables the default backend or disables every backend: New Agent must not silently start a disabled backend (Task 6).
2. A delayed model/prerequisite reply arrives after changing backend, credentials, or connection: it must not overwrite the current backend's choices or login state (Tasks 5–7).
3. The Codex process dies during initialization or with an approval outstanding: pending calls must terminate and the agent must reach one final state (Task 3).
4. A workspace is destroyed while its companion sandbox starts, or an old sandbox watcher exits after replacement: no leaked process and no teardown of the replacement (Task 4).
5. An existing agent is restarted with an explicit model/mode: retain those choices and its backend instead of applying another backend's defaults (Tasks 6–7).

## File map

- `crates/proto/src/{types,request}.rs`: additive descriptors, setup actions, and adapter selection.
- `crates/daemon/src/agents/adapter.rs`: existing `AgentAdapter` trait, moved without changing its methods.
- `crates/daemon/src/agents/backend.rs`: backend registry and preparation contract.
- `crates/daemon/src/agents/claude_backend.rs`: wrappers around existing Claude helpers.
- `crates/daemon/src/agents/{codex,codex_rpc,codex_stream,codex_backend}.rs`: process, RPC transport, transcript mapping, and preparation respectively.
- `crates/daemon/src/workspace/agent_sandboxes.rs`: companion sandbox ownership.
- Existing daemon lifecycle, proxy, setup, prerequisites, and request handlers: connect backend dispatch to existing flows.
- `crates/ide/src/model/backends.rs`: backend defaults, selection, and reply-generation rules that need no Qt.
- `crates/ide/src/qobjects/{settings,app_controller}.rs`: persistence, credential storage, RPC routing, and Qt-facing state.
- Existing `SettingsDialog`, `SetupPage`, `AgentChoices`, `NewAgentDialog`, and `AgentArea` C++ files: render backend descriptors and route the selected backend.
- New fixture directory `crates/daemon/tests/fixtures/codex-app-server/` and `fake_codex.py`: sanitized live messages and deterministic process tests.

## Task 1: Record the blocking Codex compatibility check

**Files:** Create `docs/superpowers/plans/notes/2026-09-28-codex-preflight.md`, the fixture directory above, and `scripts/probe-codex.py`.

**Interfaces:** Produces a recorded tested version, reproducible invocation, sanitized bidirectional NDJSON fixtures, provider configuration, and observed required hosts. These artifacts define the wire format for Tasks 3 and 5; this plan deliberately does not invent protocol payloads or a version.

- [ ] Inspect the installed Codex binary/help and local generated app-server schema if present. Consult official OpenAI documentation/source for remaining questions, following the openai-docs skill. Select and record an exact tested release and installation source.
- [ ] Write a bounded probe that starts app-server, records request IDs and message order, times out stalled requests, and redacts credentials. Run against a fake process first; assert `assert not any(secret in line for line in captured_lines)` using a sentinel secret and assert a non-answer exits within the configured deadline.
- [ ] In the distro, capture initialize/initialized, thread start/resume, a command, a file edit, both approval request types, steering, interrupt, completion, and model listing. Do not capture real authentication tokens in committed fixtures.
- [ ] Verify API-key authentication via provider `env_key` without `auth.json`; verify the auth-file replacement behavior; capture login/turn network hosts through the proxy; run a command and patch inside the project's bubblewrap boundary with Codex `danger-full-access`.
- [ ] Record pass/fail and sanitized evidence for each spec section 7 condition. If credentials are unavailable, record the missing check and request the required login; fake fixtures do not count as live verification. If `env_key` fails, document and test the spec's per-key home fallback before changing Task 5. If the sandbox check fails, return to design.
- [ ] Pin the observed version and fixture assertions in the following tasks, then review the preflight artifact. Commit only the probe, sanitized fixtures, and notes.

## Task 2: Add backend discovery and move Claude behind it

**Files:** Modify proto types/request/roundtrip tests; daemon `agents/{mod,claude}.rs`, `server/{handlers,dispatch}.rs`, and `daemon.rs`. Create `agents/{adapter,backend,claude_backend}.rs` and `tests/backend_integration.rs`.

**Interfaces:** Add `AgentAdapterKind::Codex`; `ListModelsParams.adapter: Option<AgentAdapterKind>`; `BackendDescriptor { id: AgentAdapterKind, label: String, permission_modes: Vec<BackendChoice>, default_permission_mode: String, permission_note: String, credential_label: String, prerequisite_names: Vec<String>, setup_actions: Vec<SetupAction> }`; `BackendChoice { id: String, label: String }`. Add `Capabilities.backends: Vec<BackendDescriptor>` with a serde default. Descriptors describe implemented backends regardless of installation; `adapters` remains the runnable subset.

The daemon `Backend: Send + Sync` uses `async_trait` and exposes `descriptor() -> BackendDescriptor`, `binary() -> Option<PathBuf>`, `ro_binds() -> Vec<(PathBuf, PathBuf)>`, `extra_hosts() -> Vec<String>`, `async prepare(&self, d: &Daemon, workspace: &Workspace, options: &AgentStartOptions) -> Result<PreparedAgent, RpcError>`, `adapter(&self, sink: AgentSink, handle: Arc<dyn SandboxHandle>, prepared: PreparedAgent) -> Box<dyn AgentAdapter>`, and `async list_models(&self, d: &Daemon, api_key: Option<&str>) -> Result<ListModelsResult, RpcError>`. `PreparedAgent` holds `argv`, process-only `env`, `cwd`, and a cloned `AgentStartOptions`; give it a redacted Debug implementation. `backend_for(kind: AgentAdapterKind) -> Result<Arc<dyn Backend>, RpcError>` refuses Terminal with the existing `pty.open` guidance.

- [ ] Add failing compatibility tests: `assert_eq!(decode_list_models("{}").adapter, None)`; old hello replies decode with empty descriptors; Codex serializes as `"codex"`; absent binaries are omitted from runnable adapters; Terminal remains refused. Use the existing roundtrip harness for decode helpers.
- [ ] Run `cargo test -p bondsymphonic-proto --test roundtrip` and `cargo test -p bondsymphonic-daemon --test backend_integration`; confirm the new assertions fail before implementation.
- [ ] Move `AgentAdapter` unchanged, retaining a public re-export in `claude.rs` for existing callers. Wrap Claude's probe, argv, credential seeding/write-back, repository settings, auth environment, and model fetching. Keep the per-workspace start gate and record-before-spawn ordering in `AgentManager`.
- [ ] Dispatch start and model listing through the registry. Do not mark Codex runnable until Task 5 supplies its implementation. Ensure Claude-specific credential write-back in `AgentSink` is not invoked for Codex.
- [ ] Run the new tests plus `cargo test -p bondsymphonic-daemon --test agent_integration --test agent_persistence`. Existing Claude assertions must remain unchanged and pass. Commit the backend extraction and additive protocol.

## Task 3: Implement Codex RPC, transcript mapping, and process lifecycle

**Files:** Create `agents/{codex,codex_rpc,codex_stream}.rs`, `tests/codex_integration.rs`, and `tests/fixtures/fake_codex.py`; register modules in `agents/mod.rs`. Read `claude_stream.rs`, `AgentSink`, and the existing transcript model before mapping events.

**Interfaces:** `CodexAdapter::new(sink: AgentSink, handle: Arc<dyn SandboxHandle>, prepared: PreparedAgent) -> Self` implements the unchanged `AgentAdapter`. `CodexRpc` correlates client requests independently from server approval requests. `CodexStream::ingest(&mut self, message: serde_json::Value) -> Vec<AgentMessageBody>` maps fixture messages and tracks tool IDs/output; RPC state separately tracks thread/turn IDs and pending approvals.

- [ ] Write fixture tests for every mapping in spec section 3.2. Assert command output joins on its item ID, edits retain paths/diff, MCP failures set `is_error`, unknown notifications produce `System { subtype: "raw", .. }`, and reasoning/plan deltas produce no transcript items. Assert streamed text plus completed text renders once through the existing transcript reducer.
- [ ] Add fake-process tests for initialization ordering, start versus steer, resume with a saved session, approve/decline/always, interrupt, and graceful stop followed by bounded kill. Add assertions that an unknown or already-answered approval ID is refused and that RPC errors, EOF, and initialization timeout fail all pending calls without duplicate exit events.
- [ ] Run `cargo test -p bondsymphonic-daemon codex` and the new integration target; confirm failures expose missing behavior.
- [ ] Implement the transport and mapper against Task 1's captured schema. Preserve the JSON type of server request IDs. Serialize outgoing frames; keep the read pump independent of a call awaiting its response. Record the thread ID through `AgentSink` before publishing resumability. Redact secrets in error and raw-event handling.
- [ ] Keep `AgentMessageBody::Result` wire-compatible: it currently contains cost/duration/turn count/session, not tokens or status. Put Codex usage and status into `System { subtype: "codex_turn", data: ... }`, emit the existing Result for completion, and show no fabricated dollar cost when Codex supplies none. Map unauthorized failure to backend-specific auth failure detail. Test completed/interrupted/failed distinctly.
- [ ] Run the focused suites and `cargo test -p bondsymphonic-ide --test transcript_tests`; all existing Claude transcript tests and Codex replay tests pass. Commit transport, adapter, and fixtures.

## Task 4: Preserve backend isolation in workspace lifecycle

**Files:** Create `workspace/agent_sandboxes.rs`; modify `workspace/{mod,lifecycle}.rs`, `daemon.rs`, `net/proxy.rs`, `agents/mod.rs`; add `tests/backend_sandbox_integration.rs`.

**Interfaces:** `AgentSandboxKey { workspace_id: WorkspaceId, adapter: AgentAdapterKind }` derives Eq/Hash; add those derives to the enum. `AgentSandboxes::ensure(&self, d: &Daemon, workspace: &Workspace, backend: &dyn Backend) -> Result<Arc<dyn SandboxHandle>, RpcError>` is async and delegates Claude to the existing workspace handle. `stop_workspace(&self, workspace_id: &WorkspaceId)` is async and closes all companions. Companion proxy registries are keyed by adapter, then workspace, so existing generation checks and workspace event IDs remain intact.

- [x] Resolve and record the sandbox decision: user selected separate sandboxes per backend; spec updated.
- [ ] Add sandbox tests proving `assert!(!claude_mounts.contains(&codex_home))`, that the Codex mount is a directory and writable, and that Codex's extra hosts do not enter Claude's default allowlist. Verify explicit user allowlist additions still work. Verify both sandboxes see the same worktree and retain read-only git protection.
- [ ] Add lifecycle tests for concurrent first starts, failed startup cleanup, restart, destroy-during-start, shutdown, and a stale watcher firing after replacement. Assert one companion per key, no duplicate proxy listener, no orphan process, and replacement generation survival.
- [ ] Run `cargo test -p bondsymphonic-daemon --test backend_sandbox_integration` and confirm the missing isolation/lifecycle behavior fails.
- [ ] Build companion specs from the existing worktree/in-place mount builders without Claude credential seeding. Use separate run/home paths under the data directory; preserve the original workspace ID in events. Create companions lazily; propagate allowlist edits, shutdown, workspace restart/destruction, and failed startup cleanup to them. A companion failure ends its agents without marking the base sandbox down.
- [ ] Run the focused target inside WSL through `./scripts/test-daemon.ps1 --test backend_sandbox_integration`, plus existing sandbox lifetime, in-place sandbox, and network suites. Assert actual mounts/proxy behavior; a skipped sandbox suite is not proof. Commit the lifecycle implementation.

## Task 5: Add Codex credentials, setup, prerequisites, and model listing

**Files:** Create `agents/codex_backend.rs`; modify proto `SetupAction`, daemon `setup.rs`, `prereqs.rs`, `server/{handlers,dispatch}.rs`, `agents/backend.rs`, and `tests/{setup_pty_integration,backend_integration}.rs`.

**Interfaces:** Implement Task 2's `Backend` for `CodexBackend`. Add `SetupAction::{InstallCodex,CodexLogin,CodexLogout}`. `codex_home(root: &Path) -> PathBuf` resolves the shared home. `BS_CODEX_BIN` supplies the test binary override, analogous to `BS_CLAUDE_BIN`. Codex's `list_models` returns existing `ModelInfo` values using the observed app-server model-list response; absent creation timestamps are empty strings and are not used to filter Codex models.

- [ ] Add tests for fixed setup argv, shared `CODEX_HOME` on login/logout, absent binary, absent or malformed auth file, API-key precedence, and no Claude credential interaction. Assert the sentinel key is absent from argv, serialized sandbox specs, Debug output, agent records, and workspace summaries; assert it appears only in the intended process environment.
- [ ] Add model-list tests using the fake app-server: multiple pages, RPC failure, missing auth, empty results, and an auth change. Keep Codex model discovery uncached initially so responses cannot reuse another account's models. Assert `assert_eq!(claude_models_after_codex_request, claude_models_before)`.
- [ ] Run `cargo test -p bondsymphonic-daemon codex` and focused setup/backend integration tests, observing the missing behavior first.
- [ ] Implement the version probe and literal setup commands validated in Task 1, process provider configuration naming `OPENAI_API_KEY`, and descriptor modes/defaults. Never put the key itself in configuration argv. Set shared `CODEX_HOME` per process and setup terminal. Refuse start without either explicit key or usable sign-in state, with a Codex-specific prerequisite message.
- [ ] Report `codex` and `codex_auth` independently of Claude. Include Codex in runnable capabilities only when its binary exists. Ensure Setup installation is followed by refreshed discovery, so installing after daemon startup works without requiring a daemon restart.
- [ ] Run focused tests and the live API-key/sign-in sandbox checks from Task 1 against the implemented backend. Verify a restarted thread resumes. Commit setup/auth/model integration.

## Task 6: Persist per-backend settings and route IDE requests

**Files:** Create `ide/src/model/backends.rs`; modify `model/mod.rs`, `qobjects/{settings,app_controller}.rs`, `model/{app_state,models,persistence}.rs`, and `tests/{persistence_tests,restore_tests,reconnect_tests}.rs`.

**Interfaces:** `BackendSettings { enabled: bool, default_model: String, default_permission_mode: String }`; `Settings.backends: BTreeMap<String, BackendSettings>` and `default_backend: String`. Pure `select_default_backend(settings: &Settings, descriptors: &[BackendDescriptor]) -> Option<AgentAdapterKind>` returns the enabled configured default, otherwise the first enabled known descriptor, or None. `start_options_with_defaults(options_json: &str, defaults: &BackendSettings) -> Result<AgentStartOptions, String>` applies model/mode defaults only to missing/null fields; explicit values survive. Credential lookup remains separate and uses the selected adapter.

- [ ] Write migration tests for old `default_permission_mode`, the existing `default` to `manual` migration, an already-migrated Claude entry winning over the legacy value, unknown backend entries surviving save, and malformed settings retaining the existing backup/error behavior.
- [ ] Write selection/default tests: fresh Claude enabled/Codex disabled; disabled default falls back; all disabled returns None; an uninstalled enabled backend remains configurable but cannot be started. Assert explicit model/mode/resume are unchanged. Assert Codex request construction obtains `openai_api_key` and never `anthropic_api_key`.
- [ ] Run `cargo test -p bondsymphonic-ide --lib` and focused persistence/restore/reconnect targets, confirming the new tests fail.
- [ ] Implement one-time read migration and write the per-backend shape on normal save. Keep write-only credential controls and the existing Claude key entry. Thread adapter identity through workspace creation, start, restart, restored tabs, authentication overrides, and model replies. Preserve old agents' Claude adapter identity.
- [ ] Key model/login state by backend and connection/request generation. Drop stale replies after credential, backend, or connection changes. Preserve Claude's newest-family selector; Codex models retain the returned order and arbitrary typed IDs. Supply legacy Claude descriptors when talking to a daemon without descriptor support and never send Codex starts to it.
- [ ] Run focused suites; assert a Codex auth error changes only Codex's gate and a Codex logout clears only Codex state. Commit settings and routing.

## Task 7: Render equal Settings tabs, Setup groups, and agent choices

**Files:** Modify `ide/cpp/{SettingsDialog,SetupPage,AgentChoices,NewAgentDialog,AgentArea,MainWindow}.{h,cpp}`, `ide/src/{ffi,testing}.rs`, `ide/src/qobjects/app_controller.rs`, and `ide/tests/{qobject_smoke,smoke}.rs`. Register new widget sources in `ide/build.rs` only if splitting a shared backend-settings widget out of SettingsDialog.

**Interfaces:** Qt receives descriptor/settings JSON and per-backend state from Task 6. Change `agentchoices` model/mode functions to accept `const QString& backend`; `setModels(const QString& backend, const QList<Choice>& fetched)` updates one backend. Every signal returning model results carries backend identity and a request generation. Settings constructs every backend tab with the same rendering function.

- [ ] Add offscreen widget tests through the existing C++/Rust harness: Claude and Codex tabs have Enabled, Sign-in/actions, Default model, and Default mode controls; no OpenCode tab exists yet; the common default chooser contains enabled backends only. Assert disabling Claude hides its setup rows exactly as disabling Codex does.
- [ ] Test backend switching with late model replies, typed model preservation on refill, backend-specific defaults, unavailable binary guidance, all-disabled New Agent behavior, and restored/restarted Codex tabs retaining their adapter. Test disabled-backend changes do not silently terminate running agents; prevent new starts/restarts until enabled again.
- [ ] Run `cargo test -p bondsymphonic-ide --features require-qt --test qobject_smoke`; confirm new assertions fail, rather than silently skipping Qt.
- [ ] Build Settings Agents with common `New agents use` and `Show turn cost and system lines`, then descriptor-driven tabs. Reuse Setup actions in each tab; group machine prerequisites under System and show backend Setup groups only when enabled. Login terminal/link handling must route the backend-specific action and completion.
- [ ] Add the backend chooser to New Agent; route models, permission modes/note, key, and defaults through the chosen backend. Label transcript panes with the backend and use that backend's login gate; preserve the transcript renderer apart from suppressing unavailable Codex cost metadata.
- [ ] Run Qt tests and smoke tests with both a legacy Claude-only daemon response and a multi-backend fake response. Commit the UI implementation.

## Task 8: Verify the integrated workflow and document it

**Files:** Modify `README.md`, `docs/user-guide.md`, `docs/daemon-protocol-notes.md`, and the preflight notes. Extend daemon/IDE integration tests only for uncovered cross-component behavior.

- [ ] Run an end-to-end fake-backed test: create a workspace, start Claude and Codex, interleave replies/approvals, interrupt one, restart the daemon, restore both transcripts, and resume Codex's saved thread. Assert no cross-agent request, credential, model, permission, or session ID routing.
- [ ] On Windows, dot-source `scripts/env.ps1`; run `cargo test --workspace`, `cargo test -p bondsymphonic-ide --features require-qt`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Record actual results, including environmental failures.
- [ ] Run `./scripts/test-daemon.ps1` for the full WSL suite; confirm the real sandbox tests executed. Re-run the preflight live workflow through the IDE with API-key and ChatGPT sign-in paths, including same-workspace mixed backends and a daemon restart.
- [ ] Document enabling/installing/logging into each backend, shared Codex history, API-key precedence, the two Codex modes, defaults migration, unavailable models, backend-specific failures, and the verified CLI version. Update placeholder claims in README.
- [ ] Review the diff for the spec's isolation guarantees, unchanged Claude behavior, compatibility defaults, secret handling, and generation cleanup. Record any live checks still blocked; do not claim support complete without them. Commit documentation and verification fixes after checks pass.

## Review and execution handoff

Sandbox ownership is confirmed and recorded in the spec. Review this plan before execution. Native execution is recommended because backend, lifecycle, and controller changes share interfaces closely; one implementer can keep those coherent. Subagent-driven execution is the alternative, with a separate implementer/reviewer cycle per task. Preflight precedes product implementation regardless of execution method.
