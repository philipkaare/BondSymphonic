//! Copying the daemon user's Claude Code login into a workspace home.
//!
//! An agent runs inside a sandbox whose `HOME` is a private per-workspace
//! directory, so it cannot see the daemon user's `~/.claude`. Without help it
//! would start logged out and exit immediately. Rather than share the real home
//! into the sandbox — which would give every agent write access to the user's
//! whole Claude configuration, and to whatever else lives there — the few files
//! that carry the login are copied in.
//!
//! Copying happens both when a workspace is created and every time an agent
//! starts, so a login performed after the workspace existed reaches it without
//! the user having to recreate anything.
//!
//! `.claude.json` is the one file that is written rather than copied: the
//! workspace's worktree is added to it as a trusted project (§8.3), because
//! Claude Code keeps that consent per project directory and a sandbox home has
//! never seen this one.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The files copied out of the daemon user's home, relative to both homes, with
/// whether the file is a secret.
///
/// `.credentials.json` holds the OAuth tokens and `.claude.json` the account
/// and per-project state, so both are created mode 0600 and never exist wider
/// than that. `settings.json` is configuration and keeps the default mode.
const FILES: [(&str, bool); 3] = [
    (".claude/settings.json", false),
    (".claude/.credentials.json", true),
    (CLAUDE_JSON, true),
];

/// The file Claude Code keeps its per-project state in, including which project
/// directories the user has accepted the trust dialog for.
const CLAUDE_JSON: &str = ".claude.json";

/// The daemon user's home, or `None` when the platform will not name one.
fn daemon_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Removes whatever is at `path` without ever following it.
///
/// A symlink is unlinked — the link itself goes, its target is untouched — and a
/// real file or directory is deleted. `Ok(())` when there was nothing there.
pub(crate) fn remove_any(path: &Path) -> std::io::Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if md.is_symlink() {
        // Windows makes a symlink to a directory a directory entry, and
        // `remove_file` refuses it; `remove_dir` unlinks the link and leaves the
        // target alone. Neither call follows the link on any platform.
        return std::fs::remove_file(path).or_else(|e| {
            if cfg!(windows) {
                std::fs::remove_dir(path)
            } else {
                Err(e)
            }
        });
    }
    if md.is_dir() {
        return std::fs::remove_dir_all(path);
    }
    std::fs::remove_file(path)
}

/// Makes `path` a real directory, replacing anything else that is sitting there.
///
/// The workspace home is bind-mounted into the sandbox **read-write** as `$HOME`
/// (`sandbox/linux_bwrap.rs`), so the agent owns every name under it: it can
/// `rm -rf ~/.claude` and leave a symlink to any host path in its place. A
/// dangling link is enough, because only the link *text* matters when the daemon
/// resolves it later on the host. `create_dir_all` follows such a link, and then
/// everything written "into the workspace home" lands outside it — the daemon
/// user's own `~/.claude/settings.json` is one `ln -s` away, and Claude Code
/// settings are hook commands, so that is code execution as the user.
///
/// `symlink_metadata` is the check that does not follow. Anything that is not a
/// real directory is unlinked and a real directory is created in its place; a
/// symlink found here is logged, because an agent that planted one was trying
/// something.
pub(crate) fn ensure_real_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => return Ok(()),
        Ok(md) => {
            tracing::warn!(
                path = %path.display(),
                symlink = md.is_symlink(),
                "replacing something other than a real directory in the workspace home"
            );
            remove_any(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(path)
}

/// Unlinks whatever is at `path` unless it is a real directory.
///
/// The directory is the one obstruction deliberately left in place: it cannot
/// redirect a write the way a symlink can, and removing it recursively would be
/// the daemon deleting data it did not put there. The `create_new` that follows
/// fails on it instead, which every caller reports.
pub(crate) fn clear_destination(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Ok(());
    }
    remove_any(path)
}

