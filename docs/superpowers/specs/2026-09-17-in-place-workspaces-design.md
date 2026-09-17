# In-place workspaces — design

Date: 2026-09-17. Status: approved in chat by the user.

## 1. Goal

Let the user run an agent **directly in a repository's own checkout**, on whatever
branch is checked out, instead of in a new `bs/<name>/work` worktree. It is the same
as running Claude Code in that folder yourself, but inside BondSymphonic's sandbox,
network allowlist, runs and agent tabs. There is no branch and no merge step: the
changes are simply in the checkout.

Decisions made with the user:

- The agent works on the checkout as it is. BondSymphonic creates no branch and
  switches none.
- The agent may use git fully (stage, commit, switch, stash), with one exception:
  the files a git running *outside* the sandbox would execute from — `.git/config`,
  hooks and the like — stay read-only.
- Approach: a second *kind* of workspace in the existing model, not a separate
  concept.

Out of scope: several agents in one checkout at once (one in-place workspace per
checkout), in-place workspaces on a linked worktree or a bare repository, merge or
PR from an in-place workspace, and submodule commits by the agent.

## 2. Model and protocol

`Workspace` (daemon registry) and `WorkspaceInfo` (proto) gain

```rust
#[serde(rename_all = "snake_case")]
enum WorkspaceKind { Worktree, InPlace }   // #[serde(default)] = Worktree
```

For an in-place workspace:

| Field | Value |
|---|---|
| `kind` | `in_place` |
| `worktree_path` | the repository root, i.e. the directory the agent works in |
| `branch` | the branch checked out at creation, or `""` when `HEAD` is detached. Display only; it is never switched, created or deleted. |
| `base_branch` | the same as `branch`. Nothing merges into it. |

A registry written by an older daemon has no `kind`, so it reads as `worktree`.

`WorkspaceCreateParams` gains `#[serde(default)] in_place: bool`. With it set,
`base_branch` is ignored and may be empty.

`RepoInfo` (the `repo.inspect` result) gains
`#[serde(default)] hooks_path_in_tree: Option<String>`: the repository's effective
`core.hooksPath` when it resolves to a directory inside the working tree (husky
does this), otherwise `None`. See §4.3.

`PROTOCOL_VERSION` goes from 1 to 2. An older daemon would ignore `in_place` and
quietly make a worktree, and the hello gate is what stops that. The IDE already
installs its own daemon before every launch (M7).

## 3. Daemon: lifecycle

### 3.1 Create (`in_place: true`)

1. Validate the name as today. Classify the path with `repo::classify`.
   `init_if_missing` works as today, so a folder can be initialised and then used
   in place.
2. Require a **repository root with a `.git` directory**. A linked worktree (where
   `.git` is a file), a bare repository and a path inside another repository are
   refused with `InvalidParams` and a sentence saying why. The existing refusals
   (`/`, `$HOME`, the data directory, a workspace worktree) apply unchanged.
