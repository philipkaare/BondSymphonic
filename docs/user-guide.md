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
# bondsymphonic-ide 0.1.0 (protocol 2)
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

**Paste the code with Ctrl+V.** The code the browser gives back is too long to
read off a screen and type in again, so the terminal pane takes a paste:
**Ctrl+V**, **Ctrl+Shift+V** or **Shift+Insert**, and right-clicking the pane
offers **Paste** as well. Every terminal in the IDE takes one, not just this
pane. AltGr characters are unaffected -- Windows reports AltGr as Ctrl+Alt, and
a paste is Ctrl without Alt.

**Nothing appears when you paste the code, and that is right.**
`claude auth login` reads the code with the terminal's echo turned off, the way
a password prompt does, so the screen does not change however you get the code
in -- pasted or typed, in this IDE or in any other terminal. The line above the
terminal says what happened instead: *Pasted 71 characters. Press Enter to send
them.* Press **Enter** and the sign-in goes through.

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
| Work in | **Work in a new worktree** (the default) or **Work directly in this checkout**. See below. |
| Base branch | The branch the workspace starts from and later merges back into. |
| Name | Names the workspace and, for a workspace in a worktree of its own, its branch `bs/<name>/work` — a workspace working directly in the checkout has no branch of its own, so there the name names only the workspace. Defaults to `agent-<n>`. One word: letters, digits, `-` or `_`, starting with a letter or a digit, at most 64 characters. A space, a `/`, a `.` or a leading `-` is refused as you type, with the reason under the field, and **Create** stays greyed out until you fix it. The daemon applies the same rule, so nothing the dialog accepts can fail later inside git. |
| Adapter | **Claude Code**, which is the default, or **Terminal**. |
| Command | For a Terminal workspace, the command to run. Empty means your default shell. |
| Model | A list of the models, or type any name Claude Code accepts. **Default** leaves `--model` off, so Claude Code decides. |
| Permission mode | `default`, `acceptEdits`, `plan` or `dontAsk` — the words Claude Code's `--permission-mode` accepts. |
| Initial prompt | Sent to the agent as soon as it is up. |
| Run config | Which run configuration the Run panel offers first. |
| Group | Which group tab the workspace's tab is filed under, or **New group…**. |

Creating the workspace adds a git worktree on `bs/<name>/work` and starts a
sandbox in front of it. Inside that sandbox the root filesystem is read-only;
only the worktree, the workspace's own git object directory and its cache are
writable; the repository's shared `refs/heads` and object store are read-only
to *this* workspace; the sandbox has its own PID namespace; and it reaches the
network only through an allowlisting proxy. (If the same repository also has a
workspace working directly in the checkout, that one's agent *can* write the
shared refs and objects — see "Working directly in a checkout" below.)

**Work in a new worktree, or directly in this checkout.** The default keeps
the agent off your own branch, in a worktree of its own — the paragraph above.
Choosing **Work directly in this checkout** instead disables Base branch,
which then simply shows the branch already checked out (or "detached HEAD")
rather than something you pick, since nothing is switched or created. The help
text under the choice says why: "The agent edits this folder on its current
branch. Its changes are not isolated on a branch of their own." If the
repository runs its git hooks from a directory the agent can write — from
inside the working tree, the way husky does — a warning names the path: "This
repository runs git hooks from \<path\>, which the agent can change. They run
outside the sandbox the next time you use git here." For a folder on a Windows
drive (`C:\…`, which the distro sees as `/mnt/c/…`), a second note says how the
protection works there: "This folder is on a Windows drive. There the sandbox
cannot make .git read-only, so a change to it is caught within a moment
instead: the sandbox stops and shows you what changed." The choice is greyed out,
with the reason as its tooltip, for a repository the daemon cannot work in
directly at all — a linked worktree of another repository, or one whose
`.git` is a file rather than a directory (a separate git directory, or a
submodule checkout) — and the dialog falls back to a new worktree for those.
The dialog remembers whichever choice you last created a workspace with and
opens on it next time; a fallback forced by a greyed-out choice does not
overwrite what you had chosen. See "Working directly in a checkout" below for
what that mode does, what it does not, and its residual risks.

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

