# BondSymphonic Daemon — Design

**Date:** 2026-09-08
**Status:** Approved for planning
**Parent:** `2026-09-08-bondsymphonic-overview-design.md`

## 1. Role

`bondsymphonic-daemon` is a headless Rust binary that runs inside the WSL2 distro
(later: natively on Linux and macOS). It owns every operation with side effects on
a repository or a sandbox: worktrees, git, sandbox lifecycle, agent processes,
PTYs, web-app runs, network policy, and file access for the editor. The IDE is a
pure client of it over the protocol defined in the overview spec.

## 2. Crate layout

```
crates/daemon/
  src/
    main.rs            CLI args, logging, start server, print port+token line
    server/
      mod.rs           TCP listener, per-connection NDJSON codec, auth
      dispatch.rs      Request -> handler routing, error mapping
      broadcast.rs     Event fan-out to all connections
    workspace/
      mod.rs           Workspace struct, WorkspaceInfo mapping
      registry.rs      Persistent registry (~/.bondsymphonic/workspaces.json)
      lifecycle.rs     create / destroy / status transitions
    git/
      mod.rs           Thin async wrapper around the git CLI
      worktree.rs      worktree add/remove, ref protection layout
      merge.rs         merge / rebase / squash, conflict parsing
      pr.rs            push + gh pr create
    sandbox/
      mod.rs           SandboxBackend trait, SandboxSpec, SandboxHandle
      linux_bwrap.rs   bubblewrap implementation
      noop.rs          runs processes unsandboxed (tests, --no-sandbox flag)
    net/
      proxy.rs         HTTP CONNECT proxy over Unix socket, allowlist
      allowlist.rs     Host pattern matching (exact, *.suffix)
      bridge.rs        Host TCP port <-> Unix socket <-> sandbox localhost port
      forwarder.rs     Tiny in-sandbox forwarder (same binary, subcommand)
    agent/
      mod.rs           AgentAdapter trait, AgentHandle, transcript store
      claude.rs        Claude Code stream-json adapter
      terminal.rs      PTY-based adapter for any CLI
    pty.rs             PTY sessions inside a sandbox
    run/
      config.rs        bondsymphonic.toml parsing
      detect.rs        auto-detection (package.json, compose, Cargo, manage.py)
      manager.rs       run processes, port allocation, state
    fs.rs              list/read/write with path containment, watcher
    prereqs.rs         check_prereqs implementation
  tests/
    workspace_integration.rs
    sandbox_integration.rs
    network_integration.rs
    fixtures/claude-stream/*.ndjson
```

Async runtime: tokio. Each workspace owns a `JoinSet` of tasks; a workspace's
tasks are cancelled together on destroy. Panics in a workspace task are caught by
the JoinSet and reported as `workspace.state {error}`.

## 3. Startup and CLI

```
bondsymphonic-daemon [--data-dir DIR] [--no-sandbox] [--log-level L]
bondsymphonic-daemon forward --socket PATH --port N     # in-sandbox forwarder
bondsymphonic-daemon proxy-shim --socket PATH           # (see 7.2)
```

On start: take the data directory's instance lock (below), load registry, bind
`127.0.0.1:0`, generate a 32-byte random token, print `{"port":N,"token":"hex"}`
as a single stdout line, then serve. Logs go to stderr and
`~/.bondsymphonic/daemon.log`. The daemon exits when it receives
`system.shutdown` or when stdin closes (the IDE holds stdin open; if the IDE dies,
the daemon stops all sandboxes and exits).

**A connection costs nothing until it says hello.** Every line is read through a
bounded reader: 64 KiB until `hello` is through, 8 MiB after. A line that hits
the cap without a newline disconnects with no reply — end of input and cap
exhaustion are told apart by whether the limit was consumed — and each pre-hello
line also gets a 10 s deadline of its own, so a peer that connects and says
nothing is gone in ten seconds rather than held for a minute. A peer may spend
at most three lines before it is authenticated; past that the reader simply
breaks. The reader waits for the request loop to acknowledge each pre-hello line
before taking the next, so a `hello` followed in the same burst by a large legal
request does not read that request under the small cap.

**A panicking handler still answers.** Each request runs in a task of its own,
and a panic there used to end the task with the caller waiting for ever and
nothing logged. The handler now runs in a nested task whose `JoinError` becomes
a response carrying `Internal`, plus a `tracing::error!`; the connection
survives and its next request is served.

**One daemon per data directory.** Before anything in the data directory is read
or written, the daemon takes an advisory exclusive lock on
`<data_dir>/daemon.lock` and holds it for its whole life. A daemon that finds the
lock held prints `another bondsymphonic-daemon owns <data_dir>` on stderr and
exits with status **2** — its own code, because the IDE's launcher restarts a
daemon that exits and this is the one exit restarting cannot fix. Two daemons on
one data directory would each rewrite the other's `workspaces.json` and
`agents.json`, restore the other's workspaces into sandboxes of their own, and
race each other's merges and destroys over the same object stores.

The lock is on an open file (`flock(LOCK_EX|LOCK_NB)` on Unix, a zero share mode
on Windows), so the operating system releases it when the process ends however it
ends. Nothing has to be cleaned up by hand and a `daemon.lock` left on disk means
nothing on its own.

On Windows the lock is a zero-share-mode open, so `ERROR_SHARING_VIOLATION` is
what *any* other handle on the file produces — an antivirus scanner, an indexer
or a backup agent reading it for a moment looked exactly like a rival daemon,
and the daemon exited 2, which the launcher treats as final. The open is
retried 20 × 50 ms before a violation is taken to mean the directory is busy; a
persistent holder is still exit 2.

Data directory default: `~/.bondsymphonic/` containing `workspaces.json`,
`agents.json`, `daemon.lock`,
`worktrees/<ws_id>/`, `homes/<ws_id>/`, `caches/<ws_id>/`, `transcripts/<agent_id>.ndjson`,
`daemon.log`.

A daemon killed part-way through writing one of the state files leaves its
temporary — `.workspaces.json.<pid>.<n>.tmp` (4) — behind. These are
**deliberately not swept**: nothing reads them, a name is never reused in a way
that matters because the writer truncates, and one file per crash-mid-write is
rare enough that a directory walk on every start would cost more than it saves.
Anything matching `.<name>.*.tmp` in the data directory is a leftover and can be
deleted by hand at any time.

## 4. Workspaces

A workspace is the unit of isolation: one worktree, one branch, one sandbox, any
number of agents/PTYs/runs inside it.

```rust
struct Workspace {
    id: WorkspaceId,           // "ws_" + 8 hex
    name: String,              // user-facing, unique per repo
    repo_path: PathBuf,        // main repo (may be under /mnt/c)
    base_branch: String,
    branch: String,            // "bs/<name>/work"  (see 5.2)
    worktree_path: PathBuf,    // ~/.bondsymphonic/worktrees/<id>
    created_at: DateTime,
    allowlist: Vec<HostPattern>,
    state: WorkspaceState,     // Creating | Ready | SandboxDown | Error(String) | Destroying
    agents: Vec<AgentId>,             // oldest first, ended agents included
    agent_records: Vec<AgentSummary>, // the same agents, in the same order
    runs: Vec<RunId>,
}
```

`AgentSummary` is `{id, adapter, state, session_id, command, model,
permission_mode}`: the id alone does not let a client that restarted rebuild the
tab, since it cannot tell a Claude agent whose transcript is still being served
from a plain terminal. The three option fields are the non-secret half of what
the agent was started with, so a client can offer "start another one like this";
there is deliberately no field the user's API key could travel in, which is what
stops a call site leaking it by forgetting to clear it. An unset option is left
out of the wire form rather than written as `null`.

The two lists are the same agents in the same order, and they are two lists
rather than one changed list so that a daemon and a client of different vintages
still understand each other. An older client reads `agents` off a newer daemon
exactly as it always did and ignores the key beside it; a newer client reading an
older daemon finds `agent_records` defaulted to empty, which it must read as
"nothing is known about these agents" rather than as "there are none".

