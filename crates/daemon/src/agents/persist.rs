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
}

impl AgentRecords {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every record, oldest first.
    ///
    /// A missing file is the normal case for a daemon that has never started an
    /// agent, and reads as empty. A file that will not parse is *not* a reason
    /// to refuse to start: it is moved aside as `<name>.corrupt` and reported,
    /// so the agents are lost but the daemon, its workspaces and their
    /// transcripts are not — and the next write does not silently overwrite the
    /// evidence.
    pub fn load(&self) -> Vec<AgentRecord> {
        let _guard = self.lock.lock();
        self.load_locked()
    }

    fn load_locked(&self) -> Vec<AgentRecord> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "could not read the agent records");
                return Vec::new();
            }
        };
        match serde_json::from_str::<FileFormat>(&text) {
            Ok(f) => f.agents,
            Err(e) => {
                let aside = self.path.with_extension("json.corrupt");
                warn!(
                    path = %self.path.display(),
                    kept = %aside.display(),
                    error = %e,
                    "the agent records will not parse; keeping them aside and starting with none"
                );
                if let Err(e) = std::fs::rename(&self.path, &aside) {
                    warn!(path = %self.path.display(), error = %e, "could not move the unreadable records aside");
                }
                Vec::new()
            }
        }
    }

    /// Adds `record`, or replaces the one with the same agent id.
    ///
    /// The API key is dropped here, whatever the caller passed.
    pub fn upsert(&self, mut record: AgentRecord) {
        record.options.api_key = None;
        let _guard = self.lock.lock();
        let mut all = self.load_locked();
        match all.iter_mut().find(|r| r.agent_id == record.agent_id) {
            Some(existing) => *existing = record,
            None => all.push(record),
        }
        self.save_locked(&all);
    }

    /// Applies `f` to the record for `agent`, if there is one. A record that is
    /// not there is not an error: the file may have been lost, and an agent
    /// whose record went missing must still be able to run.
    pub fn update(&self, agent: &AgentId, f: impl FnOnce(&mut AgentRecord)) {
        let _guard = self.lock.lock();
        let mut all = self.load_locked();
        let Some(record) = all.iter_mut().find(|r| &r.agent_id == agent) else {
            return;
        };
        f(record);
        record.options.api_key = None;
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
        self.save_locked(&records);
    }

    /// Forgets every record belonging to `ws`, answering the ids removed so the
    /// caller can take their transcripts with them.
    pub fn remove_workspace(&self, ws: &WorkspaceId) -> Vec<AgentId> {
        let _guard = self.lock.lock();
        let mut all = self.load_locked();
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

    /// Writes the file: a temporary beside it, then a rename over it, so a
    /// daemon killed mid-write leaves either the old file or the new one and
    /// never half of either.
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
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = FileFormat {
            version: FORMAT_VERSION,
            agents: records.to_vec(),
        };
        std::fs::write(&tmp, serde_json::to_vec_pretty(&data)?)?;
        std::fs::rename(&tmp, &self.path)
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
