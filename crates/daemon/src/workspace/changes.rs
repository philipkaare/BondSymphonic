//! `workspace.changes` and `workspace.diff`: what a workspace changed relative
//! to the merge-base of its base branch, computed with git and read through the
//! pinned `worktree_git` so an agent-writable worktree cannot steer git.

use crate::daemon::Daemon;
use crate::workspace::lifecycle::layout_for;
use bondsymphonic_proto::{
    ChangedFile, ChangesResult, DiffResult, FileStatus, RpcError, WorkspaceId,
};
use std::collections::HashMap;

/// Changed files vs the merge-base, committed and uncommitted alike, one entry
/// per path, sorted by path.
pub async fn changes(d: &Daemon, id: &WorkspaceId) -> Result<ChangesResult, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    let base = git
        .run(&cwd, &["merge-base", &ws.base_branch, "HEAD"])
        .await?;
    let base = base.stdout.trim().to_string();

    let name_status = git
        .run(&cwd, &["diff", "--name-status", "-M", "-z", &base, "--"])
        .await?;
    let numstat = git
        .run(&cwd, &["diff", "--numstat", "-M", "-z", &base, "--"])
        .await?;
    let status = git
        .run(
            &cwd,
            &["status", "--porcelain=v2", "--untracked-files=all", "-z"],
        )
        .await?;

    let tracked = parse_name_status_z(&name_status.stdout);
    let counts = parse_numstat_z(&numstat.stdout);
    let untracked_paths: Vec<String> = status
        .stdout
        .split('\0')
        .filter_map(|line| line.strip_prefix("? ").map(str::to_owned))
        .collect();
    let root = cwd.clone();
    // One blocking task for the whole batch: reading N untracked files is N
    // syscalls, and the pool has no reason to see them as N separate jobs.
    let untracked = tokio::task::spawn_blocking(move || {
        untracked_paths
            .into_iter()
            .map(|p| {
                let lines = crate::fs::read_file(&root, &p)
                    .map(|r| {
                        if r.encoding == "binary" {
                            0
                        } else {
                            r.content.lines().count() as u32
                        }
                    })
                    .unwrap_or(0);
                (p, lines)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;

    Ok(ChangesResult {
        files: merge_changes(tracked, &counts, untracked),
    })
}

/// Base text (merge-base version, empty if the path did not exist there) and
/// working-tree text (empty if deleted or binary) for one repo-relative path.
pub async fn diff(d: &Daemon, id: &WorkspaceId, path: &str) -> Result<DiffResult, RpcError> {
    let ws = d.workspace(id)?;
    // Containment first: git would happily show any path in the object store.
    crate::fs::resolve(&ws.worktree_path, path)?;
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    let base = git
        .run(&cwd, &["merge-base", &ws.base_branch, "HEAD"])
        .await?;
    let spec = format!("{}:{}", base.stdout.trim(), path);
    // A missing path at the merge-base is a normal "added" file, not an error.
    let base_text = match git.run(&cwd, &["show", &spec]).await {
        Ok(out) => out.stdout,
        Err(_) => String::new(),
    };
    let root = cwd.clone();
    let rel = path.to_owned();
    let work_text = tokio::task::spawn_blocking(move || match crate::fs::read_file(&root, &rel) {
        Ok(r) if r.encoding != "binary" => r.content,
        _ => String::new(),
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(DiffResult {
        base_text,
        work_text,
    })
}

/// `git diff --name-status -M -z` → (status, path). Renames yield the new path.
pub fn parse_name_status_z(text: &str) -> Vec<(FileStatus, String)> {
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let Some(code) = fields.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Added,
            _ => FileStatus::Modified,
        };
        let Some(first) = fields.next() else { break };
        // `R`/`C` records carry two paths (old, new); everything else carries one.
        let path = if code.starts_with('R') || code.starts_with('C') {
            match fields.next() {
                Some(new) => new.to_owned(),
                None => break,
            }
        } else {
            first.to_owned()
        };
        out.push((status, path));
    }
    out
}

/// `git diff --numstat -M -z` → path → (additions, deletions). Binary files
/// count as (0, 0); renames are keyed by the new path.
pub fn parse_numstat_z(text: &str) -> HashMap<String, (u32, u32)> {
    let mut map = HashMap::new();
    let mut fields = text.split('\0');
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut cols = record.splitn(3, '\t');
        // Binary files are reported as "-\t-", which parses to (0, 0).
        let add = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let del = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let path = match cols.next() {
            Some("") | None => {
                // Rename: the path columns follow as two extra NUL-separated fields.
                let _old = fields.next();
                match fields.next() {
                    Some(new) => new.to_owned(),
                    None => break,
                }
            }
            Some(p) => p.to_owned(),
        };
        map.insert(path, (add, del));
    }
    map
}

/// Tracked entries (counted from `numstat`) plus untracked ones (whole file is
/// an addition), one entry per path, sorted by path.
pub fn merge_changes(
    tracked: Vec<(FileStatus, String)>,
    counts: &HashMap<String, (u32, u32)>,
    untracked: Vec<(String, u32)>,
) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = tracked
        .into_iter()
        .map(|(status, path)| {
            let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
            ChangedFile {
                path,
                status,
                additions,
                deletions,
            }
        })
        .collect();
    files.extend(untracked.into_iter().map(|(path, lines)| ChangedFile {
        path,
        status: FileStatus::Untracked,
        additions: lines,
        deletions: 0,
    }));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_status_z_including_renames() {
        // `git diff --name-status -M -z`: status\0path\0, renames R<score>\0old\0new\0
        let text = "M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0R100\0old.rs\0renamed.rs\0";
        let v = parse_name_status_z(text);
        assert_eq!(
            v,
            vec![
                (FileStatus::Modified, "src/a.rs".into()),
                (FileStatus::Added, "new.txt".into()),
                (FileStatus::Deleted, "gone.txt".into()),
                (FileStatus::Renamed, "renamed.rs".into()),
            ]
        );
    }

    #[test]
    fn parses_numstat_z_and_treats_binary_as_zero() {
        // `git diff --numstat -z`: add\tdel\tpath\0 ; binary is "-\t-"; renames add\tdel\t\0old\0new\0
        let text = "3\t1\tsrc/a.rs\0-\t-\timg.png\0";
        let m = parse_numstat_z(text);
        assert_eq!(m["src/a.rs"], (3, 1));
        assert_eq!(m["img.png"], (0, 0));
    }

    #[test]
    fn parses_numstat_z_rename_entry() {
        let text = "2\t0\t\0old.rs\0renamed.rs\0";
        let m = parse_numstat_z(text);
        assert_eq!(m["renamed.rs"], (2, 0));
        assert!(!m.contains_key("old.rs"));
    }

    #[test]
    fn merges_tracked_and_untracked_sorted_by_path() {
        let tracked = vec![(FileStatus::Modified, "b.rs".to_string())];
        let mut counts = HashMap::new();
        counts.insert("b.rs".to_string(), (4, 2));
        let untracked = vec![("a.txt".to_string(), 7)];
        let files = merge_changes(tracked, &counts, untracked);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].status, FileStatus::Untracked);
        assert_eq!((files[0].additions, files[0].deletions), (7, 0));
        assert_eq!(files[1].path, "b.rs");
        assert_eq!((files[1].additions, files[1].deletions), (4, 2));
    }
}
