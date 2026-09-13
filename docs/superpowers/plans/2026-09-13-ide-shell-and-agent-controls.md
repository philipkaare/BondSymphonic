# IDE Shell and Agent Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the permission requests that reach nobody, then give the agent pane its own model and permission controls, make the shell's panes dockable with menus that are not empty, and add a light palette.

**Architecture:** Four ordered groups over an existing Rust + Qt Widgets desktop app bridged by cxx-qt. Group 1 is a bug hunt in the daemon's Claude adapter and must land alone, because every later group is verified by driving a real agent. Group 2 is two independent cosmetic changes. Group 3 replaces the centre `QSplitter` with a `QDockWidget` layout and fills the menu bar. Group 4 rebuilds the agent pane's composer.

**Tech Stack:** Rust 2021, cxx-qt, Qt 6 Widgets (C++), `alacritty_terminal`, serde/serde_json, tokio (daemon), WSL2 distro `bondsymphonic`.

**Spec:** `docs/superpowers/specs/2026-09-13-ide-shell-and-agent-controls-design.md`

## Global Constraints

- **Claude CLI is pinned at 2.1.263** (`TESTED_CLAUDE_VERSION` in `crates/daemon/src/agents/claude.rs`). Its `--permission-mode` choices are exactly `acceptEdits`, `auto`, `bypassPermissions`, `manual`, `dontAsk`, `plan`. **`default` is not one of them.**
- **The permission mode is always sent.** No Claude agent starts with `permission_mode: None`. The floor is `manual`.
- **The IDE offers four modes**, in this order and with these words: `Ask every time` → `manual`, `Accept edits` → `acceptEdits`, `Plan only` → `plan`, `YOLO (sandboxed)` → `bypassPermissions`. `auto` and `dontAsk` are accepted by the daemon and never offered.
- **The models offered** are `Default (Claude Code decides)` → `""`, `Opus 5` → `claude-opus-5`, `Sonnet 5` → `claude-sonnet-5`, `Haiku 4.5` → `claude-haiku-4-5-20251001`. Every model combo is editable.
- **Widget tests** live at the foot of their own `.cpp` inside `#if defined(BS_WIDGET_TESTS)`, are `extern "C" std::int32_t bs_widget_test_<name>()` returning `0` for pass and a distinct small integer per failure, and are registered in `crates/ide/tests/qobject_smoke.rs` in **both** the `extern "C"` block and the `checks` array — whose declared length `[(&str, unsafe extern "C" fn() -> i32); N]` must be bumped by hand.
- **Comment style:** the codebase explains *why*, in prose, in complete sentences. Match it. Do not add comments that restate the code.
- **Every commit message ends with** `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>` on its own last line.
- **CI gates, all three must pass before a task is done:**
  - `cargo fmt --all -- --check`
  - `cargo test -p bondsymphonic-proto -p bondsymphonic-ide --features bondsymphonic-ide/require-qt`
  - `cargo clippy -p bondsymphonic-ide --all-targets --features bondsymphonic-ide/require-qt -- -D warnings`
  - Daemon tasks additionally: `cargo test -p bondsymphonic-daemon` and `cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings`
- **The IDE holds its own `.exe` open while running.** Close the running app before a rebuild, or `cargo build` fails with `Adgang nægtet` (access denied).
- **Bash heredocs eat one level of backslash.** When a string must contain `\n` or `\x1b`, write the file with the Write tool instead, or build the string with a variable.

---

## File Structure

**New files**

| File | Responsibility |
|---|---|
| `crates/ide/cpp/AgentChoices.h` | The one list of models and the one list of permission modes, plus the label↔id lookups. Header-only, like `Theme.h`. Read by `NewAgentDialog`, `TranscriptView` and `SettingsDialog`. |
| `crates/ide/cpp/AgentChoices.cpp` | The lookups' definitions and the `bs_widget_test_agent_choices_*` entries. |
| `docs/superpowers/plans/2026-09-13-ide-shell-and-agent-controls.md` | This plan. |

**Modified, by group**

| Group | Files |
|---|---|
| 1 — permissions | `crates/daemon/src/agents/claude.rs`, `crates/daemon/src/agents/claude_stream.rs`, `crates/ide/src/qobjects/settings.rs`, `crates/ide/tests/persistence_tests.rs` |
| 2 — readings | `crates/ide/cpp/Theme.h`, `crates/ide/cpp/app.cpp`, `crates/ide/cpp/SettingsDialog.{h,cpp}`, `crates/ide/cpp/PermissionBar.cpp`, `crates/ide/cpp/TranscriptView.cpp`, `crates/ide/cpp/ExplorerDock.{h,cpp}`, `crates/ide/cpp/GroupBar.cpp`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/cpp/NewAgentDialog.{h,cpp}`, `crates/ide/src/qobjects/settings.rs`, `crates/ide/src/qobjects/app_controller.rs` |
| 3 — shell | `crates/ide/cpp/MainWindow.{h,cpp}` only |
| 4 — agent pane | `crates/ide/cpp/TranscriptView.{h,cpp}`, `crates/ide/cpp/AgentArea.{h,cpp}`, `crates/ide/cpp/WorkspaceBanner.{h,cpp}`, `crates/ide/cpp/SettingsDialog.{h,cpp}`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/src/qobjects/transcript_model.rs` |

`MainWindow.cpp` is touched by three groups. Group 3 owns it outright; groups 2 and 4 make small, named edits to it. **Never run a Group 3 task in parallel with a Group 2 or Group 4 task.**

---

# Group 1 — Permissions

## Task 1: The daemon's mode list is the CLI's mode list

The allow-list exists so a bad mode is an `InvalidParams` on `agent.start` rather than a process that dies of a usage error a second later. It currently lets through `default`, which the pinned CLI rejects — so it is failing at the one job it has.

**Files:**
- Modify: `crates/daemon/src/agents/claude.rs:41-49` (`PERMISSION_MODES`)
- Modify: `crates/ide/src/qobjects/settings.rs:109` (`DEFAULT_PERMISSION_MODE`), and `try_load` at `:213`
- Test: `crates/daemon/src/agents/claude.rs` (the `#[cfg(test)]` module at the foot), `crates/ide/tests/persistence_tests.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `PERMISSION_MODES: [&str; 6]`; `settings::DEFAULT_PERMISSION_MODE == "manual"`; a `Settings::try_load` that rewrites a stored `"default"` to `"manual"`.

- [ ] **Step 1: Write the failing daemon test**

In the existing `#[cfg(test)] mod tests` in `crates/daemon/src/agents/claude.rs`, beside `claude_argv_and_probe_bin_pin_the_program_the_flags_and_the_permission_mode`:

```rust
/// The allow-list is the CLI's own list, not a superset of it. `default` was
/// in ours and has never been in the CLI's: `--permission-mode default` is a
/// usage error, and an agent that dies of one looks from the pane exactly like
/// an agent that hung.
#[test]
fn the_permission_modes_are_the_ones_the_cli_accepts() {
    let mut sorted = PERMISSION_MODES;
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            "acceptEdits",
            "auto",
            "bypassPermissions",
            "dontAsk",
            "manual",
            "plan"
        ]
    );

    let options = |mode: &str| AgentStartOptions {
        command: None,
        resume_session: None,
        model: None,
        permission_mode: Some(mode.to_owned()),
        api_key: None,
    };
    assert!(claude_argv(&options("default"), "linux_bwrap").is_err());
    assert!(claude_argv(&options("manual"), "linux_bwrap").is_ok());
    assert!(claude_argv(&options("bypassPermissions"), "linux_bwrap").is_ok());
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-daemon the_permission_modes_are_the_ones_the_cli_accepts`
Expected: FAIL — the array still has 7 entries and `default` is accepted.

- [ ] **Step 3: Shrink the list**

```rust
/// Values the CLI accepts for `--permission-mode`, verbatim from
/// `claude --help` at [`TESTED_CLAUDE_VERSION`]. Checked here so a bad one is
/// an `InvalidParams` on `agent.start` rather than a process that exits with a
/// usage error a second later -- which is what `default` used to do, from an
/// allow-list that was supposed to prevent exactly that.
const PERMISSION_MODES: [&str; 6] = [
    "manual",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p bondsymphonic-daemon the_permission_modes_are_the_ones_the_cli_accepts`
Expected: PASS. Then `cargo test -p bondsymphonic-daemon` — fix any other test that spelled `default`.

- [ ] **Step 5: Write the failing IDE migration test**

Append to `crates/ide/tests/persistence_tests.rs`, following the `ENV_LOCK` / `temp_dir` pattern already used there:

```rust
/// A settings file written before the mode list was corrected holds
/// `"default"`, which the pinned CLI rejects -- so every agent that user starts
/// dies of a usage error. Reading it back as `manual` is the only repair that
/// does not require them to find the setting that is poisoning their IDE.
#[test]
fn a_stored_default_permission_mode_reads_back_as_manual() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("permission-mode");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"default_permission_mode":"default"}"#).expect("settings");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    assert_eq!(Settings::load().default_permission_mode, "manual");

    // A mode the CLI does take is left exactly as it was.
    std::fs::write(&path, r#"{"default_permission_mode":"plan"}"#).expect("settings");
    assert_eq!(Settings::load().default_permission_mode, "plan");

    std::env::remove_var(SETTINGS_PATH_ENV);
}
```

- [ ] **Step 6: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt a_stored_default_permission_mode_reads_back_as_manual`
Expected: FAIL — `assertion failed: left == "default"`.

- [ ] **Step 7: Change the floor and migrate on read**

In `crates/ide/src/qobjects/settings.rs`:

```rust
/// What a new Claude agent starts on. `manual` -- every tool that would prompt,
/// prompts -- because the mode is always sent and the safe end of the range is
/// the one to default to.
const DEFAULT_PERMISSION_MODE: &str = "manual";

/// The mode the CLI dropped. Anything reading a settings file written before
/// the list was corrected finds this and must not pass it on: see
/// [`DEFAULT_PERMISSION_MODE`].
const RETIRED_PERMISSION_MODE: &str = "default";
```

and in `try_load`, in the `Ok(settings)` arm:

```rust
            Ok(mut settings) => {
                if settings.default_permission_mode == RETIRED_PERMISSION_MODE {
                    settings.default_permission_mode = DEFAULT_PERMISSION_MODE.to_owned();
                }
                Ok(settings)
            }
```

- [ ] **Step 8: Run the IDE suite**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt`
Expected: PASS, including the new test.

- [ ] **Step 9: Commit**

