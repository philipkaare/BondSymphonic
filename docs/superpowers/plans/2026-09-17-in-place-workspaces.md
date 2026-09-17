# In-place Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an agent work directly in a repository's own checkout, on whatever branch is checked out, as a second kind of workspace beside the `bs/<name>/work` worktree.

**Architecture:** `WorkspaceKind { Worktree, InPlace }` travels in `WorkspaceInfo` and the daemon registry. Everything in-place-specific on the daemon side lives in one new module, `crates/daemon/src/workspace/in_place.rs` (`InPlaceLayout`: the pinned git, the `.git` preparation, the bwrap binds, the refusals); `lifecycle.rs`, `changes.rs`, `merge.rs`, `pr.rs` and `repo.rs` branch on the kind and call into it. `layout_for` refuses an in-place workspace, so no worktree-only path can be reached by accident. The IDE carries `kind` on the tab, offers the choice in the New Agent dialog, hides the git actions, and says **Close workspace…** instead of Destroy.

**Tech Stack:** Rust 2021 (tokio, serde), cxx-qt, Qt 6 Widgets (C++), git CLI, bubblewrap 0.9 in the WSL2 distro `bondsymphonic`.

**Spec:** `docs/superpowers/specs/2026-09-17-in-place-workspaces-design.md`

## Global Constraints

- **Worktree:** all work happens in `C:\git\BondSymphonic-inplace` (branch `feat/in-place-workspaces`). Never touch `C:\git\BondSymphonic`.
- **Protocol:** `PROTOCOL_VERSION` becomes `2`. `PRE_M7_PROTOCOL_VERSION` stays `1`, so a `hello` without a version is now refused.
- **Wire names:** `WorkspaceKind` serialises `snake_case` (`"worktree"`, `"in_place"`), `#[serde(default)]` = `Worktree`. `WorkspaceCreateParams.in_place: bool` (`#[serde(default)]`). `RepoInfo` gains three `#[serde(default)] Option<String>` fields: `head_branch`, `in_place_refusal`, `hooks_path_in_tree` (see "Spec resolutions" below for the two the spec did not name).
- **Refusal reason:** merge and PR on an in-place workspace answer `InvalidParams` with `data.reason = "in_place"` and the message `an in-place workspace has nothing to merge; commit and push from the checkout`.
- **Exact user-facing strings** (copy verbatim):
  - Restore/restart failure: `The repository <path> is missing or is no longer a git repository. Close the workspace, or restore the folder and press Retry.`
  - Duplicate: `this checkout already has an in-place workspace: <name>` (`Conflict`).
  - Dialog choices: `Work in a new worktree` (default), `Work directly in this checkout`.
  - Dialog help: `The agent edits this folder on its current branch. Its changes are not isolated on a branch of their own.`
  - Hooks warning: `This repository runs git hooks from <path> inside the working tree. The agent can change them, and they run outside the sandbox the next time you use git here.`
  - Detached head in the base-branch field: `detached HEAD`.
  - Close question: `Close workspace "<name>"? The agent and its sandbox stop. Your files, branches and git history are not touched.` (unnamed: `Close this workspace? The agent and its sandbox stop. Your files, branches and git history are not touched.`), window title `Close workspace`, no Force box, one non-forced destroy.
  - Menu/tab/banner verb: `Close workspace…` (U+2026).
- **Daemon-side git on an in-place workspace** is `InPlaceLayout::git()`: `GIT_DIR=<root>/.git`, `GIT_COMMON_DIR=<root>/.git`, `GIT_WORK_TREE=<root>`, `-c core.hooksPath=<data>/nohooks`. `NEUTRALISED_CONFIG` is **not** applied.
- **Every daemon-side `git status` / `git diff` against an agent-writable tree passes `--ignore-submodules=all`** (both kinds; see resolution R3).
- **Commit by path, always:** `git add <paths>` immediately followed by `git commit -m "<msg>" -- <paths>`. Other agents are committing in the same worktree at the same time; never `git add -A`, never `git commit -a`, never commit a path you did not change. Every commit message ends with a blank line and `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>` (the tasks use `git commit -m "<subject>" -m "Co-Authored-By: ..." -- <paths>`, which produces exactly that).
- **Keep every crate compiling after every step.** Another task may be building while you edit. If a step leaves the tree broken, finish the next step before pausing.
- **Shared test files are append-only** (`crates/ide/tests/smoke.rs`, `crates/ide/tests/qobject_smoke.rs`, `crates/ide/tests/model_tests.rs`): add one `mod <task_name> { ... }` at the very end. Two exceptions, both sequential, both named in their task: Task 1 edits `WorkspaceInfo` fixture literals everywhere; Task 5 edits the `AgentTab` fixture literals; Task 6 edits the `cpp_widgets` array in `qobject_smoke.rs`.
- **Never use the user's data:** tests use `tempfile` dirs and throwaway repos (`common::init_repo`). Never read or write `~/.bondsymphonic/workspaces.json`, `agents.json`, `homes/` etc., never point a test at a real repository, never launch the IDE against the real daemon (smoke tests always set `BS_DAEMON_ADDR`, `BS_SETTINGS_PATH`, `BS_STATE_PATH` to a temp dir). Test-only UI hooks act only on the workspace id a fixture named (`menuTestTarget`), never window-wide. No desktop input injection.
- **bwrap tests skip** with `eprintln!("SKIP: bwrap unavailable"); return;` when the local `bwrap_available()` probe (copy of the one in `crates/daemon/tests/sandbox_integration.rs:35`) fails, and the file is `#![cfg(target_os = "linux")]` or the module is `#[cfg(target_os = "linux")]`.
- **Comment style:** explain *why* in full sentences, as the surrounding code does. No comments that restate the code.
- **Widget tests** are `extern "C" std::int32_t bs_widget_test_<name>()` at the foot of their `.cpp` inside `#if defined(BS_WIDGET_TESTS)`, return `0` on pass and a distinct small integer per failure, and are registered in `crates/ide/tests/qobject_smoke.rs` (`cpp_widgets`) in both the `extern "C"` block and the `checks` array, whose declared length must be bumped.

## How to run

All commands are run from the PowerShell tool unless stated. The Bash tool is Git Bash: prefix any `wsl` call there with `MSYS_NO_PATHCONV=1`.

- **Proto (Windows is fine):**
  `$env:CARGO_TARGET_DIR="C:\git\BondSymphonic-inplace\target"; cargo test -p bondsymphonic-proto`
- **Daemon (always in WSL).** Build the daemon binary first; bwrap tests run `sandbox-init` out of it:
  ```
  wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic-inplace && export CARGO_TARGET_DIR=~/.bondsymphonic/target-inplace BS_DAEMON_EXE=~/.bondsymphonic/target-inplace/debug/bondsymphonic-daemon && cargo build -p bondsymphonic-daemon && cargo test -p bondsymphonic-daemon --test <file_stem> -- <filter>"
  ```
  `<file_stem>` is e.g. `in_place_sandbox`; drop `--test ...` for the whole suite. Only `target-inplace` is used under `~/.bondsymphonic`; nothing else there is touched.
- **IDE (Windows):**
  ```
  . .\scripts\env.ps1; $env:CARGO_TARGET_DIR="C:\git\BondSymphonic-inplace\target"; cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test <file_stem> -- <filter>
  ```
  Close any running `bondsymphonic-ide.exe` built from this target first. `packaged_smoke` fails by design without `BS_PACKAGED_EXE`; use `--no-fail-fast` for full runs and read past it.
