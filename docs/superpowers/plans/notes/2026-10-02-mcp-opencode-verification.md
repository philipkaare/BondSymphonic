# MCP and OpenCode verification — 2026-10-02

Work continues in the existing `feat/codex-adapter` worktree at
`.superpowers/worktrees/codex-adapter`. The earlier Codex work is retained.
No commit, merge, push, or deployment was performed.

## Verified during implementation

- OpenCode ACP: 17 deterministic protocol/process tests, including interleaved permissions, cancellation, delayed event delivery, session restoration, filesystem scope, and structured-secret redaction.
- OpenCode backend/provider follow-up: 12 backend and 16 provider tests passed, plus 4 provider-related daemon unit tests. Review fixes bind discovery endpoints and generation tickets atomically and permit explicitly configured private DNS origins only within OpenCode's exact host/port grants.
- Provider edits/removal revoke only affected origins; shared origins survive until the last grant is removed. Listener generation changes revoke old connections. A launch/edit guard prevents in-flight preparation from restoring removed permissions.
- MCP authentication/transport: 21 auth, 12 transport, and 2 streaming tests passed. Review regressions cover ping responses while a POST SSE stream remains open, expired-session reset without replaying a mutating call, durable account-replacement invalidation, and BOM/CR/LF/CRLF event framing.
- The narrow setup/settings layout and dark placeholder contrast were checked using real Qt Widgets rendered offscreen at 900×700 and 640×600. The original narrow horizontal overflow regression failed before the grid fix and passed afterward.

## Final integrated verification

Tasks 1–5 of both plans are implemented and reviewed. Task 6 live acceptance remains open.

- Protocol: 38 tests passed.
- Daemon: 600 tests passed across completed broad suites, with one live test ignored. Final audit fixes added six auth regressions and one workspace-cleanup regression, all passing (607 distinct tests across completed targets). The broad invocation was stopped in the existing `run_integration` suite after more than six minutes, with no progress for over five minutes. Its incomplete result is **not a pass**. Later suites were run separately and passed (67 of the 600). Windows `taskkill /T /F` returned `Access denied`; the exact test executable was stopped through `Stop-Process`. A subsequent bounded reproduction confirmed that denied `taskkill` leaves the Python server alive after RunStop; its inherited output pipes then prevent Tokio shutdown. Terminating a retained handle to the proven test child lets the failed test exit immediately. No test was skipped or counted as passing. This pre-existing Windows noop teardown failure remains unresolved; see `.superpowers/run-hang-review.md`. The reproduction cleaned up its children; earlier surviving processes were left untouched because ownership could no longer be proven.
- IDE: the broad run passed 375 tests with Qt enabled. The initial persistence failure exposed a test portability assumption: `directories::BaseDirs::new()` may return no profile directory. The corrected test now asserts the optional-path contract and retains deterministic override/co-location checks; all 21 persistence tests pass, including the formerly excluded test. A new prerequisite-filter regression failed before the OpenCode fix and passed afterward. That gives 377 distinct IDE tests verified, plus the separately passing packaged startup test.
- Packaged Windows release: `scripts/package.ps1 -NoDaemon` succeeded, including the 15-file completeness check. The packaged executable passed its version/offscreen startup test (one additional test). This package omits the unavailable Linux daemon.
- All-target Clippy passed with warnings denied for daemon/protocol and IDE with `require-qt`. Formatting and normal `git diff --check` passed.
- Final auth verification: 27 auth tests pass after the revocation fix; 17 MCP bridge/RPC integration tests pass after the disconnect handler change. The earlier final registry rerun passed all 14 tests.
- Real Qt widget verification passed all 52 checks. Sixteen screenshots cover setup top/bottom, providers, and MCP at 640×600 and 900×700 in both themes. Root visual review found no horizontal overflow, orphan labels, or unreadable dark placeholders in the reviewed final captures.

Logs are under worktree `.superpowers/`: `final-daemon-tests.log`,
`final-daemon-remaining.log`, `final-proto-tests.log`, `final-ide-tests.log`,
`ide-tests-profile-failure.log`, `final-package.log`, `final-packaged-smoke.log`,
and `final-{daemon,ide}-clippy.log`. Screenshots are under
`target/settings-captures/`, with durable copies at repository root in
`.superpowers/visual/setup-final/`.

A cached root release build hit a protected generated archive; a fresh release
build in this worktree succeeded. After the final prerequisite-filter change,
the package was rebuilt again and its startup smoke passed. No permission
changes were made.

