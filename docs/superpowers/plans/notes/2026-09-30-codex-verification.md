# Codex integration verification — 2026-09-30

Worktree: `.superpowers/worktrees/codex-adapter`, branch `feat/codex-adapter`.
The approved design and implementation plan remain the scope of this work.

## Review fixes

An independent review of the branch and pending implementation found two
important defects. Both were reproduced before their fixes:

- Two overlapping Codex approvals replaced the first pending request in the
  transcript. Answering one also incorrectly moved the adapter to Working while
  another request remained unanswered. The transcript now queues approvals,
  advances on local and recorded replies, preserves Codex approvals while other
  tools run, and clears the queue on turn completion or terminal state. The
  adapter stays WaitingPermission until all approvals have been answered.
- Removing a rejected OpenAI API key preserved the Codex authentication failure
  override and blocked fallback to an existing ChatGPT login. Successful key
  replacement and removal now retire that override; failed credential-store
  operations return before changing it. Claude's failure state is unaffected.

Follow-up review found no further important issues in those fixes. Regression
coverage includes replies out of order, local replies followed by daemon
acknowledgments, and termination with multiple approvals pending.

## Verification

- `scripts/test-daemon.ps1`: 617 passed, 4 ignored. Real bubblewrap, companion
  isolation, network, and lifetime tests executed. The ignored tests are the
  three explicit live Codex checks and an 8 GiB sparse-file test.
- Focused Windows IDE verification with `require-qt`: library, transcript,
  backend settings, backend authentication UI, and QObject/widget targets passed.
- `cargo fmt --all -- --check`: passed.
- Explicit live `codex_live_backend` checks: 2 passed. ChatGPT-authenticated tools
  and thread resume passed both in a new process and after daemon restart with an
  in-place workspace, against the production backend and pinned 0.158.0 binaries.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --workspace --features require-qt -j 1`: 906 passed, 1 ignored
  (the 8 GiB sparse-file test). Qt widgets, backend authentication UI, smoke,
  restore/reconnect, and the staged deployed-runtime smoke all executed.
- `git diff --check`: passed with the repository's normal line-ending settings.

The first sandboxed Windows build hit MSVC LNK1104 on the IDE import library;
running outside the execution sandbox with one build job got past linking.
The first WSL run failed the real companion test with "workspace is not ready";
the isolated case, complete companion target, and subsequent full WSL suite
passed. This transient failure was not silently treated as a successful run.

The first Windows `require-qt` workspace run stopped at packaged smoke because
`BS_PACKAGED_EXE` was unset. The final run uses the current development executable
staged with the deployed Qt/CRT runtime. This verifies that deployed runtime;
it is not a new release package or installer validation.

Live API-key authentication remains explicitly deferred by the user, as recorded
in the preflight ledger. No new claim of live API-key validation is made. The
interactive live IDE workflow is not covered by the offline widget/fake-daemon
tests; backend live checks exercise the production daemon integration.

Raw verification logs are local artifacts under
`.superpowers/sdd/2026-09-28-codex-adapter/verification-*.log`; they are not committed.
