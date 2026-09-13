# Where a permission request stops — what is established so far

**Date:** 2026-09-13
**Status:** Blocked on a login. Two of the plan's suspicions are dead; the
remaining boundary cannot be observed until Claude Code is signed in again.

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
