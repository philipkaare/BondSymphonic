# Startup harness updates: verification, 2026-10-02

Implemented in the `feat/codex-adapter` checkout. The regular application startup deploys the daemon, checks installed Claude Code, Codex and OpenCode releases, then starts the serving daemon and restores sandboxes. Updates run once per app launch; reconnects and fake daemon test endpoints bypass them. Disabled but installed harnesses are checked; absent harnesses and explicit executable overrides are skipped.

Newest stable means the newest published non-prerelease upstream release. Claude uses its `latest` release channel rather than its delayed `stable` channel. Unchanged or newer installed versions avoid artifact downloads and are never downgraded.

## Installation and compatibility behavior

- Downloads are staged separately. HTTPS, release metadata, SHA256 checksums, bounded archive extraction and Claude's pinned manifest signature are checked before activation.
- Candidate versions must match metadata. Claude's required CLI options, Codex helper transport support and app-server initialization, and OpenCode ACP v1/session-loading capability are checked without model requests. Protocol probes have isolated home/config/cache directories and no inherited credentials.
- Codex and its helper are selected together through an atomic manifest pointing to an immutable release directory. Previous bundles and existing installations remain available. Serving daemons freeze selected paths before constructing sandboxes.
- Instance and per-HOME updater locks protect startup and concurrent updates. Each harness has a 120-second budget. The IDE allows 370 seconds for the complete updater, separate from the normal daemon port handshake.
- Offline, staging, integrity and compatibility failures produce nonfatal warnings and retain the previous selection. Progress appears on the splash and status bar; warning details remain available in the status-bar tooltip. Closing the IDE closes the updater's watched stdin and cancels Linux child process groups.

## Verification evidence

Commands ran from this checkout. Windows Qt commands used `scripts/env.ps1`, `LINK=/NOEXP /NOIMPLIB`, and offscreen Qt where applicable.

| Check | Command / evidence | Result |
| --- | --- | --- |
| Artifact staging | `python -m unittest discover -s scripts -p test_stage_harness.py -v`; `.superpowers/startup-updates-staging.log` | 12 passed |
| Daemon updater and backends | `cargo test -p bondsymphonic-daemon --test harness_updates --test opencode_backend --test codex_backend`; `.superpowers/harness-updates-verified.log` | 27 passed: 9 updater/probe/lock, 13 OpenCode, 5 Codex |
| Startup orchestration | `cargo test -p bondsymphonic-ide --lib --features require-qt launcher::tests:: -j1`; final frozen-source agent execution | 16 passed |
| Qt widgets | Build `qobject_smoke` with `--features require-qt --no-run -j1`, then run `qobject_smoke-8799266e6f5b93dc.exe --nocapture cpp_widgets::widgets_behave_offscreen` | 52 passed |
| Daemon Clippy | `cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings`; `.superpowers/harness-updates-clippy.log` | Passed |
| IDE Clippy | `cargo clippy -p bondsymphonic-ide --all-targets --features require-qt -j1 -- -D warnings`; `C:/git/BondSymphonic/.superpowers/ide-updater-clippy.log` | Passed |
| Formatting / whitespace | `cargo fmt --all -- --check`; `git diff --check`; `.superpowers/startup-updates-{format,diff-check}.log` | Passed |
| Embedded shell syntax | `bash -n scripts/stage-harness.sh` | Passed |
| Windows package | `scripts/package.ps1 -NoDaemon`; `.superpowers/startup-updates-package.log` | Passed; all 15 required package files present |
| Packaged executable | `cargo test -p bondsymphonic-ide --test packaged_smoke` with `BS_PACKAGED_EXE`; `.superpowers/startup-updates-packaged-smoke.log` | 1 passed: version and offscreen startup |

The real-signature fixture generates a local Ed25519 signing key and invokes actual GPG/GPGV. Valid signatures pass; altered manifests and an untrusted signer fail. This test did run, rather than being skipped. It validates the verification implementation, not a downloaded Anthropic release.

The release and staged Windows executables share SHA256 `BC04EBC6577FCF17EC7EA8BAAC94CF3C051200F53610E59B59EA2D1CCF14F5B7`.

Visual captures are under `C:/git/BondSymphonic/.superpowers/visual/startup-updates/`. Review included a long startup warning and the narrow Setup screen in dark and light themes. Warning text stays contained, and Setup content remains reachable by scrolling.

