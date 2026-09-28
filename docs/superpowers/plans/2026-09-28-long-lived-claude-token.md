# Long-lived Claude Token Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents authenticate with a long-lived `claude setup-token` token that the daemon captures from a setup terminal, stores in the distro, and passes as `CLAUDE_CODE_OAUTH_TOKEN` to each agent process — so no refresh ever happens inside a sandbox.

**Architecture:** A daemon module owns one 0600 file, `~/.bondsymphonic/claude-oauth-token`. A new setup action runs `claude setup-token` on the host; the PTY pump feeds its output through a scanner that stores the first token it sees. `agent.start` adds the token to the agent command's own environment (API key from the IDE still wins); `claude_auth` passes on the file; Log out deletes it. The IDE's Setup row gains a button.

**Tech Stack:** Rust (tokio) daemon, serde proto, Qt/C++ + cxx-qt IDE.

**Spec:** `docs/superpowers/specs/2026-09-28-long-lived-claude-token-design.md`

## Global Constraints

- The token never appears in any log line, error message, event, or test output. Logs say "stored"/"removed" and the path.
- Token file: `<daemon data root>/claude-oauth-token` (`DataDirs.root`, i.e. `~/.bondsymphonic`), mode 0600, temp file + rename, symlinked destination refused.
- Token shape accepted by the store: prefix `sk-ant-oat01-`, then only `[A-Za-z0-9_-]`, total length ≥ 40 and ≤ 512.
- Environment precedence for the agent process: `ANTHROPIC_API_KEY` (from the IDE) > `CLAUDE_CODE_OAUTH_TOKEN` (from the file) > nothing. Given to the one command only, never to the sandbox spec.
- Credentials seeding and the write-back in `agents/credentials.rs` are unchanged.
- Parallel lanes commit **by path** (`git commit -- <paths>`), re-check `git status` right before committing, never `git add -A`. The tree must keep compiling after every commit.
- Daemon changes are verified on Windows (`cargo test -p bondsymphonic-daemon --lib`, known failure `a_snapshot_notices` — skip with `-- --skip a_snapshot_notices`) **and** in the distro: `wsl -d bondsymphonic -u bs -- bash -lc 'cd /mnt/c/git/BondSymphonic && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo test -p bondsymphonic-daemon --lib'`.
- IDE tests need `. .\scripts\env.ps1` first (PowerShell), then `cargo test -p bondsymphonic-ide --lib`, `--test qobject_smoke`, `--test smoke`. Widget tests never `show()` a top-level window (resize + activate instead).
- Commit trailer: `Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>`.

## Review Focus

