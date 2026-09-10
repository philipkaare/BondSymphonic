# BondSymphonic IDE — Design

**Date:** 2026-09-08
**Status:** Approved for planning
**Parent:** `2026-09-08-bondsymphonic-overview-design.md`

## 1. Role

`bondsymphonic-ide` is the desktop application. It renders the classic IDE layout,
edits files, shows diffs, renders agent transcripts and terminals, and talks to
the daemon. It performs no git, sandbox, or process management itself. It is
Rust with a thin C++ Qt Widgets shell bridged by cxx-qt.

## 2. Layout

```
+------------------------------------------------------------------------------+
| File  Edit  View  Workspace  Run  Help                                       |
+------------------------------------------------------------------------------+
| [Group: Frontend] [Group: API] [+]                                           |  group tabs
| ( * agent-1  main-repo ) ( o agent-2  main-repo ) ( ! agent-3  other ) [+]   |  agent tabs w/ status
+---------------+------------------------------------+-------------------------+
| EXPLORER      | src/App.tsx  x | README.md  x      | Agent: agent-1  claude  |
| [Files][Chg]  |------------------------------------|-------------------------|
|  v src        |  1 import React from "react";      | > You: add a login page |
|    App.tsx  M |  2 ...                             | Claude: I'll start by...|
|    index.tsx  |                                    | [Edit src/App.tsx   v]  |
|  package.json |                                    | [Bash npm test      v]  |
|               |                                    | ...                     |
|               |                                    |-------------------------|
|               |                                    | Allow Bash "npm test"?  |
|               |                                    |        [Allow] [Deny]   |
|               |                                    |-------------------------|
|               |                                    | [ type a message...   ] |
+---------------+------------------------------------+-------------------------+
| RUN   [web v] [Start] http://localhost:41873 [Open]  | TERMINAL               |
|  > VITE ready in 312 ms                              | $ ls                    |
+------------------------------------------------------------------------------+
| daemon: connected | sandbox: up | bs/agent-1/work | $0.42                     |
+------------------------------------------------------------------------------+
```

- **Group tabs** (top row): free-form named groups. Right-click: rename, close
  group (asks per workspace: keep, merge, discard).
- **Agent tabs** (second row): one per workspace in the group. Status glyph and
  colour: idle (grey), working (blue, animated), waiting for permission (amber),
  error (red), exited/done (green check). Tooltip shows repo, branch, adapter.
  "+" opens the New Agent dialog.
- **Explorer dock** (left): "Files" tree of the active workspace's worktree with
  git status colouring; "Changes" list of files changed vs base with +/- counts.
  Double-click opens in the editor (Files) or in the diff view (Changes). Changes
  has Merge, Rebase, Squash, Create PR, Discard buttons in its toolbar.
- **Center splitter**: editor area left, agent area right by default. View menu:
  "Swap editor and agent". The splitter ratio persists.
- **Editor area**: tabbed documents; each tab is an editor or a diff view.
  Modified-indicator, Ctrl+S saves through the daemon, external change prompts
  reload.
- **Agent area**: a stacked widget, one page per workspace: for Claude agents a
  transcript, permission bar, and input box; for terminal agents a terminal.
- **Bottom dock**: "Run" panel and "Terminal" (shell in the workspace sandbox).
  Both follow the active agent tab.
- **Status bar**: daemon state, sandbox state, branch, session cost (Claude),
  and a prerequisites warning icon when `check_prereqs` reports problems.

Docks can be moved/hidden; layout is restored on start. The window starts at
1400x900 if no saved geometry.

## 3. Code structure

```
crates/ide/
  build.rs                  cxx-qt-build: compiles cpp/, links Qt6 Widgets
  Cargo.toml
  src/
    main.rs                 starts tokio runtime thread, calls cpp::run_app()
    model/                  PURE RUST, no Qt, fully unit-tested
      app_state.rs          groups, agent tabs, active selection, persistence structs
      transcript.rs         ordered transcript items, delta coalescing
      editor_buffer.rs      text buffer, language detection, tree-sitter highlight spans
      diff.rs               side-by-side row alignment via `similar`
      terminal_grid.rs      wraps alacritty_terminal Term; cells for painting
      run_config.rs         run selection state per workspace
    client/
      mod.rs                DaemonClient: request/response futures, event stream
      launcher.rs           starts daemon (wsl.exe on Windows, direct elsewhere), installs binary
      codec.rs              NDJSON framing
    qobjects/               cxx-qt bridges (Rust QObjects visible to C++)
      app_controller.rs     connection state, open repo, create/destroy workspace, prereqs
      group_model.rs        groups + agent tabs, signals on change
      file_tree.rs          lazy directory listing per workspace
      changes_model.rs      changed-files list, merge/pr/discard invokables
      editor_document.rs    one per open file: text, dirty, save, highlight spans
      diff_document.rs      one per open diff: aligned rows
      transcript_model.rs   one per Claude agent: items, permission state, send/reply
      terminal_session.rs   one per PTY: cells, write, resize
      run_panel.rs          configs, start/stop, url, output lines
      settings.rs           distro, daemon path, editor font, api key presence
    highlight/
      languages.rs          tree-sitter grammar registry + file-extension mapping
      theme.rs              style ids -> colours (light + dark)
  cpp/
    main.cpp                QApplication, style, MainWindow
    MainWindow.{h,cpp}      docks, splitter, menus, status bar, persistence hooks
    GroupBar.{h,cpp}        two QTabBars (groups, agents) with status glyphs
    ExplorerDock.{h,cpp}    Files tree (QTreeView) + Changes list
    EditorArea.{h,cpp}      QTabWidget of EditorWidget / DiffWidget
    EditorWidget.{h,cpp}    QPlainTextEdit + line numbers + RustHighlighter
    DiffWidget.{h,cpp}      two synced QPlainTextEdits with row colouring
    AgentArea.{h,cpp}       QStackedWidget of TranscriptView / TerminalWidget
    TranscriptView.{h,cpp}  scrollable message list, tool cards, permission bar, input
    TerminalWidget.{h,cpp}  paints cells from TerminalSession, forwards keys
    RunPanel.{h,cpp}        config combo, start/stop, url label, log
    NewAgentDialog.{h,cpp}  repo picker, base branch, name, adapter, run config
    SetupPage.{h,cpp}       prereq rows, fix commands, login terminal, sign-in link
                            (a section of SettingsDialog; it is never a page of its own)
    SettingsDialog.{h,cpp}  hosts SetupPage as its first section, then agent settings
  tests/
    model_tests.rs          pure model tests
    smoke.rs                offscreen launch against a mock daemon
```

