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

use std::path::{Path, PathBuf};

/// The files copied out of the daemon user's home, as `(source, destination)`
/// relative to the two homes, with whether the file is a secret.
///
/// `.credentials.json` holds the OAuth tokens and `.claude.json` the account
/// and per-project state, so both go in mode 0600. `settings.json` is
/// configuration and keeps the default mode.
const FILES: [(&str, bool); 3] = [
    (".claude/settings.json", false),
    (".claude/.credentials.json", true),
    (".claude.json", true),
];

/// The daemon user's home, or `None` when the platform will not name one.
fn daemon_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path.display(), error = %e, "could not restrict credential file");
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {
    // Windows inherits the parent directory's ACL, and the workspace home lives
    // under the daemon's own data directory; there is no mode bit to set.
}

/// Copies the daemon user's Claude Code login into `home`, overwriting what is
/// already there so a fresh login refreshes an existing workspace.
///
/// Returns the relative names actually copied, for logging. Every failure is
/// non-fatal: a missing file simply means the user has not logged in (or has no
/// settings), and the agent will say so itself when it starts.
pub fn seed_claude_files(home: &Path) -> Vec<&'static str> {
    let Some(source_home) = daemon_home() else {
        return Vec::new();
    };
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
        // Removed first: `copy` onto an existing file keeps that file's mode,
        // and a 0600 file the user has since chmod'd would silently stay wrong.
        let _ = std::fs::remove_file(&to);
        match std::fs::copy(&from, &to) {
            Ok(_) => {
                if secret {
                    restrict(&to);
                }
                seeded.push(rel);
            }
            Err(e) => {
                tracing::warn!(from = %from.display(), error = %e, "could not seed claude file")
            }
        }
    }
    seeded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The copy itself, driven through a fake source home so the test does not
    /// depend on whether this machine has a Claude login.
    fn seed_from(source_home: &Path, home: &Path) -> Vec<&'static str> {
        let mut seeded = Vec::new();
        for (rel, secret) in FILES {
            let from = source_home.join(rel);
            if !from.is_file() {
                continue;
            }
            let to = home.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(&from, &to).unwrap();
            if secret {
                restrict(&to);
            }
            seeded.push(rel);
        }
        seeded
    }

    #[test]
    fn only_the_files_that_exist_are_seeded_and_secrets_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        std::fs::create_dir_all(src.join(".claude")).unwrap();
        std::fs::write(src.join(".claude/.credentials.json"), "{\"t\":1}").unwrap();
        std::fs::write(src.join(".claude.json"), "{}").unwrap();
        // No settings.json: it must simply be skipped.

        let seeded = seed_from(&src, &dst);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "{\"t\":1}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for rel in [".claude/.credentials.json", ".claude.json"] {
                let mode = std::fs::metadata(dst.join(rel))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{rel} must be private");
            }
        }
    }

    /// Whatever this machine's daemon home happens to hold, seeding creates
    /// exactly the files it reports and no others, and never fails.
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
