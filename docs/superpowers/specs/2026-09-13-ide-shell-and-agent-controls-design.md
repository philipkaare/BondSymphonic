# IDE Shell and Agent Controls — Design

**Date:** 2026-09-13
**Status:** Approved for planning
**Parent:** `2026-09-08-bondsymphonic-ide-design.md`

## 1. What this is

Thirteen changes an hour of real use asked for, in one batch. They divide into
four groups that share almost no code:

1. **A bug.** A tool that needs approval hangs the agent and shows nobody
   anything.
2. **The agent pane.** Model and permission mode chosen where the agent is,
   an agent that starts itself, a welcome that says what is about to answer.
3. **The shell.** Dockable panes, a Window menu that can bring a closed one
   back, and the two menus that have been empty since M1.
4. **Two readings.** A light palette, and a workspace naming its repository
   rather than the branch its own name generated.

Each section below states what is wrong, what replaces it, and what proves it.

## 2. Permission requests reach nobody

### What happens

An agent hits a tool that needs approval and stops. No amber bar appears above
the composer, no state word changes on the tab, and the turn never ends. The
user's account: *"permissions in claude are not really working, no clear way to
approve them"*.

### What is known

`claude --help` from the daemon's own distro, at the pinned
`TESTED_CLAUDE_VERSION` of 2.1.263:

```
--permission-mode <mode>    (choices: "acceptEdits", "auto",
                             "bypassPermissions", "manual", "dontAsk", "plan")
--permission-prompts <target>  "host" (the SDK host or --permission-prompt-tool)
                               or "none" ... (default: "host")
```

`default` is **not** among the choices, which made it the prime suspect: it is
the first entry in the IDE's Settings dropdown (`SettingsDialog.cpp`,
`kPermissionModes`) and the first entry in the daemon's own allow-list
(`claude.rs`, `PERMISSION_MODES`).

**Probed, and false.** The choices are enforced — `--permission-mode nonsense`
is rejected, and the error names exactly those six — but `--permission-mode
default` is accepted anyway: it is a working alias the help text no longer
lists. Confirmation from the other side: a run with `manual` reports
`"permissionMode":"default"` in its own `init` line, so the two spellings are
one mode. The IDE still standardises on `manual`, because that is the spelling
the CLI documents, but that is tidiness and not a fix, and nothing may claim it
as one.

`--permission-prompts host` is right, and `claude_stream::parse_line` already
turns a `control_request` of subtype `can_use_tool` into a `PermissionRequest`
message and a `WaitingPermission` state. `TranscriptModel::sync_pending`
publishes it, `TranscriptView` connects `permissionRequested` to
`onPermissionRequested` (`TranscriptView.cpp:236`), and six tests in
`transcript_tests.rs` cover the model's half. So the plumbing exists and is
wired end to end; something on the path is not running, and it is not any of
the wires.

**The hunt is blocked on a login.** The distro's Claude Code OAuth has expired —
`claude auth status` reports `"loggedIn": false`, and a real turn answers
"Failed to authenticate: OAuth session expired and could not be refreshed" — so
no turn reaches a tool and no `can_use_tool` line can be observed at all. That
is also a candidate explanation for the original report: an agent whose every
turn dies at authentication runs no tools, so it never asks for permission, and
from the pane that looks exactly like one that cannot be approved. A candidate,
not a conclusion. See
`docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md`.

### What happens first

Root cause before fix, and evidence before hypothesis. Instrument every
boundary on the path and run one agent into one approval:

```
CLI stdout line
  -> claude_stream::parse_line         (is a control_request arriving at all?)
  -> the reader loop in claude.rs      (is it recorded and published?)
  -> AgentEvent over the socket        (does it leave the daemon?)
  -> the IDE client                    (does it arrive?)
  -> TranscriptModel                   (does permission_requested fire?)
  -> TranscriptView -> PermissionBar   (does the bar show?)
```

The first boundary that does not see it is the one to investigate. One
suspicion is left standing — a control-protocol handshake the daemon may owe
the CLI before it will ask — and it is a suspicion, not a conclusion; the
instrumentation decides.

