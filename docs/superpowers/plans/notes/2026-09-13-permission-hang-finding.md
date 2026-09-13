# Where a permission request stops — what is established so far

**Date:** 2026-09-13
**Status:** Root cause found. Not fixed — the IDE tells the truth about it
instead, and the fix is scoped below.

## The answer

`--permission-prompts host` tells the CLI that a *host* will answer permission
questions. The daemon passes it and never becomes a host that can, so the CLI
resolves every question by **refusing**:

```
TOOL RESULT (is_error=True): This Bash command contains multiple operations.
  The following part requires approval: git -C /tmp/… ls-files README.md
```

The agent sees the refusal, works around it or gives up, and nothing ever
reaches the amber bar. Nobody is asked because the CLI already answered.

Reproduced against 2.1.263 with the daemon's exact argv. Established along the
way, all empirically:

- A command the CLI's own safety check considers harmless (`echo hello`, a
  plain `git ls-files`) runs with no permission step at all. That is why the
  defect looks intermittent: benign work succeeds, and only real work stalls.
- `--permission-mode` is not the lever. `manual`, `plan` and no flag at all
  behave identically here, and the CLI reports `"permissionMode":"default"` in
  its `init` line whatever is passed.
- `--permission-prompts none` behaves the same as `host`, which is the clue:
  with no reachable host, `host` *is* `none`.
- Sending an SDK `initialize` control-request does not help. Tried bare, with
  `hooks`, with `canUseTool: true`, with `capabilities: ["can_use_tool"]` and
  with `permissionPromptToolName`. The CLI accepts the handshake and answers
  it — and still never sends `can_use_tool`.

## What was done instead

The UI stopped claiming a protection it does not provide (option C of three put
to the user). The modes are labelled by what they do here — `YOLO (sandboxed)`,
`Accept edits (other tools blocked)`, `Plan only`, `Ask every time (blocks
instead)` — YOLO is the default for a fresh install, and a note beside every
chooser says why. An existing stored mode is never rewritten to YOLO: migrating
somebody's "ask me" into "run everything unasked" would escalate a user who
never agreed to it.

## The fix, when it is picked up

`claude --help` says `host` means "the SDK host **or** `--permission-prompt-tool`".
The documented, upgrade-resistant path is the second one: the daemon serves a
small MCP server exposing a permission-prompt tool, names it with
`--permission-prompt-tool`, and forwards the call to the IDE's existing
`PermissionBar` — which already works end to end and is covered by six tests in
`crates/ide/tests/transcript_tests.rs`. Only the CLI-facing half is missing.

The alternative — reverse-engineering the host control protocol far enough to
receive `can_use_tool` — is smaller if the handshake is found quickly, and it
is an undocumented surface that can move under us. Five probes did not find it.

---

## What was believed before the login was renewed

Kept because two of the plan's suspicions were wrong, and both were the kind
worth recording.

## The report

*"Permissions in claude are not really working, no clear way to approve them."*
An agent reaches a tool that needs approval and stops; no amber bar appears.

## Suspicion 1: `--permission-mode default` is a usage error — **FALSE**

The spec's prime suspect was that `default`, the first entry in both the IDE's
Settings dropdown and the daemon's `PERMISSION_MODES`, is not among the
choices `claude --help` lists, so every agent started with it died of a usage
error. Probed against the real binary in the `bondsymphonic` distro, 2.1.263:

```
$ claude -p --permission-mode nonsense
error: option '--permission-mode <mode>' argument 'nonsense' is invalid.
Allowed choices are acceptEdits, auto, bypassPermissions, manual, dontAsk, plan.

$ claude -p --permission-mode default
(parsing passes; the run fails later, on the prompt, not on the flag)
```

The choices **are** enforced — `nonsense` is rejected — and `default` is
**accepted** anyway. It is a working alias the help text no longer lists.
Confirmation from the other direction: a run with `--permission-mode manual`
reports `"permissionMode":"default"` in its own `system`/`init` line, so the
two spellings are the same mode.

**Consequence:** the daemon keeps accepting `default`. The IDE still
standardises on `manual` — it is the spelling the CLI documents — but that is
tidiness, not a fix, and no commit may claim otherwise.

## Suspicion 2: the IDE drops the request — **no evidence for it**

The whole IDE-side path is wired and covered:

- `claude_stream::parse_line` turns a `control_request`/`can_use_tool` into
  `AgentMessageBody::PermissionRequest` plus `AgentState::WaitingPermission`.
- `TranscriptModel::sync_pending` publishes it and raises
  `permission_requested`, and is tested by
  `a_permission_request_sets_pending_and_leaving_the_state_clears_it`,
  `a_stale_state_event_does_not_drop_a_pending_permission` and four others in
  `crates/ide/tests/transcript_tests.rs`.
- `TranscriptView` connects `permissionRequested` to `onPermissionRequested`
  (`TranscriptView.cpp:236`), which calls `PermissionBar::show`.

Nothing here is unconnected. If the break is on this side it is subtler than a
missing wire, and the instrumentation will say so.

## What blocks the rest

The distro's Claude Code login has expired:

```
$ claude auth status
{ "loggedIn": false, "authMethod": "none", ... }

# and in a real turn:
"Failed to authenticate: OAuth session expired and could not be refreshed"
```

So no turn reaches a tool, and no `can_use_tool` line can be recorded. The
daemon's own prerequisite reads `loggedIn` out of that JSON rather than
trusting the exit code — which is 0 either way — so the IDE correctly reports
"not logged in" and shows the login gate instead of a composer.

**This is also a candidate explanation for the original report.** An agent
whose every turn dies at authentication runs no tools, so it never asks for
permission, and "no clear way to approve them" is what that looks like from
the pane. It is a candidate, not a conclusion.

## Next step, once the login is renewed

`scratchpad/permhunt.sh` runs the daemon's exact argv in a throwaway repository
with an explicit mode and reports the line types it saw:

```
wsl -d bondsymphonic -- bash -lc 'bash .../permhunt.sh manual "Run the shell command: echo hello"'
```

A `can_use_tool` line in that recording means the CLI asks and the hunt moves
to the daemon's reader; no such line means the CLI does not ask, and the argv
is where to look.
