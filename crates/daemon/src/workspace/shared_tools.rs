//! Tools an agent installs, shared by every workspace on the same repository.
//!
//! A sandbox's root is read-only, so whatever an agent installs -- the .NET
//! SDK through `dotnet-install.sh`, a Rust toolchain through rustup, a Python
//! CLI through `pip --user` -- lands in its home, and every workspace has a
//! home of its own. Without this, the next agent on the same repository starts
//! from nothing and installs it all again.
//!
//! So the directories those installers write to by default ([`SHARED_DIRS`])
//! are not the workspace's own: each is bound, read-write, from
//! `<data>/tools/<repo key>/<dir>` over the same path in the sandbox home. The
//! installers need no flags and the agents need not know; the sandbox home is
//! `/home/<user>` in every workspace, so absolute paths an installer writes
//! (shebangs, rustup's `env`) are right in all of them.
//!
//! The repository is the trust boundary this draws: one agent can put a
//! program there that another agent on the same repository runs. Nothing of
//! the home besides these directories is shared -- not the agent's own
//! state, not `.local/share`, not the cache.
//!
//! Only under bubblewrap, which has mounts; the noop backend runs processes
//! with the workspace home as a plain `HOME` and nothing is bound.

use crate::daemon::Daemon;
use crate::workspace::Workspace;
use std::path::{Component, Path, PathBuf};

/// The home directories shared per repository, relative to the home.
///
/// Fixed rather than configurable: each one is a directory an installer
/// writes to by default, and none holds an agent's own state. `.local` is
/// shared only in part -- `bin` and `lib` are where `pip --user` and pipx put
/// programs, while `.local/share` and `.local/state` hold agent state.
pub const SHARED_DIRS: [&str; 9] = [
    ".dotnet",
    ".cargo",
    ".rustup",
    "go",
    ".nvm",
    ".bun",
    ".deno",
    ".local/bin",
    ".local/lib",
];

/// The `PATH` entries, relative to the home, for the shared tools whose
/// installers leave them off it (`.local/bin` and `.cargo/bin` are already on
/// the base path).
pub const EXTRA_PATH_DIRS: [&str; 5] = [
    ".dotnet",
    ".dotnet/tools",
    "go/bin",
    ".bun/bin",
    ".deno/bin",
];

/// The directory name of one repository's shared tools: the repository's
/// folder name, for whoever looks in the data directory, and a hash of its
/// full path, so two repositories with the same folder name stay apart.
///
/// FNV-1a rather than `DefaultHasher`, whose output the standard library does
/// not promise to keep from one release to the next: a daemon built with a
/// newer toolchain must find the tools the old one installed.
pub fn repo_key(repo: &Path) -> String {
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let name: String = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if name.is_empty() {
        format!("{hash:016x}")
    } else {
        format!("{name}-{hash:016x}")
    }
}

/// `<data>/tools/<repo key>`: the shared tools of `ws`'s repository.
pub fn tools_root(d: &Daemon, ws: &Workspace) -> PathBuf {
    d.dirs.tools.join(repo_key(&ws.repo_path))
}

/// The `(host, sandbox)` read-write binds that put the shared tools in place,
/// creating each host directory first: bwrap refuses a bind whose source is
/// missing, and an empty directory is how a repository with nothing installed
/// yet looks.
///
/// A directory that cannot be created is left out with a warning rather than
/// failing the start: the agent then installs into its own home, as it did
/// before this existed.
pub fn binds(d: &Daemon, ws: &Workspace, home_in_sandbox: &Path) -> Vec<(PathBuf, PathBuf)> {
    let root = tools_root(d, ws);
    SHARED_DIRS
        .iter()
        .filter_map(|rel| {
            let host = root.join(rel);
            match std::fs::create_dir_all(&host) {
                Ok(()) => Some((host, home_in_sandbox.join(rel))),
                Err(e) => {
                    tracing::warn!(ws = %ws.id, dir = %host.display(), "shared tools directory unavailable: {e}");
                    None
                }
            }
        })
        .collect()
}

/// Moves what the workspace installed in its own home, before tools were
/// shared, into the repository's shared tools -- but only into a shared
/// directory that is still empty, so nothing another workspace put there is
/// ever replaced.
///
/// Without this the bind would hide it: an agent that installed .NET last week
/// would find it gone after the upgrade. A private directory that is not moved
/// stays where it is, hidden under the bind, and is removed with the workspace.
///
/// Every component of the private path must be a real directory, not a
/// symlink. The agent owns its home and could point `.local` at a directory of
/// the daemon user's; the daemon renaming through that link would move the
/// daemon user's files into a directory the next sandbox gets read-write.
pub fn adopt_private(d: &Daemon, ws: &Workspace) {
    let home = d.dirs.home(&ws.id);
    let root = tools_root(d, ws);
    for rel in SHARED_DIRS {
        let private = home.join(rel);
        if !is_real_dir_under(&home, Path::new(rel)) || is_empty_dir(&private) {
            continue;
        }
        let shared = root.join(rel);
        if shared.exists() && !is_empty_dir(&shared) {
            continue;
        }
        if let Some(parent) = shared.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(ws = %ws.id, dir = %parent.display(), "shared tools directory unavailable: {e}");
                continue;
            }
        }
        // `rename` replaces an empty directory and fails on a non-empty one,
        // so a second workspace of the same repository adopting at the same
        // moment loses cleanly instead of overwriting the first.
        match std::fs::rename(&private, &shared) {
            Ok(()) => {
                tracing::info!(ws = %ws.id, dir = rel, "moved installed tools to the repository's shared tools")
            }
            Err(e) => tracing::warn!(ws = %ws.id, dir = rel, "kept installed tools private: {e}"),
        }
    }
}

/// Whether `base/rel` is a directory and neither it nor anything between it
/// and `base` is a symlink.
fn is_real_dir_under(base: &Path, rel: &Path) -> bool {
    let mut at = base.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return false;
        };
        at.push(name);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            _ => return false,
        }
    }
    true
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_is_stable_and_names_the_folder() {
        let key = repo_key(Path::new("/nonexistent/src/My Repo"));
        assert_eq!(key, repo_key(Path::new("/nonexistent/src/My Repo")));
        assert!(key.starts_with("My_Repo-"), "{key}");
        assert_eq!(key.len(), "My_Repo-".len() + 16);
    }

    #[test]
    fn same_folder_name_in_two_places_gets_two_keys() {
        assert_ne!(
            repo_key(Path::new("/nonexistent/a/app")),
            repo_key(Path::new("/nonexistent/b/app"))
        );
    }

    #[test]
    fn a_symlinked_component_is_not_a_real_dir() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("bin")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&elsewhere, home.join(".local")).unwrap();
            assert!(!is_real_dir_under(&home, Path::new(".local/bin")));
        }
        std::fs::create_dir_all(home.join(".dotnet")).unwrap();
        assert!(is_real_dir_under(&home, Path::new(".dotnet")));
        assert!(!is_real_dir_under(&home, Path::new(".cargo")));
    }

    #[test]
    fn empty_and_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_empty_dir(dir.path()));
        assert!(!is_empty_dir(&dir.path().join("missing")));
        std::fs::write(dir.path().join("f"), b"").unwrap();
        assert!(!is_empty_dir(dir.path()));
    }
}
