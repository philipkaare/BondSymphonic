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
//!
//! Registration - canonicalizing the root, classifying its filesystem, the
//! walk and the watch per directory - runs on a blocking thread, and `fs.watch`
//! is answered as soon as it is scheduled rather than when it is done. It used
//! to run on the runtime worker that handled the request. On an in-place
//! workspace of 77,274 directories on a 9P mount (`/mnt/c` under WSL2, where
//! every `stat` and every watch is a round trip to Windows) that held the
//! worker for minutes: the IDE re-sent `fs.watch` at each 30 s timeout, each
//! retry took another worker for another walk of the same tree, and every
//! request on the connection - `agent.send` to a healthy agent, `agent.start`,
//! `system.check_prereqs` - timed out for four minutes while the connection
//! stayed up. A second `fs.watch` for a workspace whose registration is under
//! way is now a no-op, like one for a workspace already watched.
//!
//! A root on 9P gets its root directory watched and nothing else. WSL2's drvfs
//! has no fsnotify support, so a watch there has never delivered an event: the
//! walk was pure cost, and the IDE's Refresh is how such a tree is kept
//! current. The root watch stays because it is cheap and harmless, and the
//! entry then looks like every other one.

use crate::server::broadcast::EventBus;
use bondsymphonic_proto::{Event, RpcError, WorkspaceId};
use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

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

/// The `f_type` that `statfs` reports for a 9P mount: `V9FS_MAGIC` in the
/// kernel's `magic.h`. WSL2 mounts every Windows drive this way (`/mnt/c` is
/// `9p` with `aname=drvfs`), and drvfs has no fsnotify support, so an inotify
/// watch on it never fires. Read from the kernel rather than inferred from a
/// `/mnt/<letter>/` prefix, because the prefix is only a WSL convention: on a
/// plain Linux box `/mnt/c` can be an ext4 disk whose watches work.
const V9FS_MAGIC: i64 = 0x0102_1997;

/// What the watcher callback hands the coalescing task.
enum Msg {
    /// Repo-relative paths that changed, already filtered and sorted.
    Changed(Vec<String>),
    /// A directory that appeared inside the tree and, on a per-directory
    /// backend, needs a watch of its own.
    NewDir(PathBuf),
}

/// One workspace's watch: the task that registers it and then coalesces its
/// events, and where in that life it is. Aborting the task stops whichever
/// half it is in, and drops the watcher with it.
struct Watch {
    task: JoinHandle<()>,
    state: State,
}

/// Where a workspace's watch is in its life. The entry exists from the moment
/// `enable` accepts the request, so that a second `enable` finds it whichever
/// state it is in and starts nothing.
///
/// ```text
/// enable ──▶ Registering ──(watcher built)──▶ Live
///                │  │                          │
///           (failed) └─── disable ─────────────┴─── disable ──▶ gone
///                │
///              gone
/// ```
enum State {
    /// [`register`] is canonicalizing, classifying, walking and watching the
    /// root on a blocking thread. `disable` aborts the task; the blocking step
    /// cannot be interrupted, but what it produces - the watcher - is dropped
    /// the moment it lands, so nothing stays watched. A registration that
    /// fails removes its own entry, and a later `enable` starts afresh.
    Registering {
        /// Which registration this is, so a task that reaches the map after
        /// a `disable` and a fresh `enable` for the same workspace recognises
        /// that the entry is no longer its own.
        generation: u64,
        /// Closes when the registration has settled; see
        /// [`Watchers::registered`].
        settled: watch::Receiver<()>,
    },
    /// The watcher is in place and the task is coalescing its events.
    Live,
}

/// The map of live and registering watches, shared with each registration
/// task so it can flip or remove its own entry when its blocking step lands.
type Active = Arc<Mutex<HashMap<WorkspaceId, Watch>>>;

/// The daemon's set of worktree watches, one per workspace.
#[derive(Default)]
pub struct Watchers {
    active: Active,
    /// Numbers registrations, so that a late one can tell its own entry from
    /// a successor's. See [`State::Registering`].
    generations: AtomicU64,
}

