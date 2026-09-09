# Milestone 4: Claude Code Adapter and In-IDE Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Claude Code" agent tab runs `claude` in stream-json mode inside the workspace sandbox, shows a live transcript with tool cards, asks the user to allow or deny tool use, supports interrupt/stop/resume and history replay, shows session cost in the status bar; and logging in to Claude Code and GitHub happens inside the IDE on a setup page (a terminal pane runs the login command, the login URL opens in the browser, prerequisites are re-checked when it exits).

**Architecture:** The daemon gains an `agents` module: an `AgentManager` (registry, per-agent NDJSON transcript store, event sink) and a `ClaudeAdapter` that spawns `claude -p --input-format stream-json --output-format stream-json ...` through the workspace's `SandboxHandle`, parses stdout lines into `AgentMessage`s, and writes `user`/`control_response` lines to stdin. The sandbox layer gets a real signal-number path (SIGINT for interrupt, SIGKILL for stop). A restricted `system.setup_pty` request runs one of four fixed setup commands in a host-side PTY (not sandboxed, because credentials live in the daemon user's home). The IDE keeps transcript state in Rust (`model/transcript.rs`, `TranscriptModel` QObject), routes `agent.*` events by agent id, and paints a `TranscriptView` (message frames, tool cards, permission bar, input) plus a `SetupPage` and a settings dialog (API key in the OS credential store via `keyring`).

**Tech Stack:** Rust 1.98 (MSVC), tokio, cxx-qt 0.10, Qt 6.9.2 Widgets, `keyring` 3, `shell-words`, Claude Code CLI 2.1.263 (`claude auth login`, `-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompts host`), `gh auth login`, python3 (fake `claude` for tests).

**Spec:** `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §6.1 (sandbox trait), §8 (agents: trait, Claude adapter, credentials, terminal adapter), §12 (prereqs); `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` §2 (layout: agent area, permission bar, status-bar cost), §3, §4, §8 (transcript view), §10 (New Agent dialog), §11 (settings: `api_key_set`, keyring), §12 (error presentation and the in-IDE login flow), §14; `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` §6.3 `agent.*`, §6.4 `agent.state`/`agent.message`, §10 milestone 4.

## Global Constraints

- **Boundary rule (IDE spec §3):** `model/` and `client/` never import Qt; QObjects are thin adapters; C++ lays out, paints and forwards input. Which tool is "always allowed", what the pending permission is, what the cost is, and what state a tab is in are all decided in Rust.
- **Every agent process is spawned through the workspace's `SandboxHandle`** (daemon §6.1) with the sandbox's env (init does `env_clear`; only what the spec gives is set). Credentials: `ANTHROPIC_API_KEY` is passed only in the `claude` command's `SandboxCommand.env` (per-command allowlist, an M2a carry-forward), never in `SandboxSpec.env`.
- **Setup commands run on the host, not in a sandbox**, because `~/.claude` and `gh`'s config are the daemon user's. Only the four fixed actions are allowed (`claude_login` → `claude auth login`; `gh_login` → `gh auth login`; `install_claude` → `bash -lc "curl -fsSL https://claude.ai/install.sh | bash"`; `install_gh` → `sudo apt-get install -y gh`). The request carries an action name, never a command string.
- **Pinned CLI (daemon §8.2):** the adapter records `TESTED_CLAUDE_VERSION = "2.1.263"` and the exact argv; Task 3 verifies every flag is accepted by the installed binary (a wrong flag makes `claude` exit with "unknown option" before any login check, so this is testable without being logged in) and logs a warning when the installed version differs.
- **Transcript persistence (daemon §8.1):** every emitted event is appended to `transcripts/<agent_id>.ndjson` before broadcast; `agent.history` is a file read. `seq` is monotonic per agent; `ts` is RFC 3339.
- **Unknown stream-json message types are stored verbatim** as `System { subtype: "raw", data }` (daemon §8.2). Nothing is dropped.
- **stop semantics (daemon §8.2):** close stdin, wait 5 s, then SIGKILL the process group. **interrupt:** write a `control_request` `interrupt` line; if the process has not returned to `idle` within 2 s, SIGINT.
- **Permission flow (IDE §8):** the permission bar shows tool name, summary, Allow / Deny, and "Always allow this tool for this session" (the IDE auto-answers later requests for the same tool name). Auto-answers are decided in Rust.
- **Input box:** Enter sends, Shift+Enter inserts a newline; disabled while the agent is working except the Interrupt button.
- **History replay (IDE §8):** on tab open, `agent.history` replays into the model before live events are applied; events received during replay are buffered and applied after, de-duplicated by `seq`.
- **Login flow (IDE §12):** logins happen inside the IDE, never via a terminal command the user must type. The setup page lists failing prerequisites with an action button each; the login runs in an embedded terminal pane; URLs matching `https://claude.ai/...` or `https://github.com/login/device...` seen in that pane's output open in the system browser (once per URL); when the pane's process exits the IDE re-runs `check_prereqs`; when every item passes the page dismisses itself.
- **API key (IDE §11):** stored in the OS credential store via `keyring` (service `BondSymphonic`, user `anthropic_api_key`); `settings.json` stores only `api_key_set: bool`. The key is passed to the daemon in `agent.start.options.api_key` and nowhere else; it never appears in logs.
- **Quality gates:** `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, zero MSVC `warning C`; daemon suite green on Windows (noop) and in WSL (`scripts\test-daemon.ps1`). Tests must not require a real Claude login: a fake `claude` (`crates/daemon/tests/fixtures/fake_claude.py`) replays fixtures; the daemon honours `BS_CLAUDE_BIN` (documented test hook) to point at it.
- **No desktop input injection, ever.** GUI verification uses env-gated in-process hooks (removed before commit), the app's own `QWidget::grab()`, and the offscreen smoke test.
- **Environment:** PowerShell 5.1 (no `&&`); `. .\scripts\env.ps1` before cargo on Windows; WSL distro `bondsymphonic` (user `bs`, passwordless sudo, python3 3.12, `claude` at `~/.local/bin/claude`, `gh` 2.45); `.\launch.ps1` for the full app.
- **Commit trailers:** every commit ends with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01DcYV4WREYNvdE2MsWATepg`.

## File structure

Daemon:
- `crates/daemon/src/sandbox/mod.rs`, `exec_client.rs`, `noop.rs` — `SandboxChild.signal: Box<dyn Fn(i32) + Send + Sync>` beside `killer`.
- `crates/daemon/src/agents/mod.rs` (new) — `AgentManager`, `AgentEntry`, `TranscriptStore`, `AgentSink`; `agents/claude_stream.rs` (new) — pure line parser and line builders; `agents/claude.rs` (new) — `ClaudeAdapter` process management; `agents/credentials.rs` (new) — home seeding of Claude files.
- `crates/daemon/src/pty.rs` — `open_host(argv, size)` for setup PTYs; `crates/daemon/src/setup.rs` (new) — action → argv table; `server/handlers.rs`, `server/dispatch.rs`, `daemon.rs`, `main.rs` (capabilities), `workspace/lifecycle.rs` (seed at create).
- Tests: `crates/daemon/src/agents/claude_stream.rs` unit tests over `crates/daemon/tests/fixtures/claude-stream/*.ndjson`; `crates/daemon/tests/fixtures/fake_claude.py`; `crates/daemon/tests/agent_integration.rs`; `crates/daemon/tests/setup_pty_integration.rs`; `tests/sandbox_noop.rs` (signal).

Proto: `crates/proto/src/request.rs` (`SystemSetupPty`), `types.rs` (`SetupAction`), roundtrip tests.

IDE, Rust:
- `crates/ide/src/model/transcript.rs` (new); `client/router.rs` (`subscribe_agent`, generalised early buffer); `qobjects/transcript_model.rs` (new); `qobjects/app_controller.rs` (agent start, prereqs, setup PTY, API key); `qobjects/group_model.rs` + `model/app_state.rs` (`agent_id`, status from agent state); `qobjects/terminal_session.rs` (`attach`, `linkDetected`); `qobjects/settings.rs` (keyring); `Cargo.toml` (`keyring = "3"`, `regex` if not present).
- Tests: `tests/transcript_tests.rs` (new), `tests/router_tests.rs`, `tests/qobject_smoke.rs`, `tests/smoke.rs`.

IDE, C++: `cpp/TranscriptView.{h,cpp}`, `cpp/ToolCard.{h,cpp}`, `cpp/PermissionBar.{h,cpp}`, `cpp/PromptInput.{h,cpp}`, `cpp/SetupPage.{h,cpp}`, `cpp/SettingsDialog.{h,cpp}` (new); `cpp/AgentArea.*`, `cpp/NewAgentDialog.*`, `cpp/MainWindow.*`, `cpp/app.cpp`, `build.rs`.

---

### Task 1: Signal-number path through the sandbox