1. A token printed wrapped across lines or split across two PTY reads must be captured whole or not at all — never a truncated prefix stored as if valid (Task 3 tests: split chunks, and the fixture's real layout).
2. The CLI redraws its UI; the token may appear several times — only one write, and later redraws must not overwrite it with a partial string (Task 3: repeated frames).
3. A stale/emptied `.credentials.json` in the workspace home next to a valid token must not stop the agent: the env token wins (Task 2 asserts env contents; the §3 manual check confirms CLI behaviour).
4. Log out with no token file present must not fail the logout action (Task 1: `remove` on absent is Ok; handler ignores/logs errors).
5. A workspace terminal (`pty.open`) must not receive the token (Task 2: the token is in the adapter env, not `SandboxCommand` env of the spec).

---

## Lanes

- **Lane A (daemon):** Task 1 → Task 2 → Task 3. Task 3 is **gated** on the fixture from the user's capture (spec §3.1): `/tmp/setup-token.raw` in the `bondsymphonic` distro.
- **Lane B (IDE):** Task 0 → Task 4.
- The lanes touch disjoint files except the proto enum (Task 1 only) and `docs/user-guide.md` (Task 4 only).

---

### Task 0: Land the in-flight IDE auth-failure change

The working tree holds uncommitted edits from 2026-09-23 (handoff `docs/superpowers/plans/2026-09-23-handoff-token-fixes.md`, "CLI auth sentence → not logged in"): `crates/ide/cpp/AgentArea.{cpp,h}`, `MainWindow.cpp`, `TranscriptView.{cpp,h}`, `crates/ide/src/qobjects/app_controller.rs`, `crates/ide/tests/qobject_smoke.rs`, `crates/ide/tests/smoke.rs`, `docs/user-guide.md`.

**Files:** exactly those listed above. Nothing else.

**Interfaces:**
- Produces: `AppController.login_pty` (a remembered setup PTY whose exit clears the auth-failure override), `auth_failure_in(detail: &str) -> bool`. Task 4 extends the `login_pty` match.

- [ ] **Step 1: Read the diff and the handoff section.** `git diff -- <files>`. Check it does what the handoff says: override set on an agent exit whose detail contains "OAuth session expired" / "could not be refreshed" / "Failed to authenticate" / "not logged in" / "invalid_grant"; gate shows the CLI's sentence; `claude_auth` row rewritten to a cross; cleared only by a Claude login/logout setup terminal exiting or connection loss, not by a plain re-check.
- [ ] **Step 2: Build and run the IDE tests** (PowerShell): `. .\scripts\env.ps1; cargo test -p bondsymphonic-ide --lib; cargo test -p bondsymphonic-ide --test qobject_smoke; cargo test -p bondsymphonic-ide --test smoke`. Expected: all pass.
- [ ] **Step 3: Break-it check.** Temporarily make `auth_failure_in` return `false`; confirm at least one new test fails; restore.
- [ ] **Step 4: Fix anything incomplete** found in Steps 1–3 (missing test for "a plain re-check does not clear the override" if absent — add it to `smoke.rs` in the existing style).
- [ ] **Step 5: Commit by path.**
```bash
git commit -m "fix(ide): the CLI's own auth-failure sentence marks Claude as not logged in" -- crates/ide/cpp/AgentArea.cpp crates/ide/cpp/AgentArea.h crates/ide/cpp/MainWindow.cpp crates/ide/cpp/TranscriptView.cpp crates/ide/cpp/TranscriptView.h crates/ide/src/qobjects/app_controller.rs crates/ide/tests/qobject_smoke.rs crates/ide/tests/smoke.rs docs/user-guide.md
```

---

### Task 1: Token store, setup action, and logout removal (daemon + proto)

**Files:**
- Create: `crates/daemon/src/agents/token.rs`
- Modify: `crates/daemon/src/agents/mod.rs` (add `pub mod token;` beside `pub mod credentials;`)
- Modify: `crates/daemon/src/agents/credentials.rs` (make `replace_private` `pub(crate)`)
- Modify: `crates/proto/src/types.rs` (`SetupAction::ClaudeSetupToken`)
- Modify: `crates/daemon/src/setup.rs` (`ALL_ACTIONS` → 7 entries, argv row, tests)
- Modify: `crates/daemon/src/server/handlers.rs` (`SystemSetupPty`: remove the token before `ClaudeLogout`)

**Interfaces:**
- Produces:
  - `pub const TOKEN_FILE: &str = "claude-oauth-token";`
  - `pub fn token_path(root: &Path) -> PathBuf` — `root.join(TOKEN_FILE)`
  - `pub fn is_token_shaped(s: &str) -> bool`
  - `pub fn read(path: &Path) -> Option<String>`
  - `pub fn write(path: &Path, token: &str) -> std::io::Result<()>` (refuses non-token-shaped input with `InvalidInput`)
  - `pub fn remove(path: &Path) -> std::io::Result<()>` (absent = Ok)
  - `SetupAction::ClaudeSetupToken` (serde `claude_setup_token`), argv `["claude", "setup-token"]`

- [ ] **Step 1: Write the failing tests** at the bottom of `token.rs` (`#[cfg(test)] mod tests`), using `tempfile` as the rest of the daemon does:

```rust
const T: &str = "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlAA";

#[test]
fn a_written_token_reads_back_and_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let p = token_path(dir.path());
    write(&p, T).unwrap();
    assert_eq!(read(&p).as_deref(), Some(T));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn surrounding_whitespace_is_not_part_of_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let p = token_path(dir.path());
    std::fs::write(&p, format!("  {T}\n")).unwrap();
    assert_eq!(read(&p).as_deref(), Some(T));
}

#[test]
fn malformed_contents_read_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let p = token_path(dir.path());
    for bad in ["", "hello", "sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "sk-ant-oat01-short", &format!("{T} extra")] {
        std::fs::write(&p, bad).unwrap();
        assert_eq!(read(&p), None, "{bad:?}");
    }
}

#[test]
fn a_symlink_is_neither_read_through_nor_written_through() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("elsewhere");
    std::fs::write(&target, T).unwrap();
    let p = token_path(dir.path());
    std::os::unix::fs::symlink(&target, &p).unwrap();
    assert_eq!(read(&p), None);
    assert!(write(&p, T).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), T);
}

#[test]
fn a_string_that_is_not_a_token_is_refused_by_write() {
    let dir = tempfile::tempdir().unwrap();
    let p = token_path(dir.path());
    assert_eq!(write(&p, "sk-ant-oat01-sh").unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    assert!(!p.exists());
}

#[test]
fn removing_an_absent_token_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let p = token_path(dir.path());
    remove(&p).unwrap();
    write(&p, T).unwrap();
    remove(&p).unwrap();
    assert!(!p.exists());
}
```

If the daemon's lib tests also compile on Windows, gate the unix-only tests (`#[cfg(unix)]`) the way `credentials.rs` tests are gated — copy whatever that file does.

- [ ] **Step 2: Run to see them fail:** `cargo test -p bondsymphonic-daemon --lib token::` → compile error (module missing).

- [ ] **Step 3: Implement `token.rs`.** Module doc: what the file is, why it exists (spec §1 in two sentences), that it never logs the token.

```rust
use std::path::{Path, PathBuf};

pub const TOKEN_FILE: &str = "claude-oauth-token";
const PREFIX: &str = "sk-ant-oat01-";

pub fn token_path(root: &Path) -> PathBuf {
    root.join(TOKEN_FILE)
}

pub fn is_token_shaped(s: &str) -> bool {
    (40..=512).contains(&s.len())
        && s.starts_with(PREFIX)
        && s[PREFIX.len()..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn read(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let t = body.trim();
    is_token_shaped(t).then(|| t.to_owned())
}

pub fn write(path: &Path, token: &str) -> std::io::Result<()> {
    if !is_token_shaped(token) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a setup-token token"));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "token path is a symlink"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::credentials::replace_private(path, token.as_bytes())
}

pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}
```

- [ ] **Step 4: Run token tests:** `cargo test -p bondsymphonic-daemon --lib token::` → PASS.

- [ ] **Step 5: Proto + setup table.** In `types.rs` add after `GhLogout`:
```rust
    /// Create a long-lived subscription token with `claude setup-token`. The
    /// daemon captures it from the terminal's output and gives it to agents
    /// as `CLAUDE_CODE_OAUTH_TOKEN`.
    ClaudeSetupToken,
```
In `setup.rs`: `ALL_ACTIONS: [SetupAction; 7]` with `SetupAction::ClaudeSetupToken` appended; argv row `SetupAction::ClaudeSetupToken => &["claude", "setup-token"],`. Add a test beside the existing argv tests:
```rust
#[test]
fn the_setup_token_action_runs_setup_token_and_nothing_else() {
    assert_eq!(setup_argv(SetupAction::ClaudeSetupToken), ["claude", "setup-token"]);
    assert!(ALL_ACTIONS.contains(&SetupAction::ClaudeSetupToken));
}
```
Also add a proto round-trip case for `claude_setup_token` wherever `request.rs`'s test lists `SystemSetupPty` (a second `SystemSetupPty` entry with `ClaudeSetupToken`), if that test enumerates by value.

- [ ] **Step 6: Logout removes the token.** In `handlers.rs`, `Request::SystemSetupPty(p)`, before `open_host`:
```rust
                // A logout takes away every way an agent authenticates, and the
                // long-lived token is one of them.
                if p.action == SetupAction::ClaudeLogout {
                    let path = crate::agents::token::token_path(&d.dirs.root);
                    match crate::agents::token::remove(&path) {
                        Ok(()) => tracing::info!(path = %path.display(), "long-lived claude token removed"),
                        Err(e) => tracing::warn!(path = %path.display(), "long-lived claude token not removed: {e}"),
                    }
                }
```
(Import `SetupAction` if needed.) Test: if `handlers.rs` has a test harness for `SystemSetupPty`, add "a Claude logout removes the token file" there; otherwise factor the block into `pub(crate) fn before_setup(root: &Path, action: SetupAction)` in `setup.rs` and unit-test that: token present + `ClaudeLogout` → gone; token present + `ClaudeLogin` → still there; absent + `ClaudeLogout` → no panic.

- [ ] **Step 7: Run the daemon lib tests** on Windows and in the distro (Global Constraints). Expected: pass (except the known Windows skip).
- [ ] **Step 8: Break-it:** delete the `ClaudeLogout` branch → the logout test fails; restore.
- [ ] **Step 9: Commit by path**
```bash
git commit -m "feat(daemon): a store for a long-lived Claude token, the setup-token action, and logout removes it" -- crates/daemon/src/agents/token.rs crates/daemon/src/agents/mod.rs crates/daemon/src/agents/credentials.rs crates/proto/src/types.rs crates/proto/src/request.rs crates/daemon/src/setup.rs crates/daemon/src/server/handlers.rs
```

---

### Task 2: Agents get the token; `claude_auth` passes on it (daemon)

**Files:**
- Modify: `crates/daemon/src/agents/mod.rs` (~line 872, the `env` block in `start`)
- Modify: `crates/daemon/src/prereqs.rs` (`claude_auth`, `check_all_with_backend`, `check_all`)
- Modify: `crates/daemon/src/server/handlers.rs` (~line 36, pass the token path)
- Modify: `crates/daemon/src/server/dispatch.rs` (~line 78, `check_all` call, if its signature changes)

**Interfaces:**
- Consumes: `token::{token_path, read}` from Task 1.
- Produces: `pub(crate) fn agent_auth_env(api_key: Option<&str>, token: Option<String>) -> Vec<(String, String)>` in `agents/mod.rs`; `check_all_with_backend(backend, token_path: Option<&Path>)`.

- [ ] **Step 1: Failing tests** in `agents/mod.rs`'s test module:
```rust
#[test]
fn an_api_key_the_ide_sent_wins_over_the_token() {
    let env = agent_auth_env(Some("sk-ant-api03-k"), Some("sk-ant-oat01-t".into()));
    assert_eq!(env, vec![("ANTHROPIC_API_KEY".to_owned(), "sk-ant-api03-k".to_owned())]);
}

#[test]
fn the_token_is_given_when_there_is_no_api_key() {
    for key in [None, Some("")] {
        let env = agent_auth_env(key, Some("sk-ant-oat01-t".into()));
        assert_eq!(env, vec![("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "sk-ant-oat01-t".to_owned())]);
    }
}

#[test]
fn nothing_is_given_when_there_is_neither() {
    assert!(agent_auth_env(None, None).is_empty());
}
```
And in `prereqs.rs`:
```rust
#[tokio::test]
async fn a_token_file_passes_claude_auth_without_asking_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let p = crate::agents::token::token_path(dir.path());
    crate::agents::token::write(&p, "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AA").unwrap();
    let s = claude_auth("definitely-not-a-real-binary-xyz", Some(&p)).await;
    assert!(s.ok, "{s:?}");
    assert_eq!(s.detail, "long-lived token");
}
```
(Adjust field names `ok`/`detail` to `PrereqStatus`'s actual ones.) Also a test that a malformed token file falls through to the existing path (with the nonexistent binary → the credentials-file fallback's answer, i.e. the detail is not "long-lived token").

- [ ] **Step 2: Run → fail** (functions missing / wrong arity).
- [ ] **Step 3: Implement.** In `agents/mod.rs` replace the `env` match with:
```rust
        // The key or token is given to this one command, never written into
        // the sandbox spec: the spec's environment reaches every process in
        // the workspace, including terminals the user opens. A key the IDE
        // sent is an explicit choice and wins; otherwise the long-lived token,
        // which never needs refreshing -- see `token.rs` for why that matters.
        let token = crate::agents::token::read(&crate::agents::token::token_path(&d.dirs.root));
        let env = agent_auth_env(p.options.api_key.as_deref(), token);
```
and the helper:
```rust
pub(crate) fn agent_auth_env(api_key: Option<&str>, token: Option<String>) -> Vec<(String, String)> {
    match (api_key.filter(|k| !k.is_empty()), token) {
        (Some(key), _) => vec![("ANTHROPIC_API_KEY".to_owned(), key.to_owned())],
        (None, Some(t)) => vec![("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), t)],
        (None, None) => vec![],
    }
}
```
Read the token off the runtime if the surrounding code does file I/O that way (`off_the_runtime` is used just above for the write-back — follow it). In `prereqs.rs`: `async fn claude_auth(claude_bin: &str, token: Option<&Path>) -> PrereqStatus` begins
```rust
    if token.and_then(crate::agents::token::read).is_some() {
        return status("claude_auth", true, "long-lived token", FIX);
    }
```
FIX becomes: ``"run `claude setup-token` (Setup → Set up long-lived token) or `claude auth login` in the distro, or set ANTHROPIC_API_KEY"``. `check_all_with_backend(backend, token: Option<&Path>)` passes it through; `handlers.rs` passes `Some(&crate::agents::token::token_path(&d.dirs.root))`; `check_all()` in `dispatch.rs` passes `None`.
- [ ] **Step 4: Run daemon lib tests** (Windows + distro) → pass.
- [ ] **Step 5: Break-it:** swap the first two match arms' order → precedence test fails; restore.
- [ ] **Step 6: Commit by path**
```bash
git commit -m "feat(daemon): agents authenticate with the long-lived token, and claude_auth passes on it" -- crates/daemon/src/agents/mod.rs crates/daemon/src/prereqs.rs crates/daemon/src/server/handlers.rs crates/daemon/src/server/dispatch.rs
```

---

### Task 3: Capture the token from the setup terminal (daemon) — GATED on the fixture

**Precondition:** the user has run `wsl -d bondsymphonic -u bs -- script -q -c "claude setup-token" /tmp/setup-token.raw` and completed the login. The controller (not the implementer) inspects the raw file **without printing the token** (e.g. `python3` that replaces every `sk-ant-oat01-[A-Za-z0-9_-]+` run and every run that continues it across line breaks with a synthetic token of the same length, then prints `repr()` of the region around it) and hands the implementer: (a) a sanitised fixture file, (b) a description of how the token is laid out (one line / wrapped at N cols / cursor-positioned / repeated in redraws). The controller also installs the real token with `token::write` semantics (0600) at `/home/bs/.bondsymphonic/claude-oauth-token` and deletes `/tmp/setup-token.raw`.

**Files:**
- Create: `crates/daemon/src/agents/token_scan.rs` (+ `pub mod token_scan;` in `agents/mod.rs`)
- Create: `crates/daemon/tests/fixtures/setup-token.sanitised.raw` (or under `src/agents/testdata/`, following the repo's existing fixture convention — check first)
- Modify: `crates/daemon/src/pty.rs` (`open_host`/`adopt` take an optional tap)
- Modify: `crates/daemon/src/server/handlers.rs` (`SystemSetupPty` passes a tap for `ClaudeSetupToken`)

**Interfaces:**
- Consumes: `token::{write, token_path, is_token_shaped}`.
- Produces:
  - `pub struct TokenScanner` with `pub fn new() -> Self`, `pub fn push(&mut self, bytes: &[u8]) -> Option<String>` (returns the token once, the first time a complete one is seen; `None` afterwards forever), `pub fn found(&self) -> bool`.
  - `pub type OutputTap = Box<dyn FnMut(&[u8]) + Send>;` in `pty.rs`; `open_host(d, argv, size, tap: Option<OutputTap>)`; `adopt(..., tap: Option<OutputTap>)`. Workspace `open` passes `None`.

**Scanner rules (baseline; adjust ONLY as the fixture requires):**
- Keep a pending text buffer of at most 4096 bytes after escape stripping; drop from the front beyond that.
- Strip SGR (`ESC [ … m`), OSC (`ESC ] … BEL` or `ESC ] … ESC \`), and other CSI sequences; decide from the fixture whether cursor-movement CSI and `\r\n` inside the token are wrap points (join) or separators. If the fixture shows the token on one unbroken line, every control sequence other than SGR is a separator.
- A candidate is only emitted when it is terminated (a separator follows it), so a token split across two reads is never emitted half-way.
- Emit only if `token::is_token_shaped`.

- [ ] **Step 1: Failing tests** in `token_scan.rs`:
  - the sanitised fixture fed in one push → yields exactly the synthetic token;
  - the same fixture fed in 7-byte chunks → same token, exactly once;
  - the fixture fed twice (redraw) → the second pass yields `None`;
  - ordinary `claude auth login` output (the URL line from `ide.log`, with colour codes) → `None`;
  - a token prefix at the end of a push with no terminator → `None` until the rest arrives;
  - a string of token alphabet that is too short → `None`.
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement `TokenScanner`** per the rules above.
- [ ] **Step 4: Wire the tap.** In `adopt`'s pump, `Ok(n) =>` branch: `if let Some(tap) = tap.as_mut() { tap(&buf[..n]); }` before publishing. In `handlers.rs`:
```rust
                let tap: Option<crate::pty::OutputTap> = (p.action == SetupAction::ClaudeSetupToken).then(|| {
                    let path = crate::agents::token::token_path(&d.dirs.root);
                    let mut scanner = crate::agents::token_scan::TokenScanner::new();
                    Box::new(move |bytes: &[u8]| {
                        if let Some(token) = scanner.push(bytes) {
                            match crate::agents::token::write(&path, &token) {
                                Ok(()) => tracing::info!(path = %path.display(), "long-lived claude token stored"),
                                Err(e) => tracing::warn!(path = %path.display(), "long-lived claude token not stored: {e}"),
                            }
                        }
                    }) as crate::pty::OutputTap
                });
```
(The write is a single small file; if the pump is on the runtime and the codebase insists on `off_the_runtime` for file I/O, spawn it via `tokio::task::spawn_blocking` instead.) At pump end, if the tap belonged to a setup-token terminal and nothing was stored, log `warn!("the setup-token terminal ended without a token being captured")` — implement by giving the closure a drop guard or by returning a flag from the scanner; keep it simple.
- [ ] **Step 5: Tests pass** (Windows + distro). Add a pump-level test only if `pty.rs` already has a test harness with a fake child; otherwise the scanner tests plus Step 6 suffice.
- [ ] **Step 6: Break-it:** make `push` emit on the first `is_token_shaped` prefix without waiting for a terminator → the chunked test fails; restore.
- [ ] **Step 7: Commit by path**
```bash
git commit -m "feat(daemon): the setup-token terminal's token is captured and stored" -- crates/daemon/src/agents/token_scan.rs crates/daemon/src/agents/mod.rs crates/daemon/src/pty.rs crates/daemon/src/server/handlers.rs <fixture path>
```

---

### Task 4: Setup row button, re-check on exit, user guide (IDE) — after Task 0

**Files:**
- Modify: `crates/ide/src/qobjects/app_controller.rs` (`parse_setup_action`: `"claude_setup_token" => Some(SetupAction::ClaudeSetupToken)`; the `login_pty` match at ~3015 adds `SetupAction::ClaudeSetupToken`; update the "six setup terminals" doc comment to seven)
- Modify: `crates/ide/cpp/SetupPage.cpp`, `crates/ide/cpp/SetupPage.h`
- Modify: `crates/ide/tests/qobject_smoke.rs` (and `smoke.rs` if the parse table is tested there)
- Modify: `docs/user-guide.md`

**Interfaces:**
- Consumes: `SetupAction::ClaudeSetupToken` (Task 1; merge order: Task 1 must be committed before Task 4 compiles — if Lane A is behind, Task 4 waits for Task 1's commit only).
- Produces: `SetupPage::tokenActionFor(const QString& name, bool ok, const QString& detail) -> QString` (static) returning `"claude_setup_token"` for `claude_auth` when `!ok`, or when `ok && detail != "long-lived token"`; empty otherwise. Button object name `bsClaudeSetupToken`, text `"Set up long-lived token"`, tooltip ``"Run `claude setup-token` in a terminal here; agents then use that token instead of your login"``.

- [ ] **Step 1: Failing tests** in `qobject_smoke.rs`, in the style of the existing `bsClaudeLogout` checks (SetupPage.cpp ~935/971 show how rows are fed and buttons found):
  - failing `claude_auth` row → both `Log in to Claude Code` and `bsClaudeSetupToken` present;
  - passing row with detail `"logged in to Claude"` → `bsClaudeLogout` and `bsClaudeSetupToken` present;
  - passing row with detail `"long-lived token"` → `bsClaudeLogout` present, `bsClaudeSetupToken` absent;
  - `gh_auth` rows never get it.
  - `parse_setup_action("claude_setup_token") == Some(SetupAction::ClaudeSetupToken)` (unit test in `app_controller.rs`'s tests or wherever the parse table is tested).
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement.** `buttonTextFor("claude_setup_token")` → `"Set up long-lived token"` (explicit branch, no fallthrough). In the row builder: on the passing branch, after the logout button, add the token button if `tokenActionFor(name, true, detail)` is non-empty; on the failing branch, after `addActionButton(action)`, the same with `ok=false`. Set object name + tooltip as specified. The existing `runAction` path, pending-action guard, and re-check on terminal exit apply unchanged — verify the re-check fires for this action as it does for logins (it is keyed on any setup terminal exiting; if it is keyed on specific actions, add this one).
- [ ] **Step 4: User guide.** Add a section after the existing Claude login section of `docs/user-guide.md`:
  - what it is (a one-year subscription token; agents use it instead of your login, so running several agents at once never logs any of them out);
  - how: Settings → Setup → Set up long-lived token, finish the sign-in in the browser; the daemon keeps it at `~/.bondsymphonic/claude-oauth-token` in the distro;
  - limits: inference-only scope (Remote Control and Claude in Chrome need a full login in a hand-run `claude`);
  - renew: run it again; remove: Log out of Claude Code (removes both).
- [ ] **Step 5: Run IDE tests** (`--lib`, `qobject_smoke`, `smoke`) → pass.
- [ ] **Step 6: Break-it:** make `tokenActionFor` always return empty → the smoke checks fail; restore.
- [ ] **Step 7: Commit by path**
```bash
git commit -m "feat(ide): Setup offers a long-lived Claude token" -- crates/ide/src/qobjects/app_controller.rs crates/ide/cpp/SetupPage.cpp crates/ide/cpp/SetupPage.h crates/ide/tests/qobject_smoke.rs crates/ide/tests/smoke.rs docs/user-guide.md
```

---

### Task 5: End-to-end check (controller, after all tasks)

- [ ] Build the daemon in the distro (`scripts/build-daemon.ps1`) and restart it.
- [ ] With the real token installed (Task 3 precondition), start an agent in a workspace whose `.credentials.json` copy is emptied; confirm it runs, and that `ide.log` shows no "OAuth session expired". This is spec §3.2.
- [ ] `grep` the daemon log and `ide.log` for `sk-ant-oat01` → no hits.
- [ ] Whole-branch review, then push.
