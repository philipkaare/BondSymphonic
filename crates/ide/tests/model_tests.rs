use bondsymphonic_ide::model::app_state::*;
use bondsymphonic_ide::model::file_tree::*;
use bondsymphonic_ide::model::persistence::PersistedGroup;
use bondsymphonic_proto::*;

fn info(id: &str, name: &str, state: WorkspaceState) -> WorkspaceInfo {
    WorkspaceInfo {
        id: id.into(),
        name: name.into(),
        repo_path: "/r".into(),
        base_branch: "main".into(),
        branch: format!("bs/{name}/work"),
        worktree_path: "/w".into(),
        created_at: "t".into(),
        allowlist: vec![],
        kind: bondsymphonic_proto::WorkspaceKind::Worktree,
        state,
        agents: vec![],
        agent_records: vec![],
        runs: vec![],
    }
}
fn tab(id: &str, name: &str) -> AgentTab {
    AgentTab {
        workspace_id: id.into(),
        name: name.into(),
        repo_path: "/r".into(),
        branch: format!("bs/{name}/work"),
        base_branch: "main".into(),
        status: TabStatus::Creating,
        detail: String::new(),
        worktree_path: format!("/wt/{name}"),
        kind: bondsymphonic_proto::WorkspaceKind::default(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
        run_config: None,
        options_json: String::new(),
        agent_id: None,
        agent_status: None,
        agent_detail: String::new(),
        op_error: None,
        attention: String::new(),
        workspace_problem: None,
    }
}

#[test]
fn groups_tabs_and_active_selection() {
    let mut w = Workspaces::new_default();
    assert_eq!(w.groups[0].name, "Default");
    let g = w.add_group("Frontend");
    assert_eq!(w.add_tab(g, tab("ws_1", "a")), (1, 0));
    assert_eq!(w.add_tab(0, tab("ws_2", "b")), (0, 0));
    assert_eq!(w.active().unwrap().name, "b");
    assert!(w.set_active(1, 0));
    assert_eq!(w.active().unwrap().workspace_id, WorkspaceId::from("ws_1"));
    assert!(w.remove_workspace(&"ws_1".into()));
    assert!(
        w.active().is_some(),
        "active must fall back to an existing tab"
    );
    assert!(!w.remove_workspace(&"ws_1".into()));
    let json = w.to_json();
    assert_eq!(Workspaces::from_json(&json).unwrap(), w);
}

#[test]
fn workspace_info_updates_status_and_reconcile_drops_and_adds() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "a"));
    assert_eq!(
        w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Ready)),
        Some((0, 0))
    );
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Idle);
    assert_eq!(
        w.apply_workspace_info(&info("ws_1", "a", WorkspaceState::Error("boom".into()))),
        Some((0, 0))
    );
    assert_eq!(w.groups[0].tabs[0].status, TabStatus::Error);
    assert_eq!(w.groups[0].tabs[0].detail, "boom");
    assert_eq!(
        w.apply_workspace_info(&info("ws_9", "z", WorkspaceState::Ready)),
        None
    );

    w.reconcile(&[info("ws_9", "z", WorkspaceState::SandboxDown)]);
    assert!(
        w.find(&"ws_1".into()).is_none(),
        "ws_1 gone from daemon -> dropped"
    );
    let (g, t) = w.find(&"ws_9".into()).unwrap();
    assert_eq!(w.groups[g].name, "Unsorted");
    assert_eq!(w.groups[g].tabs[t].status, TabStatus::SandboxDown);
}

#[test]
fn reconcile_preserves_active_tab_identity_when_possible() {
    let build = || {
        let mut w = Workspaces::new_default();
        w.add_tab(0, tab("ws_a", "a"));
        w.add_tab(0, tab("ws_b", "b"));
        w.add_tab(0, tab("ws_c", "c"));
        assert!(w.set_active(0, 1)); // "b" is active
        w
    };

    // Dropping an unrelated tab positioned before the active one in the same
    // group must not silently move `active` onto whatever slid into its slot.
    let mut w = build();
    w.reconcile(&[
        info("ws_b", "b", WorkspaceState::Ready),
        info("ws_c", "c", WorkspaceState::Ready),
    ]);
    assert_eq!(w.active().unwrap().workspace_id, WorkspaceId::from("ws_b"));

    // Dropping the active tab itself falls back to another tab in the same group.
    let mut w = build();
    w.reconcile(&[
        info("ws_a", "a", WorkspaceState::Ready),
        info("ws_c", "c", WorkspaceState::Ready),
    ]);
    assert_eq!(w.active_group, 0);
    assert_ne!(w.active().unwrap().workspace_id, WorkspaceId::from("ws_b"));

    // Dropping every tab leaves no active tab.
    let mut w = build();
    w.reconcile(&[]);
    assert!(w.active().is_none());
}

#[test]
fn tab_status_glyphs_and_mapping() {
    assert_eq!(
        TabStatus::from_workspace_state(&WorkspaceState::Creating),
        TabStatus::Creating
    );
    assert_eq!(
        TabStatus::from_workspace_state(&WorkspaceState::Destroying),
        TabStatus::Done
    );
    let all = [
        TabStatus::Idle,
        TabStatus::Working,
        TabStatus::WaitingPermission,
        TabStatus::Error,
        TabStatus::Done,
        TabStatus::Creating,
        TabStatus::SandboxDown,
    ];
    let glyphs: std::collections::HashSet<&str> = all.iter().map(|s| s.glyph()).collect();
    assert_eq!(glyphs.len(), all.len());
}

#[test]
fn file_tree_cache_paths_and_invalidation() {
    let mut t = FileTree::new();
    assert!(!t.is_loaded(""));
    let e = |n: &str, d: bool| FileEntry {
        name: n.into(),
        is_dir: d,
        size: 0,
        status: FileStatus::Unchanged,
    };
    t.set_dir("", vec![e("src", true), e("README.md", false)]);
    t.set_dir("src", vec![e("main.rs", false)]);
    assert_eq!(FileTree::child_path("", "src"), "src");
    assert_eq!(FileTree::child_path("src", "main.rs"), "src/main.rs");
    assert!(t.is_loaded("src"));
    t.invalidate("");
    assert!(!t.is_loaded("") && !t.is_loaded("src"));
    let json = FileTree::entries_json(&[e("a", true)]);
    assert!(json.contains("\"is_dir\":true"));
}

// --- Task 4: terminal grid ---------------------------------------------------

use bondsymphonic_ide::model::terminal_grid::{key_to_bytes, qt, Appearance, Rgb, TerminalGrid};

#[test]
fn grid_renders_text_and_tracks_cursor() {
    let mut g = TerminalGrid::new(20, 5);
    g.feed(b"hello\r\nworld");
    let rows = g.rows();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].text.trim_end(), "hello");
    assert_eq!(rows[1].text.trim_end(), "world");
    assert_eq!(g.cursor(), (5, 1, true));
    g.resize(10, 3);
    assert_eq!(g.size(), (10, 3));
}

#[test]
fn grid_spans_carry_colors_and_bold() {
    let mut g = TerminalGrid::new(20, 3);
    g.feed(b"\x1b[1;31mred\x1b[0m plain");
    let row = &g.rows()[0];
    let red = row.spans.iter().find(|s| s.start == 0).unwrap();
    assert!(red.bold);
    assert!(
        !red.fg.is_empty(),
        "fg colour must be set for the red span: {red:?}"
    );
    let plain = row
        .spans
        .iter()
        .find(|s| s.start >= 3 && s.len > 0 && !s.bold)
        .unwrap();
    assert!(plain.fg.is_empty() || plain.fg != red.fg);
    let json = g.rows_json();
    assert!(json.contains("\"spans\""));
}

