# Review Fixes (2026-09-11) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every finding of the 2026-09-11 whole-codebase review: the ten ranked correctness bugs, the forty-odd verified lower-severity bugs, and the twenty-three verified cleanup/altitude items, without changing the product's shape.

**Architecture:** No new subsystems. Wave 1 is twelve correctness tasks whose file sets are disjoint, so they run as parallel implementers. Wave 2 is the cleanup/altitude work, which crosses those file sets and therefore runs after Wave 1 in four tasks (two daemon tasks in sequence, two IDE tasks in parallel with them). Wave 3 is the final review and merge. Every finding id below (`IQ1`, `NT2`, `CD1`, …) refers to the review ledger reproduced in the appendix at the end of this plan; an implementer reads the appendix entries for their task before touching code.

**Tech Stack:** Rust 1.98 (MSVC on Windows, GNU in the `bondsymphonic` WSL2 distro), cxx-qt 0.10 + Qt 6.9.2 msvc2022_64, bubblewrap in the distro, PowerShell 5.1 scripts.

**Spec:** the review ledger (appendix of this file) plus `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§6.2 sandbox promises, §7 network) and `-ide-design.md`. Where a fix changes documented behaviour, the task says which spec section to update.

## Global Constraints

- Rust edition 2021, `cargo fmt --all` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, zero MSVC warnings in the C++ shell.
- Every daemon test skips with a printed reason (never fails) when `bwrap` is unavailable; every IDE test that needs the Qt runtime skips with a printed reason when `QMAKE` is unset. A skip prints `SKIP: <reason>` and is counted, never a silent pass.
- Tests never touch the user's real `%APPDATA%\BondSymphonic` (`BS_SETTINGS_PATH` / `BS_STATE_PATH`), never the daemon's real data dir (each test its own `--data-dir`), never a real Claude/GitHub login, never a real remote, never the user's live workspaces (`ws_eebdd832` and the two on `/mnt/c/git/Industriens-Uddannelser/ikuf`).
- No desktop input injection by any agent, ever. GUI verification is by env-gated self-test hooks, own-window `QWidget::grab()`, or the offscreen smoke test.
- Additive protocol changes only (`#[serde(default)]`).
- Test-driven: write the failing test first for every correctness fix, watch it fail, then fix. Cleanup tasks keep the existing tests green and add one only where behaviour becomes testable for the first time.
- Commit trailers: `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01JutrWQs2T1VhDojiUfpM6F`.
- Commands (PowerShell 5.1, no `&&`): `. .\scripts\env.ps1` before cargo on Windows; `cargo test -p bondsymphonic-daemon` on Windows runs the daemon's unit tests and the noop-backend integration tests; `.\scripts\test-daemon.ps1` runs the full daemon suite in WSL (bwrap); `cargo test -p bondsymphonic-ide` for the IDE; `cargo test -p bondsymphonic-proto` for proto.
- Parallel implementers share one checkout and one branch. Each implementer edits only the files its task lists, stages only those files (`git add <paths>`, never `git add -A`), and retries a commit that fails on `index.lock` after a short wait. Implementers never run `git checkout`, `git stash`, `git reset`, or `git rebase`.
- Every task ends with the task's own test files green on the platform they run on and a commit per finding group (one commit for a small task is fine).

---

## Wave 1 — correctness (twelve parallel tasks, disjoint files)

### Task 1: Sandbox isolation (daemon)

**Findings:** NT2, NT3, NT1, NT8.