3. Refuse with `Conflict` ("this checkout already has an in-place workspace:
   <name>") if a registered in-place workspace has the same canonical
   `worktree_path`. Worktree workspaces on the same repository are allowed
   alongside it.
4. No branch, no `git worktree add`, no lock, no ref directories, no private
   object directory. Record `branch` from `git symbolic-ref --short -q HEAD`.
5. From here on as today: seed the home (§8.3; `.claude.json` trusts
   `worktree_path`), persist, start the sandbox, `Ready` or `Error`.

The per-repository lock is held for steps 1–4, as for a worktree create.

### 3.2 Restore and `workspace.restart`

`ensure_registered` (worktree repair and lock) is skipped for `in_place`. What
remains to check is that `worktree_path` exists and has a `.git` directory;
otherwise the workspace is set to `Error("The repository <path> is missing or is no
longer a git repository. Close the workspace, or restore the folder and press
Retry.")`. The per-workspace gate, the teardown and the rest of the restart are
unchanged.

### 3.3 Destroy ("Close")

For `in_place`:

- `force` is ignored, and there is no dirty or unmerged check: nothing of the
  user's is deleted.
- Stop agents, runs, PTYs, the proxy and the sandbox; delete the daemon's own
  `homes/<id>`, `caches/<id>`, `run/<id>` and the agent records and transcripts,
  as today.
- **Never** run `worktree::remove` and never touch `worktree_path`, its `.git`,
  or any branch. A test asserts that the repository is byte-identical afterwards
  (tree listing plus `git status --porcelain=v2` plus `git for-each-ref` plus
  `.git/config` contents).

### 3.4 Changes, diff, status, merge, PR

- `workspace.status`, `workspace.changes` and `workspace.diff` compare against
  `HEAD` instead of `merge-base(base_branch, HEAD)`: staged, unstaged and
  untracked changes, the same as `git status`. The result types are unchanged.
- `workspace.merge` and `workspace.create_pr` answer `InvalidParams` with
  `data.reason = "in_place"` ("an in-place workspace has nothing to merge; commit
  and push from the checkout").
- Daemon-side git for an in-place workspace pins `GIT_DIR=<root>/.git`,
  `GIT_WORK_TREE=<root>` and `core.hooksPath=<no_hooks>`, like `daemon_git`. The
  repository config is the user's own and is read-only to the agent, so the
  `NEUTRALISED_CONFIG` list is not applied, for the same reason as `daemon_git`.

`Layout` gets a kind-aware constructor, or an `InPlaceLayout` beside it. The
implementer decides which, as long as no worktree-only path (`ref_dir`,
`worktree_gitdir`, `objects_dir`, `config_worktree`) can be reached for an
in-place workspace by accident. A test covers destroy and restore.

## 4. Daemon: sandbox (`linux_bwrap`)

### 4.1 Mounts for `in_place`

In this order, the later ones on top of the earlier ones (`SandboxSpec` already
separates `rw_binds`, `ro_binds` and `late_ro_binds`):

1. rw: `<root>`.
2. rw: `<root>/.git`, a mount of its own. A mount point cannot be renamed,
   removed or replaced (`EBUSY`), so the agent cannot swap `.git` for a directory
   of its own making.
3. ro, late: `<root>/.git/config`, `<root>/.git/hooks`, `<root>/.git/info` and
   `<root>/.git/modules` (the last only when it exists).
4. ro, late: `<root>/.git/config.worktree`, but only when
   `extensions.worktreeConfig` is enabled. If the file is missing, the daemon
   creates it empty first: git reads an empty file as no configuration.
5. If `.git/hooks` or `.git/info` is missing, the daemon creates the empty
   directory before binding it read-only. Git itself would create both, and an
   empty directory changes nothing.

There is no private object directory and no `GIT_OBJECT_DIRECTORY` or
`GIT_ALTERNATE_OBJECT_DIRECTORIES`: the agent's objects go into the repository's
own store. The proxy, home, cache, run dir, Claude binary and environment are as
for a worktree workspace; `cwd` is `<root>`.

The implementer must check, with bwrap 0.9 in the distro, that none of these
binds creates a file or directory in the user's repository beyond the two
creations in steps 4 and 5. The daemon makes those itself, so bwrap never has to.

### 4.2 What this protects, and what it does not

This keeps the sandbox boundary for any git the user runs later, on either side of
WSL: the agent cannot plant a hook, a `core.fsmonitor`, a `core.sshCommand` or a
filter driver, and cannot redirect `.git`.

Residual risks, documented in the user guide and not mitigated further:

- **`core.hooksPath` inside the working tree** (§4.3). Hooks there are ordinary
  files the agent can edit, and the user's git runs them.
- **`.gitattributes` in the working tree** can select filter or diff drivers, but
  only drivers already defined in the user's own read-only config. This risk
  exists today for merged worktree content too.
- **Sequencer state:** a rebase the agent leaves in progress can carry `exec`
  lines that run when the user types `git rebase --continue`.
- **Scripts in the tree** (`package.json` scripts, `Makefile`, …) run when the
  user runs them. That is inherent in letting an agent edit your checkout.
- The agent cannot init or commit in submodules (`.git/modules` is read-only).

### 4.3 `core.hooksPath` warning

`repo.inspect` resolves `git config --get core.hooksPath`, relative to the root, and
reports it in `hooks_path_in_tree` when it lies inside the working tree. The New
Agent dialog shows a warning under the in-place choice when it is set (§5.1). The
daemon does not refuse such a create.

### 4.4 Noop backend

This backend has no mounts, as today. It is a development and test backend and
isolates nothing.

## 5. IDE

### 5.1 New Agent dialog

- There is a choice under the repository field: **Work in a new worktree** (the
  default) or **Work directly in this checkout**.
- With the second choice selected:
  - The base-branch field is disabled and shows the checked-out branch from
    `repo.inspect`, or "detached HEAD".
  - The help text reads: "The agent edits this folder on its current branch. Its
    changes are not isolated on a branch of their own."
  - When `hooks_path_in_tree` is set, a warning says: "This repository runs git
    hooks from `<path>` inside the working tree. The agent can change them, and
    they run outside the sandbox the next time you use git here."
  - When the path is a linked worktree or bare repository, the choice is disabled
    with the reason. The daemon refuses these as well (§3.1).
- The chosen mode is remembered with the other dialog defaults.

### 5.2 Workspace presentation

- The tab, explorer header or current-worktree display marks an in-place workspace
  as "in place", showing the repository path and branch.
- The Changes toolbar hides Merge and Create PR. The Changes list shows the
  changes against `HEAD`.
- Destroy becomes **Close workspace…** in the Workspace menu, the tab menu and the
  down-workspace banner. It asks: *Close workspace "<name>"? The agent and its
  sandbox stop. Your files, branches and git history are not touched.* There is no
  Force box and no second "Destroy it anyway?" question.
- Retry is unchanged.

### 5.3 Restore

Tabs restored from `state.json` and `workspace.list` take `kind` from
`WorkspaceInfo`. It is not stored separately.

## 6. Testing

Daemon (in WSL; bwrap tests skip where bwrap is unavailable):

- **Create:** a root works; a linked worktree, a bare repository and a nested path
  are refused; a second in-place create on the same checkout is `Conflict`; a
  worktree workspace alongside it works; `init_if_missing` together with
  `in_place` works; `branch` is recorded, including a detached `HEAD`.
- **Sandbox (bwrap):** from inside, the agent can:
  - edit a file, `git add`, `git commit` (the new commit is in the repository),
    and `git switch -c`.

  From inside, these fail:
  - writes to `.git/config`, `.git/hooks/*` and `.git/info/*`;
  - `mv .git x`, `rm -rf .git`, and replacing `.git`;
  - `.git/config.worktree` (when that extension is enabled).

  The binds create nothing in the repository except the documented directories.
- **Destroy:** the repository is byte-identical afterwards, whatever `force` says.
- **Changes and diff:** they are measured against `HEAD`. Merge and PR refuse with
  `reason = "in_place"`.
- **Restore and restart:** there is no worktree repair; a missing repository gives
  the `Error` sentence; restart works.
- **`repo.inspect`:** `hooks_path_in_tree` is set for an in-tree `core.hooksPath`
  (relative and absolute forms) and `None` otherwise.
- **Protocol:** a version-1 hello is refused; `kind` defaults to `worktree` for an
  old registry.

IDE, with fake-daemon smoke tests and model tests:

- The dialog sends `in_place` and shows the hooks warning.
- The in-place tab hides Merge and PR.
- The Close wording is used, with no Force box, and a single non-forced destroy is
  sent.
- `kind` survives a reconnect and restore.

## 7. Docs

- **Daemon design §4:** workspace kinds, the in-place create, restore, destroy
  and changes; §6.2 the in-place mount rules.
- **Overview:** the protocol list, `in_place` and `hooks_path_in_tree`, and
  protocol version 2.
- **User guide:** a section "Working directly in a checkout", covering what it
  is, when to use it, the Close wording and the residual risks from §4.2.
