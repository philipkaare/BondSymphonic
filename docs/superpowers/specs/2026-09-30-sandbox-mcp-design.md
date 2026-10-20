# General MCP support with a Jira Cloud preset — design

Date: 2026-09-30. Status: draft for user review; product implementation has not started.

## Intent and confirmed scope

Let users add and manage MCP servers centrally in BondSymphonic and use their
tools from sandboxed agents. General MCP support is the feature; Jira Cloud is
one built-in preset that uses exactly the same configuration, authentication,
transport, enablement, and lifecycle mechanisms as user-added servers.
The user explicitly clarified this scope. Existing Claude and Codex agents,
and the forthcoming OpenCode backend, use the same server registry.
The user confirmed Jira Cloud and its availability in all workspaces by default.

"Available" means configured and usable after sign-in, not automatically
authenticated. A missing Jira login must not prevent ordinary coding agents
from starting. Each workspace can disable the integration.

## Recommended architecture

Settings gains an MCP page with Add server, Edit, Remove, Connect/Disconnect,
Test connection, status, global enablement, and backend selection. Users can add
any supported MCP server by URL or a sandbox-local command, or import server
definitions from common `mcpServers` JSON. Imports preview normalized entries,
report unsupported fields, and require resolving ID collisions before saving.
Export includes configuration and secret references, never secret values.

Each server has a stable ID, display name, transport, connection configuration,
authentication settings, enabled backend set, and default workspace enablement.
The workspace override is inherit/enabled/disabled, initially inherit. New
servers default to all workspaces and supported backends, adjustable before
saving. The Jira (Atlassian) preset starts enabled for every supported backend.
It appears through the same server editor and status/actions as other entries.

The daemon owns the server registry, a remote MCP broker, and credential records
keyed by server ID/account. Jira's account is one such record, not a separate
authentication subsystem.
Agents reach it through a stdio MCP bridge running inside their sandbox. The
bridge connects to a workspace/backend-specific Unix socket under that
sandbox's existing runtime mount. It forwards MCP requests, responses, and
notifications, preserving request ID types and cancellation.

The broker handles upstream HTTPS and OAuth. Remote server credentials are never
mounted into agent homes, written into repositories, or included in bridge argv,
sandbox environment, transcripts, or logs. A sandbox is granted access to the
configured tools, not a host shell or arbitrary outbound proxy through the broker.
Every connection is bound to its workspace, backend, integration, and current
lifecycle generation; separate clients have independent MCP session state.

Alternatives considered: backend-native OAuth would require separate logins and
credential stores; binding a shared OAuth home into every sandbox would expose
the account's credentials. The shared broker is recommended for a single login
and consistent workspace enablement.

## General authentication and browser login

Remote servers support no authentication, stored bearer/custom headers, and MCP
OAuth. OAuth discovery, PKCE, client registration where supported, and optional
user-supplied client IDs apply to any compatible remote server. Unsupported
authorization flows produce a server-specific error with editable configuration;
they do not fall through to a Jira-specific path.

Local servers support configured environment values and secret references,
resolved only for their individual MCP process. Editing secret fields is
write-only; blank keeps the existing secret and Remove explicitly deletes it.
Host-side remote credentials stay outside sandbox mounts; local process secrets
are accessible to that process within its sandbox, as its execution requires.
Secret values are omitted from configuration/list/export responses and logs.

## Jira preset

Use the official Streamable HTTP endpoint `https://mcp.atlassian.com/v2/mcp`.
OAuth discovery and registration follow the server's published metadata and the
MCP authorization specification. Do not hard-code an authorization or token
endpoint inferred from an older v1 example. These are generic OAuth mechanisms;
the preset supplies its URL and display defaults only.

The Windows IDE opens the authorization URL and owns a temporary loopback
callback listener. The daemon keeps the PKCE verifier and pending login state;
the IDE sends the callback code/state over the authenticated application RPC.
The daemon validates state, exchanges the code, and persists its credential
record privately outside sandbox mounts. This avoids depending on a Windows
browser reaching a WSL localhost callback, which failed in Codex preflight.

Refresh is serialized per account and updates the record atomically. Login,
disconnect, and account replacement invalidate pending requests and generations.
Disconnect removes local credentials and closes upstream sessions; upstream
revocation is attempted only where supported. An expired/revoked account marks
that MCP server disconnected without changing other MCP servers or the Claude,
Codex, or OpenCode model-provider authentication.

