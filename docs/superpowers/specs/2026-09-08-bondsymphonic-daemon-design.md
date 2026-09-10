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

**Create** (`workspace.create`):
1. Validate repo (`git rev-parse --git-common-dir`), base branch exists. With
   `init_if_missing`, a path that is not a repository is initialised here
   instead of failing (see below).
2. Compute branch `bs/<name>/work`; fail with `Conflict` if it exists.
3. Pre-create the writable ref directories (5.2).
4. `git worktree add -b bs/<name>/work <worktree_path> <base_branch>`.
5. Create `homes/<id>` seeded with Claude credentials (8.3) and `caches/<id>`.
6. Build the `SandboxSpec` and start the sandbox supervisor (6).
7. Persist to registry, emit `workspace.state`.

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
so the naive question is really about the nearest enclosing repository. Both
callers ask about the path itself instead, by comparing `rev-parse
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
--force`; `git branch -D bs/<name>/work`; delete `homes/`, `caches/`, transcripts;
remove from registry. With `force=false`, refuse if the worktree has uncommitted
changes or unmerged commits and return `Conflict` with details.

**Registry** is rewritten atomically after every change, and so is `agents.json`
(8.5): the bytes go to a sibling temporary whose name is unique per call
(`.<file>.<pid>.<n>.tmp`), are flushed to the device with `sync_all`, and only
then replace the file with a rename, with a best-effort fsync of the directory
afterwards on Unix. The unique name matters as much as the rename: one fixed
`<file>.tmp` is shared by every writer and by every earlier run of the daemon, so
two saves at once can rename each other's half-written file into place, and one
leftover at that name wedges the writer for good.
On startup, each registered workspace is validated: if the worktree directory or
the branch is gone, the workspace is marked `Error` rather than deleted, so the
user can decide.

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
which `git push -u` leaves behind — unlike the local workspace branch, that
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
the `git branch -D` on the way out fires `reference-transaction` again. Neither
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
  (`git diff --name-only --diff-filter=U`) as much as to the lists here.

### 5.4 Merge, rebase, squash
Run in the main repo by the daemon, never inside a sandbox:
- Serialised per repository: two merges, or a merge and a push, of the same
  repository never run at once, because both move the same base branch.
- Where it lands: in the user's own checkout when that is on the base branch,
  and otherwise in a temporary worktree of the base under
  `~/.bondsymphonic/merge-<id>`, removed on every exit path. Scratch worktrees an
  earlier run left behind (a killed daemon) are reaped at the start of each
  merge, under the same lock, so anything still there belongs to nobody.
- Guard: the working tree must be clean, **and it is only checked on the first
  of those two paths** — when the merge will land in the user's own checkout.
  Otherwise the user's checkout is not involved and is not inspected. A dirty
  base is `Conflict {reason: "base_dirty"}` with a message naming the repository
  and the base branch; `--porcelain` counts untracked files, because `git merge`
  refuses when an untracked file would be overwritten.
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

### 5.5 PR
`git push -u origin bs/<name>/work` then `gh pr create --title --body [--draft]
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
- `--ro-bind / /` as the base, then `--tmpfs /tmp`, `--proc /proc`, `--dev /dev`.
- `--tmpfs /home`, then `--bind homes/<id> /home/<user>`.
- `--bind worktree_path worktree_path` (same path inside so git paths match).
- `--bind objects-<id> objects-<id>`.
- The three writable `.git` subpaths from 5.2 (`--ro-bind <repo>/.git` first,
  then the rw binds on top).
- `--bind caches/<id> /home/<user>/.cache`.
- `--bind ~/.bondsymphonic/run/<id> /run/bs` (exec, proxy, and forward sockets).
- `--tmpfs /opt`, then `--ro-bind <resolved claude> /opt/bs/claude`: the mount
  point cannot be created under the read-only root, and an empty `/opt` also
  keeps host-installed third-party software out of a workspace. See 8.2 for how
  the host path is resolved.
- `--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup`
  and `--unshare-net` (6.3). `--die-with-parent`, `--new-session`.
- No `/mnt/c` visibility unless the repo lives there, in which case only the
  repo's `.git` and the worktree binds are visible.

### 6.3 Sandbox init (one bwrap per workspace)
bubblewrap cannot join an existing network namespace, so the daemon runs exactly
one `bwrap` per workspace and does all process management through a small init
inside it:

```
bwrap <mount and unshare flags> --die-with-parent --new-session \
      -- bondsymphonic-daemon sandbox-init --socket /run/bs/exec.sock