#[test]
fn grid_scrollback_and_marker() {
    let mut g = TerminalGrid::new(10, 3);
    for i in 0..10 {
        g.feed(format!("line{i}\r\n").as_bytes());
    }
    assert!(g.rows()[0].text.starts_with("line8") || g.rows()[0].text.starts_with("line7"));
    g.scroll(5);
    assert!(g.rows()[0].text.starts_with("line"));
    assert!(!g.cursor().2, "cursor hidden while scrolled into history");
    g.scroll_to_bottom();
    assert!(g.cursor().2);
    g.insert_marker("[output dropped]");
    // The marker is 16 characters wide and this terminal is 10 columns, so the
    // parser wraps it over two physical rows; rows() renders physical rows, so
    // the assertion is made against the joined visible text.
    let visible: String = g.rows().iter().map(|r| r.text.as_str()).collect();
    assert!(
        visible.contains("[output dropped]"),
        "marker missing from visible text: {visible:?}"
    );
}

#[test]
fn key_mapping() {
    assert_eq!(key_to_bytes(qt::KEY_RETURN, 0, "\r", false), b"\r");
    assert_eq!(key_to_bytes(qt::KEY_BACKSPACE, 0, "\x08", false), b"\x7f");
    assert_eq!(key_to_bytes(qt::KEY_UP, 0, "", false), b"\x1b[A");
    assert_eq!(key_to_bytes(qt::KEY_UP, 0, "", true), b"\x1bOA");
    assert_eq!(key_to_bytes(qt::KEY_F1, 0, "", false), b"\x1bOP");
    assert_eq!(key_to_bytes(qt::KEY_F1 + 4, 0, "", false), b"\x1b[15~");
    assert_eq!(key_to_bytes('C' as i32, qt::MOD_CTRL, "", false), b"\x03");
    assert_eq!(key_to_bytes(qt::KEY_TAB, 0, "\t", false), b"\t");
    assert_eq!(key_to_bytes(qt::KEY_ESCAPE, 0, "", false), b"\x1b");
    assert_eq!(key_to_bytes('a' as i32, 0, "a", false), b"a");
    assert_eq!(key_to_bytes('a' as i32, qt::MOD_ALT, "a", false), b"\x1ba");
    assert_eq!(key_to_bytes(qt::KEY_PAGEUP, 0, "", false), b"\x1b[5~");
    assert!(key_to_bytes(
        0x0100_0020, /* Shift key alone */
        qt::MOD_SHIFT,
        "",
        false
    )
    .is_empty());
}

#[test]
fn altgr_characters_pass_through_as_text() {
    // Windows reports AltGr as Ctrl+Alt; on a Danish layout that is how @ and
    // the brackets are typed, and the composed character arrives in the text.
    let altgr = qt::MOD_CTRL | qt::MOD_ALT;
    assert_eq!(key_to_bytes('@' as i32, altgr, "@", false), b"@");
    assert_eq!(key_to_bytes('{' as i32, altgr, "{", false), b"{");
    assert_eq!(key_to_bytes('}' as i32, altgr, "}", false), b"}");
    assert_eq!(key_to_bytes('[' as i32, altgr, "[", false), b"[");
    assert_eq!(key_to_bytes(']' as i32, altgr, "]", false), b"]");
    // Ctrl+Alt with no composed character keeps the control-code behaviour.
    assert_eq!(key_to_bytes('C' as i32, altgr, "", false), b"\x1b\x03");
    // Plain Ctrl and plain Alt are unchanged.
    assert_eq!(key_to_bytes('C' as i32, qt::MOD_CTRL, "", false), b"\x03");
    assert_eq!(key_to_bytes('a' as i32, qt::MOD_ALT, "a", false), b"\x1ba");
    // Special keys carry no text, so Ctrl+Alt+arrow still maps as an arrow.
    assert_eq!(key_to_bytes(qt::KEY_LEFT, altgr, "", false), b"\x1b[D");
}

#[test]
fn grid_tracks_title_and_application_cursor_keys() {
    let mut g = TerminalGrid::new(20, 3);
    assert_eq!(g.title(), None);
    assert!(!g.app_cursor_keys());
    g.feed(b"\x1b]0;my title\x07\x1b[?1h");
    assert_eq!(g.title().as_deref(), Some("my title"));
    assert!(g.app_cursor_keys());
    assert_eq!(
        key_to_bytes(qt::KEY_LEFT, 0, "", g.app_cursor_keys()),
        b"\x1bOD"
    );
}

#[test]
fn grid_renders_indexed_and_named_colours() {
    let mut g = TerminalGrid::new(10, 2);
    g.feed(b"\x1b[44;38;5;208mx");
    let rows = g.rows();
    let span = &rows[0].spans[0];
    assert_eq!(span.fg, "#ff8700", "256-colour cube entry 208");
    assert_eq!(span.bg, "#0000ee", "default xterm blue");
    // Cells past the written text fall back to the terminal defaults.
    let plain = rows[0].spans.last().unwrap();
    assert!(plain.fg.is_empty() && plain.bg.is_empty());
}

// ---------------------------------------------------------------------------
// Group names are unique (final review, Minor).
// ---------------------------------------------------------------------------

/// "Unsorted" is found by name, so a second group wearing that name would take
/// unclaimed workspaces off the first one forever: `reconcile` files them into
/// whichever comes first, and every workspace the user then dropped into the
/// other one would look, to the next restart, like it belonged nowhere.
#[test]
fn a_group_cannot_be_renamed_onto_another_groups_name() {
    let mut w = Workspaces::new_default();
    let frontend = w.add_group("Frontend");
    w.add_group(UNSORTED_GROUP);
    assert!(
        !w.rename_group(frontend, UNSORTED_GROUP),
        "renaming onto an existing name must be refused"
    );
    assert_eq!(w.groups[frontend].name, "Frontend", "the name is unchanged");
    assert_eq!(
        w.groups.iter().filter(|g| g.name == UNSORTED_GROUP).count(),
        1
    );
}

/// Any duplicate, not only "Unsorted": two groups with one name are two things
/// the user cannot tell apart in the bar.
#[test]
fn a_group_cannot_be_renamed_onto_any_other_name_in_use() {
    let mut w = Workspaces::new_default();
    let frontend = w.add_group("Frontend");
    let backend = w.add_group("Backend");
    assert!(!w.rename_group(backend, "Frontend"));
    assert_eq!(w.groups[backend].name, "Backend");
    assert_eq!(w.groups[frontend].name, "Frontend");
}

/// Renaming a group to what it is already called is not a clash with itself.
/// The dialog opens on the current name, so this is the ordinary "OK" press.
#[test]
fn renaming_a_group_to_its_own_name_is_allowed() {
    let mut w = Workspaces::new_default();
    let idx = w.add_group("Frontend");
    assert!(w.rename_group(idx, "Frontend"));
    assert_eq!(w.groups[idx].name, "Frontend");
    assert!(w.rename_group(idx, "Platform"));
    assert_eq!(w.groups[idx].name, "Platform");
}

/// The same rule from the other direction: adding a group that already exists
/// hands back the one that is there rather than making a second.
#[test]
fn adding_a_group_that_already_exists_returns_the_existing_one() {
    let mut w = Workspaces::new_default();
    let first = w.add_group("Frontend");
    let again = w.add_group("Frontend");
    assert_eq!(first, again);
    assert_eq!(w.groups.iter().filter(|g| g.name == "Frontend").count(), 1);
}

// ---------------------------------------------------------------------------
// The in-flight set behind `AppController::isWorkspaceBusy` (final review I4).
// ---------------------------------------------------------------------------