Jira search/read/update tools are the preset's acceptance workflow. The official
endpoint can expose additional Atlassian products according to account/admin
permissions; this first delivery does not add separate product presets or UI.
Tool availability and names come from server discovery rather than hand-written
Jira REST wrappers. The user's backend tool policy still governs tool use.

## Generic MCP configuration

All server entries support sandbox-local stdio and remote Streamable HTTP.
Local commands execute inside the chosen sandbox, never as arbitrary host setup
commands. Their executables must be installed or bound in that sandbox. Remote
entries use the broker with explicit origins and the generic authentication
options above. Legacy remote SSE is outside the initial scope; importing one
must report the unsupported transport rather than silently dropping the server.
Custom HTTPS MCP servers work without changing product code or adding a preset.

Nonsecret configuration is daemon-owned so it survives IDE reconnects. Secret
records are separate, private, and omitted from settings/list responses.
Repository MCP settings remain usable. Product-managed servers use a separate
namespace, so name collisions cannot silently overwrite either configuration.
Jira is not a reserved special case. Generate backend-specific supplemental configuration
without modifying tracked project files or deleting user MCP entries.

Claude and Codex receive native stdio server configuration for the bridge;
OpenCode consumes the same integration descriptors through its ACP session or
native MCP configuration, as verified against the pinned CLI. Additive RPCs
cover server CRUD/import/export, validation/test, status, login begin/finish,
disconnect, and workspace
overrides. Old clients/daemons keep existing behavior and show no unsupported
controls.

Disabling an integration closes its broker sessions immediately and rejects
further calls; adapters refresh tool availability where supported. Otherwise the
UI explains that restarting an agent updates its displayed tool list. Stale
tools cannot continue calling a disabled integration.

## Lifecycle and failures

Workspace/backend startup creates only its own socket/bridge configuration.
Restart, deletion, agent exit, and daemon shutdown close owned sessions and
cancel outstanding calls. Old watchers and delayed replies cannot affect a
replacement. Bound message sizes, request deadlines, and disconnected-client
cleanup. Keep one agent's MCP session IDs separate from another's.

The broker permits only configured upstream origins, validates discovered OAuth
URLs before contacting them, and applies TLS verification and redirect rules.
An enabled remote integration authorizes its named endpoint; it does not relax
the workspace's general network allowlist or private-address rules.

## Implementation areas and acceptance evidence

Add focused daemon modules for MCP configuration, broker, transport, OAuth, and
the stdio bridge. Extend proto RPCs/types, backend preparation, sandbox runtime
ownership, and IDE Settings/workspace controls. Reuse the daemon executable's
subcommand pattern for the bridge rather than distributing another runtime.

Before product implementation, record initialize/tool-list/tool-call against a
generic stdio server and a generic remote server, plus an OAuth discovery/login
flow. Record the Jira endpoint's actual metadata and compatibility as one preset
check. Verify bridge configuration and tool visibility in the pinned Claude,
Codex, and OpenCode CLIs. Record unresolved compatibility explicitly.

Tests cover fragmented frames, numeric/string IDs, parallel clients,
cancellation, expiry/refresh races, malformed responses, pending login timeout,
disconnect/account changes, per-workspace disabling, stale generations,
server CRUD, import/export and collisions, independent OAuth servers/accounts,
generic bearer/custom-header authentication, configuration preservation, and
absence of remote sentinel credentials from all
sandbox specs, files, records, transcripts, and diagnostics. Real bubblewrap
tests prove only the intended socket is reachable and no credential directory
is mounted. Test local secret delivery only to its intended MCP process. Adding
an arbitrary supported server through Settings must make its discovered tools
usable in each backend without a source change. A live Jira read verifies the account/site; mutation tests use a
disposable issue only after the user names an appropriate test project.

## Sources

- [Official Atlassian MCP server](https://atlassian.github.io/atlassian-mcp-server/)
- [OpenCode MCP configuration](https://opencode.ai/docs/mcp-servers/)

## Review decision

Approve or revise the shared broker/single-login architecture and settings
behavior before the implementation plan is written. OpenCode is specified in
`2026-09-30-opencode-adapter-design.md` and consumes this MCP contract.