```

`sandbox-init` is PID 1 inside the sandbox. It:
- listens on the Unix socket `/run/bs/exec.sock` (host path
  `~/.bondsymphonic/run/<id>/exec.sock`, rw-bound as `/run/bs`);
- accepts spawn requests (argv, env, cwd, optional pty size) from the daemon;
  for each it forks the child inside the sandbox, and passes the child's stdio
  pipes or the PTY master back to the daemon over the socket with `SCM_RIGHTS`,
  so the daemon reads and writes those fds directly with no proxying;
- reports exits (pid, code) on the socket and reaps zombies;
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
`*.anthropic.com`, `registry.npmjs.org`, `*.npmjs.org`, `pypi.org`,
`files.pythonhosted.org`, `crates.io`, `static.crates.io`, `index.crates.io`,
`github.com`, `*.github.com`, `*.githubusercontent.com`. The repo's
`bondsymphonic.toml` `[network] allow = [...]` extends it; `workspace.set_allowlist`
overrides it at runtime (at most 256 entries of at most 253 bytes each). A
wildcard must leave a registrable name behind it, so `*.example.com` is a pattern
and `*.com` is refused.

A repository extends the allowlist at creation without anyone necessarily having
read it, so the list is a list of *names* the user may not have chosen. Two rules
follow, and both are the boundary rather than hygiene:

- **The address, not the name, is what is allowed.** After resolving, any address
  that is loopback, link-local (`169.254/16`, `fe80::/10` — this is where the
  cloud metadata endpoint lives), private (`10/8`, `172.16/12`, `192.168/16`),
  unique-local (`fc00::/7`), unspecified or multicast is dropped, in either
  address family and through the IPv4-mapped form. If nothing is left the request
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

The proxy implements HTTP `CONNECT` (for TLS) and plain absolute-URI `GET/POST`
forwarding. It checks the target host against the allowlist and then its
resolved addresses against the address rules of 7.1 before connecting, and
answers `403` with a body naming the host and either the config key to add it or
the reason the destination is refused. Every denial is emitted as
`daemon.log {level: warn}` so the IDE can surface it, coalesced per host per
workspace as 7.1 says.

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
   socket.

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
`TESTED_CLAUDE_VERSION = "2.1.263"` and warns once per daemon lifetime when the
installed `claude --version` differs. Every flag above was verified accepted by
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

`permission_reply` writes a `control_response` line carrying allow (with optional
updated input) or deny (with message), and then records the answer in the
transcript as `system {subtype:"permission_reply", data:{request_id, decision}}`.
That record is what makes a replayed transcript agree with a live one: the
request is a message and comes back from disk, so without the answer beside it a
client that re-attaches raises its permission bar over a settled question and the
reply it then sends is a `NotFound`. `interrupt` writes a `control_request`
`interrupt` line if supported by the pinned version, otherwise sends SIGINT.
`stop` closes stdin, waits 5 s, then kills the process group.

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

## 10. Runs

### 10.1 `bondsymphonic.toml`

```toml
[[run]]
name = "web"
command = "npm run dev -- --port 3000"
port = 3000
cwd = "."               # optional, relative to worktree (an absolute path, or
                        # one climbing out with "..", is InvalidParams)
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

### 10.2 Auto-detection (when no file, or `repo.detect_run_configs` asks)
Ordered heuristics, each yielding `RunConfig {name, command, port, source:
"detected"}`:
- `package.json` scripts `dev`, `start`, `serve` → `npm run <script>` (or `pnpm`/
  `yarn` if the lockfile says so); port guessed from `vite.config.*`, `next` (3000),
  `angular.json` (4200), or 3000.
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

**The noop backend has no bridge.** Without a network namespace the run is a
plain child of the daemon and its port already is the host's, so `host_port` is
the configuration's own port, the URL is `http://localhost:<port>`, no
`fwd-<run_id>.sock` and no in-sandbox forwarder exist, and readiness is a direct TCP
connect to `127.0.0.1:<port>` on the same 500 ms tick. This is the path Windows
development takes, and the one the daemon's `run_integration` suite exercises on
both hosts; the bridge path is covered by `sandbox_integration` under bwrap.
A client must therefore take the URL from `run.start`'s reply or the `ready`
event and never rebuild it from the configuration's port: under bwrap the two
differ, and the host port changes on every start.

## 11. File service
- Paths are joined to the worktree root and canonicalised; anything escaping the
  root (including via symlink) returns `InvalidParams`.
- `read_file` returns UTF-8 text, or `encoding: "binary"` with no content for
  non-UTF-8 files, truncated above 4 MiB with `truncated: true`.
- `write_file` writes atomically (temp + rename) and preserves mode.
- `fs.watch` uses `notify` with 200 ms debouncing; emits relative paths; ignores
  `.git/`, `node_modules/`, `target/`.

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