Rule: `model/` and `client/` never import Qt. `qobjects/` are thin adapters that
own a model struct and expose properties, invokables, and signals. C++ files
contain layout and painting only; any conditional logic beyond "which widget to
show" belongs in Rust.

## 4. Threading and data flow

- `main.rs` starts a tokio runtime on a dedicated thread. `DaemonClient` lives
  there. All QObjects live on the Qt main thread.
- QObject invokables send requests by pushing to a `tokio::sync::mpsc` channel;
  the response future resolves on the tokio thread and posts the result back to
  the owning QObject with cxx-qt's `qt_thread().queue(...)`, which runs a closure
  on the Qt thread. Events use the same path via a single dispatcher that routes
  by `workspace_id`/`agent_id`/`pty_id` to the right QObject.
- No blocking calls on the Qt thread. Requests that take long (create workspace,
  merge) show a busy indicator on the relevant tab and stay cancellable via
  `workspace.destroy`.
- Every request has a deadline, and it is chosen from the method rather than
  passed in by the caller (`client::default_timeout_for`). The default is 30 s.
  `workspace.merge` gets 120 s and `workspace.create_pr` 180 s, because the
  daemon allows itself 60 s per git command and 120 s for `gh`: a pull request
  is a push plus a `gh pr create`, so a 30 s wait abandons a call that is
  succeeding — the branch reaches the remote, the pull request opens, the user
  is told it failed, and the retry that invites answers "a pull request for
  branch … already exists". `DaemonClient::with_request_timeout` overrides the
  per-method value for every method, which is how a test bounds a slow one.
- A merge, pull request, discard or destroy books its workspace into
  `AppController`'s in-flight set for as long as it is out. A second one on the
  same workspace is refused with a sentence rather than queued, and
  `isWorkspaceBusy(ws)` / `workspaceBusyChanged(ws, busy)` are what the views
  disable themselves from: the Changes toolbar's five actions, the tab context
  menu's Destroy, and the close-group dialog's rows. The set is on the
  controller and not on the toolbar because the toolbar is not the only caller —
  the context menu and `CloseGroupRunner` both go straight to the controller —
  and the pair that must never overlap is a merge and the destroy that deletes
  the objects it is still absorbing, which leaves the base branch pointing at
  commits whose parents are gone.

## 5. Daemon launcher (Windows)

1. Read settings: distro name (default `bondsymphonic`), daemon path inside WSL
   (default `~/.bondsymphonic/bin/bondsymphonic-daemon`).
2. Locate the Linux daemon binary on the Windows side: `target/daemon/` in dev
   builds, next to the exe in packaged builds. Compare its embedded version with
   `wsl.exe -d D -- ~/.bondsymphonic/bin/bondsymphonic-daemon --version`; if it
   differs or is missing, copy via `wsl.exe -d D -- sh -c "mkdir -p ... && cp
   /mnt/<drive>/... ... && chmod +x ..."`.
3. Spawn `wsl.exe -d D --exec <daemon> --log-level info` with piped stdin/stdout/
   stderr. Parse the first stdout line for port and token. Keep stdin open for the
   daemon's lifetime (closing it is the shutdown signal).
4. Connect TCP to `127.0.0.1:port`, send `hello`. Call `check_prereqs`; if a
   *blocking* item fails, open the Settings dialog on its Setup section. The
   window itself stays usable: the setup page is a section of a dialog, not a
   page in front of the workbench.
5. On process exit or connection loss: state `reconnecting` ("daemon:
   reconnecting (attempt N)"), retry launch with backoff (1, 2, 4, 8, 16 s, max
   30 s, unbounded attempts), then re-sync (`check_prereqs`, then
   `workspace.list`, then `agent.history` for each open agent tab). The dead
   client is dropped as soon as the loss is seen, so an action in the gap fails
   fast with a reason rather than hanging. Whichever comes first ends a
   connection: the event stream ending, or `DaemonProcess::wait_exit`
   resolving; the old child is reaped before a new daemon is launched. A loss
   after `prepareQuit()` (File > Exit, `closeEvent`) is not reconnected, it is
   `lost`. Each successful reconnect bumps the connection generation, and
   `reconnected(generation)` is emitted once the re-sync above has finished —
   whether it succeeded or not, since the window most wants to repaint its
   status bar on a failure; a client that needs to tell them apart watches
   `operationFailed` for `system.check_prereqs`/`workspace.list`.

   The backoff schedule resets to one second only after a connection that
   **held for at least five seconds**. A daemon that answers `hello` and dies a
   moment later is a crash loop, not a recovery: resetting on every successful
   handshake would relaunch it every one to two seconds for as long as the
   window is open, with the status bar frozen on "attempt 1". An ordinary
   restart still costs one second, because the connection it replaced had been
   up for minutes.
   Every subscriber re-attaches on that generation: transcripts re-`attach`
   and replay (the restored agent reads back `exited`, so the pane offers
   Restart with `restartOptionsJson()`, which carries `resume_session`), the
   Run panel refreshes, the Changes list and the file tree reload, and terminals
   mark themselves exited with `[daemon restarted]` and offer `reopen()` —
   a PTY cannot survive, since the sandbox it ran in is gone.

On Linux/macOS the launcher spawns the daemon binary directly. The launcher is
the only place with `cfg(windows)` branches in the IDE.

## 6. Editor

- `EditorDocument` holds the full text in Rust (`ropey` rope), the detected
  language, and a tree-sitter tree. C++ `QPlainTextEdit` is the view; on
  `textChanged` the widget sends the edit (`position, removed_len, inserted_text`)
  to Rust, which applies it to the rope and tree incrementally.
- `RustHighlighter : QSyntaxHighlighter` asks `EditorDocument::spans_for_line(n)`
  and applies formats from a fixed palette of style ids (keyword, string, comment,
  function, type, number, constant, operator, punctuation, attribute, tag,
  property). Theme colours come from `highlight/theme.rs`, light and dark.