- **Lint:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings` (IDE part on Windows with env.ps1; daemon part also in WSL: `cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings`).

## Execution order and parallel groups

| Task | Depends on | May run concurrently with | Files it owns |
|---|---|---|---|
| 1 Proto + fixtures | — | nothing (runs alone) | proto, every `WorkspaceInfo`/`WorkspaceCreateParams`/`RepoInfo` literal, daemon `workspace/mod.rs` |
| 2 Daemon `in_place` module | 1 | 5 | `workspace/in_place.rs` (new), `workspace/mod.rs` (one line), `tests/in_place_sandbox.rs` (new) |
| 3 Daemon lifecycle | 2 | 4, 5, 6 | `workspace/lifecycle.rs`, `tests/in_place_lifecycle.rs` (new) |
| 4 Daemon changes/merge/PR/inspect | 2 | 3, 5, 6 | `workspace/changes.rs`, `git/merge.rs`, `git/pr.rs`, `git/repo.rs`, `tests/in_place_changes.rs` (new) |
| 5 IDE model + controller | 1 | 2, 3, 4 | `model/app_state.rs`, `model/persistence.rs`, `qobjects/group_model.rs`, `qobjects/app_controller.rs`, two call lines in `cpp/MainWindow.cpp`, `AgentTab` fixtures, appended modules in `model_tests.rs`/`qobject_smoke.rs`, `persistence_tests.rs` |
| 6 IDE widgets + smoke | 5 | 3, 4 | `cpp/{NewAgentDialog,MainWindow,ChangesToolbar,ExplorerDock,WorkspaceBanner,AgentArea,GroupBar,CloseGroupDialog}.*`, `cpp/WorkspaceLabel.h`, `cpp_widgets` array, appended module in `smoke.rs` |
| 7 Docs | 3, 4, 6 | — | the three design docs, `docs/user-guide.md` |
| 8 Final verification | 7 | — | none (fixes go back to the owning task's files) |

## Spec resolutions (decided while planning; the lead should confirm R1 and R3)

- **R1 — `.git/commondir` is a sandbox escape the spec misses.** Measured in the distro (git 2.43, bwrap 0.9): an agent with a writable `.git` can write `.git/commondir` pointing at a directory of its own with a `config` holding `core.fsmonitor`, and the next `git status` **on the host** runs it — even with `GIT_DIR` pinned. `GIT_COMMON_DIR` pinned stops it for the daemon, but not for the user's own git. Resolution: before every sandbox start the daemon writes `.git/commondir` containing `.\n` (a no-op: the common dir is the git dir itself — verified with git 2.43 and Git for Windows 2.52: status, commit, switch, stash, `worktree add/remove`, gc, fsck all work) and binds it read-only; Close removes it again if it still holds exactly `.\n`. `prepare` refuses a repository that already has a `commondir` with other content.
- **R2 — `.git/worktrees` must be read-only too.** Otherwise an in-place agent can write `config.worktree` of a *worktree* workspace of the same repository, whose daemon-side `status` then runs it. The same goes for `.git/remotes` and `.git/branches`, the legacy remote definitions `git fetch <name>` / `git push <name>` read (an agent could redirect a name the user types). The daemon creates each of the three if missing (git creates them on demand; an empty one changes nothing), records which ones it created in a daemon-owned file `<data>/in-place/<id>.created` (outside every sandbox), binds all three read-only, and Close removes each one again only if the daemon created it and it is still empty. (`hooks` and `info` are also created when missing, but stay: git's own `init` makes them.)
- **R3 — embedded repositories run their own config.** Measured: an agent that `git init`s `sub/` in the tree, sets `core.fsmonitor` in `sub/.git/config` and commits the gitlink makes every `git status`/`git diff` in the parent run it — the user's, and the **daemon's** (for both workspace kinds; this is a pre-existing hole in the worktree kind as well). `-c diff.ignoreSubmodules=all` is not enough (a `.gitmodules` `ignore = none` overrides it); the explicit `--ignore-submodules=all` flag is. Resolution: every daemon-side status/diff passes the flag (Tasks 3 and 4). The user-side risk is documented as a residual risk. Not covered here and reported as follow-up: the rebase step in `git/merge.rs` (`worktree_git().run(.., ["rebase", ..])`) has no such flag.
- **R4 — `rm -rf .git` does not "fail" cleanly.** Measured: it exits non-zero and `.git`, `config`, `hooks`, `info` (the mount points) survive, but `HEAD`, `index`, `objects/`, `refs/`, `logs/` are deleted. The tests assert only what holds (non-zero exit, `.git` still a directory, `config` unchanged, `hooks` still there); the user guide says an agent can destroy the repository's history just as it can delete files.
- **R5 — bwrap creates missing bind targets on the writable repository.** Measured: a `--ro-bind` onto a missing directory creates an empty directory, and onto a missing file creates an empty `0444` file, in the user's repository. So every read-only target is created by the daemon first (`prepare`), `.git/modules` is bound only when it is a real directory, and `config.worktree` only when `extensions.worktreeConfig` is on. A mount point cannot be renamed or removed (`EBUSY`), confirmed for `.git` and for the root.
- **R6 — `repo.inspect` needs two more fields than the spec names.** The dialog must show "the checked-out branch", but `default_branch` is `origin/HEAD` first. Added `head_branch: Option<String>` (`None` when detached). And the dialog must disable the choice for a linked worktree, which `repo.inspect` does not report; added `in_place_refusal: Option<String>`, the same sentence the daemon refuses with (bare repositories are already an `inspect` error).
- **R7 — the daemon's own refusals.** "The existing refusals apply unchanged" was only true of `init_if_missing`. For in-place create the daemon refuses `/`, the daemon user's `$HOME`, a root inside the data directory **and a root that contains the data directory** (the rw bind of the root would otherwise bring every other workspace's home back into the sandbox over the tmpfs mask). A root whose `.git` is a file (separate git dir, submodule checkout) is refused like a linked worktree.
- **R8 — unborn `HEAD`.** `init_if_missing` always commits, but a user repository can have no commits. Changes and diff then measure against the empty tree `4b825dc642cb6eb9a060e54bf8d69288fbee4904`.
- **R9 — IDE gaps.** Rebase, Squash and Discard are merge-shaped too: the toolbar hides all five git actions for an in-place workspace, not only Merge and Create PR. The Close group dialog offers an in-place workspace only `Keep (move to Unsorted)` and `Close (files are kept)`, and its discard confirmation skips it. The remembered dialog mode is `StateFile.new_agent_in_place` (state.json, like `recent_repos`), written by the window after an accepted dialog only.
- **R10 — `.git/config` is read-only, so git commands that write config fail inside the sandbox** (`git switch -c x origin/x` tracking setup, `git push -u`, `git remote add`, `git sparse-checkout`). Documented, not mitigated.

## File Structure

**New**

| File | Responsibility |
|---|---|
| `crates/daemon/src/workspace/in_place.rs` | `InPlaceLayout` (paths, pinned git, `prepare`/`release`, bind lists, repository check), `head_branch`, `diff_base`, `in_place_refusal`, `target_refusal`, `nothing_to_merge`, constants. |
| `crates/daemon/tests/in_place_sandbox.rs` | The module's own tests, including a hand-built bwrap sandbox. |
| `crates/daemon/tests/in_place_lifecycle.rs` | Create / refusals / destroy byte-identity / restore / restart / status, noop and bwrap. |
| `crates/daemon/tests/in_place_changes.rs` | Changes and diff against `HEAD`, merge/PR refusal, `repo.inspect` fields, submodule hardening. |

**Modified:** see the ownership table above.

---

## Task 1: Protocol version 2, `WorkspaceKind` and the new fields

Runs alone. Touches many test fixtures, mechanically.

**Files:**
- Modify: `crates/proto/src/types.rs` (`WorkspaceKind`, `WorkspaceInfo.kind`, `RepoInfo` fields)
- Modify: `crates/proto/src/request.rs` (`WorkspaceCreateParams.in_place`, `examples()`)
- Modify: `crates/proto/src/lib.rs` (`PROTOCOL_VERSION = 2`)
- Modify: `crates/proto/src/event.rs:138` (example literal)
- Modify: `crates/daemon/src/workspace/mod.rs` (`Workspace.kind`, `info()`)
- Modify: `crates/daemon/src/workspace/lifecycle.rs:655,1210` (`Workspace` literals), `crates/daemon/src/git/repo.rs:51,402` (`RepoInfo` literals)
- Modify: `crates/ide/src/main.rs:36,42`, `crates/ide/src/qobjects/app_controller.rs:2382,2412`, `crates/ide/src/qobjects/smoke.rs:308`, `crates/ide/src/qobjects/group_model.rs:1172`
- Modify (fixtures): every `WorkspaceCreateParams {` and `WorkspaceInfo {` literal under `crates/daemon/tests`, `crates/ide/tests`, `crates/proto/tests`; `crates/daemon/tests/registry.rs:6`
- Test: `crates/proto/tests/roundtrip.rs` (append), `crates/daemon/tests/server_integration.rs:697` (rewrite one test, add one), `crates/ide/tests/client_tests.rs:647-702`, `crates/ide/tests/reconnect_tests.rs:554,672`

**Interfaces:**
- Produces:
  ```rust
  // bondsymphonic_proto
  pub enum WorkspaceKind { #[default] Worktree, InPlace }   // Copy, Eq, Default, serde snake_case
  WorkspaceInfo { .., #[serde(default)] pub kind: WorkspaceKind, .. }
  WorkspaceCreateParams { .., #[serde(default)] in_place: bool }
  RepoInfo { .., #[serde(default)] pub head_branch: Option<String>,
                 #[serde(default)] pub in_place_refusal: Option<String>,
                 #[serde(default)] pub hooks_path_in_tree: Option<String> }
  pub const PROTOCOL_VERSION: u32 = 2;
  // bondsymphonic_daemon::workspace
  Workspace { .., #[serde(default)] pub kind: WorkspaceKind }
  ```

- [ ] **Step 1: Write the failing proto tests.** Append to `crates/proto/tests/roundtrip.rs`:

```rust
/// In-place workspaces, 2026-09-17: the second kind of workspace, and the
/// version bump that keeps an older daemon from quietly making a worktree.
mod in_place_protocol {
    use bondsymphonic_proto::*;

    #[test]
    fn the_protocol_is_version_two_and_a_silent_peer_is_version_one() {
        assert_eq!(PROTOCOL_VERSION, 2);
        assert_eq!(peer_protocol_version(None), 1);
    }

    #[test]
    fn a_workspace_kind_is_spelled_in_snake_case_and_defaults_to_worktree() {
        assert_eq!(serde_json::to_value(WorkspaceKind::InPlace).unwrap(), "in_place");
        assert_eq!(serde_json::to_value(WorkspaceKind::Worktree).unwrap(), "worktree");
        assert_eq!(WorkspaceKind::default(), WorkspaceKind::Worktree);
        // A registry or a reply written before the field existed.
        let old = r#"{
            "id":"ws_1","name":"alpha","repo_path":"/r","base_branch":"main",
            "branch":"bs/alpha/work","worktree_path":"/wt","created_at":"t",
            "allowlist":[],"state":"ready","agents":[],"runs":[]
        }"#;
        let info: WorkspaceInfo = serde_json::from_str(old).unwrap();
        assert_eq!(info.kind, WorkspaceKind::Worktree);
        let v = serde_json::to_value(WorkspaceInfo {
            kind: WorkspaceKind::InPlace,
            ..info
        })
        .unwrap();
        assert_eq!(v["kind"], "in_place");
    }

    #[test]
    fn a_create_without_in_place_is_a_worktree_create() {
        let p: WorkspaceCreateParams =
            serde_json::from_str(r#"{"repo_path":"/r","base_branch":"main","name":"a"}"#).unwrap();
        assert!(!p.in_place);
        let p: WorkspaceCreateParams = serde_json::from_str(
            r#"{"repo_path":"/r","base_branch":"","name":"a","in_place":true}"#,
        )
        .unwrap();
        assert!(p.in_place);
    }

    #[test]
    fn repo_info_without_the_in_place_fields_reads_as_none() {
        let info: RepoInfo = serde_json::from_str(
            r#"{"default_branch":"main","branches":["main"],"is_dirty":false,"remotes":[]}"#,
        )
        .unwrap();
        assert_eq!(info.head_branch, None);
        assert_eq!(info.in_place_refusal, None);
        assert_eq!(info.hooks_path_in_tree, None);
    }
}
```

- [ ] **Step 2: Run and see it fail to compile.**
Run: `$env:CARGO_TARGET_DIR="C:\git\BondSymphonic-inplace\target"; cargo test -p bondsymphonic-proto --test roundtrip in_place_protocol`
Expected: FAIL, `cannot find type WorkspaceKind`.

- [ ] **Step 3: Add the types.** In `crates/proto/src/types.rs`, directly after `enum WorkspaceState`:

```rust
/// Which of the two shapes a workspace has.
///
/// `Worktree` is the original: a `bs/<name>/work` branch checked out in a
/// worktree of the daemon's own. `InPlace` is an agent working directly in the
/// repository's checkout, on whatever branch is checked out there; its
/// `worktree_path` is the repository root and nothing is ever merged out of it.
/// Defaults to `Worktree`, which is what every registry and reply written before
/// the field existed describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Worktree,
    InPlace,
}
```

In `WorkspaceInfo`, after `allowlist`:

```rust
    /// Which kind of workspace this is. See [`WorkspaceKind`].
    #[serde(default)]
    pub kind: WorkspaceKind,
```

In `RepoInfo`, after `exists`:

```rust
    /// The branch checked out in the repository, or `None` when `HEAD` is
    /// detached or the path is not a repository. `default_branch` is the
    /// remote's answer and is not this: an in-place workspace works on what is
    /// checked out, and the New Agent dialog shows that.
    #[serde(default)]
    pub head_branch: Option<String>,
    /// Why an agent cannot work directly in this checkout -- a linked worktree,
    /// a `.git` that is a file -- as the sentence `workspace.create` would
    /// refuse with, or `None` when it can.
    #[serde(default)]
    pub in_place_refusal: Option<String>,
    /// The repository's effective `core.hooksPath`, relative to the root, when
    /// it resolves to a directory inside the working tree (husky does this):
    /// hooks there are files an in-place agent can edit and the user's own git
    /// runs. `None` otherwise.
    #[serde(default)]
    pub hooks_path_in_tree: Option<String>,
```

In `crates/proto/src/request.rs`, `WorkspaceCreateParams`, turn `init_if_missing: bool` into `init_if_missing: bool,` and add after it:

```rust
    /// Work directly in the checkout at `repo_path` instead of a new worktree.
    /// `base_branch` is then ignored and may be empty. Off by default, and the
    /// protocol version went to 2 with it: an older daemon would ignore the
    /// flag and quietly make a worktree.
    #[serde(default)]
    in_place: bool
```

In `examples()`, the `WorkspaceCreate` entry gets `in_place: true,`. In `crates/proto/src/lib.rs` set `pub const PROTOCOL_VERSION: u32 = 2;` and add a doc line: "Version 2 added in-place workspaces (`WorkspaceCreateParams::in_place`)." In `crates/proto/src/event.rs` the `WorkspaceInfo` example gets `kind: WorkspaceKind::InPlace,`.

- [ ] **Step 4: Run the proto suite.**
Run: `cargo test -p bondsymphonic-proto`
Expected: compile errors in `tests/roundtrip.rs` for the three existing literals (`request_envelope_shape`, `workspace_info_carries_agent_records_beside_the_plain_ids`, `repo_info_from_a_daemon_that_predates_the_non_repo_answer`). Add `in_place: false,`, `kind: WorkspaceKind::Worktree,` and `head_branch: None, in_place_refusal: None, hooks_path_in_tree: None,` respectively. Re-run: PASS.

- [ ] **Step 5: Daemon model.** In `crates/daemon/src/workspace/mod.rs` import `WorkspaceKind` and add to `Workspace`, after `runs`:

```rust
    /// Which kind of workspace this is. Absent from a registry written before
    /// in-place workspaces existed, which only ever held worktrees.
    #[serde(default)]
    pub kind: WorkspaceKind,
```

and `kind: self.kind,` in `Workspace::info()`. In `lifecycle.rs` both `Workspace { .. }` literals get `kind: WorkspaceKind::Worktree,`; in `repo.rs` both `RepoInfo { .. }` literals get `head_branch: None, in_place_refusal: None, hooks_path_in_tree: None,` (Task 4 fills them in). `crates/daemon/tests/registry.rs` `sample()` gets `kind: bondsymphonic_proto::WorkspaceKind::Worktree,`.

- [ ] **Step 6: Fix every create literal.** Run in the Bash tool:

```bash
cd /c/git/BondSymphonic-inplace
grep -rl "init_if_missing: \(true\|false\)," crates --include=*.rs \
  | grep -v crates/proto/src/request.rs \
  | xargs perl -0pi -e 's/^([ \t]*)init_if_missing: (true|false),\n(?![ \t]*in_place:)/$1init_if_missing: $2,\n$1in_place: false,\n/mg'
grep -rn "WorkspaceCreateParams {" crates --include=*.rs | wc -l
grep -rn "in_place: \(true\|false\)," crates --include=*.rs | wc -l
```

The two counts differ only by the two `app_controller.rs` sites, which use the shorthand `init_if_missing,`: add `in_place: false,` there by hand. Task 5 threads the real value.

- [ ] **Step 7: Fix every `WorkspaceInfo` literal.** Add `kind: bondsymphonic_proto::WorkspaceKind::Worktree,` (full path, so no `use` line changes) to each literal the compilers name:

```
wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic-inplace && CARGO_TARGET_DIR=~/.bondsymphonic/target-inplace cargo build -p bondsymphonic-daemon --tests 2>&1 | grep -A3 'missing field' | head -80"
. .\scripts\env.ps1; $env:CARGO_TARGET_DIR="C:\git\BondSymphonic-inplace\target"; cargo build -p bondsymphonic-ide --tests --features bondsymphonic-ide/require-qt 2>&1 | Select-String "missing field" -Context 0,3
```

Known sites: `crates/ide/src/qobjects/group_model.rs:1172`, `crates/ide/tests/model_tests.rs:7`, `packaged_smoke.rs:298`, `persistence_tests.rs:33`, `qobject_smoke.rs:25`, `reconnect_tests.rs:493`, `restore_tests.rs:257,338,716,999`, `smoke.rs:643` and any `smoke.rs` module that builds one directly. Repeat until both builds are clean.

- [ ] **Step 8: Protocol-version tests.** In `crates/daemon/tests/server_integration.rs` replace `hello_without_a_protocol_version_is_accepted_as_version_one` (and its doc comment) with:

```rust
/// A `hello` with no `protocol_version` at all is a client built before the
/// field existed, which speaks version 1. This daemon speaks 2 -- in-place
/// workspaces -- so it is refused with both numbers, like any other mismatch,
/// and the socket closes.
#[tokio::test]
async fn hello_without_a_protocol_version_is_refused_as_version_one() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    let line = serde_json::json!({
        "type": "request",
        "id": 1,
        "method": "hello",
        "params": { "token": token, "client_version": "0.1.0" }
    })
    .to_string();
    send_line(&mut w, &line).await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            error: Some(e),
            ..
        } => assert_eq!(protocol_mismatch_versions(&e), Some((PROTOCOL_VERSION, 1))),
        other => panic!("unexpected {other:?}"),
    }
    assert!(recv(&mut r).await.is_none(), "connection should be closed");
    cancel.cancel();
}

/// A version-1 client that does say so is refused the same way.
#[tokio::test]
async fn hello_with_protocol_version_one_is_refused() {
    let (port, token, cancel, _h) = start().await;
    let (mut r, mut w) = connect(port).await;
    send(
        &mut w,
        1,
        Request::Hello(HelloParams {
            token,
            client_version: "0.1.0".into(),
            protocol_version: Some(1),
        }),
    )
    .await;
    match recv(&mut r).await.unwrap() {
        ServerMessage::Response {
            id: 1,
            error: Some(e),
            ..
        } => assert_eq!(protocol_mismatch_versions(&e), Some((PROTOCOL_VERSION, 1))),
        other => panic!("unexpected {other:?}"),
    }
    cancel.cancel();
}
```

In `crates/ide/tests/client_tests.rs`:
- `a_daemon_on_another_protocol_version_fails_the_handshake` uses `MismatchAnswer::Answers(Some(PROTOCOL_VERSION + 1))` and asserts `matches!(err, ClientError::ProtocolMismatch { daemon, client } if daemon == PROTOCOL_VERSION + 1 && client == PROTOCOL_VERSION)`.
- `a_daemon_that_refuses_our_version_is_reported_as_a_mismatch` asserts `matches!(err, ClientError::ProtocolMismatch { daemon: 7, client } if client == PROTOCOL_VERSION)`.
- `a_pre_m7_daemon_still_connects` becomes:

```rust
/// A daemon built before the field existed answers without it, which is
/// version 1. This IDE speaks 2, so the handshake stops there.
#[tokio::test]
async fn a_pre_m7_daemon_is_a_version_one_daemon_and_is_refused() {
    let addr = fake_daemon_speaking("secret", MismatchAnswer::Answers(None)).await;
    let err = DaemonClient::connect(addr, "secret", "0.1.0")
        .await
        .err()
        .expect("a version-1 daemon must not be accepted");
    assert!(
        matches!(err, ClientError::ProtocolMismatch { daemon: 1, client } if client == PROTOCOL_VERSION),
        "got {err:?}"
    );
}
```

In `crates/ide/tests/reconnect_tests.rs`: the fake answers `protocol_version: Some(PROTOCOL_VERSION + 1)`; `MISMATCH_TEXT` says `(daemon 3, IDE 2)` instead of `(daemon 2, IDE 1)`; rename `fake_daemon_speaking_protocol_two` to `fake_daemon_speaking_the_next_protocol` and fix its doc comment. In `crates/ide/src/main.rs` the version test expects `(protocol 2)` and `PROTOCOL_VERSION == 2`.

- [ ] **Step 9: Run everything touched.**
Run: `cargo test -p bondsymphonic-proto`; in WSL `cargo build -p bondsymphonic-daemon --tests` and `cargo test -p bondsymphonic-daemon --test server_integration --test registry`; on Windows `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test client_tests --test reconnect_tests --test model_tests --test qobject_smoke --bin bondsymphonic-ide`.
Expected: PASS. Then `cargo fmt --all` and check that `git diff --stat` lists only files this task changed.

- [ ] **Step 10: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/proto crates/daemon/src/workspace/mod.rs crates/daemon/src/workspace/lifecycle.rs crates/daemon/src/git/repo.rs crates/daemon/tests crates/ide/src crates/ide/tests"
git add $P
git commit -m "feat(proto): protocol 2 carries in-place workspaces" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 2: The daemon's `in_place` module, proven in a real bwrap sandbox

Depends on Task 1. May run beside Task 5.

**Files:**
- Create: `crates/daemon/src/workspace/in_place.rs`
- Modify: `crates/daemon/src/workspace/mod.rs` (`pub mod in_place;`, `DataDirs::in_place_record`)
- Test: `crates/daemon/tests/in_place_sandbox.rs` (new)

**Interfaces:**
- Consumes: `bondsymphonic_proto::WorkspaceKind` (Task 1); existing `crate::git::{Git, path_arg}`, `crate::git::repo::{RepoKind, canonical_ish, init_target_refusal, bare_repository_error}`.
- Produces (`bondsymphonic_daemon::workspace::in_place`):
  ```rust
  pub const IN_PLACE_REASON: &str = "in_place";
  pub const COMMONDIR_GUARD: &str = ".\n";
  pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
  pub struct InPlaceLayout { pub root: PathBuf, pub git_dir: PathBuf, pub no_hooks_dir: PathBuf }
  impl InPlaceLayout {
      pub fn new(root: &Path, no_hooks_dir: &Path) -> Self;
      pub fn config(&self) -> PathBuf; pub fn commondir(&self) -> PathBuf;
      pub fn hooks(&self) -> PathBuf; pub fn info(&self) -> PathBuf;
      pub fn modules(&self) -> PathBuf; pub fn worktrees(&self) -> PathBuf;
      pub fn remotes(&self) -> PathBuf; pub fn branches(&self) -> PathBuf;
      pub fn config_worktree(&self) -> PathBuf;
      pub fn git(&self) -> Git;
      pub fn check_repository(&self) -> Result<(), RpcError>;
      pub async fn worktree_config_enabled(&self) -> Result<bool, RpcError>;
      pub fn prepare(&self, worktree_config: bool, record: &Path) -> Result<(), RpcError>;
      pub fn release(&self, record: &Path);
      pub fn rw_binds(&self) -> Vec<PathBuf>;
      pub fn late_ro_binds(&self, worktree_config: bool) -> Vec<PathBuf>;
  }
  pub async fn head_branch(git: &Git, root: &Path) -> Result<String, RpcError>;   // "" when detached
  pub async fn diff_base(git: &Git, root: &Path) -> Result<String, RpcError>;     // HEAD sha, or EMPTY_TREE when unborn
  pub fn in_place_refusal(kind: &RepoKind, root: &Path) -> Option<String>;
  pub fn target_refusal(root: &Path, data_root: &Path) -> Option<String>;
  pub fn nothing_to_merge() -> RpcError;   // InvalidParams, data.reason = "in_place"
  pub const ON_DEMAND_DIRS: [&str; 3] = ["worktrees", "remotes", "branches"];
  // bondsymphonic_daemon::workspace
  impl DataDirs { pub fn in_place_record(&self, id: &WorkspaceId) -> PathBuf }  // <root>/in-place/<id>.created
  ```

- [ ] **Step 1: Write the failing tests.** Create `crates/daemon/tests/in_place_sandbox.rs`:

```rust
//! The building blocks of an in-place workspace: what the daemon does to a
//! repository's `.git` before an agent may work in it, the git it runs there
//! itself, and -- in a real bubblewrap sandbox -- that git still works for the
//! agent while everything a git outside the sandbox would execute stays out of
//! its reach.

mod common;

use bondsymphonic_daemon::git::repo::RepoKind;
use bondsymphonic_daemon::workspace::in_place::{self, InPlaceLayout};
use bondsymphonic_proto::ErrorCode;
use std::path::{Path, PathBuf};

fn layout(dir: &Path, repo: &Path) -> InPlaceLayout {
    let no_hooks = dir.join("nohooks");
    std::fs::create_dir_all(&no_hooks).unwrap();
    InPlaceLayout::new(repo, &no_hooks)
}

/// Where the daemon would record what it created for this test's workspace:
/// under the data directory, never in the repository.
fn record(dir: &Path) -> PathBuf {
    dir.join("data/in-place/ws_test.created")
}

/// The top-level names in `.git`, sorted.
fn git_dir_entries(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn prepare_makes_only_the_documented_entries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // Whether `git init` makes `branches` depends on its template; take the
    // question away so the expected list does not.
    for name in ["hooks", "info", "branches"] {
        let _ = std::fs::remove_dir_all(repo.join(".git").join(name));
    }
    let before = git_dir_entries(&repo);
    let l = layout(dir.path(), &repo);

    l.prepare(false, &record(dir.path())).unwrap();

    let mut added: Vec<String> = git_dir_entries(&repo)
        .into_iter()
        .filter(|n| !before.contains(n))
        .collect();
    added.sort();
    assert_eq!(
        added,
        ["branches", "commondir", "hooks", "info", "remotes", "worktrees"]
    );
    assert_eq!(std::fs::read(l.commondir()).unwrap(), b".\n");
    // Only the on-demand directories are recorded, and only once each.
    let recorded = std::fs::read_to_string(record(dir.path())).unwrap();
    let mut lines: Vec<&str> = recorded.lines().collect();
    lines.sort();
    assert_eq!(lines, ["branches", "remotes", "worktrees"]);
    // Idempotent: a restart prepares the same repository again, and what the
    // first start created is still remembered as the daemon's.
    l.prepare(false, &record(dir.path())).unwrap();
    assert_eq!(std::fs::read_to_string(record(dir.path())).unwrap(), recorded);
    // git still reads the repository as its own common dir.
    assert_eq!(
        common::git_out(&repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"]),
        common::git_out(&repo, &["rev-parse", "--path-format=absolute", "--git-dir"])
    );
    common::git_ok(&repo, &["status", "--porcelain"]);
}

#[test]
fn prepare_creates_config_worktree_only_with_the_extension_and_keeps_its_content() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.prepare(false, &record(dir.path())).unwrap();
    assert!(!l.config_worktree().exists());
    l.prepare(true, &record(dir.path())).unwrap();
    assert_eq!(std::fs::read(l.config_worktree()).unwrap(), b"");
    // The user's own file is theirs: never truncated.
    std::fs::write(l.config_worktree(), "[core]\n\tsparseCheckout = false\n").unwrap();
    l.prepare(true, &record(dir.path())).unwrap();
    assert!(std::fs::read_to_string(l.config_worktree())
        .unwrap()
        .contains("sparseCheckout"));
}

#[test]
fn prepare_refuses_a_commondir_it_did_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    std::fs::write(repo.join(".git/commondir"), "/somewhere/else\n").unwrap();
    let err = layout(dir.path(), &repo).prepare(false, &record(dir.path())).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(err.message.contains("commondir"), "{}", err.message);
    assert_eq!(
        std::fs::read_to_string(repo.join(".git/commondir")).unwrap(),
        "/somewhere/else\n",
        "the refusal must not touch the file"
    );
}

#[test]
fn release_takes_back_only_what_the_daemon_made_and_left_empty() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let before = git_dir_entries(&repo);
    let l = layout(dir.path(), &repo);
    l.prepare(false, &record(dir.path())).unwrap();
    l.release(&record(dir.path()));
    assert_eq!(git_dir_entries(&repo), before);
    assert!(!record(dir.path()).exists(), "the record goes with the release");

    // A worktrees dir with a registration in it is git's now, and stays.
    l.prepare(false, &record(dir.path())).unwrap();
    common::git_ok(
        &repo,
        &["worktree", "add", "-q", "-b", "side", &dir.path().join("side").to_string_lossy()],
    );
    l.release(&record(dir.path()));
    assert!(l.worktrees().is_dir());
    assert!(!l.commondir().exists());

    // An empty `remotes` the user already had is theirs: not recorded, not
    // removed.
    let other = tempfile::tempdir().unwrap();
    let repo = common::init_repo(other.path());
    std::fs::create_dir_all(repo.join(".git/remotes")).unwrap();
    let l = layout(other.path(), &repo);
    l.prepare(false, &record(other.path())).unwrap();
    l.release(&record(other.path()));
    assert!(l.remotes().is_dir());

    // A record naming anything but the three on-demand directories is ignored.
    std::fs::create_dir_all(record(other.path()).parent().unwrap()).unwrap();
    std::fs::write(record(other.path()), "hooks\n../..\nobjects\n").unwrap();
    l.release(&record(other.path()));
    assert!(l.hooks().is_dir() && repo.join(".git/objects").is_dir());
}

#[test]
fn late_binds_follow_what_the_repository_has() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    let names = |v: Vec<PathBuf>| -> Vec<String> {
        v.iter()
            .map(|p| p.strip_prefix(&repo).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    };
    assert_eq!(names(l.rw_binds()), ["", ".git"]);
    assert_eq!(
        names(l.late_ro_binds(false)),
        [
            ".git/config",
            ".git/commondir",
            ".git/hooks",
            ".git/info",
            ".git/worktrees",
            ".git/remotes",
            ".git/branches"
        ]
    );
    std::fs::create_dir_all(l.modules()).unwrap();
    assert_eq!(
        names(l.late_ro_binds(true)),
        [
            ".git/config",
            ".git/commondir",
            ".git/hooks",
            ".git/info",
            ".git/worktrees",
            ".git/remotes",
            ".git/branches",
            ".git/modules",
            ".git/config.worktree"
        ]
    );
}

#[test]
fn check_repository_names_the_folder_and_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    l.check_repository().unwrap();
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    let err = l.check_repository().unwrap_err();
    assert_eq!(
        err.message,
        format!(
            "The repository {} is missing or is no longer a git repository. Close the \
             workspace, or restore the folder and press Retry.",
            repo.display()
        )
    );
}

#[tokio::test]
async fn head_branch_and_diff_base_cover_detached_and_unborn() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let l = layout(dir.path(), &repo);
    assert_eq!(in_place::head_branch(&l.git(), &repo).await.unwrap(), "main");
    let head = common::git_out(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(in_place::diff_base(&l.git(), &repo).await.unwrap(), head);
    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    assert_eq!(in_place::head_branch(&l.git(), &repo).await.unwrap(), "");

    let unborn = dir.path().join("unborn");
    std::fs::create_dir_all(&unborn).unwrap();
    common::git_ok(&unborn, &["init", "-q", "-b", "trunk"]);
    let u = layout(dir.path(), &unborn);
    assert_eq!(in_place::head_branch(&u.git(), &unborn).await.unwrap(), "trunk");
    assert_eq!(in_place::diff_base(&u.git(), &unborn).await.unwrap(), in_place::EMPTY_TREE);
}

