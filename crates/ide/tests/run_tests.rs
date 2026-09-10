//! The run model: the ring buffer behind the output pane, the rules that
//! decide which configuration is selected, and the fold that turns `run.*`
//! events into what the Run panel paints.
//!
//! Everything here is pure Rust. `RunPanelModel` is a shell over
//! [`WorkspaceRuns`] exactly as `ChangesModel` is over the changed-file list,
//! so the decisions the panel depends on -- which run is the active one, which
//! host the toast is showing, what the JSON the combo reads looks like -- are
//! settled without a Qt event loop.

use bondsymphonic_ide::model::run_config::{
    denial_owner, parse_run_state, run_state_word, RunLog, RunView, WorkspaceRuns,
};
use bondsymphonic_ide::qobjects::run_panel::needs_resubscribe;
use bondsymphonic_proto::{RunConfig, RunConfigSource, RunId, RunInfo, RunState};
use std::collections::BTreeMap;

fn config(name: &str, port: u16) -> RunConfig {
    RunConfig {
        name: name.to_owned(),
        command: format!("run {name}"),
        port,
        cwd: None,
        env: BTreeMap::new(),
        ready_regex: None,
        source: RunConfigSource::Detected,
        port_guessed: true,
        disabled_reason: None,
    }
}

fn disabled(name: &str, reason: &str) -> RunConfig {
    RunConfig {
        disabled_reason: Some(reason.to_owned()),
        ..config(name, 0)
    }
}

fn info(run: &str, config_name: &str, state: RunState, port: u16) -> RunInfo {
    RunInfo {
        run_id: RunId(run.to_owned()),
        config_name: config_name.to_owned(),
        state,
        host_port: port,
        url: format!("http://localhost:{port}"),
    }
}

/// The output pane is a ring buffer, not a transcript: a dev server that has
/// been logging requests for an hour must not grow the IDE's memory without
/// bound, and what the user wants to see is the tail anyway.
#[test]
fn the_log_keeps_the_last_lines_and_joins_them_with_newlines() {
    let mut log = RunLog::default();
    assert_eq!(log.len(), 0);
    assert!(log.is_empty());
    assert_eq!(log.text(), "");

    log.push("first");
    log.push("second");
    assert_eq!(log.len(), 2);
    assert_eq!(log.text(), "first\nsecond");

    for n in 0..RunLog::CAP {
        log.push(format!("line {n}"));
    }
    assert_eq!(log.len(), RunLog::CAP, "the cap is a hard limit");
    let text = log.text();
    assert!(
        !text.contains("first"),
        "the oldest lines are dropped first"
    );
    assert!(
        text.starts_with("line 0\n"),
        "{}",
        &text[..40.min(text.len())]
    );
    assert!(text.ends_with(&format!("line {}", RunLog::CAP - 1)));
}

/// Detection runs again on every tab switch. Re-detecting must not move the
/// user's selection out from under them, and a configuration the daemon says
/// cannot run (docker compose) must never be picked for them.
#[test]
fn set_configs_keeps_a_live_selection_and_never_auto_selects_a_disabled_one() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![
        disabled("compose", "Docker is not available inside the sandbox"),
        config("dev", 5173),
        config("api", 8080),
    ]);
    assert_eq!(
        runs.selected.as_deref(),
        Some("dev"),
        "the first runnable config is selected, not the disabled one"
    );

    runs.select("api");
    assert_eq!(runs.selected.as_deref(), Some("api"));

    // Re-detecting the same repo keeps the choice.
    runs.set_configs(vec![config("dev", 5173), config("api", 8080)]);
    assert_eq!(runs.selected.as_deref(), Some("api"));

    // The chosen config disappearing falls back to the first runnable one.
    runs.set_configs(vec![disabled("compose", "no docker"), config("dev", 5173)]);
    assert_eq!(runs.selected.as_deref(), Some("dev"));

    // Nothing runnable at all leaves nothing selected, so Start stays off.
    runs.set_configs(vec![disabled("compose", "no docker")]);
    assert_eq!(runs.selected, None);
    assert_eq!(runs.selected_config(), None);

    runs.set_configs(vec![]);
    assert_eq!(runs.selected, None);
}

