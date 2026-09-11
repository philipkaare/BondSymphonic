//! `fs.watch`: one `notify` watcher per workspace worktree. Raw events are
//! filtered in the watcher callback, by kind and then by path, coalesced for
//! [`DEBOUNCE`] after the first surviving one, then published as a single
//! `fs.changed` carrying sorted, de-duplicated repo-relative paths.
//!
//! On Linux the watch is built one directory at a time by a walk of our own,
//! rather than by `notify`'s recursive mode, so that the directories in
//! [`IGNORED`] are never watched at all. inotify costs one watch per directory
//! and the kernel caps them per user; a JS worktree's `node_modules` alone runs
//! to tens of thousands of directories, and a recursive watch that merely
//! *silenced* them would still spend the user's whole allowance on them, after
//! which every further `fs.watch` fails. A directory that appears later is
//! watched when its creation is reported. Elsewhere the platform watcher is
//! recursive natively (one handle per tree on Windows), so the tree is watched
//! as a whole and ignored paths are dropped in the callback.

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

/// Whether the tree is watched one directory at a time (inotify) or as a whole
/// (every other backend). See the module documentation.
const PER_DIRECTORY: bool = cfg!(target_os = "linux");

/// What the watcher callback hands the coalescing task.
enum Msg {
    /// Repo-relative paths that changed, already filtered and sorted.
    Changed(Vec<String>),
    /// A directory that appeared inside the tree and, on a per-directory
    /// backend, needs a watch of its own.
    NewDir(PathBuf),
}

/// The daemon's set of live worktree watches, one per workspace: the task that
/// owns each `notify` watcher and coalesces its events. Aborting a task stops
/// the publishing and drops the watcher with it.
#[derive(Default)]
pub struct Watchers {
    active: Mutex<HashMap<WorkspaceId, tokio::task::JoinHandle<()>>>,
}

impl Watchers {
    /// Starts watching `root` recursively for `id`. Idempotent: a second
    /// `enable` for a workspace that is already watched is a no-op, so a
    /// client that re-sends `fs.watch` never ends up with two watchers
    /// publishing the same change twice.
    ///
    /// The watcher is built, and the tree walked, with the lock released:
    /// walking a large worktree takes real time, and every other workspace's
    /// `fs.watch` would otherwise queue behind it.
    pub fn enable(&self, id: WorkspaceId, root: PathBuf, events: EventBus) -> Result<(), RpcError> {
        if self.active.lock().contains_key(&id) {
            return Ok(());
        }
        // Watch the canonical root, so the paths a backend reports strip cleanly
        // whether it echoes back the path we passed (inotify,
        // ReadDirectoryChangesW) or resolves it first (FSEvents). Without this a
        // worktree reached through a symlink would strip nothing and silently drop
        // every event. A root that cannot be canonicalized is watched as given, and
        // `watch` below reports the real problem.
        let root = root.canonicalize().unwrap_or(root);
        let (tx, rx) = mpsc::unbounded_channel::<Msg>();
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
                    if PER_DIRECTORY && may_create_directory(&ev.kind) {
                        for p in &ev.paths {
                            if is_new_directory(&cb_root, p) {
                                let _ = tx.send(Msg::NewDir(p.clone()));
                            }
                        }
                    }
                    let paths = relative_paths(&cb_root, &ev.paths);
                    if !paths.is_empty() {
                        let _ = tx.send(Msg::Changed(paths));
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
        if PER_DIRECTORY {
            // The root first, so an error there is the one reported: a root that
            // cannot be watched has nothing worth walking.
            watcher
                .watch(&root, RecursiveMode::NonRecursive)
                .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
            for dir in walk(&root).dirs.iter().skip(1) {
                watch_quietly(&mut watcher, dir);
            }
        } else {
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
        }
        let task = tokio::spawn(coalesce(id.clone(), root, watcher, rx, events));
        let mut active = self.active.lock();
        if active.contains_key(&id) {
            // A concurrent `enable` for the same workspace got there first while
            // this one was walking; its watch stands and this one goes.
            task.abort();
            return Ok(());
        }
        active.insert(id, task);
        Ok(())
    }

    /// Stops watching `id`, if it is watched. Aborting the task drops an
    /// in-flight debounce window rather than publishing it after the client
    /// asked for silence, and drops the watcher with it.
    ///
    /// The drop is not synchronous with this call. Abort marks the task and
    /// the runtime drops the aborted future - and with it the `notify` watcher
    /// and its inotify registrations - at its next turn. Nothing the daemon
    /// does depends on the difference: the task publishes nothing once
    /// aborted, and a re-`enable` builds a watcher of its own rather than
    /// reusing this one. A test that counts the process's inotify watches
    /// right after a `disable` is the one thing that can see the gap, and it
    /// has to allow for it.
    pub fn disable(&self, id: &WorkspaceId) {
        if let Some(task) = self.active.lock().remove(id) {
            task.abort();
        }
    }
}

/// Adds a watch for one directory, logging rather than failing: a directory
/// that vanished between the walk and the watch, or one the kernel has no
/// allowance left for, costs that directory's events and nothing else.
fn watch_quietly(watcher: &mut RecommendedWatcher, dir: &Path) {
    if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
        tracing::debug!("fs.watch {}: {e}", dir.display());
    }
}