impl Watchers {
    /// Asks for `root` to be watched for `id`, and returns once the request is
    /// scheduled. The registration itself runs on a blocking thread - see the
    /// module documentation for the four minutes that taught us to - and this
    /// never waits for it. Idempotent: a second `enable` for a workspace that
    /// is watched, or whose registration is still under way, is a no-op, so a
    /// client that re-sends `fs.watch` never ends up with two watchers
    /// publishing the same change twice, nor with two walks of the same tree.
    ///
    /// A registration that fails is logged, not returned: by the time it is
    /// known the reply has gone. The IDE does nothing with the reply but log a
    /// failure, and the log is where one now appears.
    pub fn enable(&self, id: WorkspaceId, root: PathBuf, events: EventBus) {
        let mut active = self.active.lock();
        if active.contains_key(&id) {
            return;
        }
        // The entry goes in under the same lock that found it absent, before the
        // task has run a single instruction: the guard against a second walk has
        // to be in place the moment this returns, because the retry that used to
        // start the second walk arrives while the first is still going.
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let (settled_tx, settled) = watch::channel(());
        let task = tokio::spawn(register(
            Arc::clone(&self.active),
            id.clone(),
            root,
            events,
            generation,
            settled_tx,
        ));
        let state = State::Registering {
            generation,
            settled,
        };
        active.insert(id, Watch { task, state });
    }

    /// Resolves once `id`'s registration has settled: the watch is live, it
    /// failed, or it was disabled and the watcher it had built, if any, is
    /// gone. Resolves at once for a workspace that is not registering. The
    /// future follows the registration under way when it was made, so a caller
    /// that wants to see a registration through a `disable` obtains it first.
    ///
    /// `enable` answers before any of that has happened. This is for whatever
    /// needs the watch in place rather than merely requested, which today is
    /// the tests.
    pub fn registered(&self, id: &WorkspaceId) -> impl Future<Output = ()> + Send + 'static {
        let settled = match self.active.lock().get(id) {
            Some(Watch {
                state: State::Registering { settled, .. },
                ..
            }) => Some(settled.clone()),
            _ => None,
        };
        async move {
            if let Some(mut settled) = settled {
                // Nothing ever sends on the channel; the one outcome is the sender
                // being dropped, which `changed` reports as an error.
                let _ = settled.changed().await;
            }
        }
    }

    /// Stops watching `id`, if it is watched or registering. Aborting the
    /// task drops an in-flight debounce window rather than publishing it after
    /// the client asked for silence, and drops the watcher with it; a
    /// registration still on its blocking thread runs to the end and its
    /// watcher is dropped unused when it lands.
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
        if let Some(watch) = self.active.lock().remove(id) {
            watch.task.abort();
        }
    }
}

/// What the blocking step of a registration hands the coalescing loop.
struct Built {
    /// The canonical root every reported path is stripped against.
    root: PathBuf,
    watcher: RecommendedWatcher,
    rx: mpsc::UnboundedReceiver<Msg>,
}

/// One registration from start to finish: the blocking step, the flip of the
/// map entry to [`State::Live`], then the coalescing loop for as long as the
/// watch lasts. It is the task a [`Watch`] holds, so aborting the handle
/// stops it at whichever point it has reached.
async fn register(
    active: Active,
    id: WorkspaceId,
    root: PathBuf,
    events: EventBus,
    generation: u64,
    settled: watch::Sender<()>,
) {
    let built = {
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            counters::BUILDS.fetch_add(1, Ordering::SeqCst);
            #[cfg(test)]
            std::thread::sleep(Duration::from_millis(
                counters::HOLD_MS.load(Ordering::SeqCst),
            ));
            // Watch the canonical root, so the paths a backend reports strip cleanly
            // whether it echoes back the path we passed (inotify,
            // ReadDirectoryChangesW) or resolves it first (FSEvents). Without this a
            // worktree reached through a symlink would strip nothing and silently
            // drop every event. A root that cannot be canonicalized is watched as
            // given, and `build` reports the real problem.
            let root = root.canonicalize().unwrap_or(root);
            let coverage = coverage(&root);
            // The sender rides along with the result. When this task has been
            // aborted meanwhile, tokio drops the pair unread the moment the closure
            // returns, watcher first and sender after it, so `registered` resolves
            // only once nothing is left watching.
            (build(&id, root, coverage), settled)
        })
        .await
    };
    let (built, settled) = match built {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(ws = %id, "fs.watch: registration panicked: {e}");
            forget(&active, &id, generation);
            return;
        }
    };
    let built = match built {
        Ok(built) => built,
        Err(e) => {
            tracing::warn!(ws = %id, "fs.watch: registration failed: {}", e.message);
            forget(&active, &id, generation);
            return;
        }
    };
    {
        let mut active = active.lock();
        match active.get_mut(&id) {
            Some(w) if matches!(w.state, State::Registering { generation: g, .. } if g == generation) =>
            {
                w.state = State::Live;
            }
            // Not this registration's entry any more. A `disable` aborts the task,
            // but an abort only lands at the next yield, and there is none between
            // the blocking step returning and this lock: a `disable` and a fresh
            // `enable` can both fit in that gap. Returning drops the watcher.
            _ => return,
        }
    }
    drop(settled);
    coalesce(id, built.root, built.watcher, built.rx, events).await;
}

