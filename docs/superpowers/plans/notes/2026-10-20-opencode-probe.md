# OpenCode 1.14.48 ACP probe (2026-10-20)

Live probe of the pinned CLI, installed by `scripts/install-opencode.sh` in the
`bondsymphonic` distro, driven over ACP stdio with throwaway XDG roots. No real
model server was available: a local OpenAI-compatible mock LLM stood in for the
model so text, tools, permissions, cancel, replay and provider errors could be
exercised. Every OpenCode-side frame is real.

Evidence: `crates/daemon/tests/fixtures/opencode/probe-1.14.48/` —
`frames/<scenario>.jsonl` (every frame, `{t, dir, frame}`), `frames/*.stderr.txt`,
`scripts/` (`acpdrive.py` driver, `mockllm.py`, `logproxy.py`, scenario scripts;
set `PROBE_DIR` to rerun).

## Mismatches found in the adapter as first written

1. `agent.<N>.permission` given as the string `"ask"`/`"allow"` in
   `OPENCODE_CONFIG_CONTENT` makes OpenCode exit 1 before answering `initialize`
   ("Expected PermissionActionConfig"). The object form `{"*":"ask"}` works; the
   top-level string `permission` is accepted. (scenarios cfg0–cfg4)
2. Assistant text arrives as `agent_message_chunk`, not `assistant_message_chunk`. (C, C2)
3. The first `tool_call` and `request_permission.toolCall` carry `rawInput:{}`;
   the input arrives in a later `tool_call_update` (in_progress).
4. `rawOutput` is `{output, metadata}` on success, `{error}` on failure.
5. `session/cancel` ends the prompt with `stopReason:"end_turn"`, not
   `"cancelled"`, and chunks may still arrive after the cancel. (C)
6. Provider failures are never JSON-RPC errors: a 401, an unusable model and the
   free tier's version refusal all give an empty `end_turn`; a 500 or refused
   connection retries forever and the prompt never answers. (G401, G500, Grefused, A, B)
7. 1.14.48 cannot use OpenCode's free tier although `opencode models` lists it.
8. First run downloads ripgrep from `github.com` /
   `release-assets.githubusercontent.com`; it also contacts `registry.npmjs.org`.

## Confirmed

- `initialize`: protocolVersion 1, `loadSession`, `sessionCapabilities{close,fork,list,resume}`,
  `mcpCapabilities{http,sse}`, `authMethods[opencode-login]`.
- `session/new` returns `sessionId`, `configOptions` (ids `model`, `mode`),
  `models`, `modes`. `set_config_option`, `set_model`, `set_mode` exist; an
  unknown model is accepted silently, an unknown mode is `-32603 Agent not found`.
- A resumed session keeps its last mode, so `set_mode` after resume is required.
  The private agent's permission beats a project `opencode.json`.
- `session/load` replays history as updates before its response; `session/resume` does not replay.
- `request_permission` options: `once` (allow_once), `always` (allow_always),
  `reject` (reject_once); the id is an integer. `cancelled` rejects the tool.
- No `fs/*` or `terminal/*` peer requests with those client capabilities off.
- stdout is clean NDJSON; migration banners go to stderr.

## Network

- OpenCode honours `HTTP(S)_PROXY` (and lower-case): absolute-form requests for
  `http://`, CONNECT for `https://`. `NO_PROXY=localhost,127.0.0.1` is honoured,
  and is required: the ACP layer talks to OpenCode's own unauthenticated HTTP
  server on 127.0.0.1 (4096 or a random port).
- So a loopback provider baseURL is dialled in the sandbox's own network namespace.
- `opencode models` works offline from a bundled list.

## Plugins

`opencode models`, `opencode auth login` and `opencode acp` load plugins from
`$XDG_CONFIG_HOME/opencode/plugin(s)/`, a `plugin` key in config or
`OPENCODE_CONFIG_CONTENT`, and the working directory's `.opencode/plugin(s)/`
and `opencode.json`. `--pure` disables all of them. `auth login` with no input
never exits on its own.

## `opencode upgrade`

Re-runs the curl installer: replaces `~/.opencode/bin/opencode` (1.14.48 →
1.19.3) and appends a PATH line to `~/.bashrc`; `~/.local/bin/opencode` is left alone.

## Other

- One unreproduced crash when two `acp` processes started at the same moment on
  one XDG home ("PRAGMA journal_mode = WAL").
