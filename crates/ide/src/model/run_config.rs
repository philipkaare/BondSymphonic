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

/// Whether a blocked host may be queued for `workspace`.
///
/// `destroyed` is whether the panel has been told that workspace is *gone* --
/// that `forget_workspace` ran for it. The proxy's report of a fetch the run
/// made on its way down arrives behind that, and creating an entry to hold the
/// denial brought the workspace back: a toast raised for a workspace that no
/// longer exists, which `allowHost` would then answer by editing the allowlist
/// of a workspace the daemon has already thrown away.
///
/// Only that. "Destroyed" is not the same as "the panel is holding nothing for
/// it yet", and reading the absence of an entry as a denial to drop was a bug of
/// its own: entries are created by `setWorkspace`, which the window calls for
/// the tab that just became active, so in a restored session every tab but one
/// has no entry. Those denials are exactly the ones the per-workspace queue
/// exists for -- a host blocked behind another tab waits until that workspace is
/// shown -- and dropping them meant the user was never told at all.
///
/// An empty workspace or host is refused for the same reason `noteDenied` is
/// worth guarding at all: the toast has to name something a user can answer for.
pub fn may_queue_denial(destroyed: bool, workspace: &str, host: &str) -> bool {
    !destroyed && !workspace.is_empty() && !host.is_empty()
}

/// One finished round of detection, as the panel applies it.
///
/// Built by `detection_to_apply` and consumed by
/// [`WorkspaceRuns::apply_detection`]; it exists so that "what a detection round
/// does to a workspace" is one decision in one place rather than something the
/// QObject spells out between two `rust_mut()` borrows.
#[derive(Debug, Default, PartialEq)]
pub struct Detection {
    pub configs: Vec<RunConfig>,
    pub warnings: Vec<String>,
    pub runs: Vec<RunInfo>,
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
    /// What the daemon had to complain about in this worktree's
    /// `bondsymphonic.toml`, from the same `repo.detect_run_configs` answer the
    /// configurations came in. One finished sentence per complaint, each
    /// already naming the file.
    ///
    /// The daemon used to discard the whole file over one bad entry; it now
    /// loads the rest and says what it dropped. An entry that is not in the
    /// combo and was not complained about leaves the user staring at a file
    /// they believe is correct, so the panel shows these rather than logging
    /// them.
    ///
    /// Not a count of dropped `[[run]]` entries: a file that will not parse at
    /// all is one warning however many runs it declared.
    pub warnings: Vec<String>,
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