/// Removes `id`'s entry, if it is still the one registration `generation`
/// made: a failed registration must not take a successor's entry with it.
fn forget(active: &Active, id: &WorkspaceId, generation: u64) {
    let mut active = active.lock();
    let ours = active.get(id).is_some_and(
        |w| matches!(w.state, State::Registering { generation: g, .. } if g == generation),
    );
    if ours {
        active.remove(id);
    }
}

/// How much of a root the watcher covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    /// Every directory under the root but the [`IGNORED`] ones: what a
    /// filesystem that reports changes deserves.
    Tree,
    /// The root directory alone, because the filesystem reports no changes and
    /// a watch per directory would cost the walk and buy nothing. Nothing is
    /// heard through the root watch either; see the module documentation.
    RootOnly,
}

/// Whether inotify hears nothing from a filesystem with this `statfs` magic.
/// A pure function of the magic, so the rule is testable without a 9P mount to
/// hand.
fn delivers_no_events(magic: i64) -> bool {
    magic == V9FS_MAGIC
}

/// The filesystem magic under `path`, on the one platform where it decides
/// anything: elsewhere the watcher is recursive natively and has no walk to
/// spare, and the answer is "unknown".
#[cfg(target_os = "linux")]
#[allow(
    clippy::useless_conversion,
    reason = "`f_type` is `__fsword_t`: `i64` on x86_64-gnu, where this is an identity,               but `i32` or `u32` on other Linux targets, where it is not"
)]
fn fs_magic(path: &Path) -> Option<i64> {
    nix::sys::statfs::statfs(path)
        .ok()
        .map(|s| i64::from(s.filesystem_type().0))
}

#[cfg(not(target_os = "linux"))]
fn fs_magic(_path: &Path) -> Option<i64> {
    None
}

/// What `root` gets. A root whose filesystem cannot be identified is walked:
/// the error to make is a watch that works but was not needed, not one that
/// was needed and never fires.
fn coverage(root: &Path) -> Coverage {
    match fs_magic(root) {
        Some(magic) if delivers_no_events(magic) => Coverage::RootOnly,
        _ => Coverage::Tree,
    }
}

/// The blocking step of a registration: builds the watcher for the canonical
/// `root` and covers as much of the tree as `coverage` says. Every call in
/// here can stall on the filesystem, which is why none of it runs on a
/// runtime worker.
fn build(id: &WorkspaceId, root: PathBuf, coverage: Coverage) -> Result<Built, RpcError> {
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
    .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
    if PER_DIRECTORY {
        // The root first, so an error there is the one reported: a root that
        // cannot be watched has nothing worth walking.
        watcher
            .watch(&root, RecursiveMode::NonRecursive)
            .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
        match coverage {
            Coverage::Tree => {
                for dir in walk(&root).dirs.iter().skip(1) {
                    watch_quietly(&mut watcher, dir);
                }
            }
            Coverage::RootOnly => tracing::info!(
                ws = %id,
                root = %root.display(),
                "fs.watch: the worktree is on a 9P mount, which delivers no change \
                 notifications; it is not watched, and Refresh is what keeps it current"
            ),
        }
    } else {
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| RpcError::internal(format!("fs.watch {}: {e}", root.display())))?;
    }
    Ok(Built { root, watcher, rx })
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
    #[cfg(test)]
    counters::WALKS.fetch_add(1, Ordering::SeqCst);
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

