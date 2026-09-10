//! What the IDE remembers between runs: `state.json`.
//!
//! Pure Rust, like the rest of `model/`: nothing here imports Qt. The QObject
//! adapters hand this module strings and take strings back; what is worth
//! keeping, how a burst of changes collapses into one write, and what a
//! corrupt file means are decided here.
//!
//! The file sits beside `settings.json` (see
//! [`crate::qobjects::settings::Settings::state_path`]) and is written
//! atomically -- a temporary file next to it, then a rename -- so a crash
//! mid-write leaves either the previous state or the new one, never half of
//! each. A file that will not parse is moved aside as `<name>.corrupt` rather
//! than overwritten, so the user's groups can still be recovered by hand.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The shape this build writes. Bumped only when an older IDE could no longer
/// make sense of the file; every field is optional on the way in, so adding
/// one does not need a new version.
pub const STATE_VERSION: u32 = 1;

/// How many repositories the New Agent dialog's "Recent" list keeps.
pub const MAX_RECENT_REPOS: usize = 10;

/// How long a change waits before it is written. Long enough that dragging a
/// splitter is one write rather than sixty, short enough that a crash a second
/// after the last change still has it on disk.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// One group as the file records it: a name and the workspaces in it, in the
/// order the user left them. The tabs themselves are not stored -- the daemon
/// is authoritative about what a workspace *is*, so only the arrangement is
/// ours to remember.
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Debug, Clone)]
#[serde(default)]
pub struct PersistedGroup {
    pub name: String,
    pub workspace_ids: Vec<String>,
}

/// The groups plus which workspace was active, as `GroupModel::groupsJson`
/// hands them to `AppController::noteGroups`. One object rather than two
/// arguments so the C++ side passes a single string it never has to build.
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Debug, Clone)]
#[serde(default)]
pub struct PersistedGroups {
    pub groups: Vec<PersistedGroup>,
    pub active_workspace: Option<String>,
}

/// Everything `state.json` holds.
///
/// Every field is `#[serde(default)]` through the container attribute, so a
/// file written by an older build -- or by a newer one that grew a field this
/// build has never heard of -- still loads with the rest intact.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
#[serde(default)]
pub struct StateFile {
    pub version: u32,
    pub groups: Vec<PersistedGroup>,
    pub active_workspace: Option<String>,
    /// Open editor tabs per workspace, as worktree-relative paths in tab order.
    pub open_editors: BTreeMap<String, Vec<String>>,
    /// The editor tab that was in front, per workspace.
    pub active_editor: BTreeMap<String, String>,
    /// `QSplitter::sizes()` for the agent-area splitter.
    pub splitter_sizes: Vec<i32>,
    /// Whether the user swapped the two halves of that splitter.
    pub swapped: bool,
    /// Repositories the New Agent dialog offers, most recent first.
    pub recent_repos: Vec<String>,
    /// `QMainWindow::saveState()`, base64.
    pub window_state_b64: String,
    /// `QWidget::saveGeometry()`, base64.
    pub geometry_b64: String,
    /// Per-start port overrides, keyed by [`port_key`].
    pub port_overrides: BTreeMap<String, u16>,
}

impl Default for StateFile {
    /// Written out rather than derived so a fresh file carries
    /// [`STATE_VERSION`] instead of a zero no loader would recognise.
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            groups: Vec::new(),
            active_workspace: None,
            open_editors: BTreeMap::new(),
            active_editor: BTreeMap::new(),
            splitter_sizes: Vec::new(),
            swapped: false,
            recent_repos: Vec::new(),
            window_state_b64: String::new(),
            geometry_b64: String::new(),
            port_overrides: BTreeMap::new(),
        }
    }
}

/// The key one port override is filed under. A newline separates the two
/// halves because neither a workspace id nor a run configuration name can
/// contain one, so the key can never be ambiguous.
pub fn port_key(workspace: &str, config: &str) -> String {
    format!("{workspace}\n{config}")
}

/// The workspace half of a [`port_key`].
fn port_key_workspace(key: &str) -> &str {
    key.split('\n').next().unwrap_or(key)
}