```bash
git add crates/daemon/src/agents/claude.rs crates/ide/src/qobjects/settings.rs crates/ide/tests/persistence_tests.rs
git commit -F - <<'EOF'
fix(daemon): the permission-mode allow-list is the CLI's own

`--permission-mode` at the pinned 2.1.263 takes acceptEdits, auto,
bypassPermissions, manual, dontAsk or plan. `default` was the first
entry in ours, and the list exists precisely so a mode the CLI will
reject is an InvalidParams on agent.start rather than a process that
dies of a usage error a second later -- which is what it has been
letting happen, and what an agent that hangs with nothing on screen
looks like from the pane.

The IDE's own floor moves to `manual` and a settings file holding the
retired word reads back as `manual`, because the alternative is an IDE
that refuses to start an agent until the user finds the setting that is
poisoning it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 2: Find out where a permission request actually stops

**This task produces evidence and a written finding, not a fix.** Do not change production behaviour in it. Task 1 removed one suspect; it may or may not have been the cause.

**Files:**
- Temporarily modify (reverted at the end of this task): `crates/daemon/src/agents/claude.rs`, `crates/daemon/src/agents/claude_stream.rs`, `crates/ide/src/qobjects/transcript_model.rs`, `crates/ide/cpp/TranscriptView.cpp`
- Create: `docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md`

**Interfaces:**
- Consumes: Task 1's corrected mode list.
- Produces: a finding document naming the first boundary that does not see the request, which Task 3 fixes.

- [ ] **Step 1: Confirm the CLI even asks**

`scripts/record-claude-stream.sh` already runs `claude` with exactly the daemon's argv in a throwaway repository. Run it with a prompt that forces an approval, from a login shell:

```bash
wsl -d bondsymphonic -- bash -lc 'bash /mnt/c/git/BondSymphonic/scripts/record-claude-stream.sh "run the shell command: echo hello"'
```

Then look for the request in what it recorded:

```bash
wsl -d bondsymphonic -- bash -lc 'grep -c can_use_tool ~/.../recorded-*.ndjson'
```

Write down: does a `{"type":"control_request", ... "subtype":"can_use_tool"}` line appear on stdout at all?

- **No line** → the CLI is not asking. The cause is in the argv or the mode. Re-run by hand with `--permission-mode manual` and with `--permission-prompts host` spelled out, one variable at a time, and record which spelling makes it ask. Skip to Step 5.
- **A line appears** → the CLI asks. Continue to Step 2.

- [ ] **Step 2: Instrument the four boundaries**

Add one `tracing` line at each, all under the same target so they can be filtered together. These are temporary.

In `crates/daemon/src/agents/claude_stream.rs`, in `parse_line`'s `"control_request"` arm, before the `return`:

```rust
                tracing::warn!(target: "permhunt", "parse_line made a PermissionRequest for {tool_name}");
```

In `crates/daemon/src/agents/claude.rs`, in the reader loop where `AgentMessageBody::PermissionRequest` is matched (around `:747`):

```rust
                                tracing::warn!(target: "permhunt", "reader recorded permission request {request_id}");
```

In `crates/ide/src/qobjects/transcript_model.rs`, immediately before `self.permission_requested();` (around `:632`):

```rust
            tracing::warn!(target: "permhunt", "model raising permission_requested");
```

In `crates/ide/cpp/TranscriptView.cpp`, at the top of `onPermissionRequested`:

```cpp
    qWarning("permhunt: TranscriptView asked to show the permission bar");
```

- [ ] **Step 3: Drive one approval through the real app**

```powershell
.\launch.ps1
```

Create a Claude agent, set its permission mode to `manual` if it is not already, and send a prompt that must run a tool: `run the shell command: echo hello`. Watch the daemon log and the IDE's stderr for `permhunt` lines.

- [ ] **Step 4: Record which line is the last one**

The last `permhunt` line printed names the last boundary that saw the request. The break is between it and the next.

| Last line seen | The break is in | Task 3 fixes |
|---|---|---|
| none at all | the CLI's argv or the mode | `claude_argv` |
| `parse_line made a…` | the reader loop's recording or publishing | `claude.rs` reader |
| `reader recorded…` | the event's journey to the IDE | the agent event path / `client` |
| `model raising…` | the Qt connection to the view | `TranscriptView`'s connect |
| `TranscriptView asked…` | the bar itself — it is shown but invisible | `PermissionBar` |

- [ ] **Step 5: Write the finding and revert the instrumentation**

Create `docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md` with: the exact reproduction, the `permhunt` output, the last boundary that saw the request, and the one-sentence root cause. Then revert every edit from Step 2 — `git checkout --` the four files, or remove the lines by hand if other work is in them.

- [ ] **Step 6: Commit the finding**

```bash
git add docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md
git commit -F - <<'EOF'
docs(daemon): where a permission request stops

Evidence, not a fix: one agent driven into one approval with every
boundary from the CLI's stdout to the amber bar instrumented, and the
last one that saw the request written down.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 3: Fix the break, at the level it lives

**Files:** decided by Task 2's finding; the table in Task 2 Step 4 names the file.
**Test:** a regression test at the same level — `crates/daemon/src/agents/claude_stream.rs`'s test module if it is parsing, `crates/daemon/tests/` if it is the reader or the event, `crates/ide/tests/transcript_tests.rs` if it is the model, a `bs_widget_test_*` in `TranscriptView.cpp` if it is the view.

**Interfaces:**
- Consumes: the finding from Task 2.
- Produces: an agent that stops on a tool needing approval and shows the amber bar, with Allow and Deny both reaching the CLI.

- [ ] **Step 1: Write the failing regression test**

At the level the finding names, assert the behaviour that is missing. Whatever the level, the test must fail before the fix and pass after. If the finding is in `parse_line`, the shape is:

```rust
#[test]
fn a_can_use_tool_request_becomes_a_permission_request() {
    let line = r#"{"type":"control_request","request_id":"req_1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hello"}}}"#;
    let items = parse_line(line);
    assert!(items.iter().any(|item| matches!(
        item,
        Parsed::Message(AgentMessageBody::PermissionRequest { request_id, tool_name, .. })
            if request_id == "req_1" && tool_name == "Bash"
    )));
    assert!(items
        .iter()
        .any(|item| matches!(item, Parsed::State(AgentState::WaitingPermission, _))));
}
```

If the real shape recorded in Task 2 differs from this one, **the recording is right and the test follows the recording.**

- [ ] **Step 2: Run it and watch it fail**

Run the single test by name. Expected: FAIL, for the reason the finding gives.

- [ ] **Step 3: Fix the root cause**

One change, at the boundary the finding names. No "while I'm here" repairs elsewhere.

- [ ] **Step 4: Run it and watch it pass**

Run the single test, then the whole suite for that crate.

- [ ] **Step 5: Prove it in the running app**

```powershell
.\launch.ps1
```

Create a Claude agent on `Ask every time`, send `run the shell command: echo hello`, and confirm the amber bar appears above the composer with Allow and Deny. Press Allow; the tool runs and the turn ends. Repeat with Deny; the agent says it was denied. **Both, not just Allow** — a Deny that never reaches the CLI hangs the same way.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -F - <<'EOF'
fix: a tool that needs approval asks, and the answer reaches the CLI

<one paragraph: what the boundary was, why it dropped the request, and
what now carries it. Name the test that would have caught it.>

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

# Group 2 — Two readings

Tasks 4–5 (theme) and Tasks 6–7 (base path, spinner) touch disjoint files **except** `MainWindow.cpp`, where Task 6 edits `updateWorkspaceStatus` and the window title, and Task 5 edits nothing. They may run in parallel with each other but not with Group 3.

## Task 4: The theme choice, in Rust

**Files:**
- Modify: `crates/ide/src/qobjects/settings.rs` (the `Settings` struct at `:163`, its `Default` at `:177`)
- Modify: `crates/ide/src/qobjects/app_controller.rs` (the bridge block near `:712`, the impl near `:2679`)
- Test: `crates/ide/tests/persistence_tests.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Settings::theme: String` (one of `system`, `light`, `dark`); `AppController::theme() -> QString` and `AppController::set_theme(QString)`, exposed to C++ as `theme()` / `setTheme(const QString&)`.

- [ ] **Step 1: Write the failing test**

Append to `crates/ide/tests/persistence_tests.rs`:

```rust
/// The palette is a setting like any other: chosen once, remembered, and
/// absent from a file written before it existed -- which must read back as
/// "follow the system" rather than as an empty string nothing can apply.
#[test]
fn the_theme_choice_round_trips_and_defaults_to_the_system_one() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("theme");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"distro":"bondsymphonic"}"#).expect("settings");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    let mut settings = Settings::load();
    assert_eq!(settings.theme, "system");

    settings.theme = "light".to_owned();
    settings.save().expect("settings save");
    assert_eq!(Settings::load().theme, "light");

    std::env::remove_var(SETTINGS_PATH_ENV);
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt the_theme_choice_round_trips`
Expected: FAIL — no field `theme`.

- [ ] **Step 3: Add the field**

In `crates/ide/src/qobjects/settings.rs`, in `pub struct Settings`:

```rust
    /// Which palette the application wears: `system` (whatever Windows is
    /// doing, and whatever it changes to at dusk), `light` or `dark`. A word
    /// rather than a bool because "follow the system" is a third state, not a
    /// missing answer.
    #[serde(default = "default_theme")]
    pub theme: String,
```

beside the struct:

```rust
/// The palette a settings file that predates the choice reads back as.
pub const DEFAULT_THEME: &str = "system";

fn default_theme() -> String {
    DEFAULT_THEME.to_owned()
}
```

and in `impl Default for Settings`, `theme: DEFAULT_THEME.into(),`.

- [ ] **Step 4: Run it and watch it pass**

Run the same test. Expected: PASS.

- [ ] **Step 5: Expose it on the controller**

In `crates/ide/src/qobjects/app_controller.rs`, in the `extern "RustQt"` block beside `default_permission_mode` (around `:718`):

```rust
        /// The palette the user chose: `system`, `light` or `dark`.
        #[qinvokable]
        #[auto_cxx_name]
        fn theme(self: &AppController) -> QString;

        /// Records the palette choice. A word this does not know is ignored,
        /// because the settings file is the only writer and an unknown one
        /// could only come from a hand edit.
        #[qinvokable]
        #[auto_cxx_name]
        fn set_theme(self: &AppController, theme: QString);
```

and in the impl beside `default_permission_mode` (around `:2679`), following that method's exact shape — load, compare, assign, save, log on failure:

```rust
    pub fn theme(&self) -> QString {
        QString::from(&Settings::load().theme)
    }

    pub fn set_theme(&self, theme: QString) {
        let theme = theme.to_string();
        if !["system", "light", "dark"].contains(&theme.as_str()) {
            return;
        }
        let mut settings = Settings::load();
        if settings.theme == theme {
            return;
        }
        settings.theme = theme;
        if let Err(e) = settings.save() {
            tracing::warn!("the theme choice could not be saved: {e}");
        }
    }
```

- [ ] **Step 6: Build and run the suite**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt`
Expected: PASS. Then `cargo fmt --all` and the clippy gate.

- [ ] **Step 7: Commit**

```bash
git add crates/ide/src/qobjects/settings.rs crates/ide/src/qobjects/app_controller.rs crates/ide/tests/persistence_tests.rs
git commit -F - <<'EOF'
feat(ide): the palette is a setting, in three states