/// One operation at a time per workspace, and the second is refused rather than
/// queued: the pair that must never overlap is a merge and the destroy that
/// deletes the objects the merge is still packing out.
#[test]
fn a_workspace_takes_one_operation_at_a_time() {
    let mut busy = BusyWorkspaces::default();
    assert!(!busy.contains("ws_1"));
    assert!(busy.begin("ws_1"), "the first operation is booked in");
    assert!(busy.contains("ws_1"));
    assert!(!busy.begin("ws_1"), "the second is refused");
    assert!(busy.end("ws_1"), "the answer books it out");
    assert!(!busy.contains("ws_1"));
    assert!(busy.begin("ws_1"), "and the next one may start");
}

/// Booking is per workspace: a merge on one must not grey out another, which is
/// the whole reason this is a set rather than one id.
#[test]
fn one_busy_workspace_does_not_block_another() {
    let mut busy = BusyWorkspaces::default();
    assert!(busy.begin("ws_1"));
    assert!(busy.begin("ws_2"));
    assert!(busy.contains("ws_1") && busy.contains("ws_2"));
    assert!(busy.end("ws_1"));
    assert!(!busy.contains("ws_1"));
    assert!(busy.contains("ws_2"), "ws_2's operation is still out");
}

/// Booking out something that was never booked in changes nothing and says so,
/// which is what lets the controller emit `workspaceBusyChanged` only on a real
/// change: a failure reported for a workspace with no operation out (a bad
/// merge mode, a refused second request) must not announce that one ended.
#[test]
fn ending_an_operation_that_never_began_is_not_a_change() {
    let mut busy = BusyWorkspaces::default();
    assert!(!busy.end("ws_1"));
    assert!(busy.is_empty());
    assert!(busy.begin("ws_1"));
    assert!(busy.end("ws_1"));
    assert!(
        !busy.end("ws_1"),
        "and ending it twice is not a change either"
    );
    assert!(busy.is_empty());
}

// ---------------------------------------------------------------------------
// Restoring an agent tab from `WorkspaceInfo.agents` (final review I3).
// ---------------------------------------------------------------------------

fn agent(id: &str, adapter: AgentAdapterKind) -> AgentSummary {
    AgentSummary {
        id: AgentId(id.to_owned()),
        adapter,
        state: AgentState::Exited,
        session_id: None,
        command: None,
        model: None,
        permission_mode: None,
    }
}

/// The daemon keeps a Claude agent's record and its transcript across its own
/// restart, and lists it in `WorkspaceInfo.agents`. Without reading that, an IDE
/// restart rebuilt every Claude workspace as a terminal tab bound to no agent,
/// and nothing in the UI could reach a transcript the daemon was still serving.
#[test]
fn a_tab_built_from_the_daemons_list_adopts_the_workspaces_agent() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agent_records = vec![AgentSummary {
        model: Some("opus".into()),
        permission_mode: Some("acceptEdits".into()),
        session_id: Some("sess-1".into()),
        ..agent("ag_1", AgentAdapterKind::Claude)
    }];
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.adapter, AgentAdapterKind::Claude);
    assert_eq!(tab.agent_id, Some(AgentId("ag_1".into())));
    // The options the agent was started with come back too, so a Restart
    // resumes on the model and permission mode the user chose rather than on
    // the daemon's defaults.
    let options: serde_json::Value = serde_json::from_str(&tab.options_json).expect("an object");
    assert_eq!(options["model"], "opus");
    assert_eq!(options["permission_mode"], "acceptEdits");
    // Never the API key: there is no field in `AgentSummary` for one, and
    // nothing here invents a place to put it.
    assert!(options.get("api_key").is_none());
}

/// The last entry is the workspace's most recent agent -- the daemon hands
/// restored agents their ordinals before any new one can take theirs -- so a
/// workspace whose first agent ended and was restarted comes back on the second.
#[test]
fn the_latest_agent_is_the_one_the_tab_reattaches_to() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agent_records = vec![
        agent("ag_old", AgentAdapterKind::Claude),
        agent("ag_new", AgentAdapterKind::Claude),
    ];
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.agent_id, Some(AgentId("ag_new".into())));
}

/// A workspace the daemon has no agent for is a Claude tab with no agent yet,
/// not a terminal.
///
/// It is a guess either way -- the daemon records the adapter of an *agent*,
/// and there is none -- and this is the guess that leaves the user somewhere
/// they can work. A Claude workspace whose agent was never started, or whose
/// `agent.start` failed, used to come back after an IDE restart as a shell:
/// the pane that could have offered a prompt was not the pane that was built.
#[test]
fn a_workspace_with_no_agents_is_a_claude_tab_with_no_agent() {
    let w = info("ws_1", "alpha", WorkspaceState::Ready);
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.adapter, AgentAdapterKind::Claude);
    assert_eq!(tab.agent_id, None);
    assert_eq!(tab.options_json, "");
}

/// A terminal workspace is still a terminal workspace tomorrow.
///
/// The daemon cannot answer for this one: it records what an agent ran under,
/// and a terminal workspace has no agent record to read. So the choice is kept
/// in `state.json` beside the command, and it is only consulted when the
/// daemon has nothing to say -- an agent that actually ran beats a preference
/// saved months ago.
#[test]
fn a_terminal_workspace_is_remembered_as_one() {
    let mut model = Workspaces::new_default();
    let listed = info("ws_1", "alpha", WorkspaceState::Ready);
    let mut terminal = AgentTab::from_workspace_info(&listed);
    terminal.adapter = AgentAdapterKind::Terminal;
    terminal.command = Some("htop".to_owned());
    model.add_tab(0, terminal);

    let persisted = model.persisted_groups();
    let restored = Workspaces::from_persisted(&persisted, std::slice::from_ref(&listed), None);
    let tab = restored.active().expect("the only tab");
    assert_eq!(tab.adapter, AgentAdapterKind::Terminal);
    assert_eq!(tab.command.as_deref(), Some("htop"));

    // A Claude tab writes no adapter at all -- it is the default -- and comes
    // back as one regardless.
    let mut claude = Workspaces::new_default();
    claude.add_tab(0, AgentTab::from_workspace_info(&listed));
    let persisted = claude.persisted_groups();
    let restored = Workspaces::from_persisted(&persisted, std::slice::from_ref(&listed), None);
    assert_eq!(
        restored.active().expect("the only tab").adapter,
        AgentAdapterKind::Claude
    );

    // And once the daemon has an agent for the workspace, the agent is what
    // answers: it says what actually ran.
    let mut with_agent = listed;
    with_agent.agent_records = vec![agent("ag_1", AgentAdapterKind::Claude)];
    let mut model = Workspaces::new_default();
    let mut terminal = AgentTab::from_workspace_info(&with_agent);
    terminal.adapter = AgentAdapterKind::Terminal;
    model.add_tab(0, terminal);
    let persisted = model.persisted_groups();
    let restored = Workspaces::from_persisted(&persisted, &[with_agent], None);
    assert_eq!(
        restored.active().expect("the only tab").adapter,
        AgentAdapterKind::Claude
    );
}

/// An agent with nothing but an id and an adapter leaves the options empty
/// rather than writing an object of nulls: `TranscriptModel::restartOptions`
/// treats an empty string as "the daemon's defaults", which is what it is.
#[test]
fn an_agent_started_with_no_options_leaves_the_options_empty() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agent_records = vec![agent("ag_1", AgentAdapterKind::Claude)];
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.agent_id, Some(AgentId("ag_1".into())));
    assert_eq!(tab.options_json, "");
}