/// `select` is what the combo calls. It is the one place that can put a name
/// into `selected`, so it refuses names that would leave Start pointing at a
/// configuration the daemon would reject.
#[test]
fn select_refuses_an_unknown_or_disabled_config_and_an_empty_name_clears() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![config("dev", 5173), disabled("compose", "no docker")]);

    assert!(!runs.select("nope"), "an unknown name is not a selection");
    assert_eq!(runs.selected.as_deref(), Some("dev"));

    assert!(!runs.select("compose"), "a disabled config cannot be run");
    assert_eq!(runs.selected.as_deref(), Some("dev"));

    assert!(
        !runs.select("dev"),
        "selecting what is selected changes nothing"
    );

    runs.set_configs(vec![config("dev", 5173), config("api", 8080)]);
    assert!(runs.select("api"));
    assert_eq!(runs.selected_config().map(|c| c.port), Some(8080));

    assert!(runs.select(""), "the empty name detaches the selection");
    assert_eq!(runs.selected, None);
}

/// `run.list` is the daemon's answer about what is running now; `run.state` is
/// how each of those moves afterwards.
#[test]
fn apply_list_builds_views_and_apply_state_moves_them() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![config("dev", 5173)]);
    runs.apply_list(vec![info("run_1", "dev", RunState::Starting, 41873)]);
    assert_eq!(
        runs.runs,
        vec![RunView {
            run_id: "run_1".to_owned(),
            config_name: "dev".to_owned(),
            state: "starting".to_owned(),
            host_port: 41873,
            url: "http://localhost:41873".to_owned(),
            detail: String::new(),
        }]
    );

    assert!(
        runs.apply_state(
            "run_1",
            RunState::Ready,
            Some("http://localhost:41873"),
            None
        ),
        "a state change is a change"
    );
    assert_eq!(runs.runs[0].state, "ready");
    assert!(
        !runs.apply_state("run_1", RunState::Ready, None, None),
        "the same state again is not"
    );
    assert!(
        !runs.apply_state("run_9", RunState::Ready, None, None),
        "a run this workspace does not know about is ignored"
    );

    assert!(runs.apply_state("run_1", RunState::Failed, None, Some("exit code 1")));
    assert_eq!(runs.runs[0].state, "failed");
    assert_eq!(runs.runs[0].detail, "exit code 1");

    // A URL the daemon does not repeat is kept rather than blanked: the panel
    // still has to offer "Open" for the run that just went ready. The detail is
    // the opposite -- it describes the state the run is in now, so a transition
    // that explains nothing leaves nothing behind.
    assert!(runs.apply_state("run_1", RunState::Ready, None, None));
    assert_eq!(runs.runs[0].url, "http://localhost:41873");
    assert_eq!(
        runs.runs[0].detail, "",
        "the failure text does not outlive it"
    );

    // A relist keeps the detail of a run the daemon still reports: `RunInfo`
    // does not carry it, so re-reading the list must not erase it.
    runs.apply_state("run_1", RunState::Failed, None, Some("exit code 2"));
    runs.apply_list(vec![info("run_1", "dev", RunState::Failed, 41873)]);
    assert_eq!(runs.runs[0].detail, "exit code 2");

    // A relist drops the runs the daemon no longer has, and their logs with
    // them.
    runs.apply_output("run_1", "hello");
    runs.apply_list(vec![]);
    assert!(runs.runs.is_empty());
    assert!(runs.logs.is_empty(), "an unknown run's log is not kept");
}

/// The panel shows one run: the one belonging to the configuration in the
/// combo that is actually alive. A stopped run is history, and another
/// configuration's run belongs to another entry in the combo.
#[test]
fn active_run_follows_the_selected_config_and_only_live_states() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![config("dev", 5173), config("api", 8080)]);
    runs.apply_list(vec![
        info("run_dev", "dev", RunState::Ready, 41873),
        info("run_api", "api", RunState::Ready, 41874),
    ]);

    assert_eq!(runs.selected.as_deref(), Some("dev"));
    assert_eq!(
        runs.active_run().map(|r| r.run_id.as_str()),
        Some("run_dev")
    );

    runs.select("api");
    assert_eq!(
        runs.active_run().map(|r| r.run_id.as_str()),
        Some("run_api")
    );

    runs.apply_state("run_api", RunState::Stopped, None, None);
    assert_eq!(
        runs.active_run(),
        None,
        "a stopped run is not the active one"
    );

    runs.apply_state("run_api", RunState::Starting, None, None);
    assert_eq!(
        runs.active_run().map(|r| r.state.as_str()),
        Some("starting"),
        "a starting run is active: Stop has to be offered while it comes up"
    );

    runs.select("");
    assert_eq!(runs.active_run(), None, "nothing selected, nothing active");
}