**Files:**
- Modify: `crates/daemon/src/sandbox/mod.rs:72-82` (`SandboxChild`), `crates/daemon/src/sandbox/exec_client.rs:~215-225`, `crates/daemon/src/sandbox/noop.rs:~195-250`, `crates/daemon/src/pty.rs` (only where `SandboxChild` is destructured)
- Test: `crates/daemon/tests/sandbox_noop.rs` (new test), `crates/daemon/tests/sandbox_integration.rs` (Linux-only bwrap test)

**Interfaces:**
- Consumes: `InitRequest::Kill { pid, signal }` (already carries a signal number, `sandbox/protocol.rs:16-18`), `signal_group(pid, sig)` in `noop.rs`.
- Produces: `pub struct SandboxChild { ..., pub killer: Box<dyn Fn() + Send + Sync>, pub signal: Box<dyn Fn(i32) + Send + Sync> }` — `signal(n)` delivers signal `n` to the child's process group (bwrap: `InitRequest::Kill { pid, signal: n }`; noop: `signal_group(pid, n)`; on Windows noop: `SIGKILL`/`SIGTERM` → `TerminateProcess`, anything else → no-op returning nothing). `killer` stays as `signal(SIGTERM)` so existing callers are unchanged.

- [ ] **Step 1: Write the failing test** in `crates/daemon/tests/sandbox_noop.rs` (follow the existing `killer_terminates_a_running_child` test's setup):

```rust
#[tokio::test]
#[cfg(unix)]
async fn signal_delivers_the_requested_signal() {
    let (handle, _dir) = start_noop().await; // reuse the file's helper
    let child = handle
        .spawn(SandboxCommand {
            argv: vec!["sh".into(), "-c".into(), "trap 'exit 42' INT; sleep 30".into()],
            env: vec![],
            cwd: None,
            pty: None,
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    (child.signal)(libc::SIGINT);
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), child.exit).await.unwrap().unwrap();
    assert_eq!(code, 42, "SIGINT reached the shell's trap");
}
```

- [ ] **Step 2: Run to verify it fails**: `. .\scripts\env.ps1; cargo test -p bondsymphonic-daemon --test sandbox_noop signal_delivers` → compile error: no field `signal`.

- [ ] **Step 3: Implement.** In `mod.rs` add the field with a doc comment. In `exec_client.rs`, next to `killer`, build `signal` from the same `me`/`pid` pair sending `InitRequest::Kill { pid, signal: n }`. In `noop.rs`, `signal` calls `signal_group(pid, n)` on Unix; on Windows, `SIGTERM`/`SIGKILL` (use the `libc` constants under `cfg(unix)` and literal 15/9 on Windows) terminate the process the way `killer` does today, other numbers are ignored with a `tracing::debug!`. Update every `SandboxChild { .. }` construction and destructuring (`pty.rs` takes `child.killer` — leave that, and also carry `signal` into the PTY `Session` so Task 3's interrupt can reuse it: add `signaller: Box<dyn Fn(i32) + Send + Sync>` to `pty::Session` but do not change PTY behaviour).

- [ ] **Step 4: Run** the noop test → passes. Add the bwrap counterpart in `tests/sandbox_integration.rs` (same body over the bwrap backend, `#[cfg(target_os = "linux")]`, skipping when `bwrap` is unavailable like its siblings) and run `.\scripts\test-daemon.ps1`.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): signal-number path through SandboxChild for interrupt and kill"
```

---

### Task 2: Claude stream-json parser, fixtures, transcript store

**Files:**
- Create: `crates/daemon/src/agents/mod.rs`, `crates/daemon/src/agents/claude_stream.rs`, `crates/daemon/tests/fixtures/claude-stream/simple_turn.ndjson`, `.../tool_use_turn.ndjson`, `.../permission_turn.ndjson`, `.../partial_messages.ndjson`, `.../unknown_types.ndjson`
- Modify: `crates/daemon/src/lib.rs` (`pub mod agents;`)

**Interfaces:**
- Produces (`agents/claude_stream.rs`):
  - `pub enum Parsed { Message(AgentMessageBody), State(AgentState, Option<String>), SessionId(String), Nothing }`
  - `pub fn parse_line(line: &str) -> Vec<Parsed>` — one stdout line → zero or more items, in order (a `system init` yields `SessionId` then `Message(System{subtype:"init",..})`; a `result` yields `Message(Result{..})` then `State(Idle, None)`; a `control_request can_use_tool` yields `Message(PermissionRequest{..})` then `State(WaitingPermission, Some(tool_name))`; malformed JSON yields `Message(System{subtype:"raw", data: {"line": ...}})`).
  - `pub fn user_line(text: &str) -> String` — `{"type":"user","message":{"role":"user","content":[{"type":"text","text":...}]}}` + `\n`.
  - `pub fn control_response_line(request_id: &str, decision: PermissionDecision, updated_input: Option<Value>, message: Option<String>) -> String` — `{"type":"control_response","response":{"subtype":"success","request_id":..,"response":{"behavior":"allow","updatedInput":..}}}` or `{"behavior":"deny","message":..}`.
  - `pub fn interrupt_line(request_id: &str) -> String` — `{"type":"control_request","request_id":..,"request":{"subtype":"interrupt"}}`.
- Produces (`agents/mod.rs`):
  - `pub struct TranscriptStore { dir: PathBuf }` with `pub fn new(dir) -> Self`, `pub async fn append(&self, agent: &AgentId, msg: &AgentMessage) -> std::io::Result<()>` (one JSON line per message, `create_dir_all` on first write), `pub async fn read(&self, agent: &AgentId) -> std::io::Result<Vec<AgentMessage>>` (missing file → empty; a corrupt line is skipped with a `warn!`).
  - `pub struct AgentSink { .. }` with `pub async fn message(&self, body: AgentMessageBody) -> AgentMessage` (assigns `seq`/`ts`, appends to the store, publishes `Event::AgentMessage`) and `pub async fn state(&self, state: AgentState, detail: Option<String>)` (records into the entry, publishes `Event::AgentStateChanged`). Constructed by `AgentManager` (Task 3) from `(events, store, agent_id, workspace_id, entry)`.
  - `pub struct AgentEntry { pub workspace_id: WorkspaceId, pub adapter: AgentAdapterKind, pub state: parking_lot::Mutex<(AgentState, Option<String>)>, pub session_id: parking_lot::Mutex<Option<String>>, pub seq: AtomicU64 }`.

**Fixture shapes** (Claude Code 2.1 stream-json; one JSON object per line; these are synthetic and Task 3 verifies flag acceptance against the real binary — record a real stream with `scripts/record-claude-stream.sh` once someone is logged in, see Task 9):

`simple_turn.ndjson`:
```
{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-opus-5","tools":["Bash","Edit","Read"],"cwd":"/w","permissionMode":"default"}
{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Hello! How can I help?"}],"model":"claude-opus-5"},"session_id":"sess-1"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"num_turns":1,"result":"Hello! How can I help?","session_id":"sess-1","total_cost_usd":0.0042,"usage":{"input_tokens":10,"output_tokens":8}}
```
`tool_use_turn.ndjson`:
```
{"type":"system","subtype":"init","session_id":"sess-2","model":"claude-opus-5","tools":["Bash"]}
{"type":"assistant","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"Listing files."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"session_id":"sess-2"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt\nb.txt","is_error":false}]},"session_id":"sess-2"}
{"type":"assistant","message":{"id":"m3","role":"assistant","content":[{"type":"text","text":"Two files."}]},"session_id":"sess-2"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":2000,"num_turns":2,"result":"Two files.","session_id":"sess-2","total_cost_usd":0.01}
```
`permission_turn.ndjson`:
```
{"type":"system","subtype":"init","session_id":"sess-3","model":"claude-opus-5","tools":["Bash"]}
{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"rm -rf build"}],"behavior":"allow","destination":"session"}]}}
```
(the fake waits for a `control_response` here, then continues with:)
```
{"type":"assistant","message":{"id":"m4","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"rm -rf build"}}]},"session_id":"sess-3"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"","is_error":false}]},"session_id":"sess-3"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":500,"num_turns":1,"result":"Done.","session_id":"sess-3","total_cost_usd":0.002}
```
`partial_messages.ndjson`:
```
{"type":"system","subtype":"init","session_id":"sess-4","model":"claude-opus-5","tools":[]}
{"type":"stream_event","event":{"type":"message_start","message":{"id":"m5","role":"assistant","content":[]}},"session_id":"sess-4"}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"sess-4"}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"session_id":"sess-4"}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}},"session_id":"sess-4"}
{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"sess-4"}
{"type":"stream_event","event":{"type":"message_stop"},"session_id":"sess-4"}
{"type":"assistant","message":{"id":"m5","role":"assistant","content":[{"type":"text","text":"Hello"}]},"session_id":"sess-4"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Hello","session_id":"sess-4","total_cost_usd":0.001}
```
`unknown_types.ndjson`:
```
{"type":"system","subtype":"init","session_id":"sess-5","model":"m","tools":[]}
{"type":"future_kind","payload":{"x":1}}
this is not json
{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":10,"num_turns":0,"session_id":"sess-5","total_cost_usd":0,"errors":["boom"]}
```

- [ ] **Step 1: Write the failing tests** at the bottom of `claude_stream.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::{AgentMessageBody as B, AgentState as S, PermissionDecision};

    fn fixture(name: &str) -> Vec<String> {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-stream/");
        std::fs::read_to_string(format!("{p}{name}")).unwrap().lines().map(str::to_owned).collect()
    }
    fn all(name: &str) -> Vec<Parsed> { fixture(name).iter().flat_map(|l| parse_line(l)).collect() }

    #[test]
    fn simple_turn_yields_init_text_result_and_idle() {
        let items = all("simple_turn.ndjson");
        assert!(matches!(&items[0], Parsed::SessionId(s) if s == "sess-1"));
        assert!(matches!(&items[1], Parsed::Message(B::System { subtype, .. }) if subtype == "init"));
        assert!(matches!(&items[2], Parsed::State(S::Working, _)), "init means the agent is working");
        assert!(matches!(&items[3], Parsed::Message(B::AssistantText { text }) if text == "Hello! How can I help?"));
        assert!(matches!(&items[4], Parsed::Message(B::Result { cost_usd, num_turns, session_id, .. }) if (*cost_usd - 0.0042).abs() < 1e-9 && *num_turns == 1 && session_id == "sess-1"));
        assert!(matches!(&items[5], Parsed::State(S::Idle, None)));
    }

    #[test]
    fn tool_use_and_tool_result_are_linked_by_id() {
        let items = all("tool_use_turn.ndjson");
        let uses: Vec<_> = items.iter().filter(|p| matches!(p, Parsed::Message(B::ToolUse { .. }))).collect();
        let results: Vec<_> = items.iter().filter(|p| matches!(p, Parsed::Message(B::ToolResult { .. }))).collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(results.len(), 1);
        assert!(matches!(uses[0], Parsed::Message(B::ToolUse { id, name, input }) if id == "toolu_1" && name == "Bash" && input["command"] == "ls"));
        assert!(matches!(results[0], Parsed::Message(B::ToolResult { id, output, is_error }) if id == "toolu_1" && output == "a.txt\nb.txt" && !is_error));
        // The text block that accompanied the tool_use is still emitted.
        assert!(items.iter().any(|p| matches!(p, Parsed::Message(B::AssistantText { text }) if text == "Listing files.")));
    }

    #[test]
    fn permission_request_switches_state_and_response_lines_are_well_formed() {
        let items = all("permission_turn.ndjson");
        let idx = items.iter().position(|p| matches!(p, Parsed::Message(B::PermissionRequest { .. }))).unwrap();
        assert!(matches!(&items[idx], Parsed::Message(B::PermissionRequest { request_id, tool_name, input, suggestions }) if request_id == "req-1" && tool_name == "Bash" && input["command"] == "rm -rf build" && suggestions.len() == 1));
        assert!(matches!(&items[idx + 1], Parsed::State(S::WaitingPermission, Some(t)) if t == "Bash"));
        let allow: serde_json::Value = serde_json::from_str(control_response_line("req-1", PermissionDecision::Allow, None, None).trim()).unwrap();
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["request_id"], "req-1");
        assert_eq!(allow["response"]["subtype"], "success");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        let deny: serde_json::Value = serde_json::from_str(control_response_line("req-1", PermissionDecision::Deny, None, Some("no".into())).trim()).unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert_eq!(deny["response"]["response"]["message"], "no");
    }

    #[test]
    fn partial_messages_become_deltas_and_the_final_text_is_not_duplicated() {
        let items = all("partial_messages.ndjson");
        let deltas: Vec<String> = items.iter().filter_map(|p| match p { Parsed::Message(B::AssistantDelta { text }) => Some(text.clone()), _ => None }).collect();
        assert_eq!(deltas, vec!["Hel", "lo"]);
        // The assistant message that follows a streamed block carries the same
        // text; it is emitted as AssistantText so consumers that ignore deltas
        // still see it, and consumers that render deltas replace, not append.
        assert!(items.iter().any(|p| matches!(p, Parsed::Message(B::AssistantText { text }) if text == "Hello")));
    }

    #[test]
    fn unknown_and_malformed_lines_are_kept_as_raw_system_messages() {
        let items = all("unknown_types.ndjson");
        let raws: Vec<_> = items.iter().filter(|p| matches!(p, Parsed::Message(B::System { subtype, .. }) if subtype == "raw")).collect();
        assert_eq!(raws.len(), 2, "one unknown type, one non-JSON line");
        assert!(items.iter().any(|p| matches!(p, Parsed::State(S::Error, Some(d)) if d.contains("boom"))), "an error result becomes state Error with the detail");
    }

    #[test]
    fn user_and_interrupt_lines_are_single_line_json() {
        let u = user_line("hi\nthere");
        assert!(u.ends_with('\n') && !u.trim_end().contains('\n'));
        let v: serde_json::Value = serde_json::from_str(u.trim()).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], "hi\nthere");
        let i: serde_json::Value = serde_json::from_str(interrupt_line("r9").trim()).unwrap();
        assert_eq!(i["request"]["subtype"], "interrupt");
        assert_eq!(i["request_id"], "r9");
    }
}
```

And in `agents/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::*;

    #[tokio::test]
    async fn transcript_store_round_trips_and_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = TranscriptStore::new(dir.path().join("transcripts"));
        let id: AgentId = "ag_t".into();
        let m1 = AgentMessage { seq: 1, ts: "2026-09-09T00:00:00Z".into(), body: AgentMessageBody::UserText { text: "hi".into() } };
        let m2 = AgentMessage { seq: 2, ts: "2026-09-09T00:00:01Z".into(), body: AgentMessageBody::Result { cost_usd: 0.5, duration_ms: 10, num_turns: 1, session_id: "s".into() } };
        store.append(&id, &m1).await.unwrap();
        store.append(&id, &m2).await.unwrap();
        // Corrupt a line by hand.
        let path = dir.path().join("transcripts").join("ag_t.ndjson");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        std::fs::write(&path, text).unwrap();
        let read = store.read(&id).await.unwrap();
        assert_eq!(read, vec![m1, m2]);
        assert!(store.read(&"ag_missing".into()).await.unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --lib agents` → module not found.

- [ ] **Step 3: Implement `claude_stream.rs`.** Parsing rules, in this order per line:
  1. `serde_json::from_str::<Value>` fails → `[Message(System{subtype:"raw", data:{"line": line}})]`.
  2. `type == "system"` and `subtype == "init"` → `[SessionId(session_id), Message(System{subtype:"init", data: whole object}), State(Working, None)]`. Other `system` subtypes → `[Message(System{subtype, data})]`.
  3. `type == "stream_event"`: `event.type == "content_block_delta"` with `delta.type == "text_delta"` → `[Message(AssistantDelta{text})]`; every other stream event → `[Nothing]` (do not store; they are noise).
  4. `type == "assistant"`: for each `message.content[]` block in order: `text` → `AssistantText{text}` (skip empty), `tool_use` → `ToolUse{id, name, input}`; other block types → `System{subtype:"raw", data: block}`.
  5. `type == "user"`: for each block: `tool_result` → `ToolResult{id: tool_use_id, output: content as string (string, or concatenation of `text` items of an array), is_error}`; `text` → `UserText{text}` (only when `--replay-user-messages` echoes; harmless); others → raw.
  6. `type == "control_request"` with `request.subtype == "can_use_tool"` → `[Message(PermissionRequest{request_id, tool_name, input, suggestions: request.permission_suggestions or []}), State(WaitingPermission, Some(tool_name))]`. Other control_request subtypes → raw.
  7. `type == "control_response"` → `[Nothing]` (acks of our interrupt).
  8. `type == "result"`: `cost_usd` = `total_cost_usd` else `cost_usd` else 0.0; `duration_ms`, `num_turns`, `session_id`; emit `Message(Result{..})`; then if `is_error == true` → `State(Error, Some(errors joined or subtype))` else `State(Idle, None)`.
  9. Anything else → raw.
  Builders exactly as the tests parse them (`serde_json::json!` then `to_string` + `\n`).

- [ ] **Step 4: Implement `agents/mod.rs`** (`TranscriptStore`, `AgentEntry`, `AgentSink` as specified; `AgentSink::message` locks nothing across the file append: bump `seq` with `fetch_add`, format `ts` with `chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)`, `store.append`, then `events.publish(Some(ws), Event::AgentMessage{..})`; `AgentSink::state` sets the entry's state and publishes `Event::AgentStateChanged`). `pub mod claude_stream;` in it; `pub mod agents;` in `lib.rs`. `AgentManager` itself is Task 3.

- [ ] **Step 5: Run** `cargo test -p bondsymphonic-daemon --lib agents` → all pass.

- [ ] **Step 6: Clippy, fmt, commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): Claude stream-json parser with fixtures; transcript store and agent event sink"
```

