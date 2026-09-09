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
    SetupPage.{h,cpp}       shown when daemon/prereqs missing, with fix commands
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
4. Connect TCP to `127.0.0.1:port`, send `hello`. Call `check_prereqs`; if any
   item fails, show `SetupPage` with the fix hints but keep the window usable.
5. On process exit or connection loss: state `reconnecting`, retry launch with
   backoff (1, 2, 4, 8 s, max 30 s), then re-sync (`workspace.list`, then
   `agent.history` for each open agent tab).

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
  is_error?, collapsed}`, `Result{cost, duration, turns}`, `System(text)`.
  `assistant_delta` events append to the last streaming Assistant item;
  `tool_result` attaches to the matching `ToolUse` by id.
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
- Input box: multi-line, Enter sends, Shift+Enter newline. Disabled while the
  agent is working, except an Interrupt button.
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

## 11. Persistence

`%APPDATA%\BondSymphonic\` (or XDG/macOS equivalents via `directories` crate):
- `settings.json`: distro, daemon path, editor font/size, theme, default
  permission mode, `api_key_set: bool` (the key itself is stored in the Windows
  Credential Manager via the `keyring` crate and passed to the daemon at start).
- `state.json`: groups with ordered workspace ids, active group/tab, open editor
  tabs per workspace, splitter ratios, recent repos, and the base64 of
  `QMainWindow::saveState()` and geometry.

State is written on every structural change (debounced 500 ms) and on exit.
Workspaces are owned by the daemon; on start the IDE reconciles: workspaces in
`state.json` that the daemon no longer knows are dropped, and daemon workspaces
not in any group land in an "Unsorted" group.

## 12. Error presentation

- Per-workspace errors (sandbox failed, agent crashed, merge conflict) show as a
  red glyph on the agent tab and a dismissible banner at the top of that
  workspace's agent area, with the error message and, for `GitError`, the stderr
  in an expandable section.
- Daemon-level problems (disconnect, prereqs) show in the status bar and, when
  blocking, as the `SetupPage` replacing the central area.
- **Logins happen inside the IDE, never via a terminal command the user must
  type.** The `SetupPage` lists each failing prerequisite with an action button.
  For `claude_auth` the button is "Log in to Claude Code": it opens a terminal
  pane (a PTY in the distro, via `pty.open`) running `claude` so the user
  completes the OAuth flow there; the IDE watches the PTY output for the login
  URL and opens it in the system browser automatically, so the user only has
  to approve in the browser and paste the code if asked. The same pattern
  serves `gh_auth` with `gh auth login`. Missing tools (`bwrap`, `claude`,
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
  block to bound widget count.
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

## 14. Testing (IDE-specific)

- `model/` unit tests: transcript delta coalescing and tool-result matching; diff
  row alignment on fixtures; editor incremental edits keep tree-sitter spans
  consistent with full reparse; terminal grid after fixture byte streams; app
  state reconciliation with a daemon workspace list.
- `client/` tests: codec framing, request/response correlation, event routing,
  reconnect backoff, against an in-process fake daemon (a tokio TCP server
  implementing the proto types).
- `smoke.rs`: start the fake daemon, launch the app with
  `QT_QPA_PLATFORM=offscreen`, create a group and workspace, open a file, feed a
  recorded transcript, assert widget counts and the status bar text. Runs in
  `cargo test` on Windows when Qt is present; skipped with a message otherwise.