A tab reads `agent-4 · BondSymphonic @ main` — the agent's name, the repository
it is working in and the branch it forked from. Not its own branch: every
workspace gets one called `bs/<name>/work`, which is the agent's name spelled a
second way and says nothing the tab does not already say. The generated branch
is still what gets merged, so it is in the tab's tooltip, spelled exactly, for
when you need it in a `git` command.

---

## Working directly in a checkout

Choosing **Work directly in this checkout** in the New Agent dialog is what it
sounds like: the agent edits your own folder, on whatever branch is already
checked out there, inside the same sandbox, network allowlist, runs and agent
tabs every other workspace gets. There is no branch of the workspace's own and
nothing to merge — the changes are simply in the checkout, exactly as if you
had run Claude Code there yourself.

**When to use it.** A quick change you plan to review and commit yourself,
without the extra step of merging a workspace branch back afterwards; or a
repository whose tooling — a dev server watching the working tree, a build
that assumes it is the only checkout — cannot usefully live in a second
worktree. For anything you would rather keep isolated until you decide to
bring it in, a new worktree (the default) is still the better choice.

**What the agent may do with git, and what it may not.** Inside the sandbox
the agent can stage, commit, switch branches and stash, exactly as you could
from a terminal in that folder. What it is kept away from is the set of files a
git running *outside* the sandbox would execute: `.git/config`, hooks,
`.git/info`, `.git/commondir`, `.git/worktrees`, `.git/remotes`,
`.git/branches`, or the git directory of an existing submodule
(`.git/modules`). Those are bound read-only, so on a checkout kept inside the
distro a git command that writes into one of them fails inside the sandbox —
`git remote add`, the tracking setup behind `git switch -c x origin/x`, and
`git sparse-checkout` among them.

On a **Windows drive** (`/mnt/c/…`) that bind is not the whole story, and this
is the usual case, because the repositories you browse to are Windows folders.
A bind protects one name; Windows resolves several names to the same file —
`.git/CONFIG`, `.GIT/config`, the old-style short name `GIT~1/config` — and a
write through one of those reaches the file with the mount, the device and the
inode all unchanged. So on such a drive the protection is **detect and stop**
rather than blocked: several times a second the daemon re-reads everything
under `.git` that git runs or reads as configuration and compares it with what
it saw before the sandbox started. Anything different — however it was written
— stops the sandbox within about a quarter of a second and shows you what
changed. The window is real: a command the agent runs in that moment does take
effect, and what you are being shown is what to undo.

**What the agent can reach that no bind covers.** `.git` itself is writable, so
an agent working in the checkout can write the repository's refs and objects:
it can move *another* workspace's branch (`refs/heads/bs/<name>/work`), rewrite
`packed-refs`, write loose objects, and add `refs/replace` entries, which
change what `git log -p` and BondSymphonic's own Changes view display. If you
run workspaces of both kinds on one repository, the isolation the worktree kind
gives you holds against its own agent, not against an in-place sibling's.

**What BondSymphonic writes into `.git`.** Before the sandbox starts, the
daemon writes `.git/commondir` containing a single `.` (git already treats the
git directory as its own common directory; this makes that explicit and
protects it from being pointed anywhere else), and creates `hooks`, `info`,
`worktrees`, `remotes` and `branches` when any of them is missing — git makes
these itself as soon as it needs them, so an empty one changes nothing. It
also creates `config.worktree`, empty, when it is not already there, and binds
it read-only regardless of whether the repository has
`extensions.worktreeConfig` turned on, since an agent could turn that on
itself. **Close workspace…** removes the `commondir` guard again, and removes
each of `worktrees`, `remotes`, `branches` and `config.worktree` that the
daemon itself created — and only those, and only while they are still empty;
anything git or the agent has put there since is left alone.

**Your own git in that repository stops the workspace too.** A bind holds
against the agent, but not against anything that runs *outside* the sandbox in
the same repository while the workspace is open — your own `git config`, `git
branch -u`, `git push -u`, `git remote add`, or `git sparse-checkout`. Each of
those replaces the very file a bind was made on, which detaches the bind in the
sandbox the way any rename or removal of a mounted-over path does; nothing
inside the sandbox can stop that. The same check that catches a write through a
Windows alias catches this: the daemon snapshots the protected entries right
before the sandbox starts and re-checks them several times a second — their
identity, whether they are still mounted, and their contents — and the moment
anything differs it stops the sandbox and says so.