**A workspace name must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`.** The rule
lives in `bondsymphonic_proto::workspace_name::validate`, which both the IDE and
the daemon call, and it is deliberately narrower than `git check-ref-format`:
the name becomes the branch `bs/<name>/work`, the loose-ref directory
`refs/heads/bs/<name>/` and a directory name on the daemon's side, so everything
git merely discourages — a leading dot, a trailing `.lock`, `@{`, a `.`
anywhere — is refused rather than reasoned about. `workspace.create` checks it
before it runs any git, and returns `InvalidParams` with the reason as its
message: one of `name is empty`, `name is too long (64 max)` or `use letters,
digits, - or _`, each ready to put next to the field with no wording of the
caller's own. Before this, `feat:x` passed both sides' home-grown checks and
then failed inside `git worktree add`, after the client had been told the
workspace was being created.

**Create** (`workspace.create`):
1. Validate repo (`git rev-parse --git-common-dir`), base branch exists. With
   `init_if_missing`, a path that is not a repository is initialised here
   instead of failing (see below).
2. Compute branch `bs/<name>/work`; fail with `Conflict` if it exists.
3. Pre-create the writable ref directories (5.2).
4. `git worktree add -b bs/<name>/work <worktree_path> <base_branch>`, then
   `git worktree lock --reason "BondSymphonic manages this worktree from WSL;
   do not prune or remove it by hand"`. A git running on Windows cannot see a
   worktree under the WSL home, so its `git worktree prune` would delete every
   unlocked workspace registration; git skips a locked one when it prunes,
   whichever side runs the prune. A failed lock unwinds the create like any
   other post-add failure.
5. Create `homes/<id>` seeded with Claude credentials (8.3) and `caches/<id>`.
6. Build the `SandboxSpec` and start the sandbox supervisor (6).
7. Persist to registry, emit `workspace.state`.

`create` holds the **per-repository lock** — the one `workspace.destroy` and
`workspace.merge` take — from the repository classification through the registry
insert and `git worktree add`. The branch, its loose-ref directory and its
reflog are named after the workspace *name*, so two creates of one name at once
would otherwise both pass the registry check, both reach `worktree add -b`, and
have the loser's cleanup delete the winner's branch mid-checkout. The loser now
sees `Conflict`. No cleanup after a failed creation may delete the branch unless
that creation is the one that made it.

**Starting from a folder that is not a repository.** `repo.inspect` answers for
such a path rather than failing: `RepoInfo { is_repo: false, exists, branches:
[], default_branch: "main" }`, where `exists` says whether the directory is
there. The New Agent dialog asks about a folder before anything has been created
in it, and "not a repository yet" is an ordinary starting point there. Two path
shapes stay `InvalidParams`, because neither can become a repository and
answering "it will be created" to them would have the daemon build a directory
tree nobody named: a path that exists and is not a directory, and one whose
parent directory is itself missing. A path that cannot be read at all — a
permission error rather than "nothing there" — is an `IoError`, not `exists:
false`.

**"Is a repository" means *this* directory.** `git rev-parse` searches upwards,
so the naive question is really about the nearest enclosing repository. One
classifier answers "what is this path": `git::repo::classify` returns `RepoKind`
— `NotARepo`, `Root`, `InsideEnclosing { root }`, `Bare` or `Worktree` — and
`repo.inspect`, `init_repo` and `workspace.create` all go through it, so the New
Agent dialog's offer to initialise a folder and the daemon's willingness to do
it cannot disagree. It asks about the path itself, by comparing `rev-parse
--show-toplevel` with it: a plain folder inside a repository answers `is_repo:
false` rather than borrowing its parent's branches, dirty state and remotes, and
`workspace.create` initialises such a folder as a repository of its own instead
of making it a worktree of a repository the user did not pick. Without
`init_if_missing`, that folder is `InvalidParams` naming the situation. A **bare** repository is
refused by name from both calls — `InvalidParams`, "… is a bare repository;
BondSymphonic needs a checkout (clone it first)" — rather than answered either
way: reported as a folder it would be initialised *inside*, and reported as a
repository it would produce a workspace whose worktree cannot be checked out.

`is_repo` defaults to **true** when it is absent from the wire, which is the only
value that keeps a newer IDE honest against an older daemon: a daemon without
this behaviour failed the call outright for a non-repository, so every answer it
ever sent was about a repository. `exists` defaults to false.

`workspace.create.init_if_missing` (default false) is what turns the answer into
an action: the directory is created if needed, `git init -b main`, then
`git commit --allow-empty -m "Initial commit"`. The empty commit is not
decoration — a repository with no commits has no branch for `git worktree add` to
branch from. An identity is supplied (`BondSymphonic <bondsymphonic@localhost>`)
only where `git config user.email` finds none, so a configured identity keeps the
commit as its own, and `commit.gpgsign` is off for this one commit: it is made
while a dialog waits, and a signing program that wants a passphrase would hang it
until the git timeout. The whole sequence runs with `core.hooksPath` pinned at
the daemon's empty directory (5.4), because `git init` copies `init.templateDir`
— hooks included — into the new repository and the commit would run them. A path
that is already a repository is left exactly as it is; the flag is off by default
so that a client which does not know about it cannot initialise anything, its
user never having been shown that a folder was about to become a repository.

**Only "not a git repository" may lead to a write.** `workspace.create` acts on
the flag when the folder is absent, when git says in as many words that the path
is not a repository (exit 128 *and* that wording), or when git answered about an
enclosing repository. Every other failure — a timeout, a git that cannot be
spawned, a `safe.directory` ownership refusal, an unreadable gitfile — goes back
to the client untouched, because each of those happens on a repository that is
really there, and initialising over one would put an empty commit into somebody's
work on the strength of a transient failure. The 30-second `repo.inspect` timeout
that opened this pass is exactly such a failure.

**Targets that are refused outright**, whatever the flag says: a filesystem root,
the home directory of the user the daemon runs as, the daemon's own data
directory (or anything under it, which is every worktree, sandbox home, object
store and the registry), and a registered workspace worktree. A directory *inside*
the home is allowed, and so is a folder that already has files in it — "I have
some code, make it a project" is the ordinary case, and the empty commit adds
nothing to the index, so those files stay untracked.

**Destroy**: stop agents, runs, PTYs; tear down sandbox; `git worktree remove
--force`; delete the branch (`git update-ref -d refs/heads/bs/<name>/work`,
then `git config --local --remove-section branch.bs/<name>/work` only if that
section exists); delete `homes/`, `caches/`, transcripts; remove from
registry. Removing the branch this way, rather than with `git branch -D`
(which rewrites `.git/config` every time to drop a section that is usually not
there), is what keeps a plain destroy from touching `.git/config` at all when
there is nothing to remove from it — see §6.2 for why that matters next to an
in-place workspace of the same repository. The ref deletion refuses a branch
that is checked out in any worktree of the repository, as `git branch -D` did
and `git update-ref -d` does not: an in-place agent can `git switch` the user's
checkout onto a workspace's branch. The removal itself refuses any
`worktree_path` that is not `<data>/worktrees/<id>`, `force` included; the
daemon creates worktrees nowhere else, and an entry that names somewhere else
is a registry an older build rewrote without its `kind` — an in-place
workspace's own checkout, which a forced destroy would otherwise delete. The
registry's own `version` cannot carry that defence: `migrate` loads a file whose
version it does not know rather than refusing it, so raising it would not make
an older build fail closed, and the path check is what protects this one. An
older build still has neither, which the user guide says in as many words. With `force=false`, refuse if the worktree has uncommitted
changes or unmerged commits and return `Conflict` with details. A worktree whose
registration the repository has lost track of counts as dirty too: nothing can
be read from it to say otherwise, so the refusal errs toward asking rather than
toward destroying something that might have held work.

**Restart** (`workspace.restart`) is refused while the workspace is `Creating`
or `Destroying`, naming which. Otherwise it stops runs, agents (each announced
`Exited`; records and transcripts kept) and PTYs, removes the sandbox handle
from the live map *before* shutting it down (so its death watcher, which only
acts on a handle that is still its own, stays quiet), stops the proxy, and then
brings the workspace up exactly like a restore, repairing a pruned worktree the
same way `restore` does. It answers `WorkspaceInfo` (`Ready`) on success, or
leaves the workspace `Error(reason)` and returns an `RpcError` carrying the same
sentence. It is allowed from `Ready` (a plain sandbox restart), `SandboxDown`
and `Error`. Restart repairs a sandbox at runtime; the client restarts or
resumes the workspace's agents itself once it answers, since `workspace.restart`
stops them like any other teardown.

**Restore, restart and destroy of one workspace are serialised by a
per-workspace gate** — a mutex holding a count of the restarts and destroys
that have run, taken before the per-repository lock everywhere. Without it the
three interleave: a destroy landing while a restart is starting the sandbox
would tear down before the new handle exists and the restart would then
register a live sandbox for a workspace that is gone; a restart landing while
the startup restore is bringing the same workspace up would start a second
sandbox and drop the first one's handle with its processes still running. The
startup restore works from a list of workspaces and gate counts taken before
the server accepts its first connection, and skips a workspace whose count has
since changed — a restart or a destroy already reached it.

**Removing a worktree is a fixed sequence, not a reading of git's prose.**
`worktree::remove` runs `worktree unlock` (whose failure is not news — every
workspace worktree is locked by `create`, above, and a user may lock one too,
and a locked worktree makes `worktree remove --force`
refuse outright *and* makes `prune` skip the registration), then `worktree
remove --force`, then a `remove_dir_all` of whatever is left with a retry for
Windows sharing violations, then `worktree prune`, then the branch step. Each
of the last three runs whatever the one before it did, because returning early
on a transient directory lock would leave the registration behind — and a
registration with no directory keeps the branch checked out and makes every
later `worktree add` refuse, a state a person has to repair by hand. Every
failure is logged; the first is returned whole, with the others appended to its
message, since `workspace.destroy` turns it into `WorkspaceState::Error` and
then stops. Deciding which failures were survivable by matching git's wording is
what this used to do, and it hard-failed on every phrasing nobody had thought
of.

**Registry** is rewritten atomically after every change, and so is `agents.json`
(8.5): the bytes go to a sibling temporary whose name is unique per call
(`.<file>.<pid>.<n>.tmp`), are flushed to the device with `sync_all`, and only
then replace the file with a rename, with a best-effort fsync of the directory
afterwards on Unix. The unique name matters as much as the rename: one fixed
`<file>.tmp` is shared by every writer and by every earlier run of the daemon, so
two saves at once can rename each other's half-written file into place, and one
leftover at that name wedges the writer for good.
On startup, each registered workspace is brought up by `lifecycle::restore`,
from a snapshot of the registry and the per-workspace gates (see the Restart
paragraph above) taken before the server accepts its first connection, so no
client request can race it:
- A workspace persisted as `Creating` or `Destroying` is not brought up; it
  becomes `Error` saying that operation was interrupted when the daemon
  stopped and to remove the workspace (again, if it was being removed) to
  clean up.
- Otherwise the worktree directory must exist, and the repository must still
  list the worktree. An intact registration is locked if it is not already. A
  registration that has `HEAD`, `commondir` and `gitdir` but no `index` — a
  rebuild cut short — gets its index rebuilt from `HEAD` alone. A registration
  with some but not all of those three files — an even earlier cut-short — is
  removed and rebuilt from scratch, which happens only when the directory's
  `.git` still points at exactly that registration and the branch exists and
  is checked out nowhere else. Uncommitted work survives as unstaged changes;
  anything staged does not. A repair either way is announced with a
  `daemon.log` Warn on the workspace, starting "Re-registered this workspace's
  worktree".
- The sandbox is then started.

Any failure leaves the workspace in `Error(<a sentence saying what is wrong and
what to do>)`, never a bare `SandboxDown`, and nothing is deleted, so the user
can decide. `SandboxDown` is reserved for a sandbox that dies while running.

### 4.x Workspace kinds

Two paths through `workspace.create` and, from there, the rest of the
lifecycle, share one `Workspace` record and one `WorkspaceState`. `kind`
(`WorkspaceKind::Worktree` or `::InPlace`) decides which:

| Field | `worktree` | `in_place` |
|---|---|---|
| `worktree_path` | `~/.bondsymphonic/worktrees/<id>`, a linked worktree of the repository | the repository root itself — the folder the agent edits |
| `branch` | `bs/<name>/work`, created for the workspace | the branch checked out at creation, or `""` when `HEAD` is detached; display only — never switched, created or deleted |
| `base_branch` | the branch it was created from | the same string as `branch`; nothing ever merges into it |

A registry written by a daemon that predates `kind` has none, and reads as
`worktree`.

**Create** (`in_place: true`). The daemon requires a repository root with a
real `.git` directory: a linked worktree, a root whose `.git` is a file (a
separate git directory, or a submodule checkout), a bare repository, and a
path inside another repository are all refused with `InvalidParams` naming
why. The daemon also refuses `/`, its own `$HOME`, a root inside its data
directory, and — the one rule `init_if_missing` did not already cover — a root
that *contains* the data directory, because the read-write bind of such a root
would bring every other workspace's home and exec socket back over the tmpfs
that masks them (§6.2). A second in-place create on a checkout that already
has one is `Conflict`, `"this checkout already has an in-place workspace:
<name>"`, compared by canonical path so a different route to the same checkout
is still caught; a worktree workspace on the same repository is unaffected.
Before anything is registered, `InPlaceLayout::check_preparable` runs the same
checks `prepare` (§6.2) would make, without writing: a foreign `commondir`
refuses the create outright, rather than registering a workspace that could
only ever come up `Error`. There is no branch, no `git worktree add`, no lock
and no private object directory: `branch` is simply read with `git
symbolic-ref --short -q HEAD`.

**Restore and `workspace.restart`.** `ensure_registered` — the worktree
repair and lock a `worktree` workspace goes through — does not apply to an
in-place one; there is no worktree to repair. What is checked instead is that
`worktree_path` still exists and still has a `.git` directory; if not, the
workspace goes to `Error("The repository <path> is missing or is no longer a
git repository. Close the workspace, or restore the folder and press
Retry.")`. The per-workspace gate and the rest of restart and restore are
shared with the worktree kind. (A running in-place sandbox can also land in
`Error` on its own, with a different sentence, if its protected git entries
are replaced from outside it — see §6.2.)

**Destroy ("Close").** `force` is ignored and there is no dirty or unmerged
check: nothing of the user's is ever a reason to refuse, because nothing of
the user's is deleted. Agents, runs, PTYs, the proxy and the sandbox are
stopped and the daemon's own `homes/<id>`, `caches/<id>` and `run/<id>`
removed, exactly as for a worktree workspace. The only writes into the
checkout are `InPlaceLayout::release` taking back what `prepare` put there
(§6.2): the `commondir` guard, if it still holds exactly what the daemon
wrote, and each of the on-demand directories the daemon's own record
(`<data>/in-place/<id>.created`) says it created, if it is still empty.
`worktree::remove` is never called, and `worktree_path`, its files and its
branches are never touched.

**`layout_for` refuses an in-place workspace** with `Internal`, before it
computes anything, so no worktree-only path — `ref_dir`, `worktree_gitdir`, a
private object directory, `config_worktree` — can be reached for one by a
caller that forgot to branch on `kind`.

## 5. Git

All git access goes through the `git` CLI via `tokio::process::Command`, with
`GIT_TERMINAL_PROMPT=0`, structured stderr capture, and a 60-second timeout.
Every failure maps to `GitError {command, exit_code, stderr}`.

### 5.1 Why the CLI and not libgit2
Worktrees, rebase, and `gh` interplay are far better covered by the CLI; it
avoids a C dependency; and the sandboxed agent already needs git installed.

### 5.2 Ref protection layout

The sandbox must let the agent commit on its own branch without being able to
move the base branch or any other workspace's branch. Git updates refs by
creating `<ref>.lock` in the ref's directory and renaming, so protection has to be
per directory, not per file. Hence every workspace branch lives in its own
directory: `refs/heads/bs/<name>/work`.

Inside the sandbox, the main repo's `.git` is mounted read-only except these
paths, which are bind-mounted read-write:

| Path under main `.git` | Why writable |
|---|---|
| `worktrees/<ws_id>/` | HEAD, index, ORIG_HEAD, logs for this worktree (git names it after the worktree directory) |
| `refs/heads/bs/<name>/` | the workspace branch and its lock file |
| `logs/refs/heads/bs/<name>/` | reflog for the branch |

New objects are kept out of the shared store: the sandbox environment sets
`GIT_OBJECT_DIRECTORY=<worktree_path>/../objects-<id>` and
`GIT_ALTERNATE_OBJECT_DIRECTORIES=<repo>/.git/objects`, so the agent's commits are
written to a private object directory while it can still read every existing
object. `objects/` in the main repo therefore stays read-only.

Consequences the daemon handles:
- Every daemon-side git command touching the main repo (changes, diff, merge,
  rebase, squash, push) runs outside the sandbox with the main store as primary
  (`GIT_OBJECT_DIRECTORY` unset) and `GIT_ALTERNATE_OBJECT_DIRECTORIES=<private
  objects>`, so it can read workspace commits and any commits it creates land in
  the shared store. That is not enough on its own: a merge commit lands in the
  shared store, but the workspace commits under it are still only in the
  workspace's private object directory, which is deleted with the workspace. So
  after a successful merge, rebase, squash or push the daemon copies the range
  out — see "Absorbing the workspace's objects" below.

**Absorbing the workspace's objects.** `git repack -a -d` cannot be used for
this, and the reason is structural rather than incidental. Every live
workspace's `refs/heads/bs/<name>/work` lives in the *base repository's* ref
store while its objects live in that workspace's private object directory. A
`repack -a` packs everything reachable from every ref and then deletes the loose
objects it replaced; as soon as a second workspace exists, the refs it has to
reach include commits it cannot read, and it fails — after the delete. The
daemon therefore packs one range and nothing else:

```
git pack-objects --revs --delta-base-offset -q <repo>/.git/objects/pack/pack
  stdin: <after>
^<before>

```

For a merge, `<after>` is the base branch after the operation and `<before>`
the same ref before it, so the range is exactly the commits the merge added. A
push absorbs a different range: `<ws.branch> ^<ws.base_branch>`, everything the
workspace's branch adds to its base. The reason is `refs/remotes/origin/<branch>`,
which a push leaves behind with or without `-u` — unlike the local workspace branch, that
remote-tracking ref is *not* deleted when the workspace is destroyed, so it would
be left pointing into an object directory that no longer exists. An empty range is
a no-op. The pack is written straight into `objects/pack`, which is where `git
repack` puts its own.

The copy then proves itself, through a plain `git` carrying **no**
`GIT_ALTERNATE_OBJECT_DIRECTORIES` — the user's own view of the repository:

```
git cat-file -e <after>^{commit}
git rev-list --objects <after> ^<before>
```

`rev-list --objects` reads every commit and every tree in the range, which is
what shows the pack landed and was indexed. A failure is retried once. If it
still fails, the RPC fails with `ErrorCode::Internal`, `data.reason =
"objects_stranded"` and `data.merged` (or `data.pushed`) `true`, and a message
saying the work landed, that its objects could not be copied out, and that the
workspace must not be destroyed. It is never a warning: silently succeeding here
would hand the user a base branch that stops being readable when they clean up.
- `git fetch`/`git pull` inside the sandbox cannot update `refs/remotes/*` (read-
  only). This is intended: fetches are a daemon operation (`repo.inspect` refreshes
  remotes on request).
- `git gc` / `pack-refs` inside the sandbox fail harmlessly.

**Creating and removing a worktree runs no repository hooks.** `git worktree add`
fires `post-checkout` and, for the branch it creates, `reference-transaction`;
the `git update-ref -d` on the way out fires `reference-transaction` again. Neither
call is the user typing a git command — they happen when the IDE opens or closes
a workspace — so both go through the same pinned `Git` the rest of the daemon
side uses, with `core.hooksPath` set to an empty daemon-owned directory (5.4).
Opening a workspace must not execute code out of the repository being opened. The
one exception stays the `pre-push` of **Create PR**, for the reason 5.4 gives.

A `--no-git-protect` daemon flag mounts `.git` read-write for troubleshooting;
`check_prereqs` reports whether protection is active.

### 5.3 Changes and diff
- `workspace.changes`: `git diff --numstat --name-status <merge-base>...HEAD` plus
  uncommitted changes (`git status --porcelain=v2`), merged into one list with
  status `added|modified|deleted|renamed|untracked`.
- `workspace.diff {path}`: returns base text (`git show <merge-base>:<path>`, empty
  if absent) and working-tree text. The IDE computes and renders the diff.
- Every daemon-side git that can print a path runs with `-c core.quotePath=false`.
  Git's default is to render each path byte above 0x7f as a C-style octal escape
  and wrap the name in double quotes, so `håndbog.md` would reach the IDE as
  `"h\303\245ndbog.md"` — a file the IDE cannot open and a name the user does not
  recognise. It is in the *default* `Git` rather than at any call site or
  constructor, so a command added later cannot be forgotten and a bare
  `Git::new()` carries it too. It applies to the conflict list of 5.4
  (`git ls-files --unmerged -z --full-name`) as much as to the lists here.
- An in-place workspace measures `workspace.status`, `workspace.changes` and
  `workspace.diff` against `HEAD` — or the empty tree,
  `4b825dc642cb6eb9a060e54bf8d69288fbee4904`, in a repository with no commits
  yet — rather than `merge-base(base_branch, HEAD)`, through
  `InPlaceLayout::git()` (`GIT_DIR`, `GIT_COMMON_DIR` and `GIT_WORK_TREE` all
  pinned at the checkout). The result types are unchanged.
- **Every daemon-side `git status` and `git diff` against a tree an agent can
  write — for both kinds of workspace — passes `--ignore-submodules=all`.** An
  agent can `git init` a directory in the tree, set `core.fsmonitor` in its own
  `.git/config` and commit the gitlink as a submodule; without the flag, the
  daemon's own status or diff looks inside that embedded repository to report
  its state and runs the command it names. `-c diff.ignoreSubmodules=all` is
  not enough on its own, because a `.gitmodules` entry with `ignore = none`
  that the agent writes overrides it; the command-line flag is not overridden
  by anything in the tree. It covers `repo::is_dirty` (which `repo.inspect`
  calls, so opening the New Agent dialog on a repository is enough to run an
  embedded config without it) and the base-dirty guard of §5.4 as well as the
  workspace queries. This closes the hole for both workspace kinds except one
  known exception: the rebase step of §5.4 runs `git rebase`, which has no
  `--ignore-submodules` option, so an embedded repository an agent committed
  can still have its config run there. That is a follow-up, not fixed by this.
  The conflict list that follows a failed merge or rebase used to be a second
  exception — `git diff --diff-filter=U` compares the worktree with the index
  and looks into a submodule to do it — and now reads the index alone with `git
  ls-files --unmerged`.

### 5.4 Merge, rebase, squash
Run in the main repo by the daemon, never inside a sandbox:
- Serialised per repository: two merges, or a merge and a push, of the same
  repository never run at once, because both move the same base branch.
- Where it lands: in the user's own checkout when that is on the base branch,
  and otherwise in a temporary worktree of the base under
  `~/.bondsymphonic/merge-<id>`, removed on every exit path. Scratch worktrees an
  earlier run left behind (a killed daemon) are reaped at the start of each
  merge, under the same lock, so anything still there belongs to nobody.
- Guard: the working tree must be clean of *tracked* changes, **and it is only
  checked on the first of those two paths** — when the merge will land in the
  user's own checkout. Otherwise the user's checkout is not involved and is not
  inspected. The question asked is `git status --porcelain
  --untracked-files=no`: uncommitted edits to tracked files are `Conflict
  {reason: "base_dirty"}` with a message naming the repository and the base
  branch, and untracked files are not, because git will not overwrite one and a
  scratch file in a checkout is not a reason to refuse every merge into it.
  `RepoInfo.is_dirty` asks the same question with the same flag, so what the New
  Agent dialog calls dirty and what a merge refuses are one thing.
- `merge`: `git merge --no-ff bs/<name>/work`.
- `rebase`: `git rebase <base> bs/<name>/work` in the workspace worktree, then
  fast-forward the base.
- `squash`: `git merge --squash` + `git commit -m "<name>: <summary>"` where the
  summary is the first line of the last workspace commit unless the request
  supplies a message.
- On conflict: abort (`--abort`, or `reset --merge` for a squash), return
  `{ok:false, conflicts:[paths], reason:"conflict"}` — an RPC *success*, not an
  error, because a conflict is an answer. The workspace is untouched and the user
  can ask the agent to rebase. `conflicts` holds repo-relative paths with `/`
  separators on every platform.
- After success the base has moved and the workspace and its branch still exist;
  removing them is a separate `workspace.destroy`. The daemon then absorbs the
  merged range into the shared object store (§5.2), and a failure there fails the
  RPC with `reason: "objects_stranded"` and `merged: true`.
- **`workspace.destroy` takes the same per-repository lock** across its teardown
  half — from removing the worktree and the branch through deleting the
  workspace's private object directory. Without it a destroy can delete the
  objects a merge is still absorbing, and the base branch is left pointing at
  commits that no longer exist: `git log main` fails in the user's own
  repository. The lock is taken after the sandbox shutdown, which is slow and
  touches no git.
- **Hooks: a daemon merge runs none of the repository's.** `daemon_git` pins
  `core.hooksPath` at the empty daemon-owned directory (`DataDirs::no_hooks`),
  for the main repository and for the scratch worktree, matching what
  `worktree_git` already does on the workspace side. A merge the daemon performs
  is not the user typing `git merge`: it happens when they click a button in
  another window, over content an agent wrote, so a `post-merge` or `commit-msg`
  hook firing there would run repository-supplied shell commands nobody asked
  for. The user's own `git merge` in their own checkout is unaffected.
  - The one exception is `git push` (§5.5), which runs through `daemon_push_git`
    with the repository's hooks left in place, because `pre-push` is how
    `git-lfs` uploads the objects a push needs.
- **Filter and merge drivers still run.** The `NEUTRALISED_CONFIG` list
  `worktree_git` empties is deliberately *not* applied to `daemon_git`. Those
  keys name the user's own `filter.*.clean` / `.smudge` and `merge.*.driver`
  programs — `git-lfs` above all — the main repository's config is not
  agent-writable, and emptying them would corrupt the user's checkout by leaving
  LFS pointer files where their content should be. The residual risk is stated
  rather than closed: a `.gitattributes` **in the merged tree**, which an agent
  wrote, chooses which of the user's own drivers run and over what content,
  during a merge the daemon performs on the host outside any sandbox.
- **`workspace.merge` and `workspace.create_pr` refuse an in-place workspace
  outright**, before any git runs at all: `InvalidParams`, `data.reason =
  "in_place"`, `"an in-place workspace has nothing to merge; commit and push
  from the checkout"` (`in_place::nothing_to_merge`, shared by both calls).
  There is no branch of its own for either to land.

### 5.5 PR
`git push origin bs/<name>/work` — deliberately without `-u`, which would write
`.git/config` and stop an in-place workspace of the same repository (§6.2) —
then `gh pr create --title --body [--draft]
--head bs/<name>/work --base <base>`; parse the URL from stdout. `gh` runs on the
host with the daemon user's own configuration, never in a sandbox, and must be
authenticated in the distro (reported by `check_prereqs` as `gh_auth`).
`BS_GH_BIN` overrides the binary, split the way a shell would, for tests.

Under the same per-repository lock as §5.4, and followed by the same absorb
(§5.2) — over `<ws.branch> ^<ws.base_branch>`, because what the push leaves
behind is `refs/remotes/origin/<branch>` and that ref outlives the workspace.
A failure there is `reason: "objects_stranded"` with `pushed: true`.

**The push runs the repository's own hooks**, unlike every other daemon-side git
operation (§5.4). It goes through `Layout::daemon_push_git`, which is
`daemon_git` without the `core.hooksPath` pin, because `pre-push` is how
`git-lfs` uploads the large objects the pushed commits point at — a push that
skipped it would put pointer files on the remote with nothing behind them. So a
**Create PR** runs the user's `pre-push` over content an agent wrote. A push is
also the one daemon-side git operation the user asked for by name.

A failure of either command is a `GitError` carrying `{command, exit_code,
stderr}`. The `command` field is what tells the two apart; the title and the body
are deliberately not in it, so a client cannot echo them back from the error.

**Residual risk: `gh pr create` runs its own `git status --porcelain`.**
`pr.rs` sets `gh`'s working directory to `ws.repo_path` — the user's own
checkout — and `gh` (traced at 2.45.0, `NewCreateContext` in
`pkg/cmd/pr/create/create.go`) calls `UncommittedChangeCount`, which is
exactly `git status --porcelain`, with no `--ignore-submodules` and none of
the neutralisation `daemon_git` applies. So **Create PR** on a *worktree*
workspace, of a repository that also has an in-place workspace open, can run
an embedded repository's `core.fsmonitor` (§6.2's residual risks) as the
daemon user, outside every sandbox, the moment the button is pressed. Not
fixed: the remedy is running `gh` with `--repo owner/name` against a
daemon-owned working directory, which changes how it resolves the base and
head repositories and needs a real GitHub repository to verify. Follow-up.

