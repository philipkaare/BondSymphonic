use super::{path_arg, repo, Git};
use bondsymphonic_proto::{ErrorCode, RpcError, WorkspaceId};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub repo: PathBuf,
    pub git_common: PathBuf,
    pub name: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub objects_dir: PathBuf,
    /// An empty daemon-owned directory, used as `core.hooksPath`. See
    /// [`crate::workspace::DataDirs::no_hooks`].
    pub no_hooks_dir: PathBuf,
}

impl Layout {
    pub fn ref_dir(&self) -> PathBuf {
        self.git_common.join("refs/heads/bs").join(&self.name)
    }
    pub fn reflog_dir(&self) -> PathBuf {
        self.git_common.join("logs/refs/heads/bs").join(&self.name)
    }
    /// Relies on worktree directory basenames (the unique `ws_` ids) never colliding;
    /// git otherwise suffixes the per-worktree gitdir name (e.g. `ws_1`, `ws_11`), which
    /// would desync this from the actual `<git_common>/worktrees/<...>` directory.
    pub fn worktree_gitdir(&self) -> PathBuf {
        let base = self
            .worktree_path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        self.git_common.join("worktrees").join(base)
    }
    /// Subpaths of the main `.git` that must be bind-mounted read-write into the sandbox.
    pub fn rw_git_paths(&self) -> Vec<PathBuf> {
        vec![self.worktree_gitdir(), self.ref_dir(), self.reflog_dir()]
    }
    /// The one file inside the read-write worktree gitdir that the agent must
    /// not be able to write.
    ///
    /// On a repository with `extensions.worktreeConfig` enabled, git reads this
    /// as repository config for the worktree, and repository config is arbitrary
    /// code: a `filter.<anything>.clean` driver runs whenever `git status` has to
    /// re-hash a file the agent has touched. The daemon owns the file and the
    /// sandbox gets it read-only.
    pub fn config_worktree(&self) -> PathBuf {
        self.worktree_gitdir().join("config.worktree")
    }
    /// Env for git *inside* the sandbox: new objects go to the private dir.
    pub fn sandbox_git_env(&self) -> Vec<(String, String)> {
        vec![
            ("GIT_OBJECT_DIRECTORY".into(), path_arg(&self.objects_dir)),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
                path_arg(&self.git_common.join("objects")),
            ),
        ]
    }
    /// Git for the daemon side: main store primary, private objects as alternate.
    ///
    /// Only safe for commands that run in the *main* repository, which is never
    /// bind-mounted into a sandbox. Anything that touches the worktree must use
    /// [`Layout::worktree_git`].
    ///
    /// `core.hooksPath` is pinned at the empty daemon-owned directory, for the
    /// main repository and for the scratch worktree a merge checks out under it.
    /// A merge the daemon runs is not the user typing `git merge`: it happens on
    /// a schedule the user did not choose, over content an agent wrote, and a
    /// `post-merge` or `pre-commit` hook firing there would run repository code
    /// nobody in front of the screen asked for. Daemon design §5.4 states the
    /// rule and its one exception.
    ///
    /// The [`NEUTRALISED_CONFIG`] list is deliberately **not** applied here.
    /// Those keys name the user's own filter and merge drivers — `git-lfs`'s
    /// `filter.lfs.clean` above all — and the main repository's config is not
    /// agent-writable, so emptying them would not close a hole, it would corrupt
    /// the user's checkout by writing LFS pointers where their files should be.
    /// A `.gitattributes` in the merged tree still chooses which of those
    /// drivers run, which is the residual risk §5.4 records.
    pub fn daemon_git(&self) -> Git {
        Git::new()
            .with_env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                path_arg(&self.objects_dir),
            )
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir))
    }

    /// [`Layout::daemon_git`] with the repository's own hooks left in place, for
    /// `git push` and nothing else.
    ///
    /// `pre-push` is not decoration: it is how `git-lfs` uploads the large
    /// objects a push needs, and a push that skips it puts pointer files on the
    /// remote with nothing behind them. A push is also the one daemon-side git
    /// operation the user explicitly asked for by name, through **Create PR**.
    pub fn daemon_push_git(&self) -> Git {
        Git::new().with_env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            path_arg(&self.objects_dir),
        )
    }

    /// Git for daemon-side commands that run *against a workspace worktree*.
    ///
    /// Every file git would use to discover its repository from the working
    /// directory — `<worktree>/.git` and `<git_common>/worktrees/<id>/commondir`
    /// — is bind-mounted read-write into the sandbox, so an agent can point
    /// discovery at a gitdir of its own and have the daemon read that gitdir's
    /// `config`. Pinning all four paths explicitly takes discovery out of the
    /// agent's hands: the environment outranks anything in the worktree.
    pub fn worktree_git(&self) -> Git {
        let mut git = Git::new()
            .with_env("GIT_DIR", path_arg(&self.worktree_gitdir()))
            .with_env("GIT_COMMON_DIR", path_arg(&self.git_common))
            .with_env("GIT_WORK_TREE", path_arg(&self.worktree_path))
            .with_env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                path_arg(&self.objects_dir),
            )
            .with_config("extensions.worktreeConfig", "false")
            // Not in the list below: an empty `core.hooksPath` does not disable
            // hooks, it moves them to the filesystem root, so this one needs a
            // real directory that is always empty.
            .with_config("core.hooksPath", &path_arg(&self.no_hooks_dir));
        for key in NEUTRALISED_CONFIG {
            git = git.with_config(key, "");
        }
        git
    }
}

