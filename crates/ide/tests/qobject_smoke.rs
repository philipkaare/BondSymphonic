//! The pure-Rust half of the QObject invokables.
//!
//! `GroupModel` is a thin shell over [`Workspaces`]: every invokable parses
//! JSON, calls one method, and re-serialises. The QObject wrappers themselves
//! need a Qt event loop, so they are exercised by the widgets in later tasks;
//! what is testable here is that the JSON the invokables move across the
//! boundary survives the round trip, including the state the widgets read back
//! out of `state_json`.

use bondsymphonic_ide::highlight::theme::Theme;
use bondsymphonic_ide::model::app_state::{AgentTab, TabStatus, Workspaces};
use bondsymphonic_ide::model::file_tree::FileTree;
use bondsymphonic_ide::qobjects::changes_model::touches_workspace;
use bondsymphonic_ide::qobjects::editor_document::{
    build_buffer, may_install_disk_text, normalise_line_separators, read_only_reason,
    HIGHLIGHT_MAX_BYTES,
};
use bondsymphonic_proto::{
    AgentAdapterKind, Event, FileEntry, FileStatus, PtyId, ReadFileResult, WorkspaceId,
    WorkspaceInfo, WorkspaceState,
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
        runs: Vec::new(),
    }
}

fn tab(info: &WorkspaceInfo) -> AgentTab {
    AgentTab {
        workspace_id: info.id.clone(),
        name: info.name.clone(),
        repo_path: info.repo_path.clone(),
        branch: info.branch.clone(),
        status: TabStatus::from_workspace_state(&info.state),
        detail: String::new(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
        agent_id: None,
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
