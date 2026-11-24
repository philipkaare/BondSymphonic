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
/// The prefix of the workspace's own branches, `bs/<name>/`: the ones its
/// agent may create, switch between and delete.
pub const PREFIX_ENV: &str = "BS_GIT_BRANCH_PREFIX";
/// The branch the workspace was created with, which is never deleted.
pub const BRANCH_ENV: &str = "BS_GIT_WORKSPACE_BRANCH";

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
    // Somebody else's choice is left exactly as it is: a test harness, a
    // build tool, or git itself handing a hook a push's quarantined objects
    // all set `GIT_OBJECT_DIRECTORY` on purpose, and the shim stripping it
    // sent their objects to the wrong store.
    if set_elsewhere(
        std::env::var_os(GIT_OBJECTS).as_deref(),
        std::env::var_os(OBJECTS_ENV).as_deref(),
    ) {
        exec(cmd, &real);
    }
    // The shim's own, inherited from the shim around a parent git (a hook
    // inside the workspace calling `git`): that call decides for itself, the
    // same way, like every other.
    cmd.env_remove(GIT_OBJECTS).env_remove(GIT_ALTERNATES);
    if let Some((objects, alternates)) = routing(&real, &args) {
        // Deleting one of the workspace's own branches is done here: git
        // insists on locking the shared `packed-refs` for every deletion, and
        // that file is read-only in the sandbox even when the branch is not.
        if let Some(delete) = branch_deletion(&args) {
            std::process::exit(delete_own_branches(&real, &delete, &objects, &alternates));
        }
        // A branch made from a remote one (`git switch -c <name> origin/x`)
        // would otherwise set up tracking, which is written to the
        // repository's config -- read-only here -- and fails the whole
        // command. The branch is made untracked instead; the agent compares
        // with `origin/x` by name, and pushes through its tools.
        let mut routed = Command::new(&real);
        routed
            .args(["-c", "branch.autoSetupMerge=false"])
            .args(&args)
            .env_remove(GIT_OBJECTS)
            .env_remove(GIT_ALTERNATES)
            .env(GIT_OBJECTS, objects)
            .env(GIT_ALTERNATES, alternates);
        exec(routed, &real);
    }
    exec(cmd, &real)
}

/// A `git branch -d|-D|--delete [-f] <name>...` the shim carries out itself.
#[derive(Debug, PartialEq)]
pub struct BranchDeletion {
    pub globals: Vec<OsString>,
    pub force: bool,
    pub names: Vec<String>,
}

/// `args` as a branch deletion, or `None` for anything else -- including a
/// deletion with an option this does not know, which is then git's to answer.
pub fn branch_deletion(args: &[OsString]) -> Option<BranchDeletion> {
    let (globals, command) = split_globals(args);
    if command?.to_str()? != "branch" {
        return None;
    }
    let rest = &args[globals.len() + 1..];
    let (mut delete, mut force, mut names) = (false, false, Vec::new());
    for arg in rest {
        let arg = arg.to_str()?;
        match arg {
            "-d" | "--delete" => delete = true,
            "-D" => (delete, force) = (true, true),
            "-f" | "--force" => force = true,
            _ if arg.starts_with("--") => return None,
            _ if arg.starts_with('-') && arg.len() > 1 => {
                for flag in arg[1..].chars() {
                    match flag {
                        'd' => delete = true,
                        'D' => (delete, force) = (true, true),
                        'f' => force = true,
                        _ => return None,
                    }
                }
            }
            _ => names.push(arg.to_owned()),
        }
    }
    (delete && !names.is_empty()).then(|| BranchDeletion {
        globals: globals.to_vec(),
        force,
        names,
    })
}

