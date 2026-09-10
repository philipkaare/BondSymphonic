# BondSymphonic user guide

BondSymphonic runs several coding agents at once, each in its own git worktree
inside its own sandbox, and gives you one window to watch them, edit what they
wrote, run their web apps and land their work.

This guide is the whole product as it stands today. The README is the short
version; `docs/superpowers/specs/` is the design behind it.

---

## Install

### From a package

You were handed a `BondSymphonic-<version>-win64.zip`.

```powershell
Expand-Archive BondSymphonic-0.1.0-win64.zip -DestinationPath C:\Tools
Get-ChildItem C:\Tools\BondSymphonic -Recurse | Unblock-File
powershell -ExecutionPolicy Bypass -File C:\Tools\BondSymphonic\install.ps1
C:\Tools\BondSymphonic\bondsymphonic-ide.exe
```

**The two middle lines are not optional on a clean machine.** Everything
extracted from a downloaded zip carries the mark of the web: Windows blocks the
scripts and can refuse to load the DLLs. `Unblock-File` clears that from the
whole folder. And the default PowerShell execution policy is `Restricted`, which
will not run `install.ps1` at all, so it is started with `-ExecutionPolicy
Bypass` for that one command rather than by changing the machine's policy.

`install.ps1` creates the `bondsymphonic` WSL2 distro if it is not already
there — Ubuntu 24.04 with git, bubblewrap, python3, socat, Claude Code and the
GitHub CLI, and deliberately **no Rust toolchain**, because the daemon ships in
the folder as a compiled binary. It then runs the runtime provisioning over the
distro and starts the IDE.

It runs that provisioning on **every** invocation, not only the first. That is
deliberate: a first setup interrupted half way leaves a distro that exists but is
missing packages, and re-running the installer is how you repair it. Every step
is idempotent, so a machine that is already set up ends up unchanged.

**Close the IDE before you re-run it.** Provisioning finishes with
`wsl --terminate bondsymphonic`, which is what makes the distro's default user
take effect — and it shuts the whole distro down. Every sandbox goes with it, so
your terminals stop, your runs stop and your agents die. The IDE notices the
daemon has gone and relaunches it, but nothing that was running inside comes
back; terminals offer **Reopen** and runs have to be started again. Quit the IDE
first and none of that arises.

Idempotent is not the same as quick, either. A second run still works through the
apt packages and checks whether Claude Code is installed, so **expect it to take
minutes even when it has nothing to do**. Re-run it to repair a distro you
suspect is half-provisioned, not as a way to start the IDE — for that, run
`bondsymphonic-ide.exe` directly. `-WhatIf` prints the plan without touching
anything; `-NoStart` provisions without launching.

WSL2 itself is the one prerequisite the package cannot install for you. If
`wsl --version` does not answer, run `wsl --install` from an elevated PowerShell
and reboot before you start.

The first provisioning downloads about a gigabyte. Signing in to Claude Code and
GitHub happens **afterwards, inside the IDE**, under File > Settings… > Setup —
never by typing a login command into a terminal yourself.

To check that a package unzipped intact:

```powershell
C:\Tools\BondSymphonic\bondsymphonic-ide.exe --version
# bondsymphonic-ide 0.1.0 (protocol 1)
```

It answers before a window or a daemon is started, so it needs no display, no Qt
platform plugin and no WSL. It is not a test of nothing, though: the exe imports
the Qt and Visual C++ runtime DLLs at load time, so a package missing one of
those fails here rather than printing a version. That is what makes it the
quickest check that an unzip was complete and unblocked.

### From a source checkout

```powershell
.\launch.ps1
```

One script: it installs the Windows toolchain through an elevated window,
creates the `bondsymphonic` distro, builds the Linux daemon inside it, and
starts the IDE, which installs the daemon into the distro and connects to it.
The first run downloads several gigabytes. Switches are `-Debug` (debug daemon),
`-Release` (release IDE), `-SkipDaemon` (reuse the last daemon binary) and
`-NoSetup` (fail instead of installing anything).

To build a distributable package from a checkout: `. .\scripts\env.ps1` then
`.\scripts\package.ps1`.

### What provisioning trusts