## Review fixes and upstream checks

Independent review found and verified fixes for the missing Linux `CommandExt` import, missing creation of the isolated `CODEX_HOME`, missing launcher `--watch-stdin`, and an unwrapped splash status label. The Codex-home regression was observed failing before the fix (`.superpowers/harness-probe-home-red.log`) and passes through an actual fake protocol process afterward. Archive limits now cover ignored members as well as selected executables. Final review found no remaining blocking issue.

Official sources checked on this date:

- [Codex 0.160.0 release assets](https://github.com/openai/codex/releases/expanded_assets/rust-v0.160.0) and [release packaging workflow](https://raw.githubusercontent.com/openai/codex/main/.github/workflows/rust-release.yml): selected standalone binary/helper archives and digests remain available with the expected member names.
- [OpenCode 1.18.34 assets](https://github.com/anomalyco/opencode/releases/expanded_assets/v1.18.34) and [build script](https://raw.githubusercontent.com/anomalyco/opencode/dev/packages/opencode/script/build.ts): expected Linux archive names, digests and root executable layout.
- [Claude setup and binary verification](https://code.claude.com/docs/en/setup): manifest/signature repository and signing fingerprint `31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE`.
- [Codex home validation](https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs): an explicit `CODEX_HOME` must already exist. [Code Mode helper CLI](https://raw.githubusercontent.com/openai/codex/main/codex-rs/code-mode-host/src/main.rs): `--help` is supported; a `--version` probe is not used.

## Remaining validation boundary

This managed Windows environment cannot access the user's WSL distro. No live upstream installation, Linux sandbox launch, or real model/session request was exercised here. Compatibility checks are bounded preflight checks, not proof of every future model/tool protocol scenario.

The package was intentionally built with `-NoDaemon` and therefore contains no newly built Linux daemon. Use the regular checkout launcher, `launch.ps1 -Release -NoSetup`, from the user's normal PowerShell so it builds and deploys the matching Linux daemon. No special launch script is required for this feature.

## Follow-up: updates stay in the splash phase

The explicit startup preparation phase now begins synchronously before daemon installation. The main window remains hidden until the splash finishes; clicks, its ordinary timer, and intermediate readiness/error signals cannot reveal it while preparation is active. Installation is capped at 120 seconds. Quit and Alt+F4 remain available and cancel startup through application shutdown. Reconnect and fake-daemon smoke behavior remain unchanged.

Automatic Setup presentation is deferred until the main window is shown; this ordering was source-reviewed, without a separate widget regression. Qt testing exposed floating docks appearing over the splash despite their hidden parent. Restored floating docks are now held hidden until presentation, and temporary hidden state cannot overwrite the saved layout. A fixture using an actual persisted floating layout passes.

Final evidence from the frozen follow-up source:

- Launcher tests: 17 passed, including installation timeout/cancellation before updater or daemon spawn; `.superpowers/splash-launcher-green.log`. The new timeout test failed against the unbounded implementation first (`.superpowers/splash-install-timeout-red.log`).
- Qt widgets: 52 passed after the floating-dock fix. Coverage includes phase dismissal guards, readiness/error/completion, isolated Quit/close signals, splash sizing and saved floating layout. Quit tests use the production signal without changing process-global shutdown state.
- IDE all-target Clippy with `require-qt` and warnings denied passed; `C:/git/BondSymphonic/.superpowers/splash-ide-clippy.log`. Workspace formatting and diff checks passed.
- Visual review of `C:/git/BondSymphonic/.superpowers/visual/splash-updates/startup-update-progress.png` and `startup-update-warning-long.png` confirmed a visible Quit button and contained text/layout.
- Final Windows package passed, including the floating-dock fix; `.superpowers/splash-phase-package.log`. Packaged smoke passed 1/1 using `cargo test -p bondsymphonic-ide --features require-qt --test packaged_smoke`; `.superpowers/splash-phase-packaged-smoke.log`.

The final release and staged executable SHA256 is `3BCB607D81BA30889EA56C371F84B987167DB6F02D7C632287220C97851873C6`, superseding the earlier package hash above. Packaging still used `-NoDaemon`; live WSL installation, sandbox and model checks remain outside this environment's verified coverage. Independent final review found no remaining blocking issue.