/// Config keys whose value git executes as a command, cleared on the command
/// line for every daemon-side worktree command.
///
/// **Defence in depth, not the primary control.** The primary control is the
/// read-only bind of [`Layout::config_worktree`], because a key list cannot
/// close this class of hole: `filter.<any name>.clean` is a command git runs
/// whenever it re-hashes a file, and the driver name is chosen by whoever wrote
/// the config. This list only shortens the reach of the keys with fixed names,
/// for a repository whose `config.worktree` somehow became writable anyway and
/// for the `noop` backend, which has no mounts and therefore no protection at
/// all — it is a development and test backend and does not sandbox anything.
///
/// Pinning the repository is not enough by itself either. The per-worktree
/// gitdir is writable by the agent (it needs `HEAD`, the index and their lock
/// files), and git reads `config.worktree` from it whenever the repository has
/// `extensions.worktreeConfig` enabled. `-c extensions.worktreeConfig=false`
/// does **not** turn that off — git takes the extension from the repository
/// format during setup, before command-line config exists. Measured against git
/// 2.43 and 2.52: `core.fsmonitor` is the one `git status` reaches by name.
///
/// Every key here is emptied, which disables it. `core.hooksPath` is *not* here
/// for exactly that reason — emptying it relocates hooks to `/` instead of
/// disabling them — so it is set to an empty daemon-owned directory in
/// [`Layout::worktree_git`]. Any key added here has to be checked the same way:
/// an empty value has to mean "off", not "somewhere else".
const NEUTRALISED_CONFIG: &[&str] = &[
    "core.fsmonitor",
    "core.sshCommand",
    "core.gitProxy",
    "core.askPass",
    "core.editor",
    "core.pager",
    "core.alternateRefsCommand",
    "sequence.editor",
    "diff.external",
    "credential.helper",
    "gpg.program",
    "uploadpack.packObjectsHook",
];

/// Whether a [`remove`] may delete the workspace's branch.
///
/// The branch is the one thing in a `Layout` that is **not** private to the
/// workspace being removed. Its name comes from the workspace *name*, and so do
/// `refs/heads/bs/<name>/` and that directory's reflog; the worktree and the
/// object directory are named after the workspace id and belong to one
/// workspace alone. So a cleanup that deletes the branch unconditionally is a
/// cleanup that can delete somebody else's work: two creates of one name, one
/// of them failing, and the branch deletion lands on the branch the other one is
/// checking out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveBranch {
    /// Delete it. The workspace owns the branch, which is `workspace.destroy`
    /// and the unwinding of a creation that got as far as making it.
    Always,
    /// Leave it, and leave the name-shared ref and reflog directories with it.
    /// For a cleanup that cannot show the branch is its own.
    Never,
}

/// True when `git worktree add -b` refused because the branch is already there.
///
/// Three wordings, because git has three, and the third is the one this is
/// really for:
///
/// * `a branch named <b> already exists` — git checked before it started, so
///   the branch was there all along;
/// * `<b> is already used by worktree at <path>` — it is there and checked out;
/// * `cannot lock ref refs/heads/<b>: reference already exists` — git got past
///   its own check and lost the ref transaction. That is exactly the two-creates
///   race: both calls looked, neither saw the branch, and one of them wrote it
///   first.
///
/// Matched on git's wording and not on the exit code, which is its general
/// fatal. None of the three can be confused with the worktree *directory*
/// already existing, which git reports as `<path> already exists` — no ref, no
/// branch, no lock.
///
/// **This rests on a guarantee made elsewhere:** the phrases above are git's
/// English, and a git running under any other locale prints none of them, so
/// every branch conflict would be classified as an unrelated failure. What
/// makes that impossible is [`crate::git::Git::command`] setting `LC_ALL=C` on
/// every git this daemon starts, in one place, for exactly this reason. The
/// dependency is not local: a fourth runner that built its own `Command`
/// instead would break this function without touching this file.
fn branch_is_already_there(e: &RpcError) -> bool {
    e.data
        .as_ref()
        .and_then(|d| d["stderr"].as_str())
        .is_some_and(|s| {
            s.contains("a branch named")
                || s.contains("already used by worktree")
                || s.contains("reference already exists")
        })
}

/// What a failed [`create`] left behind for its caller to clean up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leftovers {
    /// Nothing the caller's unwind would reach: either the call refused before
    /// it made anything, or it has already taken back everything it made.
    Nothing,
    /// Directories, a branch or a worktree registration this call made and
    /// could not take back itself.
    Something,
}

