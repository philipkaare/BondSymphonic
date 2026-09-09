# Milestone 3: Editor and Changes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From the IDE, double-click a file in the Explorer to edit it with syntax highlighting and save it into the sandboxed worktree through the daemon; see the workspace's changed files with +/- counts and open any of them as a side-by-side diff; and have both the editor and the Changes list follow what agents write on disk.

**Architecture:** The daemon gains `workspace.changes` / `workspace.diff` (git against the merge-base, run through the pinned `worktree_git`) and `fs.watch` (a `notify` watcher per workspace, coalesced into `fs.changed` events). The IDE keeps all text state in Rust: `EditorBuffer` (a `ropey` rope + tree-sitter highlight spans) behind an `EditorDocument` QObject, `diff::align` (via `similar`) behind a `DiffDocument` QObject, and a `ChangesModel` QObject for the changed-files list. C++ adds a `CodeView` (QPlainTextEdit with gutter), `EditorWidget`, `RustHighlighter`, `EditorArea` (tabs), `DiffWidget`, and fills the Explorer's Changes tab. C++ paints and forwards input; every decision (what is dirty, which spans, how rows align, which events matter) is made in Rust.

**Tech Stack:** Rust 1.98 (MSVC), cxx-qt 0.10, Qt 6.9.2 Widgets, tokio, `ropey` 1, `tree-sitter` 0.25 + `tree-sitter-highlight` 0.25 + fifteen grammar crates, `similar` 2, `notify` 8 (daemon), git CLI.

**Spec:** `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§2 layout, §3 code structure, §6 editor, §7 diff view, §12 error presentation, §14 testing), `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§5.3 changes and diff, §11 file service, §13 testing), `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` (§6.3 methods, §6.4 events, §10 milestone 3).

## Global Constraints

- **Boundary rule (IDE spec §3):** `model/` and `client/` never import Qt. `qobjects/` are thin adapters that own a model struct. C++ files contain layout and painting only; any conditional logic beyond "which widget to show" belongs in Rust.
- **Paths (overview §6.3, daemon §11):** every `fs.*` and `workspace.diff` path is relative to the worktree root; anything escaping the root, including via symlink, is `InvalidParams`. Use `crate::fs::resolve` for containment before handing a path to git.
- **Read cap (daemon §11):** `fs.read_file` returns `encoding: "binary"` and no content for non-UTF-8 files, and `truncated: true` above 4 MiB. The editor opens either as a read-only notice (IDE spec §6).
- **Writes (daemon §11):** `fs.write_file` is atomic (temp + rename). Temp files are named `<name>.<pid>.<counter>.bs-tmp` (`crates/daemon/src/fs.rs:193`).
- **Watch (daemon §11):** `fs.watch` uses `notify` with 200 ms debouncing; emits relative paths; ignores `.git/`, `node_modules/`, `target/`. Event: `fs.changed {paths[]}` carrying `workspace_id`.
- **Changes (daemon §5.3):** `workspace.changes` = committed changes vs `<merge-base>` plus uncommitted, one list, status `added|modified|deleted|renamed|untracked`, with `additions`/`deletions`. `workspace.diff {path}` → `{base_text, work_text}`; base is `git show <merge-base>:<path>` (empty if absent), work is the working-tree text. The IDE computes the diff.
- **Git in the daemon (M2a rule):** run git for a workspace through `Layout::worktree_git()` from `lifecycle::layout_for`, never `daemon_git()` at the worktree path (the worktree is agent-writable). See `lifecycle::status` at `crates/daemon/src/workspace/lifecycle.rs:311`.
- **Languages (IDE spec §6):** Rust, JavaScript, TypeScript, TSX, Python, JSON, TOML, YAML, HTML, CSS, Markdown, Bash, C, C++, Go. Unknown extensions get no highlighting.
- **Style ids (IDE spec §6):** keyword, string, comment, function, type, number, constant, operator, punctuation, attribute, tag, property. Colours from `highlight/theme.rs`, light and dark.
- **Editor (IDE spec §6):** line numbers, current-line highlight, monospace font, tab width 4, no autocomplete, no folding. Ctrl+S saves through the daemon. `fs.changed` on an open unmodified file reloads silently; on a modified file a bar offers reload/keep.
- **Diff (IDE spec §7):** rows `{left_no?, left_text, right_no?, right_text, kind: Equal|Insert|Delete|Replace}`; two read-only editors with synchronised scrolling and row backgrounds (green/red/yellow tint); header with path and +/- counts; syntax highlighting on both sides.
- **Explorer (IDE spec §2):** Files tree with git status colouring; Changes list of files changed vs base with +/- counts; double-click opens in the editor (Files) or the diff view (Changes). The Merge/Rebase/Squash/PR/Discard buttons are Milestone 6.
- **Quality gates:** `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` clean; the C++ compiles with zero `warning C` lines; daemon suite green in WSL (`scripts\test-daemon.ps1`).
- **No desktop input injection, ever.** GUI verification uses an env-gated self-test hook (removed before commit), the app's own `QWidget::grab()` for screenshots, and the offscreen smoke test.
- **Environment:** PowerShell 5.1 (no `&&`). Source Qt before cargo on Windows: `. .\scripts\env.ps1`. WSL daemon tests: `.\scripts\test-daemon.ps1`. Full launcher: `.\launch.ps1`.
- **Commit trailers:** every commit ends with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01DcYV4WREYNvdE2MsWATepg`.

## File structure

Daemon:
- `crates/daemon/src/workspace/changes.rs` (new) — `changes()` and `diff()` plus the `name-status -z` / `numstat -z` parsers.
- `crates/daemon/src/fs_watch.rs` (new) — `Watchers`: one `notify` watcher per workspace, 200 ms coalescing, ignore rules, `fs.changed` publishing.
- `crates/daemon/src/daemon.rs`, `src/server/handlers.rs`, `src/lib.rs`, `src/workspace/lifecycle.rs` (destroy drops the watcher), `Cargo.toml`.
- Tests: `crates/daemon/tests/changes_integration.rs`, `crates/daemon/tests/fs_watch_integration.rs`.

IDE, Rust:
- `crates/ide/src/highlight/mod.rs`, `languages.rs`, `theme.rs` (new) — grammar registry, extension mapping, style palette.
- `crates/ide/src/model/editor_buffer.rs` (new) — rope + spans; `crates/ide/src/model/diff.rs` (new) — row alignment.
- `crates/ide/src/qobjects/editor_document.rs`, `diff_document.rs`, `changes_model.rs` (new); `app_controller.rs` gains `requestOpenFile`/`requestOpenDiff`/`requestSaveAll` signals; `smoke.rs` gains steps.
- Tests: `crates/ide/tests/editor_tests.rs`, `crates/ide/tests/diff_tests.rs` (new), `tests/smoke.rs` (extended).

IDE, C++:
- `crates/ide/cpp/CodeView.{h,cpp}` (new) — QPlainTextEdit with line-number gutter, current-line highlight, font, tab width; used by editor and diff.
- `crates/ide/cpp/RustHighlighter.{h,cpp}` (new) — QSyntaxHighlighter driven by a spans provider.
- `crates/ide/cpp/EditorWidget.{h,cpp}` (new) — one open file: CodeView + highlighter + notice/external-change bars.
- `crates/ide/cpp/EditorArea.{h,cpp}` (new) — tabbed editors and diffs.
- `crates/ide/cpp/DiffWidget.{h,cpp}` (new) — two CodeViews, synced scrolling, row tints, header.
- `crates/ide/cpp/ExplorerDock.{h,cpp}` — Changes tab, git-status colouring; `MainWindow.{h,cpp}`, `app.cpp`, `build.rs`.

---

### Task 1: Daemon `workspace.changes` and `workspace.diff`

**Files:**
- Create: `crates/daemon/src/workspace/changes.rs`, `crates/daemon/tests/changes_integration.rs`
- Modify: `crates/daemon/src/workspace/mod.rs` (add `pub mod changes;`), `crates/daemon/src/server/handlers.rs:49` (two new arms)

**Interfaces:**
- Consumes: `lifecycle::layout_for(d, &ws) -> Layout`, `Layout::worktree_git() -> Git`, `Git::run(cwd, args) -> Result<GitOutput{stdout, stderr}, RpcError>` (errors on non-zero exit via `git_error`), `crate::fs::{resolve, read_file}`, proto `ChangedFile`, `FileStatus`, `ChangesResult`, `DiffResult`, `WorkspaceDiffParams`.
- Produces: `pub async fn changes(d: &Daemon, id: &WorkspaceId) -> Result<ChangesResult, RpcError>`; `pub async fn diff(d: &Daemon, id: &WorkspaceId, path: &str) -> Result<DiffResult, RpcError>`; `pub fn parse_name_status_z(text: &str) -> Vec<(FileStatus, String)>`; `pub fn parse_numstat_z(text: &str) -> HashMap<String, (u32, u32)>`; `pub fn merge_changes(tracked: Vec<(FileStatus, String)>, counts: &HashMap<String,(u32,u32)>, untracked: Vec<(String, u32)>) -> Vec<ChangedFile>` (sorted by path).

- [ ] **Step 1: Write the failing unit tests** at the bottom of `crates/daemon/src/workspace/changes.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_status_z_including_renames() {
        // `git diff --name-status -M -z`: status\0path\0, renames R<score>\0old\0new\0
        let text = "M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0R100\0old.rs\0renamed.rs\0";
        let v = parse_name_status_z(text);
        assert_eq!(v, vec![
            (FileStatus::Modified, "src/a.rs".into()),
            (FileStatus::Added, "new.txt".into()),
            (FileStatus::Deleted, "gone.txt".into()),
            (FileStatus::Renamed, "renamed.rs".into()),
        ]);
    }

    #[test]
    fn parses_numstat_z_and_treats_binary_as_zero() {
        // `git diff --numstat -z`: add\tdel\tpath\0 ; binary is "-\t-"; renames add\tdel\t\0old\0new\0
        let text = "3\t1\tsrc/a.rs\0-\t-\timg.png\0";
        let m = parse_numstat_z(text);
        assert_eq!(m["src/a.rs"], (3, 1));
        assert_eq!(m["img.png"], (0, 0));
    }

    #[test]
    fn parses_numstat_z_rename_entry() {
        let text = "2\t0\t\0old.rs\0renamed.rs\0";
        let m = parse_numstat_z(text);
        assert_eq!(m["renamed.rs"], (2, 0));
        assert!(!m.contains_key("old.rs"));
    }

    #[test]
    fn merges_tracked_and_untracked_sorted_by_path() {
        let tracked = vec![(FileStatus::Modified, "b.rs".to_string())];
        let mut counts = HashMap::new();
        counts.insert("b.rs".to_string(), (4, 2));
        let untracked = vec![("a.txt".to_string(), 7)];
        let files = merge_changes(tracked, &counts, untracked);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].status, FileStatus::Untracked);
        assert_eq!((files[0].additions, files[0].deletions), (7, 0));
        assert_eq!(files[1].path, "b.rs");
        assert_eq!((files[1].additions, files[1].deletions), (4, 2));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run (PowerShell, from the repo root): `. .\scripts\env.ps1; cargo test -p bondsymphonic-daemon --lib changes`