/// Writes `body` into `path` the way everything under `homes/<id>` is written:
/// the destination is unlinked first, never followed, and then created with
/// `create_new` so a symlink raced back in is refused rather than written
/// through. Daemon design §8.3.
pub(crate) fn write_guarded(path: &Path, body: &str) -> std::io::Result<()> {
    clear_destination(path)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(body.as_bytes())
}

/// Creates `path` for writing, refusing to reuse anything already there, and
/// on Unix with the private mode applied by `open` itself.
///
/// The mode matters at creation rather than afterwards: `fs::copy` would make
/// the file with the process umask, typically world-readable, and write the
/// OAuth tokens into it before any `chmod` could narrow it. The umask can only
/// clear bits, and 0600 has none to spare for group or other, so the file is
/// never wider than intended.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Copies one file, giving a secret a destination that is private from the
/// moment it exists.
///
/// Both kinds go through `create_new`, never `fs::copy`: `fs::copy` opens the
/// destination by path and follows a symlink found there, so a link raced back
/// in after [`remove_any`] cleared it would be written through. `create_new`
/// refuses instead.
fn copy_file(from: &Path, to: &Path, secret: bool) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = if secret {
        create_private(to)?
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?
    };
    std::io::copy(&mut src, &mut dst)?;
    dst.flush()
}

/// Writes `body` into `path` with the private mode a secret gets, through the
/// same unlink-then-`create_new` the copies use.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    clear_destination(path)?;
    let mut f = create_private(path)?;
    f.write_all(body.as_bytes())
}

/// `source` with `worktree` marked as a trusted project, as the bytes to write.
///
/// Claude Code stores the answer to its trust dialog per project directory in
/// `~/.claude.json`, and reads a repository's `.claude/settings.json` — the
/// `permissions.allow` list a repo pins its agent's tools with — only for a
/// project it has been told to trust. A sandbox home is new every workspace and
/// has never seen this worktree, so without this entry the agent starts with the
/// repository's permissions dropped and no way to accept the dialog: it runs
/// non-interactively, and the daemon is what created the directory in the first
/// place.
///
/// Everything else in the file is the user's own and survives the merge. A
/// source that will not parse — or is not a JSON object — is replaced rather
/// than propagated: the CLI could not have read it either, and the alternative
/// is a workspace that stays untrusted for good.
pub(crate) fn claude_json_with_trust(source: Option<&str>, worktree: &Path) -> String {
    let parsed = source.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let mut root = match parsed {
        Some(serde_json::Value::Object(map)) => map,
        _ => {
            if source.is_some_and(|s| !s.trim().is_empty()) {
                tracing::warn!(
                    "the daemon user's .claude.json is not a JSON object; seeding a fresh one"
                );
            }
            serde_json::Map::new()
        }
    };
    let projects = match root.get_mut("projects") {
        Some(serde_json::Value::Object(p)) => p,
        _ => {
            root.insert("projects".into(), serde_json::json!({}));
            root.get_mut("projects")
                .and_then(|v| v.as_object_mut())
                .expect("just inserted an object")
        }
    };
    // The worktree path *as the agent sees it*: the sandbox binds it at the same
    // path it has on the host (`workspace::lifecycle::spec_for`) and starts the
    // CLI with it as the working directory, so one string serves both sides.
    let key = worktree.to_string_lossy().into_owned();
    match projects.get_mut(&key) {
        Some(serde_json::Value::Object(entry)) => {
            entry.insert("hasTrustDialogAccepted".into(), true.into());
        }
        _ => {
            projects.insert(key, serde_json::json!({ "hasTrustDialogAccepted": true }));
        }
    }
    serde_json::Value::Object(root).to_string()
}

