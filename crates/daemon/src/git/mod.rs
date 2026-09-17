pub mod merge;
pub mod pr;
pub mod repo;
pub mod worktree;

use bondsymphonic_proto::{ErrorCode, RpcError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::process::Command;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Git {
    env: Vec<(String, String)>,
    /// `key=value` pairs passed as `-c key=value` before the subcommand on
    /// every invocation. Command-line config outranks every config *file*,
    /// which is what makes it usable against a config an agent can write.
    config: Vec<String>,
}

/// `core.quotePath=false` on every `Git` the daemon builds.
///
/// Git's default is to render each path byte above 0x7f as a C-style octal
/// escape and wrap the name in double quotes, so `håndbog.md` reaches a client
/// as `"h\303\245ndbog.md"` — a file the IDE cannot open and a name the user
/// does not recognise. Every command that prints a path obeys it:
/// `diff --name-only` for the conflict list, `diff --name-status`,
/// `status --porcelain`, `rev-list --objects`, `worktree list`.
///
/// It belongs in the *default* rather than in the three `Layout` constructors,
/// because "set it where it cannot be forgotten" only holds if a bare
/// `Git::new()` has it too: `Daemon::git` is one, and it already reaches
/// `status --porcelain` through `repo::inspect` and `rev-list --objects`
/// through `verify_absorbed`. A `Git` with no path-printing call is unaffected —
/// the option costs one argument and changes nothing for an ASCII tree.
const QUOTE_PATH: (&str, &str) = ("core.quotePath", "false");

impl Default for Git {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            config: vec![format!("{}={}", QUOTE_PATH.0, QUOTE_PATH.1)],
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Raw stdout from [`Git::run_bytes`], capped. `stdout` holds at most `cap + 1`
/// bytes; the extra byte is what tells a stream that is exactly `cap` long from
/// a longer one.
#[derive(Debug, Clone)]
pub struct GitBytes {
    pub stdout: Vec<u8>,
}

/// Times every git command the daemon runs, at debug.
///
/// A `repo.inspect` that took longer than the IDE's 30 s timeout on a large
/// repository under `/mnt/c` left nothing in the log to say which of its six
/// commands had been slow, and the answer to that has to be in the log the next
/// time rather than in another round of guessing. Three fields are what it takes
/// to act on one: the subcommand, the elapsed time, and the working tree it ran
/// in. Debug rather than info, because a busy workspace runs a handful of these
/// per second.
///
/// The `-c` prefix is deliberately not part of it: it is a fixed policy of this
/// `Git`, the same on every line, and `args[0]` is the subcommand the caller
/// asked for.
fn log_elapsed(args: &[&str], cwd: &Path, started: std::time::Instant) {
    tracing::debug!(
        subcommand = args.first().copied().unwrap_or(""),
        elapsed_ms = started.elapsed().as_millis() as u64,
        cwd = %cwd.display(),
        "git command finished"
    );
}

impl Git {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// Adds a `-c key=value` override applied to every command this `Git` runs.
    pub fn with_config(mut self, key: &str, value: &str) -> Self {
        self.config.push(format!("{key}={value}"));
        self
    }

    /// The `git` every runner below starts from: this `Git`'s `-c` prefix, its
    /// environment, the working directory and the three pipes.
    ///
    /// **The one place `LC_ALL`, `GIT_TERMINAL_PROMPT` and
    /// `GIT_NO_REPLACE_OBJECTS` are set.** All three are load-bearing rather
    /// than tidiness, and a runner that forgot one would be wrong in a way
    /// nothing shouts about: half the daemon's error handling reads git's own
    /// wording — "not a git repository", "must be run in a work tree", "does
    /// not exist in" — and a translated git turns every one of those into an
    /// unexplained failure, while a git that may prompt hangs on a credential
    /// question nobody is there to answer until the 60 s timeout. Adding a
    /// fourth runner gets them by construction, which is the point.
    ///
    /// `GIT_NO_REPLACE_OBJECTS` because `refs/replace/<oid>` makes git read one
    /// commit or tree where another is named, and an agent can write
    /// `refs/replace` in both workspace kinds — the shared ref store is
    /// read-write in a worktree workspace and the whole `.git` is in an in-place
    /// one. Without it an agent could decide what the daemon's own diff shows
    /// the user before they press Merge, and what that merge then merges. The
    /// cost is that a `git replace` the *user* made is honoured by their own git
    /// and not by the daemon's; that is the right way round, because the
    /// daemon's job here is to report what is actually recorded.
    ///
    /// Stdin is `null` here; [`Git::run_with_stdin`] is the one caller that
    /// overrides it.
    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut argv: Vec<&str> = Vec::with_capacity(self.config.len() * 2 + args.len());
        for c in &self.config {
            argv.push("-c");
            argv.push(c);
        }
        argv.extend_from_slice(args);
        let mut cmd = Command::new("git");
        cmd.args(&argv)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    pub async fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, RpcError> {
        let command = describe(args);
        let mut cmd = self.command(cwd, args);
        let out = timed(&command, args, cwd, async move {
            cmd.output().await.map_err(|e| e.to_string())
        })
        .await?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        succeeded(&command, out.status, &stderr)?;
        Ok(GitOutput { stdout, stderr })
    }

    /// Runs git with `stdin` fed to it on standard input.
    ///
    /// For the handful of plumbing commands that take their arguments there and
    /// nowhere else — `pack-objects --revs` is the one the daemon needs. The
    /// input is written in full before anything is read back, which is only
    /// safe because these inputs are a few short lines: a caller that fed in
    /// more than a pipe buffer could deadlock against a child blocked on a full
    /// stdout, so this is not the method for bulk input.
    pub async fn run_with_stdin(
        &self,
        cwd: &Path,
        args: &[&str],
        stdin: &str,
    ) -> Result<GitOutput, RpcError> {
        use tokio::io::AsyncWriteExt;

        let command = describe(args);
        let mut cmd = self.command(cwd, args);
        cmd.stdin(Stdio::piped());
        let run = async {
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            {
                // Dropped at the end of this block, which is what closes the
                // pipe; git waits for end-of-input before it does anything.
                let mut si = child.stdin.take().expect("stdin is piped");
                si.write_all(stdin.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                si.flush().await.map_err(|e| e.to_string())?;
            }
            child.wait_with_output().await.map_err(|e| e.to_string())
        };
        let out = timed(&command, args, cwd, run).await?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        succeeded(&command, out.status, &stderr)?;
        Ok(GitOutput { stdout, stderr })
    }

    /// Runs git and returns raw stdout, keeping at most `cap + 1` bytes.
    ///
    /// [`Git::run`] buffers the whole of stdout and lossily decodes it, which is
    /// wrong for object contents on both counts: a blob may be binary, and it may
    /// be arbitrarily large. Git objects are as unbounded as the files in a
    /// worktree — the reason `crate::fs::read_file` caps its own reads — so only
    /// the first `cap + 1` bytes are kept, the extra byte being what tells content
    /// exactly `cap` long from longer content.
    ///
    /// Anything past the cap is read and dropped rather than left in the pipe. Git
    /// therefore always gets to finish, so the exit status still means what it
    /// says and a caller can tell "no such object" from "here is the object".
    /// Cutting the pipe short instead would be faster on a huge blob but would
    /// trade a bounded read for a killed child and a meaningless status.
    pub async fn run_bytes(
        &self,
        cwd: &Path,
        args: &[&str],
        cap: usize,
    ) -> Result<GitBytes, RpcError> {
        use tokio::io::AsyncReadExt;

        let command = describe(args);
        let mut cmd = self.command(cwd, args);
        let read = async {
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            // Both pipes were just configured above.
            let mut out = child.stdout.take().expect("stdout is piped");
            let mut err = child.stderr.take().expect("stderr is piped");
            let mut stdout = Vec::new();
            let mut stderr = String::new();
            // Drained together, not one after the other: a git blocked on a full
            // stderr pipe stops writing stdout, so a reader that finished stdout
            // before starting stderr would wait for an EOF that never comes.
            let (o, e) = tokio::join!(
                async {
                    (&mut out)
                        .take(cap as u64 + 1)
                        .read_to_end(&mut stdout)
                        .await?;
                    tokio::io::copy(&mut out, &mut tokio::io::sink()).await?;
                    Ok::<_, std::io::Error>(())
                },
                err.read_to_string(&mut stderr),
            );
            o.map_err(|e| e.to_string())?;
            e.map_err(|e| e.to_string())?;
            let status = child.wait().await.map_err(|e| e.to_string())?;
            Ok::<_, String>((stdout, stderr, status))
        };
        let (stdout, stderr, status) = timed(&command, args, cwd, read).await?;
        succeeded(&command, status, &stderr)?;
        Ok(GitBytes { stdout })
    }
}

/// A path as a git argument.
///
/// Lossy on purpose and in one place. Every runner above takes `&[&str]`, so a
/// path has to become one somewhere, and a name with an unpaired surrogate in
/// it is a file the daemon will fail to act on either way — with a replacement
/// character in the argument, or with no argument at all. There used to be
/// three of these, one per module, which is three chances to pick a different
/// answer to the same question.
pub fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The command as the daemon reports it, which names the subcommand only: the
/// `-c` prefix is a fixed policy of the `Git` that ran it, not part of what the
/// caller asked for.
fn describe(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

/// Awaits one git invocation under [`GIT_TIMEOUT`], times it, and flattens the
/// two ways it can fail to produce output into one [`RpcError`].
///
/// Neither is an exit code: a git that could not be spawned and a git that ran
/// past the timeout have no status to report, so both come back with `None`
/// where the exit code goes. Whether the command *succeeded* is a separate
/// question, asked by [`succeeded`] once there is a status to ask about.
async fn timed<T>(
    command: &str,
    args: &[&str],
    cwd: &Path,
    run: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, RpcError> {
    let started = std::time::Instant::now();
    let finished = tokio::time::timeout(GIT_TIMEOUT, run).await;
    log_elapsed(args, cwd, started);
    match finished {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(git_error(command, None, &e)),
        Err(_) => Err(git_error(command, None, "timed out after 60s")),
    }
}

/// The tail every runner shares: a non-zero exit is the error, carrying git's
/// own exit code and stderr so callers can tell its refusals apart.
fn succeeded(
    command: &str,
    status: std::process::ExitStatus,
    stderr: &str,
) -> Result<(), RpcError> {
    if !status.success() {
        return Err(git_error(command, status.code(), stderr.trim()));
    }
    Ok(())
}

/// Copies into the main repository's object store every object reachable from
/// `include` but not from `exclude`.
///
/// A workspace's commits are written to a private object directory that is
/// deleted with the workspace (daemon design §5.2). The daemon reads them
/// through `GIT_ALTERNATE_OBJECT_DIRECTORIES`, so a merge or a push leaves the
/// main repository holding refs — the base branch, `refs/remotes/origin/...` —
/// that point at objects the user's own git cannot see and that disappear the
/// moment that workspace is destroyed. This is what makes those refs stand on
/// their own.
///
/// **Not `git repack -a -d`**, which §5.2 suggests: `repack -a` walks *every*
/// ref, so a second workspace whose commits live in a *different* private
/// directory makes it fail — and it fails after deleting the loose objects it
/// had already packed, leaving the repository unreadable. Measured on git 2.52.
/// `pack-objects` over an explicit revision range touches only the objects
/// asked for, which also makes it proportional to the merge rather than to the
/// repository.
///
/// The pack is not trusted on the strength of an exit code. Whether the objects
/// really are readable without the alternate is a question with a direct
/// answer, and the consequence of getting it wrong — a `main` that stops
/// resolving the next time a workspace is destroyed — is bad enough to be worth
/// asking. One retry, then the error goes back to the caller.
pub async fn absorb_objects(
    git: &Git,
    repo: &Path,
    git_common: &Path,
    include: &str,
    exclude: &str,
) -> Result<(), RpcError> {
    if include == exclude {
        return Ok(());
    }
    // Written straight into `objects/pack`, named the way git names its own
    // packs: `pack-objects` builds each file under a temporary name and renames
    // it into place, which is exactly how `git repack` puts packs here.
    let pack_dir = git_common.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(|e| {
        RpcError::new(
            ErrorCode::IoError,
            format!("cannot create {}: {e}", pack_dir.display()),
        )
    })?;
    let prefix = pack_dir.join("pack").to_string_lossy().into_owned();
    let revs = format!("{include}\n^{exclude}\n");
    let mut last: Option<RpcError> = None;
    for attempt in 1..=2 {
        if let Err(e) = git
            .run_with_stdin(
                repo,
                &[
                    "pack-objects",
                    "--revs",
                    "--delta-base-offset",
                    "-q",
                    &prefix,
                ],
                &revs,
            )
            .await
        {
            tracing::warn!(repo = %repo.display(), attempt, "packing {include} failed: {}", e.message);
            last = Some(e);
            continue;
        }
        match verify_absorbed(repo, include, exclude).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    repo = %repo.display(),
                    attempt,
                    "the pack for {include} is not readable without the workspace's objects: {}",
                    e.message
                );
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| RpcError::internal("packing produced no result")))
}

/// Reads `include ^exclude` the way the *user's* git would: no alternate object
/// directory anywhere in the environment.
///
/// The daemon never sets `GIT_ALTERNATE_OBJECT_DIRECTORIES` process-wide — it
/// is added per command by [`worktree::Layout::daemon_git`] — so a plain [`Git`]
/// inherits none of it, and this fails exactly where the user's own git would.
///
/// `rev-list --objects` reads every commit and every tree in the range, which
/// is what proves the pack landed and was indexed. It lists blob ids without
/// opening them, but blobs travel in the same pack as the trees that name them,
/// so a pack whose trees are readable is a pack whose blobs are there too.
async fn verify_absorbed(repo: &Path, include: &str, exclude: &str) -> Result<(), RpcError> {
    let plain = Git::new();
    plain
        .run(repo, &["cat-file", "-e", &format!("{include}^{{commit}}")])
        .await?;
    plain
        .run(
            repo,
            &["rev-list", "--objects", include, &format!("^{exclude}")],
        )
        .await?;
    Ok(())
}

/// Serialises the operations that move a repository's *base* branch.
///
/// A merge advances the base branch, and both the merge and the `pack-objects`
/// that follows it write to the one shared object store and index. Two merges
/// of two workspaces of the same repository race for both: the second loses on
/// `index.lock`, or — on the scratch-worktree path — finds the base branch
/// already checked out by the first one's worktree. Nothing else serialises
/// them, because each request is its own task.
///
/// Keyed by the repository, so merges in unrelated repositories still run at
/// the same time. Held for the whole of a merge and for the push half of
/// `create_pr`.
fn repo_locks() -> &'static parking_lot::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// The lock for one repository. Canonicalised as far as the filesystem allows,
/// so two workspaces created with differently spelled paths to the same
/// repository still take the same lock.
///
/// [`repo::canonical_ish`] rather than `std::fs::canonicalize`, which answers
/// nothing at all for a path whose last component does not exist yet — and that
/// is exactly the path `workspace.create` with `init_if_missing` is given. With
/// the raw path as the key, two creates naming one missing folder two ways each
/// took a lock of its own, so neither waited for the other and both went on to
/// `git init` the same directory.
pub fn repo_lock(repo: &Path) -> Arc<tokio::sync::Mutex<()>> {
    repo_locks()
        .lock()
        .entry(repo::canonical_ish(repo))
        .or_default()
        .clone()
}

/// The merge or push happened, but the commits it brought in are still only in
/// the workspace's private object directory.
///
/// This is the one outcome that must never be reported as success. The branch
/// now points at objects the user's own git cannot read and that
/// `workspace.destroy` would delete for good, and a `tracing::warn!` is not a
/// channel any user reads. `flag` is `"merged"` or `"pushed"`, so the IDE can
/// say that the work landed even though the call failed.
///
/// `data.reason` is `"objects_stranded"`, the same machine-readable tag
/// `"base_dirty"` and `"conflict"` use, so the IDE branches on one field across
/// every merge and PR outcome rather than on prose.
pub fn objects_stranded(what: &str, flag: &str, detail: &str) -> RpcError {
    RpcError::new(
        ErrorCode::Internal,
        format!(
            "the {what} completed, but the commits it brought in could not be copied out of the \
             workspace's private object store into the repository: {detail}. Do not destroy this \
             workspace: destroying it would delete objects the branch now points at."
        ),
    )
    .with_data(serde_json::json!({ "reason": "objects_stranded", flag: true }))
}

pub fn git_error(command: &str, exit_code: Option<i32>, stderr: &str) -> RpcError {
    RpcError::new(ErrorCode::GitError, format!("{command} failed: {stderr}")).with_data(
        serde_json::json!({ "command": command, "exit_code": exit_code, "stderr": stderr }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A `MakeWriter` that keeps what a subscriber writes, so a test can read
    /// the log lines back.
    #[derive(Clone, Default)]
    struct Captured(Arc<StdMutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `f` with a debug-level subscriber of our own and returns everything
    /// it logged. The runtime is built inside the guard because the subscriber
    /// is thread-local and a `#[tokio::test]` would set it after the fact.
    fn logs_of(f: impl std::future::Future<Output = ()>) -> String {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(f);
        });
        captured.text()
    }

    /// `repo.inspect` timed out at 30 s on a large repository on `/mnt/c` and
    /// nothing in the log said which of its six git commands had been slow.
    /// Every command the daemon runs is timed, and the three fields are what it
    /// takes to act on one: which subcommand, how long, and in which working
    /// tree.
    #[test]
    fn every_git_command_logs_its_elapsed_time_at_debug() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let logs = logs_of(async move {
            Git::new().run(&cwd, &["--version"]).await.unwrap();
            // A command that fails is timed too: a slow failure is exactly the
            // one this has to be able to explain.
            let _ = Git::new().run(&cwd, &["rev-parse", "--git-dir"]).await;
        });

        assert_eq!(
            logs.matches("elapsed_ms").count(),
            2,
            "both commands are timed, the failing one included: {logs}"
        );
        assert!(logs.contains("subcommand=\"--version\""), "{logs}");
        assert!(logs.contains("subcommand=\"rev-parse\""), "{logs}");
        assert!(
            logs.contains(&dir.path().display().to_string()),
            "the working directory is part of the line: {logs}"
        );
    }

    /// The C locale and the silenced terminal prompt reach every git the daemon
    /// runs, whatever that `Git` was built with.
    ///
    /// Both are load-bearing: the daemon reads git's own wording in half a dozen
    /// places, and a translated git turns each of those into an unexplained
    /// failure, while a git allowed to prompt hangs on a credential question
    /// until the 60 s timeout. Asserted on the builder rather than on a runner
    /// because the builder is what a fourth runner would inherit them from.
    #[test]
    fn the_builder_gives_every_command_the_c_locale_and_no_terminal_prompt() {
        let git = Git::new()
            .with_env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/objects")
            .with_config("core.hooksPath", "/nohooks");
        for args in [&["status"][..], &["worktree", "prune"][..]] {
            let cmd = git.command(Path::new("."), args);
            let env: HashMap<String, Option<String>> = cmd
                .as_std()
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            assert_eq!(env.get("LC_ALL"), Some(&Some("C".to_string())), "{args:?}");
            assert_eq!(
                env.get("GIT_TERMINAL_PROMPT"),
                Some(&Some("0".to_string())),
                "{args:?}"
            );
            // The caller's own environment is still there: the fixed pair is
            // added to it, not instead of it.
            assert_eq!(
                env.get("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
                Some(&Some("/objects".to_string())),
                "{args:?}"
            );
        }
    }

    /// And they are set in exactly one place, so there is no second runner to
    /// keep in step with the first.
    ///
    /// Read off the module's own source, because "only one place" is a property
    /// of the text rather than of any value a test could call for. The test
    /// module is cut off first, or this assertion would count itself.
    #[test]
    fn the_fixed_environment_is_set_in_exactly_one_place() {
        let source = include_str!("mod.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a production half");
        for key in ["LC_ALL", "GIT_TERMINAL_PROMPT"] {
            // The assignment, not every mention: the builder's own doc comment
            // names both keys, and that is the documentation, not a second
            // place they are set.
            assert_eq!(
                production.matches(&format!(".env(\"{key}\"")).count(),
                1,
                "{key} is set in more than one place; every git command has to get it from \
                 `Git::command` alone"
            );
        }
    }

    /// Two spellings of one repository take one lock, even when the directory
    /// is not there yet.
    ///
    /// The key used to be `std::fs::canonicalize`, which answers nothing at all
    /// for a path whose last component is missing — and a missing path is
    /// exactly what `workspace.create` with `init_if_missing` is given. Two
    /// concurrent creates naming one missing folder two ways each took a lock of
    /// its own, so neither waited for the other and both ran `git init`.
    /// [`repo::canonical_ish`] resolves as much of the path as exists.
    #[test]
    fn two_spellings_of_one_missing_repository_take_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let plain = sub.join("not-there-yet");
        let roundabout = sub.join("..").join("sub").join("not-there-yet");

        // The key first, then the lock. The two halves fail for entirely
        // different reasons — a platform whose `canonical_ish` resolves `..`
        // differently, against a lock map that stopped keying on it — and a
        // single assertion at the end could not say which, least of all on the
        // platform the reader is not sitting in front of.
        let (a, b) = (
            repo::canonical_ish(&plain),
            repo::canonical_ish(&roundabout),
        );
        assert_eq!(
            a,
            b,
            "canonical_ish gives {} and {} two keys for one directory",
            a.display(),
            b.display()
        );
        assert!(
            Arc::ptr_eq(&repo_lock(&plain), &repo_lock(&roundabout)),
            "{} and {} both key on {}, so they must share one lock",
            plain.display(),
            roundabout.display(),
            a.display()
        );
    }
}