Provisioning the distro runs two vendor install scripts straight from the
network, and this is a deliberate choice rather than an oversight, so it is
written down here for you to weigh:

- `curl -fsSL https://claude.ai/install.sh | bash` runs on **every**
  provisioning, the packaged `install.ps1` path included. It installs Claude
  Code.
- `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` runs only in
  developer mode, where a checkout has to build the daemon.

Both are the vendors' own documented install commands, fetched over TLS. Neither
pins a version and neither checks a published checksum, so each provisioning
installs whatever those endpoints serve at that moment. The `bs` user inside the
distro has passwordless `sudo`, so anything either script installs can reach root
in the distro. That distro is a full WSL environment: it can read the Windows
drives under `/mnt`, and it is not the bubblewrap sandbox your agents run inside.

If that is more trust than you want to extend, provision the distro yourself and
install Claude Code from a version you have pinned and verified; the IDE only
needs `claude` on the `bs` user's `PATH`.

---

## First run and Settings > Setup

Setup lives in **File > Settings…**, in the **Setup** section at the top of the
dialog. It opens by itself when something is missing that would stop a workspace
being created — once, so you can close it and carry on even with the problem
unfixed — and the **Set up…** link in the status bar goes there whenever a check
is failing. If the same problem is still there later, or a new one appears, it
opens by itself again. It is where logging in to Claude Code and to GitHub
happens; there is no Help > Setup any more, because a login is a setting you
come back to when a token expires.

The section lists eight checks, each with a tick or a cross and the detail
behind it:

| Check | What it means |
| --- | --- |
| `git` | git 2.40 or newer inside the distro |
| `bwrap` | bubblewrap, the sandbox |
| `userns` | unprivileged user namespaces actually work |
| `claude` | the Claude Code CLI is installed |
| `claude_auth` | you are logged in to Claude Code |
| `gh` | the GitHub CLI is installed |
| `gh_auth` | you are logged in to GitHub |
| `sandbox` | the backend the daemon started with is usable |

**Log in here, not in a terminal.** A failing check the IDE can fix carries a
button — **Install Claude Code**, **Log in to Claude Code**, **Install GitHub
CLI**, **Log in to GitHub**. Pressing it runs that command in a terminal pane in
the dialog, on the host rather than inside a sandbox, because a login has to
write to your home directory. When the command exits the checks re-run.

**The sign-in link.** `claude auth login` prints a URL and then waits for the
code the browser gives you. The IDE opens that URL in your browser for you, and
a **Sign-in link** row appears under the terminal with the URL on it, a **Copy**
button and an **Open in browser** button; clicking the URL itself does both.
Use the row when the automatic open did not work, or when you want to finish the
sign-in on another machine. The row goes away when the terminal exits, because
the URL it carried is spent.

**Keep Settings open until the login terminal finishes.** Closing the dialog
ends the `claude auth login` process it was running, so a sign-in half way
through is abandoned. Closing it after a successful login is safe: the checks
re-run when the dialog closes, so the ticks and the chat box catch up either
way.

**Re-check** runs the checks again, and so does closing the dialog. The four
*blocking* checks are `git`, `bwrap`, `userns` and `sandbox`: without them there
is no worktree and no sandbox, so there is nowhere to put an agent, which is why
one of them failing is what opens Settings for you. A missing `claude` or `gh`,
or either login, costs you Claude Code and leaves the rest of the IDE working,
so those are warnings rather than a wall.

**An API key instead of a login.** The **Agents** section of the same dialog
stores an Anthropic API key in the Windows credential store, never in a config
file. A stored key counts as a credential everywhere a login does, so the chat
box opens as soon as you save one — the `claude_auth` tick stays a cross,
because that check is about the daemon's own login and cannot see your key. It is used only when
Claude Code has no login of its own. Leaving the field empty keeps the stored
key; **Remove key** deletes it. The same dialog sets the default permission mode
new agents start with.

---

## Creating an agent workspace

**New agent** on the group bar, or **File > New Agent…**.