/// Copies the Claude Code login out of `source_home` into `home`, overwriting
/// what is already there so a fresh login refreshes an existing workspace, and
/// marks `worktree` as a project this home trusts.
///
/// Returns the relative names actually written, for logging. Every failure is
/// non-fatal: a missing file simply means the user has not logged in (or has no
/// settings), and the agent will say so itself when it starts. `.claude.json` is
/// the exception that is always written, because the trust entry has to be there
/// whether or not the daemon user has a file to merge it into.
pub fn seed_claude_files_from(
    source_home: &Path,
    home: &Path,
    worktree: &Path,
) -> Vec<&'static str> {
    let mut seeded = Vec::new();
    // The home itself before anything under it: `.claude.json` is written
    // directly into it, and it is as replaceable by the agent as `.claude` is.
    if let Err(e) = ensure_real_dir(home) {
        tracing::warn!(path = %home.display(), error = %e, "could not create the workspace home");
        return seeded;
    }
    for (rel, secret) in FILES {
        let from = source_home.join(rel);
        if rel == CLAUDE_JSON {
            // Written, not copied: whatever the daemon user has (or has not) is
            // merged with this workspace's trust entry. Straight into `home`,
            // which `ensure_real_dir` has just made a real directory.
            let to = home.join(rel);
            let body =
                claude_json_with_trust(std::fs::read_to_string(&from).ok().as_deref(), worktree);
            let _ = clear_destination(&to);
            match write_private(&to, &body) {
                Ok(()) => seeded.push(rel),
                Err(e) => {
                    tracing::warn!(path = %to.display(), error = %e, "could not seed .claude.json")
                }
            }
            continue;
        }
        if !from.is_file() {
            continue;
        }
        let to = home.join(rel);
        if let Some(parent) = to.parent() {
            // `ensure_real_dir`, not `create_dir_all`: the agent can leave a
            // symlink at `<home>/.claude` pointing out of the workspace, and
            // `create_dir_all` would follow it and put the daemon user's own
            // credentials wherever it points.
            if let Err(e) = ensure_real_dir(parent) {
                tracing::warn!(path = %parent.display(), error = %e, "could not create claude dir");
                continue;
            }
        }
        // Cleared first, which is also what makes `create_new` succeed: it keeps
        // a stale mode from surviving a refresh, and unlinks a symlink planted at
        // the destination rather than writing through it.
        let _ = clear_destination(&to);
        match copy_file(&from, &to, secret) {
            Ok(()) => seeded.push(rel),
            Err(e) => {
                tracing::warn!(from = %from.display(), error = %e, "could not seed claude file")
            }
        }
    }
    seeded
}