## Final acceptance-audit fixes

- Disabled OpenCode no longer makes the overall prerequisites warning fail. The setup page already filters all backend descriptors consistently.
- Workspace deletion now removes its OpenCode home, cache, and runtime directories. A RED/GREEN regression verifies exact cleanup while preserving neighboring OpenCode/Codex workspaces and shared Codex configuration; 13 backend and 5 companion tests pass.

- OAuth disconnect now attempts bounded best-effort revocation only at a discovered, supported endpoint. Local tokens and session epochs are invalidated durably before network work; unsupported/unsafe endpoints are skipped, failures/timeouts cannot restore credentials, and refresh/reload preserves revocation metadata. A configuration/generation check around an immutable client-secret snapshot prevents replacement credentials reaching an old issuer. All 27 auth tests pass; both initial missing behavior and stale-secret regressions were observed RED before their fixes. Root source review and final daemon/protocol all-target Clippy pass.

## Environment limits

`wsl --list --quiet` returns `Wsl/EnumerateDistros/Service/E_ACCESSDENIED`.
Only the Windows Rust target is installed. Linux/bubblewrap execution, Unix
socket forwarding, real OpenCode model tool/edit/resume behavior, and live
Atlassian OAuth/site/tool authorization have not been accepted here.
OpenCode 1.18.33 is pinned from source/protocol compatibility evidence;
deterministic peers do not establish live release compatibility.

The Windows linker initially failed reopening generated IDE import/export
artifacts with LNK1104. For verification commands, setting
`LINK='/NOEXP /NOIMPLIB'` avoids generating those unused artifacts and lets the
IDE test executable build. No machine permissions were changed.

## Remote MCP scope

Streamable HTTP POST supports JSON and SSE replies for the 2025 protocol
baselines. Standalone GET notification/resumption streams, sampling,
elicitation, and newer 2026 transport operations are not advertised.
Local MCP commands are launched through the assigned sandbox handle;
remote credentials remain in the daemon. Live interoperability still needs
the environment checks above.


## Restored-agent startup follow-up

The user's real IDE log showed a successful daemon connection, healthy prerequisites,
and a later new agent start. The immediate “agent ended before the daemon restarted”
text came from the previous agent's saved history. A regression then reproduced
why automatic resume could stall: restoration can finish before the IDE subscribes
to workspace events, while the front-tab trigger waited exclusively for that event.

The daemon now advertises the additive `workspace_ready_snapshot` capability
(default false for older peers). Workspace list/get re-read current state, mask
pending restores, and refuse Ready for an absent or dead sandbox. The IDE uses
Ready snapshots only with this capability and re-arms the existing guarded resume
path. Existing legacy-daemon, destroy, crash-loop, and session-resume guards remain.

Observed RED: the new UI scenario received workspace list/history but never sent
agent.start. GREEN: all nine agent_auto_restart smoke scenarios pass, including
ready-before-connection, capable Creating waiting for an event, legacy stale Ready,
and exactly one restart with the saved session. Daemon/protocol focused checks:
79 passed, including four readiness scenarios. Daemon/protocol all-target Clippy,
workspace formatting, and diff checks pass. The Windows package was rebuilt and
its version/offscreen startup smoke passes; its executable hash matches the release
build. Final IDE all-target Clippy with warnings denied also passes. The UI smoke verifies a single resumed start and the replacement agent becoming Idle; it does not pixel-assert banner removal, which uses the existing attachment-generation logic.

The earlier Windows-only package did not contain the updated Linux daemon and
therefore used the one already installed in the user's WSL distro. The prepared
`.superpowers/start-fixed-app.ps1` builds the Linux daemon from this checkout,
copies it beside the updated packaged IDE, and launches that IDE. Its PowerShell
syntax is verified. It must run from the user's ordinary PowerShell: this managed
environment cannot access WSL and launches GUI windows on an isolated desktop.
Real WSL launch/resume remains unverified here; no claim of live success is made.

## Startup harness updates follow-up

The subsequent startup updater, compatibility checks, completed tests, package and
visual evidence are recorded in [startup harness updates verification](2026-10-02-startup-harness-updates-verification.md).
This feature uses the regular `launch.ps1 -Release -NoSetup` checkout launcher;
the earlier special helper is unnecessary. Live WSL validation remains unavailable
from the managed tool environment.