`system`, `light` or `dark` -- a word rather than a bool, because
following whatever Windows is doing is a third answer and not a missing
one. A settings file written before the choice existed reads back as
`system`, which is what the application has effectively been doing all
along.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 5: Light mode, applied

The application never sets a palette: it asks for Fusion and takes whatever Windows hands it. So "dark only" is really "the user's Windows is dark, with no way to say otherwise". What is missing is an override, and the repaint of every widget that read a palette-derived colour once and never listened again.

**Files:**
- Modify: `crates/ide/cpp/Theme.h` (add `apply`, `Choice`)
- Modify: `crates/ide/cpp/app.cpp` (apply at startup, follow the system)
- Modify: `crates/ide/cpp/SettingsDialog.{h,cpp}` (the Appearance group)
- Modify: `crates/ide/cpp/PermissionBar.cpp`, `crates/ide/cpp/TranscriptView.cpp`, `crates/ide/cpp/ExplorerDock.cpp`, `crates/ide/cpp/GroupBar.cpp` (repaint on palette change)
- Test: `crates/ide/cpp/PermissionBar.cpp` (widget check), `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: `AppController::theme()` / `setTheme()` from Task 4.
- Produces: `theme::Choice { System, Light, Dark }`, `theme::choiceFromName(const QString&)`, `theme::apply(QApplication&, Choice)`.

- [ ] **Step 1: Write the failing widget check**

At the foot of `crates/ide/cpp/PermissionBar.cpp`, inside a new `#if defined(BS_WIDGET_TESTS)` block (copy the preamble comment from `SetupPage.cpp`):

```cpp
#if defined(BS_WIDGET_TESTS)
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

extern "C" std::int32_t bs_widget_test_permission_bar_follows_the_palette() {
    QWidget host;
    QPalette dark = host.palette();
    dark.setColor(QPalette::Base, QColor(0x1e, 0x1e, 0x1e));
    dark.setColor(QPalette::Window, QColor(0x25, 0x25, 0x25));
    host.setPalette(dark);

    auto* bar = new PermissionBar(&host);
    const QColor onDark = bar->palette().color(QPalette::Window);

    QPalette light = host.palette();
    light.setColor(QPalette::Base, QColor(0xff, 0xff, 0xff));
    light.setColor(QPalette::Window, QColor(0xf0, 0xf0, 0xf0));
    host.setPalette(light);
    QCoreApplication::processEvents();

    const QColor onLight = bar->palette().color(QPalette::Window);
    if (onLight == onDark) {
        // The amber wash is mixed into the pane's own background. A bar that
        // kept the dark mix on a light palette is a bar that read the colour
        // once, at construction, and never heard that it changed.
        return 1;
    }
    if (theme::isDark(bar->palette())) {
        return 2;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
```

Register it in `crates/ide/tests/qobject_smoke.rs`: add `fn bs_widget_test_permission_bar_follows_the_palette() -> i32;` to the `extern "C"` block, add the pair `("PermissionBar re-mixes its amber for a new palette", bs_widget_test_permission_bar_follows_the_palette)` to `checks`, and bump the array length from 16 to 17.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt widgets_behave_offscreen`
Expected: FAIL with `code 1` for the new check.

- [ ] **Step 3: Make the bar listen**

In `crates/ide/cpp/PermissionBar.h`, add to the class:

```cpp
protected:
    /// A palette change is the theme moving under the bar. The amber is mixed
    /// into the pane's own background, so it has to be mixed again rather than
    /// kept.
    void changeEvent(QEvent* event) override;

private:
    void applyWash();
```

and in the `.cpp`, move the three palette lines out of the constructor into `applyWash()`, call it from the constructor, and:

```cpp
void PermissionBar::changeEvent(QEvent* event) {
    QFrame::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyWash();
    }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run the same check. Expected: PASS.

- [ ] **Step 5: Do the same for the other three**

The identical pattern — a named `apply…()` called from both the constructor and `changeEvent` — for:
- `TranscriptView.cpp`: the error banner's `QPalette::Window` wash (built around `:155`).
- `ExplorerDock.cpp`: the header's `theme::band(palette())` fill in `buildHeader` (`:161`), and the italic grey at `:514`.
- `GroupBar.cpp`: any `theme::band` or `theme::muted` read at construction.

No new tests for these three; the `PermissionBar` check pins the pattern and these follow it.

- [ ] **Step 6: Add `theme::apply`**

At the foot of `crates/ide/cpp/Theme.h`, inside `namespace theme`:

```cpp
/// Which palette the application wears. `System` is not "dark or light
/// decided once": it is Qt's own, which changes under the application when
/// Windows changes at dusk.
enum class Choice { System, Light, Dark };

/// The word `AppController::theme()` answers with, as a choice. Anything
/// unknown is `System`, because a settings file is the only writer and a word
/// this does not know could only come from a hand edit.
inline Choice choiceFromName(const QString& name) {
    if (name == QLatin1String("light")) {
        return Choice::Light;
    }
    if (name == QLatin1String("dark")) {
        return Choice::Dark;
    }
    return Choice::System;
}

/// The word to store for a choice.
inline QString nameOfChoice(Choice choice) {
    switch (choice) {
    case Choice::Light:
        return QStringLiteral("light");
    case Choice::Dark:
        return QStringLiteral("dark");
    case Choice::System:
        break;
    }
    return QStringLiteral("system");
}

/// A flat Fusion palette in the given polarity. Only the roles Fusion actually
/// reads are set; everything else is derived by the style, which is what keeps
/// a light palette from needing a second set of hand-picked greys.
inline QPalette explicitPalette(bool dark) {
    QPalette p;
    const QColor window = dark ? QColor(0x25, 0x25, 0x26) : QColor(0xf0, 0xf0, 0xf0);
    const QColor base = dark ? QColor(0x1e, 0x1e, 0x1e) : QColor(0xff, 0xff, 0xff);
    const QColor text = dark ? QColor(0xdc, 0xdc, 0xdc) : QColor(0x1e, 0x1e, 0x1e);
    const QColor disabled = dark ? QColor(0x7f, 0x7f, 0x7f) : QColor(0x9a, 0x9a, 0x9a);
    const QColor highlight = dark ? QColor(0x2d, 0x5c, 0x8a) : QColor(0x30, 0x8c, 0xc6);
    p.setColor(QPalette::Window, window);
    p.setColor(QPalette::WindowText, text);
    p.setColor(QPalette::Base, base);
    p.setColor(QPalette::AlternateBase, dark ? window.lighter(110) : window.darker(103));
    p.setColor(QPalette::Text, text);
    p.setColor(QPalette::Button, window);
    p.setColor(QPalette::ButtonText, text);
    p.setColor(QPalette::ToolTipBase, base);
    p.setColor(QPalette::ToolTipText, text);
    p.setColor(QPalette::Highlight, highlight);
    p.setColor(QPalette::HighlightedText, dark ? QColor(0xff, 0xff, 0xff) : QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::Disabled, QPalette::Text, disabled);
    p.setColor(QPalette::Disabled, QPalette::WindowText, disabled);
    p.setColor(QPalette::Disabled, QPalette::ButtonText, disabled);
    return p;
}
```

Add `#include <QApplication>`, `#include <QStyleHints>` and `#include <QString>` to `Theme.h`.

```cpp
/// Dresses the application. `System` hands the style's own palette back, which
/// is what the application wore before there was a choice at all; the other two
/// set an explicit one. Every widget that reads a palette-derived colour hears
/// this as a `QEvent::PaletteChange`.
inline void apply(QApplication& app, Choice choice) {
    switch (choice) {
    case Choice::Light:
        app.setPalette(explicitPalette(false));
        return;
    case Choice::Dark:
        app.setPalette(explicitPalette(true));
        return;
    case Choice::System:
        break;
    }
    const bool dark = app.styleHints()->colorScheme() == Qt::ColorScheme::Dark;
    app.setPalette(explicitPalette(dark));
}
```

- [ ] **Step 7: Apply it at startup and follow the system**

In `crates/ide/cpp/app.cpp`, after `QApplication::setStyle("Fusion")` and after `controller` exists:

```cpp
    theme::apply(app, theme::choiceFromName(controller->theme()));
    // "Follow the system" means following it as it changes, not as it was at
    // startup: Windows flips at dusk and the application is often older than
    // that.
    QObject::connect(app.styleHints(), &QStyleHints::colorSchemeChanged, &app, [&app, controller](Qt::ColorScheme) {
        const theme::Choice choice = theme::choiceFromName(controller->theme());
        if (choice == theme::Choice::System) {
            theme::apply(app, choice);
        }
    });
```

- [ ] **Step 8: Add the Appearance section to Settings**

In `crates/ide/cpp/SettingsDialog.cpp`, after the `agentBox` group:

```cpp
    auto* lookBox = new QGroupBox("Appearance", body);
    auto* lookForm = new QFormLayout(lookBox);
    bodyLayout->addWidget(lookBox);
    m_theme = new QComboBox(lookBox);
    m_theme->addItem("Follow system", theme::nameOfChoice(theme::Choice::System));
    m_theme->addItem("Light", theme::nameOfChoice(theme::Choice::Light));
    m_theme->addItem("Dark", theme::nameOfChoice(theme::Choice::Dark));
    const int themeIndex = m_theme->findData(m_controller->theme());
    if (themeIndex >= 0) {
        m_theme->setCurrentIndex(themeIndex);
    }
    lookForm->addRow("Theme:", m_theme);
    // Applied as it is picked rather than on OK: a palette is the one setting
    // whose effect is the whole point of choosing it, and a preview that waits
    // for a dialog to close is not a preview.
    QObject::connect(m_theme, &QComboBox::currentIndexChanged, this, [this](int) {
        theme::apply(*qApp, theme::choiceFromName(m_theme->currentData().toString()));
    });
```

In `accept()`, `m_controller->setTheme(m_theme->currentData().toString());`. In `reject()` — which must now be overridden — re-apply the stored choice, so a cancelled dialog does not leave the preview on. Declare `QComboBox* m_theme = nullptr;` in the header.

- [ ] **Step 9: Run the app and look at it**

```powershell
.\launch.ps1 -SkipDaemon
```

Open Settings, pick Light. The whole window turns light as you pick it, including the Explorer header band, the agent transcript and the terminal. Pick Dark, then Follow system, then Cancel — the window returns to the stored choice. Reopen: the choice stuck.

- [ ] **Step 10: Run the gates and commit**

