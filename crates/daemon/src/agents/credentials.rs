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
    (".claude.json", true),
];

/// The daemon user's home, or `None` when the platform will not name one.
fn daemon_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
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
fn copy_file(from: &Path, to: &Path, secret: bool) -> std::io::Result<()> {
    if !secret {
        std::fs::copy(from, to)?;
        return Ok(());
    }
    let mut src = std::fs::File::open(from)?;
    let mut dst = create_private(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.flush()
}

/// Copies the Claude Code login out of `source_home` into `home`, overwriting
/// what is already there so a fresh login refreshes an existing workspace.
///
/// Returns the relative names actually copied, for logging. Every failure is
/// non-fatal: a missing file simply means the user has not logged in (or has no
/// settings), and the agent will say so itself when it starts.
pub fn seed_claude_files_from(source_home: &Path, home: &Path) -> Vec<&'static str> {
    let mut seeded = Vec::new();
    for (rel, secret) in FILES {
        let from = source_home.join(rel);
        if !from.is_file() {
            continue;
        }
        let to = home.join(rel);
        if let Some(parent) = to.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), error = %e, "could not create claude dir");
                continue;
            }
        }
        // Removed first, which is also what makes `create_new` succeed: it
        // keeps a stale mode from surviving a refresh, and replaces a symlink
        // planted at the destination rather than writing through it.
        let _ = std::fs::remove_file(&to);
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
pub fn seed_claude_files(home: &Path) -> Vec<&'static str> {
    match daemon_home() {
        Some(source_home) => seed_claude_files_from(&source_home, home),
        None => Vec::new(),
    }
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

    #[test]
    fn only_the_files_that_exist_are_seeded_and_secrets_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        write(src.join(".claude/.credentials.json"), "{\"t\":1}");
        write(src.join(".claude.json"), "{}");
        // No settings.json: it must simply be skipped.

        let seeded = seed_claude_files_from(&src, &dst);
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
        write(src.join(".claude/.credentials.json"), "old");
        write(src.join(".claude/settings.json"), "{\"a\":1}");
        seed_claude_files_from(&src, &dst);

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

        let seeded = seed_claude_files_from(&src, &dst);
        assert_eq!(
            seeded,
            vec![".claude/settings.json", ".claude/.credentials.json"]
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
    #[test]
    fn a_destination_that_cannot_be_written_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        write(src.join(".claude/.credentials.json"), "secret");
        write(src.join(".claude.json"), "{}");
        // A regular file where the `.claude` directory has to go, so creating
        // that directory fails for the two files inside it and not for the one
        // beside it.
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join(".claude"), "in the way").unwrap();

        let seeded = seed_claude_files_from(&src, &dst);
        assert_eq!(seeded, vec![".claude.json"]);
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude")).unwrap(),
            "in the way",
            "the obstruction is reported, not overwritten"
        );
    }

    /// Whatever this machine's daemon home happens to hold, the public entry
    /// point creates exactly the files it reports and no others.
    #[test]
    fn seeding_creates_exactly_what_it_reports() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("ws-home");
        let seeded = seed_claude_files(&dst);
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
}