/// The whole point of the fix: the restore path, end to end. A session file
/// naming the workspace plus the daemon's list rebuilds the Claude tab with its
/// agent, which is what the transcript pane attaches to.
#[test]
fn a_restored_session_brings_back_the_claude_tab_with_its_agent() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agent_records = vec![agent("ag_1", AgentAdapterKind::Claude)];
    let persisted = vec![PersistedGroup {
        name: "Backend".to_owned(),
        workspace_ids: vec!["ws_1".to_owned()],
        tabs: Vec::new(),
    }];
    let model = Workspaces::from_persisted(&persisted, std::slice::from_ref(&w), Some("ws_1"));
    let tab = model.active().expect("the restored tab is active");
    assert_eq!(tab.adapter, AgentAdapterKind::Claude);
    assert_eq!(tab.agent_id, Some(AgentId("ag_1".into())));

    // And the same through `reconcile`, which is the path a workspace the
    // session file did not name takes into "Unsorted".
    let mut fresh = Workspaces::new_default();
    fresh.reconcile(std::slice::from_ref(&w));
    let (g, t) = fresh.find(&WorkspaceId("ws_1".into())).expect("filed");
    assert_eq!(fresh.groups[g].tabs[t].adapter, AgentAdapterKind::Claude);
    assert_eq!(
        fresh.groups[g].tabs[t].agent_id,
        Some(AgentId("ag_1".into()))
    );
}

// ---------------------------------------------------------------------------
// Group ids survive a restore, a rename and a removal (M7 Task 3).
// ---------------------------------------------------------------------------

/// Group ids are the handle the C++ side holds on to between two reads of
/// `state_json`, so the model must never hand the same id to two groups.
///
/// The positional scheme did: `grp_<len>` is `grp_1` both for the second group
/// ever added and for the group added after the second of three was removed.
/// A restore made that worse by seeding the ids from the persisted order, so
/// the very first group the user closed after a restart put the model into that
/// state. A rename is in here too because renaming is what the user does before
/// closing a group, and the id must not move with the name.
#[test]
fn group_ids_are_unique_across_a_restore_a_rename_and_a_removal() {
    let list = vec![
        info("ws_1", "alpha", WorkspaceState::Ready),
        info("ws_2", "beta", WorkspaceState::Ready),
        info("ws_3", "gamma", WorkspaceState::Ready),
    ];
    let persisted = vec![
        PersistedGroup {
            name: "Frontend".to_owned(),
            workspace_ids: vec!["ws_1".to_owned()],
            tabs: Vec::new(),
        },
        PersistedGroup {
            name: "Backend".to_owned(),
            workspace_ids: vec!["ws_2".to_owned()],
            tabs: Vec::new(),
        },
        PersistedGroup {
            name: "Platform".to_owned(),
            workspace_ids: vec!["ws_3".to_owned()],
            tabs: Vec::new(),
        },
    ];
    let mut w = Workspaces::from_persisted(&persisted, &list, Some("ws_1"));
    let restored: Vec<String> = w.groups.iter().map(|g| g.id.clone()).collect();
    assert_eq!(restored.len(), 3);

    // A rename keeps the id: the C++ side is still holding the one it read.
    assert!(w.rename_group(0, "Web"));
    assert_eq!(w.groups[0].id, restored[0], "a rename must not move the id");
    assert_eq!(w.groups[0].name, "Web");

    // Closing a group and making another must not reissue an id that is still
    // in use. The tab in the closed group moves to "Unsorted", which is itself
    // a group this has to number.
    assert!(w.remove_group("Backend"));
    w.add_group("Mobile");
    let ids: Vec<&str> = w.groups.iter().map(|g| g.id.as_str()).collect();
    let mut unique: Vec<&str> = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "duplicate group ids in {ids:?}");
    // And the groups that did not move still answer to the ids they were read
    // with, which is the whole point of not deriving them from the position.
    assert_eq!(w.groups[0].id, restored[0]);
    assert_eq!(
        w.groups
            .iter()
            .find(|g| g.name == "Platform")
            .map(|g| &g.id),
        Some(&restored[2])
    );
}

/// The counter travels with the model through `state_json`, so a round trip
/// cannot reset it and start handing out ids that are already taken.
#[test]
fn a_serialised_model_keeps_handing_out_fresh_group_ids() {
    let mut w = Workspaces::new_default();
    w.add_group("Frontend");
    w.add_group("Backend");
    let restored = Workspaces::from_json(&w.to_json()).expect("state json round trips");
    let mut restored = restored;
    assert!(restored.remove_group("Frontend"));
    restored.add_group("Platform");
    let ids: Vec<&str> = restored.groups.iter().map(|g| g.id.as_str()).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "duplicate group ids in {ids:?}");
}

// ---------------------------------------------------------------------------
// Restart resumes from the daemon's session id (M7 Task 3).
// ---------------------------------------------------------------------------

/// The transcript is the first place a resumable session id is looked for, but
/// it is not the only one: a history that could not be read, or one that never
/// carried a `result` message, leaves the transcript with no id at all while
/// the daemon still holds the one the agent reported. The tab carries that id
/// so a Restart resumes instead of quietly beginning a fresh conversation.
#[test]
fn a_restored_tab_carries_the_daemons_session_id_for_a_restart() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agent_records = vec![AgentSummary {
        session_id: Some("sess-daemon".into()),
        model: Some("opus".into()),
        ..agent("ag_1", AgentAdapterKind::Claude)
    }];
    let tab = AgentTab::from_workspace_info(&w);
    let options: serde_json::Value = serde_json::from_str(&tab.options_json).expect("an object");
    assert_eq!(options["resume_session"], "sess-daemon");
    assert_eq!(options["model"], "opus");
}

// ---------------------------------------------------------------------------
// A permission raised in a background tab asks for attention (M7 Task 3).
// ---------------------------------------------------------------------------

/// The permission bar lives on the workspace's own pane, so a question raised
/// by an agent in a tab the user is not looking at is invisible until they
/// happen to switch to it. The tab model carries a line of attention text for
/// exactly that: the tab bar paints a dot, the status bar shows the sentence.
#[test]
fn workspace_attention_is_set_and_cleared_by_workspace_id() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "alpha"));
    w.add_tab(0, tab("ws_2", "beta"));
    let beta = WorkspaceId("ws_2".into());

    assert_eq!(w.attention(), None, "nothing is waiting to begin with");
    assert!(w.set_workspace_attention(&beta, &permission_attention("beta")));
    assert_eq!(
        w.attention(),
        Some("beta is waiting for permission"),
        "the status bar hint names the agent that is waiting"
    );
    let (g, t) = w.find(&beta).expect("beta is tracked");
    assert_eq!(
        w.groups[g].tabs[t].attention,
        "beta is waiting for permission"
    );

    assert!(w.clear_workspace_attention(&beta));
    assert_eq!(w.attention(), None);
    assert!(
        !w.clear_workspace_attention(&beta),
        "clearing twice is not a change"
    );
    assert!(
        !w.set_workspace_attention(&WorkspaceId("ws_missing".into()), "x"),
        "a workspace this model does not track cannot be marked"
    );
}

// ---------------------------------------------------------------------------
// A long transcript collapses above 2,000 items (IDE design §13, M7 Task 3).
// ---------------------------------------------------------------------------

