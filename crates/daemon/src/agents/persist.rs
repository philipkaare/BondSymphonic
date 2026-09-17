//! What the daemon remembers about an agent after the process is gone.
//!
//! A transcript file already survives a restart, but on its own it is
//! unreachable: `agent.history` goes through the agent map, `WorkspaceInfo`
//! lists the map, and the map is built from live processes. One record per
//! agent beside the transcripts is what puts the agent back into both, so a
//! daemon that was killed and restarted shows the same tabs with the same
//! history and offers to resume the session rather than losing the
//! conversation.
//!
//! The file is small and rewritten whole, atomically, the way the workspace
//! registry is: a handful of agents per workspace, written when one starts,
//! reports its session id, or ends.

use bondsymphonic_proto::{AgentAdapterKind, AgentId, AgentStartOptions, WorkspaceId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

/// The records format this daemon writes. Version 1 is the original.
const FORMAT_VERSION: u32 = 1;

/// One agent the daemon has started, as it survives the daemon.
///
/// `options` is what the agent was started with, minus the API key: it is what
/// a client needs to offer "start another one like this", and the key is a
/// secret the user typed for one process, not state to keep on disk. It is
/// cleared by [`AgentRecords::upsert`] rather than by the caller, so no future
/// call site can forget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    pub workspace_id: WorkspaceId,
    pub adapter: AgentAdapterKind,
    /// The last session id the agent reported, which is what a resume needs.
    #[serde(default)]
    pub session_id: Option<String>,
    pub options: AgentStartOptions,
    pub started_at: String,
    /// When the agent's process ended, or `None` while it is running.
    ///
    /// A record found open at startup belonged to an agent that was still
    /// running when the daemon went, and [`crate::agents::AgentManager::restore`]
    /// closes it.
    #[serde(default)]
    pub ended_at: Option<String>,
}

/// This daemon could not read the records file, and so must not write over it.
///
/// Deliberately not an `io::Error`: every caller does the same thing with it,
/// which is nothing, and the reason has already been logged where it was known.
#[derive(Debug)]
struct Unreadable;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    agents: Vec<AgentRecord>,
}

/// The records file, read and rewritten whole.
///
/// Every mutation is a read-modify-write, serialised by [`Self::lock`] so two
/// agents starting at once cannot each write a file that omits the other. The
/// file is the daemon's only copy: the manager holds live agents, and asking
/// the file is what makes the two agree even after a write failed.
pub struct AgentRecords {
    path: PathBuf,
    /// Held across a whole read-modify-write. Never held across an `await`:
    /// every method here is synchronous.
    lock: Mutex<()>,
    /// Test hook: how long an [`update`](Self::update) that leaves a record
    /// closed takes on top of the write itself. See
    /// [`delay_closing_writes`](Self::delay_closing_writes).
    closing_delay: Mutex<Option<std::time::Duration>>,
}