/// A failed [`create`]: what went wrong, and whether anything is left of it.
///
/// The second half is a fact the caller cannot safely re-derive. Three of
/// `create`'s refusals happen before it has made a directory, a branch or a
/// registration — a name already taken, a base branch that does not exist, and
/// a `branch_exists` that failed outright — and for each of those the caller's
/// unwind would run `git worktree prune` **on the user's repository**, which
/// forgets every registration whose directory is not there at that moment: a
/// worktree on a disk that is not mounted, or one the user moved aside. Reading
/// the error *code* instead cannot tell those three apart from a real failure
/// halfway through, which is how the first version of this skip closed the
/// `Conflict` and left its two siblings open.
#[derive(Debug)]
pub struct CreateFailed {
    pub error: RpcError,
    pub left: Leftovers,
}

impl CreateFailed {
    /// For a refusal that happened before anything was made.
    fn nothing(error: RpcError) -> Self {
        Self {
            error,
            left: Leftovers::Nothing,
        }
    }

    /// For a failure with no unwind of its own, after something was made.
    fn something(error: RpcError) -> Self {
        Self {
            error,
            left: Leftovers::Something,
        }
    }

    /// For a failure [`create`] has already tried to unwind. Whether anything
    /// is left is whether that unwind worked, which is the one honest answer —
    /// and it keeps the caller's cleanup as the safety net for the case where
    /// it did not.
    fn after(unwind: Result<(), RpcError>, error: RpcError) -> Self {
        Self {
            error,
            left: match unwind {
                Ok(()) => Leftovers::Nothing,
                Err(_) => Leftovers::Something,
            },
        }
    }
}

/// The one answer for "that branch is somebody else's", whichever step found
/// out. `workspace.create` turns it into the `Conflict` a client sees when a
/// name is taken.
fn branch_conflict(branch: &str) -> RpcError {
    RpcError::new(
        ErrorCode::Conflict,
        format!("branch {branch} already exists"),
    )
}

/// Creates the workspace's branch and worktree.
///
/// Through [`Layout::daemon_git`], so none of the repository's own hooks run.
/// `git worktree add` fires `post-checkout` and, for the branch it creates,
/// `reference-transaction`; neither is the user typing a git command. Opening a
/// workspace in the IDE must not execute code out of the repository being
/// opened, on a schedule nobody chose and with no way to see it happen.
///
/// **A failure leaves nothing of this call behind, and nothing anybody else can
/// be shown to own touched.** Whoever created the branch is the only one who
/// may delete it, and the two failures are told apart rather than guessed at:
/// git saying the branch is already there means the branch is not ours, so the
/// cleanup keeps its hands off it and the caller gets a `Conflict` — the same
/// answer as a branch that was already there when the call started.
///
/// Every *other* failure unwinds with [`RemoveBranch::Always`], and that rests
/// on an inference rather than on a fact: the pre-check above saw no branch of
/// this name, so this call is the only candidate for the one that is there now.
/// The inference holds for every wording of "already there" git has, because
/// those take the `Never` path — but a future git that refuses a create for
/// that reason in words none of the three match would send the unwind down this
/// branch instead, and the branch deletion would land on somebody else's work. The
/// repository lock `workspace.create` holds is what keeps a second create of
/// the same name out of the window in practice; [`branch_is_already_there`] is
/// the part that has to stay current with git.
pub async fn create(layout: &Layout, base_branch: &str) -> Result<(), CreateFailed> {
    let git = &layout.daemon_git();
    // Everything down to the first `create_dir_all` is a question, not a write.
    // A refusal from any of it leaves the caller nothing to unwind, and saying
    // so is what keeps its cleanup — `git worktree prune` on the user's own
    // repository — away from a call that only ever asked.
    if repo::branch_exists(git, &layout.repo, &layout.branch)
        .await
        .map_err(CreateFailed::nothing)?
    {
        // Nothing of ours exists yet, so nothing is cleaned up here: the
        // directories this `Layout` names are shared with whichever workspace
        // that branch belongs to.
        return Err(CreateFailed::nothing(branch_conflict(&layout.branch)));
    }
    if !repo::branch_exists(git, &layout.repo, base_branch)
        .await
        .map_err(CreateFailed::nothing)?
    {
        return Err(CreateFailed::nothing(RpcError::invalid_params(format!(
            "base branch {base_branch} does not exist"
        ))));
    }
    // From here on this call is making things, and a failure has to say so.
    for d in [
        layout.ref_dir(),
        layout.reflog_dir(),
        layout.objects_dir.clone(),
    ] {
        std::fs::create_dir_all(&d).map_err(|e| CreateFailed::something(RpcError::io(&e)))?;
    }
    if let Some(parent) = layout.worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CreateFailed::something(RpcError::io(&e)))?;
    }
    if let Err(e) = git
        .run(
            &layout.repo,
            &[
                "worktree",
                "add",
                "-b",
                &layout.branch,
                &path_arg(&layout.worktree_path),
                base_branch,
            ],
        )
        .await
    {
        // Somebody put the branch there between the check above and now — the
        // same name created twice at once. Their branch and their checkout, so
        // only this call's own directories go.
        if branch_is_already_there(&e) {
            let unwound = remove_with(layout, RemoveBranch::Never).await;
            return Err(CreateFailed::after(
                unwound,
                branch_conflict(&layout.branch),
            ));
        }
        let unwound = remove_with(layout, RemoveBranch::Always).await;
        return Err(CreateFailed::after(unwound, e));
    }
    if let Err(e) = lock(layout).await {
        let unwound = remove_with(layout, RemoveBranch::Always).await;
        return Err(CreateFailed::after(unwound, e));
    }
    if !layout.ref_dir().join("work").is_file() {
        let unwound = remove_with(layout, RemoveBranch::Always).await;
        return Err(CreateFailed::after(
            unwound,
            RpcError::internal(format!(
                "expected loose ref at {}",
                layout.ref_dir().join("work").display()
            )),
        ));
    }
    Ok(())
}