#[test]
fn only_a_root_with_its_own_git_directory_can_be_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    assert_eq!(in_place::in_place_refusal(&RepoKind::Root, &repo), None);
    let why = in_place::in_place_refusal(&RepoKind::Worktree, &repo).unwrap();
    assert!(why.contains("linked worktree"), "{why}");

    let separate = dir.path().join("separate");
    std::fs::create_dir_all(&separate).unwrap();
    common::git_ok(
        &separate,
        &["init", "-q", "--separate-git-dir", &dir.path().join("sep.git").to_string_lossy()],
    );
    let why = in_place::in_place_refusal(&RepoKind::Root, &separate).unwrap();
    assert!(why.contains(".git is a file"), "{why}");
    assert!(in_place::in_place_refusal(&RepoKind::Bare, &repo).is_some());
    assert!(in_place::in_place_refusal(&RepoKind::NotARepo, &repo).is_some());
}

#[test]
fn the_daemon_directory_and_the_filesystem_root_are_refused_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let data_inside = repo.join("data");
    std::fs::create_dir_all(&data_inside).unwrap();
    let why = in_place::target_refusal(&repo, &data_inside).unwrap();
    assert!(why.contains("contains the daemon's own data directory"), "{why}");
    let why = in_place::target_refusal(&data_inside.join("x"), &data_inside).unwrap();
    assert!(why.contains("inside the daemon's own data directory"), "{why}");
    assert!(in_place::target_refusal(Path::new("/"), &dir.path().join("d")).is_some());
    assert_eq!(in_place::target_refusal(&repo, &dir.path().join("elsewhere")), None);
}

#[test]
fn a_merge_refusal_carries_the_in_place_reason() {
    let e = in_place::nothing_to_merge();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert_eq!(e.data.unwrap()["reason"], "in_place");
    assert_eq!(
        e.message,
        "an in-place workspace has nothing to merge; commit and push from the checkout"
    );
}

/// The escape R1 in the plan closes, from the daemon's side: a planted
/// `commondir` with a config of its own must not reach the pinned git.
#[cfg(unix)]
#[tokio::test]
async fn the_pinned_git_ignores_a_planted_commondir() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let evil = repo.join(".git/evil");
    std::fs::create_dir_all(&evil).unwrap();
    // A complete common dir of the agent's own: git refuses one without
    // objects and refs, and then the attack proves nothing.
    let git_dir = repo.join(".git");
    assert!(std::process::Command::new("cp")
        .arg("-r")
        .arg(git_dir.join("objects"))
        .arg(git_dir.join("refs"))
        .arg(&evil)
        .status()
        .unwrap()
        .success());
    let marker = dir.path().join("PWNED");
    std::fs::write(
        evil.join("config"),
        format!(
            "{}[core]\n\tfsmonitor = touch {}; false\n",
            std::fs::read_to_string(repo.join(".git/config")).unwrap(),
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(repo.join(".git/commondir"), format!("{}\n", evil.display())).unwrap();

    let l = layout(dir.path(), &repo);
    l.git().run(&repo, &["status", "--porcelain"]).await.unwrap();
    assert!(!marker.exists(), "the daemon's git followed a planted commondir");
}

#[cfg(target_os = "linux")]
mod bwrap {
    use super::*;
    use bondsymphonic_daemon::sandbox::{backend_for, SandboxCommand, SandboxHandle, SandboxSpec};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn run_in(handle: &Arc<dyn SandboxHandle>, script: &str) -> (i32, String) {
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["sh".into(), "-c".into(), script.into()],
                env: vec![],
                cwd: None,
                pty: None,
            })
            .await
            .unwrap();
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
        (child.exit.await.unwrap(), out)
    }

    /// The spec an in-place workspace gets, minus the parts (proxy, Claude,
    /// cache) that have nothing to do with `.git`. Home and run dir sit two
    /// levels under a data dir, as the backend expects.
    async fn sandbox_over(dir: &Path, l: &InPlaceLayout) -> Arc<dyn SandboxHandle> {
        let wc = l.worktree_config_enabled().await.unwrap();
        l.prepare(wc, &record(dir)).unwrap();
        let data = dir.join("data");
        let home = data.join("homes/ws_inplace");
        let run = data.join("run/ws_inplace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        // The agent's git refuses a repository it does not own otherwise; the
        // real daemon seeds the same line (`seed_home`).
        std::fs::write(home.join(".gitconfig"), "[safe]\n\tdirectory = *\n").unwrap();
        let same = |p: &PathBuf| (p.clone(), p.clone());
        let spec = SandboxSpec {
            id: "ws_inplace".into(),
            rw_binds: l.rw_binds().iter().map(same).collect(),
            ro_binds: vec![],
            late_ro_binds: l.late_ro_binds(wc).iter().map(same).collect(),
            home,
            run_dir: run,
            env: vec![],
            cwd: l.root.clone(),
        };
        backend_for("linux_bwrap").start(&spec).await.unwrap()
    }

    const PROBE: &str = r#"
root='@ROOT@'
cd "$root" || exit 99
try() { if sh -c "$2" >/dev/null 2>&1; then echo "$1: SUCCEEDED"; else echo "$1: refused"; fi; }
printf 'edited\n' >> README.md
git add README.md && git -c user.name=a -c user.email=a@a -c commit.gpgsign=false commit -q -m inside && echo "commit: ok"
git switch -q -c from-inside && echo "switch: ok"
git stash list >/dev/null && echo "stash: ok"
try config "printf x >> .git/config"
try hook "printf x > .git/hooks/pre-commit"
try info "printf x > .git/info/exclude"
try commondir "printf /tmp/evil > .git/commondir"
try commondir-unlink "rm -f .git/commondir"
try worktrees "mkdir .git/worktrees/planted"
try remotes "printf 'URL: /tmp/evil\n' > .git/remotes/origin"
try branches "printf '/tmp/evil\n' > .git/branches/origin"
try move-git "mv .git moved-git"
try replace-git "mkdir -p newgit && mv -T newgit .git"
try move-root "mv '$root' '$root-moved'"
try git-config "git config core.fsmonitor 'touch /tmp/pwned'"
try config-worktree "printf x > .git/config.worktree"
"#;

    #[tokio::test]
    async fn the_agent_uses_git_and_cannot_touch_what_a_host_git_executes() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        common::git_ok(&repo, &["config", "extensions.worktreeConfig", "true"]);
        let config_before = std::fs::read(repo.join(".git/config")).unwrap();
        // Template-dependent; see `prepare_makes_only_the_documented_entries`.
        let _ = std::fs::remove_dir_all(repo.join(".git/branches"));
        let l = layout(dir.path(), &repo);
        let before = git_dir_entries(&repo);
        let handle = sandbox_over(dir.path(), &l).await;

        // Nothing but the documented entries appeared, bwrap included.
        let added: Vec<String> = git_dir_entries(&repo)
            .into_iter()
            .filter(|n| !before.contains(n))
            .collect();
        assert_eq!(
            added,
            ["branches", "commondir", "config.worktree", "remotes", "worktrees"]
        );

        let (_, out) = run_in(&handle, &PROBE.replace("@ROOT@", &repo.to_string_lossy())).await;
        for expected in [
            "commit: ok",
            "switch: ok",
            "stash: ok",
            "config: refused",
            "hook: refused",
            "info: refused",
            "commondir: refused",
            "commondir-unlink: refused",
            "worktrees: refused",
            "remotes: refused",
            "branches: refused",
            "move-git: refused",
            "replace-git: refused",
            "move-root: refused",
            "git-config: refused",
            "config-worktree: refused",
        ] {
            assert!(out.lines().any(|l| l == expected), "expected {expected:?} in:\n{out}");
        }
        // The commit is in the repository, on the branch the agent made.
        assert_eq!(common::git_out(&repo, &["log", "-1", "--format=%s", "main"]), "inside");
        assert_eq!(common::git_out(&repo, &["branch", "--show-current"]), "from-inside");
        assert_eq!(std::fs::read(repo.join(".git/config")).unwrap(), config_before);
        assert_eq!(std::fs::read(l.commondir()).unwrap(), b".\n");
        handle.shutdown().await.unwrap();
    }

    /// Plan R4: the mount points survive, the rest of `.git` does not. The
    /// user guide says so; this pins what "survive" means.
    #[tokio::test]
    async fn removing_git_leaves_its_mount_points_and_config() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        let config_before = std::fs::read(repo.join(".git/config")).unwrap();
        let l = layout(dir.path(), &repo);
        let handle = sandbox_over(dir.path(), &l).await;
        let (code, _) = run_in(&handle, &format!("rm -rf '{}/.git'", repo.display())).await;
        assert_ne!(code, 0, "rm -rf .git must fail");
        assert!(repo.join(".git").is_dir());
        assert!(l.hooks().is_dir());
        assert_eq!(std::fs::read(repo.join(".git/config")).unwrap(), config_before);
        handle.shutdown().await.unwrap();
    }
}
```

- [ ] **Step 2: Run and see it fail to compile.**
Run (WSL command from "How to run") with `--test in_place_sandbox`.
Expected: FAIL, `could not find in_place in workspace`.

- [ ] **Step 3: Write the module.** In `crates/daemon/src/workspace/mod.rs` add `pub mod in_place;` (after `pub mod changes;`) and, in `impl DataDirs` after `run()`:

```rust
    /// Which of `.git`'s on-demand directories the daemon created in an
    /// in-place workspace's checkout, so Close takes back exactly those. Under
    /// the data root, which no sandbox can write.
    pub fn in_place_record(&self, id: &WorkspaceId) -> PathBuf {
        self.root.join("in-place").join(format!("{id}.created"))
    }
```

Then create `crates/daemon/src/workspace/in_place.rs`:

```rust
//! In-place workspaces: an agent working directly in a repository's own
//! checkout rather than in a worktree of the daemon's (in-place workspaces
//! design, 2026-09-17).
//!
//! What such a workspace shares with the user is their `.git`, and a `.git` is
//! code: its config names programs, its hooks are programs, and git follows
//! `commondir` to a config somewhere else entirely. The agent gets the git
//! directory read-write, as a mount of its own so it cannot be swapped out,
//! with every one of those entries bound read-only on top. Everything here is
//! the daemon's half of that arrangement, kept in one place so no worktree-only
//! path (`ref_dir`, `worktree_gitdir`, a private object directory) can be
//! reached for an in-place workspace by accident.

use crate::git::repo::{self, RepoKind};
use crate::git::{path_arg, Git};
use bondsymphonic_proto::{ErrorCode, RpcError};
use std::path::{Path, PathBuf};

/// The `data.reason` a merge or pull request on an in-place workspace is
/// refused with.
pub const IN_PLACE_REASON: &str = "in_place";

/// What the daemon writes to `.git/commondir` before an agent may work in the
/// checkout.
///
/// Git reads `commondir` from *any* git directory, not only a linked
/// worktree's, and takes config, refs and objects from wherever it points. An
/// agent that could write it could make the user's next `git status` run a
/// `core.fsmonitor` of its own choosing. `.` points the common directory at the
/// git directory itself, which is what it is anyway, and the file is then bound
/// read-only. Measured with git 2.43 and Git for Windows 2.52: status, commit,
/// switch, stash, worktree add and remove, gc and fsck all behave as without it.
pub const COMMONDIR_GUARD: &str = ".\n";

/// The empty tree, which git knows without it being in any object store. What
/// changes are measured against in a repository with no commits yet.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The directories under `.git` that git makes only when it needs them, which
/// the daemon makes (and binds read-only) when they are missing, and which
/// Close takes back when the daemon made them and they are still empty.
pub const ON_DEMAND_DIRS: [&str; 3] = ["worktrees", "remotes", "branches"];

/// The paths of one in-place workspace.
#[derive(Debug, Clone)]
pub struct InPlaceLayout {
    /// The repository root, which is also the workspace's `worktree_path`.
    pub root: PathBuf,
    /// `<root>/.git`, always a directory: [`in_place_refusal`] refuses the rest.
    pub git_dir: PathBuf,
    /// An empty daemon-owned directory, used as `core.hooksPath`. See
    /// [`crate::workspace::DataDirs::no_hooks`].
    pub no_hooks_dir: PathBuf,
}

/// True for a directory that is one, not a symlink to one: a bind follows
/// symlinks, and a symlink an earlier session planted must not decide what the
/// next one binds.
fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn exit_code(e: &RpcError) -> Option<i64> {
    e.data.as_ref().and_then(|d| d["exit_code"].as_i64())
}

fn io_error(what: &Path, e: std::io::Error) -> RpcError {
    RpcError::new(
        ErrorCode::IoError,
        format!("cannot prepare {}: {e}", what.display()),
    )
}