The fix goes where the break is, with a regression test at that level: a
`claude_stream` unit test if it is parsing, a daemon integration test if it is
the reader or the event, a `TranscriptModel` test if it is the model, a widget
check if it is the bar. **Nothing else in this spec is verifiable until this
is fixed** — every other agent change is tested by driving an agent — so it
lands first and alone.

### Also fixed here

`PERMISSION_MODES` in the daemon keeps all seven values and gains a comment
saying why: six are the CLI's documented choices and the seventh is the
undocumented alias it still accepts, so a mode stored by an older IDE is not
turned into an error. The IDE's own floor moves to `manual` and a stored
`default` reads back as `manual`, so one spelling reaches the CLI from here —
the documented one.

## 3. Permission mode, per agent, with YOLO

Four modes are offered, spelled for a person rather than for a CLI:

| Shown | Sent | Means |
|---|---|---|
| Ask every time | `manual` | Every tool that would prompt, prompts. |
| Accept edits | `acceptEdits` | File edits go through; everything else asks. |
| Plan only | `plan` | Nothing is executed; the agent plans. |
| YOLO (sandboxed) | `bypassPermissions` | Nothing asks. |

`auto` and `dontAsk` are accepted by the daemon but not offered: `dontAsk`
denies silently, which reads like a hang and is the failure this batch is
fixing. YOLO is offered without ceremony and without a confirmation, because
the thing that makes it reasonable is structural — the agent is inside a
sandbox, in a worktree of its own, behind a network proxy — and a dialog
warning about a risk the architecture already took is noise.

**The mode is always sent.** There is no "leave it unset": `manual` is the
default a workspace is created with, and `AgentStartOptions.permission_mode` is
never `None` for a Claude agent. An unset flag means the CLI's own default,
which is a fifth behaviour nobody chose and nobody can see.

One list, three places: the composer dropdown (this agent, now), the New Agent
dialog (this agent, at birth), and Settings ▸ Agents (what a new agent starts
with). The list lives in one place in the source and is read by all three.

## 4. The composer

A second row under the message box:

```
+-- message ------------------------------------------+
|                                                     |
+-----------------------------------------------------+
 [ Opus 5 ▾ ]  [ YOLO (sandboxed) ▾ ]       [Interrupt]
```

Models: Default, Opus 5, Sonnet 5, Haiku 4.5 — the list `NewAgentDialog`
already carries, moved to where both can read it, still editable because Claude
Code takes any name.

**Changing either restarts the agent and resumes the conversation.** `claude -p`
fixes its model and its permission mode at process start; there is no way to
change one in flight. `TranscriptModel::restartOptionsJson` already produces
the tab's options with `resume_session` filled in from the session id seen in
the history, which is exactly a restart that continues the conversation. The
dropdowns reuse it with one field overridden. The transcript records it:

```
— switched to Haiku 4.5, conversation resumed —
```

A switch while the agent is mid-turn interrupts that turn; the dropdown says so
in its tooltip rather than refusing.

Start and Stop leave the composer. **Interrupt stays** — ending a turn is not
killing a process, and it is the only one of the three that acts on the
conversation rather than on the agent.

## 5. An agent starts itself, and says hello

### Auto-start

A Claude workspace starts its agent the moment it has one to start: when it is
created (already true), when a session is restored, and when a reconnect finds
a workspace whose agent is gone. `AgentArea::setStarting` and
`MainWindow::onStartAgentRequested` already carry the machinery; what changes is
who asks — the window, on its own, rather than a button.

An agent that **exits on its own** does not restart itself. It raises the error
banner the pane already has, and that banner grows a Restart button. Exits are
rare, and one that repeats would otherwise become a loop nobody asked for.

### The welcome

A transcript with no items shows a frame the IDE writes — not the daemon, which
has nothing to say until the agent speaks:

```
agent-4 is ready.
Opus 5 · YOLO (sandboxed) · BondSymphonic @ main
/home/bs/.bondsymphonic/worktrees/ws_be2db101
```

It is a view-level frame, not a transcript item: it is not persisted, not
replayed, and it disappears behind the first real message. A pane that has been
waiting for a prompt for a minute should say what is waiting, what it will
answer as, and where it will do the work.

### Restart

Two places, one action:

- **Settings ▸ Agents** — a `Restart agent in <name>` button, acting on the
  active tab's agent, disabled when that tab has no agent.
- **Workspace ▸ Restart agent** — `Ctrl+Shift+R`.

Both go through the same `QAction`, so neither can drift from the other.

## 6. Light mode

### The choice

Settings ▸ Appearance: **Follow system** (default) / **Light** / **Dark**,
stored in `state.json` beside the other window state. "Follow system" reads
`QStyleHints::colorScheme()` and re-applies when Windows flips at dusk.

### How it reaches the widgets

One function applies it — `theme::apply(app, choice)` — building a light or a
dark `QPalette` and setting it on the application. `Theme.h` is already written
for this: `isDark`, `band`, `muted` and `ink` all derive from the live palette
rather than from a constant, and the four accents are chosen to read on either.

What has to be audited is every widget that reads a palette-derived colour
**once, at construction**, and never hears that it changed. Known: the
`PermissionBar`'s amber wash, the `TranscriptView`'s error banner, the
`ExplorerDock` header's band, `GroupBar`, and every `codeview::wash` caller.
`TerminalWidget` already answers `QEvent::PaletteChange`; the pattern it uses —
recompute in a `applyAppearance()` called from both the constructor and
`changeEvent` — is the one the others adopt.

`crates/ide/src/highlight/theme.rs` hard-codes one set of hex values for syntax
highlighting, chosen for a dark background. It grows a light variant and a way
for the editor to say which one it wants; the dark values do not change.

## 7. Docking, and the divider that comes with it

### The shape

Visual Studio's: the documents are the fixed centre, the tool windows dock
around them.

```
+-- File Edit View Workspace Run Window Help -----------+
| Explorer |  editor tabs          | Agent             |
|  (dock)  |  (central, fixed)     |  (dock)           |
+----------+-----------------------+-------------------+
| Output: Run | Terminal   (dock)                      |
+-------------------------------------------------------+
```

The agent pane becomes a `QDockWidget` on the right. Explorer stays left, Output
stays bottom, the editor stays the central widget. All three docks are movable,
floatable, tabbable and closable.

This answers the first request too — *"the dividing line between the file view
and the agent output should be clearer"*. A dock separator is a real, grabbable,
themed edge where a 1px `QSplitter` handle was; it is widened and painted with
`theme::band`, and the agent dock's title bar is itself a boundary the eye
finds.

### What it costs

`m_centerSplitter` goes. Two things read it and must be re-expressed:

- **View ▸ Swap editor and agent** becomes a re-dock of the Agent dock to the
  left area — the same intent, one call instead of an `insertWidget` and a
  size fix-up.
- **`noteSplitterState` / `splitter_sizes` in the session file** is replaced by
  `QMainWindow::saveState`, which the window already calls for dock geometry.
  The saved state's version number is bumped so a restored splitter layout from
  an older session does not fight the new one; an unreadable or absent layout
  falls back to the default arrangement above.

### The Window menu

Checkable entries for **Explorer**, **Agent** and **Output** — Qt's own
`toggleViewAction()` for each, so the tick and the dock cannot disagree — and
**Reset layout**, which restores the default arrangement for a user who has
dragged themselves into a corner.

## 8. The two empty menus

Every action below already exists as a toolbar button, a panel button or a
right-click entry. The menus reuse the same `QAction` objects, so enablement is
written once.

**Workspace**

```
New Agent…                    Ctrl+N
Restart agent                 Ctrl+Shift+R
Next agent / Previous agent   Ctrl+PgDn / Ctrl+PgUp
---
Merge · Rebase · Squash… · Create PR… · Discard…
---
Destroy workspace… · Close group…
```