/// The reason every workspace worktree is locked with, as `git worktree list`
/// shows it to whoever wonders why.
pub const LOCK_REASON: &str =
    "BondSymphonic manages this worktree from WSL; do not prune or remove it by hand";

/// Locks the workspace's worktree registration.
///
/// **This is what keeps a Windows git from deleting the workspace.** The
/// worktree lives under the WSL user's home, a path a git running on Windows
/// cannot see, so to that git every workspace registration looks stale — and a
/// `git worktree prune` there, which Windows git tools run on their own, deletes
/// `<git_common>/worktrees/<id>`. The directory and the branch survive; the
/// registration does not, and without it the worktree is not a repository any
/// more. Git skips locked registrations in `prune`, on every platform, and
/// [`remove_with`] unlocks before it removes, so the lock costs the daemon
/// nothing.
///
/// The short-lived scratch worktrees `git/merge.rs` checks out are not locked:
/// they exist for the length of one call.
async fn lock(layout: &Layout) -> Result<(), RpcError> {
    layout
        .daemon_git()
        .run(
            &layout.repo,
            &[
                "worktree",
                "lock",
                "--reason",
                LOCK_REASON,
                &path_arg(&layout.worktree_path),
            ],
        )
        .await
        .map(|_| ())
}

/// What [`ensure_registered`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// The repository still lists the worktree. It is locked now, if it was
    /// not already.
    Intact,
    /// The repository had forgotten the worktree and the registration was
    /// rebuilt. Worth telling the user: something outside BondSymphonic pruned it.
    Repaired,
}

/// Makes sure the repository still lists the workspace's worktree, and puts the
/// registration back when it safely can.
///
/// A registration a Windows git pruned (see [`lock`]) leaves a worktree whose
/// `.git` file points at nothing, so every git command in it fails and the
/// sandbox cannot even be started. The directory and the branch are usually
/// untouched, and then the registration is only three small files and an
/// index, which is what this writes back — laid out exactly as
/// `git worktree add` lays them out (git 2.43, checked against a real add):
/// `HEAD` as a symbolic ref to the branch, `commondir` as `../..`, `gitdir` as
/// the absolute path of the worktree's `.git`, each ending in a newline.
/// `git worktree repair` cannot do this: it mends links between a registration
/// and a directory that both exist, and refuses outright when the registration
/// is gone.
///
/// Only when every one of these holds, because each one that does not means
/// the worktree is not simply "ours, with the registration missing":
///
/// * the worktree directory exists;
/// * its `.git` is a gitfile naming exactly [`Layout::worktree_gitdir`];
/// * the branch exists, and no other worktree has it checked out — git allows
///   one checkout per branch, and a second one would let two worktrees move
///   the same ref under each other.
///
/// The index is rebuilt from the branch with `git read-tree`, which writes the
/// index and nothing else: the agent's uncommitted edits stay in the directory
/// and show up as unstaged changes, the way they would after a
/// `git reset --mixed`. Whatever was staged is unstaged, which is the one thing
/// the prune took that cannot be had back. The branch's newest commits may live
/// only in the workspace's private object directory, which
/// [`Layout::worktree_git`] adds as an alternate.
///
/// An intact registration that is not locked — every workspace made before
/// [`create`] locked them — is locked here, so the next daemon start protects
/// what is already on disk. A lock that fails is logged and nothing more: the
/// workspace works, it is only exposed as it always was.
///
/// Under the repository lock `workspace.create` and `workspace.destroy` hold,
/// so a destroy of the same workspace cannot remove the directory while the
/// registration is being written back into it.
pub async fn ensure_registered(layout: &Layout) -> Result<Registration, RpcError> {
    let repo_lock = super::repo_lock(&layout.repo);
    let _repo_guard = repo_lock.lock().await;
    let gitdir = layout.worktree_gitdir();
    let has = |name: &str| gitdir.join(name).is_file();
    let written = has("HEAD") && has("commondir") && has("gitdir");
    if written {
        // A registration with no index is one whose rebuild was cut short — the
        // daemon killed between writing the files and `read-tree`. Git takes a
        // missing index as every tracked file deleted, so the first plain
        // `git commit` in the worktree would commit an empty tree onto the
        // branch. `HEAD` is kept as it is: those three files are all git needs
        // to find it.
        let repaired = if has("index") {
            Registration::Intact
        } else {
            // Left by the same interrupted `read-tree`, and it would make the
            // next one refuse. Nothing else can be holding it: this runs before
            // the workspace's sandbox starts.
            let _ = std::fs::remove_file(gitdir.join("index.lock"));
            read_tree(layout).await.map_err(|e| {
                RpcError::new(
                    e.code,
                    format!(
                        "This workspace's worktree has lost its index, and rebuilding it \
                         failed: {}",
                        e.message
                    ),
                )
            })?;
            Registration::Repaired
        };
        if !gitdir.join("locked").exists() {
            if let Err(e) = lock(layout).await {
                tracing::warn!(
                    worktree = %layout.worktree_path.display(),
                    "could not lock the worktree: {}", e.message
                );
            }
        }
        return Ok(repaired);
    }
    if gitdir.exists() {
        // Half written: some of git's files are there and some are not, which is
        // a daemon killed partway through `rebuild` (or git killed partway
        // through `worktree add`). The directory is named after this
        // workspace's id and holds only registration metadata, so it is taken
        // away and the registration treated as missing.
        tracing::warn!(gitdir = %gitdir.display(), "removing a half-written worktree registration");
        std::fs::remove_dir_all(&gitdir).map_err(|e| RpcError::io(&e))?;
    }
    if let Err(why) = repairable(layout).await {
        return Err(RpcError::new(
            ErrorCode::GitError,
            format!(
                "The repository {} no longer lists this workspace's worktree, and it could \
                 not be re-registered because {}. {}",
                layout.repo.display(),
                why.reason,
                why.advice
            ),
        )
        .with_data(serde_json::json!({ "reason": "worktree_unregistered" })));
    }
    if let Err(e) = rebuild(layout).await {
        // Nothing half-built stays behind: a registration without an index or a
        // lock is worse than none, because the next start would take it as
        // intact.
        let _ = std::fs::remove_dir_all(&gitdir);
        return Err(RpcError::new(
            e.code,
            format!(
                "The repository {} no longer lists this workspace's worktree, and \
                 re-registering it failed: {}",
                layout.repo.display(),
                e.message
            ),
        ));
    }
    tracing::warn!(
        repo = %layout.repo.display(),
        worktree = %layout.worktree_path.display(),
        "re-registered a worktree the repository had forgotten"
    );
    Ok(Registration::Repaired)
}

