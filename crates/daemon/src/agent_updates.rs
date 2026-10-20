//! Keeps the agent CLIs in the daemon user's home on their latest release.
//!
//! An agent CLI that falls behind starts rejecting the models a user picks
//! ("model not supported"), and nothing else ever updates one: the setup
//! actions only install what is missing. So every daemon start updates each
//! CLI that is installed -- whichever way the daemon was launched -- and never
//! installs one that is not, which stays the user's choice in Setup. OpenCode
//! is the exception to "latest": it is held at the version its installer pins.
//!
//! Updates run on the host, outside every sandbox, for the reason `setup.rs`
//! gives for its installs: they write to the user's real home and reach the
//! network. Like the setup table, the commands here are literal.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// How long one CLI's update may run before it is killed. Generous: an update
/// is a download, and it runs in the background once restore stops waiting.
pub const UPDATE_LIMIT: Duration = Duration::from_secs(300);

/// How long workspace restore waits for the updates. Each sandbox binds the
/// CLI binaries that exist when it starts, so a restore that went first would
/// keep every restored workspace on the old versions until it restarted.
/// Bounded, because a slow network must not hold the IDE's workspaces hostage.
pub const RESTORE_WAIT: Duration = Duration::from_secs(30);

/// The Codex installer, which installs the latest release and does nothing
/// when that release is already installed -- so it doubles as the updater.
const CODEX_INSTALL_SCRIPT: &str = include_str!("../../../scripts/install-codex.sh");

/// The OpenCode installer, pinned to
/// [`PINNED_OPENCODE_VERSION`](crate::agents::opencode_backend::PINNED_OPENCODE_VERSION).
const OPENCODE_INSTALL_SCRIPT: &str = include_str!("../../../scripts/install-opencode.sh");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub name: &'static str,
    pub argv: Vec<String>,
}

fn installed(candidates: &[PathBuf]) -> Option<&PathBuf> {
    candidates.iter().find(|p| p.is_file())
}

/// The update command for every agent CLI installed under `home`, found at the
/// paths each installer puts it -- never through `PATH`, which under WSL can
/// resolve to a Windows build on `/mnt/c`.
pub fn planned(home: &Path) -> Vec<Update> {
    let local_bin = home.join(".local").join("bin");
    let mut updates = Vec::new();
    if let Some(claude) = installed(&[local_bin.join("claude")]) {
        updates.push(Update {
            name: "claude",
            argv: vec![claude.display().to_string(), "update".into()],
        });
    }
    if installed(&[local_bin.join("codex")]).is_some() {
        updates.push(Update {
            name: "codex",
            argv: vec!["bash".into(), "-c".into(), CODEX_INSTALL_SCRIPT.into()],
        });
    }
    // OpenCode is pinned rather than kept on its latest release: its "update"
    // is the pinned installer, which leaves an install at the pin alone.
    // Only BondSymphonic's own copy in ~/.local/bin; one that OpenCode's
    // installer left in ~/.opencode/bin alone is the user's.
    if installed(&[local_bin.join("opencode")]).is_some() {
        updates.push(Update {
            name: "opencode",
            argv: vec!["bash".into(), "-c".into(), OPENCODE_INSTALL_SCRIPT.into()],
        });
    }
    updates
}

/// The last non-empty line, which is where every one of these CLIs says what
/// it did ("Successfully updated ...", "... is up to date").
fn last_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Runs one update to completion, or kills it at `limit`. Never fails: a CLI
/// that cannot update is still the CLI it was, so this only logs.
pub async fn run(update: &Update, limit: Duration) -> bool {
    let child = Command::new(&update.argv[0])
        .args(&update.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(limit, child).await {
        Ok(Ok(out)) if out.status.success() => {
            tracing::info!(cli = update.name, result = %last_line(&out.stdout), "agent CLI update");
            true
        }
        Ok(Ok(out)) => {
            let detail = match last_line(&out.stderr) {
                s if s.is_empty() => last_line(&out.stdout),
                s => s,
            };
            tracing::warn!(cli = update.name, status = %out.status, %detail, "agent CLI update failed");
            false
        }
        Ok(Err(e)) => {
            tracing::warn!(cli = update.name, error = %e, "agent CLI update could not start");
            false
        }
        Err(_) => {
            tracing::warn!(
                cli = update.name,
                limit_secs = limit.as_secs(),
                "agent CLI update timed out"
            );
            false
        }
    }
}

/// Updates every installed agent CLI, side by side.
pub async fn update_all(home: PathBuf, limit: Duration) {
    let updates = planned(&home);
    futures::future::join_all(updates.iter().map(|u| run(u, limit))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn nothing_installed_means_nothing_to_update() {
        let home = tempfile::tempdir().unwrap();
        assert!(planned(home.path()).is_empty());
    }

    #[test]
    fn each_installed_cli_gets_its_own_update() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let local_bin = h.join(".local").join("bin");
        touch(&local_bin.join("claude"));
        touch(&local_bin.join("codex"));
        touch(&local_bin.join("opencode"));

        let updates = planned(h);
        let names: Vec<_> = updates.iter().map(|u| u.name).collect();
        assert_eq!(names, ["claude", "codex", "opencode"]);
        assert_eq!(
            updates[0].argv,
            [
                local_bin.join("claude").display().to_string(),
                "update".into()
            ]
        );
        assert_eq!(updates[1].argv[..2], ["bash", "-c"]);
        assert!(updates[1].argv[2].contains("releases/latest"));
        assert_eq!(updates[2].argv[..2], ["bash", "-c"]);
        assert!(updates[2].argv[2].contains("opencode.ai/install"));
    }

    /// OpenCode is pinned: its update is the pinned installer, which leaves an
    /// install at the pin alone, never `opencode upgrade` to the latest.
    #[test]
    fn opencode_follows_the_pin_and_never_self_upgrades() {
        let home = tempfile::tempdir().unwrap();
        touch(&home.path().join(".local").join("bin").join("opencode"));
        let updates = planned(home.path());
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "opencode");
        assert!(!updates[0].argv.iter().any(|a| a == "upgrade"));
        let pin = crate::agents::opencode_backend::PINNED_OPENCODE_VERSION;
        assert!(updates[0].argv[2].contains(&format!("pinned={pin}\n")));
    }

    /// OpenCode's own installer's copy is the user's, not BondSymphonic's.
    #[test]
    fn a_user_installed_opencode_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        touch(&home.path().join(".opencode").join("bin").join("opencode"));
        assert!(planned(home.path()).is_empty());
    }

    #[test]
    fn a_directory_is_not_an_installed_cli() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".local").join("bin").join("claude")).unwrap();
        assert!(planned(home.path()).is_empty());
    }

    #[test]
    fn the_last_line_skips_trailing_blank_lines() {
        assert_eq!(
            last_line(b"Checking...\nUp to date (1.2.3)\n\n"),
            "Up to date (1.2.3)"
        );
        assert_eq!(last_line(b""), "");
    }

    #[tokio::test]
    async fn a_missing_program_fails_without_panicking() {
        let update = Update {
            name: "missing",
            argv: vec!["definitely-not-a-real-binary-xyz".into(), "update".into()],
        };
        assert!(!run(&update, Duration::from_secs(5)).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_update_is_killed_at_the_limit() {
        let update = Update {
            name: "hung",
            argv: vec!["sleep".into(), "30".into()],
        };
        let start = std::time::Instant::now();
        assert!(!run(&update, Duration::from_secs(1)).await);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_update_reports_failure() {
        let update = Update {
            name: "failing",
            argv: vec!["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()],
        };
        assert!(!run(&update, Duration::from_secs(5)).await);
    }
}