- Languages in v1: Rust, JavaScript, TypeScript, TSX, Python, JSON, TOML, YAML,
  HTML, CSS, Markdown, Bash, C, C++, Go. Unknown extensions get no highlighting.
- Save: Ctrl+S → `fs.write_file`. If a `fs.changed` event arrives for an open,
  unmodified file, it reloads silently; if modified, a bar offers reload/keep.
- Files over 4 MiB or binary open as a read-only notice.
- **As built (Milestone 3):** highlighting re-runs over the *whole* buffer after
  an edit, lazily on the next span query, rather than applying the edit to the
  tree-sitter tree incrementally. That is why `EditorBuffer::set_highlighting`
  exists and why a document over 512 KiB (`HIGHLIGHT_MAX_BYTES`) opens with
  highlighting switched off: a full re-pass per keystroke does not scale past
  that. The incremental form described above is deferred until profiling asks
  for it. Injections are also not wired up — `highlight()` is called with an
  injection callback that always answers `None` — so a `<script>` block in HTML
  and a fenced code block in Markdown are not highlighted as their own language.
- Line numbers, current-line highlight, monospace font from settings, tab width
  4, no autocomplete, no folding in v1.

## 7. Diff view

`DiffDocument` calls `workspace.diff`, runs `similar::TextDiff::from_lines`, and
produces aligned rows `{left_no?, left_text, right_no?, right_text, kind:
Equal|Insert|Delete|Replace}`. `DiffWidget` shows two read-only editors with
synchronised scrolling and row backgrounds (green/red/yellow tint), plus a header
with the file path and +/- counts. Syntax highlighting is applied on both sides
using the same `RustHighlighter`.

## 8. Transcript view (Claude agents)

- `TranscriptModel` keeps `Vec<TranscriptItem>`: `User(text)`,
  `Assistant(text, streaming: bool)`, `ToolUse{id, name, input_json, result?,
  is_error?, collapsed}`, `Result{cost, duration, turns}`, `System(text)` and
  `Earlier{count, text}`. `assistant_delta` events append to the last streaming
  Assistant item; `tool_result` attaches to the matching `ToolUse` by id.
  `Earlier` is the fold described in §13: at most one, always first, and only
  ever produced by the cap.
- `TranscriptView` is a `QScrollArea` with a vertical layout of message frames.
  Assistant text is rendered as Markdown-lite (code fences, inline code, bold,
  lists) via Qt's Markdown support in `QLabel`/`QTextDocument`. Tool cards show
  the tool name and a one-line summary (e.g. file path for Edit, command for
  Bash) with an expand toggle for full input and output. Auto-scroll sticks to
  the bottom unless the user scrolled up.
- Permission bar appears above the input when state is `waiting_permission`:
  tool name, summary, Allow / Deny buttons, and an "Always allow this tool for
  this session" checkbox (implemented by the IDE auto-answering subsequent
  requests for the same tool name).
- The bar is on the workspace's own pane, so a request raised by an agent in a
  tab that is **not** in front would otherwise be invisible until the user
  happened to switch to it. Milestone 7: the window marks that tab instead --
  `GroupModel::setWorkspaceAttention(ws, text)` puts a bullet after its label
  and the sentence "<agent> is waiting for permission" in the status bar and in
  the tab's tooltip; `clearWorkspaceAttention(ws)` takes both down. It is driven
  by `agent.state` rather than by the bar, because a tab the user has never
  opened has no pane at all, and it is cleared when the agent leaves
  `waiting_permission` (which is what a reply reaching the daemon causes) or
  when the user selects that tab.
- `agent.state` is not the only driver: the window also calls
  `GroupModel::refreshAttention(previousWorkspaceId)` on every tab change. That
  clears the tab just selected **and marks the tab just left** when its agent is
  still in `waiting_permission`. Without it, an agent that asked while its own
  tab was in front stays unmarked after the user switches away, because no state
  event fires for a change of selection. The agent's own status is the source
  either way, so the bullet cannot disagree with the tab's status glyph.
- Input box: multi-line, Enter sends, Shift+Enter newline. Disabled while the
  agent is working, except an Interrupt button.
- **The composer is gated on `AppController::claudeLoggedIn`**, a bool property
  meaning "a Claude agent can answer a prompt". `claude_logged_in` in
  `app_controller.rs` is `claude_auth ok || api_key_set()`: **both** credentials
  count, because the daemon's `claude_auth` answers from `claude auth status`,
  the daemon user's credentials file and the daemon's own environment, and knows
  nothing about the Anthropic API key this IDE keeps in the Windows credential
  store and merges into `AgentStartOptions.api_key`. A gate on the prerequisite
  alone would shut a key-only user out of every composer while their agents ran,
  and send them to a dialog whose next section tells them the key is what to use
  instead of a login. An absent `claude_auth` item counts as not logged in.
  While the property is false, `TranscriptView` hides the whole composer widget
  and shows a "Log in to Claude Code…" button in its place, which the window
  turns into Settings > Setup. An agent runs `claude -p` and `-p` mode cannot
  log in, so a composer offered with neither credential can only lose what the
  user typed. The property is recomputed on every prerequisite check and again
  after `setApiKey`/`clearApiKey`, so the composer returns without an IDE
  restart either way. `AgentArea` holds the current value and applies it to
  every transcript pane, including ones built later; terminal panes and the
  permission bar are untouched.
- On tab open, `agent.history` replays into the model before live events are
  applied (events received during replay are buffered).

## 9. Terminal widget

- `TerminalSession` wraps `alacritty_terminal::Term` with a `vte` parser; PTY
  output bytes feed the parser; the grid is exposed as rows of `Cell {ch, fg,
  bg, flags}` for the visible region plus scrollback offset.
- `TerminalWidget` paints with `QPainter` on a monospace font, handles key events
  (mapping Qt keys to VT sequences for arrows, function keys, Ctrl combos),
  selection and copy, mouse-wheel scrollback, and resize → `pty.resize`.
- One `TerminalSession` per shell tab and per terminal-adapter agent.

## 10. New Agent dialog