The dialog opens on the repository you used last, and it opens *after* the
daemon has read that repository — the window shows a wait cursor and "Reading
repository…" in the meantime — so the base-branch list is filled the moment you
see the dialog rather than half a minute later. If reading it fails, the dialog
opens anyway with the reason on it, so you can correct the path. **Create** is
greyed out while the repository in the box has not been read.

| Field | What it does |
| --- | --- |
| Repository | The git repository to branch from. **Browse…** picks one; **Recent** lists repositories you have used before. |
| Base branch | The branch the workspace starts from and later merges back into. |
| Name | Names the workspace and its branch, `bs/<name>/work`. Defaults to `agent-<n>`. One word: letters, digits, `-` or `_`. A space or a `/` is refused as you type, with the reason under the field, and **Create** stays greyed out until you fix it. |
| Adapter | **Claude Code** (the default when the daemon has it) or **Terminal**. |
| Command | For a Terminal workspace, the command to run. Empty means your default shell. |
| Model | Claude Code's `--model`. Empty means its default. |
| Permission mode | `default`, `acceptEdits`, `plan` or `dontAsk` — the words Claude Code's `--permission-mode` accepts. |
| Initial prompt | Sent to the agent as soon as it is up. |
| Run config | Which run configuration the Run panel offers first. |
| Group | Which group tab the workspace's tab is filed under, or **New group…**. |

Creating the workspace adds a git worktree on `bs/<name>/work` and starts a
sandbox in front of it. Inside that sandbox the root filesystem is read-only;
only the worktree, the workspace's own git object directory and its cache are
writable; the repository's shared `refs/heads` and object store are read-only;
the sandbox has its own PID namespace; and it reaches the network only through
an allowlisting proxy.

**A folder that is not a repository yet.** Point the dialog at one and it says
so under the path: "This folder is not a git repository. It will be initialised
with an empty first commit when the agent is created." A folder that does not
exist at all says "This folder does not exist; it will be created and
initialised." Pressing **Create** is what agrees to it; there is nothing to
tick.

If the repository's `bondsymphonic.toml` extends the network allowlist, the
dialog says so before you click **Create** — "This repository adds 2 hosts to
the network allowlist: …" — because creating the workspace is what applies it.
The dialog also warns when the repository has uncommitted changes.

Each workspace is one tab in the group bar. Groups are just tabs of tabs; a
workspace the daemon has that no group claims lands in **Unsorted**.

---

## The Claude tab

A Claude Code workspace's pane shows the conversation as it arrives.

- **The prompt box only appears once Claude Code has a credential** — either
  the `claude_auth` check passes, or you have stored an Anthropic API key under
  File > Settings… Until then the foot of the pane says so and offers **Log in
  to Claude Code…**, which opens Settings on its Setup section. An agent runs
  `claude -p`, and `-p` mode cannot log in — typing `/login` into the chat
  answers "login is not available in this environment" — so the login has to
  happen in the setup terminal. The box comes back on its own when the check
  passes, and immediately when you store a key; there is no restart. Terminal
  tabs are unaffected.
- **Transcript.** Your prompts, the assistant's answers rendered as Markdown,
  one card per tool call with its input and its result, and a line per turn with
  what it cost and how long it took.
- **Tool cards** collapse and expand. The card holds the tool's input and,
  when it finishes, its result.
- **Permissions.** When the agent asks to use a tool, a bar appears above the
  prompt box: "Allow `<tool>`: `<summary>`?", **Allow**, **Deny**, and a
  checkbox, "Always allow this tool for this session". Ticking it answers later
  requests for that same tool without asking again. That memory lives in the
  IDE, in this tab, and is forgotten when the pane re-attaches — it is never
  sent to the daemon and never written to disk.
- **A permission raised in a tab you are not looking at** puts a dot on that
  tab and the sentence `agent-2 is waiting for permission` in the status bar, so
  an agent waiting behind another tab does not wait silently. Both clear when
  the answer leaves, or when you switch to that tab.
- **Cost.** The status bar shows the cost of the tab you are looking at, not the
  total across every agent.
- **Interrupt** abandons the current turn and leaves the agent alive. **Stop**
  ends the process.
