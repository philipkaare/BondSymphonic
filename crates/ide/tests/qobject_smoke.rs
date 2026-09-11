//! The pure-Rust half of the QObject invokables.
//!
//! `GroupModel` is a thin shell over [`Workspaces`]: every invokable parses
//! JSON, calls one method, and re-serialises. The QObject wrappers themselves
//! need a Qt event loop, so they are exercised by the widgets in later tasks;
//! what is testable here is that the JSON the invokables move across the
//! boundary survives the round trip, including the state the widgets read back
//! out of `state_json`.

use bondsymphonic_ide::highlight::theme::Theme;
use bondsymphonic_ide::model::app_state::{permission_attention, AgentTab, TabStatus, Workspaces};
use bondsymphonic_ide::model::file_tree::FileTree;
use bondsymphonic_ide::qobjects::app_controller::network_denial;
use bondsymphonic_ide::qobjects::changes_model::touches_workspace;
use bondsymphonic_ide::qobjects::editor_document::{
    build_buffer, may_install_disk_text, normalise_line_separators, read_only_reason,
    HIGHLIGHT_MAX_BYTES,
};
use bondsymphonic_proto::{
    AgentAdapterKind, AgentId, AgentState, AgentSummary, Event, FileEntry, FileStatus, LogLevel,
    PtyId, ReadFileResult, WorkspaceId, WorkspaceInfo, WorkspaceState,
};

fn info(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}"),
        worktree_path: format!("/wt/{name}"),
        created_at: "2026-09-09T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        state,
        agents: Vec::new(),
        agent_records: Vec::new(),
        runs: Vec::new(),
    }
}

fn tab(info: &WorkspaceInfo) -> AgentTab {
    AgentTab {
        workspace_id: info.id.clone(),
        name: info.name.clone(),
        repo_path: info.repo_path.clone(),
        branch: info.branch.clone(),
        base_branch: info.base_branch.clone(),
        status: TabStatus::from_workspace_state(&info.state),
        detail: String::new(),
        worktree_path: info.worktree_path.clone(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
        run_config: None,
        options_json: String::new(),
        agent_id: None,
        agent_status: None,
        agent_detail: String::new(),
        op_error: None,
        attention: String::new(),
    }
}

/// `state_json` is written after every mutation and read back by the views:
/// the model has to survive that round trip unchanged, active selection and
/// all.
#[test]
fn state_json_round_trips_a_populated_model() {
    let mut model = Workspaces::new_default();
    let idx = model.add_group("Backend");
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let two = info("ws_2", "beta", WorkspaceState::Creating);
    model.add_tab(0, tab(&one));
    model.add_tab(idx, tab(&two));
    assert!(model.set_active(0, 0));

    let restored = Workspaces::from_json(&model.to_json()).expect("state_json parses");
    assert_eq!(restored, model);
    assert_eq!(restored.groups.len(), 2);
    assert_eq!(restored.active().map(|t| t.name.as_str()), Some("alpha"));
}

/// What `load_state` does with a session file that is missing or corrupt.
#[test]
fn unparseable_state_json_is_rejected_rather_than_guessed() {
    assert!(Workspaces::from_json("").is_none());
    assert!(Workspaces::from_json("not json").is_none());
    assert!(Workspaces::from_json("{\"groups\":[]}").is_none());
    // The fallback the invokable installs instead.
    let fallback = Workspaces::new_default();
    assert_eq!(fallback.groups.len(), 1);
    assert!(fallback.groups[0].tabs.is_empty());
}

/// `add_tab` and `apply_workspace_info` are handed a serialised
/// `WorkspaceInfo` from the controller's signals.
#[test]
fn workspace_info_json_drives_tab_status_and_detail() {
    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    let json = serde_json::to_string(&ready).expect("info serialises");
    let parsed: WorkspaceInfo = serde_json::from_str(&json).expect("info parses");
    assert_eq!(parsed.id, ready.id);

    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&parsed));
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Idle);

    let broken = info("ws_1", "alpha", WorkspaceState::Error("disk full".into()));
    let json = serde_json::to_string(&broken).expect("info serialises");
    let parsed: WorkspaceInfo = serde_json::from_str(&json).expect("info parses");
    assert!(model.apply_workspace_info(&parsed).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(model.groups[0].tabs[0].detail, "disk full");
}

/// `reconcile` receives the controller's `workspaces_listed` payload verbatim.
#[test]
fn reconcile_accepts_a_serialised_workspace_list() {
    let list = vec![
        info("ws_1", "alpha", WorkspaceState::Ready),
        info("ws_2", "beta", WorkspaceState::SandboxDown),
    ];
    let json = serde_json::to_string(&list).expect("list serialises");
    let parsed: Vec<WorkspaceInfo> = serde_json::from_str(&json).expect("list parses");

    let mut model = Workspaces::new_default();
    model.reconcile(&parsed);
    let names: Vec<&str> = model
        .groups
        .iter()
        .flat_map(|g| g.tabs.iter())
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert!(model.find(&WorkspaceId("ws_2".into())).is_some());

    // An empty list drops everything, which is what a fresh daemon reports.
    model.reconcile(&[]);
    assert_eq!(model.groups.iter().map(|g| g.tabs.len()).sum::<usize>(), 0);
    assert!(model.active().is_none());
}

/// The flow `add_tab` has to survive: `reconcile` files a brand new workspace
/// into "Unsorted" as a plain terminal, and the `workspace_created` signal
/// then arrives carrying the group the user actually picked.
#[test]
fn a_reconciled_workspace_moves_into_the_group_the_user_asked_for() {
    let created = info("ws_9", "gamma", WorkspaceState::Creating);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&info("ws_1", "alpha", WorkspaceState::Ready)));
    model.reconcile(&[
        info("ws_1", "alpha", WorkspaceState::Ready),
        created.clone(),
    ]);

    let (group, _) = model.find(&created.id).expect("reconcile filed it");
    assert_eq!(model.groups[group].name, "Unsorted");

    assert!(model.move_tab_to_group(&created.id, "Backend"));
    let (group, idx) = model.find(&created.id).expect("still tracked");
    assert_eq!(model.groups[group].name, "Backend");
    assert_eq!(model.groups[group].tabs[idx].name, "gamma");
    // The tab it was filed next to is untouched.
    assert!(model.find(&WorkspaceId("ws_1".into())).is_some());

    // Moving it again to where it already is changes nothing.
    assert!(!model.move_tab_to_group(&created.id, "Backend"));
    // An unknown workspace is not a move.
    assert!(!model.move_tab_to_group(&WorkspaceId("ws_nope".into()), "Backend"));
}

/// Moving the active tab keeps it active, and moving another tab out from
/// under the selection does not leave it pointing at the wrong row.
#[test]
fn moving_a_tab_keeps_the_selection_pointing_at_the_same_workspace() {
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let two = info("ws_2", "beta", WorkspaceState::Ready);
    let three = info("ws_3", "delta", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&one));
    model.add_tab(0, tab(&two));
    model.add_tab(0, tab(&three));

    // The active tab follows the move.
    assert!(model.set_active(0, 2));
    assert!(model.move_tab_to_group(&three.id, "Backend"));
    assert_eq!(model.active().map(|t| t.name.as_str()), Some("delta"));

    // A tab moving out of the group ahead of the selection does not shift it.
    assert!(model.set_active(0, 1));
    assert!(model.move_tab_to_group(&one.id, "Backend"));
    assert_eq!(model.active().map(|t| t.name.as_str()), Some("beta"));
}

/// `active_tab_json` hands one tab to the widgets; it has to parse back.
#[test]
fn active_tab_json_round_trips() {
    let mut model = Workspaces::new_default();
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    model.add_tab(0, tab(&one));
    let active = model.active().expect("a tab is active");
    let json = serde_json::to_string(active).expect("tab serialises");
    let parsed: AgentTab = serde_json::from_str(&json).expect("tab parses");
    assert_eq!(&parsed, active);
}