The five git actions come from the Changes tab's toolbar, which is invisible
unless that tab is open. Destroy and Close group are right-click-only today.

**Run**

```
Run                F5
Stop               Shift+F5
Restart run
---
<detected configurations, checkable, one group>
Open in browser
---
Clear output · Copy output · Allow blocked host…
```

The blocked-host queue is per workspace and lives in `RunPanelModel`; the menu
entry answers the same queue the toast does, for the workspace the panel is
showing, and for no other.

## 9. The cursive lines become optional

Settings ▸ Agents: **Show turn cost and system lines**, on by default. Off
hides the `turn 3 · $0.0042 · 1.2 s` result lines and the italic system lines
in every open transcript, live — they are already the only two kinds that go
through `applySmallGrey`. Stored in `state.json`; read by every
`TranscriptView` as it is built and applied to the ones already open.

## 10. A workspace names its repository

`bs/agent-4/work` is a branch name generated from the agent's own name. It is
shown in four places and says nothing the tab does not already say. It is
replaced by the repository and the branch the work forked from:

| Where | Was | Becomes |
|---|---|---|
| Tab label | `agent-4 · bs/agent-4/work` | `agent-4 · BondSymphonic @ main` |
| Status bar | `branch: bs/agent-4/work` | `C:\git\BondSymphonic @ main` |
| Window title | `agent-4 — bs/agent-4/work — …` | `agent-4 — BondSymphonic @ main — …` |
| Explorer header | the generated branch | repository @ base branch |

The generated branch moves to the tooltip in each of the four. It is still what
gets merged, and a user who needs it in a `git` command needs it exactly.

## 11. The branch dropdown says it is working

`NewAgentDialog` opens with an empty Base branch combo and fills it when
`repo.inspect` answers, which on a cold or remote repository is seconds of a
dropdown that looks broken. While `m_inspectPending` is true the combo is
disabled and holds one item, `Loading branches…`, with a busy indicator beside
it; `m_inspectFailed` leaves `Could not read branches`. Create is already
disabled through both states.

## 12. Testing

| What | How |
|---|---|
| The permission bug | The instrumented reproduction above, then a regression test at whichever level the break is found. |
| Permission modes | A daemon unit test that `PERMISSION_MODES` is exactly the CLI's list and that `default` is rejected; a migration test that a stored `default` reads back as `manual`. |
| Composer dropdowns | Widget checks: the two combos exist, carry the four models and four modes, and a change produces a restart with the right options JSON. |
| Auto-start and welcome | A smoke-test assertion that a created workspace issues `agent.start` with nobody pressing anything; a widget check that an empty transcript shows the welcome frame and a non-empty one does not. |
| Light mode | A model test that the choice round-trips through `state.json`; a widget check that a palette change repaints a band rather than leaving the dark one. |
| Docking and menus | Widget checks that each dock's `toggleViewAction` hides and restores it, and that Workspace and Run are non-empty with their actions enabled for an active workspace and disabled without one. Then driving the real app: a dock dragged, floated, closed and brought back from the Window menu. |
| Base path, spinner, cursive toggle | Widget checks on the four labels, on the combo through all three states, and on a transcript with the setting off. |

Two of these have already been shown to need the running app rather than the
offscreen harness — the permission bar and anything involving a real drag — so
both end with the IDE launched and driven.

## 13. Order of work

1. **Permissions** (§2, and the mode list in §3). Alone, first: everything
   after it is tested by driving an agent.
2. **Theme** (§6) and **base path + spinner** (§10, §11) in parallel — disjoint
   files.
3. **Docking and menus** (§7, §8) — `MainWindow.cpp` throughout, so it does not
   share a window with anything else that touches it.
4. **The agent pane** (§3 dropdowns, §4, §5, §9) — `TranscriptView`,
   `AgentArea`, `TranscriptModel`, `SettingsDialog`.

Steps 3 and 4 both add menu actions that call into the agent pane, so 3 lands
the actions and 4 fills them in.