impl InPlaceLayout {
    pub fn new(root: &Path, no_hooks_dir: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            no_hooks_dir: no_hooks_dir.to_path_buf(),
        }
    }
    pub fn config(&self) -> PathBuf {
        self.git_dir.join("config")
    }
    pub fn commondir(&self) -> PathBuf {
        self.git_dir.join("commondir")
    }
    pub fn hooks(&self) -> PathBuf {
        self.git_dir.join("hooks")
    }
    pub fn info(&self) -> PathBuf {
        self.git_dir.join("info")
    }
    pub fn modules(&self) -> PathBuf {
        self.git_dir.join("modules")
    }
    pub fn worktrees(&self) -> PathBuf {
        self.git_dir.join("worktrees")
    }
    pub fn remotes(&self) -> PathBuf {
        self.git_dir.join("remotes")
    }
    pub fn branches(&self) -> PathBuf {
        self.git_dir.join("branches")
    }
    pub fn config_worktree(&self) -> PathBuf {
        self.git_dir.join("config.worktree")
    }

    /// Git for the daemon's own commands in the checkout.
    ///
    /// Discovery is taken out of the tree's hands the way
    /// [`crate::git::worktree::Layout::worktree_git`] does it, and
    /// `GIT_COMMON_DIR` is pinned as well, which is what stops a planted
    /// `commondir` (see [`COMMONDIR_GUARD`]). Hooks go to the empty directory.
    /// The repository's config is the user's own and read-only to the agent,
    /// so -- as for `daemon_git` -- the `NEUTRALISED_CONFIG` list is not
    /// applied: emptying `filter.lfs.clean` would corrupt the checkout.
    pub fn git(&self) -> Git {
        Git::new()
            .with_env("GIT_DIR", path_arg(&self.git_dir))
            .with_env("GIT_COMMON_DIR", path_arg(&self.git_dir))
            .with_env("GIT_WORK_TREE", path_arg(&self.root))
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir))
    }

    /// Whether the checkout is still there and still a repository, as the
    /// sentence the workspace's `Error` state carries when it is not.
    pub fn check_repository(&self) -> Result<(), RpcError> {
        if self.root.is_dir() && is_real_dir(&self.git_dir) && self.config().is_file() {
            return Ok(());
        }
        Err(RpcError::new(
            ErrorCode::IoError,
            format!(
                "The repository {} is missing or is no longer a git repository. Close the \
                 workspace, or restore the folder and press Retry.",
                self.root.display()
            ),
        ))
    }

    /// Whether git reads `config.worktree` in this repository.
    pub async fn worktree_config_enabled(&self) -> Result<bool, RpcError> {
        match self
            .git()
            .run(
                &self.root,
                &["config", "--bool", "--get", "extensions.worktreeConfig"],
            )
            .await
        {
            Ok(o) => Ok(o.stdout.trim() == "true"),
            // Exit 1 is "not set", which is off.
            Err(e) if exit_code(&e) == Some(1) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Makes every read-only bind target exist, before every sandbox start.
    ///
    /// bwrap creates a missing bind target itself -- an empty directory, or an
    /// empty read-only file -- and here the underlying mount is the user's
    /// writable repository, so the daemon makes each one first and knows
    /// exactly what it made: the `commondir` guard, and `hooks`, `info` and the
    /// [`ON_DEMAND_DIRS`] when they are missing, which git would create itself
    /// and which change nothing empty. `config.worktree` only matters with the
    /// extension on, and a file the user already has is theirs and is left as
    /// it is.
    ///
    /// Which on-demand directories this call created is appended to `record`,
    /// a daemon-owned file outside every sandbox
    /// ([`crate::workspace::DataDirs::in_place_record`]), so Close can take
    /// back exactly those and never one the user had.
    pub fn prepare(&self, worktree_config: bool, record: &Path) -> Result<(), RpcError> {
        let guard = self.commondir();
        match std::fs::read(&guard) {
            Ok(bytes) if bytes == COMMONDIR_GUARD.as_bytes() => {}
            Ok(_) => {
                return Err(RpcError::invalid_params(format!(
                    "{} has a .git/commondir that BondSymphonic did not write; an agent cannot \
                     work in place in it",
                    self.root.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&guard, COMMONDIR_GUARD).map_err(|e| io_error(&guard, e))?
            }
            Err(e) => return Err(io_error(&guard, e)),
        }
        for dir in [self.hooks(), self.info()] {
            std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        }
        let mut created = String::new();
        for name in ON_DEMAND_DIRS {
            let dir = self.git_dir.join(name);
            if std::fs::symlink_metadata(&dir).is_err() {
                std::fs::create_dir(&dir).map_err(|e| io_error(&dir, e))?;
                created.push_str(name);
                created.push('\n');
            }
        }
        if !created.is_empty() {
            // Written before the sandbox exists, so a failure here stops the
            // start rather than leaving a directory nobody remembers making.
            use std::io::Write;
            if let Some(parent) = record.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(record)
                .and_then(|mut f| f.write_all(created.as_bytes()))
                .map_err(|e| io_error(record, e))?;
        }
        let wt = self.config_worktree();
        if worktree_config && !wt.exists() {
            // Git reads an empty file as no configuration.
            std::fs::write(&wt, b"").map_err(|e| io_error(&wt, e))?;
        }
        Ok(())
    }

    /// Takes back what [`prepare`](Self::prepare) left that git would not have:
    /// the guard, if it is still exactly the guard, and each on-demand
    /// directory `record` says the daemon created, if it is still empty. Then
    /// the record itself. Best effort: Close must not fail over any of it.
    pub fn release(&self, record: &Path) {
        let guard = self.commondir();
        if std::fs::read(&guard).is_ok_and(|b| b == COMMONDIR_GUARD.as_bytes()) {
            let _ = std::fs::remove_file(&guard);
        }
        let recorded = std::fs::read_to_string(record).unwrap_or_default();
        for name in ON_DEMAND_DIRS {
            // Only names from the fixed list: the record is the daemon's, but
            // nothing read back from disk gets to name a path by itself.
            if recorded.lines().any(|line| line == name) {
                // `remove_dir` only removes an empty directory, which is the
                // point: whatever git has put there since is the user's.
                let _ = std::fs::remove_dir(self.git_dir.join(name));
            }
        }
        let _ = std::fs::remove_file(record);
    }

    /// Bound read-write, in this order. `.git` is a mount of its own so it
    /// cannot be renamed, removed or replaced from inside (`EBUSY`).
    pub fn rw_binds(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.git_dir.clone()]
    }

    /// Bound read-only after the read-write binds. `worktrees` is here because
    /// it holds the git directories of this repository's *worktree*
    /// workspaces, whose `config.worktree` the daemon's own status reads;
    /// `remotes` and `branches` because `git fetch <name>` and `git push
    /// <name>` read remote definitions from them. `modules` only when it is a real directory: bwrap would otherwise create
    /// it, and an agent that makes one gains nothing an embedded repository in
    /// the tree would not give it (plan R3).
    pub fn late_ro_binds(&self, worktree_config: bool) -> Vec<PathBuf> {
        let mut paths = vec![
            self.config(),
            self.commondir(),
            self.hooks(),
            self.info(),
            self.worktrees(),
            self.remotes(),
            self.branches(),
        ];
        if is_real_dir(&self.modules()) {
            paths.push(self.modules());
        }
        if worktree_config {
            paths.push(self.config_worktree());
        }
        paths
    }
}

/// The branch checked out at `root`, or `""` when `HEAD` is detached. An
/// unborn branch is still a branch and is named.
pub async fn head_branch(git: &Git, root: &Path) -> Result<String, RpcError> {
    match git.run(root, &["symbolic-ref", "--short", "-q", "HEAD"]).await {
        Ok(o) => Ok(o.stdout.trim().to_string()),
        Err(e) if exit_code(&e) == Some(1) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// What an in-place workspace's changes are measured against: `HEAD`, or the
/// empty tree in a repository with no commits yet.
pub async fn diff_base(git: &Git, root: &Path) -> Result<String, RpcError> {
    match git
        .run(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
    {
        Ok(o) => Ok(o.stdout.trim().to_string()),
        Err(e) if exit_code(&e) == Some(1) => Ok(EMPTY_TREE.to_string()),
        Err(e) => Err(e),
    }
}

/// Why `root`, which git classified as `kind`, cannot be worked on in place.
/// One sentence, shared by `repo.inspect` (which the dialog shows) and
/// `workspace.create` (which refuses with it).
pub fn in_place_refusal(kind: &RepoKind, root: &Path) -> Option<String> {
    match kind {
        RepoKind::Root if is_real_dir(&root.join(".git")) => None,
        RepoKind::Root => Some(format!(
            "{} keeps its git directory elsewhere (its .git is a file), so an agent cannot \
             work in it in place",
            root.display()
        )),
        RepoKind::Worktree => Some(format!(
            "{} is a linked worktree of another repository; pick that repository's main \
             checkout to work in place",
            root.display()
        )),
        RepoKind::Bare => Some(repo::bare_repository_error(root).message),
        RepoKind::InsideEnclosing { root: enclosing } => Some(format!(
            "{} is inside the git repository {}; pick the repository itself to work in place",
            root.display(),
            repo::canonical_ish(enclosing).display()
        )),
        RepoKind::NotARepo => Some(format!("{} is not a git repository", root.display())),
    }
}

/// Why the daemon must never bind `root` read-write into a sandbox: the
/// filesystem root, the daemon user's home, anything inside the daemon's data
/// directory -- and anything that *contains* it, because the bind would bring
/// every other workspace's home and exec socket back over the tmpfs that masks
/// them.
pub fn target_refusal(root: &Path, data_root: &Path) -> Option<String> {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    if let Some(why) = repo::init_target_refusal(root, home.as_deref()) {
        return Some(why);
    }
    let root = repo::canonical_ish(root);
    let data = repo::canonical_ish(data_root);
    if root.starts_with(&data) {
        return Some("it is inside the daemon's own data directory".into());
    }
    if data.starts_with(&root) {
        return Some("it contains the daemon's own data directory".into());
    }
    None
}

/// The answer `workspace.merge` and `workspace.create_pr` give an in-place
/// workspace.
pub fn nothing_to_merge() -> RpcError {
    RpcError::invalid_params(
        "an in-place workspace has nothing to merge; commit and push from the checkout",
    )
    .with_data(serde_json::json!({ "reason": IN_PLACE_REASON }))
}
```

- [ ] **Step 4: Run the tests.** Same command. Expected: PASS (the `bwrap` module prints `SKIP` only where bwrap is missing; in the `bondsymphonic` distro it must run). A control check for `the_pinned_git_ignores_a_planted_commondir`: temporarily run plain `common::git_ok(&repo, &["status"])` instead of the pinned git and confirm the marker *does* appear (the test would otherwise pass for the wrong reason), then restore the test. Then `cargo clippy -p bondsymphonic-daemon --all-targets -- -D warnings` in WSL and `cargo fmt --all`.

- [ ] **Step 5: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/daemon/src/workspace/in_place.rs crates/daemon/src/workspace/mod.rs crates/daemon/tests/in_place_sandbox.rs"
git add $P
git commit -m "feat(daemon): the in-place layout guards what a host git executes" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 3: Create, restore, restart, close and status for an in-place workspace

Depends on Task 2. May run beside Tasks 4, 5 and 6.

**Files:**
- Modify: `crates/daemon/src/workspace/lifecycle.rs` (`layout_for`, `spec_for`, `start_sandbox`, `create`, `create_the_workspace`, `destroy`, `bring_up`, `restore`, `status`)
- Test: `crates/daemon/tests/in_place_lifecycle.rs` (new)

**Interfaces:**
- Consumes: everything Task 2 produces; `Workspace.kind` (Task 1).
- Produces:
  ```rust
  // lifecycle.rs
  pub async fn layout_for(d: &Daemon, ws: &Workspace) -> Result<Layout, RpcError>; // now Err(Internal) for InPlace
  pub fn spec_for(d: &Daemon, ws: &Workspace, layout: &Layout) -> SandboxSpec;     // unchanged signature
  pub fn in_place_spec_for(d: &Daemon, ws: &Workspace, layout: &InPlaceLayout, worktree_config: bool) -> SandboxSpec;
  // workspace.create with in_place: true, workspace.destroy / restart / status on an InPlace workspace
  ```

- [ ] **Step 1: Write the failing noop-backend tests.** Create `crates/daemon/tests/in_place_lifecycle.rs`:

```rust
//! An in-place workspace from create to close, over the no-sandbox backend,
//! and once over bubblewrap: the checkout is used as it is, nothing of the
//! user's is created or deleted, and a checkout that has gone away says so.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox::{backend_for, SandboxHandle};
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{lifecycle, DataDirs};
use bondsymphonic_proto::*;
use common::{create_ws, start_daemon, Client};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn params(repo: &Path, name: &str) -> WorkspaceCreateParams {
    WorkspaceCreateParams {
        repo_path: repo.to_string_lossy().into(),
        base_branch: String::new(),
        name: name.into(),
        init_if_missing: false,
        in_place: true,
    }
}

async fn create_in_place(c: &mut Client, repo: &Path, name: &str) -> Result<WorkspaceInfo, RpcError> {
    c.call(Request::WorkspaceCreate(params(repo, name)))
        .await
        .map(|v| serde_json::from_value(v).unwrap())
}

/// Everything under `root`, `.git` included, as (path, bytes); directories as
/// (path + "/", empty). What "byte-identical" means in the tests below.
fn tree_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                stack.push(path);
            } else {
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// The spec's four readings of a repository. `--no-optional-locks` so reading
/// the status does not refresh (and rewrite) the index it is comparing.
fn repo_state(root: &Path) -> (Vec<(String, Vec<u8>)>, String, String, String) {
    let bytes = tree_bytes(root);
    let status = common::git_out(root, &["--no-optional-locks", "status", "--porcelain=v2"]);
    let refs = common::git_out(root, &["for-each-ref"]);
    let config = std::fs::read_to_string(root.join(".git/config")).unwrap();
    (bytes, status, refs, config)
}

async fn shut_down_sandboxes(d: &Daemon) {
    let handles: Vec<Arc<dyn SandboxHandle>> =
        d.sandboxes.lock().drain().map(|(_, h)| h).collect();
    for h in handles {
        let _ = h.shutdown().await;
    }
}

#[tokio::test]
async fn a_checkout_is_used_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let (port, token, daemon, cancel) = start_daemon(&data).await;
    let mut c = Client::connect(port, &token).await;

    let ws = create_in_place(&mut c, &repo, "here").await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);
    assert_eq!(ws.kind, WorkspaceKind::InPlace);
    assert_eq!(PathBuf::from(&ws.worktree_path), repo);
    assert_eq!((ws.branch.as_str(), ws.base_branch.as_str()), ("main", "main"));
    // No branch, no worktree, no private objects.
    assert_eq!(common::git_out(&repo, &["for-each-ref", "--format=%(refname)", "refs/heads"]), "refs/heads/main");
    assert_eq!(common::git_out(&repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    assert!(!DataDirs::new(&data).objects(&ws.id).exists());
    // `layout_for` has nothing to say about it.
    let w = daemon.registry.get(&ws.id).unwrap();
    assert_eq!(lifecycle::layout_for(&daemon, &w).await.unwrap_err().code, ErrorCode::Internal);
    // The kind survives the registry file.
    let raw = std::fs::read_to_string(data.join("workspaces.json")).unwrap();
    assert!(raw.contains("\"kind\": \"in_place\"") || raw.contains("\"kind\":\"in_place\""), "{raw}");
    cancel.cancel();
}

#[tokio::test]
async fn a_detached_head_is_recorded_as_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_in_place(&mut c, &repo, "loose").await.unwrap();
    assert_eq!((ws.branch.as_str(), ws.base_branch.as_str()), ("", ""));
    cancel.cancel();
}

#[tokio::test]
async fn worktrees_bare_and_nested_paths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let linked = dir.path().join("linked");
    common::git_ok(&repo, &["worktree", "add", "-q", "-b", "side", &linked.to_string_lossy()]);
    let bare = dir.path().join("bare.git");
    common::git_ok(dir.path(), &["init", "-q", "--bare", &bare.to_string_lossy()]);
    let nested = repo.join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let e = create_in_place(&mut c, &linked, "a").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("linked worktree"), "{}", e.message);
    let e = create_in_place(&mut c, &bare, "b").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("bare repository"), "{}", e.message);
    let e = create_in_place(&mut c, &nested, "c").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    common::assert_names_enclosing_repo(&e.message, &repo);
    assert!(daemon.registry.list().is_empty());
    cancel.cancel();
}

#[tokio::test]
async fn a_checkout_that_holds_the_data_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&repo.join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let e = create_in_place(&mut c, &repo, "greedy").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("contains the daemon's own data directory"), "{}", e.message);
    cancel.cancel();
}

#[tokio::test]
async fn one_in_place_workspace_per_checkout_and_worktrees_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    create_in_place(&mut c, &repo, "first").await.unwrap();
    // Another spelling of the same checkout is the same checkout.
    let other_spelling = repo.join(".").join("..").join("repo");
    let e = create_in_place(&mut c, &other_spelling, "second").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert_eq!(e.message, "this checkout already has an in-place workspace: first");
    let ws = create_ws(&mut c, &repo, "beside").await;
    assert_eq!(ws.kind, WorkspaceKind::Worktree);
    assert_eq!(ws.state, WorkspaceState::Ready);
    cancel.cancel();
}

#[tokio::test]
async fn a_new_folder_can_be_initialised_and_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("fresh");
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    // Without the flag it is refused as before.
    let e = create_in_place(&mut c, &folder, "fresh").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let ws: WorkspaceInfo = serde_json::from_value(
        c.call(Request::WorkspaceCreate(WorkspaceCreateParams {
            init_if_missing: true,
            ..params(&folder, "fresh")
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert_eq!(ws.branch, "main");
    assert_eq!(common::git_out(&folder, &["rev-list", "--count", "HEAD"]), "1");
    cancel.cancel();
}

#[tokio::test]
async fn closing_leaves_the_repository_byte_identical_whatever_force_says() {
    for force in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        // Work the user has in flight: none of it may be touched.
        std::fs::write(repo.join("README.md"), "edited\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "mine\n").unwrap();
        common::git_ok(&repo, &["branch", "keep-me"]);
        common::git_ok(&repo, &["--no-optional-locks", "status"]);
        let before = repo_state(&repo);
        let data = dir.path().join("data");
        let (port, token, daemon, cancel) = start_daemon(&data).await;
        let mut c = Client::connect(port, &token).await;
        let ws = create_in_place(&mut c, &repo, "closing").await.unwrap();

        c.call(Request::WorkspaceDestroy(WorkspaceDestroyParams {
            workspace_id: ws.id.clone(),
            force,
        }))
        .await
        .unwrap();

        assert_eq!(repo_state(&repo), before, "force = {force}");
        assert!(daemon.registry.get(&ws.id).is_none());
        let dirs = DataDirs::new(&data);
        for gone in [dirs.home(&ws.id), dirs.cache(&ws.id), dirs.run(&ws.id)] {
            assert!(!gone.exists(), "{} survived", gone.display());
        }
        cancel.cancel();
    }
}

#[tokio::test]
async fn status_is_measured_against_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let ws = create_in_place(&mut c, &repo, "status").await.unwrap();
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    std::fs::write(repo.join("new.txt"), "n\n").unwrap();
    common::git_ok(&repo, &["add", "new.txt"]);
    let r: WorkspaceStatusResult = serde_json::from_value(
        c.call(Request::WorkspaceStatus(WorkspaceIdParams { workspace_id: ws.id }))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut seen: Vec<(String, FileStatus, bool)> =
        r.entries.into_iter().map(|e| (e.path, e.status, e.staged)).collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        [
            ("README.md".to_string(), FileStatus::Modified, false),
            ("new.txt".to_string(), FileStatus::Added, true),
        ]
    );
    cancel.cancel();
}

/// Plan R3: an embedded repository's own config must not run when the daemon
/// asks for status, in either kind of workspace.
#[cfg(unix)]
#[tokio::test]
async fn status_does_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (port, token, daemon, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;
    let in_place = create_in_place(&mut c, &repo, "ip").await.unwrap();
    let worktree = create_ws(&mut c, &repo, "wt").await;
    for (ws, env) in [
        (&in_place, Vec::new()),
        (
            &worktree,
            lifecycle::layout_for(&daemon, &daemon.registry.get(&worktree.id).unwrap())
                .await
                .unwrap()
                .sandbox_git_env(),
        ),
    ] {
        let root = PathBuf::from(&ws.worktree_path);
        let marker = dir.path().join(format!("PWNED-{}", ws.name));
        plant_embedded_repo(&root, &marker, &env);
        // Anything the test's own git set off while planting is not the
        // daemon's doing.
        let _ = std::fs::remove_file(&marker);
        lifecycle::status(&daemon, &ws.id).await.unwrap();
        assert!(!marker.exists(), "{}: status ran the embedded repository's config", ws.name);
    }
    cancel.cancel();
}

/// `sub/` as a repository of its own, committed as a gitlink, then given a
/// `core.fsmonitor` and touched, which is what makes a parent's status look in
/// and run it. The config comes after the parent's commit, whose own status
/// would otherwise set the marker off before the daemon is asked anything.
pub fn plant_embedded_repo(root: &Path, marker: &Path, env: &[(String, String)]) {
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    common::git_ok(&sub, &["init", "-q", "-b", "main"]);
    common::git_ok(&sub, &["config", "commit.gpgsign", "false"]);
    std::fs::write(sub.join("f.txt"), "f\n").unwrap();
    common::commit_all(&sub, &[], "sub");
    common::commit_all(root, env, "gitlink");
    let config = sub.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!("[core]\n\tfsmonitor = touch {}\n", marker.display()));
    std::fs::write(&config, text).unwrap();
    std::fs::write(sub.join("dirty.txt"), "x\n").unwrap();
}

#[tokio::test]
async fn a_checkout_that_went_away_says_so_and_comes_back_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let data = dir.path().join("data");
    let first = Daemon::new(DataDirs::new(&data), backend_for("noop"), EventBus::new(64)).unwrap();
    let ws = lifecycle::create(&first, params(&repo, "moved")).await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    shut_down_sandboxes(&first).await;
    drop(first);

    let aside = dir.path().join("aside");
    std::fs::rename(&repo, &aside).unwrap();
    let daemon = Daemon::new(DataDirs::new(&data), backend_for("noop"), EventBus::new(64)).unwrap();
    daemon
        .restore_workspaces_from(lifecycle::restore_snapshot(&daemon))
        .await;
    assert_eq!(
        daemon.registry.get(&ws.id).unwrap().state,
        WorkspaceState::Error(format!(
            "The repository {} is missing or is no longer a git repository. Close the \
             workspace, or restore the folder and press Retry.",
            repo.display()
        ))
    );

    std::fs::rename(&aside, &repo).unwrap();
    let info = lifecycle::restart(&daemon, &ws.id).await.unwrap();
    assert_eq!(info.state, WorkspaceState::Ready);
    // No worktree repair ran: the repository still has exactly one worktree.
    assert_eq!(common::git_out(&repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    // A plain restart of a Ready workspace works too.
    assert_eq!(lifecycle::restart(&daemon, &ws.id).await.unwrap().state, WorkspaceState::Ready);
    lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
    assert!(repo.join(".git/config").is_file());
    assert!(!repo.join(".git/commondir").exists());
}
```

- [ ] **Step 2: Add the bwrap end-to-end test** at the end of the same file:

```rust
#[cfg(target_os = "linux")]
mod bwrap {
    use super::*;
    use bondsymphonic_daemon::sandbox::SandboxCommand;
    use bondsymphonic_daemon::server::{Server, ServerConfig};
    use tokio::io::AsyncReadExt;

    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn names(root: &Path) -> Vec<String> {
        tree_bytes(root).into_iter().map(|(p, _)| p).collect()
    }

    #[tokio::test]
    async fn an_agent_commits_in_the_checkout_and_the_binds_create_only_what_is_documented() {
        if !bwrap_available() {
            eprintln!("SKIP: bwrap unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = common::init_repo(dir.path());
        // `branches` depends on git's template; take the question away.
        for gone in [".git/hooks", ".git/branches"] {
            let _ = std::fs::remove_dir_all(repo.join(gone));
        }
        let before = names(&repo);
        let server = Server::bind(ServerConfig::default()).await.unwrap();
        let daemon = Daemon::new(
            DataDirs::new(dir.path().join("data")),
            backend_for("linux_bwrap"),
            server.event_bus(),
        )
        .unwrap();
        let ws = lifecycle::create(&daemon, params(&repo, "live")).await.unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state);

        let added: Vec<String> = names(&repo).into_iter().filter(|n| !before.contains(n)).collect();
        assert_eq!(
            added,
            [
                ".git/branches/",
                ".git/commondir",
                ".git/hooks/",
                ".git/remotes/",
                ".git/worktrees/"
            ]
        );

        let handle = daemon.sandbox(&ws.id).unwrap();
        let script = format!(
            "cd '{}' && pwd && echo more >> README.md && git add README.md && \
             git -c commit.gpgsign=false commit -q -m from-the-agent && git switch -q -c agent-work && \
             (printf x >> .git/config && echo CONFIG-WRITTEN || echo config-refused)",
            repo.display()
        );
        let mut child = handle
            .spawn(SandboxCommand {
                argv: vec!["sh".into(), "-c".into(), script],
                env: vec![],
                cwd: None,
                pty: None,
            })
            .await
            .unwrap();
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
        assert_eq!(child.exit.await.unwrap(), 0, "{out}");
        assert!(out.contains("config-refused"), "{out}");
        assert_eq!(common::git_out(&repo, &["log", "-1", "--format=%s"]), "from-the-agent");
        assert_eq!(common::git_out(&repo, &["branch", "--show-current"]), "agent-work");

        lifecycle::destroy(&daemon, &ws.id, false).await.unwrap();
        let after: Vec<String> = names(&repo).into_iter().filter(|n| !before.contains(n)).collect();
        // The hooks directory stays (git would have made it); the guard and the
        // empty on-demand directories the daemon made are taken back, and so
        // is its record of them.
        for gone in [".git/commondir", ".git/worktrees/", ".git/remotes/", ".git/branches/"] {
            assert!(!after.iter().any(|n| n == gone), "{gone} survived Close: {after:?}");
        }
        assert!(before.iter().all(|n| n != ".git/hooks/"));
        assert!(names(&repo).iter().any(|n| n == ".git/hooks/"));
        assert!(!DataDirs::new(dir.path().join("data")).in_place_record(&ws.id).exists());
    }
}
```

The commit inside works because `seed_home` writes the repository's identity (`user.name`/`user.email` are set by `common::init_repo`) and `safe.directory = *` into the sandbox home.

- [ ] **Step 3: Run and see them fail.** WSL command with `--test in_place_lifecycle`.
Expected: most tests FAIL — `in_place` is ignored and a worktree is made (`kind` is `worktree`, branch `bs/here/work`).

- [ ] **Step 4: Guard `layout_for` and split the spec.** In `lifecycle.rs` add `use crate::workspace::in_place::{self, InPlaceLayout};` and `use bondsymphonic_proto::WorkspaceKind;` (already covered by `bondsymphonic_proto::*`). At the top of `layout_for`:

```rust
    // The one door to the worktree-only paths. An in-place workspace has none
    // of them, and a caller that forgot to branch on the kind must fail here
    // rather than compute a `worktrees/<id>` gitdir inside the user's `.git`.
    if ws.kind == WorkspaceKind::InPlace {
        return Err(RpcError::internal(format!(
            "workspace {} works in place and has no worktree layout",
            ws.id
        )));
    }
```

Replace `spec_for` with the three functions below (behaviour of `spec_for` unchanged):

```rust
pub fn spec_for(d: &Daemon, ws: &Workspace, layout: &Layout) -> SandboxSpec {
    let same = |p: &Path| (p.to_path_buf(), p.to_path_buf());
    let mut rw_binds = vec![same(&ws.worktree_path), same(&layout.objects_dir)];
    rw_binds.extend(layout.rw_git_paths().iter().map(|p| same(p)));
    // Read-only *after* the read-write bind of its parent gitdir, or the parent
    // would put the writable original straight back on top of it.
    let late_ro_binds = vec![same(&layout.config_worktree())];
    finish_spec(
        d,
        ws,
        rw_binds,
        vec![same(&layout.git_common)],
        late_ro_binds,
        layout.sandbox_git_env(),
    )
}

/// The sandbox of an in-place workspace: the checkout and its `.git`
/// read-write, and what a git outside the sandbox would execute read-only on
/// top (see [`InPlaceLayout::late_ro_binds`]). No private object directory:
/// the agent's objects go into the repository's own store.
pub fn in_place_spec_for(
    d: &Daemon,
    ws: &Workspace,
    layout: &InPlaceLayout,
    worktree_config: bool,
) -> SandboxSpec {
    let same = |p: &PathBuf| (p.clone(), p.clone());
    finish_spec(
        d,
        ws,
        layout.rw_binds().iter().map(same).collect(),
        Vec::new(),
        layout.late_ro_binds(worktree_config).iter().map(same).collect(),
        Vec::new(),
    )
}

/// What both kinds share: the cache, the Claude binary, the proxy and the
/// workspace id.
fn finish_spec(
    d: &Daemon,
    ws: &Workspace,
    mut rw_binds: Vec<(PathBuf, PathBuf)>,
    mut ro_binds: Vec<(PathBuf, PathBuf)>,
    late_ro_binds: Vec<(PathBuf, PathBuf)>,
    mut env: Vec<(String, String)>,
) -> SandboxSpec {
    let cache = d.dirs.cache(&ws.id);
    let home_in_sandbox = PathBuf::from(format!("/home/{}", sandbox_user()));
    rw_binds.push((cache, home_in_sandbox.join(".cache")));
    // (move the existing comment about the Claude Code CLI bind here unchanged)
    if d.backend.name() == BWRAP_BACKEND {
        ro_binds.extend(crate::agents::claude::claude_ro_bind());
    }
    env.push(("BS_WORKSPACE".into(), ws.id.to_string()));
    // (move the existing comment about the proxy environment here unchanged)
    if d.backend.name() == BWRAP_BACKEND {
        env.extend(proxy_env());
    }
    SandboxSpec {
        id: ws.id.clone(),
        rw_binds,
        ro_binds,
        late_ro_binds,
        home: d.dirs.home(&ws.id),
        run_dir: d.dirs.run(&ws.id),
        env,
        cwd: ws.worktree_path.clone(),
    }
}
```

In `start_sandbox`, replace the `let layout = layout_for(..)` line, the `config.worktree` write and the `spec_for(d, ws, &layout)` argument with one `spec` computed after the directory loop:

```rust
    let spec = match ws.kind {
        WorkspaceKind::Worktree => {
            let layout = layout_for(d, ws).await?;
            // (keep the existing comment about the daemon owning config.worktree)
            std::fs::write(layout.config_worktree(), b"").map_err(|e| RpcError::io(&e))?;
            spec_for(d, ws, &layout)
        }
        WorkspaceKind::InPlace => {
            // Every start, like the worktree kind's `config.worktree`: what the
            // read-only binds need is put back even if something removed it
            // while the sandbox was down.
            let layout = InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks());
            let worktree_config = layout.worktree_config_enabled().await?;
            layout.prepare(worktree_config, &d.dirs.in_place_record(&ws.id))?;
            in_place_spec_for(d, ws, &layout, worktree_config)
        }
    };
```

and `d.backend.start(&spec)`. Build (`cargo build -p bondsymphonic-daemon --tests` in WSL): must compile.

- [ ] **Step 5: Create.** Split `create_the_workspace` at `let git_common = match existing { .. };`: move everything from `let mut not_a_repo` through that `let git_common = ...;` (comments included) into

```rust
/// Identifies the repository at `repo_path`, initialising it first when the
/// client asked for that, and answers its common git directory. Shared by both
/// kinds of create, so both refuse and initialise the same folders the same way.
async fn resolve_repository(
    d: &Arc<Daemon>,
    p: &WorkspaceCreateParams,
    repo_path: &Path,
) -> Result<PathBuf, RpcError> {
    // (moved body)
    Ok(git_common)
}
```

and start `create_the_workspace` with `let git_common = resolve_repository(d, p, repo_path).await?;`. Move the allowlist lines into

```rust
/// The repository's own `[network] allow` on top of the defaults. (keep the
/// existing comment about a `bondsymphonic.toml` that will not parse)
fn effective_allowlist(repo_path: &Path) -> Vec<String> {
    let repo_config = match crate::runs::config::load_repo_config(repo_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(repo = %repo_path.display(), error = %e, "using the default allowlist");
            None
        }
    };
    crate::net::allowlist::effective(repo_config.as_ref())
}
```

Add:

```rust
/// [`create_the_workspace`] for `in_place: true`: the checkout is used as it
/// is. No branch, no `git worktree add`, no lock, no private objects -- so,
/// unlike a worktree create, nothing is made in the repository here and there
/// is nothing to unwind on a refusal.
async fn create_in_place(
    d: &Arc<Daemon>,
    p: &WorkspaceCreateParams,
    repo_path: &Path,
) -> Result<Workspace, RpcError> {
    resolve_repository(d, p, repo_path).await?;
    // Asked again after `resolve_repository`, which may just have made the
    // folder a repository of its own.
    let kind = repo::classify(&d.git, repo_path).await?;
    if let Some(why) = in_place::in_place_refusal(&kind, repo_path) {
        return Err(RpcError::invalid_params(why));
    }
    if let Some(why) = in_place::target_refusal(repo_path, &d.dirs.root) {
        return Err(RpcError::invalid_params(format!(
            "refusing to work in place in {}: {why}",
            repo_path.display()
        )));
    }
    // One agent per checkout. By canonical path, so two spellings of one
    // folder are one folder, as they are for the repository lock held here.
    let target = repo::canonical_ish(repo_path);
    if let Some(other) = d.registry.list().into_iter().find(|w| {
        w.kind == WorkspaceKind::InPlace && repo::canonical_ish(&w.worktree_path) == target
    }) {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!("this checkout already has an in-place workspace: {}", other.name),
        ));
    }
    if d.registry.find_by_name(repo_path, &p.name).is_some() {
        return Err(RpcError::new(
            ErrorCode::Conflict,
            format!("workspace {} already exists for this repo", p.name),
        ));
    }
    let layout = InPlaceLayout::new(repo_path, &d.dirs.no_hooks());
    // Display only: never switched, created or deleted, and nothing merges
    // into it, so the base branch says the same.
    let branch = in_place::head_branch(&layout.git(), repo_path).await?;
    let ws = Workspace {
        id: WorkspaceId(new_id("ws_")),
        name: p.name.clone(),
        repo_path: repo_path.to_path_buf(),
        base_branch: branch.clone(),
        branch,
        worktree_path: repo_path.to_path_buf(),
        created_at: now_rfc3339(),
        allowlist: effective_allowlist(repo_path),
        state: WorkspaceState::Creating,
        agents: vec![],
        runs: vec![],
        kind: WorkspaceKind::InPlace,
    };
    d.registry
        .insert(ws.clone())
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    d.emit_state(&ws);
    Ok(ws)
}
```

In `create`, inside the repository-lock block:

```rust
        if p.in_place {
            create_in_place(d, &p, &repo_path).await?
        } else {
            create_the_workspace(d, &p, &repo_path).await?
        }