Because the daemon cannot tell your own git command from the agent's, **any
`.git/config` rewrite you make on this repository from outside BondSymphonic
stops every in-place workspace on it the same way**; this is by design, not
something to work around. BondSymphonic's own routine work on a *worktree*
workspace of the same repository — Close, Merge, Rebase, Squash and Create PR
— is written not to write `.git/config`, so it does not trigger this.

**What you are shown, and what Retry does.** The workspace's tab and its banner
carry the sentence; beside it, **What changed** opens a line diff of
`.git/config` and the other protected files, as they were when the sandbox
started and as they are now. The daemon's own `daemon.log` carries the same
warning — the sentence, the entries that changed and that diff. The banner's
copy is kept for as long as the IDE is running, a reconnect included; an IDE
restarted after the event has only the log, so read it before you restart.
**Retry** re-protects the checkout and starts the sandbox again — review the
diff first: anything written in the brief window before the check caught it is
still sitting in `.git/config`, and Retry does not undo it, it only re-protects
and restarts.

Removing your own last worktree of the repository used to stop in-place
workspaces as well. It no longer does: while an in-place workspace is open, the
daemon keeps a locked, otherwise empty entry of its own in `.git/worktrees`
(named `.bs-inplace-<workspace id>`), which is what makes `git worktree prune`,
`git gc` and git's own automatic maintenance leave that directory alone. It
holds no worktree, so `git worktree list` does not show it, and **Close
workspace…** removes it again. Deleting `.git/worktrees` by hand while a
workspace is open still stops it, as replacing any other protected entry does.

**The Changes tab shows changes against `HEAD`.** Merge, Rebase, Squash,
Create PR and Discard are all shaped around a workspace branch this kind does
not have, so none of them appear, on the toolbar or the Workspace menu, for a
workspace working directly in a checkout.

**Closing it.** The Workspace menu, the tab's own menu and the workspace's
banner all read **Close workspace…** instead of **Destroy workspace…**, and it
asks a shorter question: *Close workspace "\<name\>"? The agent and its
sandbox stop. Your files, branches and git history are not touched.* There is
no Force box and no second confirmation — closing an in-place workspace never
discards anything, so there is nothing to force.

**One in-place workspace per checkout.** A second **Work directly in this
checkout** on a folder that already has one is refused: "this checkout already
has an in-place workspace: \<name\>". A worktree workspace on the same
repository is unaffected and can run alongside it.

**Do not point an older BondSymphonic at the same distro while one of these
exists.** A build from before this feature does not know what kind of workspace
this is. It reads the daemon's `workspaces.json`, drops the field it does not
understand, and writes the entry back as an ordinary worktree workspace whose
"worktree" is your own checkout — and destroying *that*, in that older build,
would delete the folder. This build refuses to remove any worktree path it did
not create itself, so it cannot make that mistake, and every IDE installs its
own daemon into the distro before it connects, so an ordinary upgrade is safe.
What to avoid is launching an older IDE afterwards. Close your in-place
workspaces before you go back to one.

**Residual risks.** Working in the checkout itself means a few things the
sandbox cannot fully close off:

- **On a Windows drive, the protection catches rather than prevents.** See
  above: a write through an aliased name reaches `.git` and is stopped a
  fraction of a second later. What it did in that fraction of a second is
  yours to review and undo.
- **The repository's refs and objects.** An in-place agent can move any
  branch, including another workspace's, rewrite `packed-refs` and add
  `refs/replace` entries, which change what your `git log -p` and
  BondSymphonic's own Changes view show. Read a sibling workspace's changes and
  merge them while no in-place agent is running if that matters to you.