/// Why a missing registration is not put back, in words for the user.
struct NotRepairable {
    /// Finishes "it could not be re-registered because …".
    reason: String,
    /// What the user can do about it, as a sentence of its own.
    advice: String,
}

/// Checks the conditions [`ensure_registered`] lists, without writing anything.
async fn repairable(layout: &Layout) -> Result<(), NotRepairable> {
    let wt = &layout.worktree_path;
    let remove_it = || {
        "Remove the workspace to clean up; if it has work you want to keep, copy it out first."
            .to_string()
    };
    if !wt.is_dir() {
        return Err(NotRepairable {
            reason: format!("its directory {} is missing", wt.display()),
            advice: "Remove the workspace to clean up.".into(),
        });
    }
    let gitfile = wt.join(".git");
    let expected = layout.worktree_gitdir();
    let points_at = std::fs::read_to_string(&gitfile)
        .ok()
        .and_then(|s| s.strip_prefix("gitdir:").map(|p| p.trim().to_string()));
    let Some(points_at) = points_at else {
        return Err(NotRepairable {
            reason: format!(
                "{} is not the link to the repository git left there",
                gitfile.display()
            ),
            advice: remove_it(),
        });
    };
    // A relative link, which git writes under `worktree.useRelativePaths`, is
    // relative to the worktree.
    let target = wt.join(&points_at);
    if repo::canonical_ish(&target) != repo::canonical_ish(&expected) {
        return Err(NotRepairable {
            reason: format!(
                "{} points at {points_at} rather than at {}",
                gitfile.display(),
                expected.display()
            ),
            advice: remove_it(),
        });
    }
    let git = layout.daemon_git();
    let git_failed = |e: RpcError| NotRepairable {
        reason: format!("git could not inspect the repository: {}", e.message),
        advice:
            "Check that the repository is still there and readable, then restart the workspace."
                .into(),
    };
    if !repo::branch_exists(&git, &layout.repo, &layout.branch)
        .await
        .map_err(git_failed)?
    {
        return Err(NotRepairable {
            reason: format!("its branch {} no longer exists", layout.branch),
            advice: remove_it(),
        });
    }
    let listed = git
        .run(&layout.repo, &["worktree", "list", "--porcelain"])
        .await
        .map_err(git_failed)?
        .stdout;
    if let Some(other) = checked_out_at(&listed, &layout.branch) {
        return Err(NotRepairable {
            reason: format!(
                "its branch {} is checked out in another worktree, {other}",
                layout.branch
            ),
            advice: format!("Switch {other} to a different branch, then restart the workspace."),
        });
    }
    Ok(())
}

