//! What the Run panel knows about one workspace: the configurations the daemon
//! detected, which of them is selected, the runs that exist, their output, and
//! the hosts the proxy has blocked.
//!
//! `RunPanelModel` is a thin shell over [`WorkspaceRuns`]: every decision the
//! panel paints -- which run is the active one, whether Start may be offered,
//! which host the toast is showing -- is made here, where it can be tested
//! without a Qt event loop.
//!
//! This module must never import Qt types.

use bondsymphonic_proto::{RunConfig, RunInfo, RunState};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// A run's output, kept as a ring buffer.
///
/// A dev server left running for an afternoon prints an unbounded number of
/// lines and the user only ever reads the tail, so the oldest line is dropped
/// once [`RunLog::CAP`] is reached. The C++ log widget is capped at the same
/// number of blocks, so the two views agree about what exists.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RunLog {
    lines: VecDeque<String>,
}

impl RunLog {
    /// Lines kept per run. IDE spec §2: "output log (ring buffer 2000 lines
    /// per run in Rust)".
    pub const CAP: usize = 2000;

    /// Appends one line, dropping the oldest if the buffer is full.
    pub fn push(&mut self, line: impl Into<String>) {
        if self.lines.len() == Self::CAP {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    /// Every line the buffer still holds, joined by newlines. This is what the
    /// panel installs when it switches to another run; live lines after that
    /// are appended one by one.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// One run as the panel shows it: the daemon's [`RunInfo`] plus the detail that
/// only arrives with a `run.state` event.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunView {
    pub run_id: String,
    pub config_name: String,
    /// `starting`, `ready`, `stopped` or `failed`; see [`run_state_word`].
    pub state: String,
    pub host_port: u16,
    pub url: String,
    /// Why the run reached its current state, or empty. The exit status behind
    /// a `failed`, typically.
    pub detail: String,
}

impl RunView {
    /// Whether this run is one the panel offers Stop for: it exists and has
    /// not finished.
    pub fn is_live(&self) -> bool {
        self.state == run_state_word(RunState::Starting)
            || self.state == run_state_word(RunState::Ready)
    }
}

/// The daemon's snake_case spelling of a run state. It crosses into C++ as the
/// `activeState` property and inside `runsJson`, so the word is defined once.
pub fn run_state_word(state: RunState) -> &'static str {
    match state {
        RunState::Starting => "starting",
        RunState::Ready => "ready",
        RunState::Stopped => "stopped",
        RunState::Failed => "failed",
    }
}

/// Inverse of [`run_state_word`]. `None` for anything else, so a caller decides
/// what an unrecognised word means rather than being handed a guess.
pub fn parse_run_state(word: &str) -> Option<RunState> {
    match word {
        "starting" => Some(RunState::Starting),
        "ready" => Some(RunState::Ready),
        "stopped" => Some(RunState::Stopped),
        "failed" => Some(RunState::Failed),
        _ => None,
    }
}

/// Which workspace an answer to a denial toast belongs to.
///
/// The toast shows a host and no workspace, so `allowHost`/`dismissDenied`
/// carry only the host and something has to decide whose allowlist is being
/// extended. Answering for whichever workspace happens to be on screen is
/// wrong: a user who switches tabs while a toast is up would silently add the
/// host to a workspace that never asked for it, which is the exact failure the
/// allowlist exists to prevent.
///
/// `shown` is the `(workspace, host)` of the toast actually on screen, and
/// wins whenever it is about this host. `fallback` is for a caller that never
/// saw a toast (the smoke script answers a denial programmatically); it must
/// already have been checked to have `host` in its own queue, so this can never
/// name a workspace that did not report the denial.
pub fn denial_owner(
    shown: Option<(&str, &str)>,
    fallback: Option<&str>,
    host: &str,
) -> Option<String> {
    if let Some((workspace, shown_host)) = shown {
        if shown_host == host {
            return Some(workspace.to_owned());
        }
    }
    fallback.map(str::to_owned)
}

/// Everything the Run panel holds for one workspace.
#[derive(Debug, Default)]
pub struct WorkspaceRuns {
    /// What `repo.detect_run_configs` last answered for this worktree.
    pub configs: Vec<RunConfig>,
    /// The name in the combo, or `None` when nothing runnable was detected.
    pub selected: Option<String>,
    /// Reserved for the editable port of Milestone 6. Nothing writes it yet:
    /// `run.start` names a configuration and the daemon uses that
    /// configuration's port, so a per-start override would be a number the
    /// daemon never sees.
    pub port_override: Option<u16>,
    /// The runs the daemon reports for this workspace, in its order.
    pub runs: Vec<RunView>,
    /// Output per run id. Pruned by [`WorkspaceRuns::apply_list`] so a
    /// long-lived workspace cannot accumulate the logs of runs that are gone.
    pub logs: BTreeMap<String, RunLog>,
    /// Hosts the proxy blocked and the user has not answered for yet, oldest
    /// first. The head is the one the toast is showing.
    pub denied_hosts: Vec<String>,
}

impl WorkspaceRuns {
    /// Installs a freshly detected list, keeping the user's selection if it is
    /// still there and still runnable, and otherwise selecting the first
    /// configuration that can actually be started.
    ///
    /// Detection runs again on every tab switch, so this is called far more
    /// often than the list changes: moving the selection when nothing moved
    /// would take the combo out from under the user mid-click.
    pub fn set_configs(&mut self, configs: Vec<RunConfig>) {
        self.configs = configs;
        let kept = self
            .selected
            .as_ref()
            .filter(|name| self.runnable(name))
            .cloned();
        self.selected = kept.or_else(|| {
            self.configs
                .iter()
                .find(|c| c.disabled_reason.is_none())
                .map(|c| c.name.clone())
        });
    }

