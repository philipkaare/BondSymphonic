use super::Workspace;
use anyhow::{Context, Result};
use bondsymphonic_proto::{RpcError, WorkspaceId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// The registry format this daemon writes.
///
/// 1. The original.
/// 2. Every workspace carries a host allowlist that is actually enforced. A
///    version-1 file was written when the field existed but nothing read it, so
///    its workspaces have an empty list — see [`migrate`].
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    /// Defaults to 0, which is older than any version there has been, so a file
    /// without the key is migrated rather than rejected.
    #[serde(default)]
    version: u32,
    workspaces: Vec<Workspace>,
}

/// Brings stored workspaces up to [`FORMAT_VERSION`], answering whether
/// anything changed and the file is worth rewriting.
///
/// The only migration so far fills an empty `allowlist` with the defaults.
/// Before version 2 the field was written but never consulted, so every
/// workspace created then has an empty list — which, once the proxy enforces
/// it, means the workspace can reach nothing at all and the agent inside it
/// stops being able to call the Anthropic API.
///
/// It is keyed on the file's version and not on the list being empty, because
/// `workspace.set_allowlist` can empty a list deliberately: from version 2 on,
/// an empty list means "reach nothing" and has to survive a restart intact.
fn migrate(workspaces: &mut [Workspace], version: u32) -> bool {
    if version >= FORMAT_VERSION {
        return false;
    }
    for ws in workspaces.iter_mut() {
        if ws.allowlist.is_empty() {
            ws.allowlist = crate::net::allowlist::DEFAULT_ALLOW
                .iter()
                .map(|h| h.to_string())
                .collect();
            tracing::info!(ws = %ws.id, "filled an empty allowlist with the defaults");
        }
    }
    true
}

struct Inner {
    path: PathBuf,
    workspaces: Vec<Workspace>,
    /// Counts the writes this registry has prepared, so [`PendingWrite`] can
    /// tell an older one from a newer one. Bumped under the write lock, which
    /// is what makes "newer" mean "saw more changes".
    seq: u64,
}

/// Persistent workspace registry. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<Inner>>,
    /// The sequence number of the newest state that has reached the disk.
    /// Separate from [`Inner`] because it is taken on the blocking thread that
    /// does the writing, long after the registry lock has been let go.
    written: Arc<std::sync::Mutex<u64>>,
}

impl Registry {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let stored = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<FileFormat>(&s)
                .with_context(|| format!("parsing {}", path.display()))?,
            // No file yet: nothing to migrate, and nothing to write until the
            // first workspace is inserted.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileFormat {
                version: FORMAT_VERSION,
                workspaces: Vec::new(),
            },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut inner = Inner {
            path: path.to_path_buf(),
            workspaces: stored.workspaces,
            seq: 0,
        };
        let written = Arc::new(std::sync::Mutex::new(0));
        if migrate(&mut inner.workspaces, stored.version) {
            // Written here and now: `load` runs from `main` before there is a
            // runtime at all, and nothing else can be holding this registry up
            // yet. The migrated list is already correct in memory, and every
            // insert or update rewrites the file anyway, so a data directory we
            // cannot write to is a reason to complain rather than to refuse to
            // start.
            if let Err(e) = prepare(&mut inner, &written).and_then(PendingWrite::write_now) {
                tracing::warn!(path = %path.display(), error = %e, "could not rewrite the migrated registry");
            }
        }
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
            written,
        })
    }

    pub fn list(&self) -> Vec<Workspace> {
        self.inner.read().workspaces.clone()
    }

    pub fn get(&self, id: &WorkspaceId) -> Option<Workspace> {
        self.inner
            .read()
            .workspaces
            .iter()
            .find(|w| &w.id == id)
            .cloned()
    }

    pub fn find_by_name(&self, repo: &std::path::Path, name: &str) -> Option<Workspace> {
        self.inner
            .read()
            .workspaces
            .iter()
            .find(|w| w.repo_path == repo && w.name == name)
            .cloned()
    }

    pub async fn insert(&self, ws: Workspace) -> Result<()> {
        let write = {
            let mut g = self.inner.write();
            g.workspaces.retain(|w| w.id != ws.id);
            g.workspaces.push(ws);
            prepare(&mut g, &self.written)?
        };
        write.commit().await
    }

    pub async fn update(
        &self,
        id: &WorkspaceId,
        f: impl FnOnce(&mut Workspace),
    ) -> Result<Workspace, RpcError> {
        let (out, write) = {
            let mut g = self.inner.write();
            let ws = g
                .workspaces
                .iter_mut()
                .find(|w| &w.id == id)
                .ok_or_else(|| RpcError::not_found(format!("workspace {id}")))?;
            f(ws);
            let out = ws.clone();
            let write = prepare(&mut g, &self.written)
                .map_err(|e| RpcError::io(&std::io::Error::other(e.to_string())))?;
            (out, write)
        };
        write
            .commit()
            .await
            .map_err(|e| RpcError::io(&std::io::Error::other(e.to_string())))?;
        Ok(out)
    }

    pub async fn remove(&self, id: &WorkspaceId) -> Result<Option<Workspace>> {
        let (removed, write) = {
            let mut g = self.inner.write();
            let pos = g.workspaces.iter().position(|w| &w.id == id);
            let removed = pos.map(|i| g.workspaces.remove(i));
            (removed, prepare(&mut g, &self.written)?)
        };
        write.commit().await?;
        Ok(removed)
    }
}