/// `activeTabJson` is how the window learns which pane to build, and the three
/// keys it reads for a restored Claude tab have to survive the crossing.
///
/// `AgentArea::showWorkspace` switches on `adapter` and attaches the transcript
/// to `agent_id`; `MainWindow` then hands `options_json` to
/// `AgentArea::setOptionsJson`, which is what a Restart resumes with. A tab
/// rebuilt from `WorkspaceInfo.agents` fills all three, and nothing in C++ can
/// read them if the serialisation drops them.
#[test]
fn a_restored_agent_tab_crosses_the_boundary_with_its_adapter_and_agent() {
    let mut listed = info("ws_1", "alpha", WorkspaceState::Ready);
    listed.agent_records = vec![AgentSummary {
        id: AgentId("ag_1".to_owned()),
        adapter: AgentAdapterKind::Claude,
        state: AgentState::Exited,
        session_id: Some("sess-1".to_owned()),
        command: None,
        model: Some("claude-opus-5".to_owned()),
        permission_mode: Some("acceptEdits".to_owned()),
    }];
    let mut model = Workspaces::new_default();
    model.add_tab(0, AgentTab::from_workspace_info(&listed));

    let json = serde_json::to_string(model.active().expect("a tab is active")).expect("serialises");
    let tab: serde_json::Value = serde_json::from_str(&json).expect("an object");
    // The words C++ compares against, not the Rust spellings.
    assert_eq!(tab["adapter"], "claude");
    assert_eq!(tab["agent_id"], "ag_1");
    let options: serde_json::Value =
        serde_json::from_str(tab["options_json"].as_str().expect("a string")).expect("an object");
    assert_eq!(options["model"], "claude-opus-5");
    assert_eq!(options["permission_mode"], "acceptEdits");
    assert!(
        options.get("api_key").is_none(),
        "the key never crosses this boundary"
    );
}

/// `cached_entries` returns what `FileTreeModel` stored from `fs.list_dir`.
#[test]
fn file_entries_json_round_trips() {
    let entries = vec![
        FileEntry {
            name: "src".to_owned(),
            is_dir: true,
            size: 0,
            status: FileStatus::Unchanged,
        },
        FileEntry {
            name: "README.md".to_owned(),
            is_dir: false,
            size: 42,
            status: FileStatus::Modified,
        },
    ];
    let mut tree = FileTree::new();
    tree.set_dir("", entries.clone());
    assert!(tree.is_loaded(""));

    let json = FileTree::entries_json(tree.get_dir("").expect("root is cached"));
    let parsed: Vec<FileEntry> = serde_json::from_str(&json).expect("entries parse");
    assert_eq!(parsed, entries);

    // The empty array `cached_entries` returns for a directory that has not
    // been listed yet.
    assert!(tree.get_dir("src").is_none());
    let empty: Vec<FileEntry> = serde_json::from_str("[]").expect("empty entries parse");
    assert!(empty.is_empty());
}

/// `EditorDocument::open` turns a `fs.read_file` result into the sentence the
/// header shows when a file cannot be edited.
#[test]
fn read_only_reason_names_binary_and_truncated_files() {
    let ok = ReadFileResult {
        content: "x".into(),
        encoding: "utf-8".into(),
        truncated: false,
    };
    assert_eq!(read_only_reason(&ok), "");
    let bin = ReadFileResult {
        content: String::new(),
        encoding: "binary".into(),
        truncated: false,
    };
    assert_eq!(read_only_reason(&bin), "binary file");
    let big = ReadFileResult {
        content: "x".into(),
        encoding: "utf-8".into(),
        truncated: true,
    };
    assert_eq!(read_only_reason(&big), "file larger than 4 MiB (truncated)");
}

/// The filter both the editor's watch task and `ChangesModel` run over the
/// router's stream: only `fs.changed`, only for the workspace in question.
#[test]
fn fs_changed_events_are_matched_by_workspace() {
    let ws = WorkspaceId("ws_1".into());
    let ev = Event::FsChanged {
        paths: vec!["a.rs".into()],
    };
    assert!(touches_workspace(&Some(ws.clone()), &ev, "ws_1"));
    assert!(!touches_workspace(&Some(ws), &ev, "ws_2"));
    assert!(!touches_workspace(&None, &ev, "ws_1"));
    assert!(!touches_workspace(
        &Some(WorkspaceId("ws_1".into())),
        &Event::PtyExit {
            pty_id: PtyId("p".into()),
            code: 0
        },
        "ws_1"
    ));
}

/// Every break Qt starts a block at but the buffer does not is rewritten on
/// load, so the view and the buffer agree on which line is which. The breaks
/// both sides already agree on survive untouched.
#[test]
fn qt_only_line_breaks_are_normalised_to_newlines() {
    // A bare carriage return: a block break to Qt, an ordinary character to
    // the buffer.
    let (text, normalised) = normalise_line_separators("a\rb");
    assert_eq!(text, "a\nb");
    assert!(normalised);

    // A carriage return that belongs to a `\r\n` is left where it is.
    let (text, normalised) = normalise_line_separators("a\r\nb");
    assert_eq!(text, "a\r\nb");
    assert!(!normalised);

    let (text, normalised) = normalise_line_separators("a\u{2029}b");
    assert_eq!(text, "a\nb");
    assert!(normalised);

    // The ordinary file: nothing to do, and nothing claimed.
    let (text, normalised) = normalise_line_separators("a\nb\r\nc");
    assert_eq!(text, "a\nb\r\nc");
    assert!(!normalised);

    let (text, normalised) = normalise_line_separators("a\u{2029}b\u{2028}c\rd\r\ne");
    assert_eq!(text, "a\nb\nc\nd\r\ne");
    assert!(normalised);
}

/// Files past [`HIGHLIGHT_MAX_BYTES`] open without a highlight pass but keep
/// reporting their language, so the header is right and typing stays cheap.
#[test]
fn very_large_files_open_with_highlighting_off() {
    let small = "fn main() {}\n";
    let (buffer, _) = build_buffer("a.rs", small);
    assert!(buffer.highlighting());
    assert_eq!(buffer.language().map(|l| l.name()), Some("rust"));

    let big = "fn main() {}\n".repeat(HIGHLIGHT_MAX_BYTES / 8);
    assert!(big.len() > HIGHLIGHT_MAX_BYTES);
    let (mut buffer, _) = build_buffer("a.rs", &big);
    assert!(!buffer.highlighting());
    assert_eq!(buffer.language().map(|l| l.name()), Some("rust"));
    assert_eq!(buffer.spans_json(0, Theme::for_dark(false)), "[]");
}

/// The rule that keeps a keystroke typed during a save, and a save that
/// overtakes a reload, from being undone by the read they raced.
#[test]
fn a_reload_installs_disk_text_only_over_an_untouched_document() {
    // Nothing happened while the read was in flight: install it.
    assert!(may_install_disk_text(false, 7, 7));

    // The user typed while the read was in flight: their edits would be lost.
    assert!(!may_install_disk_text(true, 7, 8));

    // Clean again, but only because a save of newer text landed first.
    // Installing the older read would undo that save.
    assert!(!may_install_disk_text(false, 7, 8));

    // Dirty without the counter having moved cannot happen in practice, and
    // refusing is the safe answer either way.
    assert!(!may_install_disk_text(true, 7, 7));
}

/// A Claude tab's status comes from the agent, not the workspace: the
/// workspace stays `Ready` for the whole session while the agent moves
/// between working, waiting on a permission and idle.
#[test]
fn agent_state_maps_onto_a_tab_status() {
    use bondsymphonic_proto::AgentState;
    let pairs = [
        (AgentState::Idle, TabStatus::Idle),
        (AgentState::Working, TabStatus::Working),
        (AgentState::WaitingPermission, TabStatus::WaitingPermission),
        (AgentState::Error, TabStatus::Error),
        // A finished agent is a finished tab, not a broken one.
        (AgentState::Exited, TabStatus::Done),
    ];
    for (state, expected) in pairs {
        assert_eq!(TabStatus::from_agent_state(&state), expected, "{state:?}");
    }
}