Fields: repo path (folder picker, remembers recent repos; Windows paths are
converted to `/mnt/<drive>/...` for the daemon), base branch (combo populated by
`repo.inspect`), workspace name (default `agent-N`), adapter (Claude Code /
Terminal with command), run config (combo from `repo.detect_run_configs`, with
editable port when guessed), group (existing or new), optional initial prompt.
"Create" calls `workspace.create`, then `agent.start`, then sends the initial
prompt if any.

Three rules govern when it opens and what it will accept:

- **Inspected before shown.** `MainWindow::onNewAgent` asks `repo.inspect` about
  the repository the dialog will open on — the most recent one — under a wait
  cursor and a "Reading repository…" status line, and builds the dialog only
  when the answer (or its failure) arrives; the answer is replayed into the
  dialog through `applyInspection`. A failure still opens the dialog, with the
  reason on it. `repo.inspect` and `workspace.create` get a 120 s client-side
  wait (`client::REPOSITORY_REQUEST_TIMEOUT`): the 30 s default was shorter than
  an inspect of a large repository reached through `/mnt/c`.
- **The name is validated as it is typed.** `validate_workspace_name` in
  `app_controller.rs` is the daemon's own rule — non-empty, no whitespace, no
  `/`, no `..` — exposed as the `validateWorkspaceName` invokable. The dialog
  calls it on every edit, shows "Use a single word: letters, digits, - or _"
  in red under the field, and greys Create out. Create is also greyed out while
  the path in the box has not been inspected, while an inspection is out, and
  after one has failed.
- **A folder that is not a repository is explained.** `RepoInfo.is_repo == false`
  puts a line under the path — "This folder is not a git repository. It will be
  initialised with an empty first commit when the agent is created.", or "This
  folder does not exist; it will be created and initialised." when `exists` is
  false too — and makes `initIfMissing()` true, which the window passes to
  `createWorkspace*` as `WorkspaceCreateParams.init_if_missing`. Pressing Create
  is the consent; there is no checkbox.

## 11. Persistence

`%APPDATA%\BondSymphonic\` (or XDG/macOS equivalents via `directories` crate):
- `settings.json`: distro, daemon path, editor font/size, theme, default
  permission mode, `api_key_set: bool` (the key itself is stored in the Windows
  Credential Manager via the `keyring` crate and passed to the daemon at start).
- `state.json`: `version`, groups with ordered workspace ids, the active
  workspace, the open editor tabs per workspace and which of them was in front,
  the agent-area splitter sizes and whether its halves are swapped, recent repos
  (most recent first, capped at 10), the base64 of `QMainWindow::saveState()`
  and of `saveGeometry()`, and the per-start run port overrides keyed by
  workspace and configuration name.

Both files live in one flat `%APPDATA%\BondSymphonic\` directory. Earlier
builds nested a second `BondSymphonic` inside the first (the `ProjectDirs`
layout); a `settings.json` left there is copied up once, the first time the new
path has no file, and the old one is left where it is so an older build still
finds its settings. `BS_SETTINGS_PATH` and `BS_STATE_PATH` override the two
paths for tests, and naming the first also turns the migration off, so a test
can never read the developer's own settings.

State is written on every structural change (debounced 500 ms: a burst — a
splitter dragged, five tabs closed — is one write, 500 ms after it stops) and
again on exit, through a temporary file and a rename. A `state.json` that will
not parse is renamed `.corrupt` and read as defaults, since a layout is not
worth refusing to start over.

Workspaces are owned by the daemon; on start the IDE reconciles against the
first `workspace.list`: workspaces in `state.json` that the daemon no longer
knows are dropped — along with their editors and port overrides — and daemon
workspaces not in any group land in an "Unsorted" group. Groups keep their
persisted order, an empty group is kept (a user may be holding it open), and the
result is written straight back, which is what stops the file growing forever.
Group names are unique: a rename onto a name another group already has is
refused, because "Unsorted" is found by name and a second one would never
receive an unclaimed workspace.

**Agents are owned by the daemon too, and a tab is rebuilt from its list.**
`state.json` remembers which workspaces were in which group and nothing about
what was running in them. `WorkspaceInfo.agent_records` carries one
`AgentSummary` per agent the daemon has for that workspace — id, adapter, state,
session id, and the non-secret half of the options it was started with — oldest
first, ended agents included, in the same order as the bare ids in
`WorkspaceInfo.agents`. A daemon too old to send the records leaves them empty,
and the IDE then builds the terminal tab it always did.
`AgentTab::from_workspace_info` adopts the last of them, so a Claude
workspace comes back as a Claude tab whose transcript pane attaches to the same
agent id, replays `agent.history`, reads back `exited`, and offers Restart with
the model and permission mode the user originally chose. Without that the
records file, the transcripts and the resumable sessions were reachable only
while the IDE process itself survived: every Claude workspace returned as a
terminal tab bound to no agent. There is deliberately no field in
`AgentSummary` an API key could travel in; the key is merged in at
`agent.start` and never comes back.

## 12. Error presentation

- Per-workspace errors (sandbox failed, agent crashed, merge conflict) show as a
  red glyph on the agent tab and a dismissible banner at the top of that
  workspace's agent area, with the error message and, for `GitError`, the stderr
  in an expandable section.
- Daemon-level problems (disconnect, prereqs) show in the status bar and, when
  blocking, by opening the Settings dialog on its Setup section. The status bar's
  "Set up…" link goes to the same place while any check is failing.
- **Setup lives in File > Settings…, not under Help.** `SettingsDialog` hosts
  the whole `SetupPage` — rows, fix buttons, login terminal and sign-in link —
  as its first section, and `MainWindow::showSetupPage` opens the dialog on it.
  A login is a setting the user comes back to when a token expires; Help is
  where they look for documentation. The dialog is modal: modality stops input
  reaching other windows but not the event loop, so the login terminal inside it
  works exactly as it did on the full-window page, and a single dialog instance
  means a prerequisite re-check cannot stack a second one over the first.
  Closing the dialog destroys the page and with it the terminal's session, which
  closes the PTY and skips `onTerminalExited`, so `openSettings` calls
  `recheckPrereqs()` after `exec` returns: a user who pastes the code and closes
  Settings before the CLI process exits must not be left with a stale
  `claude_auth: false` and no way back but the Re-check button. `SetupPage` has
  no heading and no "Continue anyway" of its own — what a blocking prerequisite
  gets instead is the dialog opening by itself, which the user closes when they
  choose to carry on regardless.
- **Logins happen inside the IDE, never via a terminal command the user must
  type.** The `SetupPage` lists each failing prerequisite with an action button.
  For `claude_auth` the button is "Log in to Claude Code": it opens a terminal
  pane (a PTY in the distro, via `pty.open`) running `claude` so the user
  completes the OAuth flow there; the IDE watches the PTY output for the login
  URL and opens it in the system browser automatically, so the user only has
  to approve in the browser and paste the code if asked. The same pattern
  serves `gh_auth` with `gh auth login`. `LINK_PREFIXES` in
  `terminal_session.rs` is the closed list of hosts a browser will be opened
  for: `https://claude.ai/`, `https://claude.com/` (what the CLI actually
  prints) and `https://github.com/login/device`. Under the terminal, a
  **Sign-in link** row appears when a URL is detected: the URL elided in the
  middle, **Copy** (with a transient "Link copied") and **Open in browser**,
  and clicking the URL itself does both. The automatic open stays; the row is
  for the case it cannot be verified — no default browser, or a sign-in that has
  to finish on another machine. The row goes down when the terminal exits,
  because the URL it carried is spent. Missing tools (`bwrap`, `claude`,
  `gh`) get an "Install" button that runs the `fix_hint` command in the same
  terminal pane. After the pane's process exits, the IDE re-runs
  `check_prereqs`; when everything passes the page dismisses itself. Settings
  also offers an API-key field (stored in the OS credential store) as the
  alternative to OAuth for Claude Code.