#[test]
fn apply_output_goes_to_the_log_of_the_run_that_produced_it() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![config("dev", 5173), config("api", 8080)]);
    runs.apply_list(vec![
        info("run_dev", "dev", RunState::Ready, 1),
        info("run_api", "api", RunState::Ready, 2),
    ]);
    runs.apply_output("run_dev", "vite ready");
    runs.apply_output("run_api", "listening on 8080");
    runs.apply_output("run_dev", "GET /");

    assert_eq!(runs.log_text("run_dev"), "vite ready\nGET /");
    assert_eq!(runs.log_text("run_api"), "listening on 8080");
    assert_eq!(runs.log_text("run_none"), "");
}

/// The toast shows one host at a time. A page that fetches the same blocked
/// host forty times must produce one toast, and dismissing it must bring up
/// whatever was blocked behind it.
#[test]
fn denied_hosts_are_a_queue_that_de_dups_and_advances() {
    let mut runs = WorkspaceRuns::default();
    assert!(runs.note_denied("example.com"), "the first sighting is new");
    assert!(
        !runs.note_denied("example.com"),
        "the same host again is not"
    );
    assert!(runs.note_denied("cdn.example.net"));
    assert_eq!(runs.denied_hosts, vec!["example.com", "cdn.example.net"]);
    assert_eq!(runs.current_denial(), Some("example.com"));

    assert!(runs.clear_denied("example.com"));
    assert_eq!(runs.current_denial(), Some("cdn.example.net"));
    assert!(
        !runs.clear_denied("example.com"),
        "clearing what is gone changes nothing"
    );

    assert!(runs.clear_denied("cdn.example.net"));
    assert_eq!(runs.current_denial(), None);
    assert!(runs.denied_hosts.is_empty());
}

/// A loop in the sandbox naming a new host every time must not grow the queue
/// without bound: the user answers these one toast at a time, so a queue past a
/// couple of dozen is one nobody will ever reach the end of.
#[test]
fn the_denial_queue_is_bounded_and_drops_the_oldest() {
    let mut runs = WorkspaceRuns::default();
    let cap = WorkspaceRuns::MAX_DENIED;
    for i in 0..cap {
        assert!(runs.note_denied(&format!("h{i}.evil")));
    }
    assert_eq!(runs.denied_hosts.len(), cap);
    assert_eq!(runs.current_denial(), Some("h0.evil"));

    // One past the cap: the queue stays the same length and the oldest goes.
    assert!(runs.note_denied("newest.evil"));
    assert_eq!(runs.denied_hosts.len(), cap);
    assert_eq!(runs.current_denial(), Some("h1.evil"));
    assert_eq!(
        runs.denied_hosts.last().map(String::as_str),
        Some("newest.evil")
    );

    for i in 0..1000 {
        runs.note_denied(&format!("flood{i}.evil"));
    }
    assert_eq!(runs.denied_hosts.len(), cap);
    assert_eq!(
        runs.denied_hosts.last().map(String::as_str),
        Some("flood999.evil")
    );
    // Nothing that was pushed out is still claimed to be queued.
    assert!(!runs.clear_denied("h0.evil"));
}

/// The three JSON accessors are the whole contract with the C++ panel: the
/// combo, the run list and the port hint are read out of these strings.
#[test]
fn the_json_the_panel_reads_carries_what_it_paints() {
    let mut runs = WorkspaceRuns::default();
    runs.set_configs(vec![
        config("dev", 5173),
        disabled("compose", "Docker is not available inside the sandbox"),
    ]);
    runs.apply_list(vec![info("run_1", "dev", RunState::Ready, 41873)]);

    let configs: serde_json::Value = serde_json::from_str(&runs.configs_json()).unwrap();
    assert_eq!(configs.as_array().unwrap().len(), 2);
    assert_eq!(configs[0]["name"], "dev");
    assert_eq!(configs[0]["port"], 5173);
    assert_eq!(configs[0]["port_guessed"], true);
    assert_eq!(
        configs[1]["disabled_reason"],
        "Docker is not available inside the sandbox"
    );

    let selected: serde_json::Value = serde_json::from_str(&runs.selected_config_json()).unwrap();
    assert_eq!(selected["name"], "dev");
    assert_eq!(selected["command"], "run dev");

    let listed: serde_json::Value = serde_json::from_str(&runs.runs_json()).unwrap();
    assert_eq!(listed[0]["run_id"], "run_1");
    assert_eq!(listed[0]["config_name"], "dev");
    assert_eq!(listed[0]["state"], "ready");
    assert_eq!(listed[0]["url"], "http://localhost:41873");

    // Nothing selected still answers with parseable JSON, not an empty string
    // the panel would have to special-case.
    runs.set_configs(vec![disabled("compose", "no docker")]);
    assert_eq!(runs.selected_config_json(), "null");
}