```bash
cargo fmt --all && cargo test -p bondsymphonic-proto -p bondsymphonic-ide --features bondsymphonic-ide/require-qt && cargo clippy -p bondsymphonic-ide --all-targets --features bondsymphonic-ide/require-qt -- -D warnings
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): a light palette, and widgets that hear it arrive

The application never set a palette: it asked for Fusion and wore
whatever Windows handed it, so "dark only" was really "your Windows is
dark and there is no way to say otherwise". `theme::apply` sets an
explicit one for Light and Dark and follows `QStyleHints::colorScheme`
for Follow system -- as it changes, not as it was at startup, because
Windows flips at dusk and the window is usually older than that.

The other half is widgets that read a palette-derived colour once, at
construction, and never heard it change. The amber of the permission
bar, the transcript's error banner, the Explorer's header band and the
group bar are all mixed into the pane's own background, so they are
mixed again on `QEvent::PaletteChange` rather than kept. The terminal
already did this; the pattern is now four more widgets wide.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 6: A workspace names its repository, not its own branch

`bs/agent-4/work` is generated from the agent's name. It is shown in four places and says nothing the tab does not already say.

**Files:**
- Modify: `crates/ide/cpp/GroupBar.cpp:256-266` (tab label)
- Modify: `crates/ide/cpp/MainWindow.cpp:1490` (window title), `:1546-1550` (`m_branchLabel`), `:526` (its initial text)
- Modify: `crates/ide/cpp/ExplorerDock.cpp` (`setWorkspaceHeader`'s detail line)
- Test: `crates/ide/cpp/GroupBar.cpp` widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: the workspace JSON already passed around, which carries `name`, `branch`, `base_branch`, `repo_path` and `worktree_path`.
- Produces: a free function in `GroupBar.cpp`'s anonymous namespace is not enough — put `QString workspaceOrigin(const QJsonObject& tab, bool full)` in `Theme.h`'s neighbour `AgentChoices.h`? **No.** Put it in `crates/ide/cpp/WorkspaceLabel.h`, a new header-only file, so all three callers share one spelling.

- [ ] **Step 1: Write the failing widget check**

At the foot of `crates/ide/cpp/GroupBar.cpp`, in its `BS_WIDGET_TESTS` block (create one if absent, copying the preamble from `SetupPage.cpp`):

```cpp
extern "C" std::int32_t bs_widget_test_group_bar_names_the_repository() {
    const QJsonObject tab{
        { "workspace_id", "ws_1" },
        { "name", "agent-4" },
        { "branch", "bs/agent-4/work" },
        { "base_branch", "main" },
        { "repo_path", "C:/git/BondSymphonic" },
    };
    const QString label = workspaceLabel(tab);
    if (label.contains(QLatin1String("bs/agent-4/work"))) {
        // The generated branch is the agent's own name spelled a second way.
        return 1;
    }
    if (!label.contains(QLatin1String("agent-4")) ||
        !label.contains(QLatin1String("BondSymphonic")) ||
        !label.contains(QLatin1String("main"))) {
        return 2;
    }
    if (!workspaceTooltip(tab).contains(QLatin1String("bs/agent-4/work"))) {
        // Still what gets merged, and a user who needs it in a git command
        // needs it exactly.
        return 3;
    }
    return 0;
}
```

Register it in `qobject_smoke.rs` (extern block, `checks` pair `("GroupBar names the repository rather than the generated branch", …)`, bump the length).

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt widgets_behave_offscreen`
Expected: FAIL to compile — `workspaceLabel` does not exist.

- [ ] **Step 3: Write the shared header**

Create `crates/ide/cpp/WorkspaceLabel.h`:

```cpp
#pragma once
#include <QFileInfo>
#include <QJsonObject>
#include <QString>

/// How a workspace is named on screen.
///
/// Not by its branch. `bs/agent-4/work` is generated from the agent's own name,
/// so a tab reading "agent-4 . bs/agent-4/work" says one thing twice and never
/// says which repository the agent is in -- which is the question a window
/// holding six agents from three projects actually raises. The branch is still
/// what gets merged, so it keeps its place in the tooltip, where a user who
/// needs it for a git command finds it spelled exactly.
namespace workspacelabel {

/// The last component of a repository path: `C:/git/BondSymphonic` ->
/// `BondSymphonic`. What a tab has room for.
inline QString repoName(const QString& repoPath) {
    const QString name = QFileInfo(repoPath).fileName();
    return name.isEmpty() ? repoPath : name;
}

/// `BondSymphonic @ main` -- the repository the work came from and the branch
/// it will go back to.
inline QString origin(const QJsonObject& tab) {
    const QString repo = repoName(tab.value(QStringLiteral("repo_path")).toString());
    const QString base = tab.value(QStringLiteral("base_branch")).toString();
    if (repo.isEmpty()) {
        return base;
    }
    return base.isEmpty() ? repo : repo + QStringLiteral(" @ ") + base;
}

/// The same with the repository spelled in full, for the status bar and the
/// window title, which have the width for it.
inline QString originFull(const QJsonObject& tab) {
    const QString repo = tab.value(QStringLiteral("repo_path")).toString();
    const QString base = tab.value(QStringLiteral("base_branch")).toString();
    if (repo.isEmpty()) {
        return base;
    }
    return base.isEmpty() ? repo : repo + QStringLiteral(" @ ") + base;
}

/// What the tooltip adds: the branch the agent commits to, and the worktree it
/// does the work in.
inline QString detail(const QJsonObject& tab) {
    const QString branch = tab.value(QStringLiteral("branch")).toString();
    const QString tree = tab.value(QStringLiteral("worktree_path")).toString();
    QString out = originFull(tab);
    if (!branch.isEmpty()) {
        out += QStringLiteral("\nbranch: ") + branch;
    }
    if (!tree.isEmpty()) {
        out += QStringLiteral("\nworktree: ") + tree;
    }
    return out;
}

} // namespace workspacelabel
```

In `GroupBar.cpp`, add the two test-facing helpers used by the check (inside the anonymous namespace, above the `BS_WIDGET_TESTS` block, or as file-static functions the block can see):

```cpp
QString workspaceLabel(const QJsonObject& tab) {
    const QString name = tab.value(QStringLiteral("name")).toString();
    const QString origin = workspacelabel::origin(tab);
    return origin.isEmpty() ? name : name + separator() + origin;
}

QString workspaceTooltip(const QJsonObject& tab) {
    return workspacelabel::detail(tab);
}
```

- [ ] **Step 4: Use them in the four places**

- `GroupBar.cpp:256-266` — replace the `branch` read with `workspaceLabel(tabJson)`, and set the tab's tooltip to `workspaceTooltip(tabJson)`.
- `MainWindow.cpp:1490` — the window title's middle term becomes `workspacelabel::origin(active)`.
- `MainWindow.cpp:1546-1550` — `m_branchLabel` shows `workspacelabel::originFull(active)` and gets `workspacelabel::detail(active)` as its tooltip; its empty text at `:526` and `:1546` becomes `"-"`.
- `ExplorerDock.cpp` — the header's detail line shows `origin`, with `detail` as its tooltip. The worktree path line added last week stays exactly as it is.

- [ ] **Step 5: Run it and watch it pass**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt widgets_behave_offscreen`
Expected: PASS.

- [ ] **Step 6: Look at it**

```powershell
.\launch.ps1 -SkipDaemon
```

A tab reads `agent-4 · BondSymphonic @ main`; hovering it shows the branch and the worktree. The status bar reads the full path. The window title matches.

- [ ] **Step 7: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): a workspace names its repository, not its own branch

`bs/agent-4/work` is generated from the agent's name, so a tab reading
"agent-4 . bs/agent-4/work" said one thing twice and never said which
repository the agent was in -- the question a window holding six agents
from three projects actually raises. All four places now read the
repository and the branch the work forked from; the generated branch
keeps its place in the tooltip, because it is still what gets merged and
a user who needs it for a git command needs it exactly.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 7: The branch dropdown says it is working

**Files:**
- Modify: `crates/ide/cpp/NewAgentDialog.{h,cpp}` — `inspectRepo` (`:400`), `onRepoInspected` (`:412`), `onRepoInspectFailed`, the combo's construction (`:124`)
- Test: `crates/ide/cpp/NewAgentDialog.cpp` widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: the existing `m_inspectPending` / `m_inspectFailed` flags.
- Produces: `NewAgentDialog::updateBranchState()`, private.

- [ ] **Step 1: Write the failing widget check**

In `NewAgentDialog.cpp`'s existing `BS_WIDGET_TESTS` block:

```cpp
extern "C" std::int32_t bs_widget_test_new_agent_dialog_says_branches_are_loading() {
    FakeController controller; // the one the existing checks in this file build
    NewAgentDialog dialog(&controller, QString(), nullptr);
    auto* combo = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentBaseBranch"));
    if (combo == nullptr) {
        return 1;
    }
    dialog.setRepoPathForTest(QStringLiteral("C:/git/BondSymphonic"));
    if (combo->isEnabled()) {
        // An enabled, empty combo during an inspection looks like a repository
        // with no branches.
        return 2;
    }
    if (!combo->currentText().contains(QLatin1String("Loading"))) {
        return 3;
    }
    dialog.onRepoInspectedForTest(QStringLiteral("C:/git/BondSymphonic"),
                                  QStringLiteral(R"({"is_repo":true,"branches":["main","dev"],"default_branch":"main"})"));
    if (!combo->isEnabled() || combo->count() != 2 || combo->currentText() != QLatin1String("main")) {
        return 4;
    }
    dialog.setRepoPathForTest(QStringLiteral("C:/git/Other"));
    dialog.onRepoInspectFailedForTest(QStringLiteral("C:/git/Other"));
    if (!combo->currentText().contains(QLatin1String("Could not read"))) {
        return 5;
    }
    return 0;
}
```

Use whatever fake-controller and test-seam helpers the two existing `NewAgentDialog` checks already use; add `setRepoPathForTest` / `onRepoInspectedForTest` / `onRepoInspectFailedForTest` as `#if defined(BS_WIDGET_TESTS)` methods only if equivalents are not already there. Give the combo `setObjectName(QStringLiteral("NewAgentBaseBranch"))` at construction.