/// Deletes `request`'s branches the way `git branch -d` would, as far as the
/// sandbox allows: only the workspace's own, never the one it was created
/// with nor the one checked out, and with `-d` only when merged into `HEAD`.
/// Returns the exit code, 1 when any branch was refused.
fn delete_own_branches(
    real: &Path,
    request: &BranchDeletion,
    objects: &OsStr,
    alternates: &OsStr,
) -> i32 {
    let prefix = std::env::var(PREFIX_ENV).unwrap_or_default();
    let own = std::env::var(BRANCH_ENV).unwrap_or_default();
    let common = PathBuf::from(std::env::var_os(COMMON_ENV).unwrap_or_default());
    let git = |args: &[&str]| {
        Command::new(real)
            .args(&request.globals)
            .args(args)
            .env(GIT_OBJECTS, objects)
            .env(GIT_ALTERNATES, alternates)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let head = git(&["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
    let mut code = 0;
    for name in &request.names {
        let mut refuse = |why: String| {
            eprintln!("error: {why}");
            code = 1;
        };
        if prefix.is_empty() || !is_own_branch(&prefix, name) {
            refuse(format!(
                "cannot delete branch '{name}': only this workspace's own branches \
                 ({prefix}...) can be deleted in its sandbox"
            ));
            continue;
        }
        if *name == own {
            refuse(format!(
                "cannot delete branch '{name}': it is the branch this workspace was created with"
            ));
            continue;
        }
        let full = format!("refs/heads/{name}");
        let Some(sha) = git(&["rev-parse", "--verify", "-q", &full]) else {
            refuse(format!("branch '{name}' not found."));
            continue;
        };
        if head == full {
            refuse(format!("cannot delete branch '{name}': it is checked out"));
            continue;
        }
        if !request.force && git(&["merge-base", "--is-ancestor", &sha, "HEAD"]).is_none() {
            refuse(format!(
                "the branch '{name}' is not fully merged.\nIf you are sure you want to \
                 delete it, run 'git branch -D {name}'."
            ));
            continue;
        }
        let file = common.join("refs").join("heads").join(name);
        if !file.is_file() {
            refuse(format!(
                "cannot delete branch '{name}': it is packed in the repository's shared \
                 refs, which this sandbox cannot rewrite; ask the user to delete it"
            ));
            continue;
        }
        if let Err(e) = std::fs::remove_file(&file) {
            refuse(format!("cannot delete branch '{name}': {e}"));
            continue;
        }
        let _ = std::fs::remove_file(common.join("logs").join("refs").join("heads").join(name));
        // Directories a nested name made (`bs/<name>/fix/a`), as git leaves
        // none behind; never the workspace's own directory.
        let root = common
            .join("refs")
            .join("heads")
            .join(prefix.trim_end_matches('/'));
        let mut dir = file.parent().map(Path::to_path_buf);
        while let Some(d) = dir.filter(|d| d.starts_with(&root) && *d != root) {
            if std::fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        let short = sha.get(..7).unwrap_or(&sha);
        println!("Deleted branch {name} (was {short}).");
    }
    code
}

/// Whether `name` is under `prefix` with a name of its own, and nothing in it
/// that could reach outside the prefix's directory.
pub fn is_own_branch(prefix: &str, name: &str) -> bool {
    name.len() > prefix.len()
        && name.starts_with(prefix)
        && !name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn exec(mut cmd: Command, real: &Path) -> ! {
    let error = cmd.exec();
    eprintln!(
        "git (BondSymphonic shim): could not run {}: {error}",
        real.display()
    );
    std::process::exit(127)
}

/// Whether `GIT_OBJECT_DIRECTORY` was set by something other than the shim:
/// it is set, and not to the workspace's private directory, which is the only
/// value the shim ever gives it.
pub fn set_elsewhere(inherited: Option<&OsStr>, ours: Option<&OsStr>) -> bool {
    inherited.is_some_and(|value| Some(value) != ours)
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
    fn a_branch_deletion_is_recognised_and_anything_else_is_not() {
        let deletion = branch_deletion(&os(&["branch", "-d", "bs/a/x", "bs/a/y"])).unwrap();
        assert!(!deletion.force);
        assert_eq!(deletion.names, ["bs/a/x", "bs/a/y"]);
        assert!(
            branch_deletion(&os(&["branch", "-D", "bs/a/x"]))
                .unwrap()
                .force
        );
        assert!(
            branch_deletion(&os(&["branch", "--delete", "--force", "bs/a/x"]))
                .unwrap()
                .force
        );
        assert!(
            branch_deletion(&os(&["branch", "-df", "bs/a/x"]))
                .unwrap()
                .force
        );
        let with_globals = branch_deletion(&os(&["-C", "/w", "branch", "-d", "bs/a/x"])).unwrap();
        assert_eq!(with_globals.globals, os(&["-C", "/w"]));
        // Not a deletion, or one with an option this does not know: git's.
        assert_eq!(branch_deletion(&os(&["branch", "bs/a/x"])), None);
        assert_eq!(branch_deletion(&os(&["branch", "-d"])), None);
        assert_eq!(
            branch_deletion(&os(&["branch", "-d", "-r", "origin/x"])),
            None
        );
        assert_eq!(branch_deletion(&os(&["switch", "-d", "x"])), None);
    }

    #[test]
    fn only_names_inside_the_prefix_are_own_branches() {
        assert!(is_own_branch("bs/a/", "bs/a/x"));
        assert!(is_own_branch("bs/a/", "bs/a/fix/x"));
        assert!(!is_own_branch("bs/a/", "bs/a/"));
        assert!(!is_own_branch("bs/a/", "bs/ab/x"));
        assert!(!is_own_branch("bs/a/", "bs/a/../../main"));
        assert!(!is_own_branch("bs/a/", "bs/a//x"));
        assert!(!is_own_branch("bs/a/", "main"));
    }

    /// An object directory the shim did not choose is left alone; one it did
    /// is decided again.
    #[test]
    fn only_the_shims_own_object_directory_is_redecided() {
        let ours = Some(OsStr::new("/data/objects/ws_1"));
        assert!(!set_elsewhere(None, ours));
        assert!(!set_elsewhere(ours, ours));
        assert!(set_elsewhere(Some(OsStr::new("/tmp/quarantine")), ours));
        assert!(set_elsewhere(Some(OsStr::new("/tmp/x")), None));
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
