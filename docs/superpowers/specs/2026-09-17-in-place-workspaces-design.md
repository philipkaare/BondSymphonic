# In-place workspaces — design

Date: 2026-09-17. Status: approved in chat by the user; amended the same day while planning (`.git/commondir`, `worktrees`, `remotes` and `branches` read-only, `--ignore-submodules=all`, the extra `repo.inspect` fields and refusals, measured bwrap and git behaviour), and amended twice more during implementation (§4.1: `config.worktree` is bound unconditionally, not only when `extensions.worktreeConfig` is on; §4.2: a read-only bind can be detached from outside the sandbox, so the daemon detects and stops the sandbox instead of relying on the bind alone; §4.2 again: the daemon's own worktree and branch cleanup no longer trips that detection and a removed last worktree gets its own sentence), and amended once more in the final fix wave (§4.2: on a Windows drive the read-only binds are not the control -- name aliases get past them -- so the watcher compares the contents of everything under `.git` that git runs as configuration; the hold in `.git/worktrees` is persistent; Create PR pushes without `-u`; and the honest account of what an in-place agent can still reach, refs and objects included). The plan is `docs/superpowers/plans/2026-09-17-in-place-workspaces.md`.

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

`RepoInfo` (the `repo.inspect` result) gains three `#[serde(default)]
Option<String>` fields:

- `hooks_path_in_tree`: the repository's effective `core.hooksPath`, relative to
  the root, when it resolves to a directory inside the working tree (husky does
  this) and not inside `.git`, otherwise `None`. See §4.3.
- `head_branch`: the branch checked out, or `None` when `HEAD` is detached.
  `default_branch` is the remote's default and is not this.
- `in_place_refusal`: why the path cannot be worked in place (a linked worktree, a
  `.git` that is a file), as the sentence `workspace.create` refuses with, or
  `None`. The dialog uses it to disable the choice (§5.1).

`WorkspaceInfo` carries `kind` as well.

`PROTOCOL_VERSION` goes from 1 to 2. An older daemon would ignore `in_place` and
quietly make a worktree, and the hello gate is what stops that. The IDE already
installs its own daemon before every launch (M7).

## 3. Daemon: lifecycle

### 3.1 Create (`in_place: true`)

1. Validate the name as today. Classify the path with `repo::classify`.
   `init_if_missing` works as today, so a folder can be initialised and then used
   in place.
2. Require a **repository root with a `.git` directory**. A linked worktree, a
   root whose `.git` is a file (a separate git directory, a submodule checkout), a
   bare repository and a path inside another repository are refused with
   `InvalidParams` and a sentence saying why. The daemon also refuses `/`, the
   daemon user's `$HOME`, a root inside the daemon's data directory, and a root
   that **contains** the data directory: the read-write bind of such a root would
   bring every other workspace's home and exec socket back over the tmpfs that
   masks them. (Before this design those refusals applied only to
   `init_if_missing`.) Workspace worktrees live under the data directory and are
   linked worktrees, so both rules cover them.
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
- **Never** run `worktree::remove` and never touch `worktree_path`, its files, or
  any branch. The only writes to `.git` are taking back what §4.1 step 5 put
  there: the `commondir` guard if it still holds exactly `.`, and each of
  `worktrees`, `remotes`, `branches` and `config.worktree` that the daemon's
  record says it created and that is still empty. The record is then deleted.
  A test asserts that the
  repository is byte-identical afterwards (tree listing with contents, `.git`
  included, plus `git status --porcelain=v2` plus `git for-each-ref` plus
  `.git/config` contents).

### 3.4 Changes, diff, status, merge, PR

- `workspace.status`, `workspace.changes` and `workspace.diff` compare against
  `HEAD` instead of `merge-base(base_branch, HEAD)`: staged, unstaged and
  untracked changes, the same as `git status`. In a repository with no commits
  they compare against the empty tree. The result types are unchanged.
- Every daemon-side `git status` and `git diff` against an agent-writable tree,
  in either kind of workspace, passes `--ignore-submodules=all`. An agent can
  commit an embedded repository whose own `.git/config` sets `core.fsmonitor`,
  and a status that looks into it runs that command (measured with git 2.43).
  `-c diff.ignoreSubmodules=all` is not enough, because a `.gitmodules` entry with
  `ignore = none` overrides it; the command-line flag is not overridden. This
  also closes the same hole in worktree workspaces, except in the rebase step
  (§4.2).
- `workspace.merge` and `workspace.create_pr` answer `InvalidParams` with
  `data.reason = "in_place"` ("an in-place workspace has nothing to merge; commit
  and push from the checkout").
- Daemon-side git for an in-place workspace pins `GIT_DIR=<root>/.git`,
  `GIT_COMMON_DIR=<root>/.git`, `GIT_WORK_TREE=<root>` and
  `core.hooksPath=<no_hooks>`, like `daemon_git`. `GIT_COMMON_DIR` is what keeps
  a planted `commondir` (§4.1) away from the daemon even if the guard were gone. The
  repository config is the user's own and is read-only to the agent, so the
  `NEUTRALISED_CONFIG` list is not applied, for the same reason as `daemon_git`.

An `InPlaceLayout` lives beside `Layout`, in `workspace/in_place.rs`, and
`layout_for` refuses an in-place workspace with `Internal`, so no worktree-only
path (`ref_dir`, `worktree_gitdir`, `objects_dir`, `config_worktree`) can be
reached for one by accident. Tests cover destroy and restore.

## 4. Daemon: sandbox (`linux_bwrap`)

### 4.1 Mounts for `in_place`

In this order, the later ones on top of the earlier ones (`SandboxSpec` already
separates `rw_binds`, `ro_binds` and `late_ro_binds`):

1. rw: `<root>`.
2. rw: `<root>/.git`, a mount of its own. A mount point cannot be renamed,
   removed or replaced *from inside the sandbox* (`EBUSY`), so the agent cannot
   swap `.git` for a directory of its own making. It can still be replaced from
   *outside* the sandbox; §4.2 covers that.
3. ro, late, in this order: `<root>/.git/config`, `<root>/.git/commondir`,
   `<root>/.git/hooks`, `<root>/.git/info`, `<root>/.git/worktrees`,
   `<root>/.git/remotes`, `<root>/.git/branches`, `<root>/.git/modules` (the
   last only when it is a real directory, not a symlink), and
   `<root>/.git/config.worktree` (**unconditionally** -- amended during
   implementation, see below).
   - `commondir`: git reads it from *any* git directory, not only a linked
     worktree's, and takes config, refs and objects from wherever it points.
     Measured: an agent that writes it, pointing at a directory of its own whose
     `config` sets `core.fsmonitor`, gets that command run by the user's next
     `git status`, even with `GIT_DIR` pinned.
   - `worktrees`: it holds the git directories of this repository's *worktree*
     workspaces, whose `config.worktree` the daemon's own status reads.
   - `remotes` and `branches`: legacy remote definitions that
     `git fetch <name>` and `git push <name>` read, so an agent could redirect a
     remote name the user types.
   - `config.worktree`: this design originally bound it only when
     `extensions.worktreeConfig` was already on. That is not enough, because
     `git sparse-checkout init` turns the extension on by itself and keeps
     whatever an agent had already left in the file, so the daemon does not
     wait for the extension to decide whether the file is protected -- it
     creates and binds it whatever the extension says. If the file is missing,
     the daemon creates it empty first: git reads an empty file as no
     configuration. A file the user already has is left as it is.
4. Before every sandbox start the daemon writes `.git/commondir` containing `.`
   (a newline-terminated dot). That points the common directory at the git
   directory itself, which it is anyway. Measured with git 2.43 and Git for
   Windows 2.52: status, commit, switch, stash, `worktree add` and `remove`, gc
   and fsck all behave as without it. A repository that already has a
   `commondir` with any other content is refused (`InvalidParams`).
5. If `.git/hooks` or `.git/info` is missing, the daemon creates the empty
   directory; git's own `init` makes both, and they stay after Close. If any of
   `.git/worktrees`, `.git/remotes`, `.git/branches` or `.git/config.worktree`
   is missing, the daemon creates it (empty, or an empty directory) and appends
   its name to a record of its own, `<data>/in-place/<id>.created`, outside
   every sandbox. Git creates the first three on demand, and an empty one
   changes nothing; `config.worktree` is created for the same reason (above).
   Close removes exactly the recorded ones, and only while they are still
   empty (§3.3).

There is no private object directory and no `GIT_OBJECT_DIRECTORY` or
`GIT_ALTERNATE_OBJECT_DIRECTORIES`: the agent's objects go into the repository's
own store. The proxy, home, cache, run dir, Claude binary and environment are as
for a worktree workspace; `cwd` is `<root>`.

Measured with bwrap 0.9 in the distro: a `--ro-bind` onto a missing path creates
it on the underlying writable repository (an empty directory, or an empty `0444`
file). That is why steps 3–5 create every read-only target first; bwrap then never
has to, and the binds create nothing beyond those documented entries. Renaming or
removing a mount point (`.git`, the root, any of the files above) fails with
`EBUSY` *from inside the sandbox*. §4.2 covers what that guarantee does and does
not reach.

### 4.2 What this protects, and what it does not

On a checkout inside the distro, this keeps the sandbox boundary for any git the
user runs later, on either side of WSL: the agent cannot plant a hook, a
`core.fsmonitor`, a `core.sshCommand` or a filter driver in the repository's
config, cannot redirect `.git` or its common directory, cannot redefine a legacy
remote, and cannot reach the git directories of the repository's worktree
workspaces. Git commands that write `.git/config` therefore fail inside the
sandbox: `git push -u`, `git remote add`, upstream tracking set up by `git switch
-c x origin/x`, and `git sparse-checkout`. This is accepted.

**On a Windows drive the binds are not the control -- amended during
implementation.** A bind protects one dentry. On DrvFs (9p, `aname=drvfs`)
Windows resolves names that differ only in case, and 8.3 short names, to the
same file, and each alias is a dentry no mount covers: measured in the distro,
`printf … >> .git/CONFIG`, `>> .GIT/config` and `>> GIT~1/config` all reach the
user's `.git/config` through the read-only bind, with the mount, the device and
the inode unchanged, and `echo x > .git/HOOKS/pre-commit` plants a hook. A
Windows `git status` then ran the planted `core.fsmonitor`. The IDE is a Windows
application and the repositories the user browses to are Windows folders, so
this is the ordinary case rather than an edge one. It is answered by content
detection (below), which is the only check that tells the difference: on such a
drive the protection is detect-and-stop, not blocked, and the docs and the New
Agent dialog say so.

**What an in-place agent can reach that no bind covers.** `.git` itself is
writable. An in-place agent can therefore write the repository's refs and
objects: it can move a sibling *worktree* workspace's `refs/heads/bs/<name>/work`
between the user reading its Changes and pressing Merge, rewrite `packed-refs`,
write loose objects, and add `refs/replace` entries, which change what the
daemon's diff and the user's `git log -p` display (the daemon does not set
`GIT_NO_REPLACE_OBJECTS`). This is a residual risk of the design, listed below
and in the user guide.

**A read-only bind is not proof against something that runs outside the
sandbox -- amended during implementation.** The `EBUSY` guarantee of §4.1
holds only inside the agent's own mount namespace. It does not hold against
anything that runs outside the sandbox, in the same repository, while the
workspace is open: a `git config` the user types, `git branch -u`, `git push
-u`, `git remote add`, a `git worktree remove` that empties the last entry
under `worktrees`, or `git sparse-checkout` all replace or remove the very
file or directory a bind was made on -- git's lock-and-rename pattern for
`config` chief among them -- and the kernel detaches that bind inside the
sandbox exactly as a plain rename or remove of a mounted-over path always
has. Nothing run inside the sandbox can prevent that: the entry the bind was
protecting is simply gone from under it, and the path falls through to the
writable `.git` beneath.

The daemon watches for this rather than relying on the bind alone.
`InPlaceLayout::snapshot`, taken right after `prepare` and before the sandbox
starts, records three things: each protected entry's device, inode, type and
link count (a second hard link to `config` is refused at snapshot time and is a
breach when it appears, because a write through the other name reaches the same
file with identity and mount intact); the contents of everything under `.git`
that git runs or reads as configuration, bounded by a path and a byte cap so a
pathological repository cannot make the check unbounded; and the mount points
the entries were bound at. Every 250 ms while the sandbox is up, all three are
rechecked: every entry is re-`lstat`ed, the covered contents are re-read and
compared whole, and, given the sandbox's pid, its `/proc/<pid>/mountinfo` is
re-read to confirm each entry is still actually mounted there -- identity alone
is not enough, because two config writes in a row can hand a fresh file the same
inode number an ext4 filesystem just freed, and on a Windows drive an aliased
write changes neither identity nor mount. Measured at 71-117 ms a poll against
BondSymphonic's own `.git` (14 hooks, 2 worktree registrations) copied onto a
Windows drive -- down from 196 ms before `*.sample` hooks were listed from
their directory entry rather than opened and read, since git never runs a
file with that suffix. The first change any of the three finds is a breach:
the sandbox is torn down, the workspace moves to `Error` with a sentence naming
which entries changed, and a `daemon.log` warning carries that sentence, the
entry names and a unified line diff of the files whose contents are read. The
same sentence and diff go out as a workspace-tagged warn-level `daemon.log`
event, which is what the IDE shows behind the banner's **What changed** (§5.2).
A sandbox that died on its own is not a breach, and a check that could not be
made stops the sandbox with a sentence of its own rather than leaving it
unwatched. Retry (`workspace.restart`) prepares and snapshots again, so a
repaired repository is protected exactly as it was on create.

This closes the gap for everything that happens after the next check runs,
but not for the poll interval itself: a change the check has not yet seen --
made in the up-to-250-ms window before it fires -- can still leave
`.git/config` holding something written from outside the sandbox, and the
daemon only reports it after the fact; the logged diff is what lets the user
tell a setting they meant to make from one they did not, before pressing
Retry. One consequence follows directly and is by design, not a gap to close:
any git command that writes into a protected entry -- not only one an agent
runs, but one the user themselves runs in that repository while an in-place
workspace is open on it -- reads as a breach and stops that workspace's
sandbox, because the daemon cannot tell the two apart.

**The daemon's own worktree cleanup does not trip this -- amended during
implementation.** Two of the daemon's own operations remove or rewrite
exactly what the above protects, against an *in-place* workspace of the same
repository, not the worktree workspace being acted on. Both are now
hardened:

- Removing a worktree deletes `.git/worktrees` itself once the last linked
  worktree is gone. `worktree::remove` (a plain destroy) and the merge/rebase
  scratch-worktree reaper hold the directory open first (`WorktreesHold::take`,
  a `.bs-hold-<id>/locked` entry a locked-entry-aware git prune skips and
  `worktree list` skips too), under the same per-repository lock
  `prepare`/`release` take, and drop the hold once the operation is done. A
  stale hold a killed daemon left behind is taken away by the next one that
  reaches the repository -- only when its `locked` file says what the daemon
  writes there and the entry holds nothing else, so a worktree of the user's
  own that happens to be named that way keeps its lock.
- **Made persistent in the final wave.** The hold above covers only the
  daemon's own removals, and `git worktree prune` or a `git gc` -- which git's
  `--auto` maintenance runs behind an ordinary `git commit` -- removes an empty
  `.git/worktrees` just as well, stopping an agent for no reason the user could
  see. So `prepare` now also creates a hold of its own, named for the workspace
  -- `.bs-inplace-<workspace id>/locked` -- and `release` removes exactly that
  entry at Close, and only while its `locked` file still holds what the daemon
  wrote there. Its name deliberately does not start with `.bs-hold-`, which the
  stale sweep above takes away.
- Deleting a workspace's branch used to run `git branch -D`, which rewrites
  `.git/config` every time to drop a `branch.<name>` section that is usually
  not there. `remove_branch` now runs `git update-ref -d
  refs/heads/bs/<name>/work` and removes the `branch.<name>` section
  separately, only when one exists (a workspace whose Create PR set an
  upstream -- see the gap below). A plain destroy therefore no longer writes
  `.git/config` at all.

**A `.git/worktrees` that is gone anyway says so plainly.** With the persistent
hold, the user's own `git worktree remove`, `prune` and `gc` no longer empty
that directory, so what is left is deleting it by hand. The watcher tells that
apart from a replaced or foreign entry: `ProtectionBreach`
carries `removed`, the subset of `entries` that are gone rather than
replaced, and its `sentence()` answers differently when the breach is exactly
`.git/worktrees`, removed: *"This repository's last worktree was removed,
which also removed a directory the sandbox keeps read-only, so the sandbox
was stopped. Nothing needs checking; press Retry."* -- no diff to review,
because `.git/worktrees` names no program.

**Create PR pushes without `-u` -- changed in the final wave.**
`workspace.create_pr` used to run `git push -u origin <branch>`, which sets
that branch's upstream in `.git/config` and so stopped an in-place sibling of
the same repository. It now runs `git push origin <branch>`: `gh pr create` is
given `--head` explicitly, the object absorption that follows reads
`refs/remotes/origin/<branch>`, which a push updates with or without `-u`, and
an upstream the user wants is theirs to set. The `branch.<name>` section that
`remove_branch` still knows how to remove is now only one a user set by hand.

Residual risks, documented in the user guide and not mitigated further:

- **On a Windows drive the protection catches rather than prevents.** The
  read-only binds do not cover the names DrvFs aliases, so a write through one
  of them lands and is answered up to 250 ms later by the sandbox being stopped
  and the change shown. What that window allows is the user's to review and
  undo.
- **The repository's refs and objects.** `.git` is read-write, so an in-place
  agent can move any branch -- a sibling worktree workspace's included -- rewrite
  `packed-refs`, write loose objects, and add `refs/replace` entries, which
  change what the daemon's diff and the user's `git log -p` show. The daemon does
  not set `GIT_NO_REPLACE_OBJECTS`, and Merge does not pin the commit its Changes
  view was computed from; both are follow-ups.
- **`core.hooksPath` the agent can write** (§4.3). Hooks inside the working tree
  are ordinary files the agent can edit, and the user's git runs them. So is a
  hooks directory inside the read-write part of `.git`, which the dialog does
  not warn about today (§4.3, "Open").
- **Programs the user's own config names that live in the tree.** A
  `filter.*.clean`, a `diff.*.textconv` or a `core.fsmonitor` pointing at a
  script inside the working tree is a script the agent can rewrite, and
  `InPlaceLayout::git()` deliberately applies no neutralisation, so the daemon's
  own status and diff run it on the next Changes refresh. The worktree kind has
  the same exposure inside its own worktree; this predates the design. A
  follow-up would run the read-only queries with the drivers neutralised.
- **`.gitattributes` in the working tree** can select filter or diff drivers, but
  only drivers already defined in the user's own read-only config. This risk
  exists today for merged worktree content too.
- **Sequencer state:** a rebase the agent leaves in progress can carry `exec`
  lines that run when the user types `git rebase --continue`.
- **Scripts in the tree** (`package.json` scripts, `Makefile`, …) run when the
  user runs them. That is inherent in letting an agent edit your checkout.
- **Embedded repositories.** An agent can `git init` a directory in the tree, set
  `core.fsmonitor` (or any other command) in *its* `.git/config`, and commit it
  as a gitlink. The user's next `git status` or `git diff` in the checkout looks
  into it and runs that command. The daemon's own git is protected
  (`--ignore-submodules=all`, §3.4); the user's is not.
- **`gh pr create` is not protected either.** `pr.rs` runs `gh` with its
  working directory set to the user's checkout, and `gh` itself (2.45.0) runs
  `git status --porcelain` there to print its "N uncommitted changes"
  warning — no `--ignore-submodules`, nothing neutralised. So **Create PR** on
  a *worktree* workspace of a repository that also has an in-place workspace
  open can run an embedded repository's config the same way. Confirmed by
  reading the installed `gh` binary's source; not fixed, since the remedy
  (`--repo owner/name` against a daemon-owned directory) needs a real GitHub
  repository to test. Follow-up.
- **History can be destroyed.** `rm -rf .git` fails, and the mount points
  (`.git`, `config`, `commondir`, `hooks`, `info`, `worktrees`, `remotes`,
  `branches`) survive. But `HEAD`, the index, `objects/`, `refs/` and `logs/` are
  deleted, just as the agent can delete any file in the checkout. Keep a remote
  or a backup.
- The agent cannot init or commit in existing submodules (`.git/modules` is
  read-only).
- **Known gap, worktree workspaces only:** `workspace.merge` in rebase mode runs
  `git rebase` in the workspace worktree, and `rebase` has no
  `--ignore-submodules` option. An embedded repository an agent committed there
  can therefore have its config run by the daemon when the user presses Rebase.
  This is a follow-up and is not fixed by this design. It is the only exception
  left: the conflict listing that follows a failed merge or rebase used to be a
  second one (`git diff --diff-filter=U` is worktree-versus-index and looks into
  a submodule), and now reads the index alone with `git ls-files --unmerged -z`.

### 4.3 `core.hooksPath` warning

`repo.inspect` resolves `git config --get core.hooksPath` (relative to the root, `~/`
expanded, `..` taken out) and reports it, relative to the root, in
`hooks_path_in_tree` when it lies inside the working tree but not inside `.git`. The New
Agent dialog shows a warning under the in-place choice when it is set (§5.1), and
the sentence names the path rather than saying where it is. The daemon does not
refuse such a create.

**Open:** a `core.hooksPath` pointing inside the *read-write* part of `.git`
(`.git/my-hooks`, or `.git/hooks/../my-hooks`, which normalises to it) is a
directory the agent can fill just as well, and is not reported today, because
the resolution treats all of `.git` as unwritable. Only the entries §4.1 binds
are. The residual-risk list and the user guide say so; the dialog needs no
change when the field is corrected, which is why its sentence no longer claims
the working tree.

### 4.4 Noop backend

This backend has no mounts, as today. It is a development and test backend and
isolates nothing.

## 5. IDE

### 5.1 New Agent dialog

- There is a choice under the repository field: **Work in a new worktree** (the
  default) or **Work directly in this checkout**.
- With the second choice selected:
  - The base-branch field is disabled and shows the checked-out branch from
    `repo.inspect` (`head_branch`), or "detached HEAD". No base branch is sent.
  - The help text reads: "The agent edits this folder on its current branch. Its
    changes are not isolated on a branch of their own."
  - When `hooks_path_in_tree` is set, a warning says: "This repository runs git
    hooks from `<path>`, which the agent can change. They run outside the
    sandbox the next time you use git here." The sentence names the path rather
    than saying where it is, so that it holds for every path the field can
    report (§4.3).
  - For a repository on a Windows drive -- `C:\…` as the dialog holds it, or
    `/mnt/<letter>/…` as the daemon knows it -- a note says how the protection
    works there: "This folder is on a Windows drive. There the sandbox cannot
    make .git read-only, so a change to it is caught within a moment instead:
    the sandbox stops and shows you what changed." (§4.2.) The check is on the
    path's spelling and asks the daemon nothing.
  - When `in_place_refusal` is set (a linked worktree, a `.git` that is a file),
    the choice is disabled with that sentence as its tooltip, and the dialog
    falls back to a worktree. A bare repository is already an `inspect` error.
    The daemon refuses all of these as well (§3.1).
- The chosen mode is remembered in `state.json` (`new_agent_in_place`), beside the
  recent repositories. It is written only when a dialog is accepted while the
  choice was available, so a forced fallback does not overwrite the preference.

### 5.2 Workspace presentation

- The tab, explorer header or current-worktree display marks an in-place workspace
  as "in place", showing the repository path and branch.
- The Changes toolbar hides Merge, Rebase, Squash, Create PR and Discard. All five
  act on a branch of the workspace's own, and there is none. The Workspace menu
  shows the same actions and hides them too. The Changes list shows the changes
  against `HEAD`.
- The Close group dialog offers an in-place workspace only **Keep (move to
  Unsorted)** and **Close (files are kept)**, and leaves it out of the discard
  confirmation. That row sends the same non-forced `workspace.destroy` the tab
  menu's **Close workspace…** does, not the forced one a discard is; the run
  behind the dialog completes on either answer to it.
- The banner of a workspace stopped because its protected git files changed
  offers **What changed**, which opens the line diff the daemon sent with the
  reason (§4.2). The IDE keeps that text against the workspace id, so it
  survives the tab being built later and a reconnect; an IDE started after the
  event has only `daemon.log`, which carries the same diff.
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
  - writes to `.git/config`, `.git/hooks/*`, `.git/info/*`, `.git/commondir`,
    `.git/worktrees/*`, `.git/remotes/*` and `.git/branches/*`, and
    `git config`;
  - `mv .git x`, replacing `.git`, and moving the root;
  - `rm -rf .git` exits non-zero and leaves `.git`, `config` and `hooks` in
    place (the rest of `.git` is lost; §4.2);
  - `.git/config.worktree` (bound whether or not that extension is enabled --
    amended during implementation, §4.1).

  The binds create nothing in the repository except the documented entries
  (§4.1 steps 3–5).
- **Preparation:** `prepare` creates exactly those entries and records only the
  on-demand directories it made. A foreign `commondir` is refused. Release
  removes only recorded, still-empty directories and ignores anything else in the
  record. The pinned git ignores a planted `commondir`.
- **Protection (amended during implementation, §4.2):** a snapshot taken after
  `prepare` catches a protected entry replaced, removed or unmounted from
  *outside* the sandbox -- exercised host-side rather than from inside it,
  since that is the side the guarantee does not reach -- and answers with the
  changed entries' names; an entry edited in place (same device and inode) is
  not a breach. The daemon stops the sandbox, sets the workspace `Error` with
  the breach sentence, and logs a diff of the protected files' contents.
  Retry (`workspace.restart`) prepares and snapshots again and the workspace
  comes back `Ready`. A `mountinfo` that cannot be read reads as a breach
  rather than as nothing changed (fails closed).
- **Destroy:** the repository is byte-identical afterwards, whatever `force` says.
- **Changes and diff:** they are measured against `HEAD`, and against the empty
  tree when there are no commits. Merge and PR refuse with
  `reason = "in_place"`.
- **Embedded repositories:** status and changes, in both kinds of workspace, do
  not run an embedded repository's config.
- **Refusals:** a root that contains the data directory is refused.
- **Restore and restart:** there is no worktree repair; a missing repository gives
  the `Error` sentence; restart works.
- **`repo.inspect`:** `hooks_path_in_tree` is set for an in-tree `core.hooksPath`
  (relative and absolute forms) and `None` otherwise (outside, inside `.git`,
  unset). `head_branch` names the branch and is `None` when detached;
  `in_place_refusal` is set for a linked worktree.
- **Protocol:** a version-1 hello, and one with no version at all, is refused;
  `kind` defaults to `worktree` for an old registry.

IDE, with fake-daemon smoke tests, widget tests and model tests:

- The dialog sends `in_place` (and no base branch), shows the hooks warning, shows
  the checked-out branch or "detached HEAD", and disables the choice with the
  reason for a linked worktree.
- The in-place tab hides Merge, Rebase, Squash, Create PR and Discard.
- The Close wording is used, with no Force box, and a single non-forced destroy is
  sent.
- `kind` survives a reconnect and restore.

## 7. Docs

- **Daemon design §4:** workspace kinds, the in-place create, restore, destroy
  and changes; §6.2 the in-place mount rules.
- **Overview:** the protocol list, `in_place`, `kind`, `head_branch`,
  `in_place_refusal` and `hooks_path_in_tree`, and protocol version 2.
- **User guide:** a section "Working directly in a checkout", covering what it
  is, when to use it, the Close wording and the residual risks from §4.2.