/// One frame per item is what the transcript view builds, so a session that
/// runs for a day would eventually hold tens of thousands of widgets. IDE
/// design §13 claims the list is capped at 2,000; this is the cap.
///
/// The oldest items are not thrown away, they are folded into one "load
/// earlier" block that puts them all back when the user clicks it.
#[test]
fn a_long_transcript_folds_its_oldest_items_into_one_load_earlier_block() {
    use bondsymphonic_ide::model::transcript::{Transcript, TranscriptItem, MAX_LIVE_ITEMS};
    use bondsymphonic_proto::{AgentMessage, AgentMessageBody};

    let fixture: Vec<AgentMessage> = (0..2_500)
        .map(|seq| AgentMessage {
            seq,
            ts: "2026-09-09T10:00:00Z".to_owned(),
            body: AgentMessageBody::UserText {
                text: format!("message {seq}"),
            },
        })
        .collect();

    let mut t = Transcript::default();
    for message in &fixture {
        t.apply(message);
    }

    assert!(
        t.items.len() <= MAX_LIVE_ITEMS + 1,
        "{} live items after 2,500 messages",
        t.items.len()
    );
    assert_eq!(MAX_LIVE_ITEMS, 2_000, "the number IDE design §13 claims");

    // The block is first, it counts what it is holding, and its label is the
    // one the view paints.
    let folded = 2_500 - (t.items.len() - 1);
    match &t.items[0] {
        TranscriptItem::Earlier { count, text } => {
            assert_eq!(*count, folded);
            assert_eq!(text, &format!("Load earlier ({folded})"));
        }
        other => panic!("the first item is not a load-earlier block: {other:?}"),
    }
    // Nothing was lost: the newest message is still the last live item.
    assert!(matches!(
        t.items.last(),
        Some(TranscriptItem::User { text }) if text == "message 2499"
    ));

    // Clicking it puts every message back, block and all.
    assert!(t.expand_earlier(), "the block expands");
    assert_eq!(t.items.len(), 2_500);
    assert!(matches!(
        &t.items[0],
        TranscriptItem::User { text } if text == "message 0"
    ));
    assert!(
        !t.expand_earlier(),
        "a transcript holding nothing back does not claim to have expanded"
    );

    // Sticky: messages arriving after the click do not fold the list back up.
    // The cap is a default for a conversation nobody asked to read all of, and
    // a second fold would take the earlier half away while the user was still
    // reading it.
    for seq in 2_500..2_600 {
        t.apply(&AgentMessage {
            seq,
            ts: "2026-09-09T10:00:00Z".to_owned(),
            body: AgentMessageBody::UserText {
                text: format!("message {seq}"),
            },
        });
    }
    assert_eq!(
        t.items.len(),
        2_600,
        "an expanded transcript stays expanded"
    );
    assert!(matches!(
        &t.items[0],
        TranscriptItem::User { text } if text == "message 0"
    ));
    assert_eq!(t.earlier_count(), 0);
}

// ---------------------------------------------------------------------------
// Task 9: tab selection after a close, and who owns the badge while the
// sandbox is down.
// ---------------------------------------------------------------------------

/// Closing a tab selects the one beside it, not a tab in some other group.
///
/// The old repair only decremented `active_tab` when the removed tab was
/// *before* the active one, so closing the active tab left the index pointing
/// past the end of its group and the fallback jumped to the first non-empty
/// group -- from the tab the user was working in to whatever happened to be
/// first in the sidebar.
#[test]
fn closing_a_tab_selects_its_left_neighbour_in_the_same_group() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_a", "a"));
    let feature = w.add_group("Feature");
    w.add_tab(feature, tab("ws_b", "b"));
    w.add_tab(feature, tab("ws_c", "c"));
    assert_eq!(w.active().map(|t| t.name.as_str()), Some("c"));

    assert!(w.remove_workspace(&"ws_c".into()));
    assert_eq!(
        w.active().map(|t| t.name.as_str()),
        Some("b"),
        "the left neighbour in the group the user was working in"
    );

    // The only tab of a group: the first tab of the previous group, which is
    // the one the sidebar shows above it.
    assert!(w.remove_workspace(&"ws_b".into()));
    assert_eq!(w.active().map(|t| t.name.as_str()), Some("a"));
}

/// With no group above it, the selection falls forward instead.
#[test]
fn closing_the_only_tab_of_the_first_group_falls_forward() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_a", "a"));
    let later = w.add_group("Later");
    w.add_tab(later, tab("ws_b", "b"));
    assert!(w.set_active(0, 0));

    assert!(w.remove_workspace(&"ws_a".into()));
    assert_eq!(w.active().map(|t| t.name.as_str()), Some("b"));
}

/// Closing a tab the user is not looking at leaves the selection where it is.
#[test]
fn closing_another_tab_does_not_move_the_selection() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_a", "a"));
    w.add_tab(0, tab("ws_b", "b"));
    w.add_tab(0, tab("ws_c", "c"));
    assert!(w.set_active(0, 2));

    // One before the active tab: the index shifts, the workspace does not.
    assert!(w.remove_workspace(&"ws_a".into()));
    assert_eq!(w.active().map(|t| t.name.as_str()), Some("c"));
    // One after it: nothing moves at all.
    assert!(w.set_active(0, 0));
    assert!(w.remove_workspace(&"ws_c".into()));
    assert_eq!(w.active().map(|t| t.name.as_str()), Some("b"));
}

/// A sandbox that is down owns the tab's badge until the workspace is `Ready`
/// again. An `agent.state` event arriving in the meantime is recorded but not
/// shown: "working" painted over a sandbox that is not running says the
/// opposite of what is true, and nothing clears it, because the recovery event
/// the workspace sends is the one the tab has already stopped listening to.
#[test]
fn a_down_sandbox_keeps_the_badge_until_the_workspace_is_ready() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "alpha"));
    let ws = WorkspaceId::from("ws_1");
    let agent = AgentId("ag_1".into());
    assert!(w.set_agent(&ws, agent.clone()));

    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::SandboxDown));
    assert_eq!(
        w.active().map(|t| t.status),
        Some(TabStatus::SandboxDown),
        "the workspace's own news"
    );

    assert!(w
        .set_agent_status(&agent, TabStatus::Working, "thinking")
        .is_some());
    let showing = w.active().expect("a tab");
    assert_eq!(
        showing.status,
        TabStatus::SandboxDown,
        "the workspace still owns the badge"
    );
    assert_eq!(
        showing.agent_status,
        Some(TabStatus::Working),
        "the agent's own status is recorded all the same"
    );
    assert_eq!(showing.agent_detail, "thinking");

    // Recovery hands the badge back, carrying what the agent last said.
    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::Ready));
    let showing = w.active().expect("a tab");
    assert_eq!(showing.status, TabStatus::Working);
    assert_eq!(showing.detail, "thinking");
}

/// A key pasted out of a browser or a terminal carries whitespace often enough
/// that storing it verbatim is how a perfectly good key ends up rejected by the
/// API, with nothing in the IDE to suggest why. A field holding nothing but
/// spaces is an empty field, which means "leave the stored key alone".
#[test]
fn an_api_key_is_trimmed_before_it_is_stored() {
    use bondsymphonic_ide::qobjects::settings::normalise_api_key;

    assert_eq!(normalise_api_key(" sk-x "), Some("sk-x"));
    assert_eq!(normalise_api_key("sk-x\r\n"), Some("sk-x"));
    assert_eq!(normalise_api_key("\tsk-x"), Some("sk-x"));
    assert_eq!(normalise_api_key("   "), None);
    assert_eq!(normalise_api_key("\n"), None);
    assert_eq!(normalise_api_key(""), None);
}

// ---------------------------------------------------------------------------
// Pasting into a terminal.
// ---------------------------------------------------------------------------

/// The step the login flow ends on: `claude auth login` prints its URL, the
/// browser hands back a code, and the terminal's `Paste code here if prompted >`
/// is waiting for it. A code copied out of a browser usually arrives with the
/// line break the copy picked up, and that break has to be the carriage return
/// Enter sends or the prompt never sees a line at all.
#[test]
fn a_paste_is_the_text_a_program_would_have_been_typed() {
    use bondsymphonic_ide::model::terminal_grid::paste_bytes;

    assert_eq!(paste_bytes("code-123\n", false), b"code-123\r");
    // Both line-break conventions, and either one already a carriage return.
    assert_eq!(paste_bytes("a\r\nb\nc\r", false), b"a\rb\rc\r");
    // A tab is a character a pasted line legitimately contains.
    assert_eq!(paste_bytes("a\tb\x07c", false), b"a\tbc");
    // An escape sequence on the clipboard is text somebody copied, not a
    // command for the terminal: the ESC goes and the rest stays visible.
    assert_eq!(paste_bytes("\x1b[31mred", false), b"[31mred");
    assert!(paste_bytes("", false).is_empty());
    assert!(paste_bytes("\x1b", false).is_empty());
}

