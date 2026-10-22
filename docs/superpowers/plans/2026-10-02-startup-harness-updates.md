# Startup Harness Updates Implementation Plan

## Follow-up: keep updates in the splash phase

User requested that updates happen during the splash-screen phase, implemented with subagents. The main window is constructed for normal state restoration but remains hidden until the splash finishes. An explicit initial preparation/update phase starts before daemon deployment; clicks, the ordinary splash timer and intermediate readiness signals cannot dismiss it. Preparation and updates stay bounded, failure releases the phase and displays the retained-version warning, and quitting remains available. Reconnect and smoke-test behavior stays unchanged.

- [x] Implement the explicit phase and defer main-window presentation; cover preparation, completion, failure, dismissal and quit in focused tests.
- [x] Independently review lifecycle ordering, deferred setup dialogs, geometry restoration and cancellation.
- [x] Verify Qt rendering, relevant tests, static checks and the rebuilt Windows package; record the live WSL limitation. Final follow-up evidence is in [verification notes](notes/2026-10-02-startup-harness-updates-verification.md#follow-up-updates-stay-in-the-splash-phase).

> **For agentic workers:** Use superpowers:subagent-driven-development to implement the independently owned tasks below.

**Goal:** Check and update every installed harness once per application launch before restoring sandboxes.

**Architecture:** The IDE invokes a dedicated daemon updater command after deploying the daemon and before serving. The updater stages vendor artifacts, validates integrity and CLI/protocol compatibility, then atomically selects an immutable bundle. Existing installations remain available on failure.

**Tech Stack:** Rust/Tokio, Qt, Bash/Python vendor artifact staging.

**Spec:** User selected newest stable upstream releases with compatibility checks. This document records the agreed design.

## Global constraints

- Newest stable means newest published non-prerelease upstream release; Claude uses latest, not its delayed stable channel.
- Update installed harnesses even when disabled; do not install absent harnesses or replace explicit executable overrides.
- Once per app launch, never on reconnect. Finish before creating any sandbox or resolving binary binds.
- No model calls, authentication changes, or configuration resets during validation.
- Failed/offline/incompatible updates retain the previous selection and produce a visible nonfatal warning.
- Installation and daemon locks prevent concurrent mutations. Existing binaries and configuration are preserved.
- Use the regular launcher. No new special launch script.

## Review focus

- Concurrent app launches and a running daemon must not race an update.
- A failed second Codex artifact must not select a mismatched binary/helper pair.
- Malformed metadata, digest mismatches, unsafe archive members and release prereleases must fail closed.
- Reconnect and a fake daemon endpoint must not rerun the updater.
- A long or offline update must remain visible without freezing Qt or exhausting the port handshake timeout.

## Task 1: Artifact staging (root)

Files: `scripts/stage-harness.sh`, `scripts/test_stage_harness.py`.

Interface: `bash stage-harness.sh <claude|codex|opencode> --stage-dir <absolute>`; final JSON line contains `version`, `binary`, and Codex `helper`. Stage only, never activate. Vendor metadata provides exact stable version and SHA256; extract only expected regular files.

- [x] Write and run failing fixture tests for stable selection, integrity, traversal/link rejection, matching Codex pair and no activation.
- [x] Implement bounded HTTPS fetches and staging against official release metadata.
- [x] Run offline Python tests and review the complete failure path. Twelve tests pass, including real GPG signature verification against a generated fixture, tamper rejection and wrong-signer rejection.

## Task 2: Daemon update coordinator (run_hang_review)

Files: `crates/daemon/src/harness_updates.rs`, `main.rs`, `lib.rs`, backend resolvers and focused tests.

Interface: `update-harnesses` emits newline-delimited JSON records with `harness`, `status`, `from_version`, `to_version`, `message`. Select validated bundles with an atomic `current.json` manifest. Resolve app selections before legacy installation paths, respecting executable overrides.

- [x] Add failing injectable process/filesystem tests for failure fallback, absent/override skip, locks, version handling and compatibility rejection.
- [x] Implement bounded staging, version/CLI/protocol checks, immutable bundles and atomic manifest promotion.
- [x] Remove exact OpenCode source-candidate rejection in favor of verified compatibility; preserve matching Codex helper resolution.
- [x] Run focused daemon tests and Clippy. Twenty-seven focused tests and all-target Clippy pass.

## Task 3: Startup orchestration and progress (settings_ui)

Files: IDE launcher, app controller, splash/main-window UI and corresponding tests.

- [x] Add failing tests for install/update/spawn order, once-only execution, reconnect skip and error fallback.
- [x] Run updater before first spawn with its own bounded timeout and streaming progress callback.
- [x] Display progress and persistent nonfatal warnings; bypass updates for fake daemon test endpoints.
- [x] Run focused Rust/Qt smoke tests and Clippy. Sixteen launcher tests, 52 Qt widget checks and all-target IDE Clippy pass.

## Integration verification

- [x] Review all three tasks together; ensure all production resolvers use the selected bundles, including prerequisite checks and sandbox binds. Independent final review found no remaining blocker; live WSL validation is unavailable in this tool environment.
- [x] Run formatting, relevant tests and static checks once sources settle.
- [x] Rebuild the Windows package and check setup/startup rendering offscreen if changed. Record WSL live-update limitation separately from verified fixture behavior.

Final evidence and the live WSL validation boundary: [startup harness updates verification](notes/2026-10-02-startup-harness-updates-verification.md).
