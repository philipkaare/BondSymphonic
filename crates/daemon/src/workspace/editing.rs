//! User-initiated commit and ignore operations, under lifecycle then repository locks.
use crate::{
    daemon::Daemon,
    git::Git,
    workspace::{in_place::InPlaceLayout, lifecycle, Workspace},
};
use bondsymphonic_proto::*;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path},
};

/// The workspace's pinned Git. Its `core.hooksPath` is the daemon's empty
/// directory: a commit the user asks for still runs over content an agent
/// wrote, so no hook (husky, pre-commit) may run it outside the sandbox.
async fn git_for(d: &Daemon, ws: &Workspace) -> Result<Git, RpcError> {
    Ok(match ws.kind {
        WorkspaceKind::InPlace => InPlaceLayout::new(&ws.worktree_path, &d.dirs.no_hooks()).git(),
        WorkspaceKind::Worktree => lifecycle::layout_for(d, ws).await?.worktree_git(),
    })
}

fn ready(ws: &Workspace) -> Result<(), RpcError> {
    if ws.state != WorkspaceState::Ready {
        return Err(RpcError::invalid_params("workspace is not ready"));
    }
    Ok(())
}

fn valid_path(path: &str, root: bool) -> Result<(), RpcError> {
    if root && path == "." {
        return Ok(());
    }
    if path.is_empty()
        || path.contains(['\0', '\n', '\r'])
        || path.contains('\\')
        || Path::new(path).is_absolute()
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git"))
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(RpcError::invalid_params(
            "expected a workspace-relative path without traversal or .git",
        ));
    }
    Ok(())
}

pub async fn prepare_commit(d: &Daemon, id: &WorkspaceId) -> Result<PrepareCommitResult, RpcError> {
    let gate = lifecycle::gate(id);
    let _gate = gate.lock().await;
    let ws = d.workspace(id)?;
    ready(&ws)?;
    let lock = crate::git::repo_lock(&ws.repo_path);
    let _lock = lock.lock().await;
    prepare_locked(d, &ws).await
}

pub(crate) async fn prepare_locked(
    d: &Daemon,
    ws: &Workspace,
) -> Result<PrepareCommitResult, RpcError> {
    let git = git_for(d, ws).await?;
    // NUL format prevents tabs/newlines and rename-source fields becoming fake paths.
    let out = git
        .run(
            &ws.worktree_path,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=all",
            ],
        )
        .await?;
    let mut chunks = out.stdout.split('\0');
    let mut entries = BTreeMap::new();
    while let Some(row) = chunks.next() {
        if row.len() < 3 {
            continue;
        }
        let xy = row.as_bytes();
        let path = &row[3..];
        if xy[0] == b'R' || xy[0] == b'C' || xy[1] == b'R' || xy[1] == b'C' {
            chunks.next();
        }
        let code = if xy[0] != b' ' { xy[0] } else { xy[1] };
        let status = match code {
            b'?' => FileStatus::Untracked,
            b'A' => FileStatus::Added,
            b'D' => FileStatus::Deleted,
            b'R' | b'C' => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        entries.insert(
            path.to_owned(),
            GitStatusEntry {
                path: path.to_owned(),
                status,
                staged: xy[0] != b' ' && xy[0] != b'?',
            },
        );
    }
    // This cached diff reads only the parent index and trees: status above
    // suppresses child traversal, which otherwise also hides staged gitlinks.
    let staged = git
        .run(
            &ws.worktree_path,
            &[
                "diff",
                "--cached",
                "--name-status",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
            ],
        )
        .await?;
    let mut rows = staged.stdout.split('\0');
    while let Some(code) = rows.next() {
        if code.is_empty() {
            continue;
        }
        let Some(path) = rows.next() else {
            break;
        };
        let status = match code.as_bytes().first() {
            Some(b'A') => FileStatus::Added,
            Some(b'D') => FileStatus::Deleted,
            _ => FileStatus::Modified,
        };
        entries
            .entry(path.to_owned())
            .and_modify(|entry| entry.staged = true)
            .or_insert_with(|| GitStatusEntry {
                path: path.to_owned(),
                status,
                staged: true,
            });
    }
    let ahead = if ws.kind == WorkspaceKind::InPlace {
        0
    } else {
        git.run(
            &ws.worktree_path,
            &["rev-list", "--count", &format!("{}..HEAD", ws.base_branch)],
        )
        .await?
        .stdout
        .trim()
        .parse()
        .map_err(|_| RpcError::internal("invalid ahead count"))?
    };
    Ok(PrepareCommitResult {
        files: entries.into_values().collect(),
        ahead,
    })
}