/// A program that asked for bracketed paste is told where the block begins and
/// ends, so it can take it as data rather than as the keystrokes it looks
/// like. Claude Code's prompt asks for it, which is why a multi-line paste into
/// it must not arrive as a series of Enters.
#[test]
fn a_bracketed_paste_is_wrapped_and_cannot_be_broken_out_of() {
    use bondsymphonic_ide::model::terminal_grid::paste_bytes;

    assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~");
    // The end marker cannot be forged from the clipboard: its ESC leaves with
    // every other control character, so the wrapper still bounds the paste.
    assert_eq!(
        paste_bytes("a\x1b[201~b", true),
        b"\x1b[200~a[201~b\x1b[201~"
    );
    // Nothing to paste sends nothing at all, brackets included: an empty
    // wrapper is a keystroke the program never asked for.
    assert!(paste_bytes("", true).is_empty());
    assert!(paste_bytes("\0", true).is_empty());
}

/// Which of the two the terminal is in is the application's to say, and the
/// grid is what heard it say so.
#[test]
fn grid_tracks_bracketed_paste_mode() {
    let mut g = TerminalGrid::new(20, 3);
    assert!(!g.bracketed_paste());
    g.feed(b"\x1b[?2004h");
    assert!(g.bracketed_paste(), "DECSET 2004 turns it on");
    g.feed(b"\x1b[?2004l");
    assert!(!g.bracketed_paste(), "DECRST 2004 turns it off again");
}

/// The bug that stopped a GitHub sign-in dead: `gh auth login` asks the
/// terminal where the cursor is before each of its yes/no prompts and reads
/// nothing until the report arrives, so a terminal that drops the question
/// leaves "Authenticate Git with your GitHub credentials? (Y/n)" on screen
/// refusing every key. The sequence here is the one `gh` sends: park the
/// cursor past the far corner, then ask where it ended up, which is how a
/// program measures a screen it was not told the size of.
#[test]
fn a_cursor_position_query_is_answered() {
    let mut g = TerminalGrid::new(100, 30);
    g.feed(b"\x1b[999;999f\x1b[6n");
    assert_eq!(g.take_replies(), b"\x1b[30;100R");
    // Handed over once: a second write would be a keystroke the program never
    // asked for, and at a `(Y/n)` prompt that is an answer nobody gave.
    assert!(g.take_replies().is_empty());
}

/// "What are you?", which a program asks before it trusts the terminal with
/// anything clever. Claude Code's own UI asks it on startup.
#[test]
fn a_device_attributes_query_is_answered() {
    let mut g = TerminalGrid::new(80, 24);
    g.feed(b"\x1b[c");
    let reply = g.take_replies();
    assert!(
        reply.starts_with(b"\x1b[?"),
        "a device-attributes report: {reply:?}"
    );
}

/// A CLI asks the background colour to decide whether it is drawing on a light
/// or a dark terminal. The pane's colours come from the Qt palette, which the
/// grid only knows because the widget tells it, so the answer has to carry
/// what the widget said rather than a fixed pair.
#[test]
fn a_colour_query_is_answered_with_what_the_widget_paints() {
    let mut g = TerminalGrid::new(80, 24);
    g.set_appearance(Appearance {
        fg: Rgb {
            r: 0x20,
            g: 0x21,
            b: 0x22,
        },
        bg: Rgb {
            r: 0xf0,
            g: 0xf1,
            b: 0xf2,
        },
        cell_width: 9,
        cell_height: 19,
    });
    g.feed(b"\x1b]11;?\x07");
    let reply = String::from_utf8(g.take_replies()).expect("utf-8");
    assert!(
        reply.contains("f0f0/f1f1/f2f2"),
        "the background as the widget paints it: {reply:?}"
    );

    // And a colour the program set itself wins over any of it: it did set it.
    g.feed(b"\x1b]4;1;rgb:aa/bb/cc\x07\x1b]4;1;?\x07");
    let reply = String::from_utf8(g.take_replies()).expect("utf-8");
    assert!(reply.contains("aaaa/bbbb/cccc"), "{reply:?}");
}

/// The window in pixels, which is how a program that draws images works out
/// how big a cell is. Answered from the widget's own font metrics.
#[test]
fn a_text_area_size_query_is_answered() {
    let mut g = TerminalGrid::new(80, 24);
    g.set_appearance(Appearance {
        cell_width: 9,
        cell_height: 19,
        ..Appearance::default()
    });
    g.feed(b"\x1b[14t");
    let reply = String::from_utf8(g.take_replies()).expect("utf-8");
    // 24 rows of 19 pixels by 80 columns of 9.
    assert!(reply.contains("456") && reply.contains("720"), "{reply:?}");
}

/// OSC 52 with a `?` asks the terminal to hand the clipboard to the program,
/// and this one does not. The clipboard belongs to the person at the keyboard;
/// a program in a workspace -- or a build script that printed the sequence --
/// has no business being told what they last copied. Silence is what a
/// terminal without clipboard access looks like, and every terminal is allowed
/// to be one.
#[test]
fn the_clipboard_is_never_handed_to_the_program() {
    let mut g = TerminalGrid::new(80, 24);
    g.feed(b"\x1b]52;c;?\x07");
    assert!(g.take_replies().is_empty());
}

/// Selecting text and copying it, which a terminal without a mouse cannot do:
/// the output of a command is the one thing in this IDE that cannot be opened
/// in an editor and copied from there.
#[test]
fn a_dragged_selection_is_the_text_it_was_dragged_over() {
    let mut g = TerminalGrid::new(20, 3);
    g.feed(b"hello world\r\nsecond line");
    assert!(!g.has_selection(), "nothing is selected to begin with");
    assert_eq!(g.selection_text(), "");

    // From the `w` of "world" to the end of the word.
    g.begin_selection(6, 0, false, false);
    g.extend_selection(10, 0, true);
    assert!(g.has_selection());
    assert_eq!(g.selection_text(), "world");

    // Down a line: the terminal joins the rows the way it would be read.
    g.extend_selection(5, 1, true);
    assert_eq!(g.selection_text(), "world\nsecond");

    g.clear_selection();
    assert!(!g.has_selection());
    assert_eq!(g.selection_text(), "");
}

/// A double-click takes the word under the pointer, without the user having to
/// place either end of it.
#[test]
fn a_word_selection_takes_the_whole_word() {
    let mut g = TerminalGrid::new(20, 2);
    g.feed(b"alpha beta gamma");
    g.begin_selection(7, 0, false, true);
    assert_eq!(g.selection_text(), "beta");
}

/// The selection is part of what the widget paints, and it is painted by
/// swapping the two colours -- so the spans have to say which cells are in it,
/// and they have to break where it begins and ends.
#[test]
fn selected_cells_are_marked_in_the_rows() {
    let mut g = TerminalGrid::new(20, 2);
    g.feed(b"abcdef");
    g.begin_selection(1, 0, false, false);
    g.extend_selection(3, 0, true);

    let rows = g.rows();
    let selected: Vec<usize> = rows[0]
        .spans
        .iter()
        .filter(|span| span.selected)
        .flat_map(|span| span.start..span.start + span.len)
        .collect();
    assert_eq!(selected, vec![1, 2, 3], "{:?}", rows[0].spans);
    // And the selection has no colours of its own: the widget swaps the two
    // the cell already had.
    assert!(rows[0].spans.iter().all(|span| span.fg.is_empty()));
}