- Network denials (`daemon.log warn` with `host`) show a small toast in the run
  panel with an "Allow host" action that calls `workspace.set_allowlist`.

## 13. Footprint measures

- Qt Widgets only; no QtQuick, QtWebEngine, or QtNetwork modules linked.
- Transcript items above 2,000 per agent are collapsed into a "load earlier"
  block to bound widget count. **Implemented in Milestone 7**
  (`model::transcript::MAX_LIVE_ITEMS`): the oldest items move into the
  transcript's own `earlier` list and a single `TranscriptItem::Earlier` block
  labelled "Load earlier (N)" takes their place at the head, so the view never
  holds more than 2,001 frames however long the session runs. Nothing is
  discarded: clicking the block calls `TranscriptModel::expandEarlier`, which
  puts every item back and stops folding for the rest of that tab's life -- a
  user who asked to read the whole conversation must not have it taken away
  again while they are reading. A fold during a live message reports
  `Applied::Reset` rather than `Appended(n)`, because every index the view is
  holding has moved. `model_tests.rs` pins the cap by feeding 2,500 messages
  through and asserting the live count.
- Terminal scrollback 10,000 lines per session.
- Highlight trees are dropped for editor tabs not visible for 5 minutes and
  rebuilt on focus.
- The `model_tests` suite includes a benchmark-style test that loads a 50k-line
  file and asserts highlight of a single line stays under 5 ms.

**Measured, Milestone 2b (2026-09-09).** Debug IDE build on Windows 11 against the
daemon in the `bondsymphonic` WSL2 distro. Two workspaces open on one repository,
four live PTYs (each workspace's agent pane, plus a second shell per workspace
standing in for the bottom Terminal tab, which opens its PTY only once that tab is
shown), left idle for 60 s:

| | measured | target |
|---|---|---|
| `bondsymphonic-ide.exe` working set | 55.6 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 21.9 MB | — |
| `bondsymphonic-daemon` RSS | 9.8 MB | < 30 MB |

The daemon figure is the daemon process alone, as the target says. The sandboxed
processes it supervises (`bwrap`, its init helper and the PTY shells) accounted for
roughly 35 MB more inside the distro. Both numbers are comfortably inside the
targets, so no profiling pass was needed.

**Measured, Milestone 3 (2026-09-09).** The same machine and the same debug build
against the real daemon in WSL. The 2b scenario plus three open editor tabs
(`src/main.rs`, `README.md`, `src/lib.rs`) and one open diff, left idle for 60 s:

| | measured | 2b | target |
|---|---|---|---|
| `bondsymphonic-ide.exe` working set | 108.9 MB | 55.6 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 56.6 MB | 21.9 MB | — |
| `bondsymphonic-daemon` RSS | 9.2 MB | 9.8 MB | < 30 MB |

Both are inside the targets, but the IDE grew by 53 MB, so the growth was traced
before it was accepted. It is **not** the fifteen `HighlightConfiguration`s being
built eagerly. Each is behind its own `OnceLock` in `highlight::languages`, and
the same build measured with the 2b scenario exactly — two workspaces, four PTYs,
no editor or diff tab, so no configuration built at all — already holds 100.6 MB
working set and 54.4 MB private. Opening three files and a diff, which does build
the Rust and Markdown configurations, adds the remaining 8.3 MB.

The remaining 45 MB is carried by the M3 build before any file is opened, and the
fifteen tree-sitter grammars are *linked in* whether or not a configuration is
built from them: the debug executable is 24.6 MB. A release build and pruning the
grammar list are the levers if this ever approaches the target; at 109 MB against
150 MB it does not, so no profiling pass was made.

**Measured, Milestone 4 (2026-09-09).** The same machine and the same debug build
against the real daemon in WSL, with a fake `claude` in the workspace sandbox. One
Claude workspace whose transcript holds **198 items** — a permission turn plus 194
assistant paragraphs, replayed from `agent.history` after a detach/re-attach, not
merely streamed — the bottom Terminal tab's shell PTY in that workspace, three
open editor tabs and one open diff; plus a second workspace the daemon restored at
startup, whose sandbox is up but which has no pane open. Left idle for 75 s:

| | measured | M3 | target |
|---|---|---|---|
| `bondsymphonic-ide.exe` working set | 119.0 MB | 108.9 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 59.5 MB | 56.6 MB | — |
| `bondsymphonic-daemon` RSS | 6.8 MB | 9.2 MB | < 30 MB |

The daemon is *smaller* than in M3 because this scenario runs one PTY rather than
four; the agent it supervises costs nothing to the daemon process itself. Inside
the distro the supervised tree — four `bwrap`, two sandbox-init helpers, the
sandboxed shell and the agent process — came to about 31 MB, of which the agent
(here a python stand-in) was 11 MB.