- **`core.hooksPath` the agent can write.** A repository that points its hooks
  somewhere inside the tree (husky does this) has ordinary files there the
  agent can edit like any other, and your own git runs them the next time you
  use it. The dialog warns about this when it is set (see "Creating an agent
  workspace" above). The same exposure exists for a hooks directory inside
  `.git` that is not `.git/hooks` — `.git` itself is writable, and only the
  entries BondSymphonic binds are not.
- **Programs your own git configuration names, if they live in the tree.** A
  `filter.*.clean`, a `diff.*.textconv` or a `core.fsmonitor` pointing at a
  script inside the working tree is a script the agent can rewrite, and the
  daemon runs your configuration unchanged when it reads the checkout's
  changes. This is the hooks risk in another form; the difference is that
  nobody has to type a git command for it to run.
- **`.gitattributes` in the working tree** can select which filter or diff
  driver runs over a file — but only a driver your own configuration already
  defines. This is not new to this mode: it is true of merged worktree content
  too.
- **A rebase left in progress.** A sequencer state the agent leaves behind can
  carry `exec` lines that run a command when you type `git rebase --continue`.
- **Scripts in the tree** — `package.json` scripts, a `Makefile` — run when
  you run them, exactly as they would after any other edit.
- **An embedded repository.** An agent can `git init` a directory in the tree,
  set something like `core.fsmonitor` in *its* `.git/config`, and commit it as
  a submodule. The next time your own git looks at the parent checkout, that
  command runs. BondSymphonic's own status and diff calls pass
  `--ignore-submodules=all` and so do not run it — the one exception is the
  rebase a Rebase merge runs in a *worktree* workspace, which git offers no
  such option for. Your own git is not covered at all.
- **History can be destroyed.** `rm -rf .git` cannot remove the files
  BondSymphonic protects, but it deletes everything else — `HEAD`, the index,
  every object and every ref — just as the agent can delete any other file in
  the checkout. Keep a remote or a backup; there is no undo inside
  BondSymphonic for this.

---

## The window

The shape is Visual Studio's: the editor is the fixed centre, and everything
else is a tool window docked around it.

- **Explorer** (left), **Agent** (right) and **Output** (bottom) can each be
  dragged to another edge, floated off the window, tabbed together, or closed.
  The editor cannot — it is the centre, and a window with nowhere to put a file
  would not be an IDE.
- **Wi&ndow** in the menu bar lists the three with a tick each; that tick is the
  dock's own, so it cannot disagree with what is on screen. **Reset layout**
  puts everything back where it started, which is the way out of a layout you
  have dragged yourself into a corner with. It is `Alt+N` rather than `Alt+W`,
  because Workspace sits earlier in the bar and takes that one.
- **View > Swap editor and agent** moves the Agent dock to the other side.
- The **Workspace** menu gathers what used to be reachable only from the Changes
  tab's toolbar or a right-click: New Agent…, Restart agent (`Ctrl+Shift+R`),
  Next/Previous agent, Merge, Rebase, Squash…, Create PR…, Discard…, Destroy
  workspace… and Close group….
- The **Run** menu gathers the Run panel's buttons: Run (`F5`), Stop
  (`Shift+F5`), Restart run, the detected configurations as one checkable group,
  Open in browser, Clear output, Copy output and Allow blocked host….

Both menus offer the same `QAction` objects the toolbars and panels do, so an
entry that is greyed is greyed in both places for the same reason.

### Theme

**Settings > Appearance > Theme** is **Follow system**, **Light** or **Dark**.
Follow system means following it as it changes, so a desktop that switches at
dusk takes the IDE with it. The choice applies as you pick it — the point of
choosing a palette is seeing it — and Cancel puts back the one you had.

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
- **The agent starts itself.** A Claude workspace starts its agent when it is
  created, when a saved session is restored, and when the IDE reconnects to a
  daemon that has forgotten it. There is no Start button: it was a button for a
  thing with exactly one sensible answer. While a start is in flight the pane
  says so.
- **A welcome** stands in an empty transcript, naming the agent, the model and
  permission mode it will answer as, the repository and branch it forked from,
  and the worktree it works in. The first real message replaces it.
- **Model and permissions** are the two dropdowns under the prompt box.
  `claude -p` reads both when the process starts and there is no way to change
  either in flight, so choosing one restarts the agent with `--resume` pointed
  at the session id — the conversation continues, and a line in the transcript
  records the switch. A turn in flight is interrupted.
- **Permissions do not prompt yet, and the modes say so.** Claude Code asks its
  *host* before running a tool that needs approval. The daemon tells the CLI it
  is that host and cannot yet answer, so the CLI resolves the question by
  refusing: the tool comes back with *"The following part requires approval:
  …"*, the agent works around it or gives up, and nothing ever reaches the amber
  bar. A command the CLI's own safety check considers harmless — `echo`, a plain
  `git ls-files` — runs without any of this.

  So the list reads **YOLO (sandboxed)**, **Accept edits (other tools
  blocked)**, **Plan only** and **Ask every time (blocks instead)**, and YOLO is
  what a new agent starts on. It is the loud end of the range and it is the only
  end that lets an agent finish a job; what makes it defensible is structural,
  not a warning dialog — the agent is inside a sandbox, in a worktree of its
  own, behind a network proxy. An existing setting is never rewritten to it:
  only a fresh install gets that default.

  When the host protocol lands, the modes go back to their plain names and the
  amber bar starts asking. `docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md`
  has the reproduction.
- **Interrupt** abandons the current turn and leaves the agent alive. It is the
  only one of the old three buttons left, because ending a turn acts on the
  conversation rather than on the process.
- **Restart.** An agent that exits on its own is not restarted automatically — a
  crash that repeated would become a loop reporting itself as a working agent.
  It puts the reason in a banner offering **Restart agent**, and the same action
  is in **Workspace > Restart agent** (`Ctrl+Shift+R`) and in Settings' Agents
  section. Restart starts a new agent with `--resume` pointed at the session id,
  so the conversation continues rather than beginning again. The session id is
  read out of the transcript, and when the transcript cannot supply one — a
  damaged or truncated history — the id the daemon recorded for that agent is
  used instead.
- **The turn cost and the agent's own system lines** are the small grey italics
  under an answer. Settings' Agents section has **Show turn cost and system
  lines**, on by default; turning it off hides both in every open transcript at
  once and leaves the answers alone.
- **Long conversations.** Above 2,000 items per agent the oldest are folded into
  a single **Load earlier (N)** block at the top. Nothing is discarded: clicking
  it puts every item back and stops folding for the rest of that tab's life.

The pane replays the whole transcript from the daemon whenever it attaches, so
an IDE restarted while the daemon kept running finds its tabs where it left
them.

---

## When a workspace cannot run

A tab reading ⏸ or ✕, with the reason in its tooltip, is a workspace whose
sandbox is not running. Its pane has a red banner at the top saying what
happened:

- *The sandbox for this workspace is not running* — it stopped unexpectedly
  while the IDE was open. Retry starts it again; the worktree and the
  conversation are kept.
- *This workspace could not be started* — the daemon could not bring it back,
  for example after a restart; the line under it names the reason, such as a
  worktree whose git registration had gone missing (see Troubleshooting).

The banner offers two buttons:

- **Retry** starts the workspace's sandbox again. It reads *Retrying…* while
  it works, and both buttons are disabled until the daemon answers. If it
  succeeds the banner goes away and, if the workspace's agent needs one, a new
  agent is started resuming the same conversation. If it fails, the banner
  shows the new reason and you can retry once the cause is fixed.
- **Destroy workspace…** asks the same question as the command of that name on
  the Workspace menu and the tab's own menu — *Destroy workspace "\<name\>"? Its
  sandbox and worktree are removed.*, with a **Force (discard changes)** box
  you can tick up front. Left unticked, and the daemon cannot tell the
  workspace is clean — which happens exactly when its worktree's git
  registration is the thing that went missing — you are asked once more:
  *Workspace "\<name\>" may have uncommitted changes[, and has commits that are
  not merged into its base branch]. Destroy it anyway?* **Destroy anyway**
  discards them and deletes the workspace's branch, with any commits only it
  has.

While a workspace cannot run, its prompt box says so instead of offering to
send a message, and the model and permission-mode dropdowns are unavailable,
since choosing one would restart an agent with no sandbox to restart it in.
Your worktree and branch are untouched either way — Retry is what brings them
back.

---

## Terminal tabs

A Terminal workspace's pane runs a shell inside that workspace's sandbox. So
does the **Terminal** tab of the bottom Output dock, independently, in whichever
workspace is active.

Colours, text attributes and resize work, and each session keeps 10,000 lines of
scrollback. Two workspaces keep independent terminals, and destroying one leaves
the other running.

**Select with the mouse, copy with Ctrl+Shift+C.** Drag to select, double-click
to take a word, and copy with **Ctrl+Shift+C**, **Ctrl+Insert** or right-click
**Copy**. Plain **Ctrl+C** copies when something is selected and sends the
interrupt when nothing is -- so it still stops a runaway command, and pressing
it twice always does. Typing clears the selection, because the text it was made
on is about to move.

**Questions the terminal is asked are answered.** Programs ask a terminal where
the cursor is, what it is and what colours it uses, and they stop reading input
until the answer comes back. `gh auth login` does this before each of its
yes/no prompts. The one question that is refused is a program asking to *read*
your clipboard: that belongs to you, not to whatever is running in a workspace.

When the daemon has to drop events under load, the affected screen prints
`[output dropped]` rather than quietly losing bytes.

---

## Files and Changes

The Explorer dock has two tabs and a header naming the active workspace: its
name, its branch and repository, and the worktree the tree below is listing.
The path is shown in full in the tooltip, elided from the left when the dock is
narrow -- the end is the part that says which agent this is -- and it can be
selected and copied.

**Files** lists the workspace's worktree one directory at a time, as you expand
them. The **Refresh** button in the Explorer header reloads the file tree and
the changes list in place.

Double-clicking a file opens it in the centre pane, with tree-sitter syntax
highlighting for Rust, JavaScript, TypeScript, TSX, Python, JSON, TOML, YAML,
HTML, CSS, Markdown, Bash, C, C++ and Go. Anything else opens as plain text. A
binary file, or one over 4 MiB, opens as a read-only notice.

**The open files belong to the agent.** Each agent works in a worktree of its
own, so `src/main.rs` in one is not the file of that name in another: switching
agent tab switches the whole row of editor tabs with it, back to the files that
agent had open and to the one that was in front. Nothing is closed by
switching -- edits, undo history and scroll position all wait where they were.

Typing marks the tab with a dot. **File > Save** (Ctrl+S) writes the file back
through the daemon into the sandboxed worktree; **Save All** (Ctrl+Shift+S)
writes every dirty tab, in every agent's row and not only the one on show.
Closing a tab with unsaved edits asks first, and so does closing the window --
which counts the files of agents you are not looking at.

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
browser. **Stop** ends the process group and tears the bridge down, and the run
is then listed as `stopped` with nothing more to say. A run that dies on its own
says why: `failed` with the exit code and its last lines if it never came ready,
`stopped` with the same detail if it had.

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

**A workspace working directly in a checkout has nothing here to finish.**
Its changes are already in your checkout, on your own branch — there is no
Merge, Rebase, Squash, Create PR or Discard, because there is no workspace
branch for any of them to act on. Commit and push it the way you always have,
from your own `git` or the agent's; when you are done with the workspace,
**Close workspace…** stops the agent and its sandbox without touching any of
it (see "Working directly in a checkout" above).

The **Changes** tab's toolbar is where work goes back.

- **Merge** runs `git merge --no-ff` of `bs/<name>/work` into the base branch.
- **Rebase** replays the workspace's commits onto the base and fast-forwards it.
- **Squash…** lands the lot as one commit whose subject you type. Leave it empty
  and the workspace's last commit subject is used.
- **Create PR…** asks for a title, a body and a draft flag, then runs
  `git push origin bs/<name>/work` followed by `gh pr create` with your own
  credentials, never in a sandbox. The pull request's URL appears in the status
  bar as a link. The push deliberately sets no upstream: `-u` would write
  `.git/config`, which stops any workspace working directly in that checkout.
  Set the upstream yourself if you want it, when no such workspace is open.
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
  checked out on the base branch, an uncommitted change to a tracked file
  refuses the merge rather than merging over your work. Untracked files do not:
  git will not overwrite one, and a scratch file sitting in your checkout is no
  reason to refuse every merge into it. Checked out on another branch, your
  working tree is not inspected at all.
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
window geometry and the dock layout — where each pane is docked, which are
floating and which are closed — the recently used repositories, and the
per-workspace run port overrides. The layout is Qt's own `saveState`, stamped
with a version: a layout saved before the agent pane became a dock is discarded
rather than half-applied. It also records the command a terminal tab was opened with and the
run configuration chosen for each workspace, since neither exists anywhere else.
It is written 500 ms after the last change and again on exit. `settings.json`
sits beside it.

**Neither file is ever overwritten when it cannot be read.** A `state.json` that
will not parse is renamed to `state.json.corrupt`, and a `settings.json` that
will not parse is renamed to `settings.json.bad-<timestamp>`; the IDE then
starts on the defaults and says so in the log. A hand-edited file with a typo in
it therefore costs you that run's settings, not the file — open the kept copy,
fix the typo and rename it back. Both files are written through a temporary file
and a rename, so a crash or a power cut mid-write leaves the previous version
rather than half of the new one.

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
daemon are shipped as a pair. This IDE speaks protocol **2** (the version that
added working directly in a checkout, below); a daemon still on protocol 1
answers this way rather than silently ignoring a request it has never heard of.

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

**A workspace's worktree disappeared on its own.** Every workspace worktree is
git-locked (`git worktree lock`) the moment it is created, specifically so that
a git tool on the Windows side — which cannot see a worktree living under the
WSL home, and so treats every one of them as stale — does not delete its
registration with `git worktree prune`. Never run `git worktree prune` or
`git worktree remove` on a BondSymphonic worktree by hand; the directory and
the branch are not what "prune" removes anyway, only the bookkeeping that
makes git recognise the directory as a worktree at all. If one is pruned
despite the lock — or the daemon is stopped mid-write and leaves the
registration half built — its tab shows the reason and a **Retry** button that
puts the registration back, keeping uncommitted work as unstaged changes. See
"When a workspace cannot run" above.

**"The repository \<path\> is missing or is no longer a git repository."** A
workspace working directly in a checkout (see "Working directly in a
checkout") found, on restore or restart, that its folder was moved, deleted,
or is no longer a git repository at all. Nothing was touched by BondSymphonic
— it does not create, move or delete that folder — so this means something
else did. **Retry** checks again and brings the sandbox back once the folder
is a repository at that path again; **Close workspace…** removes the
workspace's own bookkeeping and stops offering it, without expecting anything
back from the folder.

**"Git files this workspace protects changed while the agent was running (…),
so its sandbox was stopped."** An in-place workspace's sandbox stopped itself
because one of the git files it protects changed: `.git/config`, `commondir`,
`hooks`, `info`, `worktrees`, `remotes`, `branches`, `config.worktree`. Two
quite different things produce it, and the diff is how you tell them apart.
Either something outside the sandbox replaced the file — **any git command that
writes one of those, including one you run yourself in that repository**
(`git config`, `git branch -u`, `git push -u`, `git remote add`, `git
sparse-checkout`), stops the workspace this way, since the daemon cannot tell
your own command from an agent's — or, on a Windows drive, the agent itself
wrote through a name the drive resolves to the same file, which the read-only
binds do not cover and this check does (see "Working directly in a checkout"
above). BondSymphonic's own routine work on a *worktree* workspace of the same
repository (Close, Merge, Rebase, Squash, Create PR) does not write
`.git/config` and does not trigger it.

The banner's **What changed** shows the line diff of `.git/config` and the
other protected files, between what the daemon last checked and now; the
daemon's `daemon.log` carries the same warning and the same diff. Read it
before pressing **Retry** — a setting planted in the brief window before the
check caught it is still sitting in `.git/config`, and Retry does not undo it,
only re-protects and restarts. An IDE started *after* the event has only the
log: the banner's copy of the diff lives in the running IDE. **Close
workspace…** works regardless, since it never depends on the sandbox being up.

**"The check that keeps this workspace's git files safe from the agent could
not be made, so its sandbox was stopped."** The protection check itself failed
— it could not read the sandbox's mount table, or it ended in an error — and a
sandbox that cannot be watched is stopped rather than left running. Nothing is
known to have changed; **Retry** takes a fresh snapshot and starts it again.
If it repeats, the daemon's log says what the check tripped over.

**"This repository's last worktree was removed, which also removed a
directory the sandbox keeps read-only, so the sandbox was stopped. Nothing
needs checking; press Retry."** The same protection as above, but for a
different reason: `.git/worktrees`, a directory the in-place sandbox depends
on, is gone. Ordinary git use no longer causes this — while an in-place
workspace is open the daemon keeps a locked entry in that directory, so `git
worktree remove`, `git worktree prune` and `git gc` leave it standing — so what
is left is deleting it by hand. There is nothing to review here, unlike the
sentence above: just press **Retry**.

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
with the workspace's own home mounted over it, `/tmp` and `/run` are fresh
tmpfs, `/opt` is a tmpfs holding only the bound `claude` and, where it is
needed, the daemon binary, and `/mnt` is an empty tmpfs — so a fake under any of
those, a Windows drive included, is invisible from inside. The daemon's own data
directory is masked the same way, which is how one workspace is kept out of
another's worktree, home and objects.

### Driving the daemon by hand

`docs/daemon-protocol-notes.md` starts a daemon against a scratch data directory
and drives the protocol from a small Python script or `nc`.
