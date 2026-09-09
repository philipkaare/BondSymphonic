//! `fs.watch`: one `notify` watcher per workspace worktree. Raw events are
//! filtered in the watcher callback, by kind and then by path, coalesced for
//! [`DEBOUNCE`] after the first surviving one, then published as a single
//! `fs.changed` carrying sorted, de-duplicated repo-relative paths.

use crate::server::broadcast::EventBus;
use bondsymphonic_proto::{Event, RpcError, WorkspaceId};
use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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

/// Directory names whose churn is never interesting to a client: git's own
/// bookkeeping and the two build-output directories that would otherwise drown
/// every other change.
///
/// Matched at any depth, not only at the worktree root, because the layouts
/// this exists to survive nest them: a JS monorepo has a `node_modules` per
/// package, a cargo workspace can have a `target` per crate, and a vendored
/// dependency carries its own `.git`. The cost is that a source directory
/// genuinely named `target` is invisible to the watcher, which is the right
/// trade at these three names.
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
        // Watch the canonical root, so the paths a backend reports strip cleanly
        // whether it echoes back the path we passed (inotify,
        // ReadDirectoryChangesW) or resolves it first (FSEvents). Without this a
        // worktree reached through a symlink would strip nothing and silently drop
        // every event. A root that cannot be canonicalized is watched as given, and
        // `watch` below reports the real problem.
        let root = root.canonicalize().unwrap_or(root);
        let (tx, rx) = mpsc::unbounded_channel::<Vec<String>>();
        // The callback runs on notify's own thread, so it does the cheap part and
        // hands the result to the coalescing task. Filtering here rather than at
        // publish time is what keeps a `cargo build` inside the worktree from waking
        // that task several times a second only to discard every `target/**` path it
        // was just handed: an event that filters down to nothing sends nothing.
        let cb_root = root.clone();
        let cb_id = id.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            match res {
                Ok(ev) if is_change(&ev.kind) => {
                    let paths = relative_paths(&cb_root, &ev.paths);
                    if !paths.is_empty() {
                        let _ = tx.send(paths);
                    }
                }
                // Something only read the worktree. See [`is_change`].
                Ok(_) => {}
                // Most consequentially an inotify queue overflow, which means events
                // were lost and the client's view is now stale. There is no resync
                // protocol yet, so the log is the only signal there is.
                Err(e) => tracing::warn!(ws = %cb_id, "fs.watch: watcher error: {e}"),
            }
        })
        .map_err(|e| RpcError::internal(format!("fs.watch: {e}")))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
        let task = tokio::spawn(coalesce(id.clone(), rx, events));
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

/// Publishes one `fs.changed` per burst: the first event opens a [`DEBOUNCE`]
/// window, everything arriving inside it joins the same batch.
///
/// The deadline is deliberately not reset by later events. A sliding window
/// would starve under continuous writing and never publish at all; this
/// publishes once per [`DEBOUNCE`] for as long as the writing lasts.
async fn coalesce(id: WorkspaceId, mut rx: mpsc::UnboundedReceiver<Vec<String>>, events: EventBus) {
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
        // Each batch arrives sorted and de-duplicated on its own, but a burst
        // merges several, so the union has to be sorted again. Never empty: the
        // callback only sends what already survived the filter.
        batch.sort();
        batch.dedup();
        events.publish(Some(id.clone()), Event::FsChanged { paths: batch });
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
        .filter(|rel| !is_ignored(rel))
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        // The watched root itself strips to the empty path; it is not a change.
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether an event kind reports that something *changed*, as opposed to
/// something merely having read the worktree.
///
/// This is load-bearing, not a nicety. inotify reports reads as `Access`
/// events, and a client that reacts to `fs.changed` by asking for
/// `workspace.changes` makes the daemon run git, git reads every file in the
/// worktree, and those reads publish another `fs.changed`. The loop sustains
/// itself at roughly 1.7 Hz per open workspace with nobody touching anything: a
/// 10 s probe on an idle worktree saw 691 events, every one of them
/// `OPEN` / `ACCESS` / `CLOSE_NOWRITE`, and not a single write.
///
/// Kept, deliberately:
///
/// - `Modify(Metadata(_))` other than access time, because `git status` reports
///   a mode change and the client needs to hear about a `chmod`.
/// - `Access(Close(Write))`, the one access that follows a write rather than a
///   read. A writer using `mmap` may produce no `Modify` at all, and the close
///   is then the only notification there is. It cannot re-open the loop: a
///   reader closes with `Close(Read)`, which is what the probe above saw.
/// - `Any` and `Other`. An unrecognised kind from an unfamiliar backend fails
///   open, costing a spurious refresh, rather than failing closed and silently
///   disabling the watch.
fn is_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(access) => matches!(access, AccessKind::Close(AccessMode::Write)),
        // Definitionally a read, and it would sustain exactly the same loop on
        // any backend that reports it.
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => false,
        _ => true,
    }
}

/// Whether a worktree-relative path is one no client wants to hear about: it
/// runs through an [`IGNORED`] directory at any depth, or it is one of
/// `fs::write_file`'s `<name>.<pid>.<counter>.bs-tmp` temporaries, which is
/// renamed away before any client could open it.
fn is_ignored(rel: &Path) -> bool {
    rel.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some(s) if IGNORED.contains(&s)))
        || rel
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".bs-tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};
    use std::path::PathBuf;

    #[test]
    fn reads_are_not_changes_but_writes_creations_and_removals_are() {
        // What `git status` does to every file in a worktree, and what made an
        // idle workspace refresh itself twice a second.
        assert!(!is_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_change(&EventKind::Access(AccessKind::Any)));
        assert!(!is_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));

        // The one access that follows a write rather than a read: for an `mmap`
        // writer it can be the only notification there is.
        assert!(is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));

        assert!(is_change(&EventKind::Create(CreateKind::File)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(is_change(&EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
        // git reports a mode change, so the client has to hear about a chmod.
        assert!(is_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Permissions
        ))));
        // What ReadDirectoryChangesW reports for every content change.
        assert!(is_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(&EventKind::Remove(RemoveKind::File)));

        // Unknown kinds fail open rather than silencing the watch.
        assert!(is_change(&EventKind::Any));
        assert!(is_change(&EventKind::Other));
    }

    #[test]
    fn relative_paths_filter_ignored_dirs_at_any_depth_temp_files_and_dedup() {
        let root = PathBuf::from("/wt");
        let input = vec![
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/src/main.rs"),
            PathBuf::from("/wt/.git/index"),
            PathBuf::from("/wt/node_modules/x/y.js"),
            PathBuf::from("/wt/target/debug/a"),
            // Ignored names are ignored wherever they appear, not just at the
            // worktree root: a JS monorepo nests `node_modules` per package and
            // a cargo workspace nests `target` per crate.
            PathBuf::from("/wt/packages/web/node_modules/x.js"),
            PathBuf::from("/wt/crates/ide/target/debug/a"),
            PathBuf::from("/wt/vendor/dep/.git/HEAD"),
            PathBuf::from("/wt/notes.txt.123.7.bs-tmp"),
            PathBuf::from("/wt/src/notes.txt.123.7.bs-tmp"),
            PathBuf::from("/elsewhere/z"),
            PathBuf::from("/wt/README.md"),
        ];
        assert_eq!(
            relative_paths(&root, &input),
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );
    }
}