/// `agent.state` events carry an agent id and no workspace id, so the model
/// has to find the tab by the id it was told about when the agent started.
#[test]
fn set_agent_records_the_id_and_set_agent_status_finds_the_tab_by_it() {
    let mut model = Workspaces::new_default();
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let two = info("ws_2", "beta", WorkspaceState::Ready);
    model.add_tab(0, tab(&one));
    model.add_tab(0, tab(&two));

    // Unknown agent ids change nothing.
    assert_eq!(
        model.set_agent_status(&"ag_1".into(), TabStatus::Working, ""),
        None
    );
    assert!(!model.set_agent(&"ws_missing".into(), "ag_1".into()));

    assert!(model.set_agent(&two.id, "ag_1".into()));
    assert_eq!(
        model.groups[0].tabs[1]
            .agent_id
            .as_ref()
            .map(|a| a.as_str()),
        Some("ag_1")
    );
    assert_eq!(
        model.set_agent_status(&"ag_1".into(), TabStatus::WaitingPermission, "Bash"),
        Some((0, 1))
    );
    assert_eq!(model.groups[0].tabs[1].status, TabStatus::WaitingPermission);
    assert_eq!(model.groups[0].tabs[1].detail, "Bash");
    // The other tab is untouched.
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Idle);

    // The id survives the round trip through `state_json`.
    let restored = Workspaces::from_json(&model.to_json()).expect("state_json parses");
    assert_eq!(restored.groups[0].tabs[1].agent_id, Some("ag_1".into()));
}

/// Sessions saved before agent tabs existed have no `agent_id` field.
#[test]
fn state_json_without_an_agent_id_still_loads() {
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&info("ws_1", "alpha", WorkspaceState::Ready)));
    let json = model.to_json();
    let stripped = json.replace(",\"agent_id\":null", "");
    assert!(!stripped.contains("agent_id"), "{stripped}");
    let restored = Workspaces::from_json(&stripped).expect("legacy state_json parses");
    assert_eq!(restored.groups[0].tabs[0].agent_id, None);
}

/// The two directions the `state` string crosses the boundary: the daemon's
/// `agent.state` word in, and the `TranscriptModel::state` property out.
#[test]
fn agent_state_words_round_trip() {
    use bondsymphonic_ide::model::app_state::{agent_state_word, parse_agent_state};
    use bondsymphonic_proto::AgentState;
    for state in [
        AgentState::Idle,
        AgentState::Working,
        AgentState::WaitingPermission,
        AgentState::Error,
        AgentState::Exited,
    ] {
        assert_eq!(parse_agent_state(agent_state_word(state)), Some(state));
    }
    assert_eq!(
        agent_state_word(AgentState::WaitingPermission),
        "waiting_permission"
    );
    assert_eq!(parse_agent_state("nonsense"), None);
    assert_eq!(parse_agent_state(""), None);
}

/// The badge changes hands between the workspace and the agent. A sandbox
/// outage takes it; recovery must hand it back, because an idle agent emits no
/// state of its own and would otherwise leave "sandbox down" on screen for the
/// rest of the session.
#[test]
fn an_agent_tab_gets_its_status_back_when_the_workspace_recovers() {
    let mut model = Workspaces::new_default();
    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    model.add_tab(0, tab(&ready));
    assert!(model.set_agent(&ready.id, "ag_1".into()));
    model.set_agent_status(&"ag_1".into(), TabStatus::Working, "");
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Working);

    // A `workspace.state` for a healthy workspace must not blank a live turn.
    assert!(model.apply_workspace_info(&ready).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Working);

    // The sandbox goes down: that is the workspace's news, and it wins.
    let down = info("ws_1", "alpha", WorkspaceState::SandboxDown);
    assert!(model.apply_workspace_info(&down).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::SandboxDown);

    // Recovery hands the badge back to what the agent last reported.
    assert!(model.apply_workspace_info(&ready).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Working);
}

