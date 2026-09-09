use bondsymphonic_ide::model::app_state::*;
use bondsymphonic_ide::model::file_tree::*;
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
        status: TabStatus::Creating,
        detail: String::new(),
        adapter: AgentAdapterKind::Terminal,
        command: None,
        agent_id: None,
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
