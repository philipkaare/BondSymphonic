# OpenCode backend — design

Date: 2026-09-30. Status: approved for implementation; user requested immediate subagent execution after planning.

## Intent

Implement OpenCode as the third equal agent backend, following the earlier Codex
design: local OpenAI-compatible model servers, including Ollama, vLLM, and
SGLang, are driven by OpenCode's agent loop. BondSymphonic supplies the sandbox,
network boundary, transcript, model/provider settings, approvals, and lifecycle.

Reuse the existing Backend/AgentAdapter contracts and descriptor-driven Settings
tabs. OpenCode starts disabled for existing users. Once enabled and installed,
it is offered alongside Claude and Codex in New Agent and the default chooser.
Missing model-server configuration produces actionable OpenCode guidance.

## Recommended transport

Use OpenCode's ACP over stdio, one process per BondSymphonic agent, inside the
OpenCode companion sandbox. This fits the existing process abstraction without
an additional HTTP listener or host-to-sandbox port bridge.

Pin a tested CLI release after a bounded compatibility probe. Capture actual
initialization, session creation/load, prompt, updates, permissions, cancellation,
model selection, and failure messages. Advertise only capabilities observed in
that release. Do not infer current wire fields from generic protocol examples.

The alternative is `opencode serve` with HTTP requests and streamed events. It
offers a documented server API but requires protected listener/port ownership
and cross-namespace transport. Use it only if the pinned ACP implementation
cannot satisfy session resume, tools, permissions, or MCP integration; record
that finding and revise this design before implementation.

ACP client-side filesystem or terminal requests, if the pinned release requires
them, must execute through existing workspace/sandbox facilities. Never service
an agent-supplied absolute path or command directly on the host. Reject
unsupported requests explicitly rather than leaving the agent blocked.

## Providers and models

Settings > Agents > OpenCode gains provider entries: stable ID, display name,
base URL, optional write-only API key, and model IDs. Provide Ollama, vLLM, and
SGLang presets plus custom OpenAI-compatible configuration. Users can configure
multiple endpoints; selection retains provider and model identity together.

Use OpenCode's documented OpenAI-compatible provider package and configuration.
Discover models through each configured server's `/v1/models` when supported;
retain manual model IDs if discovery is missing, fails, or returns no entries.
Allow explicit context/output limits because some local servers do not publish
usable model metadata. Discovery is keyed by provider, credential, and connection
generation; stale replies never overwrite the current selection.

Secrets are separate Windows credential entries per stable provider ID, supplied
only to the intended process. Generated OpenCode configuration refers to secret
environment variable names and never embeds their values. No mandatory API key
for unauthenticated local servers. Do not automatically route to an unrelated
cloud provider when the configured local endpoint is unavailable.

## Reaching local model servers

The sandbox's localhost belongs to its own namespace, not the Windows host or
WSL host. Model endpoints therefore use daemon-managed endpoint forwarding,
preserving the configured URL path and targeting the machine selected in
Settings: WSL host, Windows host, or explicit remote address.

An enabled provider grants its OpenCode sandbox access to that exact configured
origin (host and port), including an explicitly chosen private/loopback target.
It does not add a broad private-address exception to every workspace proxy.
Resolve Windows host routing in the actual WSL network configuration, validate
the target, and surface an unreachable endpoint with a useful error. Reapply
endpoint grants after sandbox restart or provider edits; stale generations lose
their grants. Changes leave Claude/Codex proxies unchanged.

## Sandbox, persistence, and transcript

OpenCode uses a companion per `(workspace, OpenCode)` with separate home, cache,
runtime, and proxy, sharing only the worktree and git protection mounts. Bind the
pinned executable and required runtime/helper files read-only. Its session data
survives agent and daemon restart; another backend never receives that mount.
Persist the OpenCode session ID, provider/model selection, and permission mode
with the existing agent record. Workspace deletion removes owned session data.

Translate streamed assistant text, commands, edits, MCP tool calls/results, and
permissions into existing transcript messages. Avoid duplicate final text,
retain tool IDs/output/diffs/errors, and represent unknown events as redacted
system messages. Report real usage/cost only when supplied; local model costs
must not be fabricated. Distinguish completed, cancelled, failed, and exited.

User messages during an active prompt follow the pinned ACP capability: queue
them until the current prompt ends if steering is unsupported. Interrupt cancels
the active prompt; stopping terminates the process after bounded shutdown.

Offer YOLO and Ask permission modes using verified native OpenCode behavior.
The backend descriptor defines exact mode IDs/labels from the pinned probe.
Forward approval choices supported by ACP, preserving outstanding IDs and
session-scoped decisions. Multiple pending approvals remain individually
answerable. Disabled backends keep running agents but refuse new starts/restarts.

## MCP integration

Consume the shared MCP configuration from
`2026-09-30-sandbox-mcp-design.md`. User-added MCP servers and presets are treated
identically. The Jira Cloud preset is available in all workspaces by default
after account connection. Attach configured servers through the
pinned ACP session's MCP server declarations where supported; otherwise use
supplemental native OpenCode configuration. Never attach the same server twice.
Sandbox-local and remote entries follow the same shared enablement rules. Adding
a server never requires an OpenCode-specific product code change.

## Implementation areas and acceptance evidence

Extend proto backend/setup/provider types; add daemon OpenCode backend, ACP
transport, transcript mapper, provider endpoint forwarding, prerequisites, and
pinned installer. Extend the companion registry and IDE provider settings,
credential routing, model selection, restore/restart, and setup guidance.
Keep CLI protocol handling separate from configuration and sandbox ownership.

Preflight must establish the pinned release's ACP behavior inside real
bubblewrap, dependencies/runtime binds, local model tool-calling support, and
session resume across process and daemon restart. Use fake ACP/provider peers
for deterministic error/lifecycle tests and sanitize captured fixtures.

Tests cover provider credentials/selection isolation, stale discovery,
unreachable/unauthenticated servers, explicit model retention, Windows/WSL
endpoint routing, parallel approvals, cancel/EOF/timeout, transcript replay,
same-workspace mixed backends, real mounts/network grants, Jira visibility, and
old settings/daemon compatibility. Run Windows workspace/Qt tests, the WSL
daemon suite, Clippy, and formatting. A live local model must execute a command,
edit a disposable file, and resume its session; fake success is not live proof.

## Sources

- [OpenCode ACP support](https://opencode.ai/docs/acp/)
- [OpenCode providers](https://opencode.ai/docs/providers/)
- [OpenCode server alternative](https://opencode.ai/docs/server/)
- [OpenCode MCP configuration](https://opencode.ai/docs/mcp-servers/)

## Review decision

Approve or revise ACP stdio, local provider configuration/routing, and the shared
MCP integration before the implementation plan is written. Implement shared MCP
first, then OpenCode against that contract; each has its own plan and tests.