/// The worktree `git worktree list --porcelain` shows with `branch` checked out.
fn checked_out_at(listed: &str, branch: &str) -> Option<String> {
    let wanted = format!("branch refs/heads/{branch}");
    let mut current: Option<&str> = None;
    for line in listed.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path);
        } else if line == wanted {
            return current.map(str::to_string);
        }
    }
    None
}

/// Writes the registration back and rebuilds its index. See [`ensure_registered`].
async fn rebuild(layout: &Layout) -> Result<(), RpcError> {
    let gitdir = layout.worktree_gitdir();
    std::fs::create_dir_all(&gitdir).map_err(|e| RpcError::io(&e))?;
    // Git finds a registration by comparing this path with the one it is given,
    // and git for Windows writes it, and compares it, with forward slashes
    // (checked against 2.52). A backslash here makes `worktree lock` answer
    // "is not a working tree". On Unix git writes the realpath, symlinks
    // resolved; Windows is left as given, because `canonicalize` there answers
    // with a `\\?\` path git does not write.
    let mut worktree = layout.worktree_path.clone();
    if cfg!(unix) {
        if let Ok(real) = std::fs::canonicalize(&worktree) {
            worktree = real;
        }
    }
    let mut back_link = path_arg(&worktree.join(".git"));
    if cfg!(windows) {
        back_link = back_link.replace('\\', "/");
    }
    for (name, contents) in [
        ("HEAD", format!("ref: refs/heads/{}\n", layout.branch)),
        ("commondir", "../..\n".to_string()),
        ("gitdir", format!("{back_link}\n")),
    ] {
        std::fs::write(gitdir.join(name), contents).map_err(|e| RpcError::io(&e))?;
    }
    lock(layout).await?;
    read_tree(layout).await
}

/// Rebuilds the worktree's index from its `HEAD`, leaving the files alone.
async fn read_tree(layout: &Layout) -> Result<(), RpcError> {
    layout
        .worktree_git()
        .run(&layout.worktree_path, &["read-tree", "HEAD"])
        .await
        .map(|_| ())
}

/// What a hold's `locked` file says, for anyone who finds one.
const HOLD_REASON: &str = "BondSymphonic keeps this directory while it removes a worktree\n";

/// The name every hold entry starts with.
const HOLD_PREFIX: &str = ".bs-hold-";

/// What the persistent hold of an in-place workspace says.
const IN_PLACE_HOLD_REASON: &str =
    "BondSymphonic keeps this directory while an agent works in this checkout\n";

/// The name of the persistent hold of the in-place workspace `id`.
///
/// Deliberately not [`HOLD_PREFIX`]: `WorktreesHold::take` sweeps away every
/// entry with that prefix as the leftover of a daemon that stopped mid-removal,
/// and this one is meant to outlive every worktree removal there is. It carries
/// the workspace id so that two in-place workspaces of two checkouts of the same
/// repository -- which cannot happen today, but costs nothing to allow -- each
/// take back their own.
fn in_place_hold(git_common: &Path, id: &WorkspaceId) -> PathBuf {
    git_common.join("worktrees").join(format!(".bs-inplace-{id}"))
}

/// Keeps `<git common dir>/worktrees` in place for as long as an in-place
/// workspace exists, rather than only across one removal.
///
/// The directory is bound read-only into that workspace's sandbox, and a bind
/// does not survive the directory it was made on. Git deletes `worktrees` when
/// it finds it empty -- the last `git worktree remove`, but also a plain `git
/// worktree prune` or a `git gc`, which git's own `--auto` maintenance runs
/// behind the user's ordinary `git commit`. Without this, an agent would be
/// stopped at random by the user's git doing nothing of any consequence.
///
/// Made by [`crate::workspace::in_place::InPlaceLayout::prepare`], after that
/// call has made sure `worktrees` exists, and taken back by its `release`. Best
/// effort: a hold that cannot be made costs a restart, not the workspace.
pub fn hold_worktrees_for_in_place(git_common: &Path, id: &WorkspaceId) {
    let entry = in_place_hold(git_common, id);
    if entry.join("locked").is_file() {
        return;
    }
    let made = std::fs::create_dir_all(&entry)
        .and_then(|()| std::fs::write(entry.join("locked"), IN_PLACE_HOLD_REASON));
    if let Err(e) = made {
        tracing::warn!(path = %entry.display(), error = %e, "cannot hold the worktrees directory for an in-place workspace");
    }
}

/// Takes the persistent hold away again, at Close.
pub fn release_in_place_hold(git_common: &Path, id: &WorkspaceId) {
    release_hold(&in_place_hold(git_common, id), IN_PLACE_HOLD_REASON);
}

/// Keeps `<git common dir>/worktrees` in place while the daemon removes or
/// prunes worktrees, for as long as the value lives.
///
/// Git deletes that directory when the last linked worktree goes. An in-place
/// workspace of the same repository has it bound read-only, and the bind does
/// not survive the directory: the sandbox would be stopped over the daemon's
/// own cleanup. So a registration-shaped entry holding only a `locked` file
/// sits in it meanwhile. Git skips a locked entry when it prunes (it asks
/// before looking for a `gitdir`), `worktree list` skips an entry with no
/// `gitdir`, and the directory is then never empty when git tries to remove
/// it. Measured with git 2.43: `list`, `remove`, `prune`, `repair`, `fsck`
/// and `gc` all leave such an entry and the directory alone.
///
/// Only where the directory already exists: without one there is nothing a
/// bind could be holding. Callers hold the repository lock, so any other hold
/// found here is one a stopped daemon left behind and is taken away first.
/// Best effort throughout: a hold that cannot be made costs an in-place
/// sandbox a restart, not the removal.
pub struct WorktreesHold {
    entry: Option<PathBuf>,
}