/// The agent's detail travels with its status, so a workspace error's text
/// does not outlive the workspace error.
#[test]
fn a_recovering_agent_tab_restores_the_agent_detail_not_the_workspace_error() {
    let mut model = Workspaces::new_default();
    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    model.add_tab(0, tab(&ready));
    assert!(model.set_agent(&ready.id, "ag_1".into()));
    model.set_agent_status(&"ag_1".into(), TabStatus::Error, "exit code 1");

    let broken = info("ws_1", "alpha", WorkspaceState::Error("disk full".into()));
    assert!(model.apply_workspace_info(&broken).is_some());
    assert_eq!(model.groups[0].tabs[0].detail, "disk full");

    assert!(model.apply_workspace_info(&ready).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(model.groups[0].tabs[0].detail, "exit code 1");
}

/// An agent that has not reported yet: the tab is idle, not stuck on whatever
/// the workspace last said.
#[test]
fn an_agent_tab_with_no_reported_status_yet_becomes_idle_on_ready() {
    let mut model = Workspaces::new_default();
    let creating = info("ws_1", "alpha", WorkspaceState::Creating);
    model.add_tab(0, tab(&creating));
    assert!(model.set_agent(&creating.id, "ag_1".into()));
    assert!(model.apply_workspace_info(&creating).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Creating);

    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    assert!(model.apply_workspace_info(&ready).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Idle);
    assert!(model.groups[0].tabs[0].detail.is_empty());
}

/// A tab with no agent is unaffected by any of the above.
#[test]
fn a_terminal_tab_still_takes_its_status_from_the_workspace() {
    let mut model = Workspaces::new_default();
    let ready = info("ws_1", "alpha", WorkspaceState::Ready);
    model.add_tab(0, tab(&ready));
    let down = info("ws_1", "alpha", WorkspaceState::SandboxDown);
    assert!(model.apply_workspace_info(&down).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::SandboxDown);
    assert!(model.apply_workspace_info(&ready).is_some());
    assert_eq!(model.groups[0].tabs[0].status, TabStatus::Idle);
}

// ---------------------------------------------------------------------------
// Task 7: the setup page's pure-Rust half.
// ---------------------------------------------------------------------------

/// The two login URLs the setup terminals print. Everything else in the output
/// stream is text, so only these two prefixes are recognised: an agent that
/// prints a link must not make the IDE open a browser.
#[test]
fn find_links_picks_the_login_urls_out_of_terminal_text() {
    use bondsymphonic_ide::qobjects::terminal_session::find_links;

    assert_eq!(
        find_links("open https://claude.ai/oauth/authorize?x=1 in a browser"),
        ["https://claude.ai/oauth/authorize?x=1"]
    );
    assert_eq!(
        find_links("go to https://github.com/login/device and enter ABCD-1234"),
        ["https://github.com/login/device"]
    );

    // Not a login URL: no browser opens for it.
    assert!(find_links("see https://example.com/x for details").is_empty());
    // The right host, but not the device-login path.
    assert!(find_links("https://github.com/philipkaare/BondSymphonic").is_empty());
    assert!(find_links("nothing to see here").is_empty());

    // Sentence punctuation is not part of the URL.
    assert_eq!(
        find_links("Visit https://claude.ai/oauth/authorize?x=1."),
        ["https://claude.ai/oauth/authorize?x=1"]
    );
    assert_eq!(
        find_links("(https://claude.ai/oauth/authorize?x=1),"),
        ["https://claude.ai/oauth/authorize?x=1"]
    );
    // A quote ends it, as does an escape: `gh` wraps the URL in an OSC 8
    // hyperlink, whose terminator is an ESC.
    assert_eq!(
        find_links("\"https://claude.ai/a\" and \u{1b}]8;;https://claude.ai/b\u{1b}\\"),
        ["https://claude.ai/a", "https://claude.ai/b"]
    );

    // The same URL twice in one buffer is one link.
    assert_eq!(
        find_links("https://claude.ai/a https://claude.ai/a"),
        ["https://claude.ai/a"]
    );
}

/// `pty.output` arrives in whatever chunks the daemon batched, so a URL is
/// routinely split down the middle. The scanner keeps a tail across chunks and
/// still reports each distinct URL exactly once.
#[test]
fn a_link_split_across_two_chunks_is_found_once() {
    use bondsymphonic_ide::qobjects::terminal_session::LinkScanner;

    let mut scanner = LinkScanner::new();
    assert!(scanner.push(b"please open https://cla").is_empty());
    assert_eq!(
        scanner.push(b"ude.ai/oauth/authorize?x=1\r\n"),
        ["https://claude.ai/oauth/authorize?x=1"]
    );
    // Echoed again later: already opened, so nothing more happens.
    assert!(scanner
        .push(b"https://claude.ai/oauth/authorize?x=1\r\n")
        .is_empty());

    // A second, different URL still gets through.
    assert_eq!(
        scanner.push(b"or https://github.com/login/device\r\n"),
        ["https://github.com/login/device"]
    );
}

/// A URL that is still arriving must not be opened truncated: it is held back
/// until something terminates it, and `flush` releases whatever is left when
/// the terminal exits.
#[test]
fn an_unterminated_link_waits_for_the_rest_of_its_chunk() {
    use bondsymphonic_ide::qobjects::terminal_session::LinkScanner;

    let mut scanner = LinkScanner::new();
    assert!(scanner.push(b"https://claude.ai/oauth?code=12").is_empty());
    assert!(scanner.push(b"345").is_empty());
    assert_eq!(
        scanner.flush(),
        ["https://claude.ai/oauth?code=12345"],
        "the whole URL, not the prefix that had arrived first"
    );
    // Flushing again repeats nothing.
    assert!(scanner.flush().is_empty());
}

/// The tail is bounded, so a chatty terminal cannot grow it without limit, and
/// a URL is still found across the boundary of a chunk that fills it.
#[test]
fn the_scanner_tail_stays_bounded() {
    use bondsymphonic_ide::qobjects::terminal_session::{LinkScanner, TAIL_BYTES};

    let mut scanner = LinkScanner::new();
    let noise = "x".repeat(TAIL_BYTES * 3);
    assert!(scanner.push(noise.as_bytes()).is_empty());
    assert!(scanner.tail_len() <= TAIL_BYTES);
    assert_eq!(
        scanner.push(b" https://claude.ai/late\n"),
        ["https://claude.ai/late"]
    );
}

/// Which prerequisites stop the IDE being usable at all. The four host ones do:
/// without git, bubblewrap, user namespaces or a working sandbox there is no
/// workspace to put an agent in. A missing CLI or login is a warning, because
/// everything except Claude Code still works.
#[test]
fn only_the_four_host_prerequisites_block_the_workbench() {
    use bondsymphonic_ide::qobjects::app_controller::prereqs_blocking;
    use bondsymphonic_proto::PrereqStatus;

    fn item(name: &str, ok: bool) -> PrereqStatus {
        PrereqStatus {
            name: name.to_owned(),
            ok,
            detail: String::new(),
            fix_hint: None,
        }
    }
    let all_ok = || {
        vec![
            item("git", true),
            item("bwrap", true),
            item("userns", true),
            item("claude", true),
            item("claude_auth", true),
            item("gh", true),
            item("gh_auth", true),
            item("sandbox", true),
        ]
    };

    assert!(!prereqs_blocking(&all_ok()));
    // An empty list is not a reason to hide the workbench.
    assert!(!prereqs_blocking(&[]));

    for blocking in ["git", "bwrap", "userns", "sandbox"] {
        let mut items = all_ok();
        items.iter_mut().find(|i| i.name == blocking).unwrap().ok = false;
        assert!(prereqs_blocking(&items), "{blocking} should block");
    }
    for warning in ["claude", "claude_auth", "gh", "gh_auth"] {
        let mut items = all_ok();
        items.iter_mut().find(|i| i.name == warning).unwrap().ok = false;
        assert!(!prereqs_blocking(&items), "{warning} should not block");
    }
    // A name the daemon grew after this build: unknown, so not blocking.
    assert!(!prereqs_blocking(&[item("something_new", false)]));
}

/// `settings.json` remembers that a key is in the credential store, and the
/// permission mode the New Agent dialog should start on. Neither the key nor
/// anything derived from it is ever written to the file.
#[test]
fn settings_round_trip_the_api_key_flag_and_permission_mode() {
    use bondsymphonic_ide::qobjects::settings::{Settings, SETTINGS_PATH_ENV};

    let dir = std::env::temp_dir().join(format!("bs-t7-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("settings.json");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(SETTINGS_PATH_ENV, &path);

    // A file that is not there yet reads as the defaults.
    let fresh = Settings::load();
    assert!(!fresh.api_key_set);
    assert_eq!(fresh.default_permission_mode, "default");

    let mut settings = Settings::load();
    settings.api_key_set = true;
    settings.default_permission_mode = "acceptEdits".to_owned();
    settings.save().expect("settings save");

    let loaded = Settings::load();
    assert!(loaded.api_key_set);
    assert_eq!(loaded.default_permission_mode, "acceptEdits");
    // Untouched fields survive the round trip.
    assert_eq!(loaded.distro, fresh.distro);

    let raw = std::fs::read_to_string(&path).expect("settings file");
    assert!(raw.contains("\"api_key_set\": true"), "{raw}");
    assert!(!raw.contains("sk-"), "no key material in settings.json");

    // Settings written before these fields existed still load.
    std::fs::write(&path, "{\"distro\":\"other\"}").expect("legacy settings");
    let legacy = Settings::load();
    assert_eq!(legacy.distro, "other");
    assert!(!legacy.api_key_set);
    assert_eq!(legacy.default_permission_mode, "default");

    std::env::remove_var(SETTINGS_PATH_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Run panel needs two things off the tab the window switched to: which
/// configuration the user picked when the workspace was created, and where the
/// worktree is, because `repo.detect_run_configs` takes a path. Both are
/// defaulted so a `session.json` written before Milestone 5 still loads.
#[test]
fn a_tab_carries_its_run_config_and_worktree_path_through_the_session_file() {
    let mut model = Workspaces::new_default();
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let mut t = tab(&one);
    t.run_config = Some("dev".to_owned());
    model.add_tab(0, t);

    let json = model.to_json();
    assert!(json.contains("\"run_config\":\"dev\""), "{json}");
    assert!(json.contains("\"worktree_path\":\"/wt/alpha\""), "{json}");

    let restored = Workspaces::from_json(&json).expect("state_json parses");
    assert_eq!(restored, model);
    let restored_tab = restored.active().expect("a tab");
    assert_eq!(restored_tab.run_config.as_deref(), Some("dev"));
    assert_eq!(restored_tab.worktree_path, "/wt/alpha");

    // A session file from before either field existed still loads, with the
    // tab simply having no run configuration and no path yet.
    let old = r#"{"groups":[{"id":"grp_0","name":"Default","tabs":[
        {"workspace_id":"ws_9","name":"old","repo_path":"/r","branch":"b",
         "status":"Idle","detail":"","adapter":"terminal","command":null}]}],
        "active_group":0,"active_tab":0}"#;
    let loaded = Workspaces::from_json(old).expect("an old session file still loads");
    let old_tab = &loaded.groups[0].tabs[0];
    assert_eq!(old_tab.run_config, None);
    assert_eq!(old_tab.worktree_path, "");
}

/// A workspace whose `worktree_path` was empty in a restored session heals as
/// soon as the daemon says what it is, so the Run panel can detect against it.
#[test]
fn applying_workspace_info_fills_in_a_missing_worktree_path() {
    let mut model = Workspaces::new_default();
    let one = info("ws_1", "alpha", WorkspaceState::Ready);
    let mut t = tab(&one);
    t.worktree_path = String::new();
    model.add_tab(0, t);
    assert!(model.apply_workspace_info(&one).is_some());
    assert_eq!(model.active().unwrap().worktree_path, "/wt/alpha");
}

/// The denial toast is driven by a `daemon.log` warn the proxy sends. Which
/// workspace it belongs to comes from the envelope, and the host from the
/// proto helper, so neither end parses the message text.
#[test]
fn a_denial_log_maps_onto_a_workspace_and_a_host() {
    let denial = Event::network_denied("example.com");
    assert_eq!(
        network_denial(&Some(WorkspaceId("ws_1".into())), &denial),
        Some(("ws_1".to_owned(), "example.com".to_owned()))
    );

    // No workspace: the daemon talking about itself. There is no allowlist to
    // offer to extend, so there is no toast.
    assert_eq!(network_denial(&None, &denial), None);

    // An ordinary warn that happens to carry no host is not a denial.
    let chatter = Event::DaemonLog {
        level: LogLevel::Warn,
        message: "something else went wrong".into(),
        host: None,
    };
    assert_eq!(
        network_denial(&Some(WorkspaceId("ws_1".into())), &chatter),
        None
    );
    assert_eq!(
        network_denial(&Some(WorkspaceId("ws_1".into())), &Event::events_dropped(3)),
        None
    );
}

/// The first `workspace.list` after a restart rebuilds the tab model from
/// `state.json` instead of filing everything into "Unsorted", and writes back
/// what it settled on, so the file follows the daemon rather than accumulating
/// workspaces that no longer exist.
#[test]
fn the_first_workspace_list_restores_the_persisted_arrangement() {
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateStore};
    use bondsymphonic_ide::qobjects::app_controller::restore_workspaces;

    let group = |name: &str, ids: &[&str]| PersistedGroup {
        name: name.to_owned(),
        workspace_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
    };
    // No path: nothing this test does can reach a file at all.
    let store = StateStore::new(None);
    store.update(|s| {
        s.groups = vec![group("Backend", &["ws_2", "ws_gone"]), group("Empty", &[])];
        s.active_workspace = Some("ws_2".to_owned());
        s.open_editors
            .insert("ws_gone".to_owned(), vec!["a.rs".to_owned()]);
    });

    let list = vec![
        info("ws_1", "alpha", WorkspaceState::Ready),
        info("ws_2", "beta", WorkspaceState::Ready),
    ];
    let (model, _token) = restore_workspaces(&store, &list);

    assert_eq!(
        model
            .groups
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        ["Backend", "Empty", "Unsorted"]
    );
    assert_eq!(model.active().map(|t| t.name.as_str()), Some("beta"));

    let saved = store.snapshot();
    assert_eq!(saved.groups[0].workspace_ids, ["ws_2"]);
    assert_eq!(saved.groups[2].workspace_ids, ["ws_1"]);
    assert_eq!(saved.active_workspace.as_deref(), Some("ws_2"));
    assert!(
        saved.open_editors.is_empty(),
        "the lost workspace's editors are pruned"
    );
}