The IDE is 10 MB over M3, but the two scenarios are not a controlled comparison:
this one adds 198 transcript frames and drops three PTYs with their 10,000-line
scrollbacks, so the transcript's own share is somewhere above 10 MB. What the
number does settle is that a long transcript is affordable at this size.

The "2,000 items per agent" collapse in the list above is **not implemented**: a
transcript grows one frame per item without bound today. A 2,000-item transcript
would therefore be the first thing to push this measurement towards the target,
and the collapse is the fix when it does.

**Measured, Milestone 5 (2026-09-10).** The same machine and the same debug build
against the real daemon in WSL. Two workspaces created for the measurement over
one throwaway repository — the first running `python3 -m http.server 8000` from
its `bondsymphonic.toml`, `ready`, with its port bridge up and its URL already
fetched once from Windows; the second idle with no run — plus the workspace the
daemon restored at startup, so three sandboxes were alive. No editor tab, no
diff, no agent. Left idle for 60 s after the run reached `ready`:

| | measured | M4 | target |
|---|---|---|---|
| `bondsymphonic-ide.exe` working set | 129.3 MB | 119.0 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 69.2 MB | 59.5 MB | — |
| `bondsymphonic-daemon` RSS | 9.7 MB | 6.8 MB | < 30 MB |

Both targets hold. The daemon is 2.9 MB above M4 for the obvious reason: this
scenario has three sandboxes rather than two, each with a proxy listener of its
own, and one of them also carries a bridge, a forwarder and a run supervisor.
The per-workspace cost of the whole of Milestone 5 is therefore around a
megabyte, which is what the design intends — the proxy and the bridge copy bytes
between sockets and buffer nothing but one request head.

Inside the distro the supervised tree came to roughly 90 MB across the three
sandboxes: 39.3 MB in the seven `sandbox-init` helpers, 19.4 MB in the Python
web server the run started, and the rest in `bwrap` and the shells. The web
server is the single largest item and belongs to the user's own process, not to
BondSymphonic.

The IDE's 10 MB over M4 is not a controlled comparison either — M4 held a
198-item transcript this run does not, and this run adds a Run panel with a live
log — so the useful reading is only that a bridged run with its output streaming
into the panel costs nothing that shows against a 150 MB target. The run log is
capped at 2,000 lines per run in Rust and the same in the widget, so a run that
prints for hours cannot move this number.

**Measured, Milestone 6 (2026-09-10).** The same machine and the same debug
build against the real daemon in WSL, **after a full daemon restart**: the
daemon was killed with `pkill -f bondsymphonic-daemon` and the IDE relaunched
and reconnected it by itself. Two workspaces over a throwaway repository — one a
terminal tab with `README.md` open in an editor, one a Claude tab whose
transcript was replayed from `agent.history` after the restart and which shows
the "agent ended when the daemon restarted" banner — plus the workspace the
daemon restored at startup, so three sandboxes were alive, and a run started
again after the reconnect (`python3 -m http.server`, bridged to a host port):

| | measured | M5 | target |
|---|---|---|---|
| `bondsymphonic-ide.exe` working set | 132.1 MB | 129.3 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 67.9 MB | 69.2 MB | — |
| `bondsymphonic-daemon` RSS | 20.7 MB | 9.7 MB | < 30 MB |

Both targets hold, and the IDE is where M5 left it: a reconnect cycle costs it
nothing that shows, which is the thing this milestone had to prove. A second run
of the same scenario measured 130.9 MB and 67.9 MB, so the working-set figure is
repeatable to about a megabyte.

**Read the timing before comparing.** Unlike the M2b–M5 rows, this one was *not*
taken after 60 s of idling: it was taken about five seconds after the run came
up, which is the moment the scenario the milestone is about actually exists.
Treat it as an upper bound of that moment rather than as a settled idle figure.

The daemon at 20.7 MB is 11 MB above M5 and is the one number worth explaining.
It is a daemon that has just restarted: it read its registry and its agent
records back (`restored agents from the records file restored=1 closed=1`), then
restarted a sandbox for each of the three workspaces while answering the IDE's
re-sync — allocation the M5 measurement, taken on a daemon that had been up and
idle, never did. It is still a third of the target, so it was recorded rather
than chased; if it ever matters, the measurement to take first is the same
daemon left idle for a minute after the restart, which this run did not do.

Inside the distro the supervised tree was about 100 MB: 19.7 MB in the Python
web server the run started, roughly 78 MB across the seven `sandbox-init`
helpers, and the rest in `bwrap` and the shells. The web server is the user's own
process, not BondSymphonic's.

**Measured, Milestone 7 (2026-09-10).** The first row taken on the **packaged
release build** — `dist\BondSymphonic\bondsymphonic-ide.exe` produced by
`scripts\package.ps1`, started with `BS_DAEMON_BINARY` unset so the launcher
installed its own daemon copy into the distro, and with the real Windows
platform plugin rather than offscreen, so it is comparable with M2b–M5. The
second row is the same scenario on `target\debug\bondsymphonic-ide.exe`, so the
release/debug difference can be read off directly and so M6's debug row still
has a debug successor.

The scenario, both rows: two workspaces over a throwaway repository — one a
Claude tab whose transcript replayed a 198-message stream from a fake `claude`
inside the workspace, one a terminal workspace with a live PTY — three open
editor tabs (`README.md`, `src/main.rs`, `src/lib.rs`), one open diff, and one
bridged run (`python3 -m http.server`), **plus** the workspace the daemon
restored at startup, so three sandboxes were alive. Left idle 60 s after the run
came up, and unlike M6's row this one *was* taken after that full idle.

| | release (packaged) | debug | M6 (debug) | target |
|---|---|---|---|---|
| `bondsymphonic-ide.exe` working set | 139.6 MB | 146.0 MB | 132.1 MB | < 150 MB |
| `bondsymphonic-ide.exe` private bytes | 78.7 MB | 78.7 MB | 67.9 MB | — |
| `bondsymphonic-daemon` RSS | 13.3 MB | 22.9 MB | 20.7 MB | < 30 MB |

Both targets hold, but the working set is the number to watch: **the debug build
is 4 MB under the 150 MB target**, and this is the first scenario that carries a
long transcript, three editors, a diff and a live run at the same time. The
release build buys about 6 MB of that back and is the one a user runs.