- **Restart.** An agent that has exited puts the reason in a banner and offers
  **Restart agent**. Restart starts a new agent with `--resume` pointed at the
  session id, so the conversation continues rather than beginning again. The
  session id is read out of the transcript, and when the transcript cannot
  supply one — a damaged or truncated history — the id the daemon recorded for
  that agent is used instead.
- **Long conversations.** Above 2,000 items per agent the oldest are folded into
  a single **Load earlier (N)** block at the top. Nothing is discarded: clicking
  it puts every item back and stops folding for the rest of that tab's life.

The pane replays the whole transcript from the daemon whenever it attaches, so
an IDE restarted while the daemon kept running finds its tabs where it left
them.

---

## Terminal tabs

A Terminal workspace's pane runs a shell inside that workspace's sandbox. So
does the **Terminal** tab of the bottom Output dock, independently, in whichever
workspace is active.

Colours, text attributes and resize work, and each session keeps 10,000 lines of
scrollback. Two workspaces keep independent terminals, and destroying one leaves
the other running.

When the daemon has to drop events under load, the affected screen prints
`[output dropped]` rather than quietly losing bytes.

---

## Files and Changes

The Explorer dock has two tabs and a header naming the active workspace.

**Files** lists the workspace's worktree one directory at a time, as you expand
them. The **Refresh** button in the Explorer header reloads the file tree and
the changes list in place.

Double-clicking a file opens it in the centre pane, with tree-sitter syntax
highlighting for Rust, JavaScript, TypeScript, TSX, Python, JSON, TOML, YAML,
HTML, CSS, Markdown, Bash, C, C++ and Go. Anything else opens as plain text. A
binary file, or one over 4 MiB, opens as a read-only notice.

Typing marks the tab with a dot. **File > Save** (Ctrl+S) writes the file back
through the daemon into the sandboxed worktree; **Save All** (Ctrl+Shift+S)
writes every dirty tab. Closing a tab with unsaved edits asks first, and so does
closing the window.

While a file is open the IDE watches it. A change made on disk by an agent or a
shell reloads an unmodified tab silently, keeping the caret and the scroll
position; on a tab with unsaved edits a bar offers reload or keep.

**Changes** lists the files that differ from the workspace's base branch with
their status and their +/- counts, and follows the worktree without your
pressing Refresh. Double-clicking a row opens a side-by-side diff: aligned rows,
green and red tints, two line-number columns, scrolling synchronised in both
axes, and the same highlighting as the editor. A diff does not reload when the
file changes underneath it — close it and open it again.

---

## Running the web app

The **Run** tab of the bottom dock follows the active workspace: a configuration
combo, **Start**, **Stop**, the URL as a link, **Open**, and the run's output.

**Where configurations come from.** A repository's `bondsymphonic.toml` in its
root declares them:

```toml
[[run]]
name = "web"
command = "npm run dev"
port = 5173
cwd = "frontend"          # optional, relative to the worktree root
env = { NODE_ENV = "development" }   # optional
ready_regex = "ready in"  # optional

[network]
allow = ["api.example.com"]

[claude]
settings = ".claude/settings.json"
```

`name`, `command` and `port` are required. A `[[run]]` block missing one of
them, or with port 0, or repeating an earlier block's name, costs **that block
only** — the rest of the file still loads. The Run panel hangs the reasons on
the configuration combo as a tooltip and puts an amber line under it:

```
bondsymphonic.toml: [[run]] #2 ('api') has no port; it is not offered
```

One problem shows as its own sentence, exactly as above. More than one shows as
`3 problems with bondsymphonic.toml`, with every reason in the tooltip.
Blocks are numbered from one. A file that will not parse at all falls back to
detection and reports the parser's own message, with its line and a caret, as a
single warning. Unknown keys are accepted, so a file written for a newer daemon
still loads.

With no such file the daemon guesses from marker files: `package.json` scripts
`dev`, `start` and `serve` through whichever package manager the lockfile names
(pnpm, yarn or npm), with Vite's own `server.port` when the config spells one
out and 5173, 3000 or 4200 otherwise; `docker compose up`, listed but greyed
out; `cargo run` for a crate with a `[[bin]]` or an axum/actix/rocket/warp
dependency; Django's `manage.py runserver`; and uvicorn or `flask run` from a
`pyproject.toml`. Detection only ever reads files — it never runs your build
scripts to find a port.