/// Process-wide tallies the tests read as deltas - how many registrations
/// have reached their blocking step, and how many trees have been walked - and
/// one knob: how long a blocking step is held open once it has started. A
/// test that needs a `disable` or a second `enable` to land *inside* a
/// registration sets the hold, because outside Linux the blocking step is a
/// single recursive `watch` call and over before a test can react to it.
#[cfg(test)]
mod counters {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    pub static BUILDS: AtomicUsize = AtomicUsize::new(0);
    pub static WALKS: AtomicUsize = AtomicUsize::new(0);
    pub static HOLD_MS: AtomicU64 = AtomicU64::new(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::broadcast::EventBus;
    use bondsymphonic_proto::ServerMessage;
    use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::sync::broadcast;

    /// Serialises the tests that read [`counters`] or, on Linux, count this
    /// process's inotify watches: both are per process, and the harness runs
    /// the tests on threads of its own. A `tokio::sync::Mutex` so the guard
    /// can be held across `.await` and a failing test does not poison it for
    /// the next.
    static COUNTED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Directories in the trees the registration tests walk. Enough that a
    /// walk is comfortably measurable against the microseconds `enable` takes
    /// to schedule it, few enough that creating them is not the slow part of
    /// the test.
    const BIG: usize = 2_000;

    /// Directories per level in [`tree_of`]: a fan-out that keeps the tree two
    /// deep rather than a chain, which is the shape a real worktree has.
    const FANOUT: usize = 50;

    /// The longest any registration in these tests is given to settle. Far
    /// past the milliseconds one takes, so only a real stall reaches it.
    const SETTLE: Duration = Duration::from_secs(10);

    /// How long to wait for a `fs.changed` that is expected; and, at three
    /// [`DEBOUNCE`] windows, how long to wait for one that must not come.
    const EXPECTED: Duration = Duration::from_secs(5);
    const SILENCE: Duration = Duration::from_millis(600);

    /// How long a registration's blocking step is held open for a test that
    /// has to land something inside it: long past the poll that waits for the
    /// step to start, short enough not to be the slow part of the test.
    const HOLD: Duration = Duration::from_millis(200);

    /// Sets [`counters::HOLD_MS`] for its lifetime, so a test that panics
    /// does not leave the next one's registrations slow.
    struct Hold;

    impl Hold {
        fn for_(hold: Duration) -> Self {
            counters::HOLD_MS.store(hold.as_millis() as u64, Ordering::SeqCst);
            Hold
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            counters::HOLD_MS.store(0, Ordering::SeqCst);
        }
    }

    fn tree_of(root: &Path, dirs: usize) {
        for i in 0..dirs / FANOUT {
            for j in 0..FANOUT {
                std::fs::create_dir_all(root.join(format!("d{i}/e{j}"))).unwrap();
            }
        }
    }

    /// The next `fs.changed` for `id` within `limit`, or `None`.
    async fn next_change(
        rx: &mut broadcast::Receiver<ServerMessage>,
        id: &WorkspaceId,
        limit: Duration,
    ) -> Option<Vec<String>> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Ok(ServerMessage::Event {
                    workspace_id: Some(ws),
                    event: Event::FsChanged { paths },
                })) if &ws == id => return Some(paths),
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }

    fn is_live(watchers: &Watchers, id: &WorkspaceId) -> bool {
        matches!(
            watchers.active.lock().get(id),
            Some(Watch {
                state: State::Live,
                ..
            })
        )
    }

    /// How many inotify watches this process holds, summed over every inotify
    /// descriptor it has open: one `inotify wd:` line per watch in
    /// `/proc/self/fdinfo/<fd>`. The watcher's real footprint, not anything the
    /// daemon claims. Only read while [`COUNTED`] is held.
    #[cfg(target_os = "linux")]
    fn inotify_watch_count() -> usize {
        std::fs::read_dir("/proc/self/fdinfo")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .map(|info| {
                info.lines()
                    .filter(|l| l.starts_with("inotify wd:"))
                    .count()
            })
            .sum()
    }

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
        // Walking bumps a counter the other tests read as a delta.
        let _serial = COUNTED.blocking_lock();
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

    /// `enable` schedules the registration and returns; it does not walk the
    /// tree itself. This is the request-handling worker's freedom: the walk
    /// that took a runtime worker for four minutes on a 77,274-directory 9P
    /// checkout now costs the handler the time it takes to spawn a task.
    ///
    /// The walk is what `build` does on Linux, so that is where an inline
    /// registration would be caught by the comparison against the walk's own
    /// time. Elsewhere `build` is a single recursive `watch` call and the
    /// comparison holds either way; the absolute bound is the one that means
    /// something there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enable_answers_before_the_walk_finishes() {
        let _serial = COUNTED.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        tree_of(&root, BIG);
        let walk_took = {
            let t = Instant::now();
            let found = walk(&root).dirs.len();
            assert!(found > BIG, "the fixture has {found} directories");
            t.elapsed()
        };

        let watchers = Watchers::default();
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let id: WorkspaceId = "ws_big".into();
        let t = Instant::now();
        watchers.enable(id.clone(), root.clone(), events.clone());
        let enable_took = t.elapsed();
        eprintln!("walk of {BIG} directories took {walk_took:?}; enable took {enable_took:?}");
        // Spawning a task is microseconds; a loaded host might stretch it to a
        // few milliseconds; nothing short of walking the tree reaches this.
        const ENABLE_BOUND: Duration = Duration::from_millis(50);
        assert!(
            enable_took < ENABLE_BOUND,
            "enable took {enable_took:?}: that is a registration run inline"
        );
        assert!(
            enable_took < walk_took,
            "enable took {enable_took:?} against a {walk_took:?} walk: the walk was on the \
             request path"
        );

        tokio::time::timeout(SETTLE, watchers.registered(&id))
            .await
            .expect("the registration settles");
        assert!(is_live(&watchers, &id), "the entry did not become live");
        std::fs::write(root.join("d0/e0/a.txt"), "x").unwrap();
        let paths = next_change(&mut rx, &id, EXPECTED)
            .await
            .expect("the watch is live once registered");
        assert!(paths.contains(&"d0/e0/a.txt".to_string()), "{paths:?}");
    }

    /// The IDE re-sends `fs.watch` on every workspace activation and after each
    /// 30 s timeout. While the first registration is still walking, the retry
    /// must find its entry and start nothing: a second walk of the same tree on
    /// another worker is how the runtime ran out of workers.
    ///
    /// The blocking step is held open so the second `enable` lands inside the
    /// first registration on every platform, not only where the walk makes it
    /// slow. The walk count only moves on Linux, where `build` walks; the
    /// build count is the platform-independent half of the assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_enable_during_registration_does_not_walk_again() {
        let _serial = COUNTED.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        tree_of(&root, BIG);
        let builds_before = counters::BUILDS.load(Ordering::SeqCst);
        let walks_before = counters::WALKS.load(Ordering::SeqCst);

        let watchers = Watchers::default();
        let events = EventBus::new(16);
        let id: WorkspaceId = "ws_retry".into();
        let _hold = Hold::for_(HOLD);
        watchers.enable(id.clone(), root.clone(), events.clone());
        watchers.enable(id.clone(), root.clone(), events.clone());
        tokio::time::timeout(SETTLE, watchers.registered(&id))
            .await
            .expect("the registration settles");
        assert!(is_live(&watchers, &id));
        // Anything the second `enable` started would have to have finished for
        // its counters to be missed: give it the same chance the first got.
        tokio::time::timeout(SETTLE, watchers.registered(&id))
            .await
            .expect("nothing else is registering");

        let builds = counters::BUILDS.load(Ordering::SeqCst) - builds_before;
        let walks = counters::WALKS.load(Ordering::SeqCst) - walks_before;
        assert_eq!(builds, 1, "two enables ran {builds} registrations");
        assert_eq!(
            walks,
            usize::from(PER_DIRECTORY),
            "two enables walked the tree {walks} times"
        );
    }

    /// A root on 9P is never walked: WSL2's drvfs delivers no notifications,
    /// so every one of the 77,274 watches the walk would have installed was a
    /// 9P round trip for nothing. The rule is a pure function of the `statfs`
    /// magic, tested as such; `build` is then handed the classification
    /// directly, since no test host has a 9P mount to offer.
    ///
    /// The walk count moves only on Linux, where `build` walks at all; the
    /// inotify count, also Linux, is the footprint the two coverages leave.
    #[test]
    fn a_9p_root_is_not_walked() {
        let _serial = COUNTED.blocking_lock();
        // `EXT4_SUPER_MAGIC`, and `TMPFS_MAGIC`, which is one digit from 9P's:
        // the comparison is exact, not a range.
        const EXT4_SUPER_MAGIC: i64 = 0xEF53;
        const TMPFS_MAGIC: i64 = 0x0102_1994;
        assert!(delivers_no_events(V9FS_MAGIC));
        assert!(!delivers_no_events(EXT4_SUPER_MAGIC));
        assert!(!delivers_no_events(TMPFS_MAGIC));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        const DIRS: usize = 200;
        tree_of(&root, DIRS);
        // A temp directory is tmpfs or a real disk on every host this runs on,
        // never 9P: the classifier says so, and this is the answer a worktree
        // under `~/.bondsymphonic/worktrees` gets.
        assert_eq!(coverage(&root), Coverage::Tree);
        let id: WorkspaceId = "ws_9p".into();

        let walks_before = counters::WALKS.load(Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        let watches_before = inotify_watch_count();
        let root_only = build(&id, root.clone(), Coverage::RootOnly).unwrap();
        assert_eq!(
            counters::WALKS.load(Ordering::SeqCst) - walks_before,
            0,
            "a 9P root was walked"
        );
        #[cfg(target_os = "linux")]
        {
            let installed = inotify_watch_count() - watches_before;
            assert_eq!(
                installed, 1,
                "a 9P root installed {installed} watches, not the root's one"
            );
        }

        // The same tree with the ordinary coverage, for contrast: this is what
        // the classification saves.
        let tree = build(&id, root.clone(), Coverage::Tree).unwrap();
        assert_eq!(
            counters::WALKS.load(Ordering::SeqCst) - walks_before,
            usize::from(PER_DIRECTORY),
            "a walked root was not walked exactly once"
        );
        #[cfg(target_os = "linux")]
        {
            let installed = inotify_watch_count() - watches_before;
            assert!(
                installed > DIRS,
                "a walked root installed {installed} watches for {DIRS} directories"
            );
        }
        drop(tree);
        drop(root_only);
    }

    /// A `disable` that lands while the registration is still on its blocking
    /// thread: the entry goes at once, the watcher is dropped unused when the
    /// blocking step returns, and no coalescing task is ever started to publish
    /// from it.
    ///
    /// Timed off the build counter so the disable meets a registration that
    /// has started, and the blocking step is held open so that it meets one
    /// that has not finished: on Linux the walk alone would take long enough,
    /// elsewhere the step is a single recursive `watch` call and over before
    /// the poll returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disable_during_registration_leaves_nothing_behind() {
        let _serial = COUNTED.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        tree_of(&root, BIG);
        let builds_before = counters::BUILDS.load(Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        let watches_before = inotify_watch_count();

        let watchers = Watchers::default();
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let id: WorkspaceId = "ws_gone".into();
        let _hold = Hold::for_(HOLD);
        watchers.enable(id.clone(), root.clone(), events.clone());
        let started = Instant::now();
        while counters::BUILDS.load(Ordering::SeqCst) == builds_before {
            assert!(
                started.elapsed() < SETTLE,
                "the blocking step never started"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Obtained before the disable, so it follows this registration to the
        // end rather than resolving at once for a workspace no longer in the map.
        let settled = watchers.registered(&id);
        watchers.disable(&id);
        assert!(
            watchers.active.lock().is_empty(),
            "the entry outlived the disable"
        );
        tokio::time::timeout(SETTLE, settled)
            .await
            .expect("the aborted registration settles");
        assert!(watchers.active.lock().is_empty(), "the entry came back");

        // Nothing is publishing: the coalescing task never started.
        std::fs::write(root.join("d0/e0/a.txt"), "x").unwrap();
        assert!(
            next_change(&mut rx, &id, SILENCE).await.is_none(),
            "a disabled registration published"
        );
        // And nothing is watching. `notify` closes the inotify descriptor from
        // its own thread after the watcher is dropped, so the count is polled
        // rather than read once.
        #[cfg(target_os = "linux")]
        {
            let t = Instant::now();
            while inotify_watch_count() != watches_before {
                assert!(
                    t.elapsed() < Duration::from_secs(2),
                    "{} inotify watches left behind",
                    inotify_watch_count() - watches_before
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}