Expected: compile error, `changes` module not found.

- [ ] **Step 3: Implement `crates/daemon/src/workspace/changes.rs`**

```rust
//! `workspace.changes` and `workspace.diff`: what a workspace changed relative
//! to the merge-base of its base branch, computed with git and read through the
//! pinned `worktree_git` so an agent-writable worktree cannot steer git.

use crate::daemon::Daemon;
use crate::workspace::lifecycle::layout_for;
use bondsymphonic_proto::{ChangedFile, ChangesResult, DiffResult, FileStatus, RpcError, WorkspaceId};
use std::collections::HashMap;

/// Changed files vs the merge-base, committed and uncommitted alike, one entry
/// per path, sorted by path.
pub async fn changes(d: &Daemon, id: &WorkspaceId) -> Result<ChangesResult, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    let base = git.run(&cwd, &["merge-base", &ws.base_branch, "HEAD"]).await?;
    let base = base.stdout.trim().to_string();

    let name_status = git.run(&cwd, &["diff", "--name-status", "-M", "-z", &base, "--"]).await?;
    let numstat = git.run(&cwd, &["diff", "--numstat", "-M", "-z", &base, "--"]).await?;
    let status = git
        .run(&cwd, &["status", "--porcelain=v2", "--untracked-files=all", "-z"])
        .await?;

    let tracked = parse_name_status_z(&name_status.stdout);
    let counts = parse_numstat_z(&numstat.stdout);
    let untracked_paths: Vec<String> = status
        .stdout
        .split('\0')
        .filter_map(|line| line.strip_prefix("? ").map(str::to_owned))
        .collect();
    let root = cwd.clone();
    let untracked = tokio::task::spawn_blocking(move || {
        untracked_paths
            .into_iter()
            .map(|p| {
                let lines = crate::fs::read_file(&root, &p)
                    .map(|r| if r.encoding == "binary" { 0 } else { r.content.lines().count() as u32 })
                    .unwrap_or(0);
                (p, lines)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;

    Ok(ChangesResult { files: merge_changes(tracked, &counts, untracked) })
}

/// Base text (merge-base version, empty if the path did not exist there) and
/// working-tree text (empty if deleted or binary) for one repo-relative path.
pub async fn diff(d: &Daemon, id: &WorkspaceId, path: &str) -> Result<DiffResult, RpcError> {
    let ws = d.workspace(id)?;
    // Containment first: git would happily show any path in the object store.
    crate::fs::resolve(&ws.worktree_path, path)?;
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    let base = git.run(&cwd, &["merge-base", &ws.base_branch, "HEAD"]).await?;
    let spec = format!("{}:{}", base.stdout.trim(), path);
    // A missing path at the merge-base is a normal "added" file, not an error.
    let base_text = match git.run(&cwd, &["show", &spec]).await {
        Ok(out) => out.stdout,
        Err(_) => String::new(),
    };
    let root = cwd.clone();
    let rel = path.to_owned();
    let work_text = tokio::task::spawn_blocking(move || match crate::fs::read_file(&root, &rel) {
        Ok(r) if r.encoding != "binary" => r.content,
        _ => String::new(),
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(DiffResult { base_text, work_text })
}

/// `git diff --name-status -M -z` → (status, path). Renames yield the new path.
pub fn parse_name_status_z(text: &str) -> Vec<(FileStatus, String)> {
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let Some(code) = fields.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Added,
            _ => FileStatus::Modified,
        };
        let Some(first) = fields.next() else { break };
        let path = if matches!(status, FileStatus::Renamed) || code.starts_with('C') {
            match fields.next() {
                Some(new) => new.to_owned(),
                None => break,
            }
        } else {
            first.to_owned()
        };
        out.push((status, path));
    }
    out
}

/// `git diff --numstat -M -z` → path → (additions, deletions). Binary files
/// count as (0, 0); renames are keyed by the new path.
pub fn parse_numstat_z(text: &str) -> HashMap<String, (u32, u32)> {
    let mut map = HashMap::new();
    let mut fields = text.split('\0');
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut cols = record.splitn(3, '\t');
        let add = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let del = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let path = match cols.next() {
            Some("") | None => {
                // Rename: the path columns follow as two extra NUL-separated fields.
                let _old = fields.next();
                match fields.next() {
                    Some(new) => new.to_owned(),
                    None => break,
                }
            }
            Some(p) => p.to_owned(),
        };
        map.insert(path, (add, del));
    }
    map
}

pub fn merge_changes(
    tracked: Vec<(FileStatus, String)>,
    counts: &HashMap<String, (u32, u32)>,
    untracked: Vec<(String, u32)>,
) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = tracked
        .into_iter()
        .map(|(status, path)| {
            let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
            ChangedFile { path, status, additions, deletions }
        })
        .collect();
    files.extend(untracked.into_iter().map(|(path, lines)| ChangedFile {
        path,
        status: FileStatus::Untracked,
        additions: lines,
        deletions: 0,
    }));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    files
}
```

Add `pub mod changes;` to `crates/daemon/src/workspace/mod.rs`. In `handlers.rs`, after the `WorkspaceStatus` arm:

```rust
            Request::WorkspaceChanges(p) => ok(changes::changes(d, &p.workspace_id).await?),
            Request::WorkspaceDiff(p) => ok(changes::diff(d, &p.workspace_id, &p.path).await?),
```

with `use crate::workspace::changes;` at the top. Check `Workspace` has `base_branch` (it is serialised in `WorkspaceInfo`; see `crates/daemon/src/workspace/mod.rs:9`). If `Git::run` treats a non-zero exit as `Err` (it does, via `git_error`), the `git show` fallback above is correct; confirm by reading `crates/daemon/src/git/mod.rs:43-78`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p bondsymphonic-daemon --lib changes`
Expected: 4 passed.

- [ ] **Step 5: Write the integration test** `crates/daemon/tests/changes_integration.rs` (runs on Windows too, over the noop backend, like `workspace_integration.rs`; copy its `create_ws` and `commit_in_worktree` helpers or move them into `tests/common/mod.rs` if both files need them):

```rust
mod common;

use bondsymphonic_proto::*;
use common::{init_repo, start_daemon, Client};

#[tokio::test]
async fn changes_and_diff_report_committed_uncommitted_and_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;
    let wt = std::path::Path::new(&ws.worktree_path);

    // Committed change inside the worktree, an uncommitted edit, and a new file.
    std::fs::write(wt.join("README.md"), "hello\nworld\n").unwrap();
    commit_in_worktree(wt, &sandbox_env(&ws), "README.md");
    std::fs::write(wt.join("README.md"), "hello\nworld\nagain\n").unwrap();
    std::fs::write(wt.join("notes.txt"), "a\nb\nc\n").unwrap();

    let v = c.call(Request::WorkspaceChanges(WorkspaceIdParams { workspace_id: ws.id.clone() })).await.unwrap();
    let res: ChangesResult = serde_json::from_value(v).unwrap();
    let readme = res.files.iter().find(|f| f.path == "README.md").expect("README listed");
    assert_eq!(readme.status, FileStatus::Modified);
    assert_eq!((readme.additions, readme.deletions), (2, 0));
    let notes = res.files.iter().find(|f| f.path == "notes.txt").expect("notes listed");
    assert_eq!(notes.status, FileStatus::Untracked);
    assert_eq!((notes.additions, notes.deletions), (3, 0));

    let v = c.call(Request::WorkspaceDiff(WorkspaceDiffParams { workspace_id: ws.id.clone(), path: "README.md".into() })).await.unwrap();
    let d: DiffResult = serde_json::from_value(v).unwrap();
    assert_eq!(d.base_text, "hello\n");
    assert_eq!(d.work_text, "hello\nworld\nagain\n");

    let v = c.call(Request::WorkspaceDiff(WorkspaceDiffParams { workspace_id: ws.id.clone(), path: "notes.txt".into() })).await.unwrap();
    let d: DiffResult = serde_json::from_value(v).unwrap();
    assert_eq!(d.base_text, "");
    assert_eq!(d.work_text, "a\nb\nc\n");

    let err = c.call(Request::WorkspaceDiff(WorkspaceDiffParams { workspace_id: ws.id.clone(), path: "../outside".into() })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);

    cancel.cancel();
}
```

`sandbox_env(&ws)` is whatever `workspace_integration.rs` passes to `commit_in_worktree` (read lines 10-118 there and reuse the exact construction; the worktree git needs `GIT_DIR`/`GIT_COMMON_DIR`/`GIT_WORK_TREE` from the layout, obtainable from `Layout::sandbox_git_env()` through the `_daemon` handle: `lifecycle::layout_for(&daemon, &daemon.workspace(&ws.id).unwrap()).await.unwrap().sandbox_git_env()`).

- [ ] **Step 6: Run the integration test on Windows and in WSL**

Run: `cargo test -p bondsymphonic-daemon --test changes_integration` then `.\scripts\test-daemon.ps1`
Expected: passing on both.

- [ ] **Step 7: Clippy, fmt, commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --all
git add crates/daemon
git commit -m "feat(daemon): workspace.changes and workspace.diff against the merge-base"
```

---

### Task 2: Daemon `fs.watch` with coalesced `fs.changed` events

**Files:**
- Create: `crates/daemon/src/fs_watch.rs`, `crates/daemon/tests/fs_watch_integration.rs`
- Modify: `crates/daemon/Cargo.toml` (add `notify = "8"`), `crates/daemon/src/lib.rs` (`pub mod fs_watch;`), `crates/daemon/src/daemon.rs:18-46` (field + constructor), `crates/daemon/src/server/handlers.rs` (replace the "`fs.watch` stays not-implemented" comment with an arm), `crates/daemon/src/workspace/lifecycle.rs:224` (`destroy` calls `d.watchers.disable(id)` first)

**Interfaces:**
- Consumes: `EventBus::publish(Some(workspace_id), Event::FsChanged { paths })`, `Daemon::workspace(id)`.
- Produces: `pub struct Watchers` (`Default`), `pub fn enable(&self, id: WorkspaceId, root: PathBuf, events: EventBus) -> Result<(), RpcError>` (idempotent), `pub fn disable(&self, id: &WorkspaceId)`; `pub fn relative_paths(root: &Path, paths: &[PathBuf]) -> Vec<String>` (filters ignored prefixes and `.bs-tmp` temp files, dedups, sorts); `pub const DEBOUNCE: Duration = 200 ms`.