## 6. Sandbox

### 6.1 Trait

```rust
#[async_trait]
trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn check(&self) -> Vec<PrereqStatus>;
    /// Start the long-lived sandbox for a workspace (one per workspace).
    async fn start(&self, spec: &SandboxSpec) -> Result<Box<dyn SandboxHandle>>;
}

#[async_trait]
trait SandboxHandle: Send + Sync {
    /// Spawn a process inside the running sandbox.
    async fn spawn(&self, cmd: SandboxCommand) -> Result<SandboxChild>;
    async fn shutdown(&self) -> Result<()>;   // kills every process inside
}

struct SandboxSpec {
    id: WorkspaceId,
    rw_binds: Vec<(PathBuf, PathBuf)>,   // host -> sandbox
    ro_binds: Vec<(PathBuf, PathBuf)>,
    home: PathBuf,                        // mounted at /home/<user>
    run_dir: PathBuf,                     // host ~/.bondsymphonic/run/<id>, mounted rw at /run/bs
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}

struct SandboxCommand { argv: Vec<String>, pty: Option<PtySize>, env: BTreeMap<String,String>, cwd: Option<PathBuf> }

struct SandboxChild {
    pid: u32,                       // pid as seen by the sandbox init
    stdin: Option<OwnedFd>, stdout: Option<OwnedFd>, stderr: Option<OwnedFd>,  // pipes, or
    pty_master: Option<OwnedFd>,    // when cmd.pty was set
    exit: oneshot::Receiver<i32>,
}
```