impl AgentRecords {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
            closing_delay: Mutex::new(None),
        }
    }

    /// Makes every write that leaves a record closed take `by` longer, the way
    /// an `fsync` on a busy disk does.
    ///
    /// For tests only, which is why it is hidden rather than absent: the
    /// integration tests are another crate and cannot see `cfg(test)` items.
    /// The race it exists to reproduce is between an agent's exit announcement
    /// and those writes, and a real disk is slow only when it feels like it.
    /// Only closing writes, so the session id a turn records on the way does
    /// not move anything else in the test's timeline.
    #[doc(hidden)]
    pub fn delay_closing_writes(&self, by: std::time::Duration) {
        *self.closing_delay.lock() = Some(by);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every record, oldest first, with an empty list for a file this daemon
    /// could not read.
    ///
    /// For readers only. A caller that is about to *write* must use
    /// [`read_locked`](Self::read_locked) instead and leave the file alone when
    /// it answers `Err`: the two cases look the same here and are not the same
    /// at all.
    pub fn load(&self) -> Vec<AgentRecord> {
        let _guard = self.lock.lock();
        self.read_locked().unwrap_or_default()
    }

    /// The records, or [`Unreadable`] when this daemon could not get at them.
    ///
    /// Three outcomes, and the difference between the last two is the whole
    /// point:
    ///
    /// * **No file.** The normal case for a daemon that has never started an
    ///   agent. An empty list, and writing over it is correct.
    /// * **A file that will not parse.** Not a reason to refuse to start: it is
    ///   moved aside as `<name>.corrupt` and reported, so the agents are lost
    ///   but the daemon, its workspaces and their transcripts are not. The
    ///   evidence is preserved under the new name, so an empty list is a safe
    ///   thing to write over what is now a *missing* file. If the rename itself
    ///   fails there is no copy, so it becomes `Unreadable` instead.
    /// * **A file that could not be read at all.** A sharing violation from an
    ///   indexer or a scanner on Windows, a permission the daemon lost, a
    ///   failing disk. The records are still there and still good, and treating
    ///   this as "no records" would have the next write replace every one of
    ///   them with the single record it happens to be adding. A lost update is
    ///   recoverable; an overwritten file is not.
    fn read_locked(&self) -> Result<Vec<AgentRecord>, Unreadable> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "could not read the agent records");
                return Err(Unreadable);
            }
        };
        let e = match serde_json::from_str::<FileFormat>(&text) {
            Ok(f) => return Ok(f.agents),
            Err(e) => e,
        };
        let aside = self.path.with_extension("json.corrupt");
        warn!(
            path = %self.path.display(),
            kept = %aside.display(),
            error = %e,
            "the agent records will not parse; keeping them aside and starting with none"
        );
        match std::fs::rename(&self.path, &aside) {
            Ok(()) => Ok(Vec::new()),
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "could not move the unreadable records aside");
                Err(Unreadable)
            }
        }
    }

    /// Adds `record`, or replaces the one with the same agent id.
    ///
    /// The API key is dropped here, whatever the caller passed.
    pub fn upsert(&self, mut record: AgentRecord) {
        record.options.api_key = None;
        let _guard = self.lock.lock();
        let Ok(mut all) = self.read_locked() else {
            warn!(agent = %record.agent_id, "not recording the agent: the records file could not be read");
            return;
        };
        match all.iter_mut().find(|r| r.agent_id == record.agent_id) {
            Some(existing) => *existing = record,
            None => all.push(record),
        }
        self.save_locked(&all);
    }

    /// Forgets one agent, if it is there.
    ///
    /// The start path uses it to take back the record it wrote before spawning
    /// the process, when the spawn then failed.
    pub fn remove(&self, agent: &AgentId) {
        let _guard = self.lock.lock();
        let Ok(mut all) = self.read_locked() else {
            warn!(agent = %agent, "not removing the record: the records file could not be read");
            return;
        };
        let before = all.len();
        all.retain(|r| &r.agent_id != agent);
        if all.len() != before {
            self.save_locked(&all);
        }
    }

    /// Applies `f` to the record for `agent`, if there is one. A record that is
    /// not there is not an error: the file may have been lost, and an agent
    /// whose record went missing must still be able to run.
    pub fn update(&self, agent: &AgentId, f: impl FnOnce(&mut AgentRecord)) {
        let _guard = self.lock.lock();
        let Ok(mut all) = self.read_locked() else {
            warn!(agent = %agent, "not updating the record: the records file could not be read");
            return;
        };
        let Some(record) = all.iter_mut().find(|r| &r.agent_id == agent) else {
            return;
        };
        f(record);
        record.options.api_key = None;
        let closing_delay = *self.closing_delay.lock();
        if let (Some(by), Some(_)) = (closing_delay, &record.ended_at) {
            std::thread::sleep(by);
        }
        self.save_locked(&all);
    }

    /// Replaces the whole list, in the order given.
    ///
    /// Used by the restore path, which reads every record, closes the ones that
    /// were left open and writes the result back in one step rather than once
    /// per agent.
    pub fn replace_all(&self, mut records: Vec<AgentRecord>) {
        for r in records.iter_mut() {
            r.options.api_key = None;
        }
        let _guard = self.lock.lock();
        // Read first and throw the answer away: this writes the whole file, so
        // it is the one path that could replace records it never saw.
        if self.read_locked().is_err() {
            warn!("not rewriting the records: the file could not be read");
            return;
        }
        self.save_locked(&records);
    }

    /// Forgets every record belonging to `ws`, answering the ids removed so the
    /// caller can take their transcripts with them.
    pub fn remove_workspace(&self, ws: &WorkspaceId) -> Vec<AgentId> {
        let _guard = self.lock.lock();
        let Ok(mut all) = self.read_locked() else {
            warn!(ws = %ws, "not removing the workspace's records: the file could not be read");
            return Vec::new();
        };
        let mut removed = Vec::new();
        all.retain(|r| {
            if &r.workspace_id == ws {
                removed.push(r.agent_id.clone());
                false
            } else {
                true
            }
        });
        if !removed.is_empty() {
            self.save_locked(&all);
        }
        removed
    }

    /// Writes the file through [`crate::util::atomic::write_atomic`]: a unique
    /// temporary beside it, synced, then renamed over it, so a daemon killed
    /// mid-write leaves either the old file or the new one and never half of
    /// either.
    ///
    /// A failure is logged rather than propagated. Losing the record of an
    /// agent costs the IDE its history after the next restart; refusing to
    /// start the agent over it would cost the user the agent now.
    fn save_locked(&self, records: &[AgentRecord]) {
        if let Err(e) = self.write(records) {
            warn!(path = %self.path.display(), error = %e, "could not write the agent records");
        }
    }

    fn write(&self, records: &[AgentRecord]) -> std::io::Result<()> {
        let data = FileFormat {
            version: FORMAT_VERSION,
            agents: records.to_vec(),
        };
        crate::util::atomic::write_atomic(&self.path, &serde_json::to_vec_pretty(&data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, ws: &str) -> AgentRecord {
        AgentRecord {
            agent_id: id.into(),
            workspace_id: ws.into(),
            adapter: AgentAdapterKind::Claude,
            session_id: None,
            options: AgentStartOptions {
                command: None,
                resume_session: None,
                model: Some("claude-opus-5".into()),
                permission_mode: None,
                api_key: Some("sk-ant-secret".into()),
            },
            started_at: "2026-09-10T10:00:00Z".into(),
            ended_at: None,
        }
    }

    #[test]
    fn a_missing_file_reads_as_no_records() {
        let dir = tempfile::tempdir().unwrap();
        let records = AgentRecords::new(dir.path().join("nested").join("agents.json"));
        assert!(records.load().is_empty());
        // And the first write creates the directory it needs.
        records.upsert(record("ag_1", "ws_1"));
        assert_eq!(records.load().len(), 1);
    }

    #[test]
    fn upsert_replaces_by_id_keeps_order_and_never_stores_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let records = AgentRecords::new(dir.path().join("agents.json"));
        records.upsert(record("ag_1", "ws_1"));
        records.upsert(record("ag_2", "ws_1"));
        let mut second = record("ag_1", "ws_1");
        second.session_id = Some("sess-9".into());
        second.ended_at = Some("2026-09-10T11:00:00Z".into());
        records.upsert(second);

        let all = records.load();
        assert_eq!(
            all.iter().map(|r| r.agent_id.as_str()).collect::<Vec<_>>(),
            vec!["ag_1", "ag_2"],
            "an update must not move the record to the end"
        );
        assert_eq!(all[0].session_id.as_deref(), Some("sess-9"));
        assert_eq!(all[0].ended_at.as_deref(), Some("2026-09-10T11:00:00Z"));
        assert_eq!(all[0].options.model.as_deref(), Some("claude-opus-5"));
        for r in &all {
            assert_eq!(r.options.api_key, None);
        }
        let raw = std::fs::read_to_string(dir.path().join("agents.json")).unwrap();
        assert!(!raw.contains("sk-ant-secret"), "{raw}");
    }

    #[test]
    fn update_touches_one_record_and_ignores_an_id_it_does_not_have() {
        let dir = tempfile::tempdir().unwrap();
        let records = AgentRecords::new(dir.path().join("agents.json"));
        records.upsert(record("ag_1", "ws_1"));
        records.upsert(record("ag_2", "ws_1"));
        records.update(&"ag_2".into(), |r| r.session_id = Some("sess-2".into()));
        records.update(&"ag_missing".into(), |r| r.session_id = Some("nope".into()));

        let all = records.load();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].session_id, None);
        assert_eq!(all[1].session_id.as_deref(), Some("sess-2"));
    }

    #[test]
    fn remove_workspace_takes_only_its_own_and_answers_what_it_took() {
        let dir = tempfile::tempdir().unwrap();
        let records = AgentRecords::new(dir.path().join("agents.json"));
        records.upsert(record("ag_1", "ws_1"));
        records.upsert(record("ag_2", "ws_2"));
        records.upsert(record("ag_3", "ws_1"));

        let removed = records.remove_workspace(&"ws_1".into());
        assert_eq!(
            removed.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            vec!["ag_1", "ag_3"]
        );
        let left = records.load();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].agent_id.as_str(), "ag_2");
        assert!(records.remove_workspace(&"ws_nothing".into()).is_empty());
    }

    /// The point of the whole file: a record written by one daemon is read by
    /// the next one, with the fields that matter for a resume intact.
    #[test]
    fn records_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let mut r = record("ag_1", "ws_1");
        r.session_id = Some("sess-1".into());
        r.ended_at = Some("2026-09-10T12:00:00Z".into());
        AgentRecords::new(&path).upsert(r.clone());

        let read = AgentRecords::new(&path).load();
        assert_eq!(read.len(), 1);
        r.options.api_key = None;
        assert_eq!(read[0], r);
    }

    #[test]
    fn a_file_that_will_not_parse_is_kept_aside_and_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(&path, "{ not json at all").unwrap();
        let records = AgentRecords::new(&path);
        assert!(records.load().is_empty());
        assert!(
            dir.path().join("agents.json.corrupt").exists(),
            "the unreadable file must be kept, not overwritten"
        );
        // And the daemon carries on writing new records.
        records.upsert(record("ag_1", "ws_1"));
        assert_eq!(records.load().len(), 1);
    }

    /// A file this daemon cannot read holds records it must not destroy.
    ///
    /// Unix only, and only as a non-root user: `chmod 000` is what makes the
    /// *read* fail while the directory still permits the rename, which is
    /// exactly the shape that turns "treat it as empty" into data loss. Root
    /// ignores the mode, so there is nothing to test there.
    #[cfg(unix)]
    #[test]
    fn a_records_file_that_cannot_be_read_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let records = AgentRecords::new(&path);
        records.upsert(record("ag_1", "ws_1"));
        records.upsert(record("ag_2", "ws_1"));
        let before = std::fs::read_to_string(&path).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            eprintln!("SKIP: running as root, the mode is not enforced");
            return;
        }

        // Every mutating path, because each one of them rewrites the whole file.
        records.upsert(record("ag_3", "ws_1"));
        records.update(&"ag_1".into(), |r| r.session_id = Some("lost".into()));
        records.remove(&"ag_2".into());
        assert!(records.remove_workspace(&"ws_1".into()).is_empty());
        records.replace_all(vec![record("ag_9", "ws_9")]);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed read must never become an overwrite"
        );
        let all = records.load();
        assert_eq!(
            all.iter().map(|r| r.agent_id.as_str()).collect::<Vec<_>>(),
            vec!["ag_1", "ag_2"]
        );
    }

    /// The same rule stated without needing a mode: when the read fails, the
    /// write is not attempted at all, so not even a temporary appears.
    ///
    /// A directory where the file belongs is a read error on every platform and
    /// is not `NotFound`, which is the distinction that matters. Portable, so
    /// this half of the rule is covered on Windows too.
    #[test]
    fn a_read_failure_does_not_even_start_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("in-the-way"), "not a records file").unwrap();

        let records = AgentRecords::new(&path);
        assert!(records.load().is_empty());
        records.upsert(record("ag_1", "ws_1"));

        assert!(
            !dir.path().join("agents.json.tmp").exists(),
            "the write must not have been attempted"
        );
        assert!(
            path.join("in-the-way").exists(),
            "and what was there is untouched"
        );
    }

    /// A corrupt file that cannot be moved aside is unreadable, not empty.
    ///
    /// Moving it aside is what makes writing a fresh file safe: the old content
    /// still exists under the new name. With the rename blocked there is no
    /// copy, so the same rule applies as for a read that failed outright.
    #[test]
    fn a_corrupt_file_that_cannot_be_kept_aside_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(&path, "{ not json at all").unwrap();
        // A non-empty directory is refused as a rename destination everywhere.
        let aside = dir.path().join("agents.json.corrupt");
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("occupied"), "x").unwrap();

        let records = AgentRecords::new(&path);
        records.upsert(record("ag_1", "ws_1"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json at all",
            "the evidence must survive"
        );
        assert!(!dir.path().join("agents.json.tmp").exists());
    }

    /// A leftover at the one name a fixed sibling temporary would use must not
    /// wedge the writer for good.
    ///
    /// A daemon killed mid-write leaves its temporary behind, and a records
    /// file is rewritten on every agent start, session id and exit. A directory
    /// at that name is the leftover no write can clear on its own: a writer
    /// that reuses one fixed name can never record another agent, while one
    /// that picks a unique name per call carries on.
    #[test]
    fn a_leftover_at_the_old_fixed_temporary_name_does_not_wedge_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let squatter = dir.path().join("agents.json.tmp");
        std::fs::create_dir(&squatter).unwrap();

        let records = AgentRecords::new(&path);
        records.upsert(record("ag_1", "ws_1"));
        records.upsert(record("ag_2", "ws_1"));
        assert_eq!(
            records
                .load()
                .iter()
                .map(|r| r.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ag_1", "ag_2"]
        );
        assert!(squatter.is_dir(), "what was in the way is untouched");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "agents.json" && n != "agents.json.tmp")
            .collect();
        assert!(left.is_empty(), "a temporary was left behind: {left:?}");
    }

    /// The tmp file is never left where a later `load` could pick it up, and
    /// the real file is only ever a whole document.
    #[test]
    fn writing_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let records = AgentRecords::new(dir.path().join("agents.json"));
        records.upsert(record("ag_1", "ws_1"));
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["agents.json".to_string()], "{left:?}");
    }
}