    /// Installs the warnings from the same detection `set_configs` took its
    /// list from. Replaces rather than appends: one detection is the whole
    /// answer for a worktree, so a file the user has since fixed stops
    /// complaining.
    pub fn set_warnings(&mut self, warnings: Vec<String>) {
        self.warnings = warnings;
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

    /// Applies one finished round of detection, and answers with the run ids
    /// the panel must be following afterwards.
    ///
    /// `None` in means `run.list` did not answer, and `None` out means the
    /// panel changes nothing: not the runs, not their logs, and not the
    /// subscriptions, because the caller's stale-run sweep is driven by the ids
    /// this returns. A failed list says nothing at all about what is running,
    /// and answering that silence with an empty list is what made the panel
    /// destroy its own state -- every run off the screen, their logs dropped
    /// with them, their subscriptions ended -- for a workspace whose dev server
    /// was still serving and still printing. A connection that drops
    /// mid-request is exactly when that happens.
    ///
    /// A detection that failed is not the same kind of failure and does not
    /// reach here as `None`: it arrives as empty configurations, because a
    /// worktree with nothing runnable in it is an ordinary state of the panel
    /// and an empty combo is a true thing to paint.
    pub fn apply_detection(&mut self, detection: Option<Detection>) -> Option<Vec<String>> {
        let Detection {
            configs,
            warnings,
            runs,
        } = detection?;
        let ids = runs.iter().map(|r| r.run_id.to_string()).collect();
        self.set_configs(configs);
        self.set_warnings(warnings);
        self.apply_list(runs);
        Some(ids)
    }

    /// Replaces the run list from `run.list`. Detail already known for a run
    /// the daemon still reports is kept -- `RunInfo` does not carry it -- and
    /// the logs of runs that are gone are dropped with them.
    ///
    /// One run the daemon did *not* report survives: the last finished run of a
    /// configuration the list has nothing for. `run.list` answers with the runs
    /// the daemon still holds, and a run that exited is not one of them, while
    /// detection runs again on every tab switch -- so taking the list as the
    /// whole truth made the run the user had just stopped, and the output saying
    /// why it failed, vanish the moment they looked at another tab and came
    /// back. It is kept until a newer run for that configuration appears, in the
    /// list or from [`WorkspaceRuns::record_started`], which bounds this at one
    /// extra row per configuration rather than one per run ever started.
    pub fn apply_list(&mut self, runs: Vec<RunInfo>) {
        let listed: Vec<RunView> = runs
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
        // A run still alive that the daemon has stopped reporting is gone for
        // real -- a daemon restart loses every run -- so only finished ones are
        // carried over, and only where the daemon's answer says nothing about
        // that configuration at all.
        let mut kept: Vec<RunView> = Vec::new();
        for view in std::mem::take(&mut self.runs) {
            let superseded =
                view.is_live() || listed.iter().any(|r| r.config_name == view.config_name);
            if superseded {
                continue;
            }
            kept.retain(|r| r.config_name != view.config_name);
            kept.push(view);
        }
        kept.extend(listed);
        self.runs = kept;
        self.prune_logs();
    }

    /// Records the run `run.start` has just created, as `starting`.
    ///
    /// Any finished run of the same configuration goes: it was kept by
    /// [`apply_list`](WorkspaceRuns::apply_list) only until the configuration
    /// was started again, and the row the panel shows for a configuration is
    /// about the run that is happening now.
    pub fn record_started(
        &mut self,
        run_id: String,
        config_name: String,
        host_port: u16,
        url: String,
    ) {
        self.runs
            .retain(|r| r.run_id != run_id && (r.is_live() || r.config_name != config_name));
        self.runs.push(RunView {
            run_id,
            config_name,
            // The daemon publishes `starting` as well; recording it here means
            // Stop is offered from the moment the call answers rather than from
            // whenever that event arrives.
            state: run_state_word(RunState::Starting).to_owned(),
            host_port,
            url,
            detail: String::new(),
        });
        self.prune_logs();
    }

    /// Drops the output of every run the list no longer holds, so a long-lived
    /// workspace cannot accumulate the logs of runs that are gone.
    fn prune_logs(&mut self) {
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

    /// The most blocked hosts one workspace queues at a time.
    ///
    /// The queue is answered one toast at a time by a person, so anything past
    /// a couple of dozen is a queue nobody will ever reach the end of. A loop
    /// in the sandbox asking for `a1.evil`, `a2.evil`, ... would otherwise grow
    /// this without bound and make the duplicate scan quadratic while it did.
    /// The daemon coalesces repeats of one host; this is the bound on
    /// *distinct* ones.
    pub const MAX_DENIED: usize = 32;

    /// Queues a blocked host. Returns whether it is new: a page that fetches
    /// the same blocked host forty times must produce one toast, not forty.
    ///
    /// At [`WorkspaceRuns::MAX_DENIED`] the oldest is dropped. The newest host
    /// is the one the user is most likely to be looking at the consequences of,
    /// and an unanswerable queue is worse than a short one.
    pub fn note_denied(&mut self, host: &str) -> bool {
        if self.denied_hosts.iter().any(|h| h == host) {
            return false;
        }
        self.denied_hosts.push(host.to_owned());
        while self.denied_hosts.len() > Self::MAX_DENIED {
            self.denied_hosts.remove(0);
        }
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