Every process (agent, shell, web app, forwarder) is spawned through the
workspace's `SandboxHandle`, so all of them share one set of namespaces: one
network namespace (so the forwarder can reach the dev server), one PID namespace
(so shutdown kills everything), one mount namespace.

### 6.2 Linux/bwrap filesystem rules
A sandboxed process sees the host root read-only, minus a fixed set of masks,
plus its own workspace's paths bound back in. In argv order:
- `--ro-bind / /`, `--proc /proc`, `--dev /dev`.
- An empty tmpfs over each of `/tmp`, `/home`, `/run`, `/opt`, `/mnt` and the
  daemon's data directory (`~/.bondsymphonic` by default, or `--data-dir`;
  omitted when one of the fixed masks already hides it). `/tmp` and `/run`
  because the sandbox needs writable scratch and a home for `/run/bs` under a
  read-only root; `/home` because the daemon user's own home is not a
  workspace's business; `/opt` because that is where host software lives and
  where the daemon binds its helpers; `/mnt` because on WSL that is every
  Windows drive; the data directory because it holds every other workspace's
  worktree, home, objects and exec socket.
- The masks are the whole of what is hidden. Everything else under the
  read-only root stays readable, `/var/tmp`, `/var/lib` and `/srv` included,
  and a host Unix socket sitting outside the masked roots stays connectable
  from inside a sandbox even though its network is unshared. That is
  deliberate: the masks cover the daemon's own data and every other
  workspace's, and the read-only root is what makes the rest of the host
  inspectable but unwritable. A host service that must not be reachable from a
  workspace belongs behind one of the masks, not behind the read-only root.
- Every mask precedes every bind, so only what is bound comes back:
  `--bind homes/<id> /home/<user>`; when the daemon binary itself lies under a
  mask, `--ro-bind <daemon> /opt/bs/daemon` (the handle's `helper_exe()`
  answers with whichever path applies); `--ro-bind <repo>/.git` and
  `--ro-bind <resolved claude> /opt/bs/claude` (8.2 says how the host path is
  resolved); `--bind worktree_path worktree_path` (same path inside so git
  paths match), `--bind objects-<id> objects-<id>`, the writable `.git`
  subpaths from 5.2, `--bind caches/<id> /home/<user>/.cache`; then the late
  read-only binds (`config.worktree`); then
  `--bind ~/.bondsymphonic/run/<id> /run/bs` (exec, proxy and forward sockets).
