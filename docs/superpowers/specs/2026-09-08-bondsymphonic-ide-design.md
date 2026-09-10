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
    reports passes, so the setup page never appears and no login terminal opens.
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