/// What a walk of a directory tree found: the directories, root first and in
/// walk order, and every entry along the way, files and directories alike.
/// Ignored directories are neither entered nor listed.
#[derive(Default)]
struct Walked {
    dirs: Vec<PathBuf>,
    entries: Vec<PathBuf>,
}

/// Walks `root` without following symlinks (a link to a directory outside the
/// worktree would drag that directory in, and a loop would never end).
fn walk(root: &Path) -> Walked {
    let mut out = Walked::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        out.dirs.push(dir);
        for entry in rd.flatten() {
            let path = entry.path();
            let ignored = matches!(entry.file_name().to_str(), Some(n) if IGNORED.contains(&n));
            if ignored {
                continue;
            }
            // `file_type` does not follow symlinks, which is the point.
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                pending.push(path.clone());
            }
            out.entries.push(path);
        }
    }
    out
}

/// Whether an event of this kind can be the appearance of a directory: a
/// creation, or a rename whose destination is inside the tree.
fn may_create_directory(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Any
            | EventKind::Other
    )
}

/// Whether `path` is a directory inside `root` that the watcher should now
/// follow: a real directory (not a link), not ignored, not the root itself.
fn is_new_directory(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() && !is_ignored(rel) => {
            std::fs::symlink_metadata(path).is_ok_and(|md| md.is_dir())
        }
        _ => false,
    }
}

/// Publishes one `fs.changed` per burst: the first event opens a [`DEBOUNCE`]
/// window, everything arriving inside it joins the same batch.
///
/// The deadline is deliberately not reset by later events. A sliding window
/// would starve under continuous writing and never publish at all; this
/// publishes once per [`DEBOUNCE`] for as long as the writing lasts.
///
/// The task owns the watcher, because on a per-directory backend it is also
/// what extends the watch: the callback cannot, since `notify` answers a
/// `watch` call from the same thread that runs the callback.
async fn coalesce(
    id: WorkspaceId,
    root: PathBuf,
    mut watcher: RecommendedWatcher,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    events: EventBus,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::new();
        absorb(&root, &mut watcher, first, &mut batch).await;
        let deadline = tokio::time::sleep(DEBOUNCE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                more = rx.recv() => match more {
                    Some(msg) => absorb(&root, &mut watcher, msg, &mut batch).await,
                    // The watcher is gone: publish what we have and stop.
                    None => break,
                },
            }
        }
        // Each batch arrives sorted and de-duplicated on its own, but a burst
        // merges several, so the union has to be sorted again. Never empty in
        // practice: the callback only sends what already survived the filter,
        // and a new directory arrives alongside its own creation.
        batch.sort();
        batch.dedup();
        if !batch.is_empty() {
            events.publish(Some(id.clone()), Event::FsChanged { paths: batch });
        }
    }
}

/// Folds one message into the current batch. A new directory is walked and
/// watched, and whatever the walk found is reported too: anything written
/// into the directory before its watch was in place would otherwise have
/// gone unheard.
async fn absorb(root: &Path, watcher: &mut RecommendedWatcher, msg: Msg, batch: &mut Vec<String>) {
    match msg {
        Msg::Changed(paths) => batch.extend(paths),
        Msg::NewDir(dir) => {
            let walked = tokio::task::spawn_blocking(move || walk(&dir))
                .await
                .unwrap_or_default();
            for d in &walked.dirs {
                watch_quietly(watcher, d);
            }
            batch.extend(relative_paths(root, &walked.entries));
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

    /// The walk never enters an ignored directory and never follows a link, so
    /// neither can cost a watch.
    #[test]
    fn walk_skips_ignored_directories_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/a/b")).unwrap();
        std::fs::create_dir_all(root.join("crates/x/target/debug")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("src"), root.join("link")).unwrap();

        let walked = walk(root);
        let dirs: Vec<_> = walked
            .dirs
            .iter()
            .map(|d| {
                d.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(dirs[0], "", "the root comes first");
        for d in ["src", "src/deep", "crates", "crates/x"] {
            assert!(dirs.contains(&d.to_string()), "{d} missing from {dirs:?}");
        }
        assert!(
            !dirs
                .iter()
                .any(|d| d.contains("node_modules") || d.contains("target")),
            "ignored directories were walked: {dirs:?}"
        );
        assert!(
            !dirs.contains(&"link".to_string()),
            "a symlink was followed"
        );
        let entries: Vec<_> = walked
            .entries
            .iter()
            .map(|d| {
                d.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(entries.contains(&"src/main.rs".to_string()));
        assert!(!entries.iter().any(|e| e.starts_with("node_modules")));
    }
}
