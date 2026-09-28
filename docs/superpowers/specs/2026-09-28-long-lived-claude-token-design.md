# Long-lived Claude token for agents — design

Date: 2026-09-28. Status: approved in chat by the user; this spec awaits their
review before planning.

## 1. Problem

Every agent runs on its own copy of the daemon user's
`~/.claude/.credentials.json`, seeded into its workspace home at each start. An
OAuth access token lasts eight hours; when it expires the CLI in the sandbox
refreshes it, and the refresh **rotates** the refresh token. All copies seeded
from the same login carry the same refresh token and the same expiry, so when
two agents are running at that moment, the first to refresh kills the token the
other holds, and the other fails with "OAuth session expired and could not be
refreshed" (the CLI then empties its copy).

The write-back shipped in `d2f6ffd` copies a refreshed login back to the host
on agent exit and before each seeding. That repairs *new* starts, but it cannot
reach an agent that is already running: separate copies of one rotating refresh
token cannot be kept consistent. The remaining failure is structural.

Verified on 2026-09-28: the host login itself was healthy (a `claude -p` as `bs`
refreshed it); the last recorded failure (2026-09-25 12:48) seeded a host token
that had been dead since 2026-09-14, which a write-back cannot resurrect.

## 2. Decision

Agents authenticate with a **long-lived token** produced by `claude setup-token`
(a Claude subscription token valid for about a year) passed in the environment
variable `CLAUDE_CODE_OAUTH_TOKEN`. Nothing refreshes inside a sandbox, so
nothing can go stale.

Decisions made with the user:

- The token lives **in the distro, owned by the daemon** — not in the IDE's
  Windows credential store.
- The daemon **captures it from the setup terminal's output**; the user does not
  paste it anywhere.

Out of scope: a daemon-side refresh broker; sharing one credentials file across
sandboxes; removing the credentials seeding or the write-back (both stay, see
§4.3); validating the token against the network in the prerequisite probe.

## 3. Pre-implementation check (blocking)

Two facts are assumed and must be confirmed before the plan is executed:

1. **What `claude setup-token` prints** (CLI 2.1.263 in the `bondsymphonic`
   distro): whether the token appears on one line, is wrapped by the TUI at the
   terminal width, or is interleaved with escape sequences. The user runs, once:

   ```
   wsl -d bondsymphonic -u bs -- script -q -c "claude setup-token" /tmp/setup-token.raw
   ```

   The raw capture is inspected without printing the token. It becomes the
   fixture the scanner's tests are written against (with the token replaced by
   a synthetic one of the same shape), and the captured token is installed as
   the token file (§4.1) by hand.
2. **The environment token wins over a seeded credentials file** in the agent's
   own mode (the adapter's stream-json argv), including a copy that is expired
   or emptied, and the CLI then neither refreshes nor rewrites that file.
   Checked by starting an agent against the hand-installed token.

If (1) shows the token wrapped across lines, the scanner joins wrapped
continuation lines (§4.2) and the setup terminal is opened at least as wide as
the token. If (2) fails, the design stops here and comes back to the user.

## 4. Design

### 4.1 Token store (daemon)

A new module `crates/daemon/src/agents/token.rs` owns one file:
`<daemon root>/claude-oauth-token` (i.e. `~/.bondsymphonic/claude-oauth-token`,
next to `homes/`).

- `read() -> Option<String>`: the trimmed contents if the file exists, is a
  regular file (not followed through a symlink), and matches the token shape;
  otherwise `None`.
- `write(token)`: mode 0600, temp file + rename, reusing the private-write
  helpers in `credentials.rs` (`replace_private`), refusing a symlinked
  destination.
- `remove()`: deletes it; absent is success.

The token never appears in a log line. Logs say "stored"/"removed" and the
path.

### 4.2 Capturing it (daemon)

- `SetupAction::ClaudeSetupToken` is added to the proto and to
  `setup::ALL_ACTIONS`; its row in `setup_argv` is `["claude", "setup-token"]`.
- `PtyManager::open_host` takes an optional **output tap**. For
  `ClaudeSetupToken` the handler passes a `TokenScanner`; the pump feeds it every
  chunk it already forwards as `pty.output`. Other actions pass none.
- `TokenScanner` strips ANSI/OSC escape sequences, keeps a bounded tail across
  chunks (a token may be split between reads), and yields the first string
  matching `sk-ant-oat01-[A-Za-z0-9_-]+` of plausible length. On a yield the
  pump calls `token::write` off the runtime and logs that a token was stored.
  Later matches in the same session are ignored.
- The terminal's output still reaches the IDE unchanged: the user sees what the
  CLI printed.
- Capture happens when the token appears, not at exit, so closing the terminal
  afterwards does not lose it.

### 4.3 Using it (daemon, `agent.start`)

The agent's per-process environment (today: `ANTHROPIC_API_KEY` when the IDE
sends one) becomes:

1. `ANTHROPIC_API_KEY` if the IDE sent a key — an explicit choice wins;
2. else `CLAUDE_CODE_OAUTH_TOKEN` from `token::read()` if present;
3. else nothing (today's behaviour: the seeded credentials file).

It is still given to that one command only, never to the sandbox spec, so
workspace terminals do not receive it.

Credentials seeding and the write-back are **unchanged**. The seeded file is now
used only by a `claude` the user runs by hand in a workspace terminal; the
refresh race is confined to those, and the write-back still mitigates it.

### 4.4 Prerequisite and logout (daemon)

- `claude_auth` checks `token::read()` first: present → ok, detail
  "long-lived token". Absent → the existing `claude auth status --json` probe
  and its fallback, unchanged. Its fix text names both remedies.
- `SetupAction::ClaudeLogout`: the handler calls `token::remove()` before
  opening the terminal that runs `claude auth logout`, so the Setup row's
  Log out removes every way agents authenticate.

### 4.5 IDE

- The Setup tab's `claude_auth` row gains a **"Set up long-lived token"**
  action, sending `ClaudeSetupToken` through the existing `system.setup_pty`
  path. It shows on a failing row next to Log in, and on a passing row whose
  detail is not "long-lived token" (a user on the plain login is offered the
  upgrade).
- When that setup terminal exits, the IDE re-checks prerequisites, as it already
  does for a login terminal. It also clears the auth-failure override (from the
  in-flight `auth_failure_in` work), as a login terminal's exit does.
- `docs/user-guide.md`: a short section on the long-lived token, what it
  replaces, where it lives, how to renew or remove it.

### 4.6 Failure modes

| Case | Result |
|---|---|
| Token revoked or expired (~1 year) | Agent exits with an auth message; the IDE's auth-failure override shows "not logged in"; the user runs Set up long-lived token again, which overwrites the file. |
| Scanner never matches (output format changed) | No file written; the prerequisite stays as it was; a warn is logged when the setup-token terminal exits without a capture. |
| Token file unreadable/malformed | Treated as absent (fallback to the credentials file). |
| API key and token both present | API key used. |

## 5. Testing

- `token.rs`: round trip, 0600 mode, symlinked destination refused, malformed
  contents read as absent, remove when absent.
- `TokenScanner`: the §3 fixture (synthetic token); a token split across chunk
  boundaries; surrounded by colour codes; wrapped (if §3 showed wrapping); no
  match in ordinary login output; only the first match yielded.
- `setup_argv(ClaudeSetupToken)` literal; `ALL_ACTIONS` includes it.
- `agent.start` environment precedence (API key > token > none), asserting the
  token is not in the sandbox spec's environment.
- `claude_auth` passes on a token file without running the CLI probe.
- Logout removes the token file.
- IDE: the row action appears in the two cases of §4.5 (`qobject_smoke`).
- Daemon tests run in the distro as well as on Windows
  (`CARGO_TARGET_DIR=~/.bondsymphonic/target`).

## 6. Order of work

0. Verify and commit (or park on `wip/`) the uncommitted IDE auth-failure change
   in the tree; `launch.ps1` builds from the working tree.
1. §3 check with the user.
2. Daemon: §4.1 → §4.2 → §4.3 → §4.4.
3. IDE and docs: §4.5.