**Port override.** A port the daemon guessed is labelled `(guessed :5173)` and
the port field beside the combo is editable. Starting with a different number
sends it as that start's port, `PORT` included, and the number is remembered per
workspace and configuration. A port your `bondsymphonic.toml` spells out is not
editable — change the file instead.

**Starting.** Start spawns the command inside the sandbox with the
configuration's env plus `PORT` and `HOST=0.0.0.0`. The daemon allocates a free
port on the host's loopback and bridges it to the app, so `http://localhost:<port>`
answers from your Windows browser. The run goes `starting` → `ready`, on a
`ready_regex` match or on the app accepting a connection, and its output streams
into the panel's log, capped at 2,000 lines. **Open** launches the system
browser. **Stop** ends the process group and tears the bridge down.

Bridging is raw bytes both ways, so WebSockets and hot reload work over the same
port.

**The allowlist toast.** A sandbox has only loopback in its network namespace;
everything goes through a per-workspace proxy that checks the host against an
allowlist and refuses anything else with a 403. The default list covers
Anthropic, npm, PyPI, crates.io and GitHub. When a connection is refused, the
Run panel shows a toast on that workspace: **Blocked network access to
`<host>`**, with **Allow host** and **Dismiss**. Allow host adds the host to
that workspace's allowlist and the daemon puts the new list in front of the
proxy at once — the next attempt goes through with nothing restarted — and
writes it into the registry, so it survives a daemon restart.

The proxy also refuses any destination that resolves to a loopback,
link-local, private, unique-local, unspecified or multicast address, whatever
name was allowed, unless the allowlist entry *is* that literal address.

---

## Finishing a workspace

The **Changes** tab's toolbar is where work goes back.

- **Merge** runs `git merge --no-ff` of `bs/<name>/work` into the base branch.
- **Rebase** replays the workspace's commits onto the base and fast-forwards it.
- **Squash…** lands the lot as one commit whose subject you type. Leave it empty
  and the workspace's last commit subject is used.
- **Create PR…** asks for a title, a body and a draft flag, then runs
  `git push -u origin bs/<name>/work` followed by `gh pr create` with your own
  credentials, never in a sandbox. The pull request's URL appears in the status
  bar as a link.
- **Discard…** destroys the workspace and everything unmerged in it, behind a
  confirmation naming the workspace and how many changed files go with it.

The daemon performs merge, rebase and squash itself, never inside a sandbox: in
your own checkout when it happens to be on the base branch, and otherwise in a
scratch worktree of the base under its data directory, so a repository checked
out on some other branch is never disturbed. A conflict aborts cleanly and comes
back as a banner listing the conflicting paths, with the workspace exactly as it
was. A merged workspace and its branch stay; removing them is a separate
Discard.

**Closing a group** asks once per workspace — **Keep (move to Unsorted)**,
**Merge into its base**, or **Discard (destroy it)** — and then does exactly
that, one workspace at a time. One confirmation covers every discard in the run,
and cancelling it cancels the whole run, merges included.

Three things worth knowing before you rely on this:

- **Merging into your own checkout needs it clean.** When the repository is
  checked out on the base branch, any uncommitted change in it — untracked files
  included — refuses the merge rather than merging over your work. Checked out
  on another branch, your working tree is not inspected at all.
- **A merge runs none of your repository's hooks.** The daemon pins
  `core.hooksPath` at an empty directory, so a `post-merge` or `commit-msg` hook
  of yours does not fire for work landed from the Changes tab. The push behind
  Create PR is the one exception and keeps your hooks, because `pre-push` is how
  `git-lfs` uploads objects. Your own `git merge` and `git push` are unaffected
  either way.
- **`objects_stranded`.** A workspace's commits live in its own object
  directory, and the daemon copies the merged range into the shared store
  afterwards. If that fails the RPC reports `reason: "objects_stranded"` with
  `merged` or `pushed` true. The base really did move, but the objects behind it
  are readable only while the workspace exists — do not discard it.

---

## What persists across restarts