**Files:**
- Modify: `crates/daemon/src/sandbox/linux_bwrap.rs`, `crates/daemon/src/sandbox/exec_client.rs`, `crates/daemon/src/sandbox/mod.rs` (only if `SandboxSpec` needs a field for the worktree's host path or the data dir), `crates/daemon/tests/sandbox_integration.rs`
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §6.2 (state exactly what is visible inside the sandbox after this task)

**Interfaces:**
- Produces: nothing new outside the sandbox module. `SandboxSpec` may gain `data_dir: PathBuf` if it does not already carry it; the only constructor is in `workspace/lifecycle.rs`, which Task 4 owns, so if a new field is needed, add it with a `Default`-able type and have Task 4 leave the constructor alone (fill it from `DataDirs` inside `sandbox::mod.rs` where the spec is built, or read it from the existing `home`/`run` paths' parent).

- [ ] **Step 1: Failing tests (bwrap-gated, `sandbox_integration.rs`).**
  - NT2: start the daemon-side sandbox with an environment variable `BS_TEST_SECRET=hunter2` set in the test process before `start()`; inside the sandbox run `tr '\0' '\n' < /proc/1/environ; tr '\0' '\n' < /proc/2/environ` and assert the output does not contain `hunter2`. Also assert that `PATH` and `HOME` inside a spawned shell are still what `init.rs` sets.
  - NT3: create a temporary directory outside the data dir and outside `/home` (e.g. under `/tmp/bs-nt3-<pid>` is inside `/tmp`, which is masked; use `std::env::temp_dir()`'s parent or `/var/tmp`) containing `marker.txt`, then inside the sandbox `ls /var/tmp/bs-nt3-<pid>` must fail with "No such file". Also: with two sandboxes started from one data dir, from inside sandbox A `ls <data_dir>/homes/<ws_B>` and `ls <data_dir>/run/<ws_B>` must fail, and `test -S <data_dir>/run/<ws_B>/exec.sock` must be false. When the repo itself lives under `/mnt`, the worktree must still be visible (existing tests cover this on a WSL box; add an assertion that a sibling directory of the worktree under `/mnt` is not).
  - NT1: unit test in `linux_bwrap.rs`: with the daemon exe at `/opt/x/bondsymphonic-daemon`, `bwrap_args` contains a `--ro-bind /opt/x/bondsymphonic-daemon /opt/bs/daemon`-style bind and the exec'd path inside the sandbox is the bound one. Replace the test at ~`:396` that pins the unchanged path.
  - NT8: unit test for `ExecClient`'s early-exit table: file an exit for pid 42 with no waiter, then assert it is consumed (removed) by the next `Spawned{pid:42}` it is handed to, and that an entry is dropped after `EARLY_EXIT_TTL` (choose 60 s; make the TTL a `const` the test can shorten via a `#[cfg(test)]` constructor).
- [ ] **Step 2: Run the tests and watch them fail** (`.\scripts\test-daemon.ps1` for the bwrap tests; `cargo test -p bondsymphonic-daemon linux_bwrap exec_client` on Windows for the unit tests).
- [ ] **Step 3: Implement.**
  - NT2: add `--clearenv` to the bwrap argv immediately after the bind arguments, and pass the environment init needs explicitly with `--setenv` (the same keys `init.rs` sets for its children: `PATH`, `HOME`, `TERM`, `LANG` if set, `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`, `BS_*` variables the init protocol relies on). Also call `.env_clear()` on the tokio `Command` and re-add only what bwrap itself needs (`PATH`). Per-agent secrets (the Claude adapter's `ANTHROPIC_API_KEY`) are already passed per spawn through the exec protocol, not via bwrap; confirm with a grep and leave that path alone.
  - NT3: after the base `--ro-bind / /`, add `--tmpfs /mnt` unless the worktree (or the repo it belongs to) lives under `/mnt`, in which case bind only the worktree's own path (and the repo's `.git` common dir, which the existing late binds already cover) back in. Add `--tmpfs <data_dir>` before the late binds so that only this workspace's own `homes/<ws>`, `run/<ws>` and object paths are re-bound (the late binds already bind those; verify their order relative to the new tmpfs and reorder if needed).
  - NT1: `exe_is_hidden` must return true for any exe under a path that the argv masks: compute it from the same list used to emit `--tmpfs` (`/tmp`, `/home`, `/run`, `/opt`, `/mnt`, `<data_dir>`) rather than a hard-coded pair, and bind the daemon exe read-only at a fixed in-sandbox path (`/opt/bs/daemon`, next to the existing `/opt/bs/claude` convention) whenever it is hidden.
  - NT8: in `exec_client.rs`, (a) when a `Spawned` reply arrives and the waiter's oneshot is closed, send a kill for that pid and do not file its later exit; (b) give `early` entries an `Instant` and prune entries older than `EARLY_EXIT_TTL` whenever a new entry is inserted; (c) consume (remove) an entry when a spawn with that pid claims it.
- [ ] **Step 4: Run the sandbox suite in WSL and the unit tests on Windows; both green.**
- [ ] **Step 5: Update spec §6.2** to list exactly what a sandboxed process can see: `/` read-only minus `/tmp`, `/home`, `/run`, `/opt`, `/mnt` and the daemon's data directory, plus its own home, run dir, worktree and (when the repo is under `/mnt`) that repo alone.
- [ ] **Step 6: fmt, clippy, commit.**

```bash
git add crates/daemon/src/sandbox crates/daemon/tests/sandbox_integration.rs docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md
git commit -m "fix(daemon): sandbox starts with a cleared environment, hides /mnt and other workspaces' data, binds a masked daemon exe, prunes early exits"
```

---

### Task 2: Network proxy and allowlist (daemon)

**Findings:** NT7, NT4, NT5, NT6, and the `Bridge::probe` dead method from CD7.

**Files:**
- Modify: `crates/daemon/src/net/proxy.rs`, `crates/daemon/src/net/allowlist.rs`, `crates/daemon/src/net/bridge.rs`, `crates/daemon/tests/network_integration.rs`
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §7 (plain-HTTP requests are proxied one request at a time; the private-range list)

- [ ] **Step 1: Failing tests.**
  - NT7 (`network_integration.rs`, bwrap-free: the proxy listens on a Unix socket the test can talk to directly): start two local HTTP listeners A and B that record the request line and headers they receive and answer `HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na` (keep-alive). Allowlist both as literal `127.0.0.1` entries with their ports (or hostnames the test maps). On one proxy connection send `GET http://<A>/ HTTP/1.1\r\nHost: <A>\r\n\r\n` then `GET http://<B>/secret HTTP/1.1\r\nHost: <B>\r\nAuthorization: Bearer b-token\r\n\r\n`. Assert B received `/secret` with the bearer header and A never saw `b-token`; assert the client got two responses in order.
  - NT4 unit test in `proxy.rs`: `is_private_destination` (or whatever the function at `:264` is called) returns true for `100.64.0.1`, `192.0.0.8`, `198.18.0.1`, `240.0.0.1`, `64:ff9b::1.2.3.4`, `::ffff:10.0.0.1`, and false for `8.8.8.8`, `2606:4700::1111`.
  - NT5 unit test in `bridge.rs`: an accept error that is not fatal (simulate by feeding an `io::Error` of kind `ConnectionAborted` through the accept-handling function) leaves the loop running; extract the per-accept handling into a function `fn on_accept(result: io::Result<(TcpStream, SocketAddr)>) -> Control { Continue | Stop }` so it is testable, with `Stop` only for `EBADF`/listener-closed.
  - NT6 unit tests in `allowlist.rs`: `HostPattern::parse("example.com.")` matches host `example.com` and vice versa; `HostPattern::parse("2606:4700::1111")` and `"[2606:4700::1111]"` both parse as an IPv6 literal and match that address; `refuse()`'s published `host` for a request with a trailing dot is the normalised form (test through `network_integration.rs`'s existing denied-host assertion, extended with a trailing-dot request).
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - NT7: restructure the plain-HTTP path into a per-request loop: read one request head from the client (existing head parser), decide the target host from the absolute-form URI or `Host`, run the same allowlist/private-destination checks as the first request, connect to that host (reuse the current upstream only when it is the same host:port and still open), write `origin_form(head)` with `Proxy-*` headers stripped and `Connection: close` added, relay the body according to `Content-Length`/`chunked` (or until the upstream closes when the request has no body), then relay the response until the upstream closes, then continue with the next client request. On any parse failure answer `400` and close. Keep CONNECT handling unchanged.
  - NT4: extend the private-range predicate with `100.64.0.0/10`, `192.0.0.0/24`, `198.18.0.0/15`, `240.0.0.0/4`, `64:ff9b::/96` (map the embedded IPv4 and re-check), and treat IPv4-mapped IPv6 by unmapping before every check (already done for `::ffff:` per the verdict; confirm).
  - NT5: log and continue on accept errors, sleeping 100 ms like `proxy.rs:627`; stop only when the listener itself is gone.
  - NT6: strip one trailing `.` in `HostPattern::parse` (and reject `..`); accept IPv6 literals with or without brackets; make `refuse()` publish the normalised host so the one-click Allow round-trips.
  - CD7: delete the unused `Bridge::probe` method (the free fn `bridge::probe` stays).
- [ ] **Step 4: Tests green on Windows (unit) and in WSL (integration).**
- [ ] **Step 5: Spec §7 update, fmt, clippy, commit.**

```bash
git add crates/daemon/src/net crates/daemon/tests/network_integration.rs docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md
git commit -m "fix(daemon): proxy relays plain-HTTP one request at a time, wider private ranges, bridge survives accept errors, trailing-dot and IPv6 allowlist entries"
```

---

### Task 3: Run manager races and configs (daemon)

**Findings:** RN1, RN2, RN3, RN4, RN7, RN8.

**Files:**
- Modify: `crates/daemon/src/runs/manager.rs`, `crates/daemon/src/runs/config.rs`, `crates/daemon/tests/run_integration.rs`
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §10 (stop outcome, readiness probe, Vite port rule)

**Interfaces:**
- Consumes: `workspace::lifecycle` marks a workspace `Destroying` (Task 4 owns that file). For RN7 this task must not edit `lifecycle.rs`; it re-checks the workspace state through the existing registry read it already uses at `manager.rs:218` after the run is inserted into `runs`, and tears the run down if the workspace is no longer `Ready`.

- [ ] **Step 1: Failing tests.**
  - RN2 unit test (`manager.rs`, runs on Windows): the cwd guard rejects `C:secret`, `D:foo/bar`, `\\server\share`, `/abs` and accepts `sub/dir`. Implement as a pure function `fn cwd_is_contained(base: &Path, cwd: &str) -> bool` that returns false when `Path::new(cwd)` has any `Component::Prefix` or `RootDir` or `ParentDir`, and additionally checks `base.join(cwd).starts_with(base)`.
  - RN1 (`run_integration.rs`, both backends): start a run whose command sleeps, stop it while `starting`, and assert the published terminal state is `Stopped` (not `Failed`) and `exit_detail` is empty. Run it 20 times in a loop to shake the race.
  - RN3: start a run with a config whose command ignores SIGTERM for 2 s (`trap '' TERM; sleep 30` on Unix; on Windows use a plain `sleep`), call `stop` and, without awaiting it, immediately call `start` for the same config: the second start must fail with `RpcError` reason `run_stopping`/`Conflict` rather than bind the same port.
  - RN4: a listener bound only to `[::1]:<port>` becomes `Ready` through the probe (skip when IPv6 loopback is unavailable).
  - RN8 unit tests (`config.rs`): in a repo with `vite.config.ts` (`server.port = 5173`) and `package.json` scripts `{dev: "vite", start: "node server.js"}`, `dev` reports port 5173 `port_guessed: false` and `start` reports a guessed port (`port_guessed: true`, whatever the existing default is).
  - RN7: with a run start in flight (a command that takes 1 s to spawn via a fixture, or by holding the registry write lock in the test), destroy the workspace; assert either the start fails with `workspace_not_ready` or the run is stopped and no `Failed` event is published after the workspace is gone.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - RN1: introduce a single `stopping: AtomicBool` on the run set by `stop_run` before it kills; the supervisor's finish path publishes `Stopped` when it is set, `Failed`/`Exited` otherwise; only one of the two paths may swap `finished` and whichever loses does nothing.
  - RN3: keep the run's `Claim` alive until `stop_run` has fully finished (move the claim into the stop future), so `claim()` refuses a restart during the grace with reason `run_stopping`.
  - RN4: probe `127.0.0.1` and `::1` (either success is ready).
  - RN7: after inserting into `runs`, re-read the workspace state; if not `Ready`, stop the run and return `RpcError::invalid_params` with reason `workspace_not_ready`. The supervisor drops (no publish) when the workspace is `Destroying`/gone.
  - RN8: in `node_port`, take the Vite branch only when the script's command invokes `vite` (first token, or `vite ` after `npx`/`pnpm`/`yarn`).
- [ ] **Step 4: Run tests on Windows and in WSL; green.**
- [ ] **Step 5: fmt, clippy, spec, commit.**

```bash
git add crates/daemon/src/runs crates/daemon/tests/run_integration.rs docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md
git commit -m "fix(daemon): run stop is never reported as failed, drive-relative cwd rejected, restart waits for the old process, IPv6 readiness, Vite port only for vite scripts, start loses to destroy"
```

---

### Task 4: Workspace creation and merge guards (daemon)

**Findings:** RN6, RN5, AL5, AL7 (proto validator and the daemon side of it).

**Files:**
- Modify: `crates/daemon/src/workspace/lifecycle.rs`, `crates/daemon/src/workspace/registry.rs` (only if `find_by_name`+insert need a combined `insert_if_name_free`), `crates/daemon/src/git/worktree.rs`, `crates/daemon/src/git/merge.rs`, `crates/daemon/src/git/repo.rs`, `crates/daemon/tests/workspace_integration.rs`, `crates/daemon/tests/merge_integration.rs`, `crates/daemon/tests/git_repo.rs`
- Create: `crates/proto/src/workspace_name.rs` (+ `pub mod workspace_name;` in `crates/proto/src/lib.rs`), `crates/proto/tests/workspace_name.rs`

**Interfaces:**
- Produces: `bondsymphonic_proto::workspace_name::validate(name: &str) -> Result<(), String>` — `Ok` only for `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`; the `Err` string is the user-facing reason (`"use letters, digits, - or _"`, `"name is empty"`, `"name is too long (64 max)"`). Task 10 replaces the IDE's private validator with this call.
- Produces: `git::repo::classify(path) -> RepoKind { NotARepo, Root, InsideEnclosing { root }, Bare, Worktree }` (or the closest set the three current classifiers need); `inspect`, `init_repo` and `create` all call it.

- [ ] **Step 1: Failing tests.**
  - AL7 (`proto/tests/workspace_name.rs`): `feat:x`, `a b`, `a/b`, `..`, empty, 65 chars → `Err`; `feat-1`, `Feature_A` → `Ok`. Daemon `workspace_integration.rs`: `workspace.create` with name `feat:x` is `InvalidParams` before any git work (no branch created).
  - RN6 (`workspace_integration.rs`): fire two `workspace.create` with the same name concurrently (`tokio::join!`); exactly one succeeds, the other is `Conflict`, and afterwards `git branch --list bs/<name>/work` shows the branch and the winner's worktree checks out (`git -C <wt> status` succeeds). Repeat 10 times.
  - RN5 (`merge_integration.rs`): with an untracked (non-ignored) file in the user's checkout, an in-place merge succeeds; with a modified tracked file it is still refused with `base_dirty`.
  - AL5 (`git_repo.rs`): `classify` on a plain dir, a repo root, a subdir of a repo, a bare repo, and a linked worktree returns the five kinds; `inspect` of a subdir reports the enclosing root (today collapsed into not-a-repo; keep `inspect`'s public result shape and just fill it correctly).
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - AL7: write the proto validator; call it in `lifecycle.rs` at the start of `create` (replace the four inline checks at `:474`).
  - RN6: take the repo lock (the one `destroy` and merge use) for the whole of `create`'s name check + `worktree::create` + registry insert, or add `Registry::reserve_name(name) -> Result<Reservation, Conflict>` released on failure; either way the loser sees `Conflict` before touching git. In `worktree::remove`, delete the branch only when the caller says the creation reached branch creation (`remove(layout, RemoveBranch::IfCreatedHere)`), so a failed loser can never run `branch -D` on the winner's branch.
  - RN5: `git status --porcelain --untracked-files=no` for the dirty-base guard.
  - AL5: one `classify` in `repo.rs`; `create`'s ladder at `lifecycle.rs:500-523`, `inspect` and `init_repo` map its result.
- [ ] **Step 4: Tests green on Windows (proto, noop-backend daemon tests) and in WSL.**
- [ ] **Step 5: fmt, clippy, commit.**

```bash
git add crates/proto crates/daemon/src/workspace crates/daemon/src/git crates/daemon/tests/workspace_integration.rs crates/daemon/tests/merge_integration.rs crates/daemon/tests/git_repo.rs
git commit -m "fix(daemon,proto): one workspace-name validator, same-name creates cannot delete each other's branch, untracked files no longer block merges, one repo classifier"
```

---

### Task 5: Claude adapter state and lifecycle (daemon)

**Findings:** AG1, AG2, AG3, AG4, AG5, AG6, AG7.

**Files:**
- Modify: `crates/daemon/src/agents/claude.rs`, `crates/daemon/src/agents/mod.rs`, `crates/daemon/tests/agent_integration.rs`, `crates/daemon/tests/fixtures/*` (a fake-claude fixture that issues two `can_use_tool` requests from one assistant message, and one that writes to stderr then exits non-zero)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` §8 (state ownership: only the reader publishes state; `send` while waiting for permission is refused)

- [ ] **Step 1: Failing tests (`agent_integration.rs`, fake-claude fixtures).**
  - AG1: fixture emits one assistant message with two `tool_use` blocks and two `can_use_tool` control requests without waiting; the test allows request 1 and asserts the published state after that is still `WaitingPermission` and `agent.history` shows request 2 pending.
  - AG2: `agent.send` while state is `WaitingPermission` returns `RpcError` reason `waiting_permission` and the state does not change.
  - AG3: fixture writes `boom` to stderr and exits 3; `exit_detail` in the published `Exited`/`Error` event contains `boom` every time (loop 20×).
  - AG4: start an agent, make `destroy` fail (a fixture-driven failure or by making the worktree undeletable on Windows via an open handle), then `agent.history` for that agent still answers.
  - AG5: two concurrent `agent.start` on one workspace: one succeeds, the other is `Conflict` (reason `agent_running`), and `settings.json` in the sandbox home is intact.
  - AG6: fixture prints an `is_error` result then exits; the final state is `Exited` with the error message as `detail`, `records_of` reports `Exited`, and `send` afterwards fails with reason `agent_exited` (not a broken pipe).
  - AG7: `claude --version` probe has a 10 s timeout (`BS_CLAUDE_BIN` pointing at a script that sleeps 20 s makes `agent.start` fail within ~10 s with reason `claude_probe_timeout`) and the failure is not cached: a second start with a working binary succeeds.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - AG1/AG2: remove the unconditional `State(Working)` publishes from `permission_reply` and `send`; the reader owns state and publishes `Working` when the CLI resumes. `send` while `WaitingPermission` is refused with reason `waiting_permission`.
  - AG3: join/await the stderr task before computing `exit_detail` on both the natural-exit and stop paths.
  - AG4: do not remove agents from the map in `stop_all_in` until `destroy` has succeeded (or move the removal into `forget_workspace`).
  - AG5: per-workspace `start` mutex (`tokio::sync::Mutex<()>` in a map keyed by workspace) held across seed + settings + spawn; a second start while held → `Conflict`.
  - AG6: after an `is_error` result the exit is announced as `Exited { detail: <message> }`, adapter cleared.
  - AG7: `tokio::time::timeout(10s, Command::output())` inside the probe; on error, do not fill the `OnceCell` (use `OnceCell::get_or_try_init`).
- [ ] **Step 4: Tests green on Windows (noop) and in WSL.**
- [ ] **Step 5: fmt, clippy, spec §8, commit.**

```bash
git add crates/daemon/src/agents crates/daemon/tests/agent_integration.rs crates/daemon/tests/fixtures docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md
git commit -m "fix(daemon): the reader owns agent state; stderr joined before exit detail; history outlives a failed destroy; one start at a time; dead agents exit; version probe times out"
```

---

### Task 6: Daemon server and filesystem service

**Findings:** the server-slice TOCTOU (fs.rs:46), connection.rs:112 panic-swallow, connection.rs:46 uncapped read_line, pty.rs:440 close_workspace escalation, fs_watch.rs:334 watch limit, fs_watch.rs:297 lock across watch, daemon.rs:151 sharing-violation-as-Busy, and CP4's daemon half (host PTYs closed on client disconnect).

**Files:**
- Modify: `crates/daemon/src/fs.rs`, `crates/daemon/src/server/connection.rs`, `crates/daemon/src/pty.rs`, `crates/daemon/src/fs_watch.rs`, `crates/daemon/src/daemon.rs`, `crates/daemon/src/server/handlers.rs` (only for wiring `pty.close` to host PTYs and the disconnect hook), `crates/daemon/tests/fs_service.rs`, `crates/daemon/tests/fs_integration.rs`, `crates/daemon/tests/server_integration.rs`, `crates/daemon/tests/pty_integration.rs`, `crates/daemon/tests/setup_pty_integration.rs`, `crates/daemon/tests/fs_watch_integration.rs`, `crates/daemon/tests/instance_lock.rs`
- Modify: `crates/daemon/Cargo.toml` only if `rustix` is added for `openat` (prefer `nix`, already a unix dependency, with its `fcntl::openat`/`OFlag::O_NOFOLLOW`).

**Interfaces:**
- Produces: `pty.close` accepts a host PTY id (the one `system.setup_pty` returned); the connection that opened a host PTY closes it on disconnect. Task 11's SetupPage calls `closePty` with its setup PTY id on teardown.

- [ ] **Step 1: Failing tests.**
  - TOCTOU (`fs_integration.rs`, unix-only): create `wt/notes/`, then in a loop from a second thread swap `wt/notes` between a real dir and a symlink to an outside dir while the test calls `write_file(wt, "notes/config", ..)` 500 times; assert no file ever appears in the outside dir and every call returns either `Ok` or `InvalidParams("path escapes the worktree")`. Same shape for `read_file` with a secret outside.
  - connection.rs:112 (`server_integration.rs`): a request whose handler panics (add a `system.test_panic` method gated like `system.test_drop`, fake-daemon/test-only) gets an `Internal` error reply carrying its id, and the connection stays usable.
  - connection.rs:46: a client that sends 2 MiB without a newline before `hello` is disconnected with no reply; a client that sends nothing for 10 s before `hello` is disconnected.
  - pty.rs:440 (`pty_integration.rs`, unix): a workspace PTY running `trap '' HUP; sleep 60` is gone within 3 s of `close_workspace`.
  - CP4 daemon half (`setup_pty_integration.rs`): open a host PTY, drop the client connection, assert the PTY's process is reaped within 3 s; `pty.close` with the host PTY id succeeds.
  - fs_watch.rs:334 (`fs_watch_integration.rs`): a worktree with `node_modules/` holding 200 subdirectories: enabling the watch installs no watch on those (assert via the watcher's own path list exposed under `#[cfg(test)]`, or by creating a file inside `node_modules/x/` and asserting no event, while a file in `src/` does produce one); a new directory created after enabling gets watched (create `src/new/` then `src/new/a.rs` → event).
  - fs_watch.rs:297: `enable` for a big tree does not hold `active` while walking (assert a concurrent `disable` for another workspace returns within 100 ms while `enable` is running against a tree of 5,000 dirs).
  - daemon.rs:151 (`instance_lock.rs`, windows): hold the lock file open for reading from another thread for 200 ms while the daemon takes the lock; the daemon retries and succeeds instead of exiting 2.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - TOCTOU: on unix, resolve `rel` component by component from an `O_DIRECTORY|O_NOFOLLOW` handle on the root using `openat`, refusing any symlink component; write via a temp file created with `openat` in the final directory handle and `renameat` within it; read via `openat(O_NOFOLLOW)`. On Windows keep the current canonicalise-and-retry path (dev/test only), behind `#[cfg(windows)]`.
  - connection.rs:112: run each handler inside its own `tokio::spawn` whose `JoinError` (panic) becomes an `Internal` error reply for that request id; log the panic.
  - connection.rs:46: `BufReader::take(MAX_FRAME)` (8 MiB, above the largest legal write) for reads and a 10 s `timeout` on the first line before hello.
  - pty.rs:440: `close_workspace` uses the same escalate path as `close()` (HUP → TERM → KILL with the existing grace).
  - CP4: track which connection opened each host PTY; on connection drop close them; let `pty.close` accept host ids.
  - fs_watch.rs:334/297: build the watcher outside the lock; walk the tree adding `NonRecursive` watches per directory while skipping `IGNORED` names; on `Create(Folder)` events add a watch for the new directory (skipping ignored); insert into `active` only after the walk.
  - daemon.rs:151: retry the lock open up to 20 × 50 ms on `ERROR_SHARING_VIOLATION` before reporting `Busy`.
- [ ] **Step 4: Tests green (Windows: fs_service, server, instance_lock; WSL: everything).**
- [ ] **Step 5: fmt, clippy, commit.**

```bash
git add crates/daemon/src/fs.rs crates/daemon/src/server crates/daemon/src/pty.rs crates/daemon/src/fs_watch.rs crates/daemon/src/daemon.rs crates/daemon/Cargo.toml Cargo.lock crates/daemon/tests
git commit -m "fix(daemon): symlink-safe file service, panicking handlers answer, capped pre-hello reads, escalating workspace PTY close, host PTYs die with their client, ignored dirs never watched, lock retry on sharing violations"
```

---

### Task 7: Editor document, router, client decode (IDE Rust)

**Findings:** IQ1, IQ2, IQ3, IM6, IM7, CI2.

**Files:**
- Modify: `crates/ide/src/qobjects/editor_document.rs`, `crates/ide/src/model/editor_buffer.rs`, `crates/ide/src/client/router.rs`, `crates/ide/src/client/mod.rs`, `crates/ide/tests/editor_tests.rs`, `crates/ide/tests/router_tests.rs`, `crates/ide/tests/client_tests.rs`, `crates/ide/cpp/EditorWidget.cpp` (only if the CRLF fix needs the widget to stop receiving CRLF text; see below)
- Modify: `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` §7 editor (line endings preserved on save, buffer holds LF)

**Interfaces:**
- Produces: `EditorBuffer { line_ending: LineEnding::{Lf, Crlf} }` detected on load; `EditorBuffer::text_for_save()` re-applies CRLF. `EventRouter::subscribe_fs(workspace_id) -> Receiver` (a new `StreamKey::Fs(WorkspaceId)`) used by editor documents instead of `subscribe_all`; `unsubscribe_stream(key, token)` only removes the subscriber whose token matches. Task 10's `AppController` creates the router per connection already; nothing else changes for it.

- [ ] **Step 1: Failing tests.**
  - IQ1 (`editor_tests.rs`): load `"ab\r\ncd"`, apply an edit at Qt UTF-16 position 4 inserting `X` (the position Qt reports for "between c and d" when CRLF is collapsed); the buffer text becomes `ab\r\ncXd` on save (`text_for_save()`), and the in-memory buffer is `ab\ncXd`. Add a second case with three CRLF lines editing on line 3, and a deletion of a line break that leaves no stray `\r`. A file loaded with mixed endings normalises to the majority ending and saves consistently (document the rule).
  - IQ2 (`editor_tests.rs` with the fake daemon): open a document, force a reconnect (the existing `reconnect` test helper), then have the fake daemon publish `fs.changed` for the path: the document emits `external_change`.
  - IQ3: save while dirty, then the fake daemon publishes `fs.changed` for the path with content equal to what was saved: no `external_change`; a keystroke typed between save and the event survives (buffer still contains it).
  - IM6 (`router_tests.rs`): subscribe A then B on the same key; A's tail unsubscribe (with A's token) leaves B receiving.
  - IM7 (`client_tests.rs`): the fake daemon sends one malformed event line (`{"kind":"no_such_kind"}`) then a valid one; the client logs, skips the bad line, and delivers the valid one; no `Disconnected` for pending requests.
  - CI2 (`router_tests.rs`): an `fs.changed` for workspace A is not delivered to a subscriber of workspace B's fs stream.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - IQ1: `normalise_line_separators` converts CRLF to LF and records `LineEnding::Crlf` when CRLF was the majority; `text_for_save()` converts back; `apply_edit` now maps Qt positions over an LF-only rope, so the offsets agree. The widget already receives `buffer.text()`; confirm it now gets LF text.
  - IQ2: add `on_reconnect` to `EditorDocument` mirroring `changes_model.rs:253` (re-subscribe on the new router and re-enable the watch); wire it where `ChangesModel::reload_after_reconnect` is wired (that call site is in `app_controller.rs`, owned by Task 10: expose `pub fn reconnect_all_editors()` on the document registry and tell Task 10's implementer the exact name; Task 10 adds the one-line call).
  - IQ3: remember `last_saved_generation` and the saved text's hash; on `fs.changed` for the path, when the buffer is dirty, fetch the file (existing read path) and compare its content with the saved hash; equal → ignore, else `external_change`.
  - IM6: `subscribe_stream` returns a token; `unsubscribe_stream(key, token)` compares.
  - IM7: `Err(_)` from decode → `tracing::warn!` and `continue`; only `Ok(None)` (EOF) and I/O errors break.
  - CI2: `StreamKey::Fs(WorkspaceId)`; `dispatch` routes `Event::FsChanged` by workspace to that key (plus `all`).
- [ ] **Step 4: `cargo test -p bondsymphonic-ide` green (with `QMAKE` set).**
- [ ] **Step 5: fmt, clippy, spec §7, commit.**

```bash
git add crates/ide/src/qobjects/editor_document.rs crates/ide/src/model/editor_buffer.rs crates/ide/src/client crates/ide/tests/editor_tests.rs crates/ide/tests/router_tests.rs crates/ide/tests/client_tests.rs crates/ide/cpp/EditorWidget.cpp docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md
git commit -m "fix(ide): CRLF files edit and save at the right offsets, editors survive a reconnect, own saves are not external changes, per-workspace fs stream, router unsubscribes by token, one bad event line no longer drops the daemon"
```

---

### Task 8: Transcript and run panel models (IDE Rust)

**Findings:** IQ4, IQ5, IQ6, IQ7, IQ8.

**Files:**
- Modify: `crates/ide/src/qobjects/transcript_model.rs`, `crates/ide/src/model/transcript.rs`, `crates/ide/src/qobjects/run_panel.rs`, `crates/ide/src/model/run_config.rs`, `crates/ide/tests/transcript_tests.rs`, `crates/ide/tests/run_tests.rs`

- [ ] **Step 1: Failing tests.**
  - IQ4 (`run_tests.rs`): a run that finished (`Exited`) stays in `runs_json` after `set_workspace` to another workspace and back, until a new run for that config starts.
  - IQ5: `run.list` failing (fake daemon error) leaves the existing runs, logs and subscriptions untouched and reports the error.
  - IQ6 (`transcript_tests.rs`): `agent.history` failing leaves the state `Unknown` (new variant, rendered as the previous state or "unavailable"), and a pending permission request folded from the live buffer is still shown.
  - IQ7: `always_allow` entries survive a reconnect-driven reattach for the same agent id (reset only when `attach` is called with a different agent id).
  - IQ8: `note_denied` for a workspace that was forgotten does not re-create its entry.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.**
  - IQ4: `WorkspaceRuns::apply_list` keeps a finished run for a config when the list has no run for that config; forget it only when a newer run for the config appears.
  - IQ5: on `Err`, report and return without `apply_detection`.
  - IQ6: no fabricated `Idle`: introduce `AgentState::Unknown` in the IDE-side model only (not proto) and skip `set_state_from_history` on failure; keep `pending`.
  - IQ7: hold `always_allow` in `TranscriptModel` keyed by agent id, moved into a fresh `Transcript` on reattach.
  - IQ8: `note_denied` ignores unknown workspaces.
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: fmt, clippy, commit.**

```bash
git add crates/ide/src/qobjects/transcript_model.rs crates/ide/src/model/transcript.rs crates/ide/src/qobjects/run_panel.rs crates/ide/src/model/run_config.rs crates/ide/tests/transcript_tests.rs crates/ide/tests/run_tests.rs
git commit -m "fix(ide): finished runs stay listed, a failed run list keeps state, a failed history never paints Idle, always-allow survives reconnect"
```

---

### Task 9: Launcher, settings, persistence, app state (IDE Rust)

**Findings:** IM1, IM2, IM4, IM5, IC1, IC4, IC2, IM3, IM8.

**Files:**
- Modify: `crates/ide/src/launcher.rs`, `crates/ide/src/model/settings.rs`, `crates/ide/src/model/persistence.rs`, `crates/ide/src/model/app_state.rs`, `crates/ide/tests/persistence_tests.rs`, `crates/ide/tests/restore_tests.rs`, `crates/ide/tests/model_tests.rs`, `crates/ide/cpp/SettingsDialog.cpp` (IC1: trim before the isEmpty gate; one line)
- Modify: `docs/user-guide.md` (settings file rules: a malformed file is renamed aside, never overwritten)

**Interfaces:**
- Produces: `PersistedTab { workspace_id, command: Option<String>, run_config: Option<String> }` inside `PersistedGroup` (`#[serde(default)]`, old files still load); `Workspaces::from_persisted` restores both fields. `Settings::load() -> Result<Settings, SettingsError>` where a parse error is `SettingsError::Malformed { path, backup }` after renaming the file to `settings.json.bad-<timestamp>`; callers that read-modify-write bail out on `Err` instead of persisting defaults. `Settings::save` uses a temp-file + rename. Task 10 (controller) consumes the new `load` signature in its two RMW call sites — the exact call sites are `record_api_key_set` (`settings.rs`, this task) and `set_default_permission_mode` (`app_controller.rs:2409`, Task 10).

- [ ] **Step 1: Failing tests.**
  - IM1 (`launcher.rs` unit): the stderr drain given bytes `b"ok\n\xff\xfe\n more\n"` yields three lines, the middle one lossily decoded, and keeps reading.
  - IM2: `wsl_path(r"\\wsl.localhost\bondsymphonic\home\bs\repo")` → `/home/bs/repo`; same for `\\wsl$\bondsymphonic\...`; a UNC path for another distro is an error naming the distro.
  - IM4: the `bash -lc` string for a daemon path with a space and log level `debug` quotes both (`exec '/opt/my dir/daemon' --log-level 'debug'`).
  - IM5: `parse_port_line` is applied to each stdout line until it matches, up to 20 lines / the existing 30 s; a profile that prints `hello` first still yields the port.
  - IC1 (`model_tests.rs`): `record_api_key(" sk-x ")` stores `sk-x`; `"   "` is rejected as empty.
  - IC4 (`persistence_tests.rs`): a malformed `settings.json` makes `load` return `Malformed`, the file is renamed to `settings.json.bad-*`, and a subsequent `record_api_key_set` does not write a defaults file over it (it writes a fresh file only when no settings file exists at all).
  - IC2 (`restore_tests.rs`): a terminal tab with `command: "npm run dev"` and an agent tab with `run_config: "dev"` come back with both after a save/load round trip; an old-format state file without the fields still loads.
  - IM3 (`model_tests.rs`): groups Default=[a], Feature=[b,c], active c → remove c → active is b (left neighbour, same group); remove the only tab of a group → first tab of the previous group, else next.
  - IM8: a tab whose workspace is `SandboxDown` ignores `set_agent_status` for its badge until the workspace is `Ready` again (status is stored, badge derives from workspace state first).
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement** as described in the interfaces and tests.
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: fmt, clippy, user guide note, commit.**

```bash
git add crates/ide/src/launcher.rs crates/ide/src/model/settings.rs crates/ide/src/model/persistence.rs crates/ide/src/model/app_state.rs crates/ide/tests/persistence_tests.rs crates/ide/tests/restore_tests.rs crates/ide/tests/model_tests.rs crates/ide/cpp/SettingsDialog.cpp docs/user-guide.md
git commit -m "fix(ide): lossy stderr drain, UNC wsl paths, quoted launch command, port line search, trimmed API key, malformed settings kept aside, run config and command persisted, neighbour tab selection, workspace state owns the badge"
```

---

### Task 10: App controller and group model (IDE Rust)

**Findings:** IC6, IC3, IC5, IC7, IC8; plus the wiring consumed from Tasks 4, 7, 9 (proto validator, `reconnect_all_editors`, `Settings::load` result) and the `GroupModel::arrangementChanged` signal Task 12 consumes.

**Files:**
- Modify: `crates/ide/src/qobjects/app_controller.rs`, `crates/ide/src/qobjects/group_model.rs`, `crates/ide/tests/qobject_smoke.rs`, `crates/ide/tests/reconnect_tests.rs`, `crates/ide/tests/restore_tests.rs` (only additions for IC6/IC3; Task 9 owns its other edits there — coordinate by adding a new test module at the end of the file)

**Interfaces:**
- Consumes: `bondsymphonic_proto::workspace_name::validate` (Task 4); `EditorDocuments::reconnect_all_editors()` (Task 7); `Settings::load() -> Result<_, SettingsError>` (Task 9).
- Produces: `GroupModel` signal `arrangementChanged()` emitted only when group names, membership or order change (not on status/attention updates); `changed()` keeps its current meaning. Task 12 rewires `MainWindow`'s `noteGroups` to `arrangementChanged`.

- [ ] **Step 1: Failing tests.**
  - IC6 (`restore_tests.rs`): with the fake daemon failing `workspace.list` twice, creating a workspace does not overwrite the persisted groups; after the daemon starts answering, the restore still yields Feature-A/Feature-B.
  - IC3 (`qobject_smoke.rs`): an `agent.state` event arriving before the `agent.start` reply is applied once the tab knows its agent id (status glyph `working` immediately after start).
  - IC5 (`reconnect_tests.rs`): between connection loss and the next attempt, `connectionState` reads `Reconnecting` (never `Connected` while `require_connection` fails).
  - IC7: a failed first `check_prereqs` is retried after 5 s (fake daemon fails once, then answers; `prereqs_json` fills without user action).
  - IC8 (`group_model` unit): `add_tab` with group `""` files the tab under `UNSORTED`, consistent with `note_workspace_created`.
  - arrangementChanged: `set_agent_status` does not emit it; `add_tab`/`move_tab`/`remove_group` do.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.** IC6: `note_groups` is ignored until `first_list_done` (and the restore has run). IC3: buffer `agent_state_changed` per workspace until `agent_started` lands, then replay; also make `apply_workspace_info` derive the glyph from `agent_records` when the tab has no live status. IC5: queue `set_state(Reconnecting)` before `old.shutdown()`. IC7: one retry after 5 s, then rely on the existing re-check points. IC8: route `""` to `UNSORTED`. Replace the private validator at `app_controller.rs:1089` with the proto call; call `reconnect_all_editors()` next to `reload_after_reconnect`; adapt `set_default_permission_mode` to the `Result` from `Settings::load`.
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: fmt, clippy, commit.**

```bash
git add crates/ide/src/qobjects/app_controller.rs crates/ide/src/qobjects/group_model.rs crates/ide/tests/qobject_smoke.rs crates/ide/tests/reconnect_tests.rs crates/ide/tests/restore_tests.rs
git commit -m "fix(ide): saved groups survive a failed first list, early agent state applied, Reconnecting shown promptly, prerequisite check retried, empty group routed to Unsorted, arrangement signal"
```

---

### Task 11: Editor, Changes toolbar, setup page and terminal widgets (IDE C++)

**Findings:** CP1, CP3, CP5 (SetupPage half), CP4 (IDE half), CI4, CI6 (this task's files).

**Files:**
- Modify: `crates/ide/cpp/EditorArea.cpp`, `crates/ide/cpp/EditorArea.h`, `crates/ide/cpp/ChangesToolbar.cpp`, `crates/ide/cpp/ChangesToolbar.h`, `crates/ide/cpp/SetupPage.cpp`, `crates/ide/cpp/SetupPage.h`, `crates/ide/cpp/TerminalWidget.cpp`, `crates/ide/cpp/TerminalWidget.h`, `crates/ide/cpp/Theme.h`, `crates/ide/tests/qobject_smoke.rs` (append a new `mod cpp_widgets` at the end; Task 10 also appends there, so append only, never reorder)

**Interfaces:**
- Consumes: `AppController::closePty(QString)` accepting the setup PTY id (daemon side from Task 6). If the controller lacks a `closePty` slot for host PTYs, this task may not edit `app_controller.rs`; use the existing PTY close slot and ask Task 6 to accept host ids server-side (it does).

- [ ] **Step 1: Failing tests (offscreen, `qobject_smoke.rs`, skip without `QMAKE`).**
  - CP1: open a dirty editor tab, trigger `closeTab` with the `m_ask` hook replaced by a function that destroys the workspace (calls `closeWorkspace`) before returning `Save`; no crash and no `save()` on a deleted page (assert via a QPointer-based log or ASan-free observable: the tab count and no Qt "QObject::connect: invalid nullptr" warning).
  - CP3: with the `m_ask` hook switching the toolbar's workspace mid-modal, `onDiscard` does not call `discardWorkspace` (assert the controller stub records no call, and the status bar shows "workspace changed, discard cancelled").
  - CP5: `SetupPage`'s detail label with text `<b>x</b>` renders the tags literally (`textFormat() == Qt::PlainText`).
  - CP4: destroying `SetupPage` after `setupPtyOpened` calls the controller's PTY close with that id (stub records it).
  - CI4: `TerminalWidget` paints 100 times with the same rows; `getRowsJson` is called once (counter on the stub).
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.** CP1: `QPointer` for page/editor/doc across `m_ask` and bail out when any is null. CP3: capture `workspaceId` before the modal, compare with `m_workspaceId` after; on mismatch cancel with a status message; pass the captured id to the controller in all four handlers. CP5: `setTextFormat(Qt::PlainText)` on `detailLabel`. CP4: `SetupPage` remembers its PTY id and closes it in its destructor / on page hide. CI4: cache the parsed rows per frame (invalidate on the `rowsChanged` signal, not on paint); blink repaints reuse the cache. CI6: replace `#eb5757`/`#4caf50` at `TerminalWidget.cpp:26` and `SetupPage.cpp:233` with `theme::removed()`/`theme::added()` (or a new `theme::danger()` if the semantics differ; add it to `Theme.h`).
- [ ] **Step 4: Build with zero MSVC warnings; tests green.**
- [ ] **Step 5: fmt (Rust), commit.**

```bash
git add crates/ide/cpp/EditorArea.cpp crates/ide/cpp/EditorArea.h crates/ide/cpp/ChangesToolbar.cpp crates/ide/cpp/ChangesToolbar.h crates/ide/cpp/SetupPage.cpp crates/ide/cpp/SetupPage.h crates/ide/cpp/TerminalWidget.cpp crates/ide/cpp/TerminalWidget.h crates/ide/cpp/Theme.h crates/ide/tests/qobject_smoke.rs
git commit -m "fix(ide): no use-after-free closing a tab under a destroyed workspace, toolbar actions target the confirmed workspace, plain-text setup detail, setup PTY closed with its page, terminal rows parsed once per frame, theme colours"
```

---

### Task 12: Group bar, main window, new-agent dialog (IDE C++)

**Findings:** CP2, CP6, CP5 (NewAgentDialog half), CI6 (GroupBar, NewAgentDialog), and the `arrangementChanged` rewiring for CI1.

**Files:**
- Modify: `crates/ide/cpp/GroupBar.cpp`, `crates/ide/cpp/GroupBar.h`, `crates/ide/cpp/MainWindow.cpp`, `crates/ide/cpp/MainWindow.h`, `crates/ide/cpp/NewAgentDialog.cpp`, `crates/ide/cpp/NewAgentDialog.h`, `crates/ide/tests/smoke.rs` (append-only)

**Interfaces:**
- Produces: `GroupBar::destroyRequested(const QString& workspaceId, const QString& workspaceName)` and `GroupBar::closeGroupRequested(const QString& groupName)` (replacing the index-based signals); `MainWindow::onDestroyRequested(workspaceId, workspaceName)` names the workspace in its confirmation ("Destroy workspace 'feature-a'? …"); `MainWindow::onCloseGroup(const QString& groupName)`.
- Consumes: `GroupModel::arrangementChanged` (Task 10). Until Task 10 lands, keep the `changed` wiring compiling; the switch is one line and can be done when both are on the branch — the implementer must check `group_model.rs` for the signal before wiring and leave a `TODO(arrangementChanged)` comment only if it is not there yet, to be resolved in Wave 2 Task 15.

- [ ] **Step 1: Failing tests (`smoke.rs`, offscreen).** CP2/CP6: right-click a tab, and while the menu is open (drive it through the `showAgentMenu` test seam: a hook that mutates the model before `exec` returns) add a tab to another group and make it active; choosing "Destroy workspace…" asks about the originally clicked workspace by name and the controller stub receives that id. Same for "Close group…" with a group removed ahead of the clicked one. CP5: `m_status` in `NewAgentDialog` is `Qt::PlainText`.
- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement.** Resolve workspace id + name (and group name) *before* `menu.exec()`, emit those, and have `MainWindow` act on the ids. The confirmation dialog names the workspace/group. `NewAgentDialog::m_status` gets `setTextFormat(Qt::PlainText)`. Replace `#eb5757` at `GroupBar.cpp:37` and `NewAgentDialog.cpp:115` with theme accessors.
- [ ] **Step 4: Build clean; tests green.**
- [ ] **Step 5: commit.**

```bash
git add crates/ide/cpp/GroupBar.cpp crates/ide/cpp/GroupBar.h crates/ide/cpp/MainWindow.cpp crates/ide/cpp/MainWindow.h crates/ide/cpp/NewAgentDialog.cpp crates/ide/cpp/NewAgentDialog.h crates/ide/tests/smoke.rs
git commit -m "fix(ide): context menus act on the tab and group that were clicked, and the confirmation names them"
```

---

## Wave 2 — cleanup and altitude (after Wave 1 is merged into the branch)

### Task 13: Daemon git runner, parallel git, worktree removal (daemon cleanup A)

**Findings:** CD1, CD2, AL6, CD7 (`path_str`, `fn s` duplicates, `Registry::save`, `DataDirs::bin`).

**Files:** `crates/daemon/src/git/mod.rs`, `crates/daemon/src/git/repo.rs`, `crates/daemon/src/git/worktree.rs`, `crates/daemon/src/git/merge.rs`, `crates/daemon/src/workspace/changes.rs`, `crates/daemon/src/workspace/lifecycle.rs`, `crates/daemon/src/workspace/mod.rs`, `crates/daemon/src/workspace/registry.rs`, `crates/daemon/src/sandbox/linux_bwrap.rs` (the `s` closure only), existing tests.

- [ ] CD1: one `fn command(&self, args) -> tokio::process::Command` builder plus a shared `finish(output)` tail; `run`, `run_with_stdin`, `run_bytes` become thin wrappers. `LC_ALL=C` and `GIT_TERMINAL_PROMPT=0` are set in exactly one place.
- [ ] CD2: `repo::inspect` runs its independent git calls with `tokio::try_join!`; `changes.rs:43-54` likewise; `lifecycle.rs:412` uses one `git config --get-regexp`; `lifecycle.rs:659-683` joins status and rev-list.
- [ ] AL6: `worktree::remove` no longer matches stderr phrases: run `worktree unlock` (ignore failure), `worktree remove --force` (ignore failure), `remove_dir_all` (ignore NotFound), `worktree prune`, then the branch step from Task 4; a test with a locked worktree passes.
- [ ] CD7: delete `workspace::path_str`, `Registry::save`, `DataDirs::bin` (or use it where `create_dir_all` is called), and fold the three `fn s`/closure copies into one `git::path_arg`.
- [ ] All daemon tests green (Windows + WSL); fmt; clippy; commit.

```bash
git commit -m "refactor(daemon): one git command builder, parallel independent git calls, phrase-free worktree removal, dead helpers removed"
```

---

### Task 14: Daemon agents/runs shared helpers and I/O (daemon cleanup B)

**Findings:** CD3, CD4, CD5, CD6, CD8.

**Files:** `crates/daemon/src/agents/mod.rs`, `crates/daemon/src/agents/persist.rs`, `crates/daemon/src/agents/claude.rs`, `crates/daemon/src/workspace/registry.rs` (spawn_blocking only), `crates/daemon/src/workspace/lifecycle.rs` (`start_shim` only), `crates/daemon/src/runs/manager.rs`, `crates/daemon/src/git/pr.rs`, `crates/daemon/src/pty.rs`, new `crates/daemon/src/util/ready_line.rs`, `crates/daemon/src/util/tail.rs`, `crates/daemon/src/util/argv.rs`, existing tests. Runs after Task 13 (shares `lifecycle.rs`, `registry.rs`).

- [ ] CD3: `AgentRecords::update` and `Registry::update` perform `write_atomic` inside `tokio::task::spawn_blocking` (or a dedicated writer task fed by a channel); the reader task never blocks on fsync.
- [ ] CD4: `TranscriptStore` keeps one open `File` per agent (opened on first append, closed on `ended`), `create_dir_all` once.
- [ ] CD5: `util::ready_line::wait_ready(child, pattern, timeout) -> Result<ReadyOutcome>` used by `start_shim` and `start_forwarder`; the differing not-ready behaviour stays at the call sites.
- [ ] CD6: `util::tail::Tail` (bounded `VecDeque<String>`) plus `exit_detail(code, tail, layout: TailFirst|CodeFirst)` used by both `manager.rs` and `claude.rs`; the shared exit future helper likewise.
- [ ] CD8: `util::argv::split(s, what) -> Result<Vec<String>, String>` used by `claude.rs`, `pr.rs`, `pty.rs`; each site maps the error to its own `RpcError` constructor.
- [ ] All daemon tests green; fmt; clippy; commit.

```bash
git commit -m "refactor(daemon): fsync off the runtime, one transcript file handle per agent, shared ready-line, tail and argv helpers"
```

---

### Task 15: IDE Rust cleanup

**Findings:** CI5, CI7, CI8, AL4 (Rust half), CI1 (model half, if Task 10 left anything), the `TODO(arrangementChanged)` from Task 12 if present.

**Files:** `crates/ide/src/qobjects/app_controller.rs`, `crates/ide/src/qobjects/group_model.rs`, `crates/ide/src/model/app_state.rs`, `crates/ide/src/model/editor_buffer.rs`, `crates/ide/src/model/run_config.rs`, `crates/ide/cpp/MainWindow.cpp` (the `noteGroups` wiring and the prereqs handoff only — coordinate with Task 16, which owns the rest of `MainWindow.cpp`; do this task's two edits first, commit, then Task 16 starts).

- [ ] CI5: delete `GroupModel::load_state`, `active_workspace_id`, `tab_agent_id`, `tab_run_config`, `tab_worktree_path`, `active_worktree_path`, `AppController::create_workspace`, `create_workspace_with_agent`, `EditorBuffer::replace_all`, `WorkspaceRuns.port_override` (or wire it), after confirming zero callers; `MainWindow::activeWorkspaceId` reads a cached id from the model instead of re-parsing JSON.
- [ ] CI7: `add_tab` builds its `AgentTab` via `AgentTab::from_workspace_info` (with empty records); `state_detail` shares the one `Error` match.
- [ ] CI8: one `fn workspace_op<F>(self, op: &str, ws: WorkspaceId, f: F)` prologue used by merge/create_pr/discard; one `create_workspace_inner` used by both create variants.
- [ ] AL4: `onPrereqsChecked` receives a typed struct (or the controller keeps the parsed block and exposes `prereqsBlocking()`), so the JSON is parsed once; `should_auto_open_setup` stays pure; `m_setupShownForBlock` moves into the controller as one state.
- [ ] CI1: `MainWindow` wires `noteGroups` to `arrangementChanged`; `set_groups` skips the store update when the arrangement is unchanged.
- [ ] IDE tests green; fmt; clippy; commit.

```bash
git commit -m "refactor(ide): dead model surface removed, one tab constructor, one workspace-op prologue, prerequisites parsed once, arrangement persisted only when it changes"
```

---

### Task 16: IDE C++ cleanup

**Findings:** CI3, AL1, AL2, AL3.

**Files:** `crates/ide/cpp/TranscriptView.cpp/.h`, `crates/ide/cpp/MainWindow.cpp/.h` (after Task 15's two edits are committed), `crates/ide/cpp/NewAgentDialog.cpp/.h`, `crates/ide/tests/smoke.rs`, `crates/ide/tests/transcript_tests.rs`.

- [ ] CI3: while an assistant item is streaming, `TranscriptView` appends plain text to the card's label (or coalesces deltas on a 50 ms timer) and renders Markdown once when the item completes; a test streams 2,000 deltas and asserts `itemAt`/`fromJson` are called far fewer than 2,000 times (counter on the stub).
- [ ] AL1: `MainWindow::onNewAgent` opens `NewAgentDialog` immediately; the dialog owns the whole inspect pipeline (it already has one), shows "Reading repository…" inside itself with a Cancel button, and enables OK when inspection lands. Remove `m_newAgentPending`, the cursor override, the stashed status-bar fields and the guard `QObject`.
- [ ] AL2: `AppController` failures reach `MainWindow` through typed signals per operation family (`repoInspectFailed(path, msg)`, `workspaceOpFailed(ws, op, msg)`, `prereqsCheckFailed(msg)`) instead of string comparison on `op` at `:1145-1170` — the Rust side of these signals is in `app_controller.rs`, which Task 15 owns: Task 15 adds the signals (emitting them alongside the existing `operationFailed` for one release), Task 16 consumes them.
- [ ] AL3: `openSettings` re-checks prerequisites on close only when the dialog reports it ran a setup action (`SetupPage::ranAction()`), not unconditionally.
- [ ] Build clean; tests green; commit.

```bash
git commit -m "refactor(ide): transcript streams without re-rendering, New Agent dialog owns its inspection, typed failure signals, prerequisite re-check only after setup actions"
```

---

## Wave 3 — verification and merge

### Task 17: Whole-branch verification

- [ ] `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings` (Windows) and the same inside WSL for the daemon.
- [ ] `cargo test --workspace` on Windows with `QMAKE` set (`--features bondsymphonic-ide/require-qt`).
- [ ] `.\scripts\test-daemon.ps1` in WSL: every suite green, skips listed and explained.
- [ ] `.\scripts\build-daemon.ps1` then a real launch smoke (`BS_SMOKE_SCRIPT` with `create,open_file,open_diff,detect,run_start,run_stop,close,quit` against a throwaway repo; never a user workspace).
- [ ] `/code-review` on the branch diff at high effort; fix anything Confirmed; re-run the affected tests.
- [ ] Fast-forward `main`, push, delete the branch (authorised for this project).

---

## Appendix — review ledger (verbatim verdicts, abridged to the mechanism)

Top ten (severity order): IQ1 CRLF offsets (`editor_document.rs:458`); CP3 toolbar actions after modal (`ChangesToolbar.cpp:287`); CP2/CP6 stale menu target (`GroupBar.cpp:313`, `MainWindow.cpp:904`); NT2 env leak via `/proc/1/environ` (`linux_bwrap.rs:230`); NT3 host FS visible (`linux_bwrap.rs:105`); NT7 keep-alive credential misdirection (`proxy.rs:823`); fs.rs TOCTOU symlink swap (`fs.rs:46`); CP1 use-after-free across modal (`EditorArea.cpp:275`); AG1/AG2 Working overwrites WaitingPermission (`claude.rs:667/:627`); IC6 first-list failure overwrites groups (`app_controller.rs:2502`).

**Agents (Task 5).** AG1 `permission_reply` publishes Working unconditionally after awaits, overwriting the reader's WaitingPermission for a second request; IDE `Transcript::set_state` clears `pending` on leaving WaitingPermission. AG2 same via `send`; the prompt box is live during `waiting_permission`. AG3 stderr task never joined before `exit_detail` (natural exit `:603`, stop `:741`). AG4 `stop_all_in` removes agents (`mod.rs:733`) before `destroy` can fail; `get()` then answers NotFound for history. AG5 two `agent.start` run seed + `write_settings` (`remove_any` then `create_new`) concurrently → AlreadyExists. AG6 after an `is_error` result the agent stays `Error` with the adapter live; `send` hits a broken pipe; records report Error not Exited. AG7 `claude --version` has no timeout inside a `OnceCell` init; a hang blocks all starts.

**Net/sandbox (Tasks 1, 2).** NT1 `exe_is_hidden` checks only /home,/tmp while /opt,/run are tmpfs; test at `:396` encodes the bug. NT2 no `--clearenv`/`env_clear` on the bwrap Command. NT3 `--ro-bind / /` recursive; `/mnt` and other workspaces' `homes/`, `run/` visible; Unix sockets on ro binds are connectable. NT4 private-range check misses 100.64/10, 192.0.0/24, 198.18/15, 240/4, 64:ff9b::/96. NT5 `bridge.rs:126` `let Ok(..) else { return }` ends the accept loop on any error. NT6 trailing-dot hosts never match; IPv6 literals fail `parse`; `refuse()` publishes the raw host. NT7 only the first plain-HTTP head is rewritten; later keep-alive requests are copied verbatim to the pinned upstream. NT8 `exec_client.rs:151` early-exit table keyed by pid never pruned; `connection.rs:190` `inflight.shutdown()` can abort a spawn after send.

**Runs/workspace/git (Tasks 3, 4).** RN1 `finished` race between `stop_run` and the supervisor; user stop reported Failed. RN2 `C:secret` passes the cwd guard and `join` replaces the base. RN3 `stop` removes from `runs` before killing; a restart claims the port. RN4 probe only 127.0.0.1; Node ≥17 binds ::1 first. RN5 `git status --porcelain` counts untracked files as dirty. RN6 `create` takes no repo lock; the loser's `worktree::remove` runs `branch -D` on the winner's branch mid-checkout. RN7 `run.start` between the Ready check and the `runs` insert is missed by destroy. RN8 `node_port` enters the Vite branch for every script when `vite.config.*` exists.

**IDE model (Task 9) / client (Task 7) / controller (Task 10).** IM1 stderr drain exits on the first non-UTF-8 line. IM2 `\\wsl.localhost\<distro>\…` → `//wsl.localhost/…`. IM3 removing the active last tab of a group jumps to group 0. IM4 unquoted daemon path/log level in `bash -lc`. IM5 first stdout line must be the port line; profiles print. IM6 `unsubscribe_stream` removes by key only. IM7 one decode error tears down the daemon. IM8 `set_agent_status` overwrites the workspace-state badge. IC1 API key untrimmed. IC2 run config / terminal command not persisted. IC3 `agent.state` arrives before `agent.start`'s reply and is dropped; reconcile uses `agent_status.unwrap_or(Idle)`. IC4 `Settings::load` swallows parse errors; RMW persists defaults; save non-atomic. IC5 connection lost → `Connected` still reads until the shutdown finishes. IC6 `note_groups` unguarded before the first successful list. IC7 first `check_prereqs` never retried. IC8 `add_tab("")` creates a group named "" vs `UNSORTED`.

**IDE qobjects (Tasks 7, 8).** IQ1 CRLF kept in the rope; Qt collapses to one unit. IQ2 editor subscribes once on the per-connection router; no `on_reconnect`. IQ3 own save's `fs.changed` with a dirty buffer → false external change; reload drops keystrokes. IQ4 `run.list` omits finished runs; `apply_list` forgets them on tab switch. IQ5 a failed `run.list` clears runs, logs, subscriptions. IQ6 failed history fabricates Idle and drops `pending`. IQ7 `always_allow` reset on reconnect reattach. IQ8 `note_denied` re-creates a forgotten workspace entry.

**C++ (Tasks 11, 12).** CP1 raw pointers across `m_ask`; `deleteLater` runs in the nested loop. CP2 `tabWorkspaceId(m_displayGroup, index)` resolved after `menu.exec()`. CP3 `m_workspaceId` read after `box.exec()` in discard/merge/squash/PR. CP4 setup PTY orphaned when the page dies before the reply; daemon closes host PTYs only at shutdown. CP5 `m_status` (`NewAgentDialog.cpp:179`) and SetupPage `detailLabel` lack `setTextFormat`. CP6 group index emitted after `exec`; `MainWindow::onCloseGroup` (`:904`) resolves it later.

**Server (Task 6).** fs.rs:46 canonicalise-then-open TOCTOU; connection.rs:46 uncapped pre-hello `read_line`; connection.rs:112 panicking handler never answers; pty.rs:440 `close_workspace` never escalates; fs_watch.rs:334 recursive watch over `node_modules`; fs_watch.rs:297 lock held across `watch()`; daemon.rs:151 sharing violation reported as Busy.

**Cleanup/altitude (Tasks 13–16).** CD1 three git runner copies (`git/mod.rs:106/157/227`). CD2 serial independent git spawns (`repo.rs:197`, `changes.rs:43`, `lifecycle.rs:412/659`). CD3 fsync on the runtime from the reader task (`agents/mod.rs:293`, `registry.rs:166`). CD4 open/close per transcript delta (`agents/mod.rs:65`). CD5 shim/forwarder readiness duplicated (`lifecycle.rs:205`, `manager.rs:861`). CD6 tail/exit helpers duplicated (`manager.rs:956`, `claude.rs:490`; formats deliberately differ). CD7 dead `path_str`, `Registry::save`, `Bridge::probe` method, `DataDirs::bin`, three `fn s`. CD8 argv split ×3 (`claude.rs:264`, `pr.rs:169`, `pty.rs:161`). CI1 every status event → debounced fsync of state.json (`MainWindow.cpp:508`). CI2 per-editor `subscribe_all` clones every event. CI3 O(n²) transcript re-render per delta (`TranscriptView.cpp:312`). CI4 rows JSON parsed every paint incl. blink (`TerminalWidget.cpp:173`). CI5 dead model surface (`group_model.rs:210`, controller create_* , `EditorBuffer::replace_all`, `port_override`); `TerminalGrid::size` has a test caller — keep. CI6 hard-coded `#eb5757`/`#4caf50` at `GroupBar.cpp:37`, `TerminalWidget.cpp:26`, `NewAgentDialog.cpp:115`, `SetupPage.cpp:233`. CI7 `add_tab` hand-builds `AgentTab`. CI8 copy-pasted workspace-op prologue (`app_controller.rs:2618+`) and create variants. AL1 `onNewAgent` pre-inspect pipeline duplicates the dialog's. AL2 op-name string routing of failures (`MainWindow.cpp:1144`). AL3 unconditional prereq recheck on Settings close (`:324`). AL4 prereqs JSON parsed twice, three-way auto-open state (`:335`). AL5 three repo classifiers (`lifecycle.rs:500`). AL6 stderr-phrase matcher in `worktree::remove` (`:235`). AL7 two name validators; `feat:x` passes both and git refuses the ref.
