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
        state,
        agents: vec![],
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
        adapter: AgentAdapterKind::Terminal,
        command: None,
        run_config: None,
        options_json: String::new(),
        agent_id: None,
        agent_status: None,
        agent_detail: String::new(),
        op_error: None,
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

use bondsymphonic_ide::model::terminal_grid::{key_to_bytes, qt, TerminalGrid};

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
    w.agents = vec![AgentSummary {
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
    w.agents = vec![
        agent("ag_old", AgentAdapterKind::Claude),
        agent("ag_new", AgentAdapterKind::Claude),
    ];
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.agent_id, Some(AgentId("ag_new".into())));
}

/// A workspace the daemon has no agent for is still a plain terminal tab, which
/// is what every workspace created without one is.
#[test]
fn a_workspace_with_no_agents_is_still_a_terminal_tab() {
    let w = info("ws_1", "alpha", WorkspaceState::Ready);
    let tab = AgentTab::from_workspace_info(&w);
    assert_eq!(tab.adapter, AgentAdapterKind::Terminal);
    assert_eq!(tab.agent_id, None);
    assert_eq!(tab.options_json, "");
}

/// An agent with nothing but an id and an adapter leaves the options empty
/// rather than writing an object of nulls: `TranscriptModel::restartOptions`
/// treats an empty string as "the daemon's defaults", which is what it is.
#[test]
fn an_agent_started_with_no_options_leaves_the_options_empty() {
    let mut w = info("ws_1", "alpha", WorkspaceState::Ready);
    w.agents = vec![agent("ag_1", AgentAdapterKind::Claude)];
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
    w.agents = vec![agent("ag_1", AgentAdapterKind::Claude)];
    let persisted = vec![PersistedGroup {
        name: "Backend".to_owned(),
        workspace_ids: vec!["ws_1".to_owned()],
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