async fn check_commit_branch(git: &Git, ws: &Workspace) -> Result<(), RpcError> {
    let head = git
        .run(
            &ws.worktree_path,
            &["symbolic-ref", "--no-recurse", "-q", "HEAD"],
        )
        .await;
    if ws.kind == WorkspaceKind::Worktree
        && head.as_ref().map(|o| o.stdout.trim()).ok()
            != Some(format!("refs/heads/{}", ws.branch).as_str())
    {
        return Err(RpcError::invalid_params(
            "workspace HEAD no longer names its own branch",
        ));
    }
    if let Ok(head) = head {
        let branch = head.stdout.trim();
        if git
            .run(&ws.worktree_path, &["symbolic-ref", "-q", branch])
            .await
            .is_ok()
        {
            return Err(RpcError::invalid_params(
                "the checkout branch is a symbolic ref and cannot be committed",
            ));
        }
    }
    Ok(())
}

async fn check_commit_operation(git: &Git, ws: &Workspace) -> Result<(), RpcError> {
    // Resolve per-worktree state through pinned Git rather than .git discovery.
    // A private index must never consume another operation's resolutions.
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        let output = git
            .run(&ws.worktree_path, &["rev-parse", "--git-path", marker])
            .await?;
        let path = ws.worktree_path.join(output.stdout.trim());
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Err(RpcError::invalid_params(format!("A Git operation is in progress ({marker}); finish or abort it before committing selected files"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(RpcError::io(&error)),
        }
    }
    Ok(())
}

async fn selected_paths(
    git: &Git,
    ws: &Workspace,
    selected: &[String],
) -> Result<Vec<String>, RpcError> {
    let output = git
        .run(
            &ws.worktree_path,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=all",
            ],
        )
        .await?;
    let mut paths = selected.to_vec();
    let mut rows = output.stdout.split('\0');
    let includes = |path: &str| {
        selected.iter().any(|p| {
            p == "."
                || path == p
                || path
                    .strip_prefix(p.as_str())
                    .is_some_and(|s| s.starts_with('/'))
        })
    };
    while let Some(row) = rows.next() {
        if row.len() < 3 {
            continue;
        }
        let xy = row.as_bytes();
        if xy[0] == b'R' || xy[1] == b'R' || xy[0] == b'C' || xy[1] == b'C' {
            let Some(source) = rows.next() else {
                continue;
            };
            let destination = &row[3..];
            if (xy[0] == b'R' || xy[1] == b'R') && (includes(source) || includes(destination)) {
                paths.push(source.to_owned());
                paths.push(destination.to_owned());
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub async fn commit(d: &Daemon, p: &WorkspaceCommitParams) -> Result<CommitResult, RpcError> {
    if p.message.trim().is_empty() || p.message.contains('\0') {
        return Err(RpcError::invalid_params("commit message must not be blank"));
    }
    if p.paths.is_empty() {
        return Err(RpcError::invalid_params(
            "choose at least one path to commit",
        ));
    }
    for path in &p.paths {
        valid_path(path, true)?;
    }
    let gate = lifecycle::gate(&p.workspace_id);
    let _gate = gate.lock().await;
    let ws = d.workspace(&p.workspace_id)?;
    ready(&ws)?;
    let lock = crate::git::repo_lock(&ws.repo_path);
    let _lock = lock.lock().await;
    for path in &p.paths {
        crate::fs::resolve(&ws.worktree_path, path)?;
    }
    let git = git_for(d, &ws).await?;
    check_commit_branch(&git, &ws).await?;
    check_commit_operation(&git, &ws).await?;
    let paths = selected_paths(&git, &ws, &p.paths).await?;
    // A private index includes selected working files and HEAD only, so
    // unrelated staged work never enters the commit.
    let temp = tempfile::tempdir().map_err(|e| RpcError::internal(e.to_string()))?;
    let index = temp.path().join("index");
    let isolated = git
        .clone()
        .with_env("GIT_INDEX_FILE", index.to_string_lossy());
    if git
        .run(&ws.worktree_path, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
    {
        isolated
            .run(&ws.worktree_path, &["read-tree", "HEAD"])
            .await?;
    } else {
        isolated
            .run(&ws.worktree_path, &["read-tree", "--empty"])
            .await?;
    }
    let specs: Vec<String> = paths.iter().map(|p| format!(":(literal){p}")).collect();
    let mut add = vec!["add", "--all", "--"];
    add.extend(specs.iter().map(String::as_str));
    isolated.run(&ws.worktree_path, &add).await?;
    check_commit_branch(&git, &ws).await?;
    check_commit_operation(&git, &ws).await?;
    isolated
        .run(&ws.worktree_path, &["commit", "-m", &p.message])
        .await?;
    let commit = git
        .run(&ws.worktree_path, &["rev-parse", "HEAD"])
        .await?
        .stdout
        .trim()
        .to_owned();
    let mut reset = vec!["reset", "--quiet", "HEAD", "--"];
    reset.extend(specs.iter().map(String::as_str));
    git.run(&ws.worktree_path, &reset).await.map_err(|e| {
        RpcError::internal(format!(
            "Commit {commit} succeeded, but updating selected index entries failed: {}",
            e.message
        ))
    })?;
    Ok(CommitResult { commit })
}

/// Glob characters are escaped everywhere. A space is literal inside a rule
/// and Git trims only trailing ones, so only those are escaped.
fn escape(path: &str) -> String {
    let kept = path.trim_end_matches(' ').len();
    let mut out = String::new();
    for (i, c) in path.char_indices() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '!' | '#') || (c == ' ' && i >= kept) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
fn rule(path: &str, target: IgnoreTarget) -> Result<String, RpcError> {
    valid_path(path, target == IgnoreTarget::Directory)?;
    Ok(match target {
        IgnoreTarget::File => format!("/{}", escape(path)),
        IgnoreTarget::Directory if path == "." => "/*\n!/.gitignore".to_owned(),
        IgnoreTarget::Directory => format!("/{}/", escape(path)),
        IgnoreTarget::FileType => {
            let extension = Path::new(path)
                .extension()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RpcError::invalid_params("file has no extension"))?;
            format!("*.{}", escape(extension))
        }
    })
}

pub async fn ignore_options(
    d: &Daemon,
    p: &WorkspaceIgnoreOptionsParams,
) -> Result<IgnoreOptionsResult, RpcError> {
    valid_path(&p.path, p.directory)?;
    let gate = lifecycle::gate(&p.workspace_id);
    let _gate = gate.lock().await;
    let ws = d.workspace(&p.workspace_id)?;
    ready(&ws)?;
    crate::fs::resolve(&ws.worktree_path, &p.path)?;
    let mut options = Vec::new();
    let mut add = |path: &str, target, label: String| -> Result<(), RpcError> {
        options.push(IgnoreOption {
            label,
            path: path.into(),
            target,
            pattern: rule(path, target)?,
        });
        Ok(())
    };
    if !p.directory {
        add(&p.path, IgnoreTarget::File, format!("File: {}", p.path))?;
    }
    let mut dir = if p.directory {
        Some(Path::new(&p.path))
    } else {
        Path::new(&p.path).parent()
    };
    while let Some(path) = dir {
        let path = path.to_str().unwrap_or_default();
        if path.is_empty() || path == "." {
            break;
        }
        add(path, IgnoreTarget::Directory, format!("Directory: {path}"))?;
        dir = Path::new(path).parent();
    }
    add(".", IgnoreTarget::Directory, "Workspace root".into())?;
    if let Some(extension) = Path::new(&p.path)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && !p.directory)
    {
        add(
            &p.path,
            IgnoreTarget::FileType,
            format!("All *.{extension} files"),
        )?;
    }
    Ok(IgnoreOptionsResult { options })
}

pub async fn ignore(d: &Daemon, p: &WorkspaceIgnoreParams) -> Result<IgnoreResult, RpcError> {
    let pattern = rule(&p.path, p.target)?;
    let gate = lifecycle::gate(&p.workspace_id);
    let _gate = gate.lock().await;
    let ws = d.workspace(&p.workspace_id)?;
    ready(&ws)?;
    let lock = crate::git::repo_lock(&ws.repo_path);
    let _lock = lock.lock().await;
    crate::fs::resolve(&ws.worktree_path, &p.path)?;
    append_ignore(&ws.worktree_path, &pattern)?;
    Ok(IgnoreResult { pattern })
}

fn append_ignore(root: &Path, pattern: &str) -> Result<(), RpcError> {
    let path = root.join(".gitignore");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(RpcError::invalid_params(".gitignore must not be a symlink"));
    }
    #[cfg(unix)]
    let mut file = {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::OpenOptionsExt,
        };
        // Pin the root before opening the output; a swapped root or output
        // symlink cannot redirect the append to another directory.
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .map_err(|e| RpcError::io(&e))?;
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                c".gitignore".as_ptr(),
                libc::O_RDWR
                    | libc::O_APPEND
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o666,
            )
        };
        if fd < 0 {
            return Err(RpcError::io(&std::io::Error::last_os_error()));
        }
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| RpcError::io(&e))?;
    if !file.metadata().map_err(|e| RpcError::io(&e))?.is_file() {
        return Err(RpcError::invalid_params(
            ".gitignore must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file
            .metadata()
            .map_err(|e| RpcError::internal(e.to_string()))?
            .nlink()
            != 1
        {
            return Err(RpcError::invalid_params(
                ".gitignore must not be a hard link",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| RpcError::internal(e.to_string()))?;
    let newline = if bytes.windows(2).any(|s| s == b"\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let existing: Vec<&[u8]> = bytes
        .split(|b| *b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    let lines: Vec<&str> = pattern.split('\n').collect();
    if existing
        .windows(lines.len())
        .any(|window| window.iter().zip(&lines).all(|(a, b)| *a == b.as_bytes()))
    {
        return Ok(());
    }
    let mut suffix = String::new();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        suffix.push_str(newline);
    }
    suffix.push_str(&lines.join(newline));
    suffix.push_str(newline);
    file.write_all(suffix.as_bytes())
        .map_err(|e| RpcError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignore_rules_are_literal_and_anchored() {
        assert_eq!(
            rule("a [x]/*.rs", IgnoreTarget::File).unwrap(),
            "/a \\[x\\]/\\*.rs"
        );
        assert_eq!(
            rule(".", IgnoreTarget::Directory).unwrap(),
            "/*\n!/.gitignore"
        );
        assert_eq!(rule("a/file.rs", IgnoreTarget::FileType).unwrap(), "*.rs");
        assert_eq!(rule("note ", IgnoreTarget::File).unwrap(), "/note\\ ");
        assert!(rule("../escape", IgnoreTarget::File).is_err());
        assert!(rule("a/.git/config", IgnoreTarget::File).is_err());
    }
}