impl StateFile {
    /// Records `repo` as the most recently used repository: no duplicates, most
    /// recent first, never more than [`MAX_RECENT_REPOS`]. An empty path is not
    /// a repository anyone can reopen, so it is ignored.
    pub fn note_recent(&mut self, repo: &str) {
        if repo.is_empty() {
            return;
        }
        self.recent_repos.retain(|existing| existing != repo);
        self.recent_repos.insert(0, repo.to_owned());
        self.recent_repos.truncate(MAX_RECENT_REPOS);
    }

    /// Drops everything belonging to a workspace the daemon no longer has.
    ///
    /// Empty groups survive: a group the user emptied is still a group they
    /// made, and re-creating it by hand after every restart would be worse than
    /// carrying an empty row.
    pub fn prune(&mut self, live_workspace_ids: &[String]) {
        let live = |id: &String| live_workspace_ids.iter().any(|k| k == id);
        for group in &mut self.groups {
            group.workspace_ids.retain(&live);
        }
        self.open_editors.retain(|ws, _| live(ws));
        self.active_editor.retain(|ws, _| live(ws));
        self.port_overrides.retain(|key, _| {
            live_workspace_ids
                .iter()
                .any(|k| k == port_key_workspace(key))
        });
        if let Some(active) = &self.active_workspace {
            if !live(active) {
                self.active_workspace = None;
            }
        }
    }

    /// The port a run of `config` in `workspace` should use instead of the
    /// configured one, if the user set one.
    pub fn port_override(&self, workspace: &str, config: &str) -> Option<u16> {
        self.port_overrides
            .get(&port_key(workspace, config))
            .copied()
    }

    /// Records (or, with `None`, clears) that override.
    pub fn set_port_override(&mut self, workspace: &str, config: &str, port: Option<u16>) {
        let key = port_key(workspace, config);
        match port {
            Some(port) => {
                self.port_overrides.insert(key, port);
            }
            None => {
                self.port_overrides.remove(&key);
            }
        }
    }

    /// Replaces the recorded arrangement wholesale, which is what a
    /// `GroupModel` mutation reports.
    pub fn set_groups(&mut self, groups: Vec<PersistedGroup>, active_workspace: Option<String>) {
        self.groups = groups;
        self.active_workspace = active_workspace;
    }