```

- [ ] **Step 6: Close, bring-up, restore, status.** At the top of `destroy`, right after `let ws = d.workspace(id)?;`:

```rust
    if ws.kind == WorkspaceKind::InPlace {
        // `force` means nothing here, and there is no dirty or unmerged check:
        // the work is in the user's own checkout and none of it is deleted.
        close_in_place(d, &ws).await?;
        gates().lock().remove(id);
        return Ok(Empty {});
    }
```

and add

```rust
/// `workspace.destroy` for an in-place workspace: everything that runs in its
/// sandbox stops, the daemon's own directories and records for it go, and the
/// checkout is left exactly as it was. Never `worktree::remove`, never a
/// branch, never a write to the repository beyond taking back what
/// [`InPlaceLayout::prepare`] put there.
async fn close_in_place(d: &Daemon, ws: &Workspace) -> Result<(), RpcError> {
    let id = &ws.id;
    d.set_state(id, WorkspaceState::Destroying).await?;
    d.watchers.disable(id);
    tear_down_sandbox(d, id).await;
    {
        // The same lock every other write to this repository takes.
        let repo_lock = crate::git::repo_lock(&ws.repo_path);
        let _repo_guard = repo_lock.lock().await;
        InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks())
            .release(&d.dirs.in_place_record(id));
    }
    d.agents.forget_workspace(id);
    // `remove_workspace` removes `<data>/worktrees/<id>`, never `worktree_path`.
    d.dirs.remove_workspace(id);
    d.registry
        .remove(id)
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(())
}
```

In `bring_up`, wrap the existing directory check, `layout_for` and `ensure_registered` in `if ws.kind == WorkspaceKind::Worktree { ... }` and add before it:

```rust
    if ws.kind == WorkspaceKind::InPlace {
        // No worktree to repair and no registration to put back: only the
        // checkout itself has to still be there.
        InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).check_repository()?;
    }
```

In `restore`, make the two interrupted-operation sentences kind-aware: compute `let verb = if ws.kind == WorkspaceKind::InPlace { "Close" } else { "Remove" };` and use `format!("Creating this workspace was interrupted when the daemon stopped. {verb} the workspace to clean up.")` and `format!("Removing this workspace was interrupted when the daemon stopped. {verb} the workspace again to finish.")`.

In `status`:

```rust
    let ws = d.workspace(id)?;
    // Both pin git's discovery: the tree is agent-writable either way.
    let git = match ws.kind {
        WorkspaceKind::InPlace => InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).git(),
        WorkspaceKind::Worktree => layout_for(d, &ws).await?.worktree_git(),
    };
    // `--ignore-submodules=all`: an agent can commit an embedded repository
    // whose own config names programs, and a status that looks into it runs
    // them (plan R3). The flag, not `diff.ignoreSubmodules`, which a
    // `.gitmodules` in the tree overrides.
    let out = git
        .run(
            &ws.worktree_path,
            &["status", "--porcelain=v2", "--untracked-files=all", "--ignore-submodules=all"],
        )
        .await?;
```

In `destroy`'s dirty check add `"--ignore-submodules=all"` to the `status --porcelain` arguments.

- [ ] **Step 7: Run.** WSL: `--test in_place_lifecycle`, then the neighbours that exercise the refactored paths: `--test workspace_integration --test worktree_repair --test sandbox_integration --test agent_integration`. Expected: PASS. Control check: drop `"--ignore-submodules=all"` from `status` for one run and confirm `status_does_not_run_an_embedded_repositorys_config` fails (otherwise the test proves nothing and must be fixed), then put it back. Then clippy (WSL) and `cargo fmt --all`.

- [ ] **Step 8: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/daemon/src/workspace/lifecycle.rs crates/daemon/tests/in_place_lifecycle.rs"
git add $P
git commit -m "feat(daemon): workspace.create works in place, and Close leaves the checkout alone" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 4: Changes and diff against `HEAD`, no merge or PR, and what `repo.inspect` tells the dialog

Depends on Task 2. May run beside Tasks 3, 5 and 6. Its tests never call `workspace.create` with `in_place` (Task 3 may not have landed): they put an in-place workspace straight into the registry.

**Files:**
- Modify: `crates/daemon/src/workspace/changes.rs` (`changes`, `diff`, module doc)
- Modify: `crates/daemon/src/git/merge.rs` (`merge`), `crates/daemon/src/git/pr.rs` (`create_pr`)
- Modify: `crates/daemon/src/git/repo.rs` (`inspect`, new `hooks_path_in_tree`)
- Test: `crates/daemon/tests/in_place_changes.rs` (new)

**Interfaces:**
- Consumes: `in_place::{InPlaceLayout, diff_base, nothing_to_merge, in_place_refusal}` (Task 2), `Workspace.kind`, `RepoInfo` fields (Task 1).
- Produces:
  ```rust
  // git/repo.rs
  pub fn hooks_path_in_tree(root: &Path, configured: &str) -> Option<String>;
  // repo.inspect fills head_branch, in_place_refusal, hooks_path_in_tree
  ```

- [ ] **Step 1: Write the failing tests.** Create `crates/daemon/tests/in_place_changes.rs`:

```rust
//! What an in-place workspace reports as changed -- everything `git status`
//! would, measured against `HEAD` -- and what it refuses to do: there is no
//! branch of its own to merge or push. Also what `repo.inspect` tells the New
//! Agent dialog about working in place.

mod common;

use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::git::{merge, pr, repo, Git};
use bondsymphonic_daemon::sandbox::backend_for;
use bondsymphonic_daemon::server::broadcast::EventBus;
use bondsymphonic_daemon::workspace::{changes, now_rfc3339, DataDirs, Workspace};
use bondsymphonic_proto::*;
use std::path::Path;
use std::sync::Arc;

/// A Ready in-place workspace on `root`, without a sandbox: changes, diff,
/// merge and PR never need one.
async fn in_place_on(dir: &Path, root: &Path, base_branch: &str) -> (Arc<Daemon>, WorkspaceId) {
    let d = Daemon::new(
        DataDirs::new(dir.join("data")),
        backend_for("noop"),
        EventBus::new(16),
    )
    .unwrap();
    let id = WorkspaceId("ws_inplace_changes".into());
    d.registry
        .insert(Workspace {
            id: id.clone(),
            name: "here".into(),
            repo_path: root.into(),
            base_branch: base_branch.into(),
            branch: base_branch.into(),
            worktree_path: root.into(),
            created_at: now_rfc3339(),
            allowlist: vec![],
            state: WorkspaceState::Ready,
            agents: vec![],
            runs: vec![],
            kind: WorkspaceKind::InPlace,
        })
        .await
        .unwrap();
    (d, id)
}

fn summary(r: &ChangesResult) -> Vec<(String, FileStatus)> {
    r.files.iter().map(|f| (f.path.clone(), f.status)).collect()
}

#[tokio::test]
async fn changes_are_what_git_status_says_even_after_the_branch_moved() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    // Recorded on `main`; the agent then switched and committed. Measured
    // against the merge-base with `main` that commit would be a change; against
    // `HEAD` it is not.
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    common::git_ok(&repo, &["switch", "-q", "-c", "agent"]);
    std::fs::write(repo.join("committed.txt"), "c\n").unwrap();
    common::commit_all(&repo, &[], "agent commit");
    std::fs::write(repo.join("README.md"), "hello\nmore\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "s\n").unwrap();
    common::git_ok(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("loose.txt"), "l\nl\n").unwrap();

    let r = changes::changes(&d, &id).await.unwrap();
    assert_eq!(
        summary(&r),
        [
            ("README.md".to_string(), FileStatus::Modified),
            ("loose.txt".to_string(), FileStatus::Untracked),
            ("staged.txt".to_string(), FileStatus::Added),
        ]
    );
    let readme = r.files.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!((readme.additions, readme.deletions), (1, 0));

    let diff = changes::diff(&d, &id, "README.md").await.unwrap();
    assert_eq!(diff.base_text, "hello\n");
    assert_eq!(diff.work_text, "hello\nmore\n");
    let diff = changes::diff(&d, &id, "committed.txt").await.unwrap();
    assert_eq!(diff.base_text, diff.work_text, "a committed file is unchanged");
}

#[tokio::test]
async fn a_repository_with_no_commits_is_measured_against_the_empty_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("unborn");
    std::fs::create_dir_all(&root).unwrap();
    common::git_ok(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("first.txt"), "one\n").unwrap();
    common::git_ok(&root, &["add", "first.txt"]);
    let (d, id) = in_place_on(dir.path(), &root, "main").await;

    let r = changes::changes(&d, &id).await.unwrap();
    assert_eq!(summary(&r), [("first.txt".to_string(), FileStatus::Added)]);
    let diff = changes::diff(&d, &id, "first.txt").await.unwrap();
    assert_eq!((diff.base_text.as_str(), diff.work_text.as_str()), ("", "one\n"));
}

#[tokio::test]
async fn merge_and_pull_request_are_refused_with_the_in_place_reason() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let refs_before = common::git_out(&repo, &["for-each-ref"]);
    for e in [
        merge::merge(&d, &id, MergeMode::Merge, None).await.unwrap_err(),
        merge::merge(&d, &id, MergeMode::Squash, Some("s".into())).await.unwrap_err(),
        pr::create_pr(&d, &id, "t", "b", false).await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert_eq!(e.data.as_ref().unwrap()["reason"], "in_place");
    }
    assert_eq!(common::git_out(&repo, &["for-each-ref"]), refs_before);
}

/// Plan R3, for the Changes panel: neither kind of workspace may run an
/// embedded repository's config when its changes are read.
#[cfg(unix)]
#[tokio::test]
async fn changes_do_not_run_an_embedded_repositorys_config() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let (d, id) = in_place_on(dir.path(), &repo, "main").await;
    let marker = dir.path().join("PWNED-in-place");
    plant_embedded_repo(&repo, &marker, &[]);
    let _ = std::fs::remove_file(&marker);
    changes::changes(&d, &id).await.unwrap();
    changes::diff(&d, &id, "README.md").await.unwrap();
    assert!(!marker.exists(), "in place: changes ran the embedded repository's config");

    // The worktree kind, created the ordinary way.
    let other = tempfile::tempdir().unwrap();
    let repo = common::init_repo(other.path());
    let (port, token, daemon, cancel) = common::start_daemon(&other.path().join("data")).await;
    let mut c = common::Client::connect(port, &token).await;
    let ws = common::create_ws(&mut c, &repo, "wt").await;
    let w = daemon.registry.get(&ws.id).unwrap();
    let env = bondsymphonic_daemon::workspace::lifecycle::layout_for(&daemon, &w)
        .await
        .unwrap()
        .sandbox_git_env();
    let marker = other.path().join("PWNED-worktree");
    plant_embedded_repo(&w.worktree_path, &marker, &env);
    let _ = std::fs::remove_file(&marker);
    changes::changes(&daemon, &ws.id).await.unwrap();
    assert!(!marker.exists(), "worktree: changes ran the embedded repository's config");
    cancel.cancel();
}

/// The same helper as in `in_place_lifecycle.rs`; each test crate has its own.
fn plant_embedded_repo(root: &Path, marker: &Path, env: &[(String, String)]) {
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    common::git_ok(&sub, &["init", "-q", "-b", "main"]);
    common::git_ok(&sub, &["config", "commit.gpgsign", "false"]);
    std::fs::write(sub.join("f.txt"), "f\n").unwrap();
    common::commit_all(&sub, &[], "sub");
    common::commit_all(root, env, "gitlink");
    let config = sub.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!("[core]\n\tfsmonitor = touch {}\n", marker.display()));
    std::fs::write(&config, text).unwrap();
    std::fs::write(sub.join("dirty.txt"), "x\n").unwrap();
}