- [ ] **Step 1: Write the failing unit test** in `fs_watch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn relative_paths_filter_ignored_dirs_temp_files_and_dedup() {
        let root = PathBuf::from("/wt");
        let input = vec![
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/.git/index"),
            PathBuf::from("/wt/node_modules/x/y.js"),
            PathBuf::from("/wt/target/debug/a"),
            PathBuf::from("/wt/notes.txt.123.7.bs-tmp"),
            PathBuf::from("/elsewhere/z"),
            PathBuf::from("/wt/README.md"),
        ];
        assert_eq!(relative_paths(&root, &input), vec!["README.md".to_string(), "src/main.rs".to_string()]);
    }
}
```

- [ ] **Step 2: Run to verify it fails**: `cargo test -p bondsymphonic-daemon --lib fs_watch` → module not found.

- [ ] **Step 3: Implement `crates/daemon/src/fs_watch.rs`**

```rust
//! `fs.watch`: one `notify` watcher per workspace worktree. Raw events are
//! coalesced for [`DEBOUNCE`] after the first one, converted to sorted,
//! de-duplicated repo-relative paths, and published as a single `fs.changed`.

use crate::server::broadcast::EventBus;
use bondsymphonic_proto::{Event, RpcError, WorkspaceId};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEBOUNCE: Duration = Duration::from_millis(200);
const IGNORED: [&str; 3] = [".git", "node_modules", "target"];

struct Active {
    _watcher: RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct Watchers {
    active: Mutex<HashMap<WorkspaceId, Active>>,
}

impl Watchers {
    pub fn enable(&self, id: WorkspaceId, root: PathBuf, events: EventBus) -> Result<(), RpcError> {
        let mut active = self.active.lock();
        if active.contains_key(&id) {
            return Ok(());
        }
        let (tx, rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = tx.send(ev.paths);
            }
        })
        .map_err(|e| RpcError::internal(format!("fs.watch: {e}")))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
        let task = tokio::spawn(coalesce(id.clone(), root, rx, events));
        active.insert(id, Active { _watcher: watcher, task });
        Ok(())
    }

    pub fn disable(&self, id: &WorkspaceId) {
        if let Some(a) = self.active.lock().remove(id) {
            a.task.abort();
        }
    }
}

async fn coalesce(id: WorkspaceId, root: PathBuf, mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>, events: EventBus) {
    while let Some(first) = rx.recv().await {
        let mut batch = first;
        let deadline = tokio::time::sleep(DEBOUNCE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                more = rx.recv() => match more {
                    Some(paths) => batch.extend(paths),
                    None => break,
                },
            }
        }
        let paths = relative_paths(&root, &batch);
        if !paths.is_empty() {
            events.publish(Some(id.clone()), Event::FsChanged { paths });
        }
    }
}

/// Repo-relative, forward-slash paths under `root`, minus ignored directories
/// and the daemon's own `.bs-tmp` write temporaries; sorted and de-duplicated.
pub fn relative_paths(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .filter(|rel| {
            let first = rel.components().next().and_then(|c| c.as_os_str().to_str()).unwrap_or("");
            !IGNORED.contains(&first)
        })
        .filter(|rel| !rel.to_string_lossy().ends_with(".bs-tmp"))
        .map(|rel| rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}
```

Wire it: `Daemon` gets `pub watchers: fs_watch::Watchers` (constructed with `Default::default()` in `Daemon::new`); handler arm

```rust
            Request::FsWatch(p) => {
                if p.enable {
                    let root = d.workspace(&p.workspace_id)?.worktree_path.clone().into();
                    d.watchers.enable(p.workspace_id.clone(), root, d.events.clone())?;
                } else {
                    d.watchers.disable(&p.workspace_id);
                }
                ok(Empty {})
            }
```

(`worktree_path` is a `String` or `PathBuf`; adapt `.into()` accordingly.) `lifecycle::destroy` calls `d.watchers.disable(id)` before removing the worktree so notify does not report the teardown. If `EventBus` is not `Clone`, wrap the field in `Arc` the way `sandboxes` does; the test at `server_integration.rs` shows how events are published today.

- [ ] **Step 4: Run the unit test**: `cargo test -p bondsymphonic-daemon --lib fs_watch` → 1 passed.

- [ ] **Step 5: Write the integration test** `crates/daemon/tests/fs_watch_integration.rs`:

```rust
mod common;

use bondsymphonic_proto::*;
use common::{init_repo, start_daemon, Client};
use std::time::{Duration, Instant};

async fn wait_for_fs_changed(c: &mut Client, ws: &WorkspaceId, limit: Duration) -> Option<Vec<String>> {
    let start = Instant::now();
    while start.elapsed() < limit {
        for (id, ev) in c.drain_events() {
            if let (Some(id), Event::FsChanged { paths }) = (id, ev) {
                if &id == ws { return Some(paths); }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Nudge the reader: any cheap request pumps pending events into the client.
        let _ = c.call(Request::WorkspaceList {}).await;
    }
    None
}

#[tokio::test]
async fn watch_reports_relative_paths_debounced_and_ignores_target_dir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path());
    let (port, token, _daemon, cancel) = start_daemon(dir.path()).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_ws(&mut c, &repo, "alpha").await;

    c.call(Request::FsWatch(FsWatchParams { workspace_id: ws.id.clone(), enable: true })).await.unwrap();
    c.call(Request::FsWriteFile(FsWriteParams { workspace_id: ws.id.clone(), path: "hello.txt".into(), content: "hi\n".into() })).await.unwrap();
    let paths = wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(5)).await.expect("fs.changed arrives");
    assert_eq!(paths, vec!["hello.txt".to_string()], "relative path, no temp file");

    std::fs::create_dir_all(std::path::Path::new(&ws.worktree_path).join("target")).unwrap();
    std::fs::write(std::path::Path::new(&ws.worktree_path).join("target/out.bin"), b"x").unwrap();
    assert!(wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1)).await.is_none(), "target/ is ignored");

    c.call(Request::FsWatch(FsWatchParams { workspace_id: ws.id.clone(), enable: false })).await.unwrap();
    c.call(Request::FsWriteFile(FsWriteParams { workspace_id: ws.id.clone(), path: "after.txt".into(), content: "x\n".into() })).await.unwrap();
    assert!(wait_for_fs_changed(&mut c, &ws.id, Duration::from_secs(1)).await.is_none(), "disabled watch is silent");

    cancel.cancel();
}
```

If `Client::drain_events` only surfaces events read while awaiting a response (check `tests/common/mod.rs:88-133`), the `WorkspaceList` nudge above keeps it honest; otherwise remove the nudge.

- [ ] **Step 6: Run it on Windows and in WSL**: `cargo test -p bondsymphonic-daemon --test fs_watch_integration`; `.\scripts\test-daemon.ps1`. Expected: passing on both (notify uses ReadDirectoryChangesW on Windows and inotify in WSL).

- [ ] **Step 7: Clippy, fmt, commit**

```bash
git add crates/daemon Cargo.lock
git commit -m "feat(daemon): fs.watch with coalesced fs.changed events per workspace"
```

---

### Task 3: Highlight registry, theme, and `EditorBuffer`

**Files:**
- Create: `crates/ide/src/highlight/mod.rs`, `crates/ide/src/highlight/languages.rs`, `crates/ide/src/highlight/theme.rs`, `crates/ide/src/model/editor_buffer.rs`, `crates/ide/tests/editor_tests.rs`
- Modify: `crates/ide/Cargo.toml` (deps below), `crates/ide/src/lib.rs` (`pub mod highlight;`), `crates/ide/src/model/mod.rs` (`pub mod editor_buffer;`)

**Dependencies to add** (`crates/ide/Cargo.toml`):

```toml
ropey = "1"
tree-sitter = "0.25"
tree-sitter-highlight = "0.25"
tree-sitter-rust = "0.24"
tree-sitter-javascript = "0.25"
tree-sitter-typescript = "0.23"
tree-sitter-python = "0.25"
tree-sitter-json = "0.24"
tree-sitter-toml-ng = "0.7"
tree-sitter-yaml = "0.7"
tree-sitter-html = "0.23"
tree-sitter-css = "0.25"
tree-sitter-md = "0.5"
tree-sitter-bash = "0.25"
tree-sitter-c = "0.24"
tree-sitter-cpp = "0.23"
tree-sitter-go = "0.25"
```

If a grammar crate's `tree-sitter-language` requirement conflicts with `tree-sitter 0.25`, bump `tree-sitter`/`tree-sitter-highlight` together to the version the grammars agree on (0.26 or 0.27) and note it in the report. Each grammar crate exposes `LANGUAGE` (a `LanguageFn`) and `HIGHLIGHTS_QUERY` (some also `INJECTIONS_QUERY`/`LOCALS_QUERY`; TypeScript exposes `LANGUAGE_TYPESCRIPT`/`LANGUAGE_TSX`, Markdown exposes `LANGUAGE` for the block grammar plus `HIGHLIGHT_QUERY_BLOCK`). Read each crate's `lib.rs` for the exact names.

**Interfaces:**
- Produces (`highlight/languages.rs`): `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Language { Rust, JavaScript, TypeScript, Tsx, Python, Json, Toml, Yaml, Html, Css, Markdown, Bash, C, Cpp, Go }`; `impl Language { pub fn from_path(path: &str) -> Option<Language>; pub fn name(self) -> &'static str /* "rust", "javascript", ... */; pub fn config(self) -> &'static HighlightConfiguration }` (built once per language in a `OnceLock`, `configure(STYLE_NAMES)`).
- Produces (`highlight/theme.rs`): `#[derive(Clone, Copy, Debug, PartialEq, Eq)] #[repr(u8)] pub enum StyleId { Keyword, String, Comment, Function, Type, Number, Constant, Operator, Punctuation, Attribute, Tag, Property }`; `pub const STYLE_NAMES: [&str; 12]` in the same order (`"keyword", "string", "comment", "function", "type", "number", "constant", "operator", "punctuation", "attribute", "tag", "property"`); `impl StyleId { pub fn from_index(i: usize) -> Option<StyleId> }`; `#[derive(Clone, Copy)] pub struct Style { pub fg: &'static str, pub bold: bool, pub italic: bool }`; `pub struct Theme { .. }` with `pub fn light() -> &'static Theme`, `pub fn dark() -> &'static Theme`, `pub fn for_dark(dark: bool) -> &'static Theme`, `pub fn style(&self, id: StyleId) -> Style`.
- Produces (`model/editor_buffer.rs`): `#[derive(Clone, Debug, PartialEq, Eq)] pub struct Span { pub start: usize /* char column */, pub len: usize /* chars */, pub style: StyleId }`; `pub struct EditorBuffer { .. }` with `pub fn new(path: &str, text: &str) -> EditorBuffer`, `pub fn language(&self) -> Option<Language>`, `pub fn text(&self) -> String`, `pub fn line_count(&self) -> usize`, `pub fn line(&self, n: usize) -> String` (without the newline), `pub fn utf16_to_char(&self, utf16: usize) -> usize`, `pub fn apply_edit(&mut self, char_pos: usize, removed_chars: usize, inserted: &str) -> (usize, usize)` (returns the inclusive range of lines whose spans changed compared with before the edit, or `(line, line)` when only that line changed), `pub fn replace_all(&mut self, text: &str)`, `pub fn spans_for_line(&mut self, n: usize) -> &[Span]`, `pub fn spans_json(&mut self, n: usize, theme: &Theme) -> String` producing `[{"s":0,"l":2,"fg":"#a626a4","b":true,"i":false}, ...]`.