    /// Points the combo at `name`. The empty name detaches the selection.
    /// Returns whether the selection changed: an unknown name, and one the
    /// daemon marked disabled, are both refused, because Start is offered for
    /// whatever is selected and the daemon would only reject it.
    pub fn select(&mut self, name: &str) -> bool {
        let next = if name.is_empty() {
            None
        } else if self.runnable(name) {
            Some(name.to_owned())
        } else {
            tracing::debug!("run config {name:?} is not selectable");
            return false;
        };
        if self.selected == next {
            return false;
        }
        self.selected = next;
        true
    }

    /// The selected configuration, or `None`.
    pub fn selected_config(&self) -> Option<&RunConfig> {
        let name = self.selected.as_ref()?;
        self.configs.iter().find(|c| &c.name == name)
    }

    /// Replaces the run list from `run.list`. Detail already known for a run
    /// the daemon still reports is kept -- `RunInfo` does not carry it -- and
    /// the logs of runs that are gone are dropped with them.
    pub fn apply_list(&mut self, runs: Vec<RunInfo>) {
        self.runs = runs
            .into_iter()
            .map(|info| {
                let id = info.run_id.to_string();
                let detail = self
                    .runs
                    .iter()
                    .find(|r| r.run_id == id)
                    .map(|r| r.detail.clone())
                    .unwrap_or_default();
                RunView {
                    run_id: id,
                    config_name: info.config_name,
                    state: run_state_word(info.state).to_owned(),
                    host_port: info.host_port,
                    url: info.url,
                    detail,
                }
            })
            .collect();
        self.logs
            .retain(|id, _| self.runs.iter().any(|r| &r.run_id == id));
    }

    /// Records a `run.state` event. Returns whether anything changed, so the
    /// panel repaints only when it has to.
    ///
    /// A `None` url leaves the one already known in place: the daemon sends the
    /// url with the transition to ready and need not repeat it, and blanking it
    /// would take "Open" away from a run that is still serving.
    ///
    /// A `None` detail does the opposite and clears it. The two are not the
    /// same kind of fact: a url outlives the transition that announced it,
    /// while the detail explains the state the run is in *now*, so leaving the
    /// exit status of a failed run attached to a later `starting` would show
    /// the user an error about a run that is coming back up.
    pub fn apply_state(
        &mut self,
        run_id: &str,
        state: RunState,
        url: Option<&str>,
        detail: Option<&str>,
    ) -> bool {
        let Some(view) = self.runs.iter_mut().find(|r| r.run_id == run_id) else {
            // A run started by another client, or one already relisted away.
            // There is nothing to update and inventing a view would put a run
            // with no configuration name into the panel.
            return false;
        };
        let mut changed = false;
        let word = run_state_word(state);
        if view.state != word {
            view.state = word.to_owned();
            changed = true;
        }
        if let Some(url) = url.filter(|u| !u.is_empty()) {
            if view.url != url {
                view.url = url.to_owned();
                changed = true;
            }
        }
        let detail = detail.unwrap_or_default();
        if view.detail != detail {
            view.detail = detail.to_owned();
            changed = true;
        }
        changed
    }

    /// Appends one `run.output` line to that run's log.
    pub fn apply_output(&mut self, run_id: &str, line: impl Into<String>) {
        self.logs.entry(run_id.to_owned()).or_default().push(line);
    }

    /// One run's whole log, or empty for a run with no output (or none at all).
    pub fn log_text(&self, run_id: &str) -> String {
        self.logs.get(run_id).map(RunLog::text).unwrap_or_default()
    }

    /// The run the panel is showing: the live run of the selected
    /// configuration. A stopped run is history, and another configuration's run
    /// belongs to another entry in the combo.
    pub fn active_run(&self) -> Option<&RunView> {
        let name = self.selected.as_ref()?;
        self.runs
            .iter()
            .rev()
            .find(|r| &r.config_name == name && r.is_live())
    }

    /// Queues a blocked host. Returns whether it is new: a page that fetches
    /// the same blocked host forty times must produce one toast, not forty.
    pub fn note_denied(&mut self, host: &str) -> bool {
        if self.denied_hosts.iter().any(|h| h == host) {
            return false;
        }
        self.denied_hosts.push(host.to_owned());
        true
    }

    /// Removes a host from the queue, whether it was allowed or dismissed.
    /// Returns whether it was there.
    pub fn clear_denied(&mut self, host: &str) -> bool {
        let before = self.denied_hosts.len();
        self.denied_hosts.retain(|h| h != host);
        self.denied_hosts.len() != before
    }

    /// The host the toast should be showing, or `None` when the queue is empty.
    pub fn current_denial(&self) -> Option<&str> {
        self.denied_hosts.first().map(String::as_str)
    }

    /// The detected configurations, for the combo.
    pub fn configs_json(&self) -> String {
        serde_json::to_string(&self.configs).unwrap_or_else(|_| "[]".to_owned())
    }

    /// The selected configuration, or the JSON literal `null`. The panel reads
    /// `port` and `port_guessed` out of it for the port hint.
    pub fn selected_config_json(&self) -> String {
        serde_json::to_string(&self.selected_config()).unwrap_or_else(|_| "null".to_owned())
    }

    /// Every run of this workspace, as a JSON array of [`RunView`].
    pub fn runs_json(&self) -> String {
        serde_json::to_string(&self.runs).unwrap_or_else(|_| "[]".to_owned())
    }

    /// Whether `name` is a configuration that exists and can be started.
    fn runnable(&self, name: &str) -> bool {
        self.configs
            .iter()
            .any(|c| c.name == name && c.disabled_reason.is_none())
    }
}