/// One registry state, serialised and ready to go to disk, no longer holding
/// the registry lock.
///
/// Splitting the save in two is what lets the `fsync` leave the runtime. The
/// bytes are built under the lock, so they are a state the registry really was
/// in; the write happens afterwards, on a blocking thread, with nothing held.
struct PendingWrite {
    path: PathBuf,
    bytes: Vec<u8>,
    /// Where this state sits in the order the registry lock handed writes out.
    seq: u64,
    written: Arc<std::sync::Mutex<u64>>,
}

/// Serialises the registry as it stands and claims the next sequence number.
/// Called with the registry's write lock held; does no I/O.
///
/// The error is the serialisation, which a list of workspace records cannot
/// realistically fail at -- but it is answered rather than swallowed, because
/// the alternative to "this update failed" would be writing whatever came out
/// of a failure over the daemon's only record of its workspaces.
fn prepare(inner: &mut Inner, written: &Arc<std::sync::Mutex<u64>>) -> Result<PendingWrite> {
    let data = FileFormat {
        version: FORMAT_VERSION,
        workspaces: inner.workspaces.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&data)?;
    inner.seq += 1;
    Ok(PendingWrite {
        path: inner.path.clone(),
        bytes,
        seq: inner.seq,
        written: written.clone(),
    })
}

impl PendingWrite {
    /// Writes this state, atomically, unless a newer one has already landed.
    ///
    /// The registry is the daemon's only record of which workspaces exist and
    /// where their worktrees are; a half-written one at the next start means
    /// workspaces the user can neither open nor destroy, with their worktrees
    /// and object directories left on disk.
    /// [`crate::util::atomic::write_atomic`] is what makes the file on disk
    /// either the previous registry or this one.
    ///
    /// Two writes prepared under the lock in one order can reach a blocking
    /// thread in the other, and the loser would put a state back that the
    /// winner had already moved past. The sequence number settles it: a write
    /// older than what is on disk is dropped, and its caller is answered `Ok`
    /// because a strictly newer state -- one that was serialised after this
    /// caller's own change went into the list -- is already there.
    fn write_now(self) -> Result<()> {
        // Poisoned only if a previous write panicked mid-`write_atomic`, which
        // leaves the counter itself perfectly consistent.
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        if *written >= self.seq {
            return Ok(());
        }
        crate::util::atomic::write_atomic(&self.path, &self.bytes)?;
        *written = self.seq;
        Ok(())
    }

    /// [`write_now`](Self::write_now) on a blocking thread, awaited here.
    ///
    /// The `fsync` is milliseconds on a busy disk and unbounded on a failing
    /// one, and every caller is a request handler on one of the daemon's tokio
    /// workers: `workspace.create`, a state change, a destroy. A worker inside
    /// `sync_all` is a worker polling nothing at all, so the write goes to the
    /// blocking pool, which exists for exactly this.
    ///
    /// `spawn_blocking` and not `block_in_place`: the latter moves the write
    /// off the runtime by handing *this worker's core* to a sibling thread, and
    /// the thread left without a core returns to the blocking pool and exits
    /// ten seconds later. Threads in this daemon are not interchangeable --
    /// `bwrap --die-with-parent` keys a sandbox's life to the thread that
    /// forked it -- so a call that quietly retires the caller's thread is a
    /// trap, and `workspace.create` walked into it by writing the registry
    /// just before starting a sandbox.
    ///
    /// The write is still awaited rather than queued and forgotten: a caller
    /// told that a workspace was updated has to have been told so by a file
    /// that really was written, and the error has to come back to the request
    /// that caused it.
    async fn commit(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.write_now())
            .await
            .context("writing the registry")?
    }
}