- [ ] **Step 1: Write the failing tests** `crates/ide/tests/editor_tests.rs`:

```rust
use bondsymphonic_ide::highlight::languages::Language;
use bondsymphonic_ide::highlight::theme::{StyleId, Theme};
use bondsymphonic_ide::model::editor_buffer::{EditorBuffer, Span};

#[test]
fn language_is_detected_from_the_extension() {
    assert_eq!(Language::from_path("src/main.rs"), Some(Language::Rust));
    assert_eq!(Language::from_path("web/app.tsx"), Some(Language::Tsx));
    assert_eq!(Language::from_path("a/b/c.yml"), Some(Language::Yaml));
    assert_eq!(Language::from_path("include/x.hpp"), Some(Language::Cpp));
    assert_eq!(Language::from_path("Makefile"), None);
    assert_eq!(Language::from_path("notes.unknownext"), None);
}

#[test]
fn rust_keywords_strings_and_comments_get_spans_per_line() {
    let src = "fn main() {\n    let s = \"hi\"; // greet\n}\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    assert_eq!(buf.language(), Some(Language::Rust));
    assert_eq!(buf.line_count(), 4);
    let l0 = buf.spans_for_line(0).to_vec();
    assert!(l0.contains(&Span { start: 0, len: 2, style: StyleId::Keyword }), "{l0:?}");
    let l1 = buf.spans_for_line(1).to_vec();
    assert!(l1.iter().any(|s| s.style == StyleId::Keyword && s.start == 4 && s.len == 3), "let: {l1:?}");
    assert!(l1.iter().any(|s| s.style == StyleId::String && s.start == 12 && s.len == 4), "\"hi\": {l1:?}");
    assert!(l1.iter().any(|s| s.style == StyleId::Comment && s.start == 18), "comment: {l1:?}");
    assert!(buf.spans_for_line(3).is_empty());
}

#[test]
fn multi_line_comment_spans_are_split_per_line() {
    let src = "/* a\n b */ fn f() {}\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    let l0 = buf.spans_for_line(0).to_vec();
    let l1 = buf.spans_for_line(1).to_vec();
    assert_eq!(l0, vec![Span { start: 0, len: 4, style: StyleId::Comment }]);
    assert_eq!(l1[0], Span { start: 0, len: 5, style: StyleId::Comment });
    assert!(l1.iter().any(|s| s.style == StyleId::Keyword && s.start == 6));
}

#[test]
fn an_edit_keeps_spans_identical_to_a_fresh_parse() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\n");
    // Insert a string literal into the second line: "fn b() { \"x\" }".
    let pos = buf.text().find("b() {}").unwrap() + "b() {".len();
    let (from, to) = buf.apply_edit(pos, 0, " \"x\" ");
    assert_eq!((from, to), (1, 1));
    let fresh = EditorBuffer::new("x.rs", &buf.text());
    let mut fresh = fresh;
    for n in 0..buf.line_count() {
        assert_eq!(buf.spans_for_line(n), fresh.spans_for_line(n), "line {n}");
    }
}

#[test]
fn opening_a_block_comment_reports_every_affected_line() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let (from, to) = buf.apply_edit(0, 0, "/* ");
    assert_eq!(from, 0);
    assert!(to >= 2, "lines 1 and 2 turned into comment text, got to={to}");
}

#[test]
fn utf16_offsets_map_to_char_offsets() {
    let buf = EditorBuffer::new("x.txt", "a😀b");
    assert_eq!(buf.utf16_to_char(0), 0);
    assert_eq!(buf.utf16_to_char(1), 1);
    assert_eq!(buf.utf16_to_char(3), 2); // after the surrogate pair
}

#[test]
fn unknown_language_has_no_spans_and_spans_json_is_well_formed() {
    let mut buf = EditorBuffer::new("x.unknown", "let x = 1;\n");
    assert!(buf.spans_for_line(0).is_empty());
    assert_eq!(buf.spans_json(0, Theme::light()), "[]");
    let mut rs = EditorBuffer::new("x.rs", "let x = 1;\n");
    let json = rs.spans_json(0, Theme::dark());
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let first = &v.as_array().unwrap()[0];
    assert_eq!(first["s"], 0);
    assert_eq!(first["l"], 3);
    assert!(first["fg"].as_str().unwrap().starts_with('#'));
    assert!(first["b"].is_boolean() && first["i"].is_boolean());
}
```

- [ ] **Step 2: Run to verify they fail**: `cargo test -p bondsymphonic-ide --test editor_tests` → unresolved imports.

- [ ] **Step 3: Implement `highlight/theme.rs`**

```rust
//! Style ids the highlighter emits and the two palettes that colour them.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StyleId {
    Keyword, String, Comment, Function, Type, Number, Constant, Operator, Punctuation, Attribute, Tag, Property,
}

/// Capture names handed to `HighlightConfiguration::configure`, in `StyleId`
/// order. tree-sitter matches dotted capture names by prefix, so
/// `keyword.control` resolves to `keyword`.
pub const STYLE_NAMES: [&str; 12] = [
    "keyword", "string", "comment", "function", "type", "number", "constant", "operator", "punctuation", "attribute", "tag", "property",
];

impl StyleId {
    pub fn from_index(i: usize) -> Option<StyleId> {
        use StyleId::*;
        [Keyword, String, Comment, Function, Type, Number, Constant, Operator, Punctuation, Attribute, Tag, Property].get(i).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style { pub fg: &'static str, pub bold: bool, pub italic: bool }

pub struct Theme { styles: [Style; 12] }

const fn s(fg: &'static str, bold: bool, italic: bool) -> Style { Style { fg, bold, italic } }

static LIGHT: Theme = Theme { styles: [
    s("#a626a4", true, false),  // keyword
    s("#50a14f", false, false), // string
    s("#a0a1a7", false, true),  // comment
    s("#4078f2", false, false), // function
    s("#c18401", false, false), // type
    s("#986801", false, false), // number
    s("#986801", false, false), // constant
    s("#0184bc", false, false), // operator
    s("#383a42", false, false), // punctuation
    s("#e45649", false, false), // attribute
    s("#e45649", false, false), // tag
    s("#e45649", false, false), // property
]};

static DARK: Theme = Theme { styles: [
    s("#c678dd", true, false), s("#98c379", false, false), s("#5c6370", false, true), s("#61afef", false, false),
    s("#e5c07b", false, false), s("#d19a66", false, false), s("#d19a66", false, false), s("#56b6c2", false, false),
    s("#abb2bf", false, false), s("#e06c75", false, false), s("#e06c75", false, false), s("#e06c75", false, false),
]};

impl Theme {
    pub fn light() -> &'static Theme { &LIGHT }
    pub fn dark() -> &'static Theme { &DARK }
    pub fn for_dark(dark: bool) -> &'static Theme { if dark { &DARK } else { &LIGHT } }
    pub fn style(&self, id: StyleId) -> Style { self.styles[id as usize] }
}
```

- [ ] **Step 4: Implement `highlight/languages.rs`**

```rust
//! Grammar registry: file extension → language → a configured tree-sitter
//! highlight configuration, built once per language on first use.

use crate::highlight::theme::STYLE_NAMES;
use std::sync::OnceLock;
use tree_sitter_highlight::HighlightConfiguration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language { Rust, JavaScript, TypeScript, Tsx, Python, Json, Toml, Yaml, Html, Css, Markdown, Bash, C, Cpp, Go }

const ALL: [Language; 15] = [
    Language::Rust, Language::JavaScript, Language::TypeScript, Language::Tsx, Language::Python, Language::Json,
    Language::Toml, Language::Yaml, Language::Html, Language::Css, Language::Markdown, Language::Bash,
    Language::C, Language::Cpp, Language::Go,
];

impl Language {
    pub fn from_path(path: &str) -> Option<Language> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "py" | "pyi" => Language::Python,
            "json" | "jsonc" => Language::Json,
            "toml" => Language::Toml,
            "yaml" | "yml" => Language::Yaml,
            "html" | "htm" => Language::Html,
            "css" => Language::Css,
            "md" | "markdown" => Language::Markdown,
            "sh" | "bash" | "zsh" => Language::Bash,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "go" => Language::Go,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust", Language::JavaScript => "javascript", Language::TypeScript => "typescript",
            Language::Tsx => "tsx", Language::Python => "python", Language::Json => "json", Language::Toml => "toml",
            Language::Yaml => "yaml", Language::Html => "html", Language::Css => "css", Language::Markdown => "markdown",
            Language::Bash => "bash", Language::C => "c", Language::Cpp => "cpp", Language::Go => "go",
        }
    }

    /// The configured highlighter for this language, built on first use.
    pub fn config(self) -> &'static HighlightConfiguration {
        static CONFIGS: OnceLock<Vec<OnceLock<HighlightConfiguration>>> = OnceLock::new();
        let slots = CONFIGS.get_or_init(|| ALL.iter().map(|_| OnceLock::new()).collect());
        let idx = ALL.iter().position(|l| *l == self).expect("language listed in ALL");
        slots[idx].get_or_init(|| {
            let mut cfg = build(self).expect("bundled grammar and query compile");
            cfg.configure(&STYLE_NAMES);
            cfg
        })
    }
}

fn build(lang: Language) -> Result<HighlightConfiguration, tree_sitter_highlight::Error> {
    // (language, highlights query, injections query, locals query)
    let (language, highlights, injections, locals): (tree_sitter::Language, &str, &str, &str) = match lang {
        Language::Rust => (tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY, tree_sitter_rust::INJECTIONS_QUERY, ""),
        Language::JavaScript => (tree_sitter_javascript::LANGUAGE.into(), tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::INJECTIONS_QUERY, tree_sitter_javascript::LOCALS_QUERY),
        Language::TypeScript => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), tree_sitter_typescript::HIGHLIGHTS_QUERY, "", tree_sitter_typescript::LOCALS_QUERY),
        Language::Tsx => (tree_sitter_typescript::LANGUAGE_TSX.into(), tree_sitter_typescript::HIGHLIGHTS_QUERY, "", tree_sitter_typescript::LOCALS_QUERY),
        Language::Python => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY, "", ""),
        Language::Json => (tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY, "", ""),
        Language::Toml => (tree_sitter_toml_ng::LANGUAGE.into(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY, "", ""),
        Language::Yaml => (tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY, "", ""),
        Language::Html => (tree_sitter_html::LANGUAGE.into(), tree_sitter_html::HIGHLIGHTS_QUERY, tree_sitter_html::INJECTIONS_QUERY, ""),
        Language::Css => (tree_sitter_css::LANGUAGE.into(), tree_sitter_css::HIGHLIGHTS_QUERY, "", ""),
        Language::Markdown => (tree_sitter_md::LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK, tree_sitter_md::INJECTION_QUERY_BLOCK, ""),
        Language::Bash => (tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY, "", ""),
        Language::C => (tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY, "", ""),
        Language::Cpp => (tree_sitter_cpp::LANGUAGE.into(), tree_sitter_cpp::HIGHLIGHT_QUERY, "", ""),
        Language::Go => (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY, "", ""),
    };
    HighlightConfiguration::new(language, lang.name(), highlights, injections, locals)
}
```