Register it in `qobject_smoke.rs` and bump the length.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt widgets_behave_offscreen`
Expected: FAIL with `code 2` — the combo is enabled and empty.

- [ ] **Step 3: Implement the three states**

In `NewAgentDialog.cpp`:

```cpp
void NewAgentDialog::updateBranchState() {
    if (m_inspectPending) {
        m_baseBranch->setEnabled(false);
        m_baseBranch->clear();
        // An item rather than a placeholder: a placeholder is invisible on an
        // editable combo that has an empty edit, which is exactly this state.
        m_baseBranch->addItem(QStringLiteral("Loading branches\u2026"));
        m_branchBusy->show();
        return;
    }
    m_branchBusy->hide();
    if (m_inspectFailed) {
        m_baseBranch->setEnabled(false);
        m_baseBranch->clear();
        m_baseBranch->addItem(QStringLiteral("Could not read branches"));
        return;
    }
    m_baseBranch->setEnabled(true);
}
```

`m_branchBusy` is a small indeterminate `QProgressBar` beside the combo — `setRange(0, 0)`, `setTextVisible(false)`, `setMaximumWidth(16)` — added to a `QHBoxLayout` that replaces the bare combo in `form->addRow("Base branch:", …)`.

Call `updateBranchState()` at the end of `inspectRepo`, at the top of `onRepoInspected` (before the combo is filled) and in the inspect-failed handler. `onRepoInspected` must call it *before* `m_baseBranch->clear()` so the loading item is gone before the branches go in.

- [ ] **Step 4: Run it and watch it pass**

Run the same check. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ide/cpp/NewAgentDialog.h crates/ide/cpp/NewAgentDialog.cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): the branch dropdown says it is working

The dialog opened with an empty Base branch combo and filled it when
`repo.inspect` answered, which on a cold or remote repository is several
seconds of a control that looks broken rather than busy. It now holds
one item saying so, disabled, with a busy indicator beside it, and says
"Could not read branches" when the inspection failed -- the two states
Create was already refusing to act on, finally spelled where the user
is looking.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

# Group 3 — The shell

**These four tasks own `MainWindow.{h,cpp}`. Run them in sequence, and never beside a task from another group.**

## Task 8: The agent pane becomes a dock

**Files:**
- Modify: `crates/ide/cpp/MainWindow.h` (`m_centerSplitter` out, `m_agentDock` in; `noteSplitterState` out), `crates/ide/cpp/MainWindow.cpp` (`buildCentral` `:289-312`, the View swap `:236-247`, the restore `:1162-1176`, `noteSplitterState` `:1217-1226`)
- Test: `crates/ide/cpp/MainWindow.cpp` widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `MainWindow::m_agentDock` (a `QDockWidget*` named `AgentDock`), `MainWindow::resetLayout()`, and a `saveState` version of `2`.

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_main_window_docks_the_agent_pane() {
    // Built with the same fakes the other MainWindow checks use.
    MainWindow window(controller, groupModel, fileTreeModel, changesModel, runModel);
    auto* dock = window.findChild<QDockWidget*>(QStringLiteral("AgentDock"));
    if (dock == nullptr) {
        return 1;
    }
    if (!dock->features().testFlag(QDockWidget::DockWidgetClosable) ||
        !dock->features().testFlag(QDockWidget::DockWidgetFloatable) ||
        !dock->features().testFlag(QDockWidget::DockWidgetMovable)) {
        return 2;
    }
    if (window.dockWidgetArea(dock) != Qt::RightDockWidgetArea) {
        return 3;
    }
    // The editor is the centre and cannot be closed out from under the window.
    if (window.centralWidget() == nullptr) {
        return 4;
    }
    dock->close();
    if (dock->isVisible()) {
        return 5;
    }
    dock->toggleViewAction()->trigger();
    if (!dock->isVisible()) {
        return 6;
    }
    return 0;
}
```

If no `MainWindow` widget check exists yet, build the window with the same fake controller the other checks use; if constructing a whole `MainWindow` offscreen proves impractical, assert on `buildDocks`'s result through a smaller seam and say so in the commit message.

Register it and bump the length.

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL with `code 1` — there is no `AgentDock`.

- [ ] **Step 3: Replace the splitter**

In `buildCentral`, the editor becomes the central widget and the agent area becomes a dock:

```cpp
    m_editorArea = new EditorArea(this);
    setCentralWidget(m_editorArea);

    // Visual Studio's shape: the documents are the fixed centre and the tool
    // windows dock around them. A dock also gives the boundary the splitter
    // never had -- a title bar and a grabbable separator, where a 1px handle
    // was the only thing saying where one pane stopped and the next began.
    m_agentDock = new QDockWidget("Agent", this);
    m_agentDock->setObjectName("AgentDock");
    m_agentArea = new AgentArea(m_agentDock);
    m_agentDock->setWidget(m_agentArea);
    addDockWidget(Qt::RightDockWidgetArea, m_agentDock);
    resizeDocks({ m_agentDock }, { 600 }, Qt::Horizontal);
```

Give the separator its width and colour in the constructor, next to the other window-level setup:

```cpp
    // Wide enough to grab and to read as an edge. The default is three pixels,
    // which is neither.
    setStyleSheet(QStringLiteral("QMainWindow::separator { background: %1; width: 6px; height: 6px; }")
                      .arg(theme::band(palette()).name()));
```

and re-apply it from a `changeEvent` override, the same pattern Task 5 established.

- [ ] **Step 4: Re-express what read the splitter**

- **View ▸ Swap editor and agent** becomes:

```cpp
    view->addAction("&Swap editor and agent", this, [this] {
        const Qt::DockWidgetArea now = dockWidgetArea(m_agentDock);
        addDockWidget(now == Qt::RightDockWidgetArea ? Qt::LeftDockWidgetArea : Qt::RightDockWidgetArea,
                      m_agentDock);
        m_swapped = !m_swapped;
        noteWindowState();
    });
```

- **`noteSplitterState` and `splitter_sizes`** are deleted. Everywhere `noteSplitterState()` was called, call `noteWindowState()`. In the restore path (`:1162-1176`), drop the `splitter_sizes` handling entirely; `QMainWindow::restoreState` already carries dock geometry.
- **Bump the saved-state version.** Wherever `saveState()` / `restoreState()` are called, pass `2`: `saveState(2)`, `restoreState(bytes, 2)`. A version mismatch makes `restoreState` return false and leave the default arrangement, which is exactly right for a session saved when the agent pane was not a dock.

- [ ] **Step 5: Add `resetLayout`**

```cpp
/// The default arrangement, for a user who has dragged themselves into a
/// corner. Not a `restoreState` of a remembered default: the docks are simply
/// put back where `buildDocks` puts them, which is the one arrangement that is
/// guaranteed to exist.
void MainWindow::resetLayout() {
    for (QDockWidget* dock : { static_cast<QDockWidget*>(m_explorer), m_agentDock, m_bottomDock }) {
        dock->setFloating(false);
        dock->show();
    }
    addDockWidget(Qt::LeftDockWidgetArea, m_explorer);
    addDockWidget(Qt::RightDockWidgetArea, m_agentDock);
    addDockWidget(Qt::BottomDockWidgetArea, m_bottomDock);
    resizeDocks({ static_cast<QDockWidget*>(m_explorer), m_agentDock }, { 280, 600 }, Qt::Horizontal);
    m_swapped = false;
    noteWindowState();
}
```

`m_bottomDock` is the local `bottom` in `buildDocks` promoted to a member, so `resetLayout` and the Window menu can reach it.

- [ ] **Step 6: Run it and watch it pass, then drive it**

Run the offscreen check, then:

```powershell
.\launch.ps1 -SkipDaemon
```

Drag the Agent dock to the left of the editor; drag it out to float; close it; bring it back (the next task's menu, or `View ▸ Swap` for now). Close and reopen the app: the arrangement is remembered.

- [ ] **Step 7: Commit**

```bash
git add crates/ide/cpp/MainWindow.h crates/ide/cpp/MainWindow.cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): the agent pane is a dock, and the divider is a real edge

Visual Studio's shape: the documents are the fixed centre and the tool
windows dock around them. The centre splitter is gone, the agent pane is
a QDockWidget on the right beside the Explorer and the Output dock, and
all three drag, float, tab together and close.

It answers the divider too. A 1px splitter handle was the only thing
saying where the file view stopped and the agent output began; a dock
separator is six pixels of the band colour with a title bar above it.

`splitter_sizes` leaves the session file -- `QMainWindow::saveState`
already carried dock geometry and now carries all of it -- and the saved
state's version is bumped, so a layout remembered from before the agent
pane was a dock is discarded rather than fought with.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 9: The Window menu

**Files:**
- Modify: `crates/ide/cpp/MainWindow.cpp` (`buildMenus` `:201`, `:248`)
- Test: widget check in `MainWindow.cpp`; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: `m_explorer`, `m_agentDock`, `m_bottomDock`, `resetLayout()` from Task 8.
- Produces: a `&Window` menu holding three checkable dock actions and Reset layout.

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_window_menu_brings_a_closed_dock_back() {
    MainWindow window(controller, groupModel, fileTreeModel, changesModel, runModel);
    QMenu* menu = nullptr;
    for (QMenu* candidate : window.menuBar()->findChildren<QMenu*>()) {
        if (candidate->title() == QLatin1String("&Window")) {
            menu = candidate;
        }
    }
    if (menu == nullptr || menu->actions().isEmpty()) {
        return 1;
    }
    auto* dock = window.findChild<QDockWidget*>(QStringLiteral("BottomDock"));
    dock->close();
    QAction* entry = nullptr;
    for (QAction* action : menu->actions()) {
        if (action->text().contains(QLatin1String("Output"))) {
            entry = action;
        }
    }
    if (entry == nullptr || !entry->isCheckable()) {
        return 2;
    }
    if (entry->isChecked()) {
        // The tick and the dock cannot disagree: a closed dock's entry is
        // unticked, which is what tells the user it is the way back.
        return 3;
    }
    entry->trigger();
    if (!dock->isVisible()) {
        return 4;
    }
    return 0;
}
```

Register it and bump the length.

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL with `code 1` — there is no Window menu.

- [ ] **Step 3: Build the menu**

Replacing the bare `menuBar()->addMenu("&Workspace")` / `("&Run")` placeholders' neighbour, after the View menu:

```cpp
    // Qt's own toggle actions, not hand-written ones: the tick and the dock
    // cannot then disagree, and a dock closed by its own X updates the menu
    // without anything having to notice.
    auto* window = menuBar()->addMenu("&Window");
    window->addAction(m_explorer->toggleViewAction());
    window->addAction(m_agentDock->toggleViewAction());
    window->addAction(m_bottomDock->toggleViewAction());
    window->addSeparator();
    window->addAction("&Reset layout", this, &MainWindow::resetLayout);
```

Give each dock a window title the menu can use: `m_explorer->setWindowTitle("Explorer")`, the agent dock already has `"Agent"`, the bottom dock `"Output"`.

`buildMenus` runs before `buildDocks` today (`:94`). Move the Window menu's construction into its own `buildWindowMenu()` called after `buildDocks`, or move `buildDocks` above `buildMenus` — whichever leaves the constructor reading in the order things are needed.

- [ ] **Step 4: Run it and watch it pass, then drive it**

Run the check. Then launch, close the Output dock with its X, and bring it back from Window ▸ Output. Drag the Explorer somewhere silly and use Window ▸ Reset layout.

- [ ] **Step 5: Commit**

```bash
git add crates/ide/cpp/MainWindow.cpp crates/ide/cpp/MainWindow.h crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): a Window menu, so a closed pane has a way back

Closing the Output dock by accident left no way to reopen it: Qt's only
route is a right-click on the menu bar, which nobody finds. The three
docks are listed with their own `toggleViewAction`s, so the tick and the
dock cannot disagree and a dock closed by its own X updates the menu
without anything having to notice. Reset layout is for the user who has
dragged themselves into a corner.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 10: The Workspace menu

Every action here exists already — in the Changes toolbar, in `GroupBar`'s two context menus, or as a `MainWindow` slot. The menu reuses the *same* `QAction` objects so enablement is written once.

**Files:**
- Modify: `crates/ide/cpp/MainWindow.{h,cpp}`, `crates/ide/cpp/ChangesToolbar.{h,cpp}` (expose its five actions)
- Test: widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: `ChangesToolbar::mergeAction()` … `discardAction()` (new accessors returning the existing `QAction*`s); `MainWindow::onNewAgent`, `onStartAgentRequested`, `onCloseGroup`, and the destroy path at `:932`.
- Produces: `MainWindow::m_restartAgentAction` (`QAction*`), used again by Task 14.

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_workspace_menu_gathers_the_git_actions() {
    MainWindow window(controller, groupModel, fileTreeModel, changesModel, runModel);
    QMenu* menu = menuTitled(window, QLatin1String("&Workspace"));
    if (menu == nullptr) {
        return 1;
    }
    const QStringList wanted{ "New Agent", "Restart agent", "Merge", "Rebase",
                              "Squash",    "Create PR",     "Discard", "Destroy workspace",
                              "Close group" };
    for (const QString& text : wanted) {
        if (!menuHasAction(menu, text)) {
            return 2;
        }
    }
    // With no workspace, everything that acts on one is dead rather than
    // offering an action that can only fail.
    if (menuActionEnabled(menu, QLatin1String("Merge"))) {
        return 3;
    }
    if (!menuActionEnabled(menu, QLatin1String("New Agent"))) {
        return 4;
    }
    return 0;
}
```

`menuTitled`, `menuHasAction` and `menuActionEnabled` are three small static helpers in the same `BS_WIDGET_TESTS` block; write them once here and reuse them in Task 11.

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL with `code 1`.

- [ ] **Step 3: Expose the toolbar's actions**

In `ChangesToolbar.h`:

```cpp
    /// The five git actions as the toolbar's own `QAction`s, so the Workspace
    /// menu offers the same objects rather than a second set that has to be
    /// enabled in parallel and will eventually drift from these.
    QAction* mergeAction() const;
    QAction* rebaseAction() const;
    QAction* squashAction() const;
    QAction* prAction() const;
    QAction* discardAction() const;
```

each returning the existing `m_merge` … `m_discard`.

- [ ] **Step 4: Build the menu**

```cpp
    auto* workspace = menuBar()->addMenu("&Workspace");
    workspace->addAction("&New Agent…", this, &MainWindow::onNewAgent);
    m_restartAgentAction = workspace->addAction("&Restart agent", this, [this] {
        onStartAgentRequested(activeWorkspaceId());
    });
    m_restartAgentAction->setShortcut(QKeySequence("Ctrl+Shift+R"));
    workspace->addSeparator();
    ChangesToolbar* toolbar = m_explorer->changesToolbar();
    workspace->addAction(toolbar->mergeAction());
    workspace->addAction(toolbar->rebaseAction());
    workspace->addAction(toolbar->squashAction());
    workspace->addAction(toolbar->prAction());
    workspace->addAction(toolbar->discardAction());
    workspace->addSeparator();
    m_destroyAction = workspace->addAction("&Destroy workspace…", this, [this] {
        destroyWorkspace(activeWorkspaceId());
    });
    m_closeGroupAction = workspace->addAction("&Close group…", this, [this] {
        onCloseGroup(activeGroupName());
    });
```

`destroyWorkspace(const QString&)` is the body of the existing destroy path at `:932` lifted into a named private method so both the context menu and this entry call it. Same for `activeGroupName()`.

In `updateWorkspaceStatus` — which already runs on every tab change — set `m_restartAgentAction`, `m_destroyAction` and `m_closeGroupAction` enabled from whether there is an active workspace. The five toolbar actions enable themselves already.

- [ ] **Step 5: Run it and watch it pass, then drive it**

Run the check; then launch and merge a workspace from the menu rather than from the Changes tab.

- [ ] **Step 6: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): the Workspace menu, made of the actions that already existed

