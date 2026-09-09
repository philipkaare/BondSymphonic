//! `workspace.changes` and `workspace.diff`: what a workspace changed relative
//! to the merge-base of its base branch, computed with git and read through the
//! pinned `worktree_git` so an agent-writable worktree cannot steer git.

use crate::daemon::Daemon;
use crate::git::Git;
use crate::workspace::lifecycle::layout_for;
use crate::workspace::Workspace;
use bondsymphonic_proto::{
    ChangedFile, ChangesResult, DiffResult, FileStatus, RpcError, WorkspaceId,
};
use std::collections::HashMap;

/// The commit a workspace is measured against: where its branch left the base.
async fn merge_base(git: &Git, ws: &Workspace) -> Result<String, RpcError> {
    let out = git
        .run(&ws.worktree_path, &["merge-base", &ws.base_branch, "HEAD"])
        .await?;
    Ok(out.stdout.trim().to_string())
}

/// Line endings belong to the checkout, not to the change: git stores blobs with
/// LF and hands the worktree whatever `core.eol` and `core.autocrlf` say, so
/// comparing the two sides raw would report every line of an untouched file as
/// changed. Both sides are normalised to LF before they go out.
fn normalise_eol(s: String) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n")
    } else {
        s
    }
}

/// Changed files vs the merge-base, committed and uncommitted alike, one entry
/// per path, sorted by path.
pub async fn changes(d: &Daemon, id: &WorkspaceId) -> Result<ChangesResult, RpcError> {
    let ws = d.workspace(id)?;
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    let base = merge_base(&git, &ws).await?;

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
    let resolved = crate::fs::resolve(&ws.worktree_path, path)?;
    if resolved.is_dir() {
        // `git show <sha>:<dir>` answers with a tree listing, which is not a
        // side of a diff and would reach the editor looking like file content.
        return Err(RpcError::invalid_params("path is a directory"));
    }
    let layout = layout_for(d, &ws).await?;
    let git = layout.worktree_git();
    let cwd = ws.worktree_path.clone();
    // Git speaks `/` in every pathspec, on every platform. `resolve` accepts the
    // separator the client's platform uses, so on Windows it has to be converted
    // rather than passed through; on unix `\` is an ordinary filename character
    // and converting it would ask git for a path that does not exist.
    let spec_path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    let spec = format!("{}:{}", merge_base(&git, &ws).await?, spec_path);
    // "Does this path exist at the merge-base" asked as its own question, before
    // anything is read. A missing path is a normal "added" file and its base side
    // is empty; every other failure — a lock conflict, a damaged object store, the
    // 60 s timeout — has to stay an error, because an empty base side is
    // indistinguishable from a new file and would report every line as added.
    // Reading the answer off the *read* would conflate the two, which is what this
    // used to do.
    let base_exists = match git.run(&cwd, &["cat-file", "-e", &spec]).await {
        Ok(_) => true,
        Err(e) if absent_at_rev(&e) => false,
        Err(e) => return Err(e),
    };
    // Read as bytes and classified like any other file the daemon serves: a
    // binary blob is empty rather than mojibake, and an oversized one is cut at
    // the same cap `fs::read_file` uses instead of crossing the wire whole.
    let (base_text, base_cut) = if base_exists {
        let out = git
            .run_bytes(&cwd, &["show", &spec], crate::fs::MAX_READ)
            .await?;
        let r = crate::fs::classify(&out.stdout);
        let text = if r.encoding == "binary" {
            String::new()
        } else {
            r.content
        };
        (text, r.truncated)
    } else {
        (String::new(), false)
    };
    let root = cwd.clone();
    let rel = path.to_owned();
    let (work_text, work_cut) =
        tokio::task::spawn_blocking(move || match crate::fs::read_file(&root, &rel) {
            Ok(r) if r.encoding != "binary" => (r.content, r.truncated),
            _ => (String::new(), false),
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(DiffResult {
        base_text: normalise_eol(base_text),
        work_text: normalise_eol(work_text),
        truncated: base_cut || work_cut,
    })
}

/// Whether a failed `git cat-file -e <rev>:<path>` means the path is simply not
/// in that revision.
///
/// The exit code cannot answer this: git reports an absent path as `fatal:`,
/// exit 128, the same code it uses for a malformed revision or an unreadable
/// object store. The two messages below are the ones it prints for absence, and
/// `LC_ALL=C` on every invocation is what makes matching them safe.
fn absent_at_rev(e: &RpcError) -> bool {
    let stderr = e
        .data
        .as_ref()
        .and_then(|d| d.get("stderr"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    // "does not exist in '<rev>'" for a path git has never seen; "exists on
    // disk, but not in '<rev>'" for one that is only in the working tree.
    stderr.contains("does not exist in") || stderr.contains("exists on disk, but not in")
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
    // The two sources are disjoint by construction — git does not report a path as
    // both tracked-changed and untracked — so this only ever fires if a third
    // source is added later, and it *drops* rather than merges. Anything that adds
    // one has to decide how the counts combine here.
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

    /// The two messages git prints for "not in that revision", verbatim from
    /// `git cat-file -e` under `LC_ALL=C`.
    #[test]
    fn absent_at_rev_recognises_gits_two_absence_messages() {
        let never_seen = crate::git::git_error(
            "git cat-file -e abc123:gone.txt",
            Some(128),
            "fatal: path 'gone.txt' does not exist in 'abc123'",
        );
        assert!(absent_at_rev(&never_seen));

        let untracked = crate::git::git_error(
            "git cat-file -e abc123:new.txt",
            Some(128),
            "fatal: path 'new.txt' exists on disk, but not in 'abc123'",
        );
        assert!(absent_at_rev(&untracked));
    }

    /// Everything else is a real failure and must not be read as "the file is
    /// new" — that is the bug this predicate exists to prevent.
    #[test]
    fn absent_at_rev_rejects_real_failures() {
        let timeout =
            crate::git::git_error("git cat-file -e abc123:a.rs", None, "timed out after 60s");
        assert!(!absent_at_rev(&timeout));

        let broken = crate::git::git_error(
            "git cat-file -e abc123:a.rs",
            Some(128),
            "fatal: loose object 0f2c is corrupt",
        );
        assert!(!absent_at_rev(&broken));

        let locked = crate::git::git_error(
            "git cat-file -e abc123:a.rs",
            Some(128),
            "fatal: Unable to create '.git/index.lock': File exists.",
        );
        assert!(!absent_at_rev(&locked));

        // No data at all: an error whose shape is unknown is not absence.
        assert!(!absent_at_rev(&RpcError::internal("no data")));
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