/// A selection is anchored to the text, not to the screen: scrolling the
/// viewport moves the highlight with the line it was made on.
#[test]
fn a_selection_follows_its_text_through_the_scrollback() {
    let mut g = TerminalGrid::new(20, 2);
    g.feed(b"one\r\ntwo\r\nthree\r\n");
    // Two rows of screen and four lines written, so what is on it is "three"
    // and the empty line the last newline opened.
    assert_eq!(g.rows()[0].text.trim_end(), "three");
    g.begin_selection(0, 0, false, false);
    g.extend_selection(4, 0, true);
    assert_eq!(g.selection_text(), "three");
    // One line back into the history: the same text, now a row further down.
    g.scroll(1);
    assert_eq!(g.selection_text(), "three", "the text it was made on");
    let rows = g.rows();
    let selected: Vec<usize> = rows[1]
        .spans
        .iter()
        .filter(|span| span.selected)
        .flat_map(|span| span.start..span.start + span.len)
        .collect();
    assert_eq!(
        selected,
        vec![0, 1, 2, 3, 4],
        "one row further down: {rows:?}"
    );
}

/// Choosing a model or a permission mode in the composer restarts the agent,
/// and the tab has to carry the choice afterwards.
///
/// Not for the sake of the next IDE start -- a restored tab reads its options
/// off the daemon's record of what the agent was actually started with -- but
/// for the rest of this session. Workspace > Restart agent reads the tab, and
/// a tab still holding the model the workspace was created with would undo the
/// switch the moment the user pressed it.
#[test]
fn a_tab_carries_the_options_the_composer_chose() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "agent-1"));

    let chosen = r#"{"model":"claude-haiku-4-5-20251001","permission_mode":"bypassPermissions"}"#;
    assert!(w.set_tab_options(&"ws_1".into(), chosen));
    assert_eq!(w.groups[0].tabs[0].options_json, chosen);

    // An empty string is the absence of a choice, and storing it would turn the
    // model the user picked into "whatever the daemon defaults to".
    assert!(!w.set_tab_options(&"ws_1".into(), ""));
    assert_eq!(w.groups[0].tabs[0].options_json, chosen);

    // A workspace that is not tracked is not silently created.
    assert!(!w.set_tab_options(&"ws_missing".into(), chosen));
}

/// What a workspace that cannot run says about itself, which is what its pane's
/// banner shows. A sandbox that died has no reason of its own to give, so the
/// sentence says what happened; a workspace that failed to start carries the
/// daemon's reason verbatim. Every other state has nothing to report.
#[test]
fn a_workspace_that_cannot_run_describes_its_problem() {
    let down = workspace_problem(&WorkspaceState::SandboxDown).expect("a down sandbox");
    assert_eq!(down.title, "The sandbox for this workspace is not running");
    assert!(
        down.detail.contains("stopped unexpectedly"),
        "{:?}",
        down.detail
    );

    let failed = workspace_problem(&WorkspaceState::Error(
        "The worktree's git registration is missing.".into(),
    ))
    .expect("a failed workspace");
    assert_eq!(failed.title, "This workspace could not be started");
    assert_eq!(failed.detail, "The worktree's git registration is missing.");

    for fine in [
        WorkspaceState::Ready,
        WorkspaceState::Creating,
        WorkspaceState::Destroying,
    ] {
        assert_eq!(workspace_problem(&fine), None, "{fine:?}");
    }
}

/// The problem rides on the tab from the first `WorkspaceInfo` to the one that
/// says the workspace is `Ready` again, whether or not an agent is bound to
/// the tab -- the restored tabs a daemon restart leaves behind all have one.
#[test]
fn a_tab_carries_its_workspace_problem_until_the_workspace_is_ready() {
    let restored = AgentTab::from_workspace_info(&info(
        "ws_1",
        "alpha",
        WorkspaceState::Error("sandbox would not start".into()),
    ));
    assert_eq!(
        restored
            .workspace_problem
            .as_ref()
            .map(|p| p.detail.as_str()),
        Some("sandbox would not start")
    );

    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "alpha"));
    let ws = WorkspaceId::from("ws_1");
    assert!(w.set_agent(&ws, AgentId("ag_1".into())));

    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::SandboxDown));
    let showing = w.active().expect("a tab");
    assert!(showing.workspace_problem.is_some());
    // The tooltip reads `detail`, and a bare "sandbox down" there is the
    // unexplained state this exists to replace.
    assert!(
        showing.detail.contains("stopped unexpectedly"),
        "{:?}",
        showing.detail
    );

    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::Error("gone".into())));
    assert_eq!(
        w.active().and_then(|t| t.workspace_problem.clone()),
        workspace_problem(&WorkspaceState::Error("gone".into()))
    );

    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::Ready));
    assert_eq!(w.active().and_then(|t| t.workspace_problem.clone()), None);
}

/// An agent reporting in while its workspace has failed must not paint over
/// the failure. The badge would read "error" either way, but the reason would
/// be the agent's and the workspace's would be lost from the tooltip.
#[test]
fn an_agent_state_does_not_hide_a_failed_workspace() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "alpha"));
    let ws = WorkspaceId::from("ws_1");
    let agent = AgentId("ag_1".into());
    assert!(w.set_agent(&ws, agent.clone()));
    w.apply_workspace_info(&info(
        "ws_1",
        "alpha",
        WorkspaceState::Error("worktree is gone".into()),
    ));

    assert!(w
        .set_agent_status(&agent, TabStatus::Done, "the agent ended")
        .is_some());
    let showing = w.active().expect("a tab");
    assert_eq!(showing.status, TabStatus::Error);
    assert_eq!(showing.detail, "worktree is gone");
    assert_eq!(showing.agent_status, Some(TabStatus::Done));
}

/// A Retry that the daemon refused leaves the workspace in `Error` with the
/// refusal as its reason. The tab says so at once rather than waiting for an
/// event that may never come -- a timed-out request has no event behind it.
#[test]
fn a_failed_restart_is_the_workspace_s_new_problem() {
    let mut w = Workspaces::new_default();
    w.add_tab(0, tab("ws_1", "alpha"));
    let ws = WorkspaceId::from("ws_1");
    w.apply_workspace_info(&info("ws_1", "alpha", WorkspaceState::SandboxDown));

    assert!(w.note_restart_failed(&ws, "bwrap: permission denied"));
    let showing = w.active().expect("a tab");
    assert_eq!(showing.status, TabStatus::Error);
    assert_eq!(showing.detail, "bwrap: permission denied");
    assert_eq!(
        showing.workspace_problem,
        workspace_problem(&WorkspaceState::Error("bwrap: permission denied".into()))
    );
    assert!(!w.note_restart_failed(&"ws_missing".into(), "x"));
}

/// Whether a workspace that has just come back needs its agent started: a
/// Claude tab with no agent, or with one that has ended. A terminal tab never
/// does, and an agent the daemon says is alive is left alone.
#[test]
fn a_recovered_claude_tab_needs_an_agent_unless_one_is_alive() {
    let mut claude = tab("ws_1", "alpha");
    claude.adapter = AgentAdapterKind::Claude;
    assert!(claude.agent_needs_start(), "no agent at all");

    claude.agent_id = Some(AgentId("ag_1".into()));
    assert!(claude.agent_needs_start(), "an agent nobody has heard from");
    for (status, needs) in [
        (TabStatus::Done, true),
        (TabStatus::Error, true),
        (TabStatus::Idle, false),
        (TabStatus::Working, false),
        (TabStatus::WaitingPermission, false),
    ] {
        claude.agent_status = Some(status);
        assert_eq!(claude.agent_needs_start(), needs, "{status:?}");
    }

    let terminal = tab("ws_2", "beta");
    assert!(!terminal.agent_needs_start());
}

/// In-place workspaces, 2026-09-17: the tab carries the kind the daemon
/// reports, keeps it through events and a restore, and words a stopped
/// sandbox for a checkout rather than a worktree.
mod in_place_model {
    use bondsymphonic_ide::model::app_state::{workspace_problem_for, AgentTab, Workspaces};
    use bondsymphonic_proto::*;