Merge, Rebase, Squash, Create PR and Discard were reachable only from
the Changes tab's toolbar, which is invisible unless that tab is open;
Destroy workspace and Close group were right-click-only. The menu offers
the toolbar's own QAction objects rather than a second set, so
enablement is written once and cannot drift.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 11: The Run menu

**Files:**
- Modify: `crates/ide/cpp/MainWindow.{h,cpp}`, `crates/ide/cpp/RunPanel.{h,cpp}` (expose its actions and the configuration list)
- Test: widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: `RunPanel`'s `m_start`, `m_stop`, `m_open`, `m_configs`, `m_allow`.
- Produces: `RunPanel::runAction()`, `stopAction()`, `restartAction()`, `openAction()`, `clearOutputAction()`, `copyOutputAction()`, `allowHostAction()`, and `RunPanel::configurationActions() -> QList<QAction*>` (a checkable `QActionGroup` mirroring the combo).

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_run_menu_drives_the_run_panel() {
    MainWindow window(controller, groupModel, fileTreeModel, changesModel, runModel);
    QMenu* menu = menuTitled(window, QLatin1String("&Run"));
    if (menu == nullptr) {
        return 1;
    }
    for (const QString& text : QStringList{ "Run", "Stop", "Restart", "Open in browser",
                                            "Clear output", "Copy output", "Allow blocked host" }) {
        if (!menuHasAction(menu, text)) {
            return 2;
        }
    }
    QAction* run = menuAction(menu, QLatin1String("Run"));
    if (run->shortcut() != QKeySequence(Qt::Key_F5)) {
        return 3;
    }
    if (menuAction(menu, QLatin1String("Stop"))->shortcut() !=
        QKeySequence(Qt::ShiftModifier | Qt::Key_F5)) {
        return 4;
    }
    // The detected configurations are one checkable group, so exactly one is
    // the one that will run.
    window.noteRunConfigsForTest(QStringLiteral(R"([{"name":"web"},{"name":"api"}])"));
    int checkable = 0;
    for (QAction* action : menu->actions()) {
        if (action->isCheckable()) {
            ++checkable;
        }
    }
    if (checkable != 2) {
        return 5;
    }
    return 0;
}
```

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL with `code 1`.

- [ ] **Step 3: Expose the panel's actions and build the menu**

Add the accessors above to `RunPanel`, each returning a `QAction` the panel owns and its button triggers (create the `QAction` and connect the button to it, rather than the other way round, so the panel and the menu are the same action). `configurationActions` builds a `QActionGroup` over `m_configs`' items and keeps it in step when the combo is refilled.

```cpp
    auto* run = menuBar()->addMenu("&Run");
    run->addAction(m_runPanel->runAction());
    m_runPanel->runAction()->setShortcut(QKeySequence(Qt::Key_F5));
    run->addAction(m_runPanel->stopAction());
    m_runPanel->stopAction()->setShortcut(QKeySequence(Qt::ShiftModifier | Qt::Key_F5));
    run->addAction(m_runPanel->restartAction());
    run->addSeparator();
    m_runConfigMenuAnchor = run->addSeparator();
    run->addAction(m_runPanel->openAction());
    run->addSeparator();
    run->addAction(m_runPanel->clearOutputAction());
    run->addAction(m_runPanel->copyOutputAction());
    run->addAction(m_runPanel->allowHostAction());
```

The configuration actions are inserted before `m_runConfigMenuAnchor` whenever `RunPanel` refills its combo, via a `configurationsChanged` signal the panel emits.

- [ ] **Step 4: Run it and watch it pass, then drive it**

Run the check; then launch, press F5 on a workspace with a detected configuration, and stop it with Shift+F5.

- [ ] **Step 5: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): the Run menu, with F5 where a hand expects it

Start, Stop, the configuration list, Open in browser and the blocked-host
queue were buttons inside one panel, which meant running the thing you
just built required finding that panel first. The menu offers the
panel's own actions, so a disabled Stop is disabled in both places for
the same reason, and the detected configurations are one checkable group
-- exactly one of them is the one F5 will run.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

# Group 4 — The agent pane

## Task 12: One list of models, one list of modes

**Files:**
- Create: `crates/ide/cpp/AgentChoices.h`, `crates/ide/cpp/AgentChoices.cpp`
- Modify: `crates/ide/cpp/NewAgentDialog.cpp` (`kModels` at `:72` deleted, the combo at `:170` filled from the shared list; a permission-mode combo added), `crates/ide/cpp/SettingsDialog.cpp` (`kPermissionModes` at `:22` deleted)
- Modify: `crates/ide/build.rs` if it lists sources explicitly
- Test: `crates/ide/cpp/AgentChoices.cpp` widget check; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:

```cpp
namespace agentchoices {
struct Choice { const char* label; const char* id; };
const QList<Choice>& models();
const QList<Choice>& permissionModes();
QString labelForModel(const QString& id);
QString labelForPermissionMode(const QString& id);
void fillModelCombo(QComboBox* combo, const QString& selected);
void fillPermissionCombo(QComboBox* combo, const QString& selected);
}
```

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_agent_choices_are_one_list() {
    if (agentchoices::permissionModes().size() != 4) {
        return 1;
    }
    const QStringList ids{ "manual", "acceptEdits", "plan", "bypassPermissions" };
    for (int i = 0; i < ids.size(); ++i) {
        if (QString::fromUtf8(agentchoices::permissionModes().at(i).id) != ids.at(i)) {
            return 2;
        }
    }
    // `default` is not a mode the CLI takes, and `dontAsk` denies in silence,
    // which reads like the hang this batch exists to fix.
    for (const agentchoices::Choice& choice : agentchoices::permissionModes()) {
        const QString id = QString::fromUtf8(choice.id);
        if (id == QLatin1String("default") || id == QLatin1String("dontAsk")) {
            return 3;
        }
    }
    if (agentchoices::labelForPermissionMode(QStringLiteral("bypassPermissions")) !=
        QLatin1String("YOLO (sandboxed)")) {
        return 4;
    }
    QComboBox combo;
    agentchoices::fillModelCombo(&combo, QStringLiteral("claude-sonnet-5"));
    if (combo.count() != 4 || combo.currentData().toString() != QLatin1String("claude-sonnet-5")) {
        return 5;
    }
    if (!combo.isEditable()) {
        // Claude Code takes any model name; the list is a convenience, not a
        // gate.
        return 6;
    }
    return 0;
}
```

Register it and bump the length.

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL to compile.

- [ ] **Step 3: Write the header and the source**

`AgentChoices.h` declares the interface above with the doc comments explaining *why* there is one list (three dialogs offering three different sets of models is three chances to drift). `AgentChoices.cpp` defines:

```cpp
const QList<agentchoices::Choice>& agentchoices::models() {
    static const QList<Choice> kModels{
        { "Default (Claude Code decides)", "" },
        { "Opus 5", "claude-opus-5" },
        { "Sonnet 5", "claude-sonnet-5" },
        { "Haiku 4.5", "claude-haiku-4-5-20251001" },
    };
    return kModels;
}

const QList<agentchoices::Choice>& agentchoices::permissionModes() {
    // Four of the six the CLI takes. `auto` is a mode nobody asked for, and
    // `dontAsk` denies in silence -- which is exactly what an agent that hangs
    // with nothing on screen looks like, and that is the failure this batch
    // exists to remove rather than to offer as a setting.
    static const QList<Choice> kModes{
        { "Ask every time", "manual" },
        { "Accept edits", "acceptEdits" },
        { "Plan only", "plan" },
        { "YOLO (sandboxed)", "bypassPermissions" },
    };
    return kModes;
}
```

