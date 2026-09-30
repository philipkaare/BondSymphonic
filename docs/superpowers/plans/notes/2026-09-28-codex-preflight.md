# Codex compatibility preflight

Status: core ChatGPT/sandbox compatibility verified; live API-key check deferred
by the user. Remaining validation limits are listed below.

## Environment and evidence

- Windows CLI: `codex-cli 0.158.0`, already installed with the Codex desktop app.
- Linux candidate: `codex-cli 0.158.0`, downloaded from the OpenAI Codex release
  artifact `rust-v0.158.0/codex-x86_64-unknown-linux-musl.tar.gz` on GitHub.
  Binary is in the original checkout's ignored
  `.superpowers/sdd/2026-09-28-codex-adapter/bin/` directory. This pins the
  candidate; it does not yet establish a fully tested adapter version.
- The distro initially had neither a Codex binary on PATH nor an API key,
  host auth file, or BondSymphonic shared auth file. Checks reported presence
  only, never credential contents.
- User selected ChatGPT device sign-in. Login uses
  `CODEX_HOME=~/.bondsymphonic/codex-home`, not the user's unrelated Codex home.
- Windows generated schema is captured under the same ignored preflight
  directory in `schema-windows/`.
- A real Linux app-server accepted `initialize`, `initialized`, and `model/list`.
  Initialization returned `userAgent`, `codexHome`, `platformFamily`, and
  `platformOs`. Model discovery returned `data` and `nextCursor`; a
  `remoteControl/status/changed` notification interleaved with the response.
  Capture: ignored `discovery.ndjson`. Model discovery without a live turn is
  **not evidence of working authentication**.

## Probe and automated checks

`scripts/probe-codex.py` records both directions and stderr, correlates reply IDs,
redacts known credentials and credential fields, and bounds request/turn waits.
It sends no model prompt unless `--prompt` is supplied. Approval requests are
declined unless `--approve` is explicitly supplied. Live captures still require
review before committing; only sanitized fixtures belong in source control.

Run the tests on Windows or Linux:

```text
python -m unittest discover -s scripts -p test_probe_codex.py
```

Five tests pass on Windows and Linux: nested/error-text redaction, interleaved notification
and response correlation, timeout with process reaping, prompt EOF failure,
and expired deadlines even when notifications are already queued. The tests
were observed failing before the corresponding implementation.

Baseline before product changes: `cargo test -p bondsymphonic-proto
-p bondsymphonic-daemon --lib` passed, including 227 daemon tests. This is a
Windows unit-test baseline, not a WSL sandbox-suite result.

## Findings affecting implementation

Device sign-in was attempted twice. The user reports that the website continues
to ask for device login to be enabled after enabling it. The client remained
waiting; no successful login was reported. The account/workspace setting causing
this could not be established from local evidence.

A normal browser login in WSL started its callback on port 1455, but a Windows
TCP check timed out, including outside the execution sandbox. The replacement
Windows CLI browser login uses the same shared home through
`\\wsl.localhost\bondsymphonic\home\bs\.bondsymphonic\codex-home`.
  Its Windows callback was verified reachable. Authentication completed, and
  Linux `login status` reports ChatGPT. A real no-tool turn returned PREFLIGHT_OK.
This suggests a browser-login fallback is needed in Setup; device-only login
can leave an otherwise authorized user unable to proceed. Official reference:
[Authentication](https://learn.chatgpt.com/docs/auth).

The generated 0.158.0 `ThreadStartParams` schema includes `untrusted` alongside
`on-request` and `never`. The design's statement that Codex removed `untrusted`
does not match this schema. Keep the two agreed product modes; do not tell users
that the CLI lacks a capability on the basis of the earlier design claim.
Runtime approvals were subsequently verified in the outer sandbox, as recorded below.

Official protocol reference: [Codex App Server](https://learn.chatgpt.com/docs/app-server).
The generated schema for the pinned binary is the version-specific reference.

## Blocking check ledger (spec section 7)

| Check | Result |
|---|---|
| Pinned Linux binary and protocol discovery | Passed for 0.158.0 |
| Live command, file edit, approvals, steering, interrupt, resume | Passed; live-0.158.0.ndjson contains actual frames |
| API-key provider `env_key` without auth.json | Live check explicitly deferred by user; local environment-only delivery, provider configuration, redaction and persistence tests pass |
| Auth-file replacement behavior | Pinned login/src/auth/storage.rs opens with truncate/write/create and flushes in place |
| Login and turn hosts through proxy denial log | Live sandbox turns passed with api.openai.com, auth.openai.com, chatgpt.com only; no denials observed. Browser login runs outside the proxy, so its full host set is not measured |
| Commands and patches inside the project's bwrap sandbox | Passed using actual BwrapBackend, ProxyRegistry, and proxy-shim |

The 0.158.0 CLI requires a separate matching `codex-code-mode-host` executable.
The first live test failed because this helper was absent; adding its read-only
bind at `/opt/bs/codex-code-mode-host` made command and patch execution pass.
Installation/discovery must check the pair, not only `codex --version`.

`turn/start` responds before the turn is necessarily active. The first control
probe raced startup and received `no active turn to interrupt`. Waiting for
`turn/started` before steer/interrupt fixed the live check. The adapter must
handle this startup window explicitly.

Approval fixtures were captured using Codex's read-only inner policy inside the
outer sandbox, with only disposable file writes requested. The shipped mode
remains `danger-full-access` inside the outer sandbox, separately verified.
Both command and file approval requests were accepted successfully. Interrupted
turn completion was observed. Resume was checked in the same app-server and
subsequently through the production backend in a new process. On 2026-09-30 the
live in-place test also passed tool execution and thread resume after a daemon
restart. The mixed-backend test passed separate Claude/Codex histories, approvals,
and restored backend/session identity in one workspace.

The explicit live test is `codex_live_preflight` (ignored in offline suites).
`scripts/run-codex-preflight.sh` runs it with required environment paths. Six
Python probe tests pass; the sixth preserves notifications arriving before RPC
replies, which matters when interrupt completion precedes its acknowledgment.

## Worktree/tooling

Implementation worktree: `.superpowers/worktrees/codex-adapter`, branch
`feat/codex-adapter`. The approved plan/spec were copied into it. Windows git
works there; Linux git cannot interpret its Windows absolute `.git` pointer.
Run worktree bookkeeping with Windows git, and use a disposable Linux-native
repository for live sandbox fixtures. The implemented Codex backend uses a
companion sandbox with its own home, runtime, cache and proxy; the worktree and
git protections are shared with the base workspace.