    fn info(id: &str, kind: WorkspaceKind, state: WorkspaceState) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.into(),
            name: id.into(),
            repo_path: "/r".into(),
            base_branch: "main".into(),
            branch: "main".into(),
            worktree_path: "/r".into(),
            created_at: "t".into(),
            allowlist: vec![],
            state,
            agents: vec![],
            agent_records: vec![],
            runs: vec![],
            kind,
        }
    }

    #[test]
    fn a_tab_takes_its_kind_from_the_daemon_and_keeps_it() {
        let ip = info("ws_ip", WorkspaceKind::InPlace, WorkspaceState::Ready);
        let tab = AgentTab::from_workspace_info(&ip);
        assert!(tab.is_in_place());
        let json = serde_json::to_value(&tab).unwrap();
        assert_eq!(json["kind"], "in_place");

        // Restored from the saved arrangement and the daemon's list.
        let mut model = Workspaces::from_persisted(&[], std::slice::from_ref(&ip), None);
        assert!(model.is_in_place(&ip.id));
        // An event refreshes it, as it does the branch.
        model.apply_workspace_info(&info(
            "ws_ip",
            WorkspaceKind::InPlace,
            WorkspaceState::SandboxDown,
        ));
        assert!(model.is_in_place(&ip.id));
        assert!(!model.is_in_place(&WorkspaceId("ws_other".into())));
    }

    #[test]
    fn a_tab_saved_before_the_field_existed_is_a_worktree_tab() {
        let tab = AgentTab::from_workspace_info(&info(
            "ws_wt",
            WorkspaceKind::Worktree,
            WorkspaceState::Ready,
        ));
        let mut json = serde_json::to_value(&tab).unwrap();
        json.as_object_mut().unwrap().remove("kind");
        let back: AgentTab = serde_json::from_value(json).unwrap();
        assert!(!back.is_in_place());
    }

    #[test]
    fn a_stopped_sandbox_in_place_says_the_checkout_is_untouched() {
        let p =
            workspace_problem_for(&WorkspaceState::SandboxDown, WorkspaceKind::InPlace).unwrap();
        assert!(
            p.detail.contains("your checkout is not touched"),
            "{}",
            p.detail
        );
        let p =
            workspace_problem_for(&WorkspaceState::SandboxDown, WorkspaceKind::Worktree).unwrap();
        assert!(p.detail.contains("the worktree is kept"), "{}", p.detail);
    }
}

/// The breach detail, 2026-09-17 (final wave, I1): the daemon says why it
/// stopped a workspace in two events -- a warning carrying the sentence and a
/// diff, and then the workspace's own `Error` state carrying the sentence
/// alone -- and the model has to put them back together whichever way round
/// they arrive, and for a workspace whose tab does not exist yet.
mod workspace_warnings {
    use bondsymphonic_ide::model::app_state::Workspaces;
    use bondsymphonic_proto::*;

    const SENTENCE: &str = "Git files this workspace protects were replaced while the agent was \
                            running (.git/config), so its sandbox was stopped.";
    const DIFF: &str = "--- .git/config\n+++ .git/config\n+\tfsmonitor = ./planted.sh\n";

    fn info(id: &str, state: WorkspaceState) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.into(),
            name: id.into(),
            repo_path: "/r".into(),
            base_branch: "main".into(),
            branch: "main".into(),
            worktree_path: "/r".into(),
            created_at: "t".into(),
            allowlist: vec![],
            state,
            agents: vec![],
            agent_records: vec![],
            runs: vec![],
            kind: WorkspaceKind::InPlace,
        }
    }

    fn what_changed(model: &Workspaces, id: &WorkspaceId) -> String {
        let (g, t) = model.find(id).expect("the workspace has a tab");
        model.groups[g].tabs[t]
            .workspace_problem
            .as_ref()
            .map(|p| p.what_changed.clone())
            .unwrap_or_default()
    }

    /// The daemon's own order: the warning, then the state it explains.
    #[test]
    fn the_diff_the_daemon_sent_is_shown_under_the_reason_it_explains() {
        let id = WorkspaceId("ws_ip".into());
        let mut model =
            Workspaces::from_persisted(&[], &[info("ws_ip", WorkspaceState::Ready)], None);
        assert!(model.note_workspace_warning(&id, &format!("{SENTENCE}\n{DIFF}")));
        // Nothing yet: a running workspace has no problem to explain.
        assert_eq!(what_changed(&model, &id), "");

        model.apply_workspace_info(&info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned())));
        assert_eq!(what_changed(&model, &id), DIFF);

        // A different failure afterwards is not explained by the diff of the
        // one before it.
        model.apply_workspace_info(&info(
            "ws_ip",
            WorkspaceState::Error("bwrap: permission denied".to_owned()),
        ));
        assert_eq!(what_changed(&model, &id), "");
    }

    /// And the other order, which nothing in the protocol forbids.
    #[test]
    fn a_warning_that_arrives_after_the_state_still_explains_it() {
        let id = WorkspaceId("ws_ip".into());
        let mut model =
            Workspaces::from_persisted(&[], &[info("ws_ip", WorkspaceState::Ready)], None);
        model.apply_workspace_info(&info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned())));
        assert_eq!(what_changed(&model, &id), "");
        assert!(model.note_workspace_warning(&id, &format!("{SENTENCE}\n{DIFF}")));
        assert_eq!(what_changed(&model, &id), DIFF);
    }

    /// A warning for a workspace with no tab is kept, not dropped: the pane it
    /// belongs to may be built minutes later, and a reconnect rebuilds every
    /// tab from the daemon's list.
    #[test]
    fn a_warning_survives_the_tab_it_belongs_to_being_built_or_rebuilt() {
        let id = WorkspaceId("ws_ip".into());
        let mut model = Workspaces::new_default();
        assert!(model.note_workspace_warning(&id, &format!("{SENTENCE}\n{DIFF}")));
        assert!(model.find(&id).is_none());

        model.reconcile(&[info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned()))]);
        assert_eq!(what_changed(&model, &id), DIFF);
        // A second list, as a reconnect sends: the tabs are refreshed and the
        // detail is still there.
        model.reconcile(&[info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned()))]);
        assert_eq!(what_changed(&model, &id), DIFF);

        // The workspace going takes its warning with it: a workspace that came
        // back under the same id would be a different one.
        assert!(model.remove_workspace(&id));
        model.reconcile(&[info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned()))]);
        assert_eq!(what_changed(&model, &id), "");
    }

    /// A warning with nothing under its first line explains nothing and is
    /// kept nowhere.
    #[test]
    fn a_one_line_warning_is_not_kept() {
        let id = WorkspaceId("ws_ip".into());
        let mut model =
            Workspaces::from_persisted(&[], &[info("ws_ip", WorkspaceState::Ready)], None);
        assert!(!model.note_workspace_warning(&id, "this workspace has no network: proxy is down"));
        assert!(!model.note_workspace_warning(&id, &format!("{SENTENCE}\n   \n")));
        model.apply_workspace_info(&info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned())));
        assert_eq!(what_changed(&model, &id), "");
    }

    /// `state_json` is the only way any of this reaches the banner.
    #[test]
    fn the_detail_travels_in_state_json() {
        let id = WorkspaceId("ws_ip".into());
        let mut model =
            Workspaces::from_persisted(&[], &[info("ws_ip", WorkspaceState::Ready)], None);
        model.note_workspace_warning(&id, &format!("{SENTENCE}\n{DIFF}"));
        model.apply_workspace_info(&info("ws_ip", WorkspaceState::Error(SENTENCE.to_owned())));
        let json: serde_json::Value = serde_json::from_str(&model.to_json()).expect("state json");
        assert_eq!(
            json["groups"][0]["tabs"][0]["workspace_problem"]["what_changed"],
            DIFF
        );
        let back = Workspaces::from_json(&model.to_json()).expect("state json parses");
        assert_eq!(back, model);
    }
}