- So when the repository lives under `/mnt`, only its `.git` (and the
  worktree's gitdir paths inside it) are visible there; the repository's own
  checkout, its siblings and every other Windows path are not. A repository
  elsewhere sees nothing under `/mnt` at all.
- `--clearenv`, then `--setenv` for exactly the base environment every process
  in the sandbox gets (`HOME`, `USER`, `PATH`, `TERM`, `LANG`, the git object
  directories, the proxy variables, `BS_WORKSPACE`). Both processes the sandbox
  starts from begin at a fixed environment: the daemon spawns bwrap with a
  cleared environment holding only the lookup path
  `PATH=/usr/sbin:/usr/bin:/sbin:/bin`, enough to resolve `bwrap` by name, and
  bwrap starts init from the per-workspace base environment above (plus the
  `PWD` it derives from `--chdir`). Neither inherits any variable of the
  daemon's own environment, which matters because bwrap's process stays inside
  the pid namespace as pid 1 and `/proc/1/environ` is readable by everything in
  the sandbox.
- `--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup`
  and `--unshare-net` (6.3). `--die-with-parent`, `--new-session`.

**In-place workspaces.** An in-place sandbox's mounts come from
`InPlaceLayout`, not `Layout`: read-write `<root>` and `<root>/.git` — a mount
of its own — then, late and read-only, in this order: `.git/config`,
`.git/commondir`, `.git/hooks`, `.git/info`, `.git/worktrees`,
`.git/remotes`, `.git/branches`, `.git/modules` (only when it is a real
directory, never a symlink), and `.git/config.worktree`, which is bound
unconditionally — unlike a worktree workspace's own `config.worktree`, an
in-place one is the user's single git directory, and `git sparse-checkout
init` can turn on `extensions.worktreeConfig` at any moment an agent chooses,
so the daemon does not wait for that to decide whether the file is protected.

`InPlaceLayout::prepare` runs before every sandbox start — create, restore,
restart, Retry — and creates every one of those targets that is missing,
before the sandbox ever starts. This is not tidiness: measured, bubblewrap
creates a missing `--ro-bind` target itself, an empty directory for a missing
directory or an empty `0444` file for a missing file, and the filesystem
underneath a bind here is the user's own writable repository, so a target
bwrap made on its own would be a target the daemon does not know it owns. Of
what `prepare` creates, `hooks` and `info` are left as ordinary git output —
`git init` makes both itself, and they stay after Close — but `worktrees`,
`remotes`, `branches` and `config.worktree` are recorded in a daemon-owned
file outside every sandbox, `<data>/in-place/<id>.created`
(`DataDirs::in_place_record`), so Close (`InPlaceLayout::release`) can take
back exactly the ones it made, and only while each is still empty: whatever
git has since put there belongs to the user.

`worktrees`, `remotes` and `branches` are read-only for reasons beyond what
`.git/config` itself already covers: `worktrees` holds the git directories of
this same repository's *worktree* workspaces, and their `config.worktree` is
exactly what the daemon's own status reads for them; `remotes` and `branches`
hold the legacy remote definitions `git fetch <name>` and `git push <name>`
read, which an agent could otherwise use to redirect a name the user types.

`prepare` also writes `.git/commondir` as `.` followed by a newline, before
the first bind, and refuses (`InvalidParams`) a repository whose `commondir`
already holds something else. Git reads `commondir` from *any* git directory,
not only a linked worktree's, and takes config, refs and objects from
wherever it points; an agent that could write it could point the user's next
`git status` at a config of its own choosing, even with `GIT_DIR` pinned. `.`
points the common directory at the git directory itself, which is what it
already is, and was measured to change nothing: status, commit, switch,
stash, `worktree add`/`remove`, gc and fsck all behave identically with and
without it, under git 2.43 (the distro) and Git for Windows 2.52.

A mount point — `.git` itself, or any of the entries above — cannot be
renamed or removed *from inside the sandbox*: the kernel answers `EBUSY`.
`rm -rf .git` therefore exits non-zero, and `.git`, `config`, `commondir`,
`hooks`, `info`, `worktrees`, `remotes` and `branches` all survive it — but
everything under `.git` that is not one of those mount points, `HEAD`, the
index, `objects/`, `refs/` and `logs/` included, is deleted just the same, so
the agent can still destroy the repository's history from inside its own
sandbox. There is no private object directory and no `GIT_OBJECT_DIRECTORY`
or `GIT_ALTERNATE_OBJECT_DIRECTORIES` for an in-place workspace: the agent's
objects go straight into the repository's own store, which is the point of
working in place.

**A bind can still be defeated — from outside the sandbox.** The `EBUSY`
guarantee above holds only inside the agent's own mount namespace. It is not a
guarantee against anything that runs outside the sandbox, in the same
repository, while the workspace is open: a `git config` the user types, `git
branch -u`, `git push -u`, `git remote add`, or `git sparse-checkout` all replace
or remove the file or directory a bind was made on — git's lock-and-rename
pattern for `config` chief among them — and the kernel detaches that bind
inside the sandbox exactly as a plain rename or remove of a mounted-over path
always has. Nothing run inside the sandbox can prevent that: the entry the
bind was protecting is simply gone from under it, and the path falls through
to the writable `.git` beneath.

**And on a Windows drive the bind is not the control at all.** A bind protects
one dentry. On DrvFs (9p, `aname=drvfs`) Windows resolves names that differ
only in case, and 8.3 short names, to the same file, and each alias is a dentry
no mount covers. Measured in the distro with the daemon's own bind shape:
`>> .git/config` fails as intended, while `>> .git/CONFIG`, `>> .GIT/config`
and `>> GIT~1/config` all reach the user's config, and `echo x >
.git/HOOKS/pre-commit` plants a hook — with the mount, the device and the inode
unchanged in every case. A Windows `git status` in that repository then ran the
planted `core.fsmonitor`. The IDE is a Windows application and the repositories
the user picks are Windows folders, so this is the ordinary case rather than an
edge one, and it is why the watcher below compares contents rather than
identity alone. On such a drive the protection is detect-and-stop rather than
blocked, which the New Agent dialog and the user guide say in as many words.

The daemon watches for all of this rather than relying on the bind.
`InPlaceLayout::snapshot`, taken right after `prepare` and before the sandbox
starts, records three things:

- each protected entry's device, inode, type and link count — not size or
  time, so the user editing `.git/config` in their own editor is never mistaken
  for a replacement. A protected file that already has a second hard link is
  refused by `prepare`, and one that gains a second is a breach: a write through
  the other name reaches the same file with identity and mount intact;
- the contents of everything under `.git` that git runs or reads as
  configuration — `config`, `config.worktree`, `commondir`, the listings and
  contents of `hooks`, `info`, `remotes`, `branches` and `modules`, and the
  `config.worktree` and `commondir` of the registrations under `worktrees`
  that existed when the sandbox started, which is where the same alias trick
  would otherwise reach a *worktree* workspace's own gitdir. Not the rest of
  such a registration (`index`, `HEAD`, `logs`, `refs`), which the daemon's
  own work in that sibling worktree rewrites constantly, and not a
  registration that appears later: `workspace.create` on a sibling worktree
  workspace adds one while an in-place sandbox runs, and the daemon must not
  stop an agent over its own ordinary work. `info/refs` is excluded too:
  every `git gc` rewrites it through `update-server-info`, and `gc --auto`
  runs behind an ordinary commit, so covering it would stop an in-place agent
  at random — it is a dumb-HTTP ref listing, not anything git executes. A
  `*.sample` hook is listed but not read, since git never runs a file with
  that suffix; making one run means giving it a name git knows, which is a
  new listing entry and is caught that way. The set is bounded by
  `COVERED_CAP` (512 paths), `COVERED_BYTES_CAP` (4 MiB total) and `READ_CAP`
  (256 KiB per file) — the recorded length is part of the comparison too, so
  a file that grows past its own cap still reads as changed — walked in a
  fixed sorted order, so a snapshot and a later check that both hit a cap
  stop at the same place and anything ahead of it is itself a change.
  `modules/**` descends only through path components named `modules` (how
  git nests them), eight deep, so nothing reaches into a submodule's object
  store;
- the mount points each entry was bound at.

Every `PROTECTION_POLL` (250 ms) while the sandbox is up,
`ProtectedSnapshot::check` re-`lstat`s every entry, re-reads the covered set and
compares it whole, and, given the sandbox's pid, re-reads its
`/proc/<pid>/mountinfo` to confirm each entry is still actually mounted there —
identity alone is not enough, because two config writes in a row can hand a
fresh file the same inode number an ext4 filesystem just freed, and an aliased
write on DrvFs changes neither identity nor mount. Measured at 71–117 ms a
poll against BondSymphonic's own `.git` (14 hooks, 2 worktree registrations)
copied onto a Windows drive, against the 250 ms period — down from 196 ms
before two optimisations: one syscall fewer per path, and listing a
`*.sample` hook from its directory entry rather than reading it. The first change
any of the three finds is a breach: the sandbox is torn down, the workspace
moves to `Error(ProtectionBreach::sentence())`, and a `daemon.log` **warning**
carries that sentence, the entries that changed and a unified line diff of the
files whose contents are read. The same sentence and diff go out as a
workspace-tagged warn-level `daemon.log` *event*, which is what the IDE keeps
against the workspace and offers behind its banner's **What changed** (IDE
design §12); the IDE pairs the two by their first line, so the wording stays
the daemon's alone. A sandbox that has died on its own is not reported as a
breach — its pid is gone, so nothing would read as mounted — and a check that
could not be made at all stops the sandbox with a sentence of its own rather
than leaving it unwatched. Retry (`workspace.restart`) calls `prepare` and takes
a fresh snapshot, so a repaired repository is protected exactly as it was on
create.

This closes the gap for everything that happens after the next check runs,
but not for the poll interval itself: a change the check has not yet seen —
made in the up-to-250-ms window before it fires — can still leave
`.git/config` holding something written from outside the sandbox, and the
daemon only reports it after the fact; the logged diff is what lets the user
tell a setting they meant to make from one they did not, before pressing
Retry. One consequence follows directly: any git command that writes into a
protected entry — not only one an agent runs, but one the user themselves
runs in that repository while an in-place workspace is open on it — reads as
a breach and stops that workspace's sandbox. This is by design: the daemon
cannot tell the user's own `git config` apart from an agent's, and the
alternative would be trusting a bind that has already been shown to be
defeatable from outside.

**The daemon's own worktree cleanup does not trip this.** Two of the
daemon's own operations remove or rewrite exactly the entries §4.1 (in-place
paragraph, above) protects, and both were shown by the protection watcher's
own tests to trip it — against an *in-place* workspace of the same
repository, not the worktree workspace being acted on:

- **Removing a worktree deletes `.git/worktrees` itself once the last linked
  worktree is gone**, which detaches the bind under any in-place sandbox of
  the same repository. `worktree::remove` (a plain destroy) and the merge/
  rebase scratch-worktree reaper (§5.4) hold the directory open first:
  `WorktreesHold::take` creates a `.bs-hold-<id>/locked` entry in
  `.git/worktrees` before either operation runs, under the same
  per-repository lock `InPlaceLayout::prepare`/`release` take, and drops it
  (removing the entry again) once the operation is done. Git skips a locked
  entry when it prunes, and `worktree list` skips one with no `gitdir`, so the
  directory is never empty while a hold is live and is never deleted out from
  under a bind the daemon itself still needs. A stale hold a killed daemon
  left behind is taken away by the next one that reaches that repository,
  before it takes its own — but only when its `locked` file says exactly what
  the daemon writes there and the entry holds nothing else, so a worktree of
  the user's own that happens to carry the name keeps its lock. Best effort: a
  hold that cannot be made costs the in-place sandbox a Retry, not the removal
  it was protecting against.
- **The user's own `git worktree prune` and `git gc` do the same, and are not
  the daemon's to bracket.** Git's `--auto` maintenance runs behind an ordinary
  `git commit`, and an empty `.git/worktrees` is removed by it. So while an
  in-place workspace exists the directory carries a hold of that workspace's
  own: `InPlaceLayout::prepare` writes `.bs-inplace-<workspace id>/locked` and
  `release` removes exactly that entry at Close, and only while it still holds
  what the daemon wrote. Its name deliberately does not start with `.bs-hold-`,
  which the stale sweep above takes away. It registers no worktree, so `git
  worktree list` does not show it.
- **Deleting a workspace's branch used to run `git branch -D`, which rewrites
  `.git/config` every time** to drop a `branch.<name>` section that is
  usually not there at all — and a rewritten `.git/config` is exactly what the
  protection watcher exists to catch. `remove_branch` now deletes the ref
  directly (`git update-ref -d refs/heads/bs/<name>/work`) and removes the
  `branch.<name>` config section separately, and only when `git config
  --local --get-regexp` finds one — which happens for a workspace whose
  Create PR set an upstream (see the gap below). A plain destroy therefore no
  longer touches `.git/config` at all, and no longer stops an in-place
  sibling.

**A `.git/worktrees` that is gone anyway says so plainly.** With the persistent
hold the user's own `git worktree remove`, `prune` and `gc` no longer empty
that directory, so what is left is deleting it by hand, which still detaches
the bind. The watcher tells that apart from a replaced or foreign entry:
`ProtectionBreach` now
carries `removed`, the subset of `entries` that are gone rather than
replaced, and `ProtectionBreach::sentence()` answers with a different
sentence when the breach is exactly `.git/worktrees`, removed: *"This
repository's last worktree was removed, which also removed a directory the
sandbox keeps read-only, so the sandbox was stopped. Nothing needs checking;
press Retry."* — no diff to review, because there is nothing to have planted:
`.git/worktrees` names no program.

**Create PR pushes without `-u`.** `workspace.create_pr` used to run `git push
-u origin <branch>` (§5.5), which sets that branch's upstream in `.git/config`
and so stopped an in-place sibling of the same repository, the same way any
other host-side `.git/config` write does (above). It now runs `git push origin
<branch>`: nothing in the flow needs the tracking, because `gh pr create` is
given `--head` explicitly and the object absorption that follows reads
`refs/remotes/origin/<branch>`, which a push updates either way. An upstream
the user wants is theirs to set, when no in-place workspace is open.

### 6.3 Sandbox init (one bwrap per workspace)
bubblewrap cannot join an existing network namespace, so the daemon runs exactly
one `bwrap` per workspace and does all process management through a small init
inside it:

```
bwrap <mount and unshare flags> --die-with-parent --new-session \
      -- bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock
```

`sandbox-init` runs as pid 2 inside the sandbox (bwrap's own process is pid 1
and reaps for it). It:
- listens on the Unix socket `/run/bs/exec.sock` (host path
  `~/.bondsymphonic/run/<id>/exec.sock`, rw-bound as `/run/bs`);
- accepts spawn requests (argv, env, cwd, optional pty size) from the daemon;
  for each it forks the child inside the sandbox, and passes the child's stdio
  pipes or the PTY master back to the daemon over the socket with `SCM_RIGHTS`,
  so the daemon reads and writes those fds directly with no proxying;
- reports exits (pid, code) on the socket and reaps zombies. The daemon side
  keeps an exit that arrives before its spawn has returned for at most 60 s and
  hands it over once; a spawn whose caller went away before init answered is
  killed on arrival and its exit dropped;
- on `shutdown` or socket close sends SIGTERM to every child, SIGKILL after 5 s,
  then exits, which tears the sandbox down.

The `linux_bwrap` backend's `start` spawns bwrap, waits for `exec.sock` to
appear (5 s timeout), and returns a handle wrapping the socket client. `spawn`
is one request/response on that socket. This mirrors how container runtimes
implement `exec` and keeps every bwrap-specific flag in one place.

The daemon itself lives outside the sandbox, so the proxy (7.2) and port bridge
(7.3) sockets are simply files in `/run/bs` that both sides can open.

### 6.4 Noop backend
Runs commands directly with the same env and cwd. Used by unit tests, on hosts
without namespace support (with a loud warning in the UI), and via
`--no-sandbox`.

## 7. Network

### 7.1 Allowlist
`HostPattern` is either an exact host (`api.anthropic.com`) or a suffix wildcard
(`*.npmjs.org`). Ports are unrestricted. Default list: `api.anthropic.com`,
`*.anthropic.com`, `platform.claude.com` (the CLI's OAuth token endpoint),
`registry.npmjs.org`, `*.npmjs.org`, `pypi.org`,
`files.pythonhosted.org`, `crates.io`, `static.crates.io`, `index.crates.io`,
`github.com`, `*.github.com`, `*.githubusercontent.com`. The repo's
`bondsymphonic.toml` `[network] allow = [...]` extends it; `workspace.set_allowlist`
overrides it at runtime (at most 256 entries of at most 253 bytes each). A
wildcard must leave a registrable name behind it, so `*.example.com` is a pattern
and `*.com` is refused. An entry and a host are compared in one normalised
form: lowercased, without the root dot of a fully qualified name, and — for an
IPv6 literal, which may be written with or without the brackets a URI wraps it
in — in canonical spelling. A published denial carries that same form, so the
entry a one-click "Allow host" writes back is the one the next request matches.

A repository extends the allowlist at creation without anyone necessarily having
read it, so the list is a list of *names* the user may not have chosen. Two rules
follow, and both are the boundary rather than hygiene:

- **The address, not the name, is what is allowed.** After resolving, any
  address that is not on the internet is dropped, in either address family.
  IPv4: `0/8`, `127/8`, `10/8`, `172.16/12`, `192.168/16`, `169.254/16` (where
  the cloud metadata endpoint lives), `100.64/10` (carrier-grade NAT — the
  operator's network, not the internet), `192.0.0/24` (IETF protocol
  assignments), `198.18/15` (benchmarking), `224/4` (multicast) and `240/4`
  (reserved, broadcast included). IPv6: `::/96` — which covers loopback, the
  unspecified address and the withdrawn IPv4-compatible spelling — `fc00::/7`,
  `fe80::/10`, `ff00::/8`, and `64:ff9b::/96`, the NAT64 prefix, through which
  an IPv6-only host names an IPv4 destination via a translator on the local
  network. An IPv4-mapped address (`::ffff:a.b.c.d`) is classified as the IPv4
  address it is. If nothing is left the request
  is refused `403` with a body saying the destination is private. The one
  exception is an allowlist entry that *is* that literal address: writing
  `127.0.0.1` down is a thing only a person does.
- **A denied host is validated before it is published.** The sandbox chooses the
  text of a request's target, and a denial travels into the IDE's toast and from
  there into the allowlist. A target that is not a plain ASCII hostname (labels
  1–63 of letters, digits and `-`; at most 253 in all) or an IP literal is
  answered `400` and produces no denial event.

Repeated denials of one host in one workspace are coalesced to one `daemon.log`
warn per 5 s. The `403` is still sent every time.

### 7.2 Proxy
Per workspace, the daemon listens on a Unix socket
`~/.bondsymphonic/run/<id>/proxy.sock`, bound into the sandbox at
`/run/bs/proxy.sock`. Because most tools only speak proxy over TCP, a shim inside
the sandbox (`bondsymphonic-daemon proxy-shim`) listens on `127.0.0.1:3128` and
pipes to the Unix socket. The sandbox env sets `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, `NO_PROXY=localhost,127.0.0.1`, and git/npm/pip/cargo honour those.

The proxy implements HTTP `CONNECT` (for TLS) and plain absolute-URI
forwarding. Either way the target host is checked against the allowlist and its
resolved addresses against the address rules of 7.1 before anything is
connected, and a refusal is a `403` whose body names the host and either the
config key to add it or the reason the destination is refused. Every denial is
emitted as `daemon.log {level: warn}` so the IDE can surface it, coalesced per
host per workspace as 7.1 says.

A `CONNECT` pins its connection: once the tunnel is up, everything the client
sends goes to the one host it was allowed. A plain-HTTP connection is not one
decision but a sequence of them, because a keep-alive client's next request
names its own host. So plain HTTP is served **one request at a time** — read one
head, decide the host from the absolute-form URI alone, run the same checks,
open a connection to *that* host, forward the request in origin form with the
hop-by-hop headers stripped and `Connection: close` added, relay the body by its
`Content-Length` or chunked framing, relay the response until the upstream
closes, then go round for the next request. The added `Connection: close` is
what delimits the response without the proxy having to parse response framing. A
head whose body framing is ambiguous (a non-chunked transfer coding, a
non-numeric or contradictory `Content-Length`) is answered `400` and the
connection is closed, rather than guessed at. So is a head that carries a bare
`LF`, a lone `CR` or a `NUL` inside a header name or value, or a header name
that is not a token: the head is split on CRLF here, so those bytes survive
inside a field and an origin that ends a line on a bare `LF` would read a second
request where this proxy checked one.

There is no fallback to the `Host` header: a request whose target is not an
absolute-form `http://` URI has no host this proxy will serve and is answered
`400`. `Host` is the field a smuggled request most easily disagrees with the URI
about, so the authority the allowlist cleared is the authority the socket is
opened to, and the `Host` sent upstream is rewritten from that URI rather than
copied from the client.

An exchange is answered only while there is still a response to answer with: the
two directions are counted as they are relayed, and a `400` or a `408` is
written only when nothing of a response has gone downstream yet; past that point
the connection is simply closed rather than having a second response spliced
onto a half-written first. A 60 s idle deadline bounds a stalled exchange,
measured from the last byte that moved in *either* direction and lifted once the
request body is through, so a slow upload is never cut off and an origin that
thinks for a long time still answers; a stall with nothing relayed yet is a
`408`.

### 7.3 Port bridge (web apps)
When a run declares port `P`, the daemon:
1. Allocates a free host port `H`.
2. Creates `~/.bondsymphonic/run/<id>/fwd-<run_id>.sock`, bound into the sandbox
   at `/run/bs/fwd-<run_id>.sock`. Keyed by the **run**, not by the port:
   detection routinely yields several configurations sharing one port (`dev`,
   `start` and `serve` out of one `package.json`), and a port-named socket would
   have the second run unlink the first one's live socket, leaving run 1's bridge
   on run 2's forwarder and either stop taking both down.
3. Spawns inside the sandbox `bondsymphonic-daemon forward --socket
   /run/bs/fwd-<run_id>.sock --port P`, which accepts on the Unix socket and
   connects to `127.0.0.1:P`.
4. Listens on `127.0.0.1:H` and bridges each accepted TCP connection to the Unix
   socket. A failed `accept` is logged and the loop carries on after a 100 ms
   pause: a peer that reset before the handshake finished, or a moment of
   descriptor exhaustion, leaves the listener perfectly able to accept the next
   connection. Only a listener that is itself closed — or 50 consecutive
   failures with no connection between them, which is a listener that is never
   going to recover — ends the bridge, and then with one warning rather than one
   every 100 ms for ever. Any accept that succeeds resets the count. A loop that
   returned on the first error left the port published in the IDE with nothing
   behind it, which looks exactly like the app inside the sandbox having died.

**The status byte.** Before any of the application's own bytes, the forwarder
writes exactly one byte on each accepted Unix connection: `1` once it holds the
in-sandbox TCP connection, `0` when it could not get one (the connect attempt is
on a 2 s clock). The host side reads that byte before it starts copying, and on
`0`, on a read error, or after 5 s of silence it shuts the TCP connection down
rather than leaving it open — so a browser is told "connection refused" instead
of spinning on a port whose server is not there.

The byte is also what makes readiness a fact rather than a guess. A TCP port on
the host that accepts proves only that the bridge's own listener is up; a `1`
from the far side of the namespace proves the application answered. The
readiness poll (10.3) is therefore one round trip on the socket — connect, read
the byte, close — with a 400 ms budget over both steps so a probe cannot spill
into the next 500 ms tick. Nothing is written to the application, so probing a
server that logs its requests does not fill the run's output with them.

WSL2 forwards Windows `localhost:H` to the distro, so the IDE presents
`http://localhost:H`. Once the byte says `1`, bridging is bidirectional byte
copying with per-connection tasks and no lifetime cap: no protocol awareness, so
WebSockets and HMR work.

## 8. Agents

### 8.1 Trait

```rust
#[async_trait]
trait AgentAdapter: Send {
    async fn start(&mut self, ctx: AgentContext) -> Result<()>;
    async fn send(&mut self, text: String) -> Result<()>;
    async fn permission_reply(&mut self, reply: PermissionReply) -> Result<()>;
    async fn interrupt(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}
```

`AgentContext` gives the adapter the sandbox backend + spec, an event sink
(`mpsc::Sender<AgentEvent>`), and the transcript store. Every event the adapter
emits is appended to `transcripts/<agent_id>.ndjson` before broadcast, so
`agent.history` is a file read.

An agent leaves the manager's map only when its workspace is destroyed, and only
once the destroy has actually removed the worktree. Stopping a workspace's
agents ahead of teardown ends their processes and leaves the entries; a destroy
that fails late leaves the workspace behind in `error`, and its agents'
transcripts are the one thing still worth having out of it.

**One `agent.start` at a time per workspace.** A start seeds the workspace home
and copies the repository's `[claude] settings` into it before it spawns
anything, unlinking the destination and then creating it; two at once leave the
second agent's settings file missing or half written. A second start while the
first holds the gate is a `Conflict` with `data.reason = "agent_starting"` —
refused rather than queued, because the client that asked has a user waiting on
it. The reason names the gate and not a count: a workspace that already has
three agents in it takes a fourth start perfectly happily, so a client acting on
the string tells the user to try again rather than to stop an agent.

### 8.2 Claude Code adapter
Spawns inside the sandbox, as built by `claude_argv` in
`crates/daemon/src/agents/claude.rs`:

```
<claude> -p --input-format stream-json --output-format stream-json --verbose \
       --include-partial-messages --permission-prompts host \
       [--resume <session_id>] [--model M] [--permission-mode MODE]
```

`<claude>` is always an absolute path, never the bare name. Under `linux_bwrap`
it is `/opt/bs/claude`, where `workspace::lifecycle::spec_for` binds the daemon
user's own install read-only; under a backend with no mounts it is that
install's own path. Claude Code is a single native executable, so one bind of
one file is the whole of it, and nothing is created on the host. The binary is
resolved from `~/.local/bin/claude`, canonicalised because it is a symlink into
`~/.local/share/claude/versions/<v>`, falling back to a `PATH` search that
refuses anything under `/mnt/`. A host with no install makes `agent.start` fail
with `PrereqMissing` naming the path it looked at, rather than an exec failure a
second later.

`--permission-prompts host` is what routes a tool prompt back over stdout as a
`control_request` for the IDE to answer, instead of the CLI asking at a terminal
it does not have. The three optional flags are appended only when
`agent.start.options` carries a non-empty value for them, and `permission_mode`
is validated against the CLI's own list (`default`, `acceptEdits`, `plan`,
`dontAsk`, `bypassPermissions`, `auto`, `manual`) so a bad value is an
`InvalidParams` on `agent.start` rather than a usage error a second later.

The flag set is pinned per Claude Code version: the adapter records
`TESTED_CLAUDE_VERSION = "2.1.263"` and warns when the installed `claude
--version` differs. The probe runs before every agent's first start of a given
program, under a 10 s timeout with `kill_on_drop`: a CLI that never answers — a
binary on a filesystem that has gone away, one waiting on a terminal that is not
there — fails `agent.start` with `PrereqMissing` and
`data.reason = "claude_probe_timeout"` rather than hanging the request. Only a
program that *answered* is remembered, so a half-installed CLI the user then
repairs is usable without restarting the daemon, and the list of programs
already probed belongs to the `AgentManager` rather than to the process, so
every daemon starts from nothing probed. The program probed is the host's own
install, on every backend: under `linux_bwrap` the agent is spawned as
`/opt/bs/claude`, which is a mount point inside the sandbox and not a path the
daemon can run, so probing that name on the host is an `ENOENT` that answers
nothing and would silently retire the version check. `BS_CLAUDE_BIN` is the
exception and is probed as given, since a hand-named stand-in has no host
original to map back to. Every flag above was verified accepted by
2.1.263 — a wrong flag makes `claude` exit with "unknown option" before any login
check, so this is testable without being logged in. Two traps found in practice:

- Under WSL a *non-login* shell inherits the Windows PATH, so a bare `claude`
  can resolve to a Windows npm install of a different version. 2.1.177 rejects
  `--permission-prompts` outright. This is why the daemon never spawns the bare
  name: the version check, the prerequisite check and the agent spawn all go
  through the one resolver above, which prefers `~/.local/bin/claude` and refuses
  `/mnt/...`. `scripts/record-claude-stream.sh` refuses a `/mnt/...` binary for
  the same reason. The setup terminals still put `$HOME/.local/bin` first on
  their `PATH`, because those run `claude auth login` by name on the host.
- `BS_CLAUDE_BIN` replaces the program (split with `shell_words`, so an
  interpreter plus a script works) and suppresses the version warning, because a
  stand-in's version says nothing about the protocol. It wins on every backend,
  so under `linux_bwrap` whatever it names has to be reachable *inside* the
  sandbox -- under the worktree, or another bound path -- because the sandbox has
  its own `/home` and `/tmp`.

Input: each `agent.send` is first recorded as a `user_text` transcript event
(so history replay shows the user's side), then becomes a
`{"type":"user","message":{"role":"user","content":[{"type":"text","text":...}]}}`
line on stdin. Output lines are parsed into `AgentEvent`:

| stream-json | AgentEvent |
|---|---|
| `system` (init) | `system {subtype:"init", session_id, model, tools}`; session_id saved for resume |
| `stream_event` with text delta | `assistant_delta` |
| `assistant` message blocks | `assistant_text`, `tool_use` |
| `user` message with tool_result | `tool_result` |
| `control_request` (`can_use_tool`) | `permission_request`; state → `waiting_permission` |
| `result` | `result {cost, duration, turns}`; state → `idle` |

**State belongs to the reader.** The stdout reader is the only thing that
publishes an agent's state. `agent.send` and `agent.permission_reply` write a
line and record a transcript entry; what the agent is doing is whatever its own
output last said. A message line from an agent that was `idle` or `error` is
what moves it to `working`; a line that carries a state of its own (`init`,
`result`, `can_use_tool`) says it better and is left alone.

Only the conversation moving counts: an assistant message, a delta, a tool use
or result, a user line. Every line the parser cannot interpret is kept as
`system {subtype:"raw"}` so that nothing is lost, and a `system` line is about
the conversation rather than part of it, so neither is a resumption — one stray
line on the CLI's stdout must not leave an idle agent showing `working`.

This is why answering a permission request publishes nothing. One assistant
message can propose two tools, and the CLI then has two `can_use_tool` requests
outstanding at once; a `working` published on the answer to the first takes the
IDE's bar down over the second, which can then never be answered, and the CLI
waits for ever.

`agent.send` is refused in the two states where the CLI will not read the line.
An agent that has ended answers `AgentError` with
`data.reason = "agent_exited"`. An agent with a permission question still
outstanding answers `AgentError` with `data.reason = "waiting_permission"` —
counted from the open requests rather than the published state, which by the
rule above only moves when the reader sees a line: a user who answers the last
question and immediately types must not be turned away over a question that is
already settled. The exit is tested first, because a process that died with a
question open leaves it open for ever. Written anyway, the first turn vanishes
without a trace and the second comes back as a broken pipe.

A turn that ends with an `is_error` result and a process that then exits is
announced as `exited`, carrying the error's own message as the detail with the
exit code after it. An agent left in `error` is a live tab in the IDE for ever.

`permission_reply` writes a `control_response` line carrying allow (with optional
updated input) or deny (with message), and then records the answer in the
transcript as `system {subtype:"permission_reply", data:{request_id, decision}}`.
That record is what makes a replayed transcript agree with a live one: the
request is a message and comes back from disk, so without the answer beside it a
client that re-attaches raises its permission bar over a settled question and the
reply it then sends is a `NotFound`. `interrupt` writes a `control_request`
`interrupt` line if supported by the pinned version, otherwise sends SIGINT.
`stop` closes stdin, waits 5 s, then kills the process group. Both paths that
announce an exit — `stop`, and the reader when the process goes on its own —
wait up to 1 s for the stderr reader to reach end of input before they build the
exit detail. The tail is the useful half of that detail ("Invalid API key", "Not
logged in"), it is read by a task of its own, and the CLI hands its stderr to
every tool it runs, so the agent's last words are routinely still in flight when
the exit code lands. Bounded, because a killed grandchild can hold the write end
open indefinitely and an exit nobody announces is worse than one that cannot say
why.

**One exit, announced once, with its reason.** The two paths race, and a flag
decides which of them announces; the rules below are what keep that race from
deciding *what* the client hears.

- **The reader bounds its own pipe waits.** Once the process has exited, each
  stdout read gets 1 s of silence (`READER_DRAIN`, measured from the last line,
  so output already in the pipe is still read however slow the disk work
  between lines is), and then the 1 s stderr wait above. A grandchild holding
  stdout open costs the reader a second, not the exit.
- **Close, claim, publish, in that order.** Each path closes the agent's record
  (8.5) *before* it claims the announcement, and publishes with
  `AgentSink::publish_state`, which cannot yield. An abort or a cancellation
  therefore never lands between the claim and the publish — the case in which
  the flag is set, the event is never sent, and the other path stays quiet
  because it reads the flag as somebody else's announcement. `AgentSink::state`
  closes the record itself, and is not used on these paths for exactly that
  reason.
- **`stop` waits for the reader** for its two pipe bounds plus `RECORD_GRACE`
  (3 s) for its disk work — 5 s in all — and aborts it only then. The record
  close used to count against the pipe budget alone, so a disk busy with a
  parallel build decided whether the exit kept its reason or was announced at
  all. When `stop` does abort the reader it makes the stderr wait itself; when
  the reader finished normally it does not wait again.
- **Every record close on an exit path is bounded by `RECORD_GRACE`.** `stop`
  holds the adapter's lock and a workspace restart or destroy waits on it, so a
  disk that does not answer must not hold them. When the bound runs out the
  close stays queued and `Exited` is published anyway (8.5 says what a restart
  then finds).
- **Both paths build the same detail**: the `is_error` result's message with the
  exit code after it when the agent is in `error`, otherwise the stderr tail and
  the code. Which of them announces is timing; the reason is true either way.

The reader's session-id write is waited for too, up to 2 s
(`SESSION_RECORD_WAIT`, 8.5), before it reads the next line. Longer, and a disk
held up for seconds would keep the reader from the line saying why the turn
failed until `stop` had given up.

Unknown message types are stored verbatim as `system {subtype:"raw"}` so nothing
is lost when Claude Code adds message kinds.

Fixtures in `tests/fixtures/claude-stream/` drive the parser tests, and
`tests/fixtures/fake_claude.py` replays one of them in place of the real CLI for
the integration tests, so no suite needs a network or a login. As of Milestone 4
those fixtures are **synthetic**: written from the documented shapes, with every
flag verified against the real binary, but no line in them came out of a real
`claude`. `scripts/record-claude-stream.sh` records one real turn into
`recorded-<version>.ndjson` beside them; run it once from a login shell on a
logged-in machine and correct any synthetic shape that differs.

### 8.3 Credentials and home seeding
On workspace creation, `homes/<id>/` receives:
- `.claude/settings.json` copied from the daemon user's `~/.claude/settings.json`
  if present.
- `.claude/.credentials.json` copied if present (OAuth login), else
  `ANTHROPIC_API_KEY` is passed through from the daemon's environment or from
  `agent.start.options.api_key`.
- `.claude.json` **written rather than copied**: the daemon user's own file is
  parsed, `projects["<worktree path>"].hasTrustDialogAccepted` is set to true,
  and the result is written with the same 0600 mode the copy would have had.
  Every other key survives the merge — account, onboarding state, the user's
  other projects. Where the daemon user has no such file, or one that will not
  parse, the workspace gets a fresh object carrying only the trust entry: the
  CLI could not have read an unparseable file either, and the alternative is a
  workspace that stays untrusted for good.
- `.gitconfig` with `user.name`/`user.email` copied from the daemon user's config.

**Why the trust entry.** Claude Code keeps the answer to its trust dialog per
project directory, and reads a repository's `.claude/settings.json` — the
`permissions.allow` list a repo pins its agent's tools with — only for a project
that has been trusted. A sandbox home is new for every workspace and has never
seen this worktree, so without the entry the agent starts with the repository's
permissions dropped (`Ignoring N permissions.allow entries from
.claude/settings.json: this workspace has not been trusted`) and nobody can
accept the dialog: the agent runs non-interactively, and the daemon created the
directory in the first place. The key is the worktree path **as the agent sees
it**, which is the path it has on the host: the sandbox binds the worktree at the
same path and starts the CLI there (6.2).

The spec accepts that the agent can read these credentials; the sandbox protects
the host, not the credentials. A `.claude/settings.json` supplied via
`bondsymphonic.toml [claude] settings = "path"` overrides the copy, which is how a
repo can pin allowed tools.

The override is applied at every `agent.start`, after the seeding it replaces,
and the path is read from the *worktree's* copy of `bondsymphonic.toml` rather
than the source repository's: the workspace is a checkout of a branch of its
own. The path is repo-relative and resolved with the file service's containment
rule (11), so an absolute path, a `..`, and a symlink pointing out are all
`InvalidParams` on the start. So is a file that is not there, and so is a
`bondsymphonic.toml` that will not parse: each of those would leave the agent
running under settings nobody chose, which for a setting whose job is to pin
allowed tools is worse than not starting at all.

**Every write into `homes/<id>` de-symlinks its own path.** The home is bound
into the sandbox read-write as `$HOME` (§6.2), so between one `agent.start` and
the next the agent owns every name under it and can leave a symlink to any host
path where `.claude` was. The target need not exist inside the namespace: only
the link text survives to the host, where the daemon resolves it. What the
settings override then writes is *the agent's own bytes*, and Claude Code
settings carry `hooks`, which are shell commands — so following such a link is
code execution as the daemon user the next time they run `claude` themselves.
The rule, applied by both writers (the settings override and the credential
seeding of §8.3):

- Each directory on the way in is checked with `symlink_metadata`, which does not
  follow. Anything that is not a real directory is unlinked — removing the link,
  never its target — and a real directory is created in its place, with a warning
  in the log.
- The destination file is unlinked and then created with `create_new`, so a link
  raced back in between the two is refused rather than followed. Never
  `fs::copy`, which opens the destination by path and follows what it finds.
- The source is opened **once**, `O_NOFOLLOW` where the platform has it, and every
  later read is of that handle. `fs::resolve` proves the path is inside the
  worktree; re-opening it afterwards would let an agent swap a regular file for a
  symlink in between and have the link followed.
- A real *directory* found at a destination file is the one obstruction that is
  left alone: it cannot redirect a write, and removing it recursively would be
  the daemon deleting data it did not put there. The copy fails, which the
  seeding logs and skips and the settings override reports as `IoError`.

### 8.4 Terminal adapter
A terminal agent is a PTY, not an entry in the agent registry: the IDE calls
`pty.open` directly with the configured command (default `$SHELL -l`, or e.g.
`codex`) inside the sandbox, and never `agent.start`. Events are raw
`pty.output`; input is `pty.write`; the process dying is `pty.exit`. There is no
`AgentAdapter` implementation behind it, no transcript file and no `agent.state`,
which is why a terminal tab's status follows its workspace rather than an agent.
`AgentAdapterKind::Terminal` exists in the protocol and in `capabilities.adapters`
only to name the choice in the New Agent dialog; `agent.start` refuses it with
`InvalidParams("terminal agents use pty.open")`.

### 8.5 Agent records
`agents.json` under the data root, beside `workspaces.json`, holds one record per
agent the daemon has started: `{agent_id, workspace_id, adapter, session_id,
options, started_at, ended_at}`. `options` never carries `api_key` — the key is
given to one process and is not state to keep. The file is rewritten whole
through a temporary and a rename, when an agent starts, when the CLI reports its
session id, and when the process ends.

**How the writes happen.** Every write ends in an `fsync`, so none of them runs
on a tokio worker. The start's record, and its removal after a failed spawn, are
done by the start itself on the blocking pool and awaited. A running agent's
writes — its session id, and the record's close — go through one ordered queue
per records file, worked by a thread of its own, so the last session id queued
is the one the file ends up with:

- **The session id** is queued and waited for up to 2 s
  (`SESSION_RECORD_WAIT`) before the reader reads on. Once a client has seen
  anything after the `init` line, the id is on disk — on any disk that answers
  within that bound. On one that does not, the write stays queued and the
  reader goes on, because the alternative is an exit that cannot say why the
  turn failed (8.2).
- **The close** is written once: the first `AgentSink::ended` queues it with the
  time the process ended and waits for it; later calls return at once, even
  while that write is still going. The exit paths bound that wait (8.2), and a
  close that outlasts the bound is still written when the disk comes back. A
  daemon that goes before then finds the record open at the next start, and
  restores the agent as one that ended when the daemon restarted.
- **On the way out**, `main` flushes the queue, bounded to 5 s, as soon as the
  server has stopped and before any sandbox is shut down: the IDE gives the
  whole exit five seconds before it kills the relay, and the queue does not
  outlive the process. A crash loses whatever is still queued, which on a disk
  that answers is at most the one write in flight.

The record is written **before** the process is spawned, and removed again if
the spawn fails. Writing it afterwards leaves a window in which the CLI's `init`
line — and therefore the session id `--resume` needs — arrives before there is
anything to record it against, and the id is then lost for good. Ordering the
two this way closes the window structurally rather than by timing: the stdout
reader cannot exist before the record does.

Without it a transcript survives a restart but is unreachable: `agent.history`
and `WorkspaceInfo.agents` both come from the live agent map, and a restart kills
every sandbox. On startup `AgentManager::restore` reads the records and puts each
one back as an agent with no process — state `Exited`, detail naming the restart.
It runs to completion *before the accept loop starts*, since the ids it puts in
the map are what a new agent's id is minted against; restarting the sandboxes,
which takes seconds, runs alongside the accept loop instead.
Its `agent.history` still reads the transcript, `WorkspaceInfo.agents` still
lists it in its original order with its adapter and start options, and
`agent.send`, `agent.permission_reply` and `agent.interrupt` answer `NotFound`
pointing at `resume_session`; `agent.stop`
answers `Ok`. A record found open belonged to an agent that was still running
when the daemon went, and restoring is what closes it. The conversation continues
through a *new* agent started with `options.resume_session` set to the recorded
session id, which is what the CLI's `--resume` needs.

A file that will not parse is moved aside as `agents.json.corrupt` and read as
empty: the workspaces live in a different file and must still come back. If it
cannot even be moved aside, it is *unreadable* rather than empty, and this
daemon never writes the file again — overwriting records it could not read would
lose agents that a later run, or a person, could still recover.
`workspace.destroy` removes the workspace's records and their transcripts, and so
does a restore that finds a record whose workspace the registry no longer has.

## 9. PTY
`portable-pty` (Rust) opens a pty pair; the slave is handed to the sandboxed
child. Output is read in 4 KiB chunks and emitted as `pty.output` base64.
Resize forwards `TIOCSWINSZ`. Idle PTYs cost one task each.

**Closing a workspace's PTYs escalates.** The backend's own killer is
`portable-pty`'s bare SIGHUP, which never goes further, so a command that traps
it survived `workspace.destroy` and kept running while the worktree was deleted
under it. `close_workspace` fires the killer, waits one signal grace, and then
sends SIGTERM and SIGKILL to the process group of anything still registered. The
ladder runs in a task of its own per victim and `close_workspace` returns as soon
as every terminal has been *asked*, so `workspace.destroy` goes straight on to
remove the worktree with the last terminal possibly still dying. That is
deliberate: it is the difference between a destroy that answers at once and one
that spends a second per open terminal doing nothing, the ladder is what
guarantees the processes go, a worktree removal is not blocked by a process whose
working directory is inside it, and nothing the destroy does afterwards needs
them gone first. A caller that has to know a workspace's terminals are really
gone watches for their `pty.exit` events; the call returning is not that promise.
Closing the daemon's host PTYs is the other way round and *does* await its grace,
because it runs on the way out of the process, where a task nobody waits for is a
task that never runs.

**A host PTY dies with the connection that opened it.** Setup terminals
(`system.setup_pty`) run on the host, outside any workspace, so nothing else
would ever end them: the connection carries a cancellation token, fired when its
reader stops, and each host session it opened is closed on it. `pty.close`
accepts a host PTY id like any other — host terminals are adopted into the same
session map — so the IDE can also close one itself.

## 10. Runs

### 10.1 `bondsymphonic.toml`

```toml
[[run]]
name = "web"
command = "npm run dev -- --port 3000"
port = 3000
cwd = "."               # optional, relative to worktree (an absolute path, or
                        # one climbing out with "..", is InvalidParams; so is
                        # a Windows drive-relative "C:secret" — see below)
env = { NODE_ENV = "development" }
ready_regex = "Local:.*http"   # optional, marks state "ready"

[[run]]
name = "api"
command = "cargo run -p api"
port = 8080

[network]
allow = ["*.mycompany.com"]

[claude]
settings = ".claude/settings.json"   # optional
```

**A bad `[[run]]` block costs that block and nothing else.** `name`, `command`
and a non-zero `port` are what a run cannot do without, and they are checked per
entry rather than by the parser: a block that is missing one of them, or that
repeats a name an earlier block used, is dropped and reported as one line in
`repo.detect_run_configs`'s `warnings`, while every other block loads. Requiring
them at the parse level made one forgotten line discard the whole file and answer
with detection instead, with the reason only in the daemon log.

The same list carries the parse error of a file that will not parse at all, which
is still a fall back to detection. `warnings` is for people: the IDE shows it
beside the run list, and nothing in the daemon reads it. `run.start` sees only
the entries that survived, so a name that was dropped is `NotFound` like any
other name the repo never declared.

**`cwd` is read as text before any host's path parser sees it.** The file is
written on the machine the IDE runs on and read by the daemon in the distro,
where `C:secret` is an ordinary directory name and nothing in `Path` finds
anything wrong with it — while on Windows it is the *drive-relative* form: no
root, so it counts as relative, and joining it to the worktree throws the base
away and leaves the command wherever that drive's current directory happens to
point. A leading `/` or `\`, a `<letter>:` prefix, and a `..` between either
separator are all refused outright, and the host's own reading of the path is
then applied on top for the spellings that text check does not name.

### 10.2 Auto-detection (when no file, or `repo.detect_run_configs` asks)
Ordered heuristics, each yielding `RunConfig {name, command, port, source:
"detected"}`:
- `package.json` scripts `dev`, `start`, `serve` → `npm run <script>` (or `pnpm`/
  `yarn` if the lockfile says so); port guessed from `vite.config.*`, `next` (3000),
  `angular.json` (4200), or 3000. A `vite.config.*` sets the port of the scripts
  that actually run Vite's **dev server**, and of no others: `vite` as the
  program a script's command invokes, directly, through a package runner
  (`npx`, `npm`, `pnpm`, `yarn`, `bun`), or behind `cross-env` or a leading
  environment assignment, and with no subcommand or `dev`/`serve`. `vite
  preview` serves the built site and takes 4173 as a guess — the config's
  `server` block is the dev server's port, not its — and `vite build` is a
  compiler that binds nothing, so it takes the same generic guess as any other
  script. Every script that is not the dev server keeps its framework default
  with `port_guessed` set, which is what makes the Run panel offer to correct
  it. The rule used to be "a `vite.config.*` exists in the repository", which
  reported the `start` that runs `node server.js` beside a Vite front end as
  serving on 5173, with `port_guessed` cleared so nobody could fix it.
- `docker-compose.yml` → not runnable in v1 (no Docker in sandbox); listed with
  `disabled_reason`.
- `Cargo.toml` with a `[[bin]]` or `axum`/`actix`/`rocket` dependency →
  `cargo run`, port 8080 guess.
- `manage.py` → `python manage.py runserver 0.0.0.0:8000`, port 8000.
- `pyproject.toml` with `fastapi`/`flask` → `uvicorn`/`flask run`, port 8000/5000.

Guessed ports are flagged `port_guessed: true`, which is what lets the IDE offer
the port for editing: a configured port is the repository's and is shown
read-only, a guessed one is the daemon's and can be replaced per start through
`run.start`'s `port` (10.3). A port pinned for good still belongs in
`bondsymphonic.toml`.

### 10.3 Manager
`run.start` spawns the command through the sandbox with the run's env, plus
`PORT=<port>` and `HOST=0.0.0.0`, sets up the bridge (7.3), and streams stdout/
stderr lines as `run.output`. `<port>` is `params.port` when the request carries
one and the configuration's port otherwise; the override reaches the `PORT`
variable, the bridge and the readiness probe alike, so nothing in the run is left
pointing at the port that was replaced. It applies to that start only — nothing
is written back to `bondsymphonic.toml`, which is the repository's — and `port:
0` is `InvalidParams`, since a run whose port nobody knows cannot be bridged,
probed or opened. One run per configuration is unchanged: the claim is keyed by
the configuration's name, not by the port. `PORT` and `HOST` are pushed after the config's own
env, so a repository cannot quietly redefine the two variables every run is
promised. State: `starting` → `ready` (regex match, else the forwarder's status
byte, polled every 500 ms) → `stopped`/`failed`. A configuration that sets
`ready_regex` is saying the port alone is not good enough, so it is never made
ready by a probe. `run.stop` sends SIGTERM to the process group, SIGKILL after
5 s, and tears down the bridge; `workspace.destroy` stops every run first.

**How a run's end is reported.** There are two terminal states and three
endings. A run the user ended reports `stopped` with **no** detail, whatever exit
status the signal produced and whichever of the supervisor and the stop observes
the exit first: a stop needs no explanation, and the exit status of a signalled
process says nothing useful. A run whose workspace is going is the same thing
from further away and is announced the same way. That is decided by reading the
registry fresh at the moment the exit is announced, rather than from anything the
start was holding, so a run that dies because the worktree went out from under it
or because init took the sandbox down is not reported as a failure to a client
that has already been told the workspace is gone. Without that read, the "no
`failed` after a destroy" rule was only ever a matter of timing.

A run that died on its own reports `failed` if it never became ready, and
`stopped` if it had; both carry the exit code and the last lines of output as
their detail. So the presence of the detail is what separates the two `stopped`
cases over the wire, and a client must not read a bare `stopped` as a failure.
The detail is assembled *after* the output readers are drained, because a command
that prints an error and exits usually delivers its exit code before the daemon
has read its pipes.

**Readiness by port** is a connection that is accepted on either loopback —
`127.0.0.1` or `::1` — with nothing sent. Both are asked at once: a dev server
that binds `localhost` binds whichever address its runtime resolves first, and
on a current host that is `::1`.

**Starting a run reads the workspace's state twice**, once before it does
anything and once after its run is in the list. A `workspace.destroy` marks the
workspace and then sweeps its runs, so a start that registered its run after that
sweep would otherwise outlive the workspace. On the second read the start tears
its own run down and answers `InvalidParams` with `data.reason`
`workspace_not_ready` — the same `data.reason` the first read gives, from the
same constant, so a client telling "the workspace is going away" from "no such
run configuration" never has to read English to do it. Neither refusal announces
anything: the `starting` event is published only after the second read, and the
output readers check the run's finished flag before they publish a line, so a
start that loses to a destroy is silent rather than half-announced.

**A `(workspace, config)` pair is spoken for** from before its process is spawned
until after its teardown is complete. A `run.start` for a pair whose run is still
being stopped is refused with `Conflict` and `data.reason` `run_stopping`, rather
than being handed a port the dying process has not let go of; one whose run is up
is refused with `Conflict` and `data.reason` `run_running`. The claim is released
before the terminal event is published, on both the path a stop takes and the one
a death takes, so a client that reacts to `stopped` by starting the same
configuration again is not told `run_stopping` for a run the daemon has just said
is over. The stopping flag is set while the run list's lock is still held, so
there is no window in which a run is out of the list but not yet marked.

**The noop backend has no bridge.** Without a network namespace the run is a
plain child of the daemon and its port already is the host's, so `host_port` is
the configuration's own port, the URL is `http://localhost:<port>`, no
`fwd-<run_id>.sock` and no in-sandbox forwarder exist, and readiness is the direct
TCP connect above — both loopbacks at `<port>` — on the same 500 ms tick. This is the path Windows
development takes, and the one the daemon's `run_integration` suite exercises on
both hosts; the bridge path is covered by `sandbox_integration` under bwrap.
A client must therefore take the URL from `run.start`'s reply or the `ready`
event and never rebuild it from the configuration's port: under bwrap the two
differ, and the host port changes on every start.

## 11. File service
- **Containment is a walk, not a resolved path.** A path that is checked and
  then opened is a path the agent can swap underneath the daemon: it owns the
  worktree, and a directory replaced by a symlink between the two steps is a
  read or a write outside the worktree. On unix the service never resolves to a
  path at all. It walks from an `O_RDONLY|O_DIRECTORY|O_CLOEXEC` handle on the
  root, each component an `openat` with `O_NOFOLLOW|O_DIRECTORY`, so the kernel
  refuses a symlink at the moment of the open; `read_file` opens with
  `O_NOFOLLOW`, `write_file` creates its temporary with
  `O_CREAT|O_EXCL|O_NOFOLLOW` in the final directory handle and `renameat`s
  within that same handle, and `list_dir` reads through `fdopendir` on the
  handle and stats entries with `fstatat`. A path that really does climb out of
  the root is `InvalidParams` naming the escape. `resolve` — a path, not a
  handle — remains for the callers that genuinely need one: git invocations, and
  the Windows service.
- **A symlink is refused as a symlink, with its own message**, and is never
  listed as a folder. The walk refuses *every* link, in-tree ones included,
  because telling in from out means following it, which is the thing it must not
  do; so most refusals are not escapes, and answering "path escapes the
  worktree" to somebody's own `docs -> shared/docs` was a false accusation.
  `list_dir` stats with `AT_SYMLINK_NOFOLLOW`, so a link is listed — it is
  really there — but as a plain entry of size 0, never as a directory the
  Explorer would render and then fail to open; the same `lstat` absorbs the
  dangling case. Following a link safely means resolving its target through the
  same walk, which is `openat2(RESOLVE_BENEATH)` on Linux 5.6+: the noted
  upgrade path. What the walk pins is the inode and not the path, so a directory
  renamed *out* of the worktree mid-request is still written into — not a
  containment failure, and not something `RESOLVE_BENEATH` would change either.
- `read_file` returns UTF-8 text, or `encoding: "binary"` with no content for
  non-UTF-8 files, truncated above 4 MiB with `truncated: true`.
- `write_file` writes atomically (temp + rename) and carries over the mode of
  the file it replaces. Content longer than the 4 MiB `read_file` would ever
  return is `InvalidParams` naming the limit, refused before any file is opened:
  content that JSON-escapes past the 8 MiB frame cap used to kill the connection
  with no reply at all.
- `fs.watch` uses `notify` with 200 ms debouncing; emits relative paths; ignores
  `.git/`, `node_modules/`, `target/`. **Ignored directories are not watched,
  only-filtered-afterwards being the bug this replaces**: a recursive watch
  installs one inotify watch per directory, so a JS worktree's `node_modules`
  spent the user's whole `max_user_watches` allowance and the next `fs.watch`
  for any workspace failed. On Linux the root is watched non-recursively and a
  walk of the daemon's own adds one non-recursive watch per directory, never
  entering an ignored name and never following a symlink. A directory that
  appears later is watched when its creation is reported, and the walk that
  watches it reports what it found, so writes that beat the watch into place are
  not lost. Elsewhere the platform watcher is recursive natively and the tree is
  watched whole. Enabling a watch builds the watcher and walks the tree with the
  registry lock released, so a large worktree does not queue every other
  workspace's `fs.watch` behind it.

## 12. Prerequisite checks
`check_prereqs` returns, in order: `git ≥ 2.40`, `bwrap` present, user namespaces
usable (spawn `bwrap --ro-bind / / --unshare-all true`), `--userns` support,
`claude` present + version, `claude` authenticated (`~/.claude/.credentials.json`
or API key present), `gh` present, `gh auth status` ok. Each has `fix_hint` with a
shell command.

## 13. Testing (daemon-specific)
- Unit: allowlist matching; run config parsing and detection on fixture trees;
  stream-json parsing on recorded fixtures; registry load/save; path containment.
- Integration (`tests/`, Linux only, `#[cfg(target_os = "linux")]`):
  - `workspace_integration`: temp repo → create → commit into the worktree with
    the environment the sandbox gives git (`GIT_OBJECT_DIRECTORY` and
    `GIT_ALTERNATE_OBJECT_DIRECTORIES`), which is what decides where the objects
    land and what lets this chain run under the noop backend and on Windows too;
    the sandbox's *own* git is exercised by `sandbox_integration` below →
    `changes` lists it → `merge` succeeds and base contains it →
    the objects were **absorbed** (5.2), so the base branch reads through a git
    carrying no `GIT_ALTERNATE_OBJECT_DIRECTORIES` → destroy cleans everything:
    worktree, private objects, home, cache, run directory, agent records and
    transcripts. One test walks the whole chain and asserts each directory is
    gone. (Earlier drafts said "`repack` made objects visible"; `repack -a -d` is
    not what the daemon does, and 5.2 says why.)
  - `sandbox_integration`: sandboxed `touch /etc/x` fails; `touch $HOME/x`
    succeeds; `git update-ref refs/heads/main <sha>` inside fails;
    `git update-ref refs/heads/bs/<name>/work <sha>` inside succeeds.
  - `network_integration`: a local TCP server on the host is unreachable directly
    from the sandbox; reachable via proxy only when allowlisted. The third case —
    a `python3 -m http.server` inside the sandbox reachable on the bridged host
    port — lives in `sandbox_integration` beside the rest of the bridge tests,
    because it needs a real workspace and a real run rather than a bare sandbox.
  - `instance_lock`: a second daemon on one data directory exits 2 with the
    message of 3, and the directory is free again once the first one is gone.
- Tests skip with a clear message (not fail) when `bwrap` is unavailable, so
  `cargo test` on a bare CI box still passes the non-sandbox tests.