`fillModelCombo` sets `setEditable(true)`, adds every label/id pair, and selects by `findData(selected)` — falling back to `setEditText(selected)` for an id the list does not hold, exactly as `NewAgentDialog::onRepoInspected` does for branches. `fillPermissionCombo` is the same without `setEditable`.

- [ ] **Step 4: Use it in the two dialogs**

- `NewAgentDialog.cpp`: delete `kModels`, fill `m_claudeModel` with `fillModelCombo(m_claudeModel, QString())`, and add a `Permissions:` row with `m_permissionMode`, filled with `fillPermissionCombo(m_permissionMode, controller->defaultPermissionMode())`. Add its value to `optionsJson()`; **always**, never conditionally.
- `SettingsDialog.cpp`: delete `kPermissionModes`, fill `m_permissionMode` with `fillPermissionCombo(...)`. Its label becomes `"New agents start on:"`.

- [ ] **Step 5: Run it and watch it pass**

Run the offscreen suite. Expected: PASS, and the two existing `NewAgentDialog` checks still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): one list of models, one list of permission modes

Three dialogs offering three hand-written lists is three chances to
drift, and the permission list had already drifted into offering
`default`, which the CLI rejects. There is one of each now.

The modes are four of the six the CLI takes, spelled for a person: Ask
every time, Accept edits, Plan only, and YOLO (sandboxed) -- offered
without a confirmation dialog, because what makes it reasonable is
structural. The agent is in a sandbox, in a worktree of its own, behind
a network proxy; a warning about a risk the architecture already took is
noise. `auto` and `dontAsk` are accepted by the daemon and offered by
nobody: `dontAsk` denies in silence, which is the failure this batch
exists to remove.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 13: The composer chooses the model and the mode

