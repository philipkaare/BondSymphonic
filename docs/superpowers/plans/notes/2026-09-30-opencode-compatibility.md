# OpenCode ACP compatibility evidence

Probe date: 2026-09-30. Status: documentation/source probe complete; live compatibility **unverified**. No model requests, account access, installations, or product changes were made.

## Executable evidence

`Get-Command opencode -ErrorAction SilentlyContinue` returned no executable. The usual Windows npm directory contained no OpenCode launcher. `~/.cache/opencode/bin` exists but is empty; a cached models index is not an executable. `wsl -e sh -lc 'command -v opencode; opencode --version'` failed before starting Linux with `Wsl/Service/CreateInstance/E_ACCESSDENIED`. Therefore neither an installed version nor Linux runtime dependencies could be measured. No captured wire fixture exists.

[Official latest release](https://github.com/anomalyco/opencode/releases/tag/v1.18.33) resolved to **v1.18.33** during this probe. This is a source inspection candidate, not a tested pin. Installation must select an explicit artifact and checksum, then validate `--version` and ACP initialization before enabling starts.

## Protocol matrix

All rows below are documentation/source evidence, never live observations.

| Capability | Evidence and adapter requirement |
|---|---|
| Transport | `opencode acp`, UTF-8 newline-delimited JSON-RPC 2.0 on stdio; logs only on stderr. Never use LSP Content-Length framing. |
| Initialization | Request `initialize` with `protocolVersion: 1`, minimal `clientCapabilities`, and client identity. Require response version 1; reject unsupported versions. |
| Creation | `session/new`: absolute sandbox `cwd`, `mcpServers` array. Persist returned `sessionId`. |
| Reload | Gate `session/load` on `agentCapabilities.loadSession`; updates replay before response. Maintain a replay phase to prevent transcript duplication. |
| Resume | Gate `session/resume` on `sessionCapabilities.resume`; reconnect without historical replay. |
| Prompt | `session/prompt` carries `sessionId` and `prompt` content blocks; process `session/update` while its request remains pending. |
| Tools | Merge `tool_call` and partial `tool_call_update` by session/tool ID; preserve content, diffs, locations, input/output, failure. |
| Permissions | Handle peer request `session/request_permission` concurrently with pending prompt. Echo request ID, return selected option or cancelled outcome. |
| Cancellation | Send `session/cancel` notification (no ID); wait bounded time for prompt result, then terminate if necessary. Cancellation and process exit are separate outcomes. |
| Model | Prefer returned `configOptions` and `session/set_config_option`; use advertised opaque option values. Do not assume legacy `models` response exists. |
| MCP | Stdio declarations use command/args/env; HTTP/SSE use type/name/url/headers and require capability flags. Attach once via ACP or native config. |
| Steering | Not live verified; serialize prompts and queue follow-up messages. |
| Sandbox peers | Advertise filesystem and terminal capabilities false until sandbox implementations exist; return JSON-RPC method-not-found for unsupported requests. Never host-execute supplied paths/commands. |

Sources: [OpenCode ACP](https://opencode.ai/docs/acp/), [ACP initialization](https://agentclientprotocol.com/protocol/v1/initialization), [session setup](https://agentclientprotocol.com/protocol/v1/session-setup), [prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn), [tools/permissions](https://agentclientprotocol.com/protocol/v1/tool-calls), [transport](https://agentclientprotocol.com/protocol/v1/transports).

## Tagged implementation findings

The [v1.18.33 ACP service](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.33/packages/opencode/src/acp/service.ts) advertises protocol 1, load, HTTP/SSE MCP, image/embedded context, and list/resume/close/fork. New/load return `configOptions`; configuration IDs include `model`, `effort`, `mode`. `session/set_config_option` model selection emits configuration updates. ACP mode selects an OpenCode agent, so it must not become the Ask/YOLO permission selector. MCP conversion maps stdio to native local config and HTTP/SSE to native remote config; registration catches and ignores failures. Successful session creation therefore does not prove MCP health. Supplement adapter errors/status diagnostics where possible and verify connection separately.

The [permission handler](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.33/packages/opencode/src/acp/permission.ts) offers exact option IDs `once`, `always`, `reject`; permissions are queued per session. Missing permission callback rejects requests. Approved edits can trigger a client `fs/write_text_file` request. This is another reason to explicitly reject unsupported peer methods and never execute them on the host. A cancelled permission outcome maps to reject. Session-scoped persistence of `always` needs a live test; do not promise process-persistent approval memory.

The [event mapper](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.33/packages/opencode/src/acp/event.ts) emits message IDs on text/reasoning updates, replays user/assistant content and tool history, and uses tool-call updates for output snapshots. Transcript code should append text chunks but merge tool snapshots, not concatenate repeated output. Unknown update variants need a redacted system record.

## Provider, permission, and runtime configuration

[Provider documentation](https://opencode.ai/docs/providers/) specifies `provider.<stable-id>.npm = @ai-sdk/openai-compatible`, `options.baseURL`, optional `options.apiKey = {env:...}`, and `models.<id>` with optional context/output limits. The selected native model is provider/model. Use the `/v1` base path for compatible chat-completions servers, and preserve provider identity. Omit API-key fields entirely for unauthenticated servers. Generate only the configured providers and explicitly select the chosen model; no fallback to a cloud model.

[Native permission configuration](https://opencode.ai/docs/permissions/) uses allow/ask/deny rules. Implement product Ask/YOLO using explicit native permission configuration plus ACP decisions, not agent mode IDs. Confirm effective settings despite project/global configuration precedence in the live probe.

[Native MCP configuration](https://opencode.ai/docs/mcp-servers/) supports local command arrays/environment and remote URL/headers, with OAuth options. Use daemon-owned credentials, not an independent OpenCode login flow. OAuth/browser bootstrap and token persistence are not exercised here. ACP declaration headers/env contain resolved secret values: redact them from diagnostics and recordings. Supplemental config must not duplicate ACP attachments.

The [tagged package manifest](https://raw.githubusercontent.com/anomalyco/opencode/v1.18.33/packages/opencode/package.json) pins ACP SDK **0.21.0**, OpenAI-compatible provider **2.0.41**, and MCP SDK **1.29.0**. Source development uses Bun; this does not prove an external Bun runtime is required by release binaries. Measure the Linux release artifact with `file`/`ldd`, `--version`, and ACP under bubblewrap before finalizing runtime binds. Prewarm required package/helper caches in a controlled installation step and make runtime binds read-only. Do not discover/download provider dependencies during agent start.

The Rust daemon already depends on serde_json, Tokio, tokio-util, bytes, futures, and async-trait. A minimal bounded JSON-RPC implementation can use these existing libraries; no ACP Rust dependency is required for initial transport. Pin any future ACP crate to a reviewed schema version; npm SDK version is not a Rust dependency.

## Required follow-up before release claims

Run the Linux candidate inside the actual companion sandbox with isolated home/cache/config. Record exact release/hash and init/new/load/resume/prompt/cancel/permission/config/MCP traffic, replacing the synthetic fixtures with sanitized captures in a separate directory. Exercise a configured local model: command, disposable edit, Ask rejection/approval, YOLO behavior, cancel while approval waits, process restart and session resume. Confirm filesystem peer requests remain sandbox-local and MCP failures surface. Inspect ELF/runtime/helper dependencies and network grants. Until then advertise no capability as live verified, and retain ACP rather than switch to HTTP without evidence of ACP incompatibility.

## Fixtures

Review round 1 adds concrete model state. The [tagged configuration builder](https://github.com/anomalyco/opencode/blob/v1.18.33/packages/opencode/src/acp/config-option.ts#L32) emits `id: model`, `name: Model`, `category: model`, `type: select`, `currentValue`, and flat `options` entries with `value`/`name`. Values are provider/model; displayed names are provider-name/model-name. The builder's `buildModelSelectOptions` emits flat arrays, not grouped options. The [ACP configuration schema](https://agentclientprotocol.com/protocol/v1/session-config-options) also permits groups; those remain defensive parser support, not this release's observed output. Fixtures now give a nonempty new/load/resume response and a matching model selection request, response, and full configuration update. Fixture models have no variants/modes, so the complete configuration array contains only the model selector.

The unsupported `terminal/create` request/error pair is a defensive [ACP terminal](https://agentclientprotocol.com/protocol/v1/terminals) scenario. It is not a tagged-agent emission or captured event. A compliant agent must not request terminal methods when the client advertises `terminal: false`. The inspected event handler's connection type references update, permission, and write callbacks, not terminal callbacks; this inspection does not establish every runtime branch.

Concrete native configuration examples (documentation-derived; also in fixture `nativeConfigurations`):

```json
{"$schema":"https://opencode.ai/config.json","permission":{"*":"ask"}}
```

```json
{"$schema":"https://opencode.ai/config.json","permission":"allow"}
```

The first is the proposed Ask configuration; the second is the proposed YOLO configuration. These use the exact native `permission` field documented in [OpenCode permissions](https://opencode.ai/docs/permissions/). ACP `mode` selects an agent; the inspected `configOptions` model/mode/effort selectors are not a permission-policy channel. Agent-specific permission overrides take precedence over global rules. Before claiming effective Ask/YOLO, validate isolated config/home plus project and selected-agent rules, and observe a command/edit approval under each policy. The examples alone do not prove that configuration precedence preserves the intended product policy.

`crates/daemon/tests/fixtures/opencode/documented-acp-v1.json` contains synthetic protocol examples and provenance. It is useful for fake-peer parsing/lifecycle tests only. IDs, paths, commands, model names, and endpoint addresses are disposable placeholders. It cannot establish OpenCode release compatibility or satisfy live acceptance tests.