`%APPDATA%\BondSymphonic\state.json` records the groups and their order, the
active tab, the open editor tabs per workspace and which was in front, the
splitter sizes and whether the two halves are swapped, the window geometry and
dock layout, the recently used repositories, and the per-workspace run port
overrides. It is written 500 ms after the last change and again on exit.
`settings.json` sits beside it.

On start the IDE reconciles that file against the daemon: workspaces the daemon
no longer has are dropped along with their editors and overrides, and workspaces
it has that no group claims land in **Unsorted**.

The daemon keeps its own state under `~/.bondsymphonic` inside the distro: the
workspace registry, one worktree and one private object directory per workspace,
each sandbox's home and cache, one NDJSON transcript per agent, and a record of
every agent it has started. Those survive both restarts.

What does **not** survive a daemon restart: PTY processes, runs and port
bridges, and agent processes. Only the agents' history survives, not the process.

---

## What happens when the daemon restarts

When the daemon dies the status bar says `daemon: reconnecting (attempt N)` and
the IDE relaunches it, backing off 1, 2, 4, 8, 16 and then 30 seconds between
tries. The schedule resets only after a connection that held for five seconds,
so a daemon that crash-loops backs off instead of being restarted every second.

On reconnect the IDE re-runs the prerequisite checks and the workspace list, and
every pane re-attaches by itself: the file tree re-lists, the Changes tab
re-subscribes, the Run panel re-detects and each transcript replays its history.

What you have to do by hand:

- **Terminals.** The pane prints `[daemon restarted]` and offers **Reopen**,
  which starts a fresh shell. The scrollback above the marker is kept; the
  process, its shell history and whatever it was running are gone. The bottom
  Terminal tab behaves the same way.
- **Runs.** The Run panel comes back empty. Start the run again.
- **Agents.** A restored agent is `exited` and answers history and nothing else.
  Press **Restart agent**: it starts a new one with `--resume` on the recorded
  session id, so the conversation continues.

---

## Troubleshooting

**"daemon: protocol mismatch (daemon M, IDE N)".** The IDE and the daemon are
from different builds. The IDE stops at the handshake and does not try again:
every attempt would be refused the same way. It has nothing to repair either —
it installs the daemon binary it ships with before every launch, so a mismatch
means that binary is itself from a different build. Fix it by making the pair
match: reinstall the package, or rebuild the daemon with
`.\scripts\build-daemon.ps1` from the same checkout as the IDE. An IDE and a
daemon are shipped as a pair.

**Reading the logs.** Neither side writes a log file. The daemon's own output
and its `daemon.log` protocol events are folded into the IDE's log stream, so
starting the IDE from a console shows both:

```powershell
$env:BS_LOG = "debug"
C:\Tools\BondSymphonic\bondsymphonic-ide.exe
```

Daemon lines are tagged `daemon`. `BS_LOG` takes any `tracing` filter, so
`info,bondsymphonic_ide::client=debug` narrows it to the protocol.

To watch the daemon alone, start it by hand inside the distro against a scratch
data directory — see `docs/daemon-protocol-notes.md`.

**"another bondsymphonic-daemon owns \<dir\>"** on stderr, exit code 2. A daemon
already holds an exclusive lock on that data directory. Only one may own one at
a time, because two would write the same registry over each other. Either you
started a second one by hand, or the IDE's daemon is still running. Find it:

```powershell
wsl -d bondsymphonic -- pgrep -a -f bondsymphonic-daemon
```

A leftover `daemon.lock` file means nothing on its own — the lock is released by
the operating system whenever the process ends, however it ends.

**Sandbox failures.** The sandbox is bubblewrap. If the `bwrap` or `userns`
check fails, unprivileged user namespaces are usually restricted:

```bash
echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee /etc/sysctl.d/60-bondsymphonic.conf
sudo sysctl --system
```

The daemon never silently falls back to running unsandboxed. Passing
`--no-sandbox` runs every workspace's processes directly in the distro instead,
which is a **development** option only: it removes the filesystem, PID and
network isolation an agent is meant to run behind.

**WSL distro health.**

```powershell
wsl --list --verbose                       # bondsymphonic should be Running or Stopped, version 2
wsl -d bondsymphonic -- bash -lc "git --version; bwrap --version; claude --version; gh --version"
wsl --shutdown                             # then start the IDE again
```