The constant names above are the ones the crates used at the time of writing; if `cargo build` says a name does not exist, open the crate's source under `~/.cargo/registry/src/*/tree-sitter-<x>-*/bindings/rust/lib.rs` and use the name it exports. Injections are passed with `None` language lookup at highlight time, so a query referencing another language simply highlights nothing for the injected region. JavaScript's highlight query is often needed by TypeScript's (`tree-sitter-typescript` documents concatenating `tree_sitter_javascript::HIGHLIGHT_QUERY` first); do that if `fn`/`const` in a `.ts` file come back without spans in the tests you add.

`highlight/mod.rs`:

```rust
pub mod languages;
pub mod theme;
```

- [ ] **Step 5: Implement `model/editor_buffer.rs`**

```rust
//! The text of one open file and its highlight spans. Pure Rust: the widget
//! sends edits in and reads spans per line out.
//!
//! Highlighting re-runs over the whole buffer after an edit, lazily on the
//! next span query. tree-sitter parses a 4 MiB file in tens of milliseconds
//! and typical files in under one, so incremental tree edits are deferred
//! until profiling asks for them (IDE spec §6 describes the incremental form).

use crate::highlight::languages::Language;
use crate::highlight::theme::{StyleId, Theme};
use ropey::Rope;
use tree_sitter_highlight::{HighlightEvent, Highlighter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span { pub start: usize, pub len: usize, pub style: StyleId }

pub struct EditorBuffer {
    rope: Rope,
    language: Option<Language>,
    /// Spans per line; `None` until computed or after an edit.
    spans: Option<Vec<Vec<Span>>>,
}

impl EditorBuffer {
    pub fn new(path: &str, text: &str) -> EditorBuffer {
        EditorBuffer { rope: Rope::from_str(text), language: Language::from_path(path), spans: None }
    }

    pub fn language(&self) -> Option<Language> { self.language }
    pub fn text(&self) -> String { self.rope.to_string() }
    /// Lines as the editor counts them: a trailing newline yields a final empty line.
    pub fn line_count(&self) -> usize { self.rope.len_lines() }
    pub fn line(&self, n: usize) -> String {
        if n >= self.rope.len_lines() { return String::new(); }
        let s = self.rope.line(n).to_string();
        s.trim_end_matches(['\n', '\r']).to_string()
    }
    pub fn utf16_to_char(&self, utf16: usize) -> usize {
        self.rope.utf16_cu_to_char(utf16.min(self.rope.len_utf16_cu()))
    }

    /// Applies one edit in char units and returns the inclusive line range
    /// whose spans differ from before, so a view can re-highlight exactly
    /// those lines. Always includes the edited line.
    pub fn apply_edit(&mut self, char_pos: usize, removed_chars: usize, inserted: &str) -> (usize, usize) {
        let before = self.take_spans();
        let pos = char_pos.min(self.rope.len_chars());
        let end = (pos + removed_chars).min(self.rope.len_chars());
        self.rope.remove(pos..end);
        self.rope.insert(pos, inserted);
        let edited_line = self.rope.char_to_line(pos);
        let after = self.compute_spans();
        let (from, to) = changed_range(&before, &after, edited_line);
        self.spans = Some(after);
        (from, to)
    }

    pub fn replace_all(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.spans = None;
    }

    pub fn spans_for_line(&mut self, n: usize) -> &[Span] {
        if self.spans.is_none() { self.spans = Some(self.compute_spans()); }
        self.spans.as_ref().unwrap().get(n).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn spans_json(&mut self, n: usize, theme: &Theme) -> String {
        let items: Vec<serde_json::Value> = self
            .spans_for_line(n)
            .iter()
            .map(|s| {
                let st = theme.style(s.style);
                serde_json::json!({"s": s.start, "l": s.len, "fg": st.fg, "b": st.bold, "i": st.italic})
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    }

    fn take_spans(&mut self) -> Vec<Vec<Span>> {
        match self.spans.take() {
            Some(s) => s,
            None => self.compute_spans(),
        }
    }

    /// Full highlight pass: byte ranges from tree-sitter become per-line char
    /// spans, split at line boundaries.
    fn compute_spans(&self) -> Vec<Vec<Span>> {
        let mut lines: Vec<Vec<Span>> = vec![Vec::new(); self.rope.len_lines()];
        let Some(lang) = self.language else { return lines };
        let text = self.rope.to_string();
        let mut highlighter = Highlighter::new();
        let Ok(events) = highlighter.highlight(lang.config(), text.as_bytes(), None, |_| None) else { return lines };
        let mut stack: Vec<StyleId> = Vec::new();
        for ev in events.flatten() {
            match ev {
                HighlightEvent::HighlightStart(h) => { if let Some(id) = StyleId::from_index(h.0) { stack.push(id) } else { stack.push(StyleId::Punctuation) } }
                HighlightEvent::HighlightEnd => { stack.pop(); }
                HighlightEvent::Source { start, end } => {
                    let Some(&style) = stack.last() else { continue };
                    let start_c = self.rope.byte_to_char(start);
                    let end_c = self.rope.byte_to_char(end);
                    let mut c = start_c;
                    while c < end_c {
                        let line = self.rope.char_to_line(c);
                        let line_start = self.rope.line_to_char(line);
                        let line_end = if line + 1 < self.rope.len_lines() { self.rope.line_to_char(line + 1) } else { self.rope.len_chars() };
                        let stop = end_c.min(line_end);
                        // Exclude the newline itself so spans never cover it.
                        let visible_end = stop.min(line_start + self.line(line).chars().count());
                        if visible_end > c {
                            lines[line].push(Span { start: c - line_start, len: visible_end - c, style });
                        }
                        c = stop.max(c + 1);
                    }
                }
            }
        }
        lines
    }
}

fn changed_range(before: &[Vec<Span>], after: &[Vec<Span>], edited_line: usize) -> (usize, usize) {
    let mut from = edited_line;
    let mut to = edited_line;
    let n = before.len().max(after.len());
    for i in 0..n {
        let same = before.get(i).map(Vec::as_slice).unwrap_or(&[]) == after.get(i).map(Vec::as_slice).unwrap_or(&[]);
        if !same { from = from.min(i); to = to.max(i); }
    }
    (from, to)
}
```

Note `StyleId::from_index(h.0)`: unknown capture indices cannot occur because `configure` restricts the indices to `STYLE_NAMES`, but keep the fallback. Note `changed_range` compares line-by-line, so an insertion of a newline shifts every later line and reports a large range; that is correct (their block numbers changed too).

- [ ] **Step 6: Run the tests**: `cargo test -p bondsymphonic-ide --test editor_tests` → all pass. If `l1[0]` in the multi-line comment test has `len` 4 or 5 depending on whether the query captures the trailing space, adjust the assertion to the observed exact span and say so in the report. The first build compiles fifteen C grammars; allow a 10-minute timeout.

- [ ] **Step 7: Clippy, fmt, commit**

```bash
git add crates/ide Cargo.lock
git commit -m "feat(ide): tree-sitter highlight registry, theme, and rope-backed EditorBuffer"
```

---

### Task 4: `model/diff.rs` row alignment

**Files:**
- Create: `crates/ide/src/model/diff.rs`, `crates/ide/tests/diff_tests.rs`
- Modify: `crates/ide/Cargo.toml` (`similar = "2"`), `crates/ide/src/model/mod.rs` (`pub mod diff;`)

**Interfaces:**
- Produces: `#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)] #[serde(rename_all = "lowercase")] pub enum RowKind { Equal, Insert, Delete, Replace }`; `#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)] pub struct DiffRow { pub left_no: Option<usize>, pub left_text: String, pub right_no: Option<usize>, pub right_text: String, pub kind: RowKind }` (line numbers 1-based, texts without newline); `pub fn align(base: &str, work: &str) -> Vec<DiffRow>`; `pub fn counts(rows: &[DiffRow]) -> (usize, usize)` (additions = Insert + Replace rows, deletions = Delete + Replace rows); `pub fn rows_json(rows: &[DiffRow]) -> String`.

- [ ] **Step 1: Write the failing tests** `crates/ide/tests/diff_tests.rs`:

```rust
use bondsymphonic_ide::model::diff::{align, counts, rows_json, DiffRow, RowKind};

fn kinds(rows: &[DiffRow]) -> Vec<RowKind> { rows.iter().map(|r| r.kind).collect() }

#[test]
fn identical_texts_are_all_equal_rows_with_both_numbers() {
    let rows = align("a\nb\n", "a\nb\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Equal]);
    assert_eq!(rows[1], DiffRow { left_no: Some(2), left_text: "b".into(), right_no: Some(2), right_text: "b".into(), kind: RowKind::Equal });
    assert_eq!(counts(&rows), (0, 0));
}

#[test]
fn pure_insert_and_delete_rows_are_one_sided() {
    let rows = align("a\nc\n", "a\nb\nc\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Insert, RowKind::Equal]);
    assert_eq!(rows[1].left_no, None);
    assert_eq!(rows[1].left_text, "");
    assert_eq!(rows[1].right_no, Some(2));
    assert_eq!(counts(&rows), (1, 0));

    let rows = align("a\nb\nc\n", "a\nc\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Delete, RowKind::Equal]);
    assert_eq!(rows[1].right_no, None);
    assert_eq!(counts(&rows), (0, 1));
}

#[test]
fn replace_pairs_lines_and_spills_the_remainder() {
    // 2 old lines replaced by 3 new lines: two Replace rows + one Insert.
    let rows = align("x\nold1\nold2\ny\n", "x\nnew1\nnew2\nnew3\ny\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Replace, RowKind::Replace, RowKind::Insert, RowKind::Equal]);
    assert_eq!((rows[1].left_text.as_str(), rows[1].right_text.as_str()), ("old1", "new1"));
    assert_eq!((rows[3].left_no, rows[3].right_no), (None, Some(4)));
    assert_eq!(counts(&rows), (3, 2));
}

#[test]
fn empty_base_is_all_inserts_and_missing_trailing_newline_is_kept_as_text() {
    let rows = align("", "a\nb");
    assert_eq!(kinds(&rows), vec![RowKind::Insert, RowKind::Insert]);
    assert_eq!(rows[1].right_text, "b");
}

#[test]
fn rows_json_uses_lowercase_kinds_and_null_numbers() {
    let rows = align("a\n", "b\n");
    let v: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert_eq!(v[0]["kind"], "replace");
    let rows = align("", "a\n");
    let v: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert!(v[0]["left_no"].is_null());
    assert_eq!(v[0]["right_no"], 1);
}
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-ide --test diff_tests`.