**Read the repeatability before drawing a trend.** Two samples ten seconds apart
within one session were identical to 0.1 MB, but an earlier run of the same
release scenario — on a package differing only by two smoke-script steps that
nothing invoked — measured 126.6 MB working set, 80.0 MB private and 11.3 MB
daemon. Working set therefore moves about 13 MB between runs while private bytes
hold to about 1.3 MB, so **private bytes are the figure to compare across
milestones** and the working-set column should be read as an upper bound of its
run rather than as a settled constant. The higher of the two runs is recorded
above.

The daemon is 13.3 MB against M6's 20.7 MB for the reason M6 gave for being high:
that row was taken seconds after a restart, while this one had been up and idle
for a minute. The debug row's 22.9 MB is the same daemon binary under a session
that had just destroyed two workspaces and created two more, which is the
allocation-churn case again.

Inside the distro the supervised tree came to roughly 58 MB across the three
sandboxes: 27.2 MB in `bwrap` and the `sandbox-init` helpers, 19.5 MB in the
Python web server the run started, and 10.9 MB in the Python stand-in for
`claude`. The last two are the user's own processes, not BondSymphonic's.

**Teardown.** After a quit through `closeEvent` the daemon is gone from the
distro within **3.4 s**, measured by polling once a second from the moment the
IDE process exited. `wsl -d bondsymphonic -- pgrep -f bondsymphonic-daemon` is a
sound check: it starts no shell at all, and `pgrep` never reports itself. So is a
lone `bash -lc "pgrep -f bondsymphonic-daemon"`, because a shell given a single
simple command `exec`s it in place rather than forking, so no process carrying
the pattern on its command line survives to be matched.

What does give a false positive is wrapping that in anything **compound** --
`pgrep -f bondsymphonic-daemon || echo EMPTY`, or the same with a trailing
`; echo $?`. The shell then has to stay alive to run the second half, its own
command line contains the pattern, and the check reports a daemon that is not
there. Reaching for `pgrep -x` to dodge that fails in the more dangerous
direction: the name is 20 characters and `comm` is truncated to 15
(`bondsymphonic-d`), so `-x` matches nothing, ever, and the check reports success
whether or not a daemon is running. `ps -eo args | grep '[b]ondsymphonic-daemon'`
is immune to both and is what to use when a compound command is unavoidable.

## 14. Testing (IDE-specific)

- `model/` unit tests: transcript delta coalescing and tool-result matching; diff
  row alignment on fixtures; editor incremental edits keep tree-sitter spans
  consistent with full reparse; terminal grid after fixture byte streams; app
  state reconciliation with a daemon workspace list.
- `client/` tests: codec framing, request/response correlation, event routing,
  reconnect backoff, against an in-process fake daemon (a tokio TCP server
  implementing the proto types).