---

### Task 3: Claude adapter, agent manager, `agent.*` handlers, credentials

**Files:**
- Create: `crates/daemon/src/agents/claude.rs`, `crates/daemon/src/agents/credentials.rs`, `crates/daemon/tests/fixtures/fake_claude.py`, `crates/daemon/tests/agent_integration.rs`
- Modify: `crates/daemon/src/agents/mod.rs` (`AgentManager`), `crates/daemon/src/daemon.rs` (`pub agents: agents::AgentManager`), `crates/daemon/src/server/handlers.rs` (six `agent.*` arms), `crates/daemon/src/main.rs` (`capabilities.adapters` gains `Claude`), `crates/daemon/src/workspace/lifecycle.rs` (`seed_home` calls `credentials::seed_claude_files`; `destroy` stops the workspace's agents first), `crates/daemon/src/workspace/mod.rs` (`Workspace.agents`? — no: the manager owns the list; `Workspace::info()` fills `agents` from the manager, see below)

**Interfaces:**
- Consumes: Task 1's `SandboxChild.signal`, Task 2's parser/store/sink, `Daemon::sandbox(id)`, `lifecycle::layout_for`, `DataDirs::home(id)`, `DataDirs::transcripts`.
- Produces:
  - `pub const TESTED_CLAUDE_VERSION: &str = "2.1.263";`
  - `pub fn claude_argv(options: &AgentStartOptions) -> Vec<String>`: base is `[bin, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--permission-prompts", "host"]`, then `--resume <id>` if `resume_session`, `--model <m>` if `model`, `--permission-mode <mode>` if `permission_mode` (validated against `["default","acceptEdits","plan","dontAsk","bypassPermissions","auto","manual"]`, else `InvalidParams`). `bin` is `$BS_CLAUDE_BIN` split with `shell-words` when set (test hook, documented in README), else `claude`.
  - `#[async_trait] pub trait AgentAdapter: Send { async fn start(&mut self) -> Result<(), RpcError>; async fn send(&mut self, text: String) -> Result<(), RpcError>; async fn permission_reply(&mut self, request_id: String, decision: PermissionDecision, updated_input: Option<Value>, message: Option<String>) -> Result<(), RpcError>; async fn interrupt(&mut self) -> Result<(), RpcError>; async fn stop(&mut self) -> Result<(), RpcError>; }`
  - `pub struct ClaudeAdapter` (fields: `sink: AgentSink`, `handle: Arc<dyn SandboxHandle>`, `argv`, `env: Vec<(String,String)>` (`ANTHROPIC_API_KEY` when given), `cwd`, `child: Option<Running { stdin: Mutex<ChildWriter>, signal: Box<dyn Fn(i32)+Send+Sync>, exit: Shared<oneshot>, reader_task: JoinHandle<()> }>`, `pending_request_ids: Mutex<HashSet<String>>`).
  - `AgentManager`: `pub fn new(events: EventBus, transcripts: PathBuf) -> Self`; `pub async fn start(&self, d: &Daemon, p: AgentStartParams) -> Result<AgentStartResult, RpcError>`; `send`, `permission_reply`, `interrupt`, `stop`, `history` mirroring the requests; `pub fn agents_of(&self, ws: &WorkspaceId) -> Vec<AgentId>`; `pub async fn stop_all_in(&self, ws: &WorkspaceId)`.
  - `credentials::seed_claude_files(home: &Path)`: copies, if present in the daemon user's home, `.claude/settings.json`, `.claude/.credentials.json`, `.claude.json` into `<home>/.claude/...` and `<home>/.claude.json` (create dirs; `0600` on Unix for the two credential files; overwrite so a later login refreshes an existing workspace). Returns `Vec<&'static str>` of what was seeded (for logging).

**Adapter behaviour:**
- `start`: `handle.spawn(SandboxCommand { argv, env, cwd: Some(worktree cwd in sandbox), pty: None })`; keep stdin; spawn the reader task: `BufReader::new(stdout).lines()`, for each line `for item in parse_line(&line)`: `Message(b)` → `sink.message(b)` (a `PermissionRequest` also inserts its id into `pending_request_ids`); `State(s, d)` → `sink.state(s, d)`; `SessionId(s)` → entry.session_id = Some(s); `Nothing` → skip. On stdout EOF wait for `exit`; if the last state is not `Error`, emit `State(Exited, Some(format!("exit code {code}")))`. stderr is drained to `tracing::warn!` lines prefixed with the agent id (and the last 20 lines kept for the exit detail). After spawn, `sink.state(Working, None)` is NOT emitted here (init does it); emit `State(Idle, None)`? No — emit nothing until the init line. If the process exits before any init line, the exit detail includes the stderr tail (that is how "not logged in" surfaces).
- `send`: `sink.message(UserText{text})` first, then write `user_line(text)` to stdin, then `sink.state(Working, None)`.
- `permission_reply`: if `request_id` not pending → `NotFound`; write `control_response_line(...)`; remove from pending; `sink.state(Working, None)`.
- `interrupt`: write `interrupt_line(new_id("req"))`; spawn a 2 s timer: if state is still `Working`/`WaitingPermission` afterwards, `(signal)(SIGINT)`.
- `stop`: drop stdin (close), wait up to 5 s for exit, else `(signal)(SIGKILL)`; state → `Exited`.
- `AgentManager::start`: workspace must be `Ready`; adapter `Terminal` → `InvalidParams("terminal agents use pty.open")` (deviation from daemon §8.4, recorded: the IDE drives terminals directly); `seed_claude_files(home)` again (login may have happened after creation); `api_key` → env; create `AgentEntry` + `AgentSink`; `ClaudeAdapter::start`; insert; return id. `history` → `store.read`. `agents_of` for `WorkspaceInfo.agents`: `Workspace::info()` is called in many places; give `Daemon` a `fn workspace_info(&self, ws: &Workspace) -> WorkspaceInfo` that fills `agents` from the manager and use it in `handlers.rs` (`WorkspaceGet`, `WorkspaceList`) and `emit_state`.
- `lifecycle::destroy` calls `d.agents.stop_all_in(&id).await` before tearing down the sandbox.

**Fake claude** (`tests/fixtures/fake_claude.py`, python3, no dependencies): argv is ignored except `--resume X` is echoed into the init line's `session_id`; `FAKE_CLAUDE_FIXTURE` env names the fixture file to replay; behaviour: print lines one per 20 ms; on a line `{"type":"control_request",...}` print it and block until a stdin line with `"type":"control_response"` arrives, then continue; after the fixture ends, keep reading stdin: each `user` line triggers a replay of `simple_turn.ndjson`'s assistant+result lines with the text echoed (`"echo: <text>"`); a `control_request` `interrupt` line makes it print a `result` line with `"subtype":"error_during_execution","errors":["interrupted"]`... no: interrupt yields `State(Error)` then; instead print `{"type":"control_response","response":{"subtype":"success","request_id":..}}` followed by a normal result line with `num_turns: 0` (state Idle). SIGINT → exit 130. EOF on stdin → exit 0. Lines are flushed.

- [ ] **Step 1: Write the failing integration test** `crates/daemon/tests/agent_integration.rs` (Windows + WSL over the noop backend; `BS_CLAUDE_BIN` set for the daemon process via `std::env::set_var` before `start_daemon` — the daemon reads it at spawn time; on Windows the value is `"<python.exe> <fixture dir>/fake_claude.py"` with the paths quoted; detect python as `python3` on Unix and `python` on Windows, skipping with a message if neither runs `--version`):

```rust
mod common;
use bondsymphonic_proto::*;
use common::{create_ws, init_repo, start_daemon, Client};
use std::time::Duration;

fn python() -> Option<String> { /* try "python3" then "python"; return the one whose --version succeeds */ }
fn fixture_dir() -> std::path::PathBuf { std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures") }

async fn next_agent_events(c: &mut Client, ag: &AgentId, want: usize, limit: Duration) -> Vec<Event> {
    // drain until `want` agent events for `ag` were seen or `limit` passed; nudge with WorkspaceList like fs_watch_integration does
}

#[tokio::test]
async fn claude_agent_streams_a_turn_and_records_history() {
    let Some(py) = python() else { eprintln!("skipping: no python"); return };
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    std::env::set_var("BS_CLAUDE_BIN", format!("\"{py}\" \"{}\"", fixture_dir().join("fake_claude.py").display()));
    std::env::set_var("FAKE_CLAUDE_FIXTURE", fixture_dir().join("claude-stream/tool_use_turn.ndjson").display().to_string());
    let (port, token, _d, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    let v = c.call(Request::AgentStart(AgentStartParams { workspace_id: ws.id.clone(), adapter: AgentAdapterKind::Claude, options: AgentStartOptions::default() })).await.unwrap();
    let started: AgentStartResult = serde_json::from_value(v).unwrap();
    let ag = started.agent_id.clone();

    let events = next_agent_events(&mut c, &ag, 8, Duration::from_secs(10)).await;
    let bodies: Vec<String> = events.iter().filter_map(|e| match e { Event::AgentMessage { message, .. } => Some(format!("{:?}", message.body).split(' ').next().unwrap().to_string()), _ => None }).collect();
    assert!(bodies.iter().any(|b| b.starts_with("System")), "{bodies:?}");
    assert!(bodies.iter().any(|b| b.starts_with("ToolUse")), "{bodies:?}");
    assert!(bodies.iter().any(|b| b.starts_with("ToolResult")), "{bodies:?}");
    assert!(bodies.iter().any(|b| b.starts_with("Result")), "{bodies:?}");
    let states: Vec<AgentState> = events.iter().filter_map(|e| match e { Event::AgentStateChanged { state, .. } => Some(*state), _ => None }).collect();
    assert_eq!(states.first(), Some(&AgentState::Working));
    assert_eq!(states.last(), Some(&AgentState::Idle));

    // A user message is recorded before it is sent, then the fake echoes it.
    c.call(Request::AgentSend(AgentSendParams { agent_id: ag.clone(), text: "hello".into() })).await.unwrap();
    let more = next_agent_events(&mut c, &ag, 4, Duration::from_secs(10)).await;
    assert!(more.iter().any(|e| matches!(e, Event::AgentMessage { message, .. } if matches!(&message.body, AgentMessageBody::UserText { text } if text == "hello"))));
    assert!(more.iter().any(|e| matches!(e, Event::AgentMessage { message, .. } if matches!(&message.body, AgentMessageBody::AssistantText { text } if text.contains("echo: hello")))));

    let v = c.call(Request::AgentHistory(AgentIdParams { agent_id: ag.clone() })).await.unwrap();
    let h: HistoryResult = serde_json::from_value(v).unwrap();
    let seqs: Vec<u64> = h.messages.iter().map(|m| m.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq monotonic: {seqs:?}");
    assert!(h.messages.iter().any(|m| matches!(&m.body, AgentMessageBody::UserText { text } if text == "hello")));

    let v = c.call(Request::WorkspaceGet(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap();
    let info: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(info.agents, vec![ag.clone()]);

    c.call(Request::AgentStop(AgentIdParams { agent_id: ag.clone() })).await.unwrap();
    let last = next_agent_events(&mut c, &ag, 1, Duration::from_secs(10)).await;
    assert!(last.iter().any(|e| matches!(e, Event::AgentStateChanged { state: AgentState::Exited, .. })));
    cancel.cancel();
}

#[tokio::test]
async fn permission_request_waits_for_the_reply() {
    // fixture permission_turn.ndjson: expect PermissionRequest + WaitingPermission, then reply Allow,
    // expect ToolUse, ToolResult, Result, Idle. Also: replying to an unknown request_id → NotFound.
}

#[tokio::test]
async fn agent_start_refuses_a_terminal_adapter_and_an_unready_workspace() { /* InvalidParams both */ }

#[tokio::test]
async fn destroying_a_workspace_stops_its_agents() { /* start agent, destroy with force, expect Exited event before workspace destroyed */ }
```

Also a unit test in `claude.rs` for `claude_argv`: default set exactly as pinned; `BS_CLAUDE_BIN="python x.py"` splits into two leading argv entries; invalid permission mode → `Err(InvalidParams)`; `resume_session` adds `--resume <id>`.

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --test agent_integration` → compile errors (no `agents` handlers).

- [ ] **Step 3: Implement** the adapter, the manager, `credentials.rs`, the handler arms (`Request::AgentStart(p) => ok(d.agents.start(d, p).await?)`, etc.), capabilities, `workspace_info`, `destroy` ordering, `seed_home` → `seed_claude_files`. Write `fake_claude.py`.

- [ ] **Step 4: Verify the pinned flags against the installed binary** (WSL): `wsl -d bondsymphonic -- bash -lc 'claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompts host --version'` must print the version (flags accepted) — and additionally with `--resume x --model sonnet --permission-mode acceptEdits`. Record the exact output in the report. If any flag is rejected, change `claude_argv` to the accepted spelling and update `TESTED_CLAUDE_VERSION` handling accordingly (this is the one place the plan defers to reality). Add a `warn!` at `start` when `claude --version` (run once per daemon lifetime, cached) does not start with `TESTED_CLAUDE_VERSION`.

- [ ] **Step 5: Run** `cargo test -p bondsymphonic-daemon` on Windows and `.\scripts\test-daemon.ps1` in WSL → green (the WSL run uses python3 and the fake through the noop backend; add one bwrap variant of `claude_agent_streams_a_turn_and_records_history` in `tests/sandbox_integration.rs`-style gating to prove the fake runs inside the real sandbox: `BS_CLAUDE_BIN` pointing at `/usr/bin/python3 <path under the worktree>` — copy the fake into the temp repo so it is visible inside the sandbox).

- [ ] **Step 6: Clippy, fmt, commit**

```bash
git add crates/daemon
git commit -m "feat(daemon): Claude Code adapter over stream-json; agent manager, transcripts, credentials seeding"
```

---

### Task 4: Host-side setup PTY (`system.setup_pty`) and prerequisite re-check

**Files:**
- Create: `crates/daemon/src/setup.rs`, `crates/daemon/tests/setup_pty_integration.rs`
- Modify: `crates/proto/src/types.rs` (`SetupAction`), `crates/proto/src/request.rs` (`SystemSetupPty(SetupPtyParams { action, cols, rows })` → `PtyOpenResult`; method name `system.setup_pty`; roundtrip test entry), `crates/daemon/src/pty.rs` (`open_host`), `crates/daemon/src/daemon.rs` (`host: OnceCell<Arc<dyn SandboxHandle>>` built lazily from `NoopBackend` with `home = the daemon user's real home`, `run_dir = dirs.run.join("host")`, `cwd = home`), `crates/daemon/src/server/handlers.rs` (arm), `crates/daemon/src/prereqs.rs` (no change unless `claude auth status` is a better `claude_auth` probe than the credentials file — check `claude auth status --help`; if it exists and exits non-zero when logged out, use it and keep the file check as fallback)

**Interfaces:**
- Produces (proto): `#[serde(rename_all = "snake_case")] pub enum SetupAction { ClaudeLogin, GhLogin, InstallClaude, InstallGh }`; `SetupPtyParams { action: SetupAction, cols: u16, rows: u16 }`.
- Produces (daemon): `pub fn setup_argv(action: SetupAction) -> Vec<String>` — `ClaudeLogin` → `["claude","auth","login"]`; `GhLogin` → `["gh","auth","login"]`; `InstallClaude` → `["bash","-lc","curl -fsSL https://claude.ai/install.sh | bash"]`; `InstallGh` → `["sudo","apt-get","install","-y","gh"]`. `PtyManager::open_host(&self, d: &Daemon, argv: Vec<String>, size: PtySize) -> Result<PtyOpenResult, RpcError>` — same session/pump code as `open` (refactor the post-spawn part into a private `fn adopt(child, workspace_id: Option<WorkspaceId>, ...)`), `workspace_id: None` in the events' envelope, `HOME`/`PATH` from the host handle's env (the noop backend sets `HOME` to `spec.home`; make sure `PATH` includes `~/.local/bin` so `claude` resolves: pass `env: [("PATH", format!("{home}/.local/bin:{}", std::env::var("PATH")...))]` in the `SandboxCommand`).
- Consumes: `NoopBackend::start(spec)` (`sandbox/noop.rs:56`), `PtyManager` internals.

- [ ] **Step 1: Write the failing tests.** Unit (`setup.rs`): each action maps to the exact argv above; there is no way to pass a free-form command. Integration (`setup_pty_integration.rs`, Unix only for the command bodies; on Windows assert `InvalidParams`/`Internal` is returned cleanly rather than a panic): call `system.setup_pty { action: gh_login }` with `gh` replaced by a stub — set `PATH` for the daemon test process to a temp dir first containing an executable `gh` script that prints `Open https://github.com/login/device and enter code ABCD-1234` and exits 0; assert a `pty.output` event with `workspace_id: None` whose decoded bytes contain that URL, then `pty.exit { code: 0 }`; then `pty.write`/`pty.close` on the returned id behave as for workspace PTYs. Also assert `hello` is still required (unauthenticated → `Unauthorized`).

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --test setup_pty_integration`.

- [ ] **Step 3: Implement** proto + daemon as specified. Keep `handlers.rs`'s arm tiny: `Request::SystemSetupPty(p) => ok(d.ptys.open_host(d, setup::setup_argv(p.action), PtySize{cols: p.cols.max(2), rows: p.rows.max(1)}).await?)`. The host handle is built once (`tokio::sync::OnceCell`) and its `shutdown` is called from the daemon's shutdown path so a login left open does not outlive the daemon.

- [ ] **Step 4: Run** on Windows and in WSL; also run a real smoke by hand in WSL: `wsl -d bondsymphonic -- bash -lc 'gh auth login --help | head -3'` (only to confirm the binary answers; do not log in).

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/proto crates/daemon
git commit -m "feat(daemon,proto): system.setup_pty runs the four fixed setup commands in a host PTY"
```

---

### Task 5: IDE transcript model, agent event routing, `TranscriptModel`, agent start flow

**Files:**
- Create: `crates/ide/src/model/transcript.rs`, `crates/ide/src/qobjects/transcript_model.rs`, `crates/ide/tests/transcript_tests.rs`
- Modify: `crates/ide/src/model/mod.rs`, `crates/ide/src/model/app_state.rs` (`AgentTab.agent_id: Option<AgentId>`, `TabStatus::from_agent_state`), `crates/ide/src/client/router.rs` (`subscribe_agent`, early buffer keyed by `StreamKey`), `crates/ide/src/qobjects/group_model.rs` (`setAgent`, `setAgentStatus`), `crates/ide/src/qobjects/app_controller.rs` (`startAgent`, `agentStarted`, `agentStateChanged`), `crates/ide/src/qobjects/mod.rs`, `crates/ide/build.rs`, `crates/ide/tests/router_tests.rs`, `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Produces (`model/transcript.rs`):
  - `#[derive(Clone, Debug, PartialEq, Serialize)] #[serde(tag = "kind", rename_all = "snake_case")] pub enum TranscriptItem { User { text: String }, Assistant { text: String, streaming: bool }, ToolUse { id: String, name: String, summary: String, input_json: String, result: Option<String>, is_error: bool, collapsed: bool }, Result { cost_usd: f64, duration_ms: u64, num_turns: u32 }, System { text: String } }`
  - `pub fn tool_summary(name: &str, input: &Value) -> String` — `Edit`/`Write`/`Read`/`MultiEdit` → the `file_path`; `Bash` → the `command` (first line, ≤ 80 chars); `Grep`/`Glob` → `pattern`; else the first string-valued field, or the tool name.
  - `pub struct Transcript { pub items: Vec<TranscriptItem>, pub state: AgentState, pub state_detail: String, pub cost_usd: f64, pub turns: u32, pub session_id: Option<String>, pub pending: Option<PendingPermission>, pub always_allow: BTreeSet<String>, last_seq: u64 }` with `pub struct PendingPermission { pub request_id: String, pub tool_name: String, pub summary: String, pub input_json: String }`.
  - `pub enum Applied { Appended(usize), Changed(usize), Nothing }`; `pub fn apply(&mut self, msg: &AgentMessage) -> Applied` — duplicates (`seq <= last_seq`) → `Nothing`; `UserText` → append `User`; `AssistantDelta` → if last item is `Assistant{streaming:true}` append text and `Changed(i)`, else append a new streaming `Assistant` → `Appended`; `AssistantText` → if last item is `Assistant{streaming:true}` replace its text and set `streaming=false` (`Changed`), else append non-streaming; `ToolUse` → append (`summary` from `tool_summary`, `collapsed: true`); `ToolResult` → find the `ToolUse` with that id (search from the end), set `result`/`is_error` → `Changed(i)`, or if none append `System{"tool result for unknown id"}`; `PermissionRequest` → set `pending` (no item) → `Nothing`; `Result` → `cost_usd += cost`, `turns += num_turns`, `session_id = Some`, append `Result` item; `System{subtype:"init", data}` → `session_id` from data, append `System{text: "session <id>, model <model>"}`; other `System` → append `System{text: "<subtype>: <data compact, ≤ 200 chars>"}`.
  - `pub fn set_state(&mut self, state: AgentState, detail: Option<String>)` — clears `pending` when the state leaves `WaitingPermission`.
  - `pub fn decide_permission(&self, tool_name: &str) -> Option<PermissionDecision>` — `Some(Allow)` when `always_allow` contains the tool name, else `None`.
  - `pub fn items_json(&self) -> String`, `pub fn item_json(&self, i: usize) -> String`.
- Produces (`client/router.rs`): `pub fn subscribe_agent(&self, id: &AgentId) -> EventRx` (early-buffered like PTYs; `agent.message`/`agent.state` events carry the id); `pub fn unsubscribe_agent(&self, id: &AgentId)`; internal `enum StreamKey { Pty(PtyId), Agent(AgentId) }` replacing the PTY-only maps.
- Produces (`app_state.rs`): `TabStatus::from_agent_state(&AgentState) -> TabStatus` (Idle→Idle, Working→Working, WaitingPermission→WaitingPermission, Error→Error, Exited→Done); `AgentTab.agent_id: Option<AgentId>`; `Workspaces::set_agent(ws, agent_id)`, `Workspaces::set_agent_status(agent_id, status, detail) -> Option<(group, tab)>`.
- Produces (`GroupModel`): invokables `setAgent(workspaceId, agentId)`, `setAgentStatus(agentId, state: QString, detail: QString)`; `state_json`/`active_tab_json` include `agent_id`.
- Produces (`AppController`): invokable `startAgent(workspaceId, optionsJson)` (`AgentStartOptions` JSON; the API key from the credential store is merged in by the controller, never by C++), signal `agentStarted(workspaceId, agentId)`, signal `agentStateChanged(agentId, state, detail)` (from `drain_events`), `operationFailed("agent.start", msg)` on failure. `createWorkspace` gains a `startClaude: bool` + `initialPrompt: QString` + `optionsJson: QString` variant: `createWorkspaceWithAgent(repo, base, name, group, optionsJson, initialPrompt)` — after `workspace_created`, calls `agent.start`, then `agent.send(initialPrompt)` if non-empty, emitting `agentStarted` in between. (`createWorkspace` for terminal tabs is unchanged.)
- Produces (`TranscriptModel` QObject, one per Claude tab, created from C++ with `new TranscriptModel(parent)`): properties `agentId`, `workspaceId`, `state: QString` (snake_case of `AgentState`), `stateDetail`, `costUsd: f64`, `turns: i32`, `pendingJson: QString` (empty when none), `busy: bool` (true while history is replaying); invokables `attach(workspaceId, agentId)` (subscribes, fetches `agent.history`, replays, then applies buffered live events), `send(text)`, `reply(requestId, allow: bool, alwaysAllow: bool, message: QString)` (records the tool name in `always_allow` when asked; sends `agent.permission_reply`), `interrupt()`, `stop()`, `itemsJson()`, `itemJson(i)`, `setCollapsed(i, bool)`; signals `itemAppended(i)`, `itemChanged(i)`, `resetItems()`, `permissionRequested()` (pendingJson set), `permissionCleared()`, `errorOccurred(message)`. Auto-answer: when a `PermissionRequest` arrives and `decide_permission` returns `Some(Allow)`, the model sends the reply itself and never sets `pending`.

- [ ] **Step 1: Write the failing tests** `crates/ide/tests/transcript_tests.rs` (pure model): delta coalescing (`"Hel"`, `"lo"`, then final `AssistantText "Hello"` → exactly one Assistant item `"Hello"` non-streaming); tool result attaches by id and unknown id yields a System item; duplicate `seq` ignored; `Result` accumulates cost and turns across two turns; `PermissionRequest` sets `pending` and `set_state(Idle)` clears it; `decide_permission` honours `always_allow`; `tool_summary` for Bash/Edit/unknown; `items_json` has `kind` tags. Router test (`router_tests.rs`): an `agent.message` dispatched before `subscribe_agent` is replayed on subscribe, and `unsubscribe_agent` closes the receiver. `qobject_smoke.rs`: `TabStatus::from_agent_state` mapping and `set_agent_status` finding the tab.

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-ide --test transcript_tests`.

- [ ] **Step 3: Implement** the model, router generalisation, app-state additions, `TranscriptModel` (follow `terminal_session.rs` for a per-instance QObject with a background task, generation guard, `Drop` aborting; replay: set `busy=true`, subscribe first (so nothing is missed), request `agent.history`, apply each message, then drain the receiver's buffered events, then `busy=false`, emit `resetItems()`), `GroupModel` and `AppController` additions, `build.rs`/`mod.rs` registration.

- [ ] **Step 4: Build and test**: `cargo build -p bondsymphonic-ide`, `cargo test -p bondsymphonic-ide` → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide
git commit -m "feat(ide): transcript model, agent event routing, TranscriptModel QObject, agent start flow"
```

---

### Task 6: C++ transcript view, permission bar, input; New Agent dialog; status-bar cost

**Files:**
- Create: `crates/ide/cpp/TranscriptView.{h,cpp}`, `crates/ide/cpp/ToolCard.{h,cpp}`, `crates/ide/cpp/PermissionBar.{h,cpp}`, `crates/ide/cpp/PromptInput.{h,cpp}`
- Modify: `crates/ide/cpp/AgentArea.{h,cpp}`, `crates/ide/cpp/NewAgentDialog.{h,cpp}`, `crates/ide/cpp/MainWindow.{h,cpp}`, `crates/ide/build.rs`

**Interfaces:**
- `PromptInput : QPlainTextEdit` — Enter emits `submitted(text)` and clears; Shift+Enter inserts a newline; `setBusy(bool)` disables editing and greys the placeholder ("Claude is working…").
- `ToolCard : QFrame` — `ToolCard(const QJsonObject& item, QWidget*)`; header row: tool name in bold, summary elided, status glyph (⏳ no result, ✓ result, ✗ error), expand toggle; body (hidden while collapsed): input JSON pretty-printed in a monospace `QPlainTextEdit` (read-only, max height 200 px) and the result text likewise; `void update(const QJsonObject& item)`; signal `collapsedChanged(bool)`.
- `PermissionBar : QFrame` — `void show(const QJsonObject& pending)` (tool name, summary), Allow / Deny buttons, "Always allow this tool for this session" checkbox; signals `allowed(bool always)`, `denied()`; hidden when cleared.
- `TranscriptView : QWidget` — `TranscriptView(TranscriptModel* model, QWidget*)`: vertical layout { `QScrollArea` (widget with a `QVBoxLayout` of frames + stretch), error banner (hidden; from `errorOccurred` and from `state == error` with `stateDetail`), `PermissionBar`, bottom row { `PromptInput`, "Interrupt" button (enabled while `state == working`), "Stop" button } }. On `resetItems()` rebuild all frames from `itemsJson()`; on `itemAppended(i)` append one frame from `itemJson(i)`; on `itemChanged(i)` update that frame in place (Assistant frame: `QLabel` with `Qt::MarkdownText`, `setTextInteractionFlags(TextSelectableByMouse)`, `setWordWrap`; User frame: plain label with a distinct background wash from `Theme.h`; Result frame: small grey line "turn N · $0.0042 · 1.2 s"; System frame: small grey italic). Auto-scroll: stick to bottom unless the user scrolled up more than one line (track `verticalScrollBar()->value() >= maximum() - lineHeight` before appending). `permissionRequested` → `PermissionBar::show(pendingJson)`; `permissionCleared` → hide. `PromptInput::submitted` → `model->send`; Allow/Deny → `model->reply(requestId, allow, always, "")`; Interrupt → `model->interrupt()`; Stop → `model->stop()`. `busy` → input disabled + "replaying history…" placeholder.
- `AgentArea::showWorkspace(workspaceId, adapter, command, agentId)` — `claude` adapter → a `TranscriptView` page per workspace (created once; `model->attach(ws, agentId)` when the agent id becomes known: `AgentArea::setAgent(workspaceId, agentId)` called from MainWindow on `agentStarted` and from the initial listing for tabs that already carry an agent id); `terminal` as today.
- `NewAgentDialog`: adapter combo `Claude Code` (default when `AppController::capabilities` include `claude`; add a `capabilitiesJson()` invokable to AppController if none exists) / `Terminal`; Claude fields: model (`QLineEdit`, placeholder "default"), permission mode combo (`default`, `acceptEdits`, `plan`, `dontAsk`), initial prompt (`QPlainTextEdit`, 4 rows); Terminal fields as today; accessors `optionsJson()` (`{"model":..,"permission_mode":..}` without empty fields) and `initialPrompt()`.
- `MainWindow`: `onNewAgent` calls `createWorkspaceWithAgent` for Claude; `agentStarted` → `m_groupModel->setAgent` + `m_agentArea->setAgent`; `agentStateChanged` → `m_groupModel->setAgentStatus`; status bar cost: on active-tab change and on the active `TranscriptModel::costUsdChanged`, `m_costLabel` shows `$%.4f` (0 for terminal tabs); `workspaceDestroyed` removes the transcript page.

- [ ] **Step 1: Implement** the four classes and wiring.

- [ ] **Step 2: Verification with a temporary env-gated hook** (`BS_T6_SELFTEST=1`; no desktop input; own-window `grab()` to `<scratchpad>\m4-transcript.png`; removed before commit) against the real daemon in WSL with `BS_CLAUDE_BIN` pointing at the fake claude inside the distro (`python3 /mnt/c/git/BondSymphonic/crates/daemon/tests/fixtures/fake_claude.py`; set `FAKE_CLAUDE_FIXTURE` to `permission_turn.ndjson`; both env vars must reach the daemon process: export them in the `wsl.exe` launch — check `launcher.rs` for how the daemon is started and pass them through `WSLENV` or by writing a temporary wrapper script in the distro; document what worked). The hook: create a Claude workspace on a throwaway repo with initial prompt "hi"; wait for `agentStarted`; log the transcript frames as they appear (kinds, texts); when `permissionRequested` fires, log the bar's tool name and summary, click Allow programmatically (`PermissionBar` exposes `allowForTest()`? — no: call `model->reply(...)` directly, and separately `QMetaObject::invokeMethod` on the button's `click()` slot is in-process and allowed); confirm ToolUse card gets its result and the Result frame shows the cost; confirm the status bar cost equals the sum; send "again" through `PromptInput` (`setPlainText` + `QMetaObject::invokeMethod(input, "submitted")`... simpler: call the same slot the Enter key triggers); log the tab status glyph transitions (working → waiting_permission → working → idle); take the screenshot; destroy; quit. Record every stderr line in the report.

- [ ] **Step 3: Build, clippy, fmt, `cargo test -p bondsymphonic-ide`, zero MSVC warnings; remove the hook; commit**

```bash
git add crates/ide
git commit -m "feat(ide): transcript view with tool cards, permission bar and prompt input; Claude Code in the New Agent dialog"
```

---

### Task 7: Setup page with in-IDE login, prerequisite re-check, settings dialog with API key

**Files:**
- Create: `crates/ide/cpp/SetupPage.{h,cpp}`, `crates/ide/cpp/SettingsDialog.{h,cpp}`
- Modify: `crates/ide/src/qobjects/app_controller.rs` (`prereqsChecked(json)`, `recheckPrereqs()`, `openSetupPty(action, cols, rows)` → `setupPtyOpened(action, ptyId)`, `setApiKey`, `clearApiKey`, `apiKeySet`), `crates/ide/src/qobjects/terminal_session.rs` (`attach(ptyId, cols, rows)`, `linkDetected(url)`), `crates/ide/src/qobjects/settings.rs` (`api_key_set`, keyring helpers), `crates/ide/Cargo.toml` (`keyring = { version = "3", features = ["windows-native"] }`), `crates/ide/cpp/MainWindow.{h,cpp}`, `crates/ide/build.rs`, `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- `AppController`: signal `prereqsChecked(json: QString)` — JSON array of `PrereqStatus`; emitted after every check (start-up and `recheckPrereqs()`); the existing `prereqWarning(message)` stays for the status bar. Invokable `recheckPrereqs()`. Invokable `openSetupPty(action: QString, cols: i32, rows: i32)` (`claude_login`|`gh_login`|`install_claude`|`install_gh`) → `system.setup_pty` → signal `setupPtyOpened(action, ptyId)` or `operationFailed("setup", msg)`. Invokables `setApiKey(key: QString) -> bool` (keyring set; updates `settings.api_key_set`), `clearApiKey()`, `apiKeySet() -> bool`. `startAgent`/`createWorkspaceWithAgent` read the key from keyring at call time and put it in `options.api_key` when set. The key never enters a log line or a signal.
- `TerminalSession::attach(ptyId, cols, rows)`: adopts an already-open PTY (subscribes via the router, sends `pty.resize`), otherwise identical to `open`. Signal `linkDetected(url: QString)`: emitted once per distinct URL when the output stream contains `https://claude.ai/` or `https://github.com/login/device` up to the next whitespace or quote (scan the decoded bytes of each `pty.output` chunk with a rolling 1 KiB tail so a URL split across chunks is found; strip trailing `.`/`,`/`)`/`'`/`"`).
- `SetupPage : QWidget` — `SetupPage(AppController*, QWidget*)`: title "Set up BondSymphonic"; a list of rows, one per `PrereqStatus`: glyph (✓/✗), name, detail, and for failing items an action button: `claude` → "Install Claude Code" (`install_claude`), `claude_auth` → "Log in to Claude Code" (`claude_login`), `gh` → "Install GitHub CLI" (`install_gh`), `gh_auth` → "Log in to GitHub" (`gh_login`), everything else (git, bwrap, userns, sandbox) → the `fix_hint` in a copyable label and no button; below the list an embedded `TerminalWidget` over one `TerminalSession` (hidden until an action runs; 24 rows tall); a "Re-check" button and a "Continue anyway" button (enabled only when every blocking item — `git`, `bwrap`, `userns`, `sandbox` — passes). Behaviour: action click → `controller->openSetupPty(action, cols, rows)` → on `setupPtyOpened` `session->attach(ptyId, cols, rows)`, show the pane, focus it; `session->linkDetected(url)` → `QDesktopServices::openUrl(url)`; `session->exitedSignal()` → `controller->recheckPrereqs()`; `prereqsChecked(json)` → rebuild the rows; when every item passes emit `completed()`.
- `MainWindow`: keeps the central widget in a `QStackedWidget { workbench, SetupPage }`; on `prereqsChecked`: if any *blocking* item fails show the page; if only `claude_auth`/`gh_auth`/`claude`/`gh` fail keep the workbench and show the status-bar warning with a clickable "Set up…" link (a `QLabel` with rich text, `linkActivated` → show the page); Help > "Setup…" shows the page any time; `SetupPage::completed()` and "Continue anyway" return to the workbench. File > "Settings…" opens `SettingsDialog`.
- `SettingsDialog : QDialog` — "Anthropic API key" (`QLineEdit` password echo, placeholder "stored in the Windows credential store" when `apiKeySet()`), "Remove key" button, default permission mode combo (persisted in `settings.json` as `default_permission_mode`, read by `NewAgentDialog` as its initial combo value); OK → `setApiKey` when the field is non-empty.

- [ ] **Step 1: Write the failing pure-Rust tests** in `qobject_smoke.rs`: `terminal_session::find_links(tail: &str) -> Vec<String>` (extract `https://claude.ai/oauth/authorize?x=1` from surrounding text, ignore `https://example.com`, strip a trailing `.`, split-across-chunks case by feeding two halves through a `LinkScanner` struct that keeps the tail); `settings::api_key_set` round trip through `Settings::load/save` using a temp `BS_SETTINGS_PATH` override if `Settings::path()` can be redirected (add such an override if absent, documented test-only); `prereqs_blocking(items) -> bool` helper (names `git|bwrap|userns|sandbox` count as blocking) used by MainWindow through an invokable `prereqsBlock(json) -> bool` on `AppController`.

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-ide --test qobject_smoke`.

- [ ] **Step 3: Implement** Rust then C++. Keyring: `keyring::Entry::new("BondSymphonic", "anthropic_api_key")`; `set_password`, `get_password` (a `NoEntry` error means unset), `delete_credential`. Never log the key; the `Debug` impl of any struct holding it must redact.

- [ ] **Step 4: Verification with a temporary env-gated hook** (`BS_T7_SELFTEST=1`; no desktop input; own-window grab to `<scratchpad>\m4-setup.png`; removed before commit): on start, force the page (`Help > Setup…` equivalent call); log the rows (names, ok flags); trigger `gh_login` with a stub `gh` on the daemon's PATH inside the distro (write `~/.local/bin/gh` in the distro temporarily — a script printing a device URL and exiting 0 — and restore/remove it afterwards; log that the pane showed the URL, that `linkDetected` fired with it, that `QDesktopServices::openUrl` was intercepted — replace it with a logged no-op inside the hook so the browser does not actually open — and that `prereqsChecked` fired after exit); open the settings dialog programmatically, set an API key `sk-test-123`, log `apiKeySet() == true`, confirm via `Get-StoredCredential`-free check that a `keyring` `get_password` round trip returns it, then clear it and log `false`; screenshot; quit. Never print the key itself in the log.

- [ ] **Step 5: Build, clippy, fmt, tests; remove the hook; commit**

```bash
git add crates/ide Cargo.lock
git commit -m "feat(ide): setup page with in-IDE Claude Code and GitHub login, prerequisite re-check, API key settings"
```

---

### Task 8: Smoke test, docs, footprint, end-to-end

**Files:**
- Modify: `crates/ide/src/qobjects/smoke.rs` (steps `start_agent`, `send`, `allow`), `crates/ide/tests/smoke.rs` (fake daemon: `agent.start`, `agent.send`, `agent.permission_reply`, `agent.interrupt`, `agent.stop`, `agent.history`, `system.setup_pty`; emits `agent.state`/`agent.message` sequences from the `permission_turn` shape), `README.md` ("What works now": Claude Code tabs, permission bar, in-IDE login, API key; "Test hooks": `BS_CLAUDE_BIN`, `FAKE_CLAUDE_FIXTURE`, new smoke steps; limitations: synthetic fixtures until a real stream is recorded, no resume across daemon restarts until M6, "always allow" is per IDE session), `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§13 footprint row; §14 smoke description), `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§8.2: the pinned argv and tested version; §8.4: terminal agents use `pty.open` directly), `scripts/record-claude-stream.sh` (new: runs the real `claude` with the pinned flags in a temp repo with a one-line prompt and saves the stdout NDJSON to `crates/daemon/tests/fixtures/claude-stream/recorded-<version>.ndjson`; documented as "run once logged in; then replace the synthetic fixtures' shapes if they differ")

- [ ] **Step 1: Smoke** script `create_claude,open_agent,send,allow,tree,close,destroy,quit` (or extend the existing script; keep the 2b/M3 assertions): the fake daemon answers `agent.start` with an id and emits `agent.state working`, then on `agent.send` emits a `permission_request` + `waiting_permission`; the IDE's transcript model surfaces the bar; the smoke step `allow` calls `TranscriptModel::reply` through the production path (`AppController` gets nothing new: the script reaches the active `TranscriptModel` via a `requestPermissionReply(agentId, requestId, allow)` invokable+signal pair on `AppController` that `MainWindow` routes to the right view — production API, since Milestone 6's notifications will use it); the fake then emits `tool_use`, `tool_result`, `result`, `idle`. Assertions: order contains `agent.start`, `agent.history`, `agent.send`, `agent.permission_reply`, `agent.stop` (from destroy); no `"failed"` warnings for `agent.*`; the journal shows `agent.permission_reply` with `decision: allow`.

- [ ] **Step 2: Footprint** as in M3 plus one Claude tab with a 200-item replayed transcript (the fake daemon can serve a long history): IDE and daemon RSS into spec §13.

- [ ] **Step 3: Docs** as listed; the daemon spec §8.2 records `TESTED_CLAUDE_VERSION` and the exact argv from `claude_argv`.

- [ ] **Step 4: Full verification**: Windows `cargo test --workspace`, clippy, fmt, zero MSVC warnings; WSL `.\scripts\test-daemon.ps1`; `.\launch.ps1` end to end with the fake claude in the distro (`BS_CLAUDE_BIN` / `FAKE_CLAUDE_FIXTURE` reaching the daemon the way Task 6 found) driven by a temporary hook: create a Claude workspace with an initial prompt, allow the permission, see the result and the cost, open Help > Setup and confirm the page lists `claude_auth` as failing with a "Log in to Claude Code" button (do not click it: that would start a real login); screenshot `<scratchpad>\m4-final.png`; remove the hook.

- [ ] **Step 5: Commit**

```bash
git add crates/ide README.md docs scripts
git commit -m "test(ide): smoke covers a Claude agent turn with a permission reply; docs and footprint for milestone 4"
```

---

## Milestone 4 exit criteria

- New Agent → Claude Code creates a workspace, starts `claude` inside its sandbox with the pinned stream-json flags, and the agent pane shows the live transcript: user messages, streamed assistant text, tool cards with results, per-turn cost; the tab glyph follows `agent.state`; the status bar shows the session cost.
- A tool-permission request shows Allow / Deny with "always allow this tool for this session"; the reply reaches `claude` as a `control_response`; auto-allow works for the rest of the session.
- Interrupt and Stop work (control line then SIGINT; stdin close then SIGKILL); a destroyed workspace stops its agents; `agent.history` replays a tab's transcript on reopen.
- Setup page: failing prerequisites are listed with action buttons; "Log in to Claude Code" / "Log in to GitHub" run in an embedded terminal on the host, the login URL opens in the browser automatically, prerequisites are re-checked when the command exits, and the page dismisses itself when everything passes. An API key can be stored in the Windows credential store and is passed to the daemon per agent start.
- Daemon suites green on Windows (noop) and WSL, using the fake `claude`; every pinned flag verified against Claude Code 2.1.263; smoke test covers a permission turn; clippy/fmt clean; footprint recorded.