- [ ] **Step 3: Implement `model/diff.rs`**

```rust
//! Side-by-side alignment of two texts into rows, for the diff view.

use serde::Serialize;
use similar::{DiffTag, TextDiff};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RowKind { Equal, Insert, Delete, Replace }

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiffRow {
    pub left_no: Option<usize>,
    pub left_text: String,
    pub right_no: Option<usize>,
    pub right_text: String,
    pub kind: RowKind,
}

fn strip(line: &str) -> String { line.trim_end_matches(['\n', '\r']).to_string() }

pub fn align(base: &str, work: &str) -> Vec<DiffRow> {
    let diff = TextDiff::from_lines(base, work);
    let old: Vec<&str> = diff.old_slices().to_vec();
    let new: Vec<&str> = diff.new_slices().to_vec();
    let mut rows = Vec::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = (op.tag(), op.old_range(), op.new_range());
        match tag {
            DiffTag::Equal => {
                for (o, n) in old_range.zip(new_range) {
                    rows.push(DiffRow { left_no: Some(o + 1), left_text: strip(old[o]), right_no: Some(n + 1), right_text: strip(new[n]), kind: RowKind::Equal });
                }
            }
            DiffTag::Delete => {
                for o in old_range {
                    rows.push(DiffRow { left_no: Some(o + 1), left_text: strip(old[o]), right_no: None, right_text: String::new(), kind: RowKind::Delete });
                }
            }
            DiffTag::Insert => {
                for n in new_range {
                    rows.push(DiffRow { left_no: None, left_text: String::new(), right_no: Some(n + 1), right_text: strip(new[n]), kind: RowKind::Insert });
                }
            }
            DiffTag::Replace => {
                let mut olds = old_range;
                let mut news = new_range;
                loop {
                    match (olds.next(), news.next()) {
                        (Some(o), Some(n)) => rows.push(DiffRow { left_no: Some(o + 1), left_text: strip(old[o]), right_no: Some(n + 1), right_text: strip(new[n]), kind: RowKind::Replace }),
                        (Some(o), None) => rows.push(DiffRow { left_no: Some(o + 1), left_text: strip(old[o]), right_no: None, right_text: String::new(), kind: RowKind::Delete }),
                        (None, Some(n)) => rows.push(DiffRow { left_no: None, left_text: String::new(), right_no: Some(n + 1), right_text: strip(new[n]), kind: RowKind::Insert }),
                        (None, None) => break,
                    }
                }
            }
        }
    }
    rows
}

pub fn counts(rows: &[DiffRow]) -> (usize, usize) {
    let adds = rows.iter().filter(|r| matches!(r.kind, RowKind::Insert | RowKind::Replace)).count();
    let dels = rows.iter().filter(|r| matches!(r.kind, RowKind::Delete | RowKind::Replace)).count();
    (adds, dels)
}

pub fn rows_json(rows: &[DiffRow]) -> String {
    serde_json::to_string(rows).expect("rows serialise")
}
```

`similar::DiffOp` exposes `tag()`, `old_range()`, `new_range()`; `TextDiff::old_slices()` / `new_slices()` give the line slices. If the `similar` version you resolve names them differently, adapt and keep the tests unchanged.

- [ ] **Step 4: Run the tests** → all pass.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide Cargo.lock
git commit -m "feat(ide): side-by-side diff row alignment"
```

---

### Task 5: `EditorDocument`, `DiffDocument`, `ChangesModel` QObjects; controller open/save requests

**Files:**
- Create: `crates/ide/src/qobjects/editor_document.rs`, `crates/ide/src/qobjects/diff_document.rs`, `crates/ide/src/qobjects/changes_model.rs`
- Modify: `crates/ide/src/qobjects/mod.rs`, `crates/ide/src/qobjects/app_controller.rs` (three signals + three invokables), `crates/ide/build.rs` (three `.file(...)` lines), `crates/ide/tests/qobject_smoke.rs` (JSON helper tests)

**Interfaces:**
- Consumes: `require_connection() -> Result<Shared{client, router}, &'static str>`, `runtime()`, `EventRouter::subscribe_all() -> EventRx`, `DaemonClient::request::<T>(Request)`, proto `FsPathParams`, `FsWriteParams`, `FsWatchParams`, `ReadFileResult`, `DiffResult`, `ChangesResult`, `WorkspaceIdParams`, `WorkspaceDiffParams`, `Event::FsChanged`; `EditorBuffer`, `diff::align/counts/rows_json`, `Theme`.
- Produces (C++ names via `#[auto_cxx_name]`):

`EditorDocument` (one per open file; created from C++ with `new EditorDocument(parent)`):
- properties: `workspaceId: QString`, `path: QString`, `dirty: bool`, `readOnlyReason: QString` (empty when editable; else "binary file", "file larger than 4 MiB (truncated)"), `language: QString`, `error: QString`, `darkTheme: bool`.
- invokables: `open(workspaceId, path)`; `applyEdit(utf16Pos: i32, utf16Removed: i32, inserted: QString)`; `text() -> QString`; `lineCount() -> i32`; `spansForLine(n: i32) -> QString`; `save()`; `acceptExternal()` (reload from disk, drops local edits); `keepLocal()` (dismiss the external-change state; the next save overwrites).
- signals: `loaded()` (text is ready or was replaced; the view must reset its document from `text()`), `highlightChanged(fromLine: i32, toLine: i32)`, `saved()`, `saveFailed(message: QString)`, `externalChange()` (disk changed while dirty), `loadFailed(message: QString)`.

`DiffDocument`: properties `workspaceId`, `path`, `darkTheme: bool`, `additions: i32`, `deletions: i32`, `error: QString`; invokables `load(workspaceId, path)`, `rowsJson() -> QString`, `spansForLeftLine(n) -> QString`, `spansForRightLine(n) -> QString` (line numbers are the 1-based `left_no`/`right_no` from the rows); signals `rowsLoaded()`, `loadFailed(message)`.

`ChangesModel` (one, app-wide, like `FileTreeModel`): invokables `setWorkspace(workspaceId)` (enables `fs.watch` for it and refreshes), `refresh()`, `workspaceId() -> QString`; signals `changesLoaded(json: QString)` (JSON array of `ChangedFile`), `loadFailed(message)`. Auto-refreshes 500 ms after any `fs.changed` for its workspace (coalesced).

`AppController` additions: signals `openFileRequested(workspaceId, path)`, `openDiffRequested(workspaceId, path)`, `saveAllRequested()`; invokables `requestOpenFile(workspaceId, path)`, `requestOpenDiff(workspaceId, path)`, `requestSaveAll()` which just emit them. These are the one path by which anything in Rust (smoke script now, transcript tool cards in Milestone 4) asks the window to open something.

- [ ] **Step 1: Write the pure-Rust tests** added to `crates/ide/tests/qobject_smoke.rs` (the QObjects need a Qt event loop, so test their pure helpers):

```rust
use bondsymphonic_ide::qobjects::editor_document::read_only_reason;
use bondsymphonic_ide::qobjects::changes_model::touches_workspace;
use bondsymphonic_proto::{Event, PtyId, ReadFileResult, WorkspaceId};

#[test]
fn read_only_reason_names_binary_and_truncated_files() {
    let ok = ReadFileResult { content: "x".into(), encoding: "utf-8".into(), truncated: false };
    assert_eq!(read_only_reason(&ok), "");
    let bin = ReadFileResult { content: String::new(), encoding: "binary".into(), truncated: false };
    assert_eq!(read_only_reason(&bin), "binary file");
    let big = ReadFileResult { content: "x".into(), encoding: "utf-8".into(), truncated: true };
    assert_eq!(read_only_reason(&big), "file larger than 4 MiB (truncated)");
}

#[test]
fn fs_changed_events_are_matched_by_workspace() {
    let ws = WorkspaceId("ws_1".into());
    let ev = Event::FsChanged { paths: vec!["a.rs".into()] };
    assert!(touches_workspace(&Some(ws.clone()), &ev, "ws_1"));
    assert!(!touches_workspace(&Some(ws), &ev, "ws_2"));
    assert!(!touches_workspace(&None, &ev, "ws_1"));
    assert!(!touches_workspace(&Some(WorkspaceId("ws_1".into())), &Event::PtyExit { pty_id: PtyId("p".into()), code: 0 }, "ws_1"));
}
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-ide --test qobject_smoke`.

- [ ] **Step 3: Implement `qobjects/editor_document.rs`**. Follow `file_tree.rs` for the bridge shape, `terminal_session.rs` for a per-instance QObject that owns a background task and aborts it on `Drop` (read how it handles `close`/`Drop` and the `QtHandle` alias in `app_controller.rs`).

```rust
//! One open file: text and highlight spans live in an [`EditorBuffer`]; the
//! daemon is the file system. Edits arrive from the view in UTF-16 units and
//! are applied in char units; spans go back out per line as JSON.

use crate::highlight::theme::Theme;
use crate::model::editor_buffer::EditorBuffer;
use crate::qobjects::app_controller::{require_connection, runtime};
use bondsymphonic_proto::{Event, FsPathParams, FsWatchParams, FsWriteParams, ReadFileResult, Request, WorkspaceId, Empty};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, path)]
        #[qproperty(bool, dirty)]
        #[qproperty(QString, read_only_reason)]
        #[qproperty(QString, language)]
        #[qproperty(QString, error)]
        #[qproperty(bool, dark_theme)]
        type EditorDocument = super::EditorDocumentRust;

        #[qsignal] fn loaded(self: Pin<&mut EditorDocument>);
        #[qsignal] fn highlight_changed(self: Pin<&mut EditorDocument>, from_line: i32, to_line: i32);
        #[qsignal] fn saved(self: Pin<&mut EditorDocument>);
        #[qsignal] fn save_failed(self: Pin<&mut EditorDocument>, message: QString);
        #[qsignal] fn external_change(self: Pin<&mut EditorDocument>);
        #[qsignal] fn load_failed(self: Pin<&mut EditorDocument>, message: QString);

        #[qinvokable] fn open(self: Pin<&mut EditorDocument>, workspace_id: QString, path: QString);
        #[qinvokable] fn apply_edit(self: Pin<&mut EditorDocument>, utf16_pos: i32, utf16_removed: i32, inserted: QString);
        #[qinvokable] fn text(self: &EditorDocument) -> QString;
        #[qinvokable] fn line_count(self: &EditorDocument) -> i32;
        #[qinvokable] fn spans_for_line(self: Pin<&mut EditorDocument>, n: i32) -> QString;
        #[qinvokable] fn save(self: Pin<&mut EditorDocument>);
        #[qinvokable] fn accept_external(self: Pin<&mut EditorDocument>);
        #[qinvokable] fn keep_local(self: Pin<&mut EditorDocument>);
    }

    impl cxx_qt::Threading for EditorDocument {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

pub struct EditorDocumentRust {
    workspace_id: QString,
    path: QString,
    dirty: bool,
    read_only_reason: QString,
    language: QString,
    error: QString,
    dark_theme: bool,
    buffer: Option<EditorBuffer>,
    /// Background subscription to `fs.changed`, aborted on re-open and Drop.
    watch_task: Option<tokio::task::JoinHandle<()>>,
    /// Set while a disk change is pending the user's reload/keep decision.
    external_pending: bool,
    /// Bumped on every `open` so a late read for an earlier file is dropped.
    generation: u64,
}

impl Default for EditorDocumentRust { /* all empty/false/None/0 */ }

impl Drop for EditorDocumentRust {
    fn drop(&mut self) { if let Some(t) = self.watch_task.take() { t.abort(); } }
}

/// Why a file opens read-only, or empty when it is editable.
pub fn read_only_reason(res: &ReadFileResult) -> &'static str {
    if res.encoding == "binary" { "binary file" } else if res.truncated { "file larger than 4 MiB (truncated)" } else { "" }
}
```