    /// Files `workspace` under `group`, creating that group at the end if it is
    /// new and taking the workspace out of whatever group held it before, so a
    /// workspace is never in two groups at once. It becomes the active one.
    ///
    /// This is how the controller keeps the file honest between the first
    /// `workspace.list` and the next report from the tab model.
    pub fn add_to_group(&mut self, group: &str, workspace: &str) {
        for existing in &mut self.groups {
            existing.workspace_ids.retain(|id| id != workspace);
        }
        let idx = match self.groups.iter().position(|g| g.name == group) {
            Some(idx) => idx,
            None => {
                self.groups.push(PersistedGroup {
                    name: group.to_owned(),
                    workspace_ids: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        self.groups[idx].workspace_ids.push(workspace.to_owned());
        self.active_workspace = Some(workspace.to_owned());
    }

    /// Forgets one workspace entirely: its group entry, its editors and its
    /// port overrides. [`StateFile::prune`] for a single id.
    pub fn forget_workspace(&mut self, workspace: &str) {
        for group in &mut self.groups {
            group.workspace_ids.retain(|id| id != workspace);
        }
        self.open_editors.remove(workspace);
        self.active_editor.remove(workspace);
        self.port_overrides
            .retain(|key, _| port_key_workspace(key) != workspace);
        if self.active_workspace.as_deref() == Some(workspace) {
            self.active_workspace = None;
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// `<path>.corrupt` -- the whole file name plus a suffix, so `state.json`
/// becomes `state.json.corrupt` rather than losing its extension.
fn corrupt_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.file_name().unwrap_or_default());
    name.push(".corrupt");
    path.with_file_name(name)
}

/// Reads `state.json`.
///
/// A file that is not there is a first run and reads as the defaults. A file
/// that will not parse is moved aside as `<name>.corrupt` and reported at warn
/// level: overwriting it would throw away the only copy of an arrangement the
/// user may well want back.
pub fn load(path: &Path) -> StateFile {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return StateFile::default(),
        Err(e) => {
            tracing::warn!("{} could not be read ({e}); starting fresh", path.display());
            return StateFile::default();
        }
    };
    match serde_json::from_str::<StateFile>(&raw) {
        Ok(state) => {
            if state.version != STATE_VERSION {
                tracing::warn!(
                    "{} was written by another version of this file format ({} rather than {STATE_VERSION}); reading it anyway",
                    path.display(),
                    state.version
                );
            }
            state
        }
        Err(e) => {
            let aside = corrupt_path(path);
            match std::fs::rename(path, &aside) {
                Ok(()) => tracing::warn!(
                    "{} is not readable ({e}); kept as {} and starting fresh",
                    path.display(),
                    aside.display()
                ),
                Err(rename) => tracing::warn!(
                    "{} is not readable ({e}) and could not be moved aside ({rename}); starting fresh",
                    path.display()
                ),
            }
            StateFile::default()
        }
    }
}

/// Writes `state.json` atomically: a temporary file beside it, then a rename.
/// The temporary lives in the same directory so the rename stays on one volume
/// and is therefore atomic.
pub fn save(path: &Path, state: &StateFile) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut name = OsString::from(path.file_name().unwrap_or_default());
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leaving a stray `.tmp` beside the real file would be read as a
            // half-written state by the next person to look in the directory.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The [`StateFile`] the running IDE holds, plus the debounce bookkeeping.
///
/// Every change goes through [`StateStore::update`], which marks the state
/// dirty and hands back a token. The caller sleeps [`DEBOUNCE`] and then calls
/// [`StateStore::flush_if_current`] with that token; a change made in the
/// meantime has issued a newer token, so the earlier sleeper does nothing and
/// a burst collapses into one write. [`StateStore::flush`] ignores the tokens
/// and is what "save before exiting" uses.
///
/// The sleeping is deliberately not done here: this module owns no runtime.
pub struct StateStore {
    /// `None` when the platform gave us no config directory. Everything still
    /// works, nothing is written, and the reason is logged once per attempt.
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

struct Inner {
    state: StateFile,
    dirty: bool,
    token: u64,
}

impl StateStore {
    /// A store over `path` starting from the defaults. Nothing is read.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            inner: Mutex::new(Inner {
                state: StateFile::default(),
                dirty: false,
                token: 0,
            }),
        }
    }

    /// A store over `path` holding what that file says, or the defaults when
    /// there is no file or it will not parse.
    pub fn load(path: Option<PathBuf>) -> Self {
        let state = match &path {
            Some(p) => load(p),
            None => StateFile::default(),
        };
        Self {
            path,
            inner: Mutex::new(Inner {
                state,
                dirty: false,
                token: 0,
            }),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Reads the state without changing it.
    pub fn with<R>(&self, f: impl FnOnce(&StateFile) -> R) -> R {
        f(&self.lock().state)
    }

    pub fn snapshot(&self) -> StateFile {
        self.with(|s| s.clone())
    }

    pub fn to_json(&self) -> String {
        self.with(|s| s.to_json())
    }

    /// Changes the state, marks it dirty, and returns the token whose timer is
    /// allowed to write it.
    pub fn update<R>(&self, f: impl FnOnce(&mut StateFile) -> R) -> u64 {
        let mut inner = self.lock();
        f(&mut inner.state);
        inner.dirty = true;
        inner.token = inner.token.wrapping_add(1);
        inner.token
    }

    /// Writes iff there is something to write and `token` is still the newest
    /// one handed out. Returns whether it wrote.
    pub fn flush_if_current(&self, token: u64) -> bool {
        if self.lock().token != token {
            return false;
        }
        self.flush()
    }

    /// Writes iff there is something to write, whatever the tokens say.
    /// Returns whether it wrote.
    pub fn flush(&self) -> bool {
        let mut inner = self.lock();
        if !inner.dirty {
            return false;
        }
        let Some(path) = &self.path else {
            tracing::warn!("no config directory: state.json cannot be written");
            // Left dirty on purpose: nothing was saved, so nothing is clean.
            return false;
        };
        match save(path, &inner.state) {
            Ok(()) => {
                inner.dirty = false;
                tracing::debug!("state written to {}", path.display());
                true
            }
            Err(e) => {
                tracing::warn!("{} could not be written: {e}", path.display());
                false
            }
        }
    }

    /// A poisoned lock means a panic while the state was borrowed. The state is
    /// a plain data structure with no invariant a panic can break halfway, so
    /// carrying on with it is better than taking the IDE down over it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
