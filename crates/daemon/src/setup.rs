//! The host-side setup terminals: what they run, and where they run it.
//!
//! Four of the prerequisites `system.check_prereqs` reports cannot be fixed by
//! the daemon on its own — two installs and two interactive logins. Each is
//! fixed by running one known command in a terminal the user can see and type
//! into, which is what `system.setup_pty` opens. The two logouts run there for
//! the same reason, though what they do is the opposite: they take a login
//! away, so the user can put a different one in its place.
//!
//! Those commands run on the *host*, outside every workspace sandbox: a login
//! has to write credentials into the daemon user's real home and reach the
//! network, and an install has to touch the real system, all of which a
//! workspace sandbox exists to prevent. That makes the argv table here a
//! security boundary rather than a convenience: [`setup_argv`] takes a
//! [`SetupAction`] and nothing else, so no client can name the program that
//! runs unsandboxed on the developer's machine.

use bondsymphonic_proto::SetupAction;
use std::path::{Path, PathBuf};

/// The sandbox backend the host handle is built from. Always the no-sandbox
/// one, whatever the daemon's own backend is: everything a setup command has
/// to do is what a sandbox takes away.
pub const HOST_BACKEND: &str = "noop";

/// Every action, for callers that need to enumerate them (tests, and any future
/// listing of what the daemon can fix).
pub const ALL_ACTIONS: [SetupAction; 6] = [
    SetupAction::ClaudeLogin,
    SetupAction::GhLogin,
    SetupAction::InstallClaude,
    SetupAction::InstallGh,
    SetupAction::ClaudeLogout,
    SetupAction::GhLogout,
];

/// The command each action runs. The whole table is literal: an action selects
/// a row, it never contributes to one.
pub fn setup_argv(action: SetupAction) -> Vec<String> {
    let argv: &[&str] = match action {
        SetupAction::ClaudeLogin => &["claude", "auth", "login"],
        SetupAction::GhLogin => &["gh", "auth", "login"],
        SetupAction::InstallClaude => &[
            "bash",
            "-lc",
            "curl -fsSL https://claude.ai/install.sh | bash",
        ],
        SetupAction::InstallGh => &["sudo", "apt-get", "install", "-y", "gh"],
        // Logging out is as interactive as logging in -- `gh auth logout` asks
        // which host to forget and then asks again whether it means it -- so
        // both run in the same terminal the logins do rather than being fired
        // off with no way to answer them.
        SetupAction::ClaudeLogout => &["claude", "auth", "logout"],
        SetupAction::GhLogout => &["gh", "auth", "logout"],
    };
    argv.iter().map(|s| (*s).to_string()).collect()
}

/// The daemon user's own home directory, which is where a setup command has to
/// write: `claude auth login` puts its credentials under `~/.claude`, and
/// `gh auth login` under `~/.config/gh`. Unlike a workspace's home this is not
/// a directory the daemon owns and can recreate.
///
/// `$HOME` is consulted after [`directories::BaseDirs`] rather than before it,
/// so the answer matches what every other part of the daemon calls home, and
/// the temp directory is the last resort so callers never get an empty path.
pub fn host_home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
}

/// A `PATH` for a setup terminal: the daemon's own with `~/.local/bin` in
/// front.
///
/// `claude` installs itself there, and a daemon started by the IDE rather than
/// from a login shell need not have that directory on its `PATH` — so
/// `claude auth login` would fail to resolve in a terminal opened right after
/// the install that put it there. Prepending is safe when the directory is
/// already on the path: the duplicate resolves to the same program.
pub fn path_with_local_bin(home: &Path) -> (String, String) {
    let local_bin = home.join(".local").join("bin");
    let separator = if cfg!(windows) { ';' } else { ':' };
    let inherited = std::env::var("PATH").unwrap_or_default();
    let path = if inherited.is_empty() {
        local_bin.display().to_string()
    } else {
        format!("{}{separator}{inherited}", local_bin.display())
    };
    ("PATH".to_string(), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_action_maps_to_its_fixed_command() {
        assert_eq!(
            setup_argv(SetupAction::ClaudeLogin),
            ["claude", "auth", "login"]
        );
        assert_eq!(setup_argv(SetupAction::GhLogin), ["gh", "auth", "login"]);
        assert_eq!(
            setup_argv(SetupAction::InstallClaude),
            [
                "bash",
                "-lc",
                "curl -fsSL https://claude.ai/install.sh | bash"
            ]
        );
        assert_eq!(
            setup_argv(SetupAction::InstallGh),
            ["sudo", "apt-get", "install", "-y", "gh"]
        );
        assert_eq!(
            setup_argv(SetupAction::ClaudeLogout),
            ["claude", "auth", "logout"]
        );
        assert_eq!(setup_argv(SetupAction::GhLogout), ["gh", "auth", "logout"]);
    }

    /// Each provider's logout runs the same program as its login. What makes a
    /// logout safe is that it is the login's own tool putting its own
    /// credentials away: nothing here deletes a file by path, so no row can be
    /// pointed at something that is not a session.
    #[test]
    fn each_logout_runs_its_own_providers_tool() {
        assert_eq!(
            setup_argv(SetupAction::ClaudeLogout)[0],
            setup_argv(SetupAction::ClaudeLogin)[0]
        );
        assert_eq!(
            setup_argv(SetupAction::GhLogout)[0],
            setup_argv(SetupAction::GhLogin)[0]
        );
    }

    /// The table is closed: every action runs one of the four known programs, and
    /// the only input is the action, so there is no argument through which a
    /// caller could reach a program of its own. A new action added without a
    /// row would not compile; one added with a row lands here.
    #[test]
    fn every_action_runs_one_of_the_four_known_programs() {
        for action in ALL_ACTIONS {
            let argv = setup_argv(action);
            assert!(
                ["claude", "gh", "bash", "sudo"].contains(&argv[0].as_str()),
                "{action:?} runs an unexpected program: {argv:?}"
            );
        }
    }

    #[test]
    fn the_setup_path_leads_with_the_home_local_bin() {
        let (key, value) = path_with_local_bin(Path::new("/home/bs"));
        assert_eq!(key, "PATH");
        let expected_first = Path::new("/home/bs").join(".local").join("bin");
        assert!(
            value.starts_with(&expected_first.display().to_string()),
            "{value}"
        );
        // Whatever the daemon inherited is still reachable behind it.
        if let Ok(inherited) = std::env::var("PATH") {
            assert!(value.ends_with(&inherited), "{value}");
        }
    }

    #[test]
    fn the_host_home_is_never_empty() {
        assert!(!host_home().as_os_str().is_empty());
    }
}