- `smoke.rs`: start the fake daemon, launch the real binary with
  `QT_QPA_PLATFORM=offscreen`, and drive it through `BS_SMOKE_SCRIPT` —
  `create_claude,open_agent,send,allow,tree,open_file,open_diff,stop,create,detect,run_start,allow_host,run_stop,merge,merge,pr,open,close,reconnect,destroy,quit`.
  Two workspaces, because the two halves need different panes: the first is a
  Claude tab and carries one whole agent turn, the second a terminal tab that
  also carries the run and the network denial, and whose PTY the
  `close`/`destroy` pair tears down. Every step that touches the agent, the
  editor or the denial toast goes through the window rather than through the
  script's own client: `open_file` and `open_diff` emit
  `AppController::openFileRequested`/`openDiffRequested`, `allow` emits
  `permissionReplyRequested`, `send` emits `agentSendRequested`, `stop`
  emits `agentStopRequested`, `detect` calls `detectRunConfigs` (the invokable
  the New Agent dialog calls) and `allow_host` emits `allowHostRequested` — all
  signals a person's click produces. So the window builds the editor and diff
  tabs, answers the permission bar, and the prompt, the stop and the allow leave
  `TranscriptModel::send`/`::stop` and `RunPanelModel::allowHost`, which is what
  makes the "no failure warning" guards below able to fire at all. Only
  `create*`, `open_agent`, `run_start`, `run_stop`, `open`, `close`,
  `reconnect` and `destroy` are the script's own requests, because they stand in
  for a dialog or for a button this suite cannot press rather than for a click on
  a pane. `merge` and `pr` are through the window too: they call
  `AppController::mergeWorkspace` and `::createPr`, the invokables the Changes
  toolbar calls once its confirmation or its dialog has been answered. The
  run is the script's for that reason — the Start button is a widget and no test
  here touches the desktop — but everything the run provokes is the window's:
  the fake daemon reports `starting` → one output line → `ready` on a **bridged**
  host port that is not the configuration's own, and behind them a network
  denial for `example.com`, which the window has to turn into a toast on the
  workspace that raised it.

  The assertions are on the requests the fake daemon received, on the one
  permission reply it received, and on what the IDE logged.

  - `hello`, `workspace.create`, `agent.start`, `agent.history`, `agent.send`,
    `agent.permission_reply`, `fs.list_dir`, `fs.read_file`, `workspace.diff`,
    `agent.stop`, `workspace.merge`, `workspace.merge`, `workspace.create_pr`,
    `pty.open`, `pty.close`, `hello`, `workspace.list`, `workspace.destroy` in
    that order.
    `agent.history` is the window's own: only `TranscriptModel::attach` sends it,
    and the model attaches only because the window reacted to `agentStarted`, so
    its place before `agent.send` shows the pane was wired up before the turn.
  - The reply the daemon received is exactly one `allow` for the request the
    transcript was showing. The window routes `permissionReplyRequested` to the
    active pane only when that pane is attached to the named agent *and* has
    that request on its bar, and logs "permission reply not routed" otherwise —
    so a reply reaching the daemon at all is the proof the bar was up.
  - `fs.watch` and `workspace.changes` each seen after `workspace.create`; two
    `fs.list_dir` and two `pty.open`, since the window issues its own beside the
    script's.
  - No `agent.*` after `workspace.destroy`: a destroyed workspace's transcript
    detaches and stops nothing, because the daemon reaps its agents itself. No
    `pty.close`, `pty.resize` or `pty.write` after it either, because those PTYs
    have already exited.
  - `system.setup_pty` never asked for: every prerequisite the fake daemon
    reports passes, so Settings never opens on Setup and no login terminal
    opens. The fake daemon reports `git` and `claude_auth`, both ok; the second
    is part of the fixture rather than decoration, because it is what makes
    `claudeLoggedIn` true and so runs the scripted `send` against an open
    composer rather than against a pane showing the login button.
  - `repo.detect_run_configs`, `run.list`, `run.start`, `workspace.get`,
    `workspace.set_allowlist` and `run.stop` in that order, after `agent.stop`
    and before the script's `pty.open`. The first two are the Run panel's own,
    issued when the second workspace's tab appears and the panel is pointed at
    its worktree; the last four are the run and the answered toast.
    `workspace.get` **before** `workspace.set_allowlist` is part of the claim:
    `RunPanelModel::allowHost` reads the daemon's own list at the moment of the
    click and extends it, rather than sending back a cached or empty one.
  - The allowlist the daemon received is exactly the twelve defaults, in order,
    with `example.com` appended. A list that replaced them would silently
    un-allow every registry an agent needs and would still have satisfied the
    journal.
  - No failure warning logged for any of `fs.read_file`, `workspace.diff`,
    `workspace.changes`, `fs.watch`, `agent.history`, `agent.send`,
    `agent.permission_reply`, `agent.stop`, `repo.detect_run_configs`,
    `run.list`, `workspace.get` or `workspace.set_allowlist` — the journal shows
    a request arrived, these show its reply was accepted. Each of these warnings
    is logged by the IDE code that issued the request, so each is reachable only
    because the corresponding step travels the production path. `run.start` and
    `run.stop` are not among them: the script issues those on its own client,
    where a failure ends the step and leaves the process running, which the
    exit-status assertion catches instead.
  - No `allow host not routed`. The window answers `allowHostRequested` only
    when the Run panel is showing exactly the workspace named, and logs that
    line otherwise — so a `workspace.set_allowlist` reaching the daemon at all
    is the proof the toast was up on the right tab.
  - The two merges the daemon received are `merge` with no summary and then
    `squash` with one. The journal shows two `workspace.merge` calls arrived;
    this shows the toolbar's word crossed into the daemon's `MergeMode` for both,
    and that an empty summary box crossed as "not supplied" rather than as an
    empty string, which is what makes the daemon take the workspace's last commit
    subject. The fake answers the first as work that landed and the second as a
    conflict, so one run covers both shapes of `MergeResult` — both RPC
    successes, since a conflict is an answer and not an error.
  - The pull request the daemon received carries the title, the body and the
    draft flag that were sent.
  - Exactly **two** `hello`s. Nothing in the script connects to anything, so the
    second can only be the controller's reconnect loop noticing the socket had
    gone and rebuilding the connection by itself; `system.test_drop` in the
    journal is what asked the fake daemon to close it, and the `workspace.destroy`
    after the second `hello` is the proof the next step reached the daemon on the
    new connection. (`reconnect_tests.rs` covers the same loop in more detail:
    the status-bar text, every pane re-attaching, and no `pty.*` for a PTY the
    restarted daemon never had.)
  - The `state.json` at `BS_STATE_PATH` carries `version`, the surviving Claude
    workspace filed under its group, no trace of the destroyed one, and
    `README.md` as that workspace's open editor. Nothing in the script writes it:
    the window reports its arrangement through `noteGroups` and its editors
    through `noteEditors`, and the controller writes the file behind them, so
    this is the whole persistence path having run — across the reconnect, and
    with the developer's own `%APPDATA%` untouched.
  - No failure warning for `workspace.merge`, `workspace.create_pr`,
    `workspace.list` or `system.check_prereqs`, and none for the toolbar's
    `workspace.status`/`workspace.changes` summary pair. The last two are the
    re-sync's own, issued on the connection the controller has just built, so a
    warning there is a status bar saying "connected" over a client that could not
    be used.

  Runs in `cargo test` on Windows when Qt is present; skipped with a message
  otherwise.

- **Qt-less runs skip loudly (`require-qt`).** `smoke.rs`, `reconnect_tests.rs`
  and `restore_tests.rs` launch the IDE binary, so they need the Qt runtime on
  `PATH` and cannot run without it. Each begins with
  `bondsymphonic_ide::testing::skip_without_qt(<suite>)`, the one place that
  decision is made: with no Qt reachable it prints
  `SKIP: <suite>: neither QMAKE nor a qmake on PATH, ...` on **stderr**, where
  every other skip in the repository reports itself, and the test returns. A
  developer who has not dot-sourced `scripts\env.ps1` still gets a useful run of
  the pure-Rust suites and can see, with `--nocapture`, exactly which tests did
  nothing.

  "Reachable" is `QMAKE`, **or** a `qmake` on `PATH`, because `cxx-qt-build`
  accepts either. Probing only the variable would skip real coverage on a host
  that builds and runs the suite perfectly well but reached its Qt some other
  way -- and under `require-qt` would fail a job that should have been green.
  Finding `qmake` on `PATH` also means `<Qt>\bin` is on `PATH`, which is where
  the runtime libraries the launched binary needs actually live. The search is
  `testing::path_has_qmake`, which takes the `PATH` value rather than reading
  the environment so it can be tested: on Windows a host with no `qmake` has no
  Qt DLLs either, so the test binary would not load far enough to run it.

  The `require-qt` cargo feature turns that skip into a panic. CI's Windows job
  builds with it (`cargo test -p bondsymphonic-ide --features require-qt`, or
  `--features bondsymphonic-ide/require-qt` when the invocation names more than
  one package), so a build machine that has lost its Qt installation fails the
  job instead of reporting a green run in which every test that needs the
  runtime quietly passed without doing anything. The feature is off by default
  and adds no code to the shipped binary.

  `qobject_smoke.rs` and the other pure-Rust suites deliberately have no such
  guard: they exercise library functions that need no Qt runtime, and skipping
  them on a Qt-less machine would lose real coverage rather than protect it.
  (Every test binary links Qt because the crate does, so a machine that cannot
  load the Qt libraries at all fails to start them, which is a visible failure
  and not a silent pass.)