/// [`seed_claude_files_from`] out of the daemon user's own home.
///
/// A platform that will not name a home still gets the trust entry: the merge
/// has nothing to merge into, which is the same case as a user who has never
/// logged in.
pub fn seed_claude_files(home: &Path, worktree: &Path) -> Vec<&'static str> {
    // Never a path under `home`: that one is the agent's to write, and a source
    // it could create is a source it could dictate.
    let source_home = daemon_home().unwrap_or_else(|| PathBuf::from("/nonexistent/no-daemon-home"));
    seed_claude_files_from(&source_home, home, worktree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write(path: PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A directory symlink at `link`, or `false` where the platform refuses to
    /// make one (Windows without the symlink privilege). Unix, which is where
    /// the sandbox and therefore the attack live, always makes it.
    fn symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    #[test]
    fn only_the_files_that_exist_are_seeded_and_secrets_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "{\"t\":1}");
        write(src.join(".claude.json"), "{}");
        // No settings.json: it must simply be skipped.

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "{\"t\":1}"
        );
        assert!(!dst.join(".claude/settings.json").exists());
        #[cfg(unix)]
        for rel in [".claude/.credentials.json", ".claude.json"] {
            assert_eq!(mode_of(&dst.join(rel)), 0o600, "{rel} must be private");
        }
    }

    /// The point of seeding again at every `agent.start`: a user who logs in
    /// after the workspace was created must get the new tokens, and a
    /// destination someone left world-readable must not stay that way.
    #[test]
    fn seeding_again_refreshes_the_content_and_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "old");
        write(src.join(".claude/settings.json"), "{\"a\":1}");
        seed_claude_files_from(&src, &dst, &worktree);

        // A stale, wide-open destination, as an earlier version of this code
        // (or a user) could have left behind.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dst.join(".claude/.credentials.json"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        write(src.join(".claude/.credentials.json"), "new");

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(
            seeded,
            vec![
                ".claude/settings.json",
                ".claude/.credentials.json",
                ".claude.json"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "new",
            "a later login must overwrite the older one"
        );
        #[cfg(unix)]
        assert_eq!(
            mode_of(&dst.join(".claude/.credentials.json")),
            0o600,
            "a refresh must not leave a stale mode in place"
        );
    }

    /// One file that cannot be written is logged and skipped, and the rest of
    /// the seeding still happens. Seeding is best effort: a workspace that gets
    /// most of a login is more useful than one that gets none.
    ///
    /// The obstruction is a real *directory* at a destination file. That is the
    /// one thing the seeding does not clear: a symlink or a file in the way is
    /// unlinked (see the two tests below), because leaving either there is how a
    /// write escapes the home, but a directory cannot redirect anything and
    /// removing it recursively would be this code deleting data it did not put
    /// there.
    #[test]
    fn a_destination_that_cannot_be_written_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "secret");
        write(src.join(".claude.json"), "{}");
        std::fs::create_dir_all(dst.join(".claude/.credentials.json")).unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude.json"]);
        assert!(std::fs::read_to_string(dst.join(".claude.json"))
            .unwrap()
            .contains("hasTrustDialogAccepted"));
        assert!(
            dst.join(".claude/.credentials.json").is_dir(),
            "the obstruction is reported, not removed"
        );
    }

    /// C1: the agent owns `$HOME` inside the sandbox, so `.claude` can be a
    /// symlink out of the workspace by the time the daemon seeds it again. The
    /// link must be replaced, not followed.
    #[test]
    fn a_symlinked_claude_dir_is_replaced_and_its_target_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        let outside = dir.path().join("the-users-real-home").join(".claude");
        write(
            src.join(".claude/.credentials.json"),
            "the daemon user's tokens",
        );
        write(outside.join("settings.json"), "the user's own settings");

        std::fs::create_dir_all(&dst).unwrap();
        if !symlink_dir(&outside, &dst.join(".claude")) {
            eprintln!("SKIP: this host will not create directory symlinks");
            return;
        }

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert!(
            std::fs::symlink_metadata(dst.join(".claude"))
                .unwrap()
                .is_dir(),
            "the link was replaced by a real directory"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "the daemon user's tokens"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("settings.json")).unwrap(),
            "the user's own settings",
            "nothing outside the workspace home may be touched"
        );
        assert!(
            !outside.join(".credentials.json").exists(),
            "nothing may be written through the link"
        );
    }

    /// The same guard with no symlink privilege needed, so it runs on Windows
    /// too: anything that is not a real directory is replaced.
    #[test]
    fn a_file_where_the_claude_dir_goes_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "tokens");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join(".claude"), "in the way").unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert!(dst.join(".claude").is_dir());
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "tokens"
        );
    }

    /// `write_guarded` is what `seed_home` writes `.gitconfig` through, so the
    /// spec's claim that *every* write into `homes/<id>` de-symlinks its path is
    /// true of that one too.
    #[test]
    fn a_guarded_write_unlinks_a_symlink_rather_than_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("the-users-real.gitconfig");
        std::fs::write(&outside, "the user's own").unwrap();
        let home = dir.path().join("ws-home");
        std::fs::create_dir_all(&home).unwrap();
        let to = home.join(".gitconfig");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &to).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&outside, &to).is_err() {
            eprintln!("SKIP: this host will not create file symlinks");
            return;
        }

        write_guarded(&to, "ours").unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "ours");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the user's own",
            "the link's target must be untouched"
        );
    }

    /// And a real directory in the way is left there rather than deleted.
    #[test]
    fn a_guarded_write_leaves_a_real_directory_alone() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join(".gitconfig");
        std::fs::create_dir_all(to.join("something")).unwrap();
        assert!(write_guarded(&to, "ours").is_err());
        assert!(to.join("something").is_dir(), "nothing was deleted");
    }

    /// Whatever this machine's daemon home happens to hold, the public entry
    /// point creates exactly the files it reports and no others.
    #[test]
    fn seeding_creates_exactly_what_it_reports() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        let seeded = seed_claude_files(&dst, &worktree);
        for rel in &seeded {
            assert!(
                dst.join(rel).is_file(),
                "{rel} was reported but not written"
            );
        }
        for (rel, _) in FILES {
            if !seeded.contains(&rel) {
                assert!(
                    !dst.join(rel).exists(),
                    "{rel} appeared without being reported"
                );
            }
        }
    }

    /// The bug this fixes, in the daemon log of the first real run:
    /// `claude: Ignoring 9 permissions.allow entries from .claude/settings.json:
    /// this workspace has not been trusted`. Claude Code keeps that consent per
    /// project directory in `~/.claude.json`, and the sandbox home is a fresh
    /// one every workspace — so unless the daemon writes the entry, a repository
    /// that pins the tools its agent may use has those settings ignored, and
    /// nobody inside the sandbox can answer the dialog that would fix it.
    ///
    /// Everything else in the file is the user's own and is carried across
    /// untouched: their account, their other projects, their onboarding state.
    #[test]
    fn the_seeded_claude_json_trusts_the_worktree_and_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("worktrees").join("ws_1234");
        write(
            src.join(".claude.json"),
            r#"{
              "userID": "u-1",
              "hasCompletedOnboarding": true,
              "projects": {
                "/home/someone/other": {"hasTrustDialogAccepted": true, "history": ["a"]}
              }
            }"#,
        );

        seed_claude_files_from(&src, &dst, &worktree);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(v["userID"], "u-1");
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["projects"]["/home/someone/other"]["history"][0], "a");
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    /// A daemon user who has never run Claude Code has no `.claude.json` at all,
    /// and the workspace still needs one: the trust entry is the point of the
    /// file here, not a decoration on a copy.
    #[test]
    fn a_home_with_no_claude_json_still_gets_a_trusted_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        std::fs::create_dir_all(&src).unwrap();
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");

        let seeded = seed_claude_files_from(&src, &dst, &worktree);

        assert_eq!(seeded, vec![".claude.json"]);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
        #[cfg(unix)]
        assert_eq!(
            mode_of(&dst.join(".claude.json")),
            0o600,
            "it is written the way the copy of it would be"
        );
    }

    /// A `.claude.json` that will not parse cannot be merged into, and refusing
    /// to seed over it would leave the workspace untrusted for good. The trust
    /// entry wins; what is lost is a file the CLI could not have read either.
    #[test]
    fn an_unparseable_claude_json_still_yields_a_trusted_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude.json"), "half a file {");

        seed_claude_files_from(&src, &dst, &worktree);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    /// The merge is where a mistake would be silent, so its edges are checked
    /// directly: a `projects` key of the wrong shape must not make the whole
    /// file unwritable, and an entry that is already there keeps its own fields.
    #[test]
    fn the_trust_merge_replaces_only_what_it_has_to() {
        let wt = Path::new("/w/t");
        let key = "/w/t";

        let v: serde_json::Value = serde_json::from_str(&claude_json_with_trust(
            Some(r#"{"projects": {"/w/t": {"history": ["x"], "hasTrustDialogAccepted": false}}}"#),
            wt,
        ))
        .unwrap();
        assert_eq!(v["projects"][key]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"][key]["history"][0], "x");

        // `projects` as something other than an object, and a top level that is
        // not an object at all: both are replaced rather than propagated.
        for source in [r#"{"projects": 7}"#, "[1, 2]", "null"] {
            let v: serde_json::Value =
                serde_json::from_str(&claude_json_with_trust(Some(source), wt)).unwrap();
            assert_eq!(
                v["projects"][key]["hasTrustDialogAccepted"], true,
                "source {source}"
            );
        }
    }
}