`wsl --shutdown` stops every distro, so close anything else using WSL first. If
the distro itself is missing, re-run `install.ps1` (packaged) or
`.\scripts\setup-wsl.ps1` (checkout).

**The IDE will not start at all.** Run `bondsymphonic-ide.exe --version` first.

If that prints nothing, or Windows reports a missing DLL, the package is
incomplete or still blocked. Unzip it again into a clean folder and run
`Get-ChildItem <folder> -Recurse | Unblock-File`. The package carries the Qt
libraries and the Visual C++ runtime it needs, so there is no redistributable to
install separately.

If the version prints but no window appears, the platform plugin is the suspect:
`platforms\qwindows.dll` must be there, beside `platforms\qoffscreen.dll`.
Setting `QT_DEBUG_PLUGINS=1` makes Qt say which plugin it tried and why it
failed.

**Nothing was created but the agent produced nothing.** Check **File >
Settings… > Setup**: an agent that dies at start-up because nobody is logged in
reports the reason in a banner above the prompt.

---

## The v1 acceptance scenario

This is the checklist the product is measured against. It needs a real Claude
Code login, so run it yourself after signing in under File > Settings… > Setup.

1. [ ] Open the IDE. File > Settings… > Setup shows eight ticks, or opens by
       itself with the failing ones.
2. [ ] **New agent** on a repository you can afford to branch. Adapter **Claude
       Code**, name `alpha`, group `demo`, an initial prompt that asks for a
       visible change to the web app.
3. [ ] **New agent** again on the same repository: name `beta`, same group, a
       different task.
4. [ ] Both tabs show a transcript that is moving, tool cards appearing, and a
       cost in the status bar. Answer a permission prompt in each.
5. [ ] In the Run tab, **Start** `alpha`'s run configuration. Wait for `ready`,
       then **Open**. The app answers in your browser.
6. [ ] Switch to `beta` and start its run too. Open that one as well: **both
       apps are open in the browser at the same time**, on different ports.
7. [ ] Explorer > **Changes** on `alpha`: the agent's files are listed with
       their +/- counts. Double-click one and read the diff.
8. [ ] The same on `beta`.
9. [ ] On `alpha`: **Merge**. The status bar says
       `Merged bs/alpha/work into <base>`. Confirm in your own checkout that the
       base branch moved.
10. [ ] On `beta`: **Discard…**, confirm. The tab goes, and so do the worktree
        and the sandbox.
11. [ ] Quit the IDE and start it again. The group, the remaining tab and the
        editor tabs come back.

---

## For developers

### Tests

