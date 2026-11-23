//! The `git` every process in a worktree workspace's sandbox runs.
//!
//! A worktree workspace keeps the objects its agent creates in a private
//! directory, because the main repository's object store is mounted read-only
//! into the sandbox. Git is told where that directory is through
//! `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and those two
//! used to be set for every process in the sandbox. They are not scoped to a
//! repository: every git command saw the workspace's object store, whichever
//! repository it ran in. Reading any other checkout failed (`fatal: bad object
//! HEAD`), and a repository the agent made itself -- `git init` in `/tmp`, a
//! test suite's scratch repo -- silently put its objects in the workspace's
//! store, where nothing outside the sandbox could find them.
//!
//! So the sandbox carries the routing under names git does not read
//! ([`COMMON_ENV`], [`OBJECTS_ENV`], [`ALTERNATES_ENV`]), and `git` on its
//! `PATH` is this shim: a symlink to the daemon binary, which recognises the
//! name it was run under. It asks the real git which repository the command is
//! for, and sets the two git variables only when that is the workspace's own;
//! anything else runs plain git.
//!
//! Getting this wrong inside the workspace cannot write into the user's
//! repository: the main object store is read-only in the sandbox, so a commit
//! routed the plain way fails instead. The shim is about git working, not
//! about what the sandbox allows.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where the shim is linked inside the sandbox, first on its `PATH`.
pub const SHIM_DIR: &str = "/opt/bs/bin";
/// The workspace repository's common git directory (the main `.git`).
pub const COMMON_ENV: &str = "BS_GIT_COMMON_DIR";
/// What `GIT_OBJECT_DIRECTORY` becomes for the workspace repository.
pub const OBJECTS_ENV: &str = "BS_GIT_OBJECT_DIRECTORY";
/// What `GIT_ALTERNATE_OBJECT_DIRECTORIES` becomes for it.
pub const ALTERNATES_ENV: &str = "BS_GIT_ALTERNATE_OBJECT_DIRECTORIES";

const GIT_OBJECTS: &str = "GIT_OBJECT_DIRECTORY";
const GIT_ALTERNATES: &str = "GIT_ALTERNATE_OBJECT_DIRECTORIES";

/// Global options that take their value as the next argument.
const VALUE_OPTIONS: [&str; 6] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
];

/// Whether this process was started as `git`, which is how the shim is run.
pub fn invoked_as_git() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|argv0| Path::new(&argv0).file_name() == Some(OsStr::new("git")))
}

/// Runs the real git with the routing the command's repository calls for.
/// Never returns: the process becomes git.
pub fn run() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let real = real_git();
    let mut cmd = Command::new(&real);
    cmd.args(&args);
    // Never inherited as such. A hook git runs inside the workspace calls
    // `git` with these already set by the shim around its parent; that call
    // decides for itself, the same way, like every other.
    cmd.env_remove(GIT_OBJECTS).env_remove(GIT_ALTERNATES);
    if let Some((objects, alternates)) = routing(&real, &args) {
        cmd.env(GIT_OBJECTS, objects)
            .env(GIT_ALTERNATES, alternates);
    }
    let error = cmd.exec();
    eprintln!(
        "git (BondSymphonic shim): could not run {}: {error}",
        real.display()
    );
    std::process::exit(127)
}

/// The two object directories when `args` is a command on the workspace's own
/// repository, else `None`.
fn routing(real: &Path, args: &[OsString]) -> Option<(OsString, OsString)> {
    let common = std::env::var_os(COMMON_ENV)?;
    let objects = std::env::var_os(OBJECTS_ENV)?;
    let alternates = std::env::var_os(ALTERNATES_ENV)?;
    let (globals, command) = split_globals(args);
    // These make a repository rather than work in one. Run inside the worktree
    // the probe below would name the workspace's, and the new repository's
    // objects would land in the workspace's store.
    if matches!(command.and_then(|c| c.to_str()), Some("init" | "clone")) {
        return None;
    }
    let out = Command::new(real)
        .args(globals)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env_remove(GIT_OBJECTS)
        .env_remove(GIT_ALTERNATES)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let found = String::from_utf8(out.stdout).ok()?;
    same_dir(Path::new(found.trim()), Path::new(&common)).then_some((objects, alternates))
}

/// The global options before the subcommand, and the subcommand itself.
pub fn split_globals(args: &[OsString]) -> (&[OsString], Option<&OsString>) {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.to_string_lossy();
        if !text.starts_with('-') {
            return (&args[..i], Some(arg));
        }
        i += if VALUE_OPTIONS.contains(&text.as_ref()) {
            2
        } else {
            1
        };
    }
    (&args[..args.len().min(i)], None)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The first `git` on `PATH` that is not this shim.
fn real_git() -> PathBuf {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir != Path::new(SHIM_DIR))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file() && std::fs::canonicalize(candidate).ok() != me)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_subcommand_is_found_after_the_global_options() {
        let args = os(&["-C", "/w", "-c", "a=b", "--no-pager", "commit", "-m", "x"]);
        let (globals, command) = split_globals(&args);
        assert_eq!(globals, &args[..5]);
        assert_eq!(command, Some(&OsString::from("commit")));

        let args = os(&["--git-dir=/g", "log"]);
        let (globals, command) = split_globals(&args);
        assert_eq!(globals, &args[..1]);
        assert_eq!(command, Some(&OsString::from("log")));

        let args = os(&["--version"]);
        assert_eq!(split_globals(&args), (&args[..], None));
        // A value option at the very end takes nothing past the end.
        let args = os(&["-C"]);
        assert_eq!(split_globals(&args), (&args[..], None));
    }
}