#[tokio::test]
async fn inspect_names_the_checked_out_branch_and_whether_it_can_be_worked_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    common::git_ok(&repo, &["switch", "-q", "-c", "feature"]);
    let git = Git::new();
    let info = repo::inspect(&git, &repo).await.unwrap();
    assert_eq!(info.head_branch.as_deref(), Some("feature"));
    assert_eq!(info.in_place_refusal, None);
    assert_eq!(info.hooks_path_in_tree, None);

    common::git_ok(&repo, &["checkout", "-q", "--detach"]);
    assert_eq!(repo::inspect(&git, &repo).await.unwrap().head_branch, None);

    let linked = dir.path().join("linked");
    common::git_ok(&repo, &["worktree", "add", "-q", "-b", "side", &linked.to_string_lossy()]);
    let why = repo::inspect(&git, &linked).await.unwrap().in_place_refusal.unwrap();
    assert!(why.contains("linked worktree"), "{why}");

    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let info = repo::inspect(&git, &plain).await.unwrap();
    assert!(!info.is_repo);
    assert_eq!((info.head_branch, info.hooks_path_in_tree), (None, None));
}

#[tokio::test]
async fn inspect_reports_a_hooks_path_inside_the_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = common::init_repo(dir.path());
    let git = Git::new();
    for (configured, expected) in [
        (".husky/_".to_string(), Some(".husky/_")),
        (repo.join("tools/hooks").to_string_lossy().into_owned(), Some("tools/hooks")),
        (dir.path().join("elsewhere").to_string_lossy().into_owned(), None),
        (".git/hooks".to_string(), None),
        ("../outside".to_string(), None),
    ] {
        common::git_ok(&repo, &["config", "core.hooksPath", &configured]);
        let info = repo::inspect(&git, &repo).await.unwrap();
        assert_eq!(info.hooks_path_in_tree.as_deref(), expected, "{configured}");
    }
    assert_eq!(repo::hooks_path_in_tree(&repo, "."), Some(".".into()));
    assert_eq!(repo::hooks_path_in_tree(&repo, ""), None);
}
```

- [ ] **Step 2: Run and see them fail.** WSL command with `--test in_place_changes`.
Expected: compile error (`hooks_path_in_tree` missing); after stubbing nothing, the changes test would fail on `layout_for`'s `Internal` error once Task 3 lands, or on `committed.txt` being listed before it.

- [ ] **Step 3: Changes and diff.** In `crates/daemon/src/workspace/changes.rs`: change the module doc's first sentence to "what a workspace changed: a worktree workspace relative to the merge-base of its base branch, an in-place one relative to `HEAD`". Add imports `use crate::workspace::in_place::{self, InPlaceLayout};` and `use bondsymphonic_proto::WorkspaceKind;`, and:

```rust
/// The pinned git for `ws`'s tree and the commit (or tree) its changes are
/// measured against.
///
/// A worktree workspace is measured from where its branch left the base: its
/// commits are the work. An in-place workspace has no branch of its own, so its
/// changes are what `git status` would show -- staged, unstaged and untracked,
/// against `HEAD` -- whatever the agent has committed or switched to since.
async fn measuring(d: &Daemon, ws: &Workspace) -> Result<(Git, String), RpcError> {
    match ws.kind {
        WorkspaceKind::InPlace => {
            let git = InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).git();
            let base = in_place::diff_base(&git, &ws.worktree_path).await?;
            Ok((git, base))
        }
        WorkspaceKind::Worktree => {
            let git = layout_for(d, ws).await?.worktree_git();
            let base = merge_base(&git, ws).await?;
            Ok((git, base))
        }
    }
}
```

In `changes`, replace the `layout`/`git`/`base` lines with `let (git, base) = measuring(d, &ws).await?;` (keep `let cwd = ...`), and add `--ignore-submodules=all` to all three argument lists, with one comment above them:

```rust
    // `--ignore-submodules=all` on each: an agent can commit an embedded
    // repository whose own config names programs, and a status or diff that
    // looks into it runs them (plan R3). The flag, not the config key, which a
    // `.gitmodules` in the tree overrides.
    let name_status_args = ["diff", "--name-status", "-M", "-z", "--ignore-submodules=all", &base, "--"];
    let numstat_args = ["diff", "--numstat", "-M", "-z", "--ignore-submodules=all", &base, "--"];
    let status_args = ["status", "--porcelain=v2", "--untracked-files=all", "--ignore-submodules=all", "-z"];
```

In `diff`, replace `let layout = ...; let git = layout.worktree_git();` and the `merge_base(&git, &ws)` inside the `spec` format with `let (git, base) = measuring(d, &ws).await?;` and `let spec = format!("{base}:{spec_path}");`. Update the doc comments that say "merge-base" to "the base (see [`measuring`])".

- [ ] **Step 4: Merge and PR.** In `merge::merge` and `pr::create_pr`, directly after `let ws = d.workspace(id)?;`:

```rust
    // Before the state check: an in-place workspace has nothing to merge in
    // any state, and the IDE branches on this reason rather than the prose.
    if ws.kind == bondsymphonic_proto::WorkspaceKind::InPlace {
        return Err(crate::workspace::in_place::nothing_to_merge());
    }
