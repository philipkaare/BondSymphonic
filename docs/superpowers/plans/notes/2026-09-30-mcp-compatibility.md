# MCP compatibility evidence — 2026-09-30

## Evidence and limits

This task verifies BondSymphonic wire fixtures, not broker interoperability. Local Windows executables report Claude Code **2.1.285** and Codex CLI **0.159.2**. `claude --help` advertises `--mcp-config` and `--strict-mcp-config`; `codex --help` advertises dotted TOML `-c/--config` overrides. Codex emits a home-directory/PATH-alias warning in this restricted environment. No MCP initialization, tool visibility, remote tool call, Atlassian discovery or browser OAuth flow was performed here. Linux sandbox CLI versions remain unverified.

## Primary documentation reviewed

Current `/latest` authorization resolves to **2026-07-28**, reviewed alongside the prior transport baseline. [Current authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization) prefers Client ID Metadata Documents, retains DCR as deprecated compatibility fallback, and requires issuer validation before code exchange. `McpAuthConfig::OAuth.client_metadata_url` and `McpCallbackParams.issuer` preserve those inputs; both default absent for older payloads. Resource indicators must be sent at authorization and token exchange. Do not normalize issuer strings before comparison.

- [MCP 2025-11-25 transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports): newline-delimited UTF-8 JSON-RPC stdio; HTTP POST/GET with JSON or event streams. HTTP session IDs and protocol-version headers apply. Legacy SSE is excluded from this implementation.
- [MCP 2025-11-25 authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization): discovery and HTTPS authorization endpoints; metadata must support PKCE S256, with redirect validation and state checks. Missing PKCE support must fail safely.
- [Claude native MCP](https://code.claude.com/docs/en/mcp): supplemental JSON via `--mcp-config`; `--strict-mcp-config` excludes other configured servers and must not be used for additive configuration.
- [Codex native MCP](https://developers.openai.com/codex/mcp/): `mcp_servers` TOML tables support stdio command/args/env, remote HTTP, OAuth and timeouts. Supplemental dotted `-c` arguments are supported by the observed CLI help; additive preservation and actual tool discovery require later sandbox integration tests.

## Proposed supplemental fixtures (not live acceptance evidence)

Claude generated runtime JSON, under a collision-checked product namespace:

```json
{"mcpServers":{"bondsymphonic_mcp_example":{"type":"stdio","command":"/runtime/bondsymphonic-daemon","args":["mcp-bridge","--socket","/runtime/mcp/example.sock"]}}}
```

Pass its runtime file using `--mcp-config`; leave existing project files and user entries untouched. Reserve no Jira-specific ID or code path.

Codex equivalent supplemental overrides, each passed as a separate argv pair (no shell interpolation):

```text
-c mcp_servers.bondsymphonic_mcp_example.command="/runtime/bondsymphonic-daemon"
-c mcp_servers.bondsymphonic_mcp_example.args=["mcp-bridge","--socket","/runtime/mcp/example.sock"]
-c mcp_servers.bondsymphonic_mcp_example.required=false
```

These contain only the bridge command and owned socket, never remote credentials. Namespace collisions must be resolved before generating them. Live native merge behavior is a remaining acceptance gate.

## Tested application payloads

`crates/proto/tests/mcp.rs` exercises old capabilities without MCP, registry snapshots without overrides, default enabled/all-backend HTTP entries, stdio defaults, bearer references, unsupported SSE rejection, all MCP method names and request serialization. A sentinel secret in upsert/import/callback writes is absent from Debug. Registry/export fixtures contain only secret references and discard unknown secret-value fields. Reads cannot carry `McpSecretValue`; writes serialize it deliberately for authenticated application RPC.

Initial proto baseline: 33 tests. Test-first run failed at compile time on absent MCP contracts. Initial green run: 37 tests; final OAuth compatibility green run: 38 tests. Protocol version stays 2, absent capabilities mean unsupported, absent workspace overrides mean inherit. Backend IDs are strings, allowing OpenCode and future backends without widening legacy adapter enums.

## Dependency selection for the broker owner

Pin this compatible stack in the daemon manifest when transport implementation starts:

```toml
reqwest = { version = "=0.12.28", default-features = false, features = ["rustls-tls", "json", "stream"] }
url = "=2.5.8"
oauth2 = { version = "=5.0.0", default-features = false, features = ["reqwest"] }
```

Primary versioned references: [reqwest 0.12.28](https://docs.rs/reqwest/0.12.28/reqwest/), [URL 2.5.8](https://docs.rs/url/2.5.8/url/), [OAuth2 5.0.0](https://docs.rs/oauth2/5.0.0/oauth2/). OAuth2's reqwest integration targets the 0.12 family; using the newer reqwest 0.13 client would need its own integration. This selection is documented, **not compiled or locked by this proto-only task**; no unused network dependencies are added to the wire crate. Verify Rust/toolchain compatibility with the daemon build before claiming that requirement complete. Keep certificate verification, disable default redirects/proxy inheritance, validate discovered origins and use library PKCE rather than handwritten TLS/cryptography.

## Remaining limits

The wire crate cannot prevent callers putting secrets into fields documented as nonsecret (URL, args, env, names or references). Daemon validation must reject embedded credentials and keep imported secret values out of normalized config, preview issues, errors and diagnostics. Enforce size/deadline bounds, revision conflicts, explicit collision resolutions, secret Keep/Set/Remove and blank-keeps-current semantics in the daemon. Jira preset URL remains `https://mcp.atlassian.com/v2/mcp` from the design, not a freshly verified endpoint in this task.

Current [2026-07-28 transport overview](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports) changes protocol semantics: per-request metadata, no server-initiated requests, HTTP cancellation by closing the response stream. The older initialize/session handshake belongs to the compatibility era. The broker must explicitly choose/negotiate the CLI-supported era and implement documented fallback; **do not treat the 2025-11-25 behavior above as current normative behavior**. No live era negotiation is established by this task.