impl WorktreesHold {
    pub fn take(git_common: &Path) -> Self {
        let worktrees = git_common.join("worktrees");
        if !std::fs::symlink_metadata(&worktrees).is_ok_and(|m| m.is_dir()) {
            return Self { entry: None };
        }
        if let Ok(read) = std::fs::read_dir(&worktrees) {
            for stale in read.flatten() {
                if stale.file_name().to_string_lossy().starts_with(HOLD_PREFIX) {
                    release_hold(&stale.path(), HOLD_REASON);
                }
            }
        }
        let entry = worktrees.join(format!("{HOLD_PREFIX}{}", crate::ids::new_id("")));
        let made = std::fs::create_dir(&entry)
            .and_then(|()| std::fs::write(entry.join("locked"), HOLD_REASON));
        match made {
            Ok(()) => Self { entry: Some(entry) },
            Err(e) => {
                tracing::warn!(path = %entry.display(), error = %e, "cannot hold the worktrees directory");
                release_hold(&entry, HOLD_REASON);
                Self { entry: None }
            }
        }
    }
}

impl Drop for WorktreesHold {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            release_hold(&entry, HOLD_REASON);
        }
    }
}

/// Takes a hold entry away: its `locked` file and then the entry, and nothing
/// else.
///
/// Both steps are read before they are taken. A registration has a `gitdir`,
/// and a user is free to name a worktree whatever they like -- including
/// something that begins the way a hold does -- so an entry with one is theirs
/// and is left alone, lock and all. The `locked` file has to say exactly what
/// the daemon wrote there, because a `locked` a user wrote is a worktree they
/// asked git to protect. The directory then goes only if it is empty, which is
/// what leaves anything else in it standing.
fn release_hold(entry: &Path, reason: &str) {
    if entry.join("gitdir").exists() {
        return;
    }
    let locked = entry.join("locked");
    match std::fs::read(&locked) {
        Ok(bytes) if bytes == reason.as_bytes() => {
            let _ = std::fs::remove_file(&locked);
        }
        // A hold whose `locked` was never written: the directory is still the
        // daemon's to take back.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        _ => return,
    }
    let _ = std::fs::remove_dir(entry);
}

/// Removes the worktree, its registration and its branch.
///
/// [`remove_with`] with [`RemoveBranch::Always`]: what `workspace.destroy`
/// wants, where the workspace owns everything the layout names.
pub async fn remove(layout: &Layout) -> Result<(), RpcError> {
    remove_with(layout, RemoveBranch::Always).await
}

/// Removes the worktree and its registration, and the branch if `branch` says
/// so.
///
/// Through [`Layout::daemon_git`] for the same reason as [`create`]: the
/// branch deletion at the end fires `reference-transaction`, and a destroy is the
/// last moment at which running the repository's code would be welcome.
///
/// With [`RemoveBranch::Never`] the branch stays, and so do
/// `refs/heads/bs/<name>/` and its reflog directory — those are named after the
/// workspace *name*, not its id, so they are shared with any other workspace of
/// the same name and are not this call's to delete either. What always goes is
/// the worktree directory and its registration, which belong to one workspace.
pub async fn remove_with(layout: &Layout, branch: RemoveBranch) -> Result<(), RpcError> {
    let git = &layout.daemon_git();
    let _hold = WorktreesHold::take(&layout.git_common);
    // `worktree unlock` first, and its failure is not news: it fails for a
    // worktree that was never locked, which is almost all of them. When one
    // *is* locked — a checkout the user parked on a removable disk, say — git
    // refuses `worktree remove --force` outright *and* skips the registration
    // during `prune`, so without this the workspace could not be destroyed at
    // all.
    let _ = git
        .run(
            &layout.repo,
            &["worktree", "unlock", &path_arg(&layout.worktree_path)],
        )
        .await;
    // Git's own removal, for the registration and the directory in one step,
    // and equally not worth reading the outcome of: a worktree that is not
    // registered, a directory that is already gone and a gitfile git will not
    // parse all end here, and all three are states the two steps below reach
    // anyway. Reading git's *prose* to decide which failures were survivable is
    // what this used to do, and it hard-failed on every wording nobody had
    // thought of — the locked one above being exactly that.
    let _ = git
        .run(
            &layout.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &path_arg(&layout.worktree_path),
            ],
        )
        .await;
    // Whatever git left. Not being there is the ordinary case — git usually did
    // the job — but anything else is a directory still standing where the
    // daemon has just told a client the workspace is gone, so it is reported
    // rather than swallowed. Windows gets a while to let go first: `destroy`
    // kills the sandbox and its agents a few lines before this runs, and their
    // handles outlive them by a moment.
    let directory = crate::util::fs_retry::remove_dir_all(&layout.worktree_path)
        .await
        .map_err(|e| {
            RpcError::new(
                ErrorCode::IoError,
                format!(
                    "cannot remove the worktree directory {}: {e}",
                    layout.worktree_path.display()
                ),
            )
        });
    // The rest runs whatever the directory did, and the first failure of the
    // three is reported at the end.
    //
    // Returning early on the directory would make a transient lock cost far
    // more than the directory: the registration would outlive it, and a
    // registration without a directory keeps the branch checked out and makes
    // every later `worktree add` refuse — a state a human has to repair by
    // hand. The prune and the branch deletion do not depend on the directory
    // being gone, so there is no reason to make them hostage to it.
    let pruned = git
        .run(&layout.repo, &["worktree", "prune"])
        .await
        .map(|_| ());
    let branched = match branch {
        RemoveBranch::Never => Ok(()),
        RemoveBranch::Always => remove_branch(git, layout).await,
    };
    report(
        layout,
        [
            ("removing the worktree directory", directory),
            ("pruning the worktree registration", pruned),
            ("deleting the branch", branched),
        ],
    )
}

