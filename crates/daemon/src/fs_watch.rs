//! `fs.watch`: one `notify` watcher per workspace worktree. Raw events are
//! coalesced for [`DEBOUNCE`] after the first one, converted to sorted,
//! de-duplicated repo-relative paths, and published as a single `fs.changed`.

use crate::server::broadcast::EventBus;
use bondsymphonic_proto::{Event, RpcError, WorkspaceId};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a burst of raw filesystem events is collected before one
/// `fs.changed` is published. Long enough that a save (write temp file,
/// rename, touch the directory) is a single event, short enough that the
/// editor still feels live.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Top-level worktree directories whose churn is never interesting to a client:
/// git's own bookkeeping and the two build-output directories that would
/// otherwise drown every other change.
const IGNORED: [&str; 3] = [".git", "node_modules", "target"];

/// A live watch: the `notify` watcher (dropped to unregister) and the task
/// that coalesces its events (aborted to stop publishing).
struct Active {
    _watcher: RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

/// The daemon's set of live worktree watches, one per workspace.
#[derive(Default)]
pub struct Watchers {
    active: Mutex<HashMap<WorkspaceId, Active>>,
}

impl Watchers {
    /// Starts watching `root` recursively for `id`. Idempotent: a second
    /// `enable` for a workspace that is already watched is a no-op, so a
    /// client that re-sends `fs.watch` never ends up with two watchers
    /// publishing the same change twice.
    pub fn enable(&self, id: WorkspaceId, root: PathBuf, events: EventBus) -> Result<(), RpcError> {
        let mut active = self.active.lock();
        if active.contains_key(&id) {
            return Ok(());
        }
        let (tx, rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
        // The callback runs on notify's own thread, so it only hands paths to the
        // coalescing task; everything else happens on the runtime.
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = tx.send(ev.paths);
            }
        })
        .map_err(|e| RpcError::internal(format!("fs.watch: {e}")))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
        let task = tokio::spawn(coalesce(id.clone(), root, rx, events));
        active.insert(
            id,
            Active {
                _watcher: watcher,
                task,
            },
        );
        Ok(())
    }

    /// Stops watching `id`, if it is watched. Aborting the task first means an
    /// in-flight debounce window is dropped rather than published after the
    /// client asked for silence; dropping `Active` then drops the watcher.
    pub fn disable(&self, id: &WorkspaceId) {
        if let Some(a) = self.active.lock().remove(id) {
            a.task.abort();
        }
    }
}

/// Publishes one `fs.changed` per burst: the first raw event opens a
/// [`DEBOUNCE`] window, everything arriving inside it joins the same batch.
async fn coalesce(
    id: WorkspaceId,
    root: PathBuf,
    mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    events: EventBus,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = first;
        let deadline = tokio::time::sleep(DEBOUNCE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                more = rx.recv() => match more {
                    Some(paths) => batch.extend(paths),
                    // The watcher is gone: publish what we have and stop.
                    None => break,
                },
            }
        }
        let paths = relative_paths(&root, &batch);
        if !paths.is_empty() {
            events.publish(Some(id.clone()), Event::FsChanged { paths });
        }
    }
}

/// Repo-relative, forward-slash paths under `root`, minus ignored directories
/// and the daemon's own `.bs-tmp` write temporaries; sorted and de-duplicated.
///
/// Dropping the temp files matters on Windows, where `fs.write_file`'s
/// temp-file-plus-rename surfaces as a write to `<name>.<pid>.<n>.bs-tmp`
/// followed by a rename to the real path: without the filter a single save
/// would report a file the client can never open.
pub fn relative_paths(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        // Paths outside the worktree (notify can report the watched root's own
        // parent on some backends) simply drop out here.
        .filter_map(|p| p.strip_prefix(root).ok())
        .filter(|rel| {
            let first = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .unwrap_or("");
            !IGNORED.contains(&first)
        })
        .filter(|rel| !rel.to_string_lossy().ends_with(".bs-tmp"))
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn relative_paths_filter_ignored_dirs_temp_files_and_dedup() {
        let root = PathBuf::from("/wt");
        let input = vec![
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/.git/index"),
            PathBuf::from("/wt/node_modules/x/y.js"),
            PathBuf::from("/wt/target/debug/a"),
            PathBuf::from("/wt/notes.txt.123.7.bs-tmp"),
            PathBuf::from("/elsewhere/z"),
            PathBuf::from("/wt/README.md"),
        ];
        assert_eq!(
            relative_paths(&root, &input),
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );
    }
}