```

- [ ] **Step 5: `repo.inspect`.** In `crates/daemon/src/git/repo.rs`, keep the classified kind: `let kind = classify(git, repo).await?; match &kind { ... }` with the same arms. Add two helpers beside `configured_head`:

```rust
/// The branch checked out, or `None` for a detached `HEAD`.
async fn head_branch(git: &Git, repo: &Path) -> Result<Option<String>, RpcError> {
    match git.run(repo, &["symbolic-ref", "--short", "-q", "HEAD"]).await {
        Ok(o) => Ok(Some(o.stdout.trim().to_string())),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `core.hooksPath` as the repository's config sets it, or `None`.
async fn configured_hooks_path(git: &Git, repo: &Path) -> Result<Option<String>, RpcError> {
    match git.run(repo, &["config", "--get", "core.hooksPath"]).await {
        Ok(o) => Ok(Some(o.stdout.trim().to_string())),
        Err(e) if e.data.as_ref().and_then(|d| d["exit_code"].as_i64()) == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `configured` (a `core.hooksPath` value) relative to `root`, with `/`
/// separators, when it resolves inside the working tree -- `.` for the root
/// itself -- and `None` when it resolves outside it or inside `.git`, which an
/// in-place agent cannot write. A relative value is relative to the root,
/// which is where git runs hooks from in a repository with a working tree.
pub fn hooks_path_in_tree(root: &Path, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    let expanded = match configured.strip_prefix("~/") {
        Some(rest) => daemon_home()?.join(rest),
        None => PathBuf::from(configured),
    };
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    };
    let resolved = canonical_ish(&normalise(&resolved));
    let root = canonical_ish(root);
    let rel = resolved.strip_prefix(&root).ok()?;
    if rel.starts_with(".git") {
        return None;
    }
    if rel.as_os_str().is_empty() {
        return Some(".".into());
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// `..` and `.` taken out lexically, so a hooks path that climbs out of the
/// root is seen to, even when the directory it names does not exist.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
```

Extend the `try_join!` with `head_branch(git, repo)` and `configured_hooks_path(git, repo)`, and fill the `RepoInfo`:

```rust
        head_branch,
        in_place_refusal: crate::workspace::in_place::in_place_refusal(&kind, repo),
        hooks_path_in_tree: hooks_path.and_then(|p| hooks_path_in_tree(repo, &p)),
```

(`not_a_repo` keeps the three `None`s from Task 1.)

- [ ] **Step 6: Run.** WSL: `--test in_place_changes --test changes_integration --test merge_integration --test pr_integration --test git_repo`. Expected: PASS. Control check: drop `--ignore-submodules=all` from the status args once and confirm `changes_do_not_run_an_embedded_repositorys_config` fails, then restore. Clippy (WSL), `cargo fmt --all`.

- [ ] **Step 7: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/daemon/src/workspace/changes.rs crates/daemon/src/git/merge.rs crates/daemon/src/git/pr.rs crates/daemon/src/git/repo.rs crates/daemon/tests/in_place_changes.rs"
git add $P
git commit -m "feat(daemon): an in-place workspace shows its changes against HEAD and merges nothing" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 5: The IDE's model and controller know the kind

Depends on Task 1. May run beside Tasks 2, 3 and 4. Task 6 depends on it.

**Files:**
- Modify: `crates/ide/src/model/app_state.rs` (`AgentTab.kind`, `from_workspace_info`, `Workspaces::apply_workspace_info`, `Workspaces::is_in_place`, `workspace_problem_for`)
- Modify: `crates/ide/src/model/persistence.rs` (`StateFile.new_agent_in_place`)
- Modify: `crates/ide/src/qobjects/group_model.rs` (`workspace_in_place` invokable, `tab_tooltip`, test fixture)
- Modify: `crates/ide/src/qobjects/app_controller.rs` (`create_params`, `in_place` on both create invokables, `new_agent_in_place` / `set_new_agent_in_place`)
- Modify: `crates/ide/cpp/MainWindow.cpp` — **only** the two `createWorkspace*` calls in `onNewAgent`, which gain a trailing `false` (Task 6 replaces it)
- Test: `crates/ide/tests/model_tests.rs` (fixture `tab()` + appended `mod in_place_model`), `crates/ide/tests/qobject_smoke.rs` (fixture `tab()` + appended `mod in_place_controller`), `crates/ide/src/model/app_state.rs` and `group_model.rs` test fixtures

**Interfaces:**
- Consumes: `WorkspaceKind`, `WorkspaceInfo.kind`, `WorkspaceCreateParams.in_place` (Task 1).
- Produces:
  ```rust
  // model::app_state
  AgentTab { .., #[serde(default)] pub kind: WorkspaceKind }   // tab JSON: "kind": "in_place" | "worktree"
  impl AgentTab { pub fn is_in_place(&self) -> bool }
  impl Workspaces { pub fn is_in_place(&self, id: &WorkspaceId) -> bool }
  pub fn workspace_problem_for(state: &WorkspaceState, kind: WorkspaceKind) -> Option<WorkspaceProblem>;
  // model::persistence
  StateFile { .., pub new_agent_in_place: bool }
  // qobjects::app_controller
  pub fn create_params(repo_path: String, base_branch: String, name: String, init_if_missing: bool, in_place: bool) -> WorkspaceCreateParams;
  ```
  C++ (cxx-qt camelCase):
  ```cpp
  bool GroupModel::workspaceInPlace(const QString& workspaceId) const;
  void AppController::createWorkspaceWithRun(repo, base, name, group, adapter, command, runConfig, bool initIfMissing, bool inPlace);
  void AppController::createWorkspaceWithAgentAndRun(repo, base, name, group, optionsJson, initialPrompt, runConfig, bool initIfMissing, bool inPlace);
  bool AppController::newAgentInPlace() const;
  void AppController::setNewAgentInPlace(bool inPlace) const;
  ```

- [ ] **Step 1: Write the failing model tests.** Append to `crates/ide/tests/model_tests.rs`:

```rust
/// In-place workspaces, 2026-09-17: the tab carries the kind the daemon
/// reports, keeps it through events and a restore, and words a stopped
/// sandbox for a checkout rather than a worktree.
mod in_place_model {
    use bondsymphonic_ide::model::app_state::{workspace_problem_for, AgentTab, Workspaces};
    use bondsymphonic_proto::*;

    fn info(id: &str, kind: WorkspaceKind, state: WorkspaceState) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.into(),
            name: id.into(),
            repo_path: "/r".into(),
            base_branch: "main".into(),
            branch: "main".into(),
            worktree_path: "/r".into(),
            created_at: "t".into(),
            allowlist: vec![],
            state,
            agents: vec![],
            agent_records: vec![],
            runs: vec![],
            kind,
        }
    }

    #[test]
    fn a_tab_takes_its_kind_from_the_daemon_and_keeps_it() {
        let ip = info("ws_ip", WorkspaceKind::InPlace, WorkspaceState::Ready);
        let tab = AgentTab::from_workspace_info(&ip);
        assert!(tab.is_in_place());
        let json = serde_json::to_value(&tab).unwrap();
        assert_eq!(json["kind"], "in_place");

        // Restored from the saved arrangement and the daemon's list.
        let mut model = Workspaces::from_persisted(&[], &[ip.clone()], None);
        assert!(model.is_in_place(&ip.id));
        // An event refreshes it, as it does the branch.
        model.apply_workspace_info(&info("ws_ip", WorkspaceKind::InPlace, WorkspaceState::SandboxDown));
        assert!(model.is_in_place(&ip.id));
        assert!(!model.is_in_place(&WorkspaceId("ws_other".into())));
    }

    #[test]
    fn a_tab_saved_before_the_field_existed_is_a_worktree_tab() {
        let tab = AgentTab::from_workspace_info(&info("ws_wt", WorkspaceKind::Worktree, WorkspaceState::Ready));
        let mut json = serde_json::to_value(&tab).unwrap();
        json.as_object_mut().unwrap().remove("kind");
        let back: AgentTab = serde_json::from_value(json).unwrap();
        assert!(!back.is_in_place());
    }

    #[test]
    fn a_stopped_sandbox_in_place_says_the_checkout_is_untouched() {
        let p = workspace_problem_for(&WorkspaceState::SandboxDown, WorkspaceKind::InPlace).unwrap();
        assert!(p.detail.contains("your checkout is not touched"), "{}", p.detail);
        let p = workspace_problem_for(&WorkspaceState::SandboxDown, WorkspaceKind::Worktree).unwrap();
        assert!(p.detail.contains("the worktree is kept"), "{}", p.detail);
    }
}
```

Append to `crates/ide/tests/qobject_smoke.rs`:

```rust
/// In-place workspaces: what the New Agent dialog's choice turns into on the
/// wire, and the dialog's remembered mode in `state.json`.
mod in_place_controller {
    use bondsymphonic_ide::model::persistence::StateFile;
    use bondsymphonic_ide::qobjects::app_controller::create_params;

    #[test]
    fn an_in_place_create_sends_the_flag_and_no_base_branch() {
        let p = create_params("/r".into(), "main".into(), "a".into(), false, true);
        assert!(p.in_place);
        assert_eq!(p.base_branch, "", "ignored by the daemon, so not sent");
        let p = create_params("/r".into(), "main".into(), "a".into(), true, false);
        assert!(!p.in_place && p.init_if_missing);
        assert_eq!(p.base_branch, "main");
    }

    #[test]
    fn the_dialog_mode_defaults_to_a_worktree_and_round_trips() {
        let fresh: StateFile = serde_json::from_str("{}").unwrap();
        assert!(!fresh.new_agent_in_place);
        let saved = StateFile {
            new_agent_in_place: true,
            ..StateFile::default()
        };
        let back: StateFile = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert!(back.new_agent_in_place);
    }
}
```

- [ ] **Step 2: Run and see them fail.**
Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test model_tests --test qobject_smoke in_place`
Expected: compile errors (`workspace_problem_for`, `is_in_place`, `create_params`, `new_agent_in_place` missing).

- [ ] **Step 3: Model.** In `app_state.rs` import `WorkspaceKind`, add to `AgentTab` after `worktree_path`:

```rust
    /// Which kind of workspace the tab shows, from `WorkspaceInfo`. Not kept
    /// anywhere of its own: every restore and every event brings it again.
    /// Defaulted, so a tab serialised before the field existed reads as the
    /// worktree it was.
    #[serde(default)]
    pub kind: WorkspaceKind,
```

In `from_workspace_info`: `kind: info.kind,` and `workspace_problem: workspace_problem_for(&info.state, info.kind),`. In `Workspaces::apply_workspace_info`: `tab.kind = info.kind;` beside `tab.branch`, and `tab.workspace_problem = workspace_problem_for(&info.state, info.kind);`. Add:

```rust
impl AgentTab {
    /// Whether the agent works directly in the repository's checkout.
    pub fn is_in_place(&self) -> bool {
        self.kind == WorkspaceKind::InPlace
    }
}
```

(inside the existing `impl AgentTab` block), and in `impl Workspaces`:

```rust
    /// Whether `id` is an in-place workspace. False for one the model does not
    /// track, which is the wording the window falls back on anyway.
    pub fn is_in_place(&self, id: &WorkspaceId) -> bool {
        self.find(id)
            .is_some_and(|(g, t)| self.groups[g].tabs[t].is_in_place())
    }
```

Replace `workspace_problem` with:

```rust
pub fn workspace_problem(state: &WorkspaceState) -> Option<WorkspaceProblem> {
    workspace_problem_for(state, WorkspaceKind::Worktree)
}

/// [`workspace_problem`] for a workspace of `kind`. Only the stopped-sandbox
/// sentence differs: an in-place workspace has no worktree to keep, and what
/// the user wants to hear is that their checkout was not touched.
pub fn workspace_problem_for(state: &WorkspaceState, kind: WorkspaceKind) -> Option<WorkspaceProblem> {
    match state {
        WorkspaceState::SandboxDown => Some(WorkspaceProblem {
            title: "The sandbox for this workspace is not running".to_owned(),
            detail: match kind {
                WorkspaceKind::Worktree => {
                    "It stopped unexpectedly. Retry starts it again; the worktree is kept."
                }
                WorkspaceKind::InPlace => {
                    "It stopped unexpectedly. Retry starts it again; your checkout is not touched."
                }
            }
            .to_owned(),
        }),
        WorkspaceState::Error(reason) => Some(WorkspaceProblem {
            title: "This workspace could not be started".to_owned(),
            detail: reason.clone(),
        }),
        WorkspaceState::Ready | WorkspaceState::Creating | WorkspaceState::Destroying => None,
    }
}
```

(keep the existing doc comment on `workspace_problem`). Add `kind: WorkspaceKind::default(),` (or `bondsymphonic_proto::WorkspaceKind::default()`) to the four `AgentTab { .. }` literals: `app_state.rs` test `tab()`, `group_model.rs` `tab_with_error()`, `crates/ide/tests/model_tests.rs` `tab()`, `crates/ide/tests/qobject_smoke.rs` `tab()`.

In `persistence.rs` add to `StateFile`, after `recent_repos`:

```rust
    /// Whether the New Agent dialog last created an in-place workspace, so it
    /// opens on the same choice. Written only when a dialog was accepted with
    /// the choice available.
    pub new_agent_in_place: bool,
```

and `new_agent_in_place: false,` in its `Default`.

- [ ] **Step 4: Group model.** In `group_model.rs`, in the bridge next to `workspace_name`:

```rust
        /// Whether `workspace_id` works directly in its repository's checkout.
        /// The window words Close and the menus from it; false for a workspace
        /// the model does not track.
        #[qinvokable]
        fn workspace_in_place(self: &GroupModel, workspace_id: QString) -> bool;
```

and the implementation:

```rust
    pub fn workspace_in_place(&self, workspace_id: QString) -> bool {
        self.rust()
            .workspaces
            .is_in_place(&WorkspaceId(workspace_id.to_string()))
    }
```

In `tab_tooltip`, after the `branch:` line, when `tab.is_in_place()` push `"\nworks in place: no branch or merge of its own"`.

- [ ] **Step 5: Controller.** In `app_controller.rs`, add near `note_workspace_created`:

```rust
/// The `workspace.create` the New Agent dialog's answers make. The base branch
/// of an in-place create is not sent: the daemon ignores it, and a branch name
/// on the wire would read as though one had been chosen.
pub fn create_params(
    repo_path: String,
    base_branch: String,
    name: String,
    init_if_missing: bool,
    in_place: bool,
) -> WorkspaceCreateParams {
    WorkspaceCreateParams {
        repo_path,
        base_branch: if in_place { String::new() } else { base_branch },
        name,
        init_if_missing,
        in_place,
    }
}
```

Add `in_place: bool` as the last parameter of both `create_workspace_with_run` and `create_workspace_with_agent_and_run`, in the bridge declarations (with a doc line: "`in_place` works directly in the checkout; `base_branch` is then ignored.") and the implementations, whose `WorkspaceCreateParams { .. }` become `create_params(repo_path.to_string(), base_branch.to_string(), name.to_string(), init_if_missing, in_place)`. Beside `recent_repos` in the bridge:

```rust
        /// Whether the New Agent dialog should open on "Work directly in this
        /// checkout".
        #[qinvokable]
        fn new_agent_in_place(self: &AppController) -> bool;

        /// Records the dialog's choice for next time.
        #[qinvokable]
        fn set_new_agent_in_place(self: &AppController, in_place: bool);
```

and:

```rust
    pub fn new_agent_in_place(&self) -> bool {
        state_store().with(|s| s.new_agent_in_place)
    }

    pub fn set_new_agent_in_place(&self, in_place: bool) {
        note_state_if_changed(|s| {
            let changed = s.new_agent_in_place != in_place;
            s.new_agent_in_place = in_place;
            changed
        });
    }
```

In `crates/ide/cpp/MainWindow.cpp` `onNewAgent`, append `, false` to the argument lists of `createWorkspaceWithAgentAndRun` and `createWorkspaceWithRun` (after `dialog.initIfMissing()`), nothing else.

- [ ] **Step 6: Run.**
Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --lib --test model_tests --test qobject_smoke --test restore_tests --test smoke`
Expected: PASS. Then `cargo clippy -p bondsymphonic-ide --all-targets --features bondsymphonic-ide/require-qt -- -D warnings` and `cargo fmt --all`.

- [ ] **Step 7: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/ide/src/model/app_state.rs crates/ide/src/model/persistence.rs crates/ide/src/qobjects/group_model.rs crates/ide/src/qobjects/app_controller.rs crates/ide/cpp/MainWindow.cpp crates/ide/tests/model_tests.rs crates/ide/tests/qobject_smoke.rs"
git add $P
git commit -m "feat(ide): tabs and creates carry the workspace kind" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 6: The window offers, shows and closes an in-place workspace

Depends on Task 5. May run beside Tasks 3 and 4 (it never touches daemon files).

**Files:**
- Modify: `crates/ide/cpp/NewAgentDialog.{h,cpp}` (mode choice, help, hooks warning, base-branch handling, widget test)
- Modify: `crates/ide/cpp/MainWindow.cpp` (`onNewAgent`, `onDestroyRequested`, Workspace-menu text, problem loop, `onCloseGroup`, `confirmDiscards`, `setWorkspaceHeader` call)
- Modify: `crates/ide/cpp/ChangesToolbar.{h,cpp}` (`setInPlace`, widget test)
- Modify: `crates/ide/cpp/ExplorerDock.{h,cpp}` (`setWorkspaceHeader(..., bool inPlace = false)`)
- Modify: `crates/ide/cpp/WorkspaceBanner.{h,cpp}` (`setInPlace`, widget test)
- Modify: `crates/ide/cpp/AgentArea.{h,cpp}` (`setWorkspaceProblem(..., bool inPlace = false)`)
- Modify: `crates/ide/cpp/GroupBar.cpp` (tab menu verb, seam match by object name)
- Modify: `crates/ide/cpp/CloseGroupDialog.{h,cpp}` (`CloseGroupChoice.inPlace`)
- Modify: `crates/ide/cpp/WorkspaceLabel.h` (`inPlace(tab)`, origin/detail wording)
- Test: `crates/ide/tests/qobject_smoke.rs` (`cpp_widgets`: three new entries, array length 36 → 39), `crates/ide/tests/smoke.rs` (append `mod in_place_close`)

**Interfaces:**
- Consumes (Task 5): `GroupModel::workspaceInPlace`, `AppController::newAgentInPlace`/`setNewAgentInPlace`, the `inPlace` argument of both `createWorkspace*` invokables, tab JSON `"kind"`, `RepoInfo` JSON `head_branch`, `in_place_refusal`, `hooks_path_in_tree`.
- Produces:
  ```cpp
  bool NewAgentDialog::inPlace() const;            // the choice as it will be sent
  bool NewAgentDialog::inPlaceAvailable() const;   // the choice could be made at all
  void ChangesToolbar::setInPlace(bool inPlace);
  void WorkspaceBanner::setInPlace(bool inPlace);
  void AgentArea::setWorkspaceProblem(const QString&, const QString&, const QString&, bool inPlace = false);
  void ExplorerDock::setWorkspaceHeader(name, branch, repoPath, baseBranch, worktreePath, bool inPlace = false);
  struct CloseGroupChoice { ...; bool inPlace = false; };
  namespace workspacelabel { inline bool inPlace(const QJsonObject& tab); }
  extern "C" std::int32_t bs_widget_test_new_agent_dialog_offers_the_checkout_itself();
  extern "C" std::int32_t bs_widget_test_changes_toolbar_hides_git_actions_in_place();
  extern "C" std::int32_t bs_widget_test_banner_says_close_for_an_in_place_workspace();
  ```
  Object names: `NewAgentWorktreeMode`, `NewAgentInPlaceMode`, `NewAgentInPlaceHelp`, `NewAgentHooksWarning`, `GroupBarDestroyAction`.

- [ ] **Step 1: Write the three failing widget tests and register them.**

At the foot of `NewAgentDialog.cpp`, inside the `BS_WIDGET_TESTS` block (add `#include <QRadioButton>` at the top of the file):

```cpp
/// The dialog offers the checkout itself, says what that means, warns about
/// hooks the agent could edit, and will not offer it where the daemon would
/// refuse.
extern "C" std::int32_t bs_widget_test_new_agent_dialog_offers_the_checkout_itself() {
    AppController controller;
    GroupModel model;
    NewAgentDialog dialog(&controller, &model, QString::fromUtf8(kDialogRepo));
    auto* worktree = dialog.findChild<QRadioButton*>(QStringLiteral("NewAgentWorktreeMode"));
    auto* inPlace = dialog.findChild<QRadioButton*>(QStringLiteral("NewAgentInPlaceMode"));
    auto* help = dialog.findChild<QLabel*>(QStringLiteral("NewAgentInPlaceHelp"));
    auto* warning = dialog.findChild<QLabel*>(QStringLiteral("NewAgentHooksWarning"));
    auto* branch = dialog.findChild<QComboBox*>(QStringLiteral("NewAgentBaseBranch"));
    if (worktree == nullptr || inPlace == nullptr || help == nullptr || warning == nullptr ||
        branch == nullptr) {
        return 1;
    }
    controller.repoInspected(
        QString::fromUtf8(kDialogRepo),
        QStringLiteral(R"({"is_repo":true,"branches":["main","feature"],"default_branch":"main",)"
                       R"("head_branch":"feature","hooks_path_in_tree":".husky/_"})"));
    // A worktree is the default, with the branches offered.
    if (!worktree->isChecked() || dialog.inPlace() || !help->isHidden() || !warning->isHidden() ||
        !branch->isEnabled() || branch->currentText() != QLatin1String("main")) {
        return 2;
    }

    inPlace->setChecked(true);
    if (!dialog.inPlace() || help->isHidden() || warning->isHidden()) {
        return 3;
    }
    if (help->text() != QLatin1String("The agent edits this folder on its current branch. Its "
                                      "changes are not isolated on a branch of their own.")) {
        return 4;
    }
    if (!warning->text().contains(QLatin1String(".husky/_")) ||
        !warning->text().contains(QLatin1String("outside the sandbox"))) {
        return 5;
    }
    // The branch is the checked-out one, shown and not chosen, and not sent.
    if (branch->isEnabled() || branch->currentText() != QLatin1String("feature") ||
        !dialog.baseBranch().isEmpty()) {
        return 6;
    }
    // Back to a worktree: the list comes back, with the user's choice.
    worktree->setChecked(true);
    if (!branch->isEnabled() || branch->count() != 2) {
        return 7;
    }

    // A detached HEAD says so.
    inPlace->setChecked(true);
    controller.repoInspected(QString::fromUtf8(kDialogRepo),
                             QStringLiteral(R"({"is_repo":true,"branches":["main"],)"
                                            R"("default_branch":"main"})"));
    if (branch->currentText() != QLatin1String("detached HEAD") || !warning->isHidden()) {
        return 8;
    }

    // A linked worktree cannot be worked in place: the choice is off, says
    // why, and the dialog falls back to a worktree.
    controller.repoInspected(
        QString::fromUtf8(kDialogRepo),
        QStringLiteral(R"({"is_repo":true,"branches":["main"],"default_branch":"main",)"
                       R"("head_branch":"main","in_place_refusal":"it is a linked worktree"})"));
    if (inPlace->isEnabled() || dialog.inPlace() || dialog.inPlaceAvailable() ||
        !inPlace->toolTip().contains(QLatin1String("linked worktree"))) {
        return 9;
    }
    return 0;
}
```

At the foot of `ChangesToolbar.cpp` (inside its `BS_WIDGET_TESTS` block):

```cpp
/// An in-place workspace has no branch of its own to merge, squash, push or
/// discard, so all five actions leave the toolbar -- and with it the Workspace
/// menu, which shows these same actions -- and come back for a worktree.
extern "C" std::int32_t bs_widget_test_changes_toolbar_hides_git_actions_in_place() {
    AppController controller;
    ChangesToolbar toolbar(&controller);
    toolbar.setWorkspace(QStringLiteral("ws_ip"), QStringLiteral("here"), QStringLiteral("main"),
                         QStringLiteral("main"));
    const QList<QAction*> actions{ toolbar.mergeAction(), toolbar.rebaseAction(),
                                   toolbar.squashAction(), toolbar.prAction(),
                                   toolbar.discardAction() };
    for (QAction* action : actions) {
        if (!action->isVisible()) {
            return 1;
        }
    }
    toolbar.setInPlace(true);
    for (QAction* action : actions) {
        if (action->isVisible()) {
            return 2;
        }
    }
    toolbar.setInPlace(false);
    for (QAction* action : actions) {
        if (!action->isVisible()) {
            return 3;
        }
    }
    return 0;
}
```

At the foot of `WorkspaceBanner.cpp`:

```cpp
/// The banner's second button names what it does to an in-place workspace:
/// Close, which touches none of the user's files, rather than Destroy.
extern "C" std::int32_t bs_widget_test_banner_says_close_for_an_in_place_workspace() {
    QWidget host;
    auto* banner = new WorkspaceBanner(&host);
    auto* remove = banner->findChild<QPushButton*>(QStringLiteral("WorkspaceBannerRemoveButton"));
    if (remove == nullptr) {
        return 1;
    }
    const QString ellipsis(QChar(0x2026));
    if (remove->text() != QStringLiteral("Destroy workspace") + ellipsis) {
        return 2;
    }
    banner->setInPlace(true);
    banner->showWorkspaceProblem(QStringLiteral("This workspace could not be started"),
                                 QStringLiteral("The repository /r is missing"));
    if (remove->isHidden() || remove->text() != QStringLiteral("Close workspace") + ellipsis ||
        !remove->toolTip().contains(QLatin1String("not touched"))) {
        return 3;
    }
    banner->setInPlace(false);
    if (remove->text() != QStringLiteral("Destroy workspace") + ellipsis) {
        return 4;
    }
    return 0;
}
```

In `crates/ide/tests/qobject_smoke.rs` `mod cpp_widgets`: add the three `fn bs_widget_test_...() -> i32;` declarations at the end of the `extern "C"` block, change `[(&str, unsafe extern "C" fn() -> i32); 36]` to `39`, and append at the end of the array:

```rust
            (
                "NewAgentDialog offers the checkout itself, and says what that means",
                bs_widget_test_new_agent_dialog_offers_the_checkout_itself,
            ),
            (
                "ChangesToolbar hides the git actions for an in-place workspace",
                bs_widget_test_changes_toolbar_hides_git_actions_in_place,
            ),
            (
                "WorkspaceBanner says Close for an in-place workspace",
                bs_widget_test_banner_says_close_for_an_in_place_workspace,
            ),
```

(The dialog check reads `newAgentInPlace()` from the state store, which is why it goes after the four MainWindow checks that redirect it, as every entry at the end does.)

- [ ] **Step 2: Run and see them fail to build.**
Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test qobject_smoke cpp_widgets`
Expected: FAIL — C++ compile errors (`inPlace`, `setInPlace` not members).

- [ ] **Step 3: Toolbar and banner.** `ChangesToolbar.h`, public:

```cpp
    /// Whether the workspace works directly in its checkout. Such a workspace
    /// has no branch of its own, so Merge, Rebase, Squash, Create PR and
    /// Discard are hidden, here and in the Workspace menu that shows these same
    /// actions. The dock sets this with the workspace.
    void setInPlace(bool inPlace);
```

`ChangesToolbar.cpp`:

```cpp
void ChangesToolbar::setInPlace(bool inPlace) {
    for (QAction* action : { m_merge, m_rebase, m_squash, m_pr, m_discard }) {
        action->setVisible(!inPlace);
    }
}
```

`WorkspaceBanner.h`, public: `/// Words the Remove button for an in-place workspace, whose Close touches none of the user's files. Sticky, like the restart offer.` `void setInPlace(bool inPlace);`. `WorkspaceBanner.cpp`:

```cpp
void WorkspaceBanner::setInPlace(bool inPlace) {
    // The window's verb for the same act, so the button and the question
    // behind it agree.
    m_remove->setText((inPlace ? QStringLiteral("Close workspace")
                               : QStringLiteral("Destroy workspace")) +
                      QChar(0x2026));
    m_remove->setToolTip(
        inPlace ? QStringLiteral("Stop this workspace's agent and sandbox, after asking. Your "
                                 "files, branches and git history are not touched.")
                : QStringLiteral("Remove this workspace's sandbox, worktree and branch, after "
                                 "asking"));
}
```

`AgentArea.h`: `setWorkspaceProblem(const QString& workspaceId, const QString& title, const QString& detail, bool inPlace = false);` and `bool inPlace = false;` in `struct WorkspaceProblem`. `AgentArea.cpp`: the "same problem again" check also compares `found->inPlace == inPlace`; the insert becomes `m_problems.insert(workspaceId, { title, detail, false, inPlace });`; `applyWorkspaceProblem` calls `banner->setInPlace(found->inPlace);` before `banner->showWorkspaceProblem(...)`.

- [ ] **Step 4: Dialog.** `NewAgentDialog.h`: forward-declare `class QRadioButton;`, add public

```cpp
    /// Whether the agent is to work directly in the checkout rather than in a
    /// new worktree. Only ever true when the choice was available.
    bool inPlace() const;
    /// Whether the last inspection allowed working in place at all. The window
    /// remembers the choice only then, so a linked worktree that forced a
    /// worktree does not overwrite what the user prefers.
    bool inPlaceAvailable() const;
```

private `void updateModeState();` (doc: "Puts the base-branch row, the help and the hooks warning into the state the chosen mode and the last inspection describe.") and members:

```cpp
    QRadioButton* m_worktreeMode = nullptr;
    QRadioButton* m_inPlaceMode = nullptr;
    /// What working in place means, under the choice while it is selected.
    QLabel* m_inPlaceHelp = nullptr;
    /// The repository runs hooks from inside its working tree, which an agent
    /// working in place can rewrite. Plain text: the path is the repository's.
    QLabel* m_hooksWarning = nullptr;
    /// From the last inspection: the branches, the default, the checked-out
    /// branch (empty for a detached HEAD), why in place is refused (empty when
    /// it is not) and the in-tree hooks path (empty when there is none).
    QStringList m_branches;
    QString m_defaultBranch;
    QString m_headBranch;
    QString m_inPlaceRefusal;
    QString m_hooksPath;
```

`NewAgentDialog.cpp` constructor, directly after `form->addRow(QString(), m_repoState);`:

```cpp
    // Where the agent works. A worktree is the default: it is the choice that
    // keeps the agent's work off the user's own branch.
    auto* modes = new QWidget(this);
    auto* modesLayout = new QVBoxLayout(modes);
    modesLayout->setContentsMargins(0, 0, 0, 0);
    m_worktreeMode = new QRadioButton(QStringLiteral("Work in a new worktree"), modes);
    m_worktreeMode->setObjectName(QStringLiteral("NewAgentWorktreeMode"));
    m_inPlaceMode = new QRadioButton(QStringLiteral("Work directly in this checkout"), modes);
    m_inPlaceMode->setObjectName(QStringLiteral("NewAgentInPlaceMode"));
    modesLayout->addWidget(m_worktreeMode);
    modesLayout->addWidget(m_inPlaceMode);
    (m_controller->newAgentInPlace() ? m_inPlaceMode : m_worktreeMode)->setChecked(true);
    form->addRow("Work in:", modes);

    m_inPlaceHelp = new QLabel(QStringLiteral("The agent edits this folder on its current branch. "
                                              "Its changes are not isolated on a branch of their "
                                              "own."),
                               this);
    m_inPlaceHelp->setObjectName(QStringLiteral("NewAgentInPlaceHelp"));
    m_inPlaceHelp->setWordWrap(true);
    m_inPlaceHelp->setTextFormat(Qt::PlainText);
    form->addRow(QString(), m_inPlaceHelp);

    m_hooksWarning = new QLabel(this);
    m_hooksWarning->setObjectName(QStringLiteral("NewAgentHooksWarning"));
    m_hooksWarning->setWordWrap(true);
    m_hooksWarning->setTextFormat(Qt::PlainText);
    // The same red as the name hint: this is a thing that can go wrong.
    m_hooksWarning->setStyleSheet(
        QStringLiteral("color:%1")
            .arg(theme::ink(theme::removed(), theme::isDark(palette())).name()));
    form->addRow(QString(), m_hooksWarning);
```

After the other `connect` calls:

```cpp
    QObject::connect(m_inPlaceMode, &QRadioButton::toggled, this, [this] {
        updateModeState();
        updateOkEnabled();
    });
```

and call `updateModeState();` just before the final `updateOkEnabled();` of the constructor. Definitions:

```cpp
bool NewAgentDialog::inPlace() const {
    return m_inPlaceMode->isChecked() && inPlaceAvailable();
}

bool NewAgentDialog::inPlaceAvailable() const {
    return m_inPlaceRefusal.isEmpty() && !m_inspectPending && !m_inspectFailed;
}

void NewAgentDialog::updateModeState() {
    m_inPlaceMode->setEnabled(m_inPlaceRefusal.isEmpty());
    m_inPlaceMode->setToolTip(m_inPlaceRefusal);
    if (!m_inPlaceRefusal.isEmpty() && m_inPlaceMode->isChecked()) {
        // Said by the tooltip on the disabled choice; the daemon would refuse
        // the create with the same sentence.
        m_worktreeMode->setChecked(true);
    }
    const bool inPlace = m_inPlaceMode->isChecked();
    m_form->setRowVisible(m_inPlaceHelp, inPlace);
    m_hooksWarning->setText(
        m_hooksPath.isEmpty()
            ? QString()
            : QStringLiteral("This repository runs git hooks from %1 inside the working tree. "
                             "The agent can change them, and they run outside the sandbox the "
                             "next time you use git here.")
                  .arg(m_hooksPath));
    m_form->setRowVisible(m_hooksWarning, inPlace && !m_hooksPath.isEmpty());
    if (m_inspectPending || m_inspectFailed) {
        // `updateBranchState` owns the row while an answer is out or missing.
        return;
    }
    const QSignalBlocker quiet(m_baseBranch);
    m_baseBranch->clear();
    if (inPlace) {
        // Shown, not chosen: nothing is switched and nothing is sent.
        m_baseBranch->addItem(m_headBranch.isEmpty() ? QStringLiteral("detached HEAD")
                                                     : m_headBranch);
        m_baseBranch->setEnabled(false);
        return;
    }
    m_baseBranch->setEnabled(true);
    m_baseBranch->addItems(m_branches);
    const QString preferred = m_branchChoice.isEmpty() ? m_defaultBranch : m_branchChoice;
    const int index = m_baseBranch->findText(preferred);
    if (index >= 0) {
        m_baseBranch->setCurrentIndex(index);
    } else {
        m_baseBranch->setEditText(preferred);
    }
}
```

(add `#include <QSignalBlocker>`). `baseBranch()` becomes `return inPlace() ? QString() : m_baseBranch->currentText().trimmed();`. In `inspectRepo`, before `updateBranchState()`, remember the choice only when the combo is offering branches in worktree mode: change `if (m_baseBranch->isEnabled())` to `if (m_baseBranch->isEnabled() && !m_inPlaceMode->isChecked())`, and clear `m_inPlaceRefusal`, `m_hooksPath`, `m_headBranch` there. In `onRepoInspected`, replace the combo filling (from `m_baseBranch->clear();` to the `setEditText` branch) with:

```cpp
    m_branches.clear();
    for (const QJsonValue& value : info.value("branches").toArray()) {
        m_branches.append(value.toString());
    }
    m_defaultBranch = info.value("default_branch").toString();
    // A folder about to be initialised will be on `main`, which is also what
    // `default_branch` says for one.
    m_headBranch = info.value("is_repo").toBool(true) ? info.value("head_branch").toString()
                                                     : m_defaultBranch;
    m_inPlaceRefusal = info.value("in_place_refusal").toString();
    m_hooksPath = info.value("hooks_path_in_tree").toString();
    updateModeState();
```

In `onRepoInspectFailed`, after `updateBranchState();`, call `updateModeState();`. In `updateOkEnabled`, replace `!baseBranch().isEmpty()` with `(inPlace() || !baseBranch().isEmpty())`.

- [ ] **Step 5: Labels, header, menus, Close.** `WorkspaceLabel.h`:

```cpp
/// Whether a tab's workspace works directly in its checkout.
inline bool inPlace(const QJsonObject& tab) {
    return tab.value(QStringLiteral("kind")).toString() == QLatin1String("in_place");
}

/// What `origin` adds for such a workspace. It has no branch of its own, so
/// the one named is the checkout's, and saying so is what keeps a user from
/// thinking there is something to merge.
inline QString inPlaceSuffix() {
    return QStringLiteral(" (in place)");
}
```

and make the three JSON wrappers kind-aware: `origin(tab)` and `originFull(tab)` append `inPlaceSuffix()` when `inPlace(tab)` and the result is not empty; `detail(tab)` for an in-place tab returns `originFull(repo, base) + "\nworks in place: no branch or merge of its own\ncheckout: " + worktree_path` (instead of the `branch:`/`worktree:` lines).

`ExplorerDock.h/.cpp`: add `bool inPlace = false` as the last parameter of `setWorkspaceHeader`; first line of its body `m_changesToolbar->setInPlace(inPlace);`; the detail text becomes `workspacelabel::origin(repoPath, baseBranch) + (inPlace ? workspacelabel::inPlaceSuffix() : QString())`, and the tooltip for in place uses the same wording as `detail(tab)` (build it inline with the same three lines).

`MainWindow.cpp`:
- The `setWorkspaceHeader` call passes `workspacelabel::inPlace(active)` as the new last argument.
- After `m_destroyAction->setEnabled(live);` add
  ```cpp
    // The verb follows the workspace: an in-place one is closed, and nothing
    // of the user's goes with it.
    m_destroyAction->setText(workspacelabel::inPlace(active) ? QStringLiteral("&Close workspace…")
                                                             : QStringLiteral("&Destroy workspace…"));
  ```
- In the workspace-problem loop, compute `const bool inPlace = workspacelabel::inPlace(tab);`, make `shown` end with `+ (inPlace ? QStringLiteral("\nin place") : QString())`, and call `m_agentArea->setWorkspaceProblem(workspaceId, title, detail, inPlace);`.
- `onNewAgent`: replace the two trailing `false` arguments Task 5 added with `dialog.inPlace()`, and right after the `result != Accepted` early return add
  ```cpp
    if (dialog.inPlaceAvailable()) {
        m_controller->setNewAgentInPlace(dialog.inPlace());
    }
  ```
- `onDestroyRequested`: after the busy check,

```cpp
    if (m_groupModel->workspaceInPlace(workspaceId)) {
        // Nothing of the user's is removed, so there is nothing to force and
        // nothing to ask twice about: one plain question, one plain destroy.
        const QString question =
            (workspaceName.isEmpty()
                 ? QStringLiteral("Close this workspace?")
                 : QStringLiteral("Close workspace \"%1\"?").arg(workspaceName)) +
            QStringLiteral(" The agent and its sandbox stop. Your files, branches and git "
                           "history are not touched.");
        if (announceMenuTest("destroy", workspaceId, question)) {
            if (menuTestTarget("destroy-yes") == workspaceId) {
                m_controller->destroyWorkspace(workspaceId, false);
            }
            return;
        }
        if (QMessageBox::question(this, QStringLiteral("Close workspace"), question,
                                  QMessageBox::Yes | QMessageBox::Cancel,
                                  QMessageBox::Cancel) != QMessageBox::Yes) {
            return;
        }
        if (isWorkspaceBusy(workspaceId)) {
            sayWorkspaceIsBusy();
            return;
        }
        m_controller->destroyWorkspace(workspaceId, false);
        return;
    }
```

- `onCloseGroup`: `choice.inPlace = workspacelabel::inPlace(tab);`. `confirmDiscards`: `if (choice.action != CloseGroupAction::Discard || choice.inPlace) { continue; }` (a Close loses nothing, so it is not in the question).

`CloseGroupDialog.h`: `/// The workspace works in its checkout: it cannot be merged, and discarding it only closes it.` `bool inPlace = false;` in `CloseGroupChoice`. `CloseGroupDialog.cpp`, in the row loop:

```cpp
        for (const CloseGroupAction action : kActions) {
            // An in-place workspace has nothing to merge, and its "discard"
            // closes it without touching the checkout, so it is worded as that.
            if (choice.inPlace && action == CloseGroupAction::Merge) {
                continue;
            }
            combo->addItem(choice.inPlace && action == CloseGroupAction::Discard
                               ? QStringLiteral("Close (files are kept)")
                               : actionLabel(action),
                           static_cast<int>(action));
        }
```

`GroupBar.cpp` `openAgentMenu`:

```cpp
    QAction* destroyAction =
        workspaceId.isEmpty()
            ? nullptr
            : menu.addAction(m_model->workspaceInPlace(workspaceId) ? "Close workspace…"
                                                                   : "Destroy workspace…");
    if (destroyAction != nullptr) {
        destroyAction->setObjectName(QStringLiteral("GroupBarDestroyAction"));
    }
```

and in `execMenu`'s seam loop match `action->text() == m_menuTestChoice || (m_menuTestChoice == QStringLiteral("Destroy workspace…") && action->objectName() == QLatin1String("GroupBarDestroyAction"))`, so the `destroy` seam step reaches the item whichever verb it carries.

- [ ] **Step 6: Run the widget tests.** Same command as Step 2. Expected: PASS, including the 36 existing checks.

- [ ] **Step 7: Write the smoke test.** Append to `crates/ide/tests/smoke.rs`:

```rust
/// In-place workspaces: the banner of an in-place workspace that cannot run
/// asks to *close* it, in words that promise the checkout is untouched, and a
/// yes sends exactly one destroy, unforced. The fake daemon reports the
/// workspace with `kind: in_place`, which is the only way the IDE learns it.
///
/// The seam steps name the one workspace they may press on.
mod in_place_close {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "in-place-close-token";
    const SCRIPT: &str = "wait,quit";
    const MENU_TEST: &str = "sandbox-banner,sandbox-remove=ws_inplace1,destroy-yes=ws_inplace1";
    const WORKSPACE: &str = "ws_inplace1";
    const NAME: &str = "checkout-one";
    const REASON: &str = "The repository /smoke/repo is missing or is no longer a git \
                          repository. Close the workspace, or restore the folder and press Retry.";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn an_in_place_workspace_is_closed_with_one_plain_destroy() {
        if bondsymphonic_ide::testing::skip_without_qt("in-place close") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config =
            std::env::temp_dir().join(format!("bs-in-place-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "here".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);

        let question = out
            .lines()
            .find(|l| l.starts_with(&format!("BS_MENU_TEST destroy target={WORKSPACE} ")))
            .unwrap_or_else(|| panic!("Close did not ask\n{context}"));
        assert!(
            question.ends_with(&format!(
                "question=Close workspace \"{NAME}\"? The agent and its sandbox stop. Your \
                 files, branches and git history are not touched."
            )),
            "{question}"
        );
        assert!(!out.contains("BS_MENU_TEST destroy-refused"), "{context}");
        let destroys: Vec<&String> = seen
            .iter()
            .filter(|m| m.starts_with("workspace.destroy"))
            .collect();
        assert_eq!(
            destroys,
            [&format!("workspace.destroy:{WORKSPACE}:false")],
            "{context}"
        );
    }

    /// One in-place workspace that cannot run. Any destroy succeeds: the daemon
    /// ignores `force` for this kind, and a second, forced one would be the bug.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut destroyed = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}:{}", p.workspace_id.0, p.force)
                        }
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let listed = if destroyed { vec![] } else { vec![workspace()] };
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => {
                            ServerMessage::ok(id, &WorkspaceListResult { workspaces: listed })
                        }
                        Request::WorkspaceGet(_) => ServerMessage::ok(id, &workspace()),
                        Request::WorkspaceDestroy(_) => {
                            destroyed = true;
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    /// No agent, for the reason `sandbox_remove_dirty` gives: the question is
    /// about the workspace.
    fn workspace() -> WorkspaceInfo {
        WorkspaceInfo {
            kind: WorkspaceKind::InPlace,
            worktree_path: "/smoke/repo".to_owned(),
            branch: "main".to_owned(),
            ..super::workspace(
                WORKSPACE,
                NAME,
                WorkspaceState::Error(REASON.to_owned()),
                &[],
            )
        }
    }
}
```

- [ ] **Step 8: Run.**
Run: `cargo test -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --test smoke in_place_close` and then `--test smoke --test qobject_smoke` in full.
Expected: PASS. Control check: temporarily pass `true` in the in-place `destroyWorkspace` call and confirm the smoke test fails, then restore. Then clippy (IDE) and `cargo fmt --all` (C++ is not formatted by it; keep the surrounding clang-format-like style by hand).

- [ ] **Step 9: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="crates/ide/cpp crates/ide/tests/qobject_smoke.rs crates/ide/tests/smoke.rs"
git status --short crates/ide/cpp   # only the files listed for this task
git add $P
git commit -m "feat(ide): New Agent can work in the checkout, and such a workspace is closed, not destroyed" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 7: Docs

Depends on Tasks 3, 4 and 6 (the docs describe what landed; read the diffs first: `git log --stat -8`).

**Files:**
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§4, §5.3, §5.4/§5.5, §6.2)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md` (§6.1, §6.3)
- Modify: `docs/user-guide.md` (install line, Creating an agent workspace, new section, Finishing a workspace, Troubleshooting)

No tests; the check is a read-through against the code. The in-place spec (`docs/superpowers/specs/2026-09-17-in-place-workspaces-design.md`) was already brought in line with R1–R10 while planning; if an implementation step had to deviate from it, update that spec here too.

- [ ] **Step 1: Daemon design.**
  - §4 Workspaces: a subsection **4.x Workspace kinds** with the table from spec §2 (`kind`, `worktree_path`, `branch`, `base_branch`) and one paragraph each for create (spec §3.1 plus R7: `/`, `$HOME`, inside or containing the data directory, `.git` a file, linked worktree, bare, one in-place workspace per checkout by canonical path, `Conflict` sentence), restore/restart (no `ensure_registered`; the `Error` sentence verbatim), destroy ("Close": `force` ignored, no dirty/unmerged check, teardown, `release()` of the guard and of the empty on-demand directories the daemon recorded creating, never `worktree::remove`), and `layout_for` refusing an in-place workspace.
  - §5.3 Changes and diff: in place measures against `HEAD` (empty tree when unborn) with `InPlaceLayout::git()`; every daemon-side status/diff carries `--ignore-submodules=all`, and why (R3; the config key is not enough because `.gitmodules` overrides it). Note the rebase step as the known exception.
  - §5.4/§5.5: merge and PR refuse an in-place workspace with `data.reason = "in_place"`.
  - §6.2 Linux/bwrap filesystem rules: an **In-place workspaces** paragraph listing the binds in order (rw root, rw `.git`, late ro `config`, `commondir`, `hooks`, `info`, `worktrees`, `remotes`, `branches`, `modules` if a real directory, `config.worktree` if the extension is on), what `prepare` creates and why (bwrap creates missing targets on the writable repository — measured, R5), the `commondir` guard and why (R1, with the git versions measured), why `worktrees`, `remotes` and `branches` are read-only and how the daemon records which of them it created (`<data>/in-place/<id>.created`) so Close removes only those, and only when empty (R2), that mount points cannot be renamed or removed, and that `rm -rf .git` still deletes everything that is not a mount point (R4). No private object directory, no `GIT_OBJECT_DIRECTORY`.

- [ ] **Step 2: Overview.** §6.1/§6.3: `PROTOCOL_VERSION` is `2` (version 2 added in-place workspaces; a peer that sends no version is version 1 and is refused); `workspace.create` gains `in_place`; `repo.inspect` returns `head_branch`, `in_place_refusal`, `hooks_path_in_tree`; `WorkspaceInfo` carries `kind`; merge/PR `InvalidParams` with `reason: "in_place"`. Update the "which is `1`" sentence at line 152 and the `repo.inspect` result line at 175.

- [ ] **Step 3: User guide.**
  - Install: `# bondsymphonic-ide 0.1.0 (protocol 2)`.
  - "Creating an agent workspace": the **Work in** choice, both labels verbatim, the help sentence, the hooks warning, and when the choice is greyed out (linked worktree, `.git` a file) and that the dialog remembers the last choice.
  - New section **Working directly in a checkout**, after "Creating an agent workspace": what it is (the agent edits your folder on its current branch, inside the same sandbox, allowlist, runs and tabs; no branch, no merge), when to use it (a quick change you will review and commit yourself; a repository whose tooling cannot live in a second worktree), what the agent may do with git (stage, commit, switch, stash) and cannot (write `.git/config`, hooks, `.git/info`, `.git/commondir`, `.git/worktrees`, `.git/remotes`, `.git/branches`, submodule git dirs; commands that write config such as `git push -u`, `git remote add`, tracking setup — R10), what BondSymphonic writes into `.git` (`commondir` containing `.`, and `hooks`, `info`, `worktrees`, `remotes`, `branches` when missing; `config.worktree` when that extension is on) and that Close removes the `commondir` and, of the last three, those it created that are still empty, the Changes tab (against `HEAD`, no Merge/Rebase/Squash/Create PR/Discard), **Close workspace…** and its exact question, one in-place workspace per checkout (worktree workspaces beside it are fine), and **Residual risks**: `core.hooksPath` inside the tree; `.gitattributes` choosing your own filter/diff drivers; a rebase left in progress with `exec` lines; scripts in the tree; an embedded repository the agent commits as a submodule runs its own config the next time *your* git looks at it (R3); the agent can delete the repository's history (`rm -rf .git` removes everything but the protected entries, R4) just as it can delete files — keep a remote or a backup.
  - "Finishing a workspace": one paragraph pointing in-place users to their own `git commit`/`git push`, and to Close.
  - "Troubleshooting": the `Error` sentence for a moved or deleted checkout and what Retry/Close do; the protocol-mismatch line now names version 2.

- [ ] **Step 4: Commit.**

```bash
cd /c/git/BondSymphonic-inplace
P="docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md docs/superpowers/specs/2026-09-08-bondsymphonic-overview-design.md docs/user-guide.md"
git add $P
git commit -m "docs: in-place workspaces" -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>" -- $P
```

---

## Task 8: Full verification

Depends on Task 7. Fixes found here go back into the owning task's files and are committed by path with a `fix(...)` message.

- [ ] **Step 1: Daemon suite in WSL, all of it.**

```
wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic-inplace && export CARGO_TARGET_DIR=~/.bondsymphonic/target-inplace BS_DAEMON_EXE=~/.bondsymphonic/target-inplace/debug/bondsymphonic-daemon && cargo build -p bondsymphonic-daemon && cargo test -p bondsymphonic-proto -p bondsymphonic-daemon --no-fail-fast 2>&1 | tail -80"
```

Expected: every test passes; `SKIP: bwrap unavailable` must **not** appear (the distro has bwrap).

- [ ] **Step 2: IDE suite.**

```
. .\scripts\env.ps1; $env:CARGO_TARGET_DIR="C:\git\BondSymphonic-inplace\target"; cargo test -p bondsymphonic-proto -p bondsymphonic-ide --features bondsymphonic-ide/require-qt --no-fail-fast
```

Expected: everything passes except `packaged_smoke`, which fails by design without `BS_PACKAGED_EXE`.

- [ ] **Step 3: Lint.**

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features bondsymphonic-ide/require-qt -- -D warnings
wsl -d bondsymphonic -- bash -lc "cd /mnt/c/git/BondSymphonic-inplace && CARGO_TARGET_DIR=~/.bondsymphonic/target-inplace cargo clippy -p bondsymphonic-proto -p bondsymphonic-daemon --all-targets -- -D warnings"
```

Expected: clean.

- [ ] **Step 4: Nothing strayed.** `git status --short` is empty; `git log --stat main..HEAD` touches only the files named in this plan; no file under `~/.bondsymphonic` other than `target-inplace` changed (`ls -la ~/.bondsymphonic` in WSL, compare modification times of `workspaces.json`/`agents.json` with before the run).

---

## Self-review (done while writing)

- **Spec coverage.** §2 model/protocol → Task 1. §3.1 create → Task 3 Step 5 (+R7). §3.2 restore/restart → Task 3 Step 6 and test `a_checkout_that_went_away...`. §3.3 destroy → Task 3 Step 6, byte-identity test for both `force` values. §3.4 changes/diff/status/merge/PR/pinned git/`Layout` guard → Tasks 3 (status, `layout_for`) and 4. §4.1 mounts → Task 2 (`late_ro_binds`, `prepare`, bwrap probe) and Task 3 (`in_place_spec_for`, "binds create only what is documented" test). §4.2 residual risks → Task 7. §4.3 hooks warning → Task 4 (`hooks_path_in_tree`) and Task 6 (dialog). §4.4 noop → unchanged. §5.1 dialog → Task 6 Step 4. §5.2 presentation → Task 6 Step 5 (labels, toolbar, Close wording in menu, tab menu and banner). §5.3 restore → Task 5 (`kind` from `WorkspaceInfo`, not stored). §6 testing → each item has a named test above; "kind survives a reconnect and restore" is `a_tab_takes_its_kind_from_the_daemon_and_keeps_it` (the reconnect path is the same `from_persisted`/`apply_workspace_info` pair). §7 docs → Task 7.
- **Placeholders.** None; the only prose-described edits are mechanical fixture additions (Task 1 Steps 6–7) and docs (Task 7), each with exact content.
- **Type consistency.** `InPlaceLayout::{new, git, prepare, release, rw_binds, late_ro_binds, check_repository, worktree_config_enabled}`, `in_place::{head_branch, diff_base, in_place_refusal, target_refusal, nothing_to_merge, EMPTY_TREE, COMMONDIR_GUARD, IN_PLACE_REASON, ON_DEMAND_DIRS}`, `DataDirs::in_place_record`, `lifecycle::in_place_spec_for`, `repo::hooks_path_in_tree`, `app_state::workspace_problem_for`, `Workspaces::is_in_place`, `app_controller::create_params`, `GroupModel::workspaceInPlace`, `AppController::{newAgentInPlace,setNewAgentInPlace}`, `ChangesToolbar::setInPlace`, `WorkspaceBanner::setInPlace`, `workspacelabel::{inPlace,inPlaceSuffix}` are spelled the same in every task that uses them.

## Known follow-ups (out of scope for this plan)

- **The rebase in `crates/daemon/src/git/merge.rs` can run an embedded repository's config (R3).** `workspace.merge` with mode `rebase` runs `git rebase` through `Layout::worktree_git()` in a worktree workspace's worktree, and git's own clean-tree check inside `rebase` looks into submodules; there is no `--ignore-submodules` option for `rebase`. An agent that commits an embedded repository with a `core.fsmonitor` in its `.git/config` can therefore have that command run by the daemon when the user presses Rebase. Worktree workspaces only (an in-place workspace refuses merges). Needs its own measurement and fix — for example refusing to rebase a branch that adds a gitlink, or running the rebase with `GIT_CONFIG_*` overrides measured to stop the child git.