/// One answer for three independent steps: the first failure, carrying what the
/// others said.
///
/// Each of the three can fail for its own reason now that none of them is
/// skipped, and a caller gets one `RpcError`. Returning the first and dropping
/// the rest would be the wrong half to keep: `workspace.destroy` turns this into
/// a `WorkspaceState::Error` and then stops, so whatever is not in this message
/// is a thing nobody can go back and ask about. The first error is kept whole —
/// its code and its `data` are what clients branch on — and the others are
/// appended to its message.
///
/// Logged as well as returned, because the log is where the daemon's own
/// operator looks and it keeps each error unflattened.
fn report(
    layout: &Layout,
    steps: [(&'static str, Result<(), RpcError>); 3],
) -> Result<(), RpcError> {
    let mut failures: Vec<(&'static str, RpcError)> = Vec::new();
    for (what, outcome) in steps {
        if let Err(e) = outcome {
            tracing::warn!(
                repo = %layout.repo.display(),
                worktree = %layout.worktree_path.display(),
                "{what} failed: {}", e.message
            );
            failures.push((what, e));
        }
    }
    let mut failures = failures.into_iter();
    let Some((_, mut first)) = failures.next() else {
        return Ok(());
    };
    let rest: Vec<String> = failures
        .map(|(what, e)| format!("{what} failed: {}", e.message))
        .collect();
    if !rest.is_empty() {
        first.message = format!("{} (also: {})", first.message, rest.join("; "));
    }
    Err(first)
}

/// `text` as a POSIX extended regular expression that matches only itself,
/// which is what `git config --get-regexp` takes.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if r"\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Deletes the workspace's branch, and the two directories named after the
/// workspace name that go with it.
///
/// `update-ref -d` rather than `branch -D`: `branch -D` rewrites
/// `.git/config` every time, to drop a `branch.<name>` section that is usually
/// not there, and a replaced config stops any in-place workspace of the same
/// repository (see [`crate::workspace::in_place::ProtectedSnapshot`]). The
/// section is removed separately, and only when there is one -- a workspace
/// whose pull request set an upstream.
///
/// What `branch -D` also did, and `update-ref -d` does not, is refuse a branch
/// that is checked out somewhere. By here the workspace's own worktree is gone
/// and pruned, so the only way to find the branch checked out is that somebody
/// else took it -- an in-place agent that ran `git switch bs/<name>/work` in
/// the user's own checkout, say. Deleting it under them would leave that
/// checkout on a branch that does not exist, so it is refused as it used to be.
async fn remove_branch(git: &Git, layout: &Layout) -> Result<(), RpcError> {
    if repo::branch_exists(git, &layout.repo, &layout.branch).await? {
        let listed = git
            .run(&layout.repo, &["worktree", "list", "--porcelain"])
            .await?;
        if let Some(other) = checked_out_at(&listed.stdout, &layout.branch) {
            return Err(RpcError::new(
                ErrorCode::Conflict,
                format!(
                    "branch {} is checked out at {other}, so it was left alone",
                    layout.branch
                ),
            ));
        }
        let full = format!("refs/heads/{}", layout.branch);
        git.run(&layout.repo, &["update-ref", "-d", &full]).await?;
        let section = format!("branch.{}", layout.branch);
        let has_section = git
            .run(
                &layout.repo,
                &[
                    "config",
                    "--local",
                    "--name-only",
                    "--get-regexp",
                    &format!("^{}\\.", regex_escape(&section)),
                ],
            )
            .await
            .is_ok_and(|o| !o.stdout.trim().is_empty());
        if has_section {
            git.run(
                &layout.repo,
                &["config", "--local", "--remove-section", &section],
            )
            .await?;
        }
    }
    let _ = std::fs::remove_dir(layout.ref_dir());
    let _ = std::fs::remove_dir_all(layout.reflog_dir());
    Ok(())
}