/// `noteEditors` is called by the window with the tab list of one workspace.
/// It takes the object the window builds, and a bare array from anything that
/// only knows the open paths.
#[test]
fn note_editors_takes_an_object_or_a_bare_array() {
    use bondsymphonic_ide::qobjects::app_controller::parse_editors;

    assert_eq!(
        parse_editors(r#"{"open":["a.rs","b.rs"],"active":"b.rs"}"#),
        Some((
            vec!["a.rs".to_owned(), "b.rs".to_owned()],
            Some("b.rs".to_owned())
        ))
    );
    assert_eq!(
        parse_editors(r#"["a.rs"]"#),
        Some((vec!["a.rs".to_owned()], None))
    );
    // Every tab closed: an empty list, not "leave it as it was".
    assert_eq!(parse_editors("[]"), Some((Vec::new(), None)));
    assert_eq!(parse_editors("{}"), Some((Vec::new(), None)));
    // An active tab that is not open is not a state the window can be in.
    assert_eq!(
        parse_editors(r#"{"open":["a.rs"],"active":"gone.rs"}"#),
        Some((vec!["a.rs".to_owned()], None))
    );
    assert_eq!(parse_editors("not json"), None);
    assert_eq!(parse_editors(""), None);
}

/// What Restart sends after a reconnect. The tab's own options are kept
/// whole -- including any field this build does not know about -- and the
/// session id the transcript last saw is written into `resume_session`, which
/// is what makes the new agent continue the conversation rather than begin one.
#[test]
fn restart_options_carry_the_tab_options_and_the_last_session() {
    use bondsymphonic_ide::qobjects::transcript_model::restart_options;

    let merged = restart_options(r#"{"model":"opus","future_field":7}"#, Some("sess-3"));
    let value: serde_json::Value = serde_json::from_str(&merged).expect("an object");
    assert_eq!(value["model"], "opus");
    assert_eq!(value["resume_session"], "sess-3");
    // An unrecognised key survives the merge: this step copies through what it
    // is not changing rather than round-tripping the options via a struct.
    // (It does not survive the whole trip -- `start_options` parses into
    // `AgentStartOptions` before building the request -- but a merge that
    // silently dropped keys would be lossy in a place nobody would look.)
    assert_eq!(value["future_field"], 7);
}

/// The transcript is the first place a session id is looked for, and the tab
/// is the second. A history the daemon could not serve, or one that never got
/// as far as a `result` message, leaves the transcript with no id while the
/// daemon still holds the one the agent reported -- and that id is on the tab,
/// put there by `AgentTab::from_workspace_info` out of `AgentSummary`.
///
/// Restarting into a fresh session there loses the conversation the user is
/// looking at, which is the one thing Restart exists to keep.
#[test]
fn restart_options_fall_back_to_the_session_id_the_tab_carries() {
    use bondsymphonic_ide::qobjects::transcript_model::restart_options;

    let merged = restart_options(r#"{"model":"opus","resume_session":"sess-daemon"}"#, None);
    let value: serde_json::Value = serde_json::from_str(&merged).expect("an object");
    assert_eq!(value["model"], "opus");
    assert_eq!(
        value["resume_session"], "sess-daemon",
        "with no id in the transcript the tab's own is what a Restart resumes"
    );

    // The transcript still wins when it has one: it is the newer of the two.
    let merged = restart_options(
        r#"{"model":"opus","resume_session":"sess-daemon"}"#,
        Some("sess-newer"),
    );
    let value: serde_json::Value = serde_json::from_str(&merged).expect("an object");
    assert_eq!(value["resume_session"], "sess-newer");
}

#[test]
fn restart_options_with_no_session_anywhere_ask_for_a_fresh_one() {
    use bondsymphonic_ide::qobjects::transcript_model::restart_options;

    // Neither side has one: the key is absent, not null. A null would be a
    // request to resume a session named nothing.
    let merged = restart_options(r#"{"model":"opus"}"#, None);
    let value: serde_json::Value = serde_json::from_str(&merged).expect("an object");
    assert_eq!(value["model"], "opus");
    assert!(value.get("resume_session").is_none());
}

#[test]
fn restart_options_survive_a_tab_with_no_options_at_all() {
    use bondsymphonic_ide::qobjects::transcript_model::restart_options;

    // An empty string is the ordinary case for a terminal-adapter tab or a
    // dialog that set nothing; anything unparseable is treated the same way,
    // because the point of the call is options that can start an agent.
    for options in ["", "{}", "not json", "[1,2,3]"] {
        let merged = restart_options(options, Some("sess-9"));
        let value: serde_json::Value = serde_json::from_str(&merged).expect("an object");
        assert_eq!(value["resume_session"], "sess-9", "for {options:?}");
        assert!(value.is_object(), "for {options:?}");
    }
}

// ---------------------------------------------------------------------------
// A permission raised in a background tab (M4 deferral, closed in M7 Task 3).
// ---------------------------------------------------------------------------

/// `GroupModel::setWorkspaceAttention` and `clearWorkspaceAttention` across the
/// boundary: the window sets a line of text on the tab whose agent is waiting,
/// the tab bar reads it back out of `state_json` to paint its dot, and the
/// status bar reads `attentionText` for the sentence.
///
/// The text is the model's, not the window's invention: one wording for the dot
/// and the hint means the two cannot drift.
#[test]
fn attention_crosses_the_boundary_and_shows_up_in_the_state_json() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let beta = info("ws_2", "beta", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    model.add_tab(0, tab(&beta));

    let hint = permission_attention("beta");
    assert_eq!(hint, "beta is waiting for permission");
    assert!(model.set_workspace_attention(&beta.id, &hint));

    let restored = Workspaces::from_json(&model.to_json()).expect("state json round trips");
    let (g, t) = restored.find(&beta.id).expect("beta is tracked");
    assert_eq!(restored.groups[g].tabs[t].attention, hint);
    assert_eq!(restored.attention(), Some(hint.as_str()));
    // The tab that is not waiting says nothing, and its tooltip is unchanged.
    let (ag, at) = restored.find(&alpha.id).expect("alpha is tracked");
    assert!(restored.groups[ag].tabs[at].attention.is_empty());

    let mut restored = restored;
    assert!(restored.clear_workspace_attention(&beta.id));
    assert_eq!(restored.attention(), None);
    let _ = (g, t, ag, at);
}

/// An older `state.json`, written before tabs carried attention (or before the
/// group-id counter), still loads: the new fields default rather than failing
/// the whole restore and throwing away the user's groups.
#[test]
fn state_json_without_attention_still_loads() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    // The file as an older build wrote it: the two keys added in Milestone 7
    // taken back out again.
    let mut older: serde_json::Value =
        serde_json::from_str(&model.to_json()).expect("the model serialises");
    older
        .as_object_mut()
        .expect("an object")
        .remove("next_group_id");
    for group in older["groups"].as_array_mut().expect("groups") {
        for t in group["tabs"].as_array_mut().expect("tabs") {
            t.as_object_mut().expect("a tab").remove("attention");
        }
    }
    let older = older.to_string();
    assert!(!older.contains("attention"), "the fixture is a pre-M7 file");
    assert!(!older.contains("next_group_id"));

    let restored = Workspaces::from_json(&older).expect("an older state file still loads");
    assert_eq!(restored.attention(), None);
    assert_eq!(restored.groups[0].id, "grp_0");
    // And the counter picks up past the ids the file already carries rather
    // than reissuing `grp_0`.
    let mut restored = restored;
    restored.add_group("Second");
    assert_ne!(restored.groups[1].id, restored.groups[0].id);
}

// ---------------------------------------------------------------------------
// The Qt-less skip (M7 Task 3).
// ---------------------------------------------------------------------------

/// The guard the three binary-launching suites use. This host reached Qt one
/// way or the other -- it linked and started this very binary against it -- so
/// the guard must not skip.
#[test]
fn the_qt_guard_does_not_skip_on_a_host_that_has_qt() {
    assert!(!bondsymphonic_ide::testing::skip_without_qt("self-check"));
}

/// The half of the probe that `QMAKE` usually short-circuits.
///
/// `cxx-qt-build` accepts a bare `qmake` on `PATH` as well as the variable, so
/// a host that builds and runs the suite perfectly well but never dot-sourced
/// `scripts\env.ps1` must not have real coverage skipped out from under it --
/// nor, under `require-qt`, fail a job that would otherwise be green.
///
/// Driven through the value rather than the process environment: changing this
/// process's `PATH` would race every other thread in the binary, and on Windows
/// the search is otherwise unreachable, because a host with no `qmake` also has
/// no Qt DLLs and this binary would not have loaded.
#[test]
fn a_qmake_on_the_path_counts_even_with_the_variable_unset() {
    use bondsymphonic_ide::testing::path_has_qmake;
    use std::ffi::OsString;

    let root = std::env::temp_dir().join(format!("bs-qmake-probe-{}", std::process::id()));
    let with = root.join("qt-bin");
    let without = root.join("empty");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&with).expect("probe dir");
    std::fs::create_dir_all(&without).expect("probe dir");
    // Contents are never read -- `cxx-qt-build` is what runs it, not this.
    std::fs::write(with.join("qmake.exe"), b"").expect("fake qmake");

    let joined = |dirs: &[&std::path::Path]| -> OsString {
        std::env::join_paths(dirs.iter().map(|d| d.to_path_buf())).expect("a PATH")
    };

    assert!(!path_has_qmake(None), "no PATH at all is no Qt");
    assert!(!path_has_qmake(Some(&OsString::new())));
    assert!(!path_has_qmake(Some(&joined(&[without.as_path()]))));
    assert!(path_has_qmake(Some(&joined(&[with.as_path()]))));
    // Found wherever it sits in the list, not only at the front.
    assert!(path_has_qmake(Some(&joined(&[
        without.as_path(),
        with.as_path()
    ]))));
    // A directory named but not present is skipped rather than fatal.
    assert!(!path_has_qmake(Some(&joined(&[root
        .join("gone")
        .as_path()]))));

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Run configurations the daemon refused to load (M7 Task 3, daemon Task 2).
// ---------------------------------------------------------------------------

/// `repo.detect_run_configs` answers with a `warnings` entry per complaint the
/// daemon has about `bondsymphonic.toml`, and loads whatever else parsed.
/// Silently discarding the whole file is what the daemon used to do; the panel
/// is where the user finds out that an entry they wrote is not in the combo.
///
/// The daemon's own wording, so this pins what the panel does with the strings
/// that actually arrive rather than with invented ones.
#[test]
fn complaints_about_the_repo_config_are_summarised_for_the_panel() {
    use bondsymphonic_ide::qobjects::run_panel::warning_status;

    const NO_PORT: &str = "bondsymphonic.toml: [[run]] #2 ('api') has no port; it is not offered";
    const NO_COMMAND: &str =
        "bondsymphonic.toml: [[run]] #3 ('web') has no command; it is not offered";

    assert_eq!(warning_status(&[]), "");
    // One complaint is the daemon's finished sentence, which already names the
    // file. Anything added here would say it twice.
    assert_eq!(warning_status(&[NO_PORT.to_owned()]), NO_PORT);
    // Several are counted, and counted as problems rather than as ignored
    // configurations: a file that will not parse at all is a single warning
    // covering however many runs it declared, so the length is a count of
    // complaints and not of dropped entries. The sentences themselves are on
    // the combo's tooltip.
    assert_eq!(
        warning_status(&[NO_PORT.to_owned(), NO_COMMAND.to_owned()]),
        "2 problems with bondsymphonic.toml"
    );

    // A file that does not parse at all is a *single* warning carrying the
    // `toml` crate's message, offending line and caret -- for a file that may
    // have declared any number of runs. It is shown as the daemon wrote it,
    // newlines and all; the label wraps and the combo's tooltip carries the
    // same text, which is where a caret diagram actually reads.
    let parse_error = concat!(
        "bondsymphonic.toml: TOML parse error at line 3, column 1\n",
        "  |\n",
        "3 | port =\n",
        "  | ^\n",
        "invalid string — the file was ignored and the runs below were detected instead"
    );
    assert_eq!(warning_status(&[parse_error.to_owned()]), parse_error);
    // And two of anything is still counted, never joined into one line.
    assert_eq!(
        warning_status(&[parse_error.to_owned(), NO_PORT.to_owned()]),
        "2 problems with bondsymphonic.toml"
    );
}

/// The transition the feature exists for, and the one the first attempt missed.
///
/// An agent asks for permission while its own tab is in front, so nothing is
/// marked -- correctly, the bar is right there. The user then switches away,
/// and **no `agent.state` event fires**: the agent has not changed, only the
/// selection has. `refresh_attention` is what re-decides on a tab change, so
/// the tab just left picks up its bullet and the status bar its hint.
#[test]
fn switching_away_from_a_waiting_agent_marks_the_tab_it_left() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let beta = info("ws_2", "beta", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    model.add_tab(0, tab(&beta));

    // alpha is in front and its agent asks. The window marks nothing, because
    // the pane the user is looking at is showing the bar itself.
    model.set_active(0, 0);
    model.groups[0].tabs[0].agent_id = Some(AgentId("ag_1".into()));
    model.set_agent_status(
        &AgentId("ag_1".into()),
        TabStatus::WaitingPermission,
        "Bash",
    );
    assert_eq!(model.attention(), None, "the tab in front is not marked");

    // The user switches to beta. Nothing about ag_1 changed, so only this call
    // can notice.
    model.set_active(0, 1);
    assert!(
        model.refresh_attention(Some(&alpha.id)),
        "switching away is a change to the tab bar"
    );
    assert_eq!(model.attention(), Some("alpha is waiting for permission"));
    let (g, t) = model.find(&alpha.id).expect("alpha is tracked");
    assert_eq!(
        model.groups[g].tabs[t].attention,
        "alpha is waiting for permission"
    );

    // Switching back takes it down: the user can see the bar again.
    model.set_active(0, 0);
    assert!(model.refresh_attention(Some(&beta.id)));
    assert_eq!(model.attention(), None);

    // And an agent that is no longer waiting is not marked on the way past --
    // this is what stops every tab change leaving a trail of bullets behind it.
    model.set_agent_status(&AgentId("ag_1".into()), TabStatus::Working, "");
    model.set_active(0, 1);
    assert!(
        !model.refresh_attention(Some(&alpha.id)),
        "a working agent asks for nothing, so nothing is republished"
    );
    assert_eq!(model.attention(), None);
}

/// A tab change with no previous tab, and one that names the tab now in front,
/// are both no-ops rather than mistakes: the first is the session's very first
/// selection, the second is what a republish re-entering the window produces.
#[test]
fn refreshing_attention_without_a_previous_tab_changes_nothing() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    model.set_active(0, 0);
    model.groups[0].tabs[0].agent_id = Some(AgentId("ag_1".into()));
    model.set_agent_status(
        &AgentId("ag_1".into()),
        TabStatus::WaitingPermission,
        "Bash",
    );

    assert!(!model.refresh_attention(None));
    assert!(
        !model.refresh_attention(Some(&alpha.id)),
        "the tab in front is never marked, however it is named"
    );
    assert_eq!(model.attention(), None);
}

/// A daemon restart takes every agent with it, and the tabs have to stop
/// claiming otherwise.
///
/// The live `agent.state` stream reports changes, and an agent that no longer
/// exists has none to report: a background tab left `waiting_permission` when
/// the daemon died would keep its bullet forever, and the status bar would keep
/// naming an agent that is gone. `reconcile` is the first thing a reconnect
/// does, and the daemon's own `agent_records` are what it re-derives from.
#[test]
fn a_reconnect_re_derives_agent_state_and_drops_a_stale_attention_mark() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let beta = info("ws_2", "beta", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    model.add_tab(0, tab(&beta));
    model.groups[0].tabs[0].agent_id = Some(AgentId("ag_1".to_owned()));
    model.groups[0].tabs[1].agent_id = Some(AgentId("ag_2".to_owned()));

    // beta is in front; alpha's agent asks for permission behind it and the
    // window marks the tab.
    model.set_active(0, 1);
    model.set_agent_status(
        &AgentId("ag_1".to_owned()),
        TabStatus::WaitingPermission,
        "Bash",
    );
    model.set_workspace_attention(&alpha.id, &permission_attention("alpha"));
    assert_eq!(model.attention(), Some("alpha is waiting for permission"));

    // The daemon dies and comes back. Both agents are gone; the records say so,
    // and no `agent.state` event will ever be sent about them again.
    let record = |id: &str, state: AgentState| AgentSummary {
        id: AgentId(id.to_owned()),
        adapter: AgentAdapterKind::Claude,
        state,
        session_id: None,
        command: None,
        model: None,
        permission_mode: None,
    };
    let mut listed_alpha = alpha.clone();
    listed_alpha.agent_records = vec![record("ag_1", AgentState::Exited)];
    let mut listed_beta = beta.clone();
    listed_beta.agent_records = vec![record("ag_2", AgentState::Exited)];
    model.reconcile(&[listed_alpha, listed_beta]);

    assert_eq!(
        model.attention(),
        None,
        "the bullet outlived the agent it was about"
    );
    let (g, t) = model.find(&alpha.id).expect("alpha is still tracked");
    assert_eq!(model.groups[g].tabs[t].attention, "");
    assert_eq!(
        model.groups[g].tabs[t].status,
        TabStatus::Done,
        "an exited agent's tab shows exited, not the permission it was blocked on"
    );

    // An agent that really is still blocked keeps its mark across the
    // reconnect: the point is to re-derive, not to blank.
    let mut still_waiting = alpha.clone();
    still_waiting.agent_records = vec![record("ag_1", AgentState::WaitingPermission)];
    let mut idle_beta = beta.clone();
    idle_beta.agent_records = vec![record("ag_2", AgentState::Idle)];
    model.reconcile(&[still_waiting, idle_beta]);
    assert_eq!(model.attention(), Some("alpha is waiting for permission"));

    // ...unless it is the tab in front, whose permission bar the user can see.
    model.set_active(0, 0);
    let mut still_waiting = alpha.clone();
    still_waiting.agent_records = vec![record("ag_1", AgentState::WaitingPermission)];
    let mut idle_beta = beta.clone();
    idle_beta.agent_records = vec![record("ag_2", AgentState::Idle)];
    model.reconcile(&[still_waiting, idle_beta]);
    assert_eq!(model.attention(), None);
}

/// A daemon too old to send `agent_records` sends an empty list, which means
/// "nothing is known about these agents" rather than "there are none". The tab
/// falls back to idle and carries no mark, instead of keeping a claim that
/// nothing on the wire supports.
#[test]
fn a_reconnect_to_a_daemon_without_records_clears_rather_than_keeps() {
    let alpha = info("ws_1", "alpha", WorkspaceState::Ready);
    let beta = info("ws_2", "beta", WorkspaceState::Ready);
    let mut model = Workspaces::new_default();
    model.add_tab(0, tab(&alpha));
    model.add_tab(0, tab(&beta));
    model.groups[0].tabs[0].agent_id = Some(AgentId("ag_1".to_owned()));
    model.set_active(0, 1);
    model.set_agent_status(
        &AgentId("ag_1".to_owned()),
        TabStatus::WaitingPermission,
        "Bash",
    );
    model.set_workspace_attention(&alpha.id, &permission_attention("alpha"));

    model.reconcile(&[alpha.clone(), beta]);

    assert_eq!(model.attention(), None);
    let (g, t) = model.find(&alpha.id).expect("alpha is still tracked");
    assert_eq!(model.groups[g].tabs[t].status, TabStatus::Idle);
    assert_eq!(model.groups[g].tabs[t].agent_status, None);
}

// ---------------------------------------------------------------------------
// UI pass 2: the sign-in link, the name rule and the login gate.
// ---------------------------------------------------------------------------

/// What `claude auth login` actually prints. The URL is on `claude.com`, not
/// `claude.ai`, and the line after it is the prompt for the code -- so the
/// scanner has to stop at the newline and report the query string whole.
///
/// This is the failure the first real run hit: the browser never opened,
/// because the only Claude prefix the IDE knew was `https://claude.ai/`.
#[test]
fn the_real_claude_sign_in_url_opens_the_browser() {
    use bondsymphonic_ide::qobjects::terminal_session::{find_links, LinkScanner};

    const URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c\
                       &state=abc";
    let printed = format!(
        "Opening browser to sign in… If the browser didn't open, visit: {URL}\
         \r\nPaste code here if prompted > "
    );

    assert_eq!(find_links(&printed), [URL]);

    // And through the chunked path the PTY actually takes: once, and whole.
    let mut scanner = LinkScanner::new();
    let (head, tail) = printed.split_at(90);
    assert!(scanner.push(head.as_bytes()).is_empty());
    assert_eq!(scanner.push(tail.as_bytes()), [URL]);
    assert!(scanner.push(printed.as_bytes()).is_empty());
    assert!(scanner.flush().is_empty());

    // The list stays closed: `claude.com` is a login host, not an open door.
    assert!(find_links("see https://claude.com.evil.example/x").is_empty());
}

/// The rule the daemon enforces after the dialog has closed, moved to where
/// the user is still typing. Exactly the daemon's four conditions, so the
/// dialog can never accept a name `workspace.create` then refuses.
#[test]
fn a_workspace_name_must_be_one_path_safe_word() {
    use bondsymphonic_ide::qobjects::app_controller::validate_workspace_name;

    assert!(validate_workspace_name("agent-1").is_ok());
    assert!(validate_workspace_name("agent_1").is_ok());
    assert!(validate_workspace_name("fix.the.bug").is_ok());

    // Empty, whitespace anywhere, a path separator, or a parent reference.
    assert!(validate_workspace_name("").is_err());
    assert!(validate_workspace_name("my agent").is_err());
    assert!(validate_workspace_name("agent\t1").is_err());
    assert!(validate_workspace_name("feature/x").is_err());
    assert!(validate_workspace_name("..").is_err());
    assert!(validate_workspace_name("a..b").is_err());

    // One sentence, so the dialog can show it verbatim under the field.
    assert_eq!(
        validate_workspace_name("my agent").unwrap_err(),
        "Use a single word: letters, digits, - or _"
    );
}

/// Whether the chat composer is open, read off the prerequisite list and the
/// credential store rather than off anything the IDE remembers: `claude_auth`
/// is the daemon's answer to "can Claude Code run without a login prompt", and
/// `-p` mode cannot log in, so a composer offered with neither credential only
/// ever produces "login is not available in this environment".
#[test]
fn claude_login_is_read_off_the_claude_auth_prerequisite() {
    use bondsymphonic_ide::qobjects::app_controller::claude_logged_in;
    use bondsymphonic_proto::PrereqStatus;

    fn item(name: &str, ok: bool) -> PrereqStatus {
        PrereqStatus {
            name: name.to_owned(),
            ok,
            detail: String::new(),
            fix_hint: None,
        }
    }
    const NO_KEY: bool = false;
    const KEY: bool = true;

    assert!(claude_logged_in(
        &[item("claude", true), item("claude_auth", true)],
        NO_KEY
    ));
    assert!(!claude_logged_in(
        &[item("claude", true), item("claude_auth", false)],
        NO_KEY
    ));
    // No answer is not a yes: a daemon that never reported the item leaves the
    // gate shut rather than opening a composer that cannot send.
    assert!(!claude_logged_in(&[item("claude", true)], NO_KEY));
    assert!(!claude_logged_in(&[], NO_KEY));

    // A stored API key is the other credential. The daemon's `claude_auth`
    // cannot see it -- it answers from `claude auth status` and the daemon's own
    // environment -- so a gate on that alone would shut a user out of every
    // composer while their agents ran perfectly on the key.
    assert!(claude_logged_in(
        &[item("claude", true), item("claude_auth", false)],
        KEY
    ));
    assert!(claude_logged_in(&[], KEY));
    // And removing the key shuts it again when nothing else vouches for Claude.
    assert!(!claude_logged_in(
        &[item("claude", true), item("claude_auth", false)],
        NO_KEY
    ));
    // Either credential alone is enough; neither is not.
    assert!(claude_logged_in(&[item("claude_auth", true)], KEY));
}

/// Settings opens on Setup by itself when a blocking prerequisite fails, and
/// it has to be closable afterwards.
///
/// Closing the dialog re-checks the prerequisites. On the machine the setup
/// flow exists for -- a first run with no bubblewrap -- that answer still says
/// "blocked", so an auto-open with no memory reopens the dialog every time it
/// is closed and the user cannot get past it. This walks that episode.
#[test]
fn setup_opens_itself_once_per_run_of_blocking_failures() {
    use bondsymphonic_ide::qobjects::app_controller::should_auto_open_setup;

    // First answer of the session: blocked, nothing shown yet.
    assert!(should_auto_open_setup(true, false));
    // The window records that it opened one. The re-check that closing the
    // dialog issues arrives still blocked -- and must not reopen it.
    assert!(!should_auto_open_setup(true, true));
    // Nor does any later check, for as long as the same failure stands.
    assert!(!should_auto_open_setup(true, true));

    // Nothing blocking: no dialog, and the window clears its flag on this
    // answer, which is what re-arms the rule.
    assert!(!should_auto_open_setup(false, true));
    assert!(!should_auto_open_setup(false, false));

    // A prerequisite that breaks again later is a new episode, so the dialog
    // is offered again rather than staying silent for the session.
    assert!(should_auto_open_setup(true, false));
}

/// The C++ widgets, driven offscreen.
///
/// The IDE's shell is Qt Widgets in `crates/ide/cpp` and its suites are Rust,
/// so the checks that need a live widget are written beside the widget they are
/// about and exported from its own translation unit as a plain C entry point.
/// Each answers 0 for a pass and a small non-zero code naming the assertion
/// that failed, which is all that has to cross the boundary.
///
/// One `#[test]`, with every entry called from it in turn: Qt allows widgets
/// only on the thread that built the `QApplication`, and libtest hands each
/// test function a thread of its own.
mod cpp_widgets {
    use bondsymphonic_ide::testing::skip_without_qt;

    extern "C" {
        /// Builds the offscreen `QApplication` the entries below need. Called
        /// once; a second call is a no-op.
        fn bs_widget_test_begin();
        fn bs_widget_test_editor_area_survives_a_destroyed_workspace() -> i32;
        fn bs_widget_test_changes_toolbar_cancels_a_switched_workspace() -> i32;
        fn bs_widget_test_changes_toolbar_acts_on_the_confirmed_workspace() -> i32;
        fn bs_widget_test_setup_page_detail_is_plain_text() -> i32;
        fn bs_widget_test_setup_page_closes_its_pty() -> i32;
        fn bs_widget_test_terminal_parses_its_rows_once_per_frame() -> i32;
    }

    /// Every widget check, in one run of one thread.
    ///
    /// The codes are printed as they come back rather than only at the end: a
    /// use-after-free that takes the process with it leaves no assertion
    /// message behind, and the last line printed is then what says which check
    /// was running.
    #[test]
    fn widgets_behave_offscreen() {
        if skip_without_qt("qobject_smoke::cpp_widgets") {
            return;
        }
        let checks: [(&str, unsafe extern "C" fn() -> i32); 6] = [
            (
                "EditorArea closes a tab under a destroyed workspace",
                bs_widget_test_editor_area_survives_a_destroyed_workspace,
            ),
            (
                "ChangesToolbar cancels an action whose workspace moved",
                bs_widget_test_changes_toolbar_cancels_a_switched_workspace,
            ),
            (
                "ChangesToolbar acts on the workspace it confirmed",
                bs_widget_test_changes_toolbar_acts_on_the_confirmed_workspace,
            ),
            (
                "SetupPage renders a daemon detail as text",
                bs_widget_test_setup_page_detail_is_plain_text,
            ),
            (
                "SetupPage closes the PTY it opened",
                bs_widget_test_setup_page_closes_its_pty,
            ),
            (
                "TerminalWidget parses its rows once per frame",
                bs_widget_test_terminal_parses_its_rows_once_per_frame,
            ),
        ];

        // SAFETY: each entry is a C function in this crate's own C++, takes
        // nothing, returns a code and touches only widgets it builds itself.
        // They run in sequence on this thread, which is the thread
        // `bs_widget_test_begin` builds the QApplication on.
        unsafe { bs_widget_test_begin() };
        let mut failed = Vec::new();
        for (what, run) in checks {
            let code = unsafe { run() };
            eprintln!("cpp_widgets: {what} -> {code}");
            if code != 0 {
                failed.push(format!("{what} (code {code})"));
            }
        }
        assert!(
            failed.is_empty(),
            "offscreen widget checks failed: {failed:?}"
        );
    }
}