**Files:**
- Modify: `crates/ide/cpp/TranscriptView.{h,cpp}` (the composer at `:165-190`)
- Modify: `crates/ide/src/qobjects/transcript_model.rs` (`restart_options_json`, `:736`)
- Test: `crates/ide/cpp/TranscriptView.cpp` widget check; `crates/ide/tests/transcript_tests.rs`; `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Consumes: `agentchoices::fillModelCombo` / `fillPermissionCombo`; `TranscriptModel::restartOptionsJson()`.
- Produces: `TranscriptModel::restartOptionsWith(const QString& model, const QString& permissionMode) -> QString`; `TranscriptView::optionsChanged(const QString& optionsJson)` signal, forwarded by `AgentArea` and answered by `MainWindow` with a `startAgent`.

- [ ] **Step 1: Write the failing Rust test**

In `crates/ide/tests/transcript_tests.rs`:

```rust
/// `claude -p` fixes its model and its permission mode at process start, so
/// changing either is a restart -- and a restart that loses the conversation is
/// not a model switch, it is a new agent. The session id the history carries is
/// what makes it the same conversation.
#[test]
fn changing_the_model_keeps_the_session_and_the_other_options() {
    let options = r#"{"model":"claude-opus-5","permission_mode":"manual"}"#;
    let merged = bondsymphonic_ide::qobjects::transcript_model::restart_options_with(
        options,
        "sess_42",
        Some("claude-haiku-4-5-20251001"),
        None,
    );
    let value: serde_json::Value = serde_json::from_str(&merged).expect("json");
    assert_eq!(value["model"], "claude-haiku-4-5-20251001");
    assert_eq!(value["permission_mode"], "manual");
    assert_eq!(value["resume_session"], "sess_42");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt changing_the_model_keeps_the_session`
Expected: FAIL — no `restart_options_with`.

- [ ] **Step 3: Generalise the existing merge**

`restart_options_json` already merges `resume_session` into the tab's options (the free function at `transcript_model.rs:736`). Rename it `restart_options_with`, give it two more parameters — `model: Option<&str>` and `permission_mode: Option<&str>`, each overriding the stored value when `Some` — and have the existing `restart_options_json` call it with `None, None`. Add the qinvokable:

```rust
        /// The tab's options with the session filled in and one field replaced:
        /// what a model or permission change sends, because both are fixed at
        /// process start and changing either is a restart.
        #[qinvokable]
        #[auto_cxx_name]
        fn restart_options_with(
            self: &TranscriptModel,
            model: QString,
            permission_mode: QString,
        ) -> QString;
```

An empty `QString` for either means "leave the stored one alone"; the `Default (Claude Code decides)` model id is `""`, so it is passed as the literal string `"default"`-free sentinel — use a distinct marker: the C++ side sends `"-"` for "no change" and the empty string for "clear the model". Document that on the qinvokable.

- [ ] **Step 4: Run it and watch it pass**

Run the test. Expected: PASS.

- [ ] **Step 5: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_transcript_composer_offers_model_and_mode() {
    TranscriptView view(nullptr);
    auto* model = view.findChild<QComboBox*>(QStringLiteral("TranscriptModelChoice"));
    auto* mode = view.findChild<QComboBox*>(QStringLiteral("TranscriptPermissionChoice"));
    if (model == nullptr || mode == nullptr) {
        return 1;
    }
    if (view.findChild<QPushButton*>(QStringLiteral("bsStartAgent")) != nullptr) {
        // Start and Stop are gone; Interrupt stays, because ending a turn is
        // not killing a process.
        return 2;
    }
    QString sent;
    QObject::connect(&view, &TranscriptView::optionsChanged, [&sent](const QString& json) { sent = json; });
    mode->setCurrentIndex(mode->findData(QStringLiteral("bypassPermissions")));
    if (!sent.contains(QLatin1String("bypassPermissions"))) {
        return 3;
    }
    return 0;
}
```

- [ ] **Step 6: Run it, watch it fail, build the row**

In `TranscriptView`'s constructor, replace the `m_start` / `m_stop` column with:

```cpp
    auto* choices = new QHBoxLayout();
    choices->setSpacing(4);
    m_modelChoice = new QComboBox(this);
    m_modelChoice->setObjectName(QStringLiteral("TranscriptModelChoice"));
    m_modelChoice->setToolTip(QStringLiteral(
        "The model this agent answers with. Changing it restarts the agent and resumes the "
        "conversation; a turn in flight is interrupted."));
    agentchoices::fillModelCombo(m_modelChoice, QString());
    m_permissionChoice = new QComboBox(this);
    m_permissionChoice->setObjectName(QStringLiteral("TranscriptPermissionChoice"));
    m_permissionChoice->setToolTip(QStringLiteral(
        "What this agent asks before it acts. Changing it restarts the agent and resumes the "
        "conversation."));
    agentchoices::fillPermissionCombo(m_permissionChoice, QStringLiteral("manual"));
    choices->addWidget(m_modelChoice);
    choices->addWidget(m_permissionChoice);
    choices->addStretch(1);
    choices->addWidget(m_interrupt);
```

with `m_start` and `m_stop` deleted from the class entirely. Each combo's `currentIndexChanged` calls a guarded `emitOptionsChanged()` — guarded by an `m_applyingOptions` bool so that *setting* the combos from the tab's options does not emit.

`AgentArea` forwards `optionsChanged` as `agentOptionsChanged(workspaceId, optionsJson)`; `MainWindow` answers it with `m_agentArea->setStarting(id, true); m_controller->startAgent(id, optionsJson);` — the same path `onStartAgentRequested` uses.

- [ ] **Step 7: Record the switch in the transcript**

When the restart answers with a new agent id, `TranscriptView` appends a view-level system line: `— switched to Haiku 4.5, conversation resumed —`, built from `agentchoices::labelForModel`. It is a frame, not a transcript item, so it is not persisted and not replayed.

- [ ] **Step 8: Run the suite and drive it**

Launch, switch a running agent from Opus 5 to Haiku 4.5 mid-conversation, and ask it what you were just talking about. It knows.

- [ ] **Step 9: Commit**

```bash
git add crates/ide/cpp crates/ide/src/qobjects/transcript_model.rs crates/ide/tests
git commit -F - <<'EOF'
feat(ide): the model and the permission mode are chosen where the agent is

`claude -p` fixes both at process start, so choosing either is a restart
-- and a restart that loses the conversation is not a switch, it is a
new agent. The session id the history already carries is what makes it
the same one: `restart_options_with` is the merge `restartOptionsJson`
was doing for the Restart button, with one field replaced.

Start and Stop leave the composer. Interrupt stays: ending a turn is not
killing a process, and it is the only one of the three that acts on the
conversation rather than on the agent.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 14: An agent starts itself; Restart is where it belongs

**Files:**
- Modify: `crates/ide/cpp/MainWindow.cpp` (the `workspaceCreated` handler at `:651`, the reconnect at `:645`, the restore path), `crates/ide/cpp/AgentArea.{h,cpp}`, `crates/ide/cpp/WorkspaceBanner.{h,cpp}`, `crates/ide/cpp/SettingsDialog.{h,cpp}`
- Test: `crates/ide/tests/smoke.rs`; widget check in `WorkspaceBanner.cpp`

**Interfaces:**
- Consumes: `m_restartAgentAction` from Task 10; `MainWindow::onStartAgentRequested`.
- Produces: `WorkspaceBanner::restartRequested()` signal and its Restart button; `MainWindow::startAgentsThatHaveNone()`.

- [ ] **Step 1: Write the failing smoke assertion**

In `crates/ide/tests/smoke.rs`, extend the fake daemon's journal (the `pty_writes` pattern is already there) with an `agent_starts` list, and assert:

```rust
    // A Claude workspace restored from the session file starts its agent with
    // nobody pressing anything. The Start button it used to offer was a button
    // for a thing that has exactly one sensible answer.
    assert!(
        !journal.agent_starts.is_empty(),
        "a restored Claude workspace started no agent"
    );
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test smoke`
Expected: FAIL — nothing starts an agent for a restored workspace.

- [ ] **Step 3: Start them**

In `MainWindow`, one method:

```cpp
/// Starts an agent for every Claude workspace that has none.
///
/// Called after a session restore and after a reconnect, which are the two ways
/// a Claude tab comes to exist without a start already on its way -- creation
/// is the third and already starts one. A workspace whose agent exited is not
/// touched: that is a failure with a banner, and restarting it on a timer would
/// turn one crash into a loop.
void MainWindow::startAgentsThatHaveNone() {
    for (const QJsonValue& value : m_groupModel->tabsJson()) {
        const QJsonObject tab = value.toObject();
        if (tab.value("adapter").toString() != QLatin1String("claude")) {
            continue;
        }
        if (!tab.value("agent_id").toString().isEmpty()) {
            continue;
        }
        if (tab.value("state").toString() == QLatin1String("exited")) {
            continue;
        }
        const QString id = tab.value("workspace_id").toString();
        m_agentArea->setStarting(id, true);
        m_controller->startAgent(id, tab.value("options_json").toString());
    }
}
```

Call it from the restore path and from the `reconnected` handler at `:645`.

- [ ] **Step 4: Put Restart in the banner**

`WorkspaceBanner` gains a `Restart agent` button, shown only when the banner was raised for an exited agent (a new `bool restartable` parameter on `showBanner`), emitting `restartRequested()`. `AgentArea` forwards it as `startAgentRequested(workspaceId)`, which `MainWindow` already handles.

- [ ] **Step 5: Put Restart in Settings**

In `SettingsDialog`'s Agents group:

```cpp
    m_restart = new QPushButton(QStringLiteral("Restart agent"), agentBox);
    m_restart->setToolTip(QStringLiteral("Stops this workspace's agent and starts it again, "
                                         "resuming the same conversation."));
    form->addRow(QString(), m_restart);
```

connected to the *same* `QAction` the Workspace menu holds — `m_controller` cannot reach it, so `SettingsDialog` takes a `QAction* restart` in its constructor and calls `restart->trigger()`, and disables the button when the action is disabled.

- [ ] **Step 6: Run it and watch it pass, then drive it**

Launch with a saved session holding a Claude agent. It is answering before you touch anything, and there is no Start button anywhere. Kill the agent from the daemon side; the banner appears with Restart, and Restart brings it back with its history.

- [ ] **Step 7: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/smoke.rs
git commit -F - <<'EOF'
feat(ide): an agent starts itself, and Restart is where a restart belongs

A Start button is a button for a thing with exactly one sensible answer.
Every Claude workspace now starts its agent as soon as it has one to
start -- on creation, which was already true, and on a session restore
and a reconnect, which were not. An agent that exits on its own is not
restarted: that is a failure with a banner, and restarting it on a timer
would turn one crash into a loop, so the banner grows the Restart button
instead.

Restart otherwise lives in the Workspace menu and in Settings, both
triggering the same QAction.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 15: A pane that is waiting says what it is waiting as

**Files:**
- Modify: `crates/ide/cpp/TranscriptView.{h,cpp}`, `crates/ide/cpp/AgentArea.cpp` (pass the workspace's own name and origin in)
- Test: widget check in `TranscriptView.cpp`

**Interfaces:**
- Consumes: `agentchoices::labelForModel`, `labelForPermissionMode`, `workspacelabel::origin` (Task 6).
- Produces: `TranscriptView::setWelcome(const QString& name, const QString& origin, const QString& worktree)`.

- [ ] **Step 1: Write the failing widget check**

```cpp
extern "C" std::int32_t bs_widget_test_transcript_welcomes_an_empty_pane() {
    TranscriptView view(nullptr);
    view.setWelcome(QStringLiteral("agent-4"), QStringLiteral("BondSymphonic @ main"),
                    QStringLiteral("/home/bs/.bondsymphonic/worktrees/ws_1"));
    view.setTestItems(QStringList());
    const QString shown = view.welcomeTextForTest();
    if (!shown.contains(QLatin1String("agent-4")) ||
        !shown.contains(QLatin1String("BondSymphonic @ main")) ||
        !shown.contains(QLatin1String("ws_1"))) {
        return 1;
    }
    // The first real message takes its place: a welcome that stays is a frame
    // the user scrolls past for the rest of the conversation.
    view.setTestItems(QStringList{ R"({"kind":"user_text","text":"hello"})" });
    if (!view.welcomeTextForTest().isEmpty()) {
        return 2;
    }
    return 0;
}
```

- [ ] **Step 2: Run it and watch it fail**

Expected: FAIL with a compile error — no `setWelcome`.

- [ ] **Step 3: Build the frame**

A single `QLabel` above the scroll's frame column, hidden whenever the model has items and shown when it does not, with `applySmallGrey(label, false)` for the second and third lines. `rebuild()` and `onItemAppended` both call `updateWelcomeVisibility()`. It is never added to `m_frames` and never persisted.

`AgentArea::ensureTranscript` calls `setWelcome` with the workspace's name, `workspacelabel::origin(tab)` and its worktree path, which `MainWindow` already passes down for the Explorer header.

- [ ] **Step 4: Run it and watch it pass, then look at it**

A brand-new agent's pane reads:

```
agent-4 is ready.
Opus 5 · YOLO (sandboxed) · BondSymphonic @ main
/home/bs/.bondsymphonic/worktrees/ws_be2db101
```

- [ ] **Step 5: Commit**

```bash
git add crates/ide/cpp crates/ide/tests/qobject_smoke.rs
git commit -F - <<'EOF'
feat(ide): a pane that is waiting says what it is waiting as

An empty transcript said nothing at all, which after the Start button
went away left a blank pane and a prompt box. It now names the agent,
what it will answer as, which repository and branch it forked from, and
the worktree it will do the work in. Written by the view, not by the
daemon: the daemon has nothing to say until the agent speaks, and the
first real message takes the welcome's place.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 16: The small grey lines become optional

**Files:**
- Modify: `crates/ide/src/qobjects/settings.rs` (`show_agent_meta: bool`), `crates/ide/src/qobjects/app_controller.rs` (accessors), `crates/ide/cpp/SettingsDialog.{h,cpp}`, `crates/ide/cpp/TranscriptView.{h,cpp}` (`:535`)
- Test: `crates/ide/tests/persistence_tests.rs`; widget check in `TranscriptView.cpp`

**Interfaces:**
- Consumes: the settings pattern from Task 4.
- Produces: `Settings::show_agent_meta: bool` (default `true`); `AppController::show_agent_meta()` / `set_show_agent_meta(bool)`; `TranscriptView::setShowMeta(bool)`.

- [ ] **Step 1: Write the failing tests**

Rust, in `persistence_tests.rs`, the same shape as Task 4's theme test but asserting `Settings::load().show_agent_meta` is `true` for a file that predates the field and round-trips `false`.

C++, in `TranscriptView.cpp`:

```cpp
extern "C" std::int32_t bs_widget_test_transcript_hides_the_small_grey_lines() {
    TranscriptView view(nullptr);
    view.setTestItems(QStringList{
        R"({"kind":"assistant_text","text":"done"})",
        R"({"kind":"result","cost_usd":0.0042,"duration_ms":1200,"num_turns":3})",
        R"({"kind":"system","subtype":"init","text":"session started"})",
    });
    if (view.frameCount() != 3) {
        return 1;
    }
    view.setShowMeta(false);
    if (view.frameCount() != 1) {
        // The answer stays; the turn cost and the system line are what the
        // setting is about.
        return 2;
    }
    view.setShowMeta(true);
    if (view.frameCount() != 3) {
        return 3;
    }
    return 0;
}
```

- [ ] **Step 2: Run them and watch them fail**

Expected: FAIL on both.

- [ ] **Step 3: Add the setting and the hiding**

The Rust half follows Task 4 exactly. In `TranscriptView`, `setShowMeta(bool)` stores the flag and calls `rebuild()`; `makeFrame` returns nullptr — and `rebuild` skips — for `kind == "result"` and `kind == "system"` while it is false. These are already the only two kinds that go through `applySmallGrey`.

In `SettingsDialog`'s Agents group:

```cpp
    m_showMeta = new QCheckBox("Show turn cost and system lines", agentBox);
    m_showMeta->setChecked(m_controller->showAgentMeta());
    m_showMeta->setToolTip(QStringLiteral(
        "The small italic lines under an answer: what the turn cost, how long it took, and what "
        "the agent's own startup reported."));
    form->addRow(QString(), m_showMeta);
```

`accept()` writes it; `MainWindow` applies it to every open transcript when Settings closes.

- [ ] **Step 4: Run them and watch them pass**

- [ ] **Step 5: Commit**

```bash
git add crates/ide crates/ide/tests
git commit -F - <<'EOF'
feat(ide): the small grey lines are optional

The turn cost and the agent's own system lines are useful while you are
learning what an agent costs and noise for ever after. One checkbox in
Settings, on by default, hides both in every open transcript. The answers
themselves are never touched.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Task 17: The documentation catches up, and everything runs

**Files:**
- Modify: `docs/user-guide.md`, `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md`
- Test: the three CI gates, the full suite, and the running app

- [ ] **Step 1: Update the user guide**

Sections for: choosing a theme; the agent's model and permission dropdowns and what a switch does to the conversation; YOLO and why the sandbox is what makes it reasonable; agents starting themselves and where Restart is; the Window menu and dragging panes; the Workspace and Run menus with their shortcuts; turning off the small grey lines.

- [ ] **Step 2: Update the IDE design spec**

The layout diagram at §2 gains the Window menu and loses the centre splitter. The `MainWindow` description says the editor is central and the other three are docks. Remove any claim that the agent pane is a splitter child. The permission-mode list is corrected wherever it is spelled.

- [ ] **Step 3: Run every gate**

```bash
cargo fmt --all -- --check
cargo test -p bondsymphonic-proto -p bondsymphonic-ide --features bondsymphonic-ide/require-qt
cargo clippy -p bondsymphonic-ide --all-targets --features bondsymphonic-ide/require-qt -- -D warnings
cargo test -p bondsymphonic-daemon
cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings
```

All five clean.

- [ ] **Step 4: Drive the whole thing once**

```powershell
.\launch.ps1
```

In one session: create an agent (it starts itself and welcomes you), send a prompt that needs a tool (the bar appears; Allow works, Deny works), switch its model mid-conversation (it remembers), switch it to YOLO (the next tool does not ask), close the Output dock and bring it back from the Window menu, drag the Agent dock to the left, merge from the Workspace menu, press F5, switch to Light, and turn the small grey lines off.

- [ ] **Step 5: Commit**

```bash
git add docs
git commit -F - <<'EOF'
docs: the guide and the spec catch up with the shell

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
```

---

## Self-review

**Spec coverage:** §2 → Tasks 2, 3 (and Task 1 for the mode list it also fixes). §3 → Tasks 1, 12. §4 → Task 13. §5 → Tasks 14, 15. §6 → Tasks 4, 5. §7 → Task 8. §8 → Tasks 9, 10, 11. §9 → Task 16. §10 → Task 6. §11 → Task 7. §12 → every task's test steps plus Task 17. §13 → the group ordering. No section is unclaimed.

**Naming consistency:** `agentchoices::fillModelCombo` / `fillPermissionCombo` (Task 12) are the names used in Tasks 13 and 15. `workspacelabel::origin` / `originFull` / `detail` (Task 6) are the names used in Tasks 13 and 15. `restart_options_with` (Task 13) is the renamed `restart_options_json`, whose old name survives as a caller. `m_restartAgentAction` (Task 10) is the action Task 14's Settings button triggers. `m_bottomDock` is promoted from a local in Task 8 and used in Task 9's menu and Task 8's `resetLayout`.

**Ordering hazard:** `MainWindow.cpp` is edited by Tasks 6 (status bar, title), 8–11 (the shell) and 13–14 (the agent wiring). Tasks 8–11 must be one unbroken sequence with nothing else running.