/// The state word crosses the boundary into C++ and comes back out of the JSON
/// above, so the two directions are defined once and tested together.
#[test]
fn run_state_words_round_trip() {
    for state in [
        RunState::Starting,
        RunState::Ready,
        RunState::Stopped,
        RunState::Failed,
    ] {
        assert_eq!(parse_run_state(run_state_word(state)), Some(state));
    }
    assert_eq!(run_state_word(RunState::Starting), "starting");
    assert_eq!(parse_run_state("nonsense"), None);
    assert_eq!(parse_run_state(""), None);
}

/// A reconnect builds a fresh `EventRouter`, so every subscription made on the
/// previous connection is dead: its sender lives in the old router, which
/// nothing dispatches into any more. The panel has to notice that and subscribe
/// again, or a run that is still running goes silent for the rest of the
/// session with Refresh unable to recover it.
#[test]
fn a_subscription_from_an_earlier_connection_is_replaced() {
    assert!(
        needs_resubscribe(None, 1),
        "a run with no subscription needs one"
    );
    assert!(
        !needs_resubscribe(Some(1), 1),
        "a subscription on the live connection is kept"
    );
    assert!(
        needs_resubscribe(Some(1), 2),
        "a subscription from before a reconnect is replaced"
    );
}

/// The toast carries no workspace of its own, so answering it has to name one.
/// Extending the allowlist of a workspace that never denied the host is exactly
/// what an allowlist exists to prevent, so the answer goes to the workspace the
/// toast was raised for -- or, for a caller that never saw a toast, to a
/// workspace that has the host queued itself.
#[test]
fn a_denial_is_answered_for_the_workspace_it_was_raised_for() {
    // The ordinary case: the toast is up for ws_a and the user clicks Allow.
    assert_eq!(
        denial_owner(Some(("ws_a", "example.com")), Some("ws_a"), "example.com"),
        Some("ws_a".to_owned())
    );

    // The user switched to ws_b while the toast for ws_a was up. The host goes
    // to ws_a, which is the workspace that was actually blocked.
    assert_eq!(
        denial_owner(Some(("ws_a", "example.com")), None, "example.com"),
        Some("ws_a".to_owned()),
        "the toast's workspace wins over whatever is on screen"
    );

    // A caller answering a host that is not the one on screen falls back to the
    // workspace that has it queued, and to nothing when none does.
    assert_eq!(
        denial_owner(
            Some(("ws_a", "example.com")),
            Some("ws_b"),
            "cdn.example.net"
        ),
        Some("ws_b".to_owned())
    );
    assert_eq!(
        denial_owner(Some(("ws_a", "example.com")), None, "cdn.example.net"),
        None,
        "a host no workspace denied is not allowed anywhere"
    );
    assert_eq!(denial_owner(None, None, "example.com"), None);
    assert_eq!(
        denial_owner(None, Some("ws_b"), "example.com"),
        Some("ws_b".to_owned()),
        "no toast on screen, but this workspace queued the host"
    );
}

// ---------------------------------------------------------------------------
// Run configurations the daemon could not load (M7 Task 3).
// ---------------------------------------------------------------------------

/// `repo.detect_run_configs` answers with one `warnings` entry per complaint
/// about `bondsymphonic.toml`, and loads whatever else parsed. The panel holds
/// them beside the configurations they were detected with, so switching to a
/// worktree whose file is clean takes the previous worktree's complaints down.
#[test]
fn detection_warnings_are_held_per_workspace_and_replaced_on_the_next_detection() {
    const NO_PORT: &str = "bondsymphonic.toml: [[run]] #2 ('api') has no port; it is not offered";

    let mut runs = WorkspaceRuns::default();
    assert!(runs.warnings.is_empty());

    runs.set_configs(vec![config("dev", 5173)]);
    runs.set_warnings(vec![NO_PORT.to_owned()]);
    assert_eq!(runs.warnings, vec![NO_PORT.to_owned()]);
    assert_eq!(
        runs.selected.as_deref(),
        Some("dev"),
        "the good entry loads"
    );

    // The next detection is the whole answer, so a fixed file clears the line.
    runs.set_warnings(Vec::new());
    assert!(runs.warnings.is_empty());
}