```powershell
. .\scripts\env.ps1                                  # QMAKE and PATH; required before cargo
cargo test --workspace                               # Windows
cargo test -p bondsymphonic-ide --features require-qt # Qt-less skips become failures
.\scripts\test-daemon.ps1                            # the daemon suite inside WSL
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Every daemon test skips with a printed reason when `bwrap` is unavailable, and
every IDE test that needs the Qt runtime skips when `QMAKE` is unset. A skip
prints `SKIP: <reason>` and is counted in a `skipped:` line — it is never a
silent pass. Building the IDE crate with `--features require-qt` turns those
skips into failures, which is what CI's Windows job does so a missing Qt can
never pass as green.

### IDE test hooks

The IDE reads these from its own environment, once at startup. All of them do
nothing when unset, which is every ordinary run.

- **`BS_DAEMON_ADDR`** (a loopback `host:port`) and **`BS_DAEMON_TOKEN`**:
  connect straight to that address with that handshake token instead of starting
  a daemon through `wsl.exe`. Only loopback addresses are accepted, since the
  protocol carries file contents and PTY traffic; anything else is reported and
  ignored rather than guessed at.
- **`BS_DAEMON_BINARY`**: the daemon binary to install into the distro, ahead of
  the packaged copy beside the exe and the dev copy in `target\daemon\`. A path
  that does not exist is still used, with a warning — a caller who set it meant
  that path.
- **`BS_SMOKE_SCRIPT`**: a comma-separated list of steps the controller performs
  once the connection is up. `create` and `create_claude` make a workspace over
  `BS_SMOKE_REPO` as a terminal tab or a Claude tab; `open_agent` starts a Claude
  agent in it; `send`, `allow` and `stop` go through the window, leaving the same
  model calls a person's typing and clicking would; `open` opens a PTY; `tree`
  lists the workspace root; `open_file` and `open_diff` open `README.md` as an
  editor tab and as a diff; `detect` asks for the repository's run
  configurations; `run_start` and `run_stop` start and end a run; `allow_host`
  answers a denial toast; `close` closes the script's PTY; `merge` merges the
  workspace (the first `merge` step of a run sends mode `merge` with no summary,
  the second `squash` with one); `pr` opens a pull request; `reconnect` asks the
  daemon to drop the connection and waits for the IDE to build a new one;
  `destroy` destroys the workspace; `quit` ends the process with status 0.
- **`BS_SETTINGS_PATH`** and **`BS_STATE_PATH`**: where `settings.json` and
  `state.json` are read and written. A test points both at a directory of its
  own so it never touches the developer's real `%APPDATA%\BondSymphonic`.
  `BS_SETTINGS_PATH` also turns off the one-time migration from the old
  per-project location, so a test can never read those settings either;
  `BS_LEGACY_SETTINGS_PATH` is what a migration test points at a fake old file.
- **`BS_PACKAGED_EXE`**: the built `dist\BondSymphonic\bondsymphonic-ide.exe`
  that `crates/ide/tests/packaged_smoke.rs` exercises. Unset, that test skips.
- **`BS_LOG`**: the `tracing` filter for the IDE's own log stream, which also
  carries the daemon's.

One more hook belongs to the fake daemons in the tests rather than to the IDE: a
request whose method is `system.test_drop` makes them close the connection
without answering, which is what a dead daemon looks like from the IDE's side.
It is not a method the protocol has — the real daemon answers `invalid_params`
and leaves the connection up.

### Daemon test hooks

All read from the *daemon's* own environment, all inert when unset. No test
needs a Claude login, a GitHub login or a network.

- **`BS_GH_BIN`**: the command to run instead of the real `gh`, split the way a
  shell would, so the pull-request tests can point it at
  `crates/daemon/tests/fixtures/gh_stub.py`. The stub writes its argv to
  `$GH_STUB_LOG` and prints a pull request URL, and fails with `not logged in`
  when `GH_STUB_FAIL=1`. Nothing in the suite reaches GitHub: the tests push to
  a bare `origin` made with `git init --bare` beside the repository.
- **`BS_CLAUDE_BIN`**: the command to run instead of the real `claude`, so a
  stand-in can be an interpreter plus a script
  (`python3 <worktree>/fake_claude.py`). It also suppresses the "untested Claude
  Code version" warning, since a stand-in's version says nothing about the
  protocol.
- **`FAKE_CLAUDE_FIXTURE`**: which NDJSON stream
  `crates/daemon/tests/fixtures/fake_claude.py` replays. Inside a bubblewrap
  sandbox this never arrives — the sandbox builds the agent's environment from
  the spec alone — so the fake also falls back to `fixture.ndjson` beside itself.
- **`FAKE_CLAUDE_ECHO_DELAY`**: seconds the fake holds an echoed turn open
  before answering, for testing interrupt against something still working.

Because the daemon reads them from its own environment, launching it through
`wsl.exe` means naming them in `WSLENV`
(`WSLENV=BS_CLAUDE_BIN/u:FAKE_CLAUDE_FIXTURE/u`, and
`BS_GH_BIN/u:GH_STUB_LOG/u` for the `gh` stub) before starting the IDE, and
putting the fake somewhere the sandbox can see. The workspace's **worktree** is
the reliable place: it is bound read-write at its own path. `/home` is a tmpfs
with the workspace's own home mounted over it, `/tmp` is a fresh tmpfs, and
`/opt` is a tmpfs holding only the bound `claude`, so a fake under any of those
is invisible from inside.

### Driving the daemon by hand

`docs/daemon-protocol-notes.md` starts a daemon against a scratch data directory
and drives the protocol from a small Python script or `nc`.