Behaviour to implement in `impl qobject::EditorDocument`:
- `open`: abort any `watch_task`; bump `generation`; set properties (`workspace_id`, `path`, `language` from `Language::from_path(...).map(name)`), `dirty=false`, `read_only_reason=""`, `error=""`; `require_connection()` else `load_failed`; spawn a task: `fs.read_file` → on Qt thread, if generation matches: build `EditorBuffer::new(path, content)`, set `read_only_reason`, emit `loaded()`. Then (same task, after the read succeeds) send `fs.watch enable` for the workspace (ignore its result beyond a `tracing::warn`), and spawn the watch task: `router.subscribe_all()`; for each `(ws, ev)` where `touches_workspace(&ws, &ev, &workspace_id)` and `paths` contains `path`: queue onto Qt: if `dirty` → set `external_pending=true`, emit `external_change()`; else re-read via `fs.read_file` (new task) and, if the content differs from `buffer.text()`, `replace_all` + emit `loaded()`.
- `apply_edit`: if `buffer` is None or `read_only_reason` non-empty → return; convert `utf16_pos`/`utf16_removed` with `buffer.utf16_to_char` (removed = `utf16_to_char(pos+removed) - utf16_to_char(pos)`); `let (from, to) = buffer.apply_edit(..)`; set `dirty=true` (emits `dirtyChanged` through the property); emit `highlight_changed(from, to)`.
- `text`, `line_count`, `spans_for_line(n)` → `buffer.spans_json(n, Theme::for_dark(dark_theme))` or `"[]"`.
- `save`: if no buffer or read-only → return; spawn `fs.write_file` with `buffer.text()`; on Ok → Qt thread: `dirty=false`, `external_pending=false`, emit `saved()`; on Err → `save_failed(message)` and `error` property set. (The daemon's own write triggers an `fs.changed`; the watch handler then re-reads and sees identical content, so nothing happens. That is the intended, stateless way to ignore our own writes.)
- `accept_external`: re-read via `fs.read_file`; on Qt thread `replace_all`, `dirty=false`, `external_pending=false`, emit `loaded()`.
- `keep_local`: `external_pending=false`.

`touches_workspace` lives in `changes_model.rs` (below) and is reused here.

- [ ] **Step 4: Implement `qobjects/diff_document.rs`**: properties and invokables as listed. `load` spawns `workspace.diff`; on Qt thread: `rows = align(base, work)`, `(a, d) = counts(&rows)`, keep `rows_json` string, build two `EditorBuffer`s (`EditorBuffer::new(&path, &base_text)`, `EditorBuffer::new(&path, &work_text)`) for `spans_for_left_line(n)` = left buffer line `n-1`, `spans_for_right_line(n)` = right buffer line `n-1` (n ≤ 0 → `"[]"`); set `additions`, `deletions`; emit `rows_loaded()`. Generation guard as in `open`.

- [ ] **Step 5: Implement `qobjects/changes_model.rs`**:

```rust
pub fn touches_workspace(ws: &Option<WorkspaceId>, ev: &Event, workspace_id: &str) -> bool {
    matches!(ev, Event::FsChanged { .. }) && ws.as_ref().map(|w| w.0 == workspace_id).unwrap_or(false)
}
```

`set_workspace(id)`: if unchanged → return; store; abort the previous watch task; if `id` empty → emit `changes_loaded("[]")` and return; spawn: `fs.watch enable` (warn on failure), then `refresh()`; spawn the watch task: on any matching `fs.changed`, sleep 500 ms while draining further matching events (`tokio::select!` pattern from Task 2's `coalesce`), then queue `refresh()` on the Qt thread. `refresh()`: `workspace.changes` → Qt thread (generation guard by workspace id, like `file_tree.rs`) → `changes_loaded(serde_json::to_string(&res.files))` or `load_failed`.

- [ ] **Step 6: `AppController` signals/invokables** (`app_controller.rs` bridge block):

```rust
        #[qsignal] fn open_file_requested(self: Pin<&mut AppController>, workspace_id: QString, path: QString);
        #[qsignal] fn open_diff_requested(self: Pin<&mut AppController>, workspace_id: QString, path: QString);
        #[qsignal] fn save_all_requested(self: Pin<&mut AppController>);
        #[qinvokable] fn request_open_file(self: Pin<&mut AppController>, workspace_id: QString, path: QString);
        #[qinvokable] fn request_open_diff(self: Pin<&mut AppController>, workspace_id: QString, path: QString);
        #[qinvokable] fn request_save_all(self: Pin<&mut AppController>);
```

Each invokable emits its signal. Register the three new bridge files in `build.rs` (`.file("src/qobjects/editor_document.rs")` etc.) and `pub mod` them in `qobjects/mod.rs`.

- [ ] **Step 7: Build and test**: `cargo build -p bondsymphonic-ide` (cxx-qt regenerates headers; fix any bridge syntax errors), `cargo test -p bondsymphonic-ide`. Expected: all green, the two new tests included.

- [ ] **Step 8: Clippy, fmt, commit**

```bash
git add crates/ide
git commit -m "feat(ide): EditorDocument, DiffDocument and ChangesModel QObjects; open/save requests on AppController"
```

---

### Task 6: `CodeView`, `RustHighlighter`, `EditorWidget`, `EditorArea`; MainWindow wiring

**Files:**
- Create: `crates/ide/cpp/CodeView.h`, `CodeView.cpp`, `RustHighlighter.h`, `RustHighlighter.cpp`, `EditorWidget.h`, `EditorWidget.cpp`, `EditorArea.h`, `EditorArea.cpp`
- Modify: `crates/ide/cpp/MainWindow.h`, `crates/ide/cpp/MainWindow.cpp` (replace `m_editorPlaceholder`; menus; wiring), `crates/ide/build.rs` (`cpp_files`)

**Interfaces:**
- Consumes: `EditorDocument` (Task 5), `ExplorerDock::fileActivated(path)`, `GroupModel` active tab (how `MainWindow::onActiveTabChanged` reads the active workspace id), `AppController::openFileRequested/saveAllRequested`, `AppController::workspaceDestroyed`.
- Produces:
  - `class CodeView : public QPlainTextEdit` — `explicit CodeView(QWidget* parent)`: monospace font `QFontDatabase::systemFont(FixedFont)` size 10, `setTabStopDistance(4 * advance('M'))`, `setLineWrapMode(NoWrap)`, a `LineNumberArea` child painted from `blockBoundingGeometry` (classic Qt code-editor example), current-line highlight via `setExtraSelections` (palette `AlternateBase` tint), `void setLineNumberProvider(std::function<QString(int block)> f)` (default: block+1), `void setRowTints(const QVector<QColor>& perBlock)` (empty = none; applied as extra selections behind the current-line one), `int gutterWidth() const`.
  - `class RustHighlighter : public QSyntaxHighlighter` — `RustHighlighter(QTextDocument* doc, std::function<QString(int block)> spansProvider)`; `highlightBlock(text)` parses the JSON array `[{"s","l","fg","b","i"}]` and calls `setFormat(s, l, fmt)`; `void rehighlightLines(int from, int to)` calls `rehighlightBlock` for each block in range (clamped).
  - `class EditorWidget : public QWidget` — `EditorWidget(EditorDocument* doc, QWidget* parent)`: vertical layout {notice bar (QLabel, hidden unless `readOnlyReason` non-empty), external-change bar (QFrame with label "File changed on disk" + "Reload" + "Keep mine", hidden), `CodeView`}; `EditorDocument* document() const`; `CodeView* view() const`; `void save()`. Behaviour: on `doc->loaded()` set `m_settingText = true; view->setPlainText(doc->text()); m_settingText = false`, `setReadOnly(!readOnlyReason.isEmpty())`, show/hide the notice, hide the external bar, `highlighter->rehighlight()`; on `QTextDocument::contentsChange(pos, removed, added)` when `!m_settingText` → `doc->applyEdit(pos, removed, view->document()->toPlainText().mid(pos, added))` (a `QTextCursor` at `pos` selecting `added` chars is cheaper than `toPlainText` on large files; use `QTextCursor::setPosition(pos); setPosition(pos+added, KeepAnchor); selectedText()` and replace U+2029 with '\n'); on `doc->highlightChanged(from,to)` → `highlighter->rehighlightLines(from,to)`; on `doc->externalChange()` → show the bar; Reload → `doc->acceptExternal()`; Keep → `doc->keepLocal()` + hide; `doc->darkTheme` set once in the ctor from `palette().base().color().lightness() < 128`; Ctrl+S is a `QShortcut` on the widget calling `doc->save()`; `doc->saveFailed(msg)` → `QMessageBox::warning`.
  - `class EditorArea : public QWidget` — `explicit EditorArea(QWidget* parent)`: `QStackedWidget{ QLabel("Open a file from the Explorer"), QTabWidget(closable, movable, document mode) }`; `void openFile(const QString& workspaceId, const QString& path)` (existing tab keyed by `workspaceId + "\n" + path` is activated; otherwise `new EditorDocument(this)`, `new EditorWidget(doc, this)`, tab text = file name, tooltip = `workspaceId:path`, `doc->open(...)`); `void openDiff(...)` (Task 7; declare now, body added there); `EditorWidget* currentEditor() const` (null when the current tab is not an editor); `void saveAll()`; `void closeWorkspace(const QString& workspaceId)` (removes its tabs, discarding edits); `bool closeTab(int index)` (dirty → `QMessageBox` Save / Discard / Cancel; Save calls `doc->save()` and closes on `saved()`); tab title gets a leading `● ` while `dirty` (connect `dirtyChanged`). Signal: `void currentEditorChanged(EditorWidget* editor)` (null for none).
  - `MainWindow`: `m_editorArea` replaces `m_editorPlaceholder` in the splitter (same 3:2 seeding); Explorer `fileActivated(path)` → `m_editorArea->openFile(activeWorkspaceId(), path)` where `activeWorkspaceId()` is the helper `onActiveTabChanged` already computes (extract it if it is inline); `AppController::openFileRequested` → `openFile`; `saveAllRequested` → `saveAll`; `workspaceDestroyed(id)` → `closeWorkspace(id)` in addition to what it does today. Menus: File gains "Save" (Ctrl+S → `currentEditor()->save()`), "Save All" (Ctrl+Shift+S); Edit gets Undo/Redo/Cut/Copy/Paste/Select All with standard shortcuts forwarding to `currentEditor()->view()` (disabled when null, via `currentEditorChanged`); View gets "Swap editor and agent" which calls `m_centerSplitter->insertWidget(0, m_centerSplitter->widget(1))`.

- [ ] **Step 1: Implement** the four classes and the wiring above. Keep painting in `CodeView` (gutter + tints) and JSON parsing in `RustHighlighter`; nothing in C++ decides whether a file is dirty, editable, or which lines to re-highlight.

- [ ] **Step 2: Verification with a temporary env-gated hook** (`BS_T6_SELFTEST=1`, read in `MainWindow` after `AppController::connected`; removed before commit). The hook: creates a workspace on a throwaway repo under the scratchpad (make one with `git init`, a `src/main.rs` with `fn main() { println!("hi"); }` and a `README.md`), waits for `workspaceCreated`, calls `openFile(ws, "src/main.rs")`, waits for `loaded()`, logs to stderr the tab title, `doc->readOnlyReason()`, `doc->lineCount()`, `doc->spansForLine(0)`; then edits through the widget's own document (`view->textCursor().insertText("// edited\n")` at position 0, which goes through `contentsChange` exactly like typing does), logs `doc->dirty()` (expect true) and the tab title (expect `● main.rs`), calls `save()`, waits for `saved()`, then reads the file back through the daemon (`fs.read_file` via a second `EditorDocument` or the WSL path) and logs that the first line is `// edited`; then writes to the file from outside (`wsl -d bondsymphonic -- bash -c "echo external >> <worktree>/src/main.rs"` from PowerShell while the hook waits) and logs that `externalChange()` fired only if the buffer was dirty, else that `loaded()` fired with the new content; opens a `.png` or a 5 MiB file and logs the notice text; grabs `window()->grab().save("<scratchpad>\m3-editor.png")`; destroys the workspace; quits. Record every stderr line in the report. Confirm in the log the daemon's `fs.changed` arrived and the smoke-free path re-read once (identical content, no `loaded()`).

- [ ] **Step 3: Build, clippy, fmt, remove the hook, commit**

```bash
git add crates/ide
git commit -m "feat(ide): code editor with tree-sitter highlighting, save through the daemon, tabbed editor area"
```

---

### Task 7: `DiffWidget`, Explorer Changes tab and status colouring

**Files:**
- Create: `crates/ide/cpp/DiffWidget.h`, `crates/ide/cpp/DiffWidget.cpp`
- Modify: `crates/ide/cpp/ExplorerDock.h`, `ExplorerDock.cpp`, `crates/ide/cpp/EditorArea.h`, `EditorArea.cpp` (`openDiff`), `crates/ide/cpp/MainWindow.h`, `MainWindow.cpp`, `crates/ide/cpp/app.cpp` (construct `ChangesModel`), `crates/ide/build.rs`

**Interfaces:**
- Consumes: `DiffDocument`, `ChangesModel` (Task 5), `CodeView` + `RustHighlighter` (Task 6), `AppController::openDiffRequested`.
- Produces:
  - `class DiffWidget : public QWidget` — `DiffWidget(DiffDocument* doc, QWidget* parent)`: header row (`QLabel` path, `QLabel` "+N −M" coloured green/red), below it a `QSplitter` with two read-only `CodeView`s. On `rowsLoaded()`: parse `doc->rowsJson()`, fill the left view with every row's `left_text` joined by '\n' and the right with `right_text`, set each view's line-number provider to the row's `left_no`/`right_no` (empty string for null), set row tints per block (Insert → green tint on the right and grey on the left, Delete → red on the left and grey on the right, Replace → yellow both sides, Equal → none; tints derived from the palette so they work on dark), attach a `RustHighlighter` per side whose provider maps block → row → `doc->spansForLeftLine(left_no)` / `spansForRightLine(right_no)` (null → `"[]"`), and synchronise the two vertical scrollbars (connect each `valueChanged` to the other's `setValue`, guarded against re-entry) and horizontal likewise. `loadFailed(msg)` → header shows the error in red.
  - `EditorArea::openDiff(workspaceId, path)`: tab keyed `workspaceId + "\n#diff\n" + path`, title `path (diff)`, `new DiffDocument(this)` + `DiffWidget`, `doc->load(...)`. `currentEditor()` returns null for diff tabs.
  - `ExplorerDock(FileTreeModel*, ChangesModel*, QWidget*)`: Changes tab is a `QTreeView` over a `QStandardItemModel` with columns Path, Status, +, −; rows from `ChangesModel::changesLoaded(json)` (status text lowercased as sent; + and − right-aligned; status coloured: added/untracked green, modified amber, deleted red, renamed blue); double-click → new signal `diffActivated(const QString& path)`; `setWorkspace(id)` also calls `changes->setWorkspace(id)`; `refresh()` also calls `changes->refresh()`. Files tree: `FileEntry.status` is `unchanged` today (daemon `fs.list_dir` does not fill it; `crates/daemon/src/fs.rs:82`), so colour by status where present but do not add a lookup; leave a one-line comment that colouring lights up when the daemon reports status.
  - `MainWindow`: creates the `ChangesModel` in `app.cpp` next to the other models and passes it to `ExplorerDock`; `diffActivated(path)` → `m_editorArea->openDiff(activeWorkspaceId(), path)`; `AppController::openDiffRequested` → same.

- [ ] **Step 1: Implement** the widget, the tab, and the wiring.

- [ ] **Step 2: Verification with a temporary env-gated hook** (`BS_T7_SELFTEST=1`, removed before commit; no desktop input): create a workspace on a throwaway repo; write two files through the daemon (`fs.write_file`: modify `README.md`, add `new.txt`) and commit nothing; wait for the Changes list to populate through `changesLoaded` (the watch should trigger it without pressing Refresh; log the elapsed time from the write); log each row (path, status, +, −); call `openDiff(ws, "README.md")`, wait for `rowsLoaded`, log `additions/deletions`, the first three rows' kinds and both line-number columns, and both views' `blockCount()` (must be equal); scroll the left view to the bottom programmatically and log both scrollbars' values (must match); grab `window()->grab()` to `<scratchpad>\m3-diff.png`; destroy the workspace; quit. Record the stderr in the report.

- [ ] **Step 3: Build, clippy, fmt, remove the hook, commit**

```bash
git add crates/ide
git commit -m "feat(ide): side-by-side diff view and live Changes list in the Explorer"
```

---

### Task 8: Smoke test, docs, footprint, end-to-end verification

**Files:**
- Modify: `crates/ide/src/qobjects/smoke.rs` (steps `open_file`, `open_diff`), `crates/ide/tests/smoke.rs` (fake daemon methods + assertions), `README.md` ("What works now", "Test hooks"), `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§6 note on full re-highlight, §13 numbers, §14 smoke description), `.github/workflows/ci.yml` (only if the daemon job needs a `notify`-related change)

**Interfaces:** none new. Smoke steps: `open_file` → `controller.request_open_file(ws, "README.md")`; `open_diff` → `controller.request_open_diff(ws, "README.md")`; `BS_SMOKE_SCRIPT=create,open,tree,open_file,open_diff,quit`.

- [ ] **Step 1: Extend the fake daemon** in `tests/smoke.rs`: `fs.read_file` → `{content: "hello\n", encoding: "utf-8", truncated: false}`; `fs.write_file` → `{}`; `fs.watch` → `{}`; `workspace.changes` → one `ChangedFile{path:"README.md", status: Modified, additions:1, deletions:0}`; `workspace.diff` → `{base_text:"hello\n", work_text:"hello\nworld\n"}`. Extend the expected-order list with `fs.read_file`, `workspace.diff`, and assert `fs.watch` and `workspace.changes` were each seen at least once after `workspace.create`. Keep the existing assertions (two `fs.list_dir`, two `pty.open`, no `pty.*` after destroy).

- [ ] **Step 2: Run** `cargo test -p bondsymphonic-ide --test smoke` → green; then the whole workspace: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`; WSL: `.\scripts\test-daemon.ps1`.

- [ ] **Step 3: Footprint**: as in Milestone 2b (two workspaces, agent pane + shell tab each) plus three open editor tabs and one diff, 60 s idle: IDE `WorkingSet64` and daemon RSS. Write both into IDE spec §13 next to the 2b numbers. Targets unchanged (IDE < 150 MB, daemon < 30 MB). If the IDE grew by more than 40 MB over 2b, check whether all fifteen `HighlightConfiguration`s were built (they are lazy per language; only opened languages should be resident) and note the finding.

- [ ] **Step 4: Docs**: README "What works now" adds editing with highlighting, save, external-change handling, Changes list, diff view; "Test hooks" lists the new smoke steps. IDE spec §6 gets the sentence from `editor_buffer.rs`'s module doc about full re-highlight; §14's smoke bullet is rewritten to what the test now does (create, PTY, tree, open file, open diff, request-order assertion) instead of the transcript feed that belongs to Milestone 4.

- [ ] **Step 5: End-to-end** with `.\launch.ps1` and a temporary hook (`BS_T8_SELFTEST=1`, removed before commit): two workspaces on one throwaway repo; in workspace A open `src/main.rs`, edit and save; in workspace B open the Changes tab and confirm A's edit is *not* listed (worktrees are independent) while B's own `fs.write_file` is; open B's diff; destroy A; confirm B's editor tab still saves. Screenshot `<scratchpad>\m3-final.png` via `grab()`. Paste the stderr evidence.

- [ ] **Step 6: Commit**

```bash
git add crates/ide README.md docs
git commit -m "test(ide): smoke covers editor and diff; footprint numbers; docs for milestone 3"
```

---

## Milestone 3 exit criteria

- Double-clicking a file in the Explorer opens it in a tab with syntax highlighting for the fifteen languages; typing marks the tab dirty; Ctrl+S writes through the daemon into the sandboxed worktree; a binary or >4 MiB file opens as a read-only notice.
- A change made on disk by something else (an agent, a shell) reloads an unmodified open file silently and offers reload/keep on a modified one.
- The Changes tab lists files changed vs the base branch with +/- counts, updates within about a second of a write without pressing Refresh, and double-click opens a side-by-side diff with aligned rows, tints, line numbers, synchronised scrolling and highlighting.
- Daemon: `workspace.changes`, `workspace.diff`, `fs.watch` implemented with integration tests passing on Windows (noop) and in WSL; paths remain contained.
- Offscreen smoke test covers open-file and open-diff; clippy and fmt clean; footprint recorded.
