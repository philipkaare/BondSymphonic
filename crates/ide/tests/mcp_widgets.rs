//! The MCP servers settings tab, its server editor and import dialog, and the
//! per-workspace "MCP servers…" dialog, offscreen. Its own binary, so it builds
//! its own offscreen `QApplication` and never touches the state store.

use bondsymphonic_ide::testing::skip_without_qt;

extern "C" {
    fn bs_widget_test_begin();
    fn bs_widget_test_settings_has_mcp_tab() -> i32;
    fn bs_widget_test_mcp_page_lists_servers() -> i32;
    fn bs_widget_test_mcp_secret_editor_is_write_only() -> i32;
    fn bs_widget_test_mcp_import_preview_before_apply() -> i32;
    fn bs_widget_test_workspace_dialog_shows_inherit() -> i32;
}

/// One test, because the widget hooks share one leaked `QApplication`, and the
/// steps run in a fixed order on the thread that made it.
#[test]
fn mcp_widgets() {
    if skip_without_qt("mcp_widgets") {
        return;
    }
    unsafe { bs_widget_test_begin() };
    let steps: [(&str, unsafe extern "C" fn() -> i32); 5] = [
        ("settings_has_mcp_tab", bs_widget_test_settings_has_mcp_tab),
        (
            "mcp_page_lists_servers",
            bs_widget_test_mcp_page_lists_servers,
        ),
        (
            "mcp_secret_editor_is_write_only",
            bs_widget_test_mcp_secret_editor_is_write_only,
        ),
        (
            "mcp_import_preview_before_apply",
            bs_widget_test_mcp_import_preview_before_apply,
        ),
        (
            "workspace_dialog_shows_inherit",
            bs_widget_test_workspace_dialog_shows_inherit,
        ),
    ];
    for (name, step) in steps {
        let code = unsafe { step() };
        assert_eq!(code, 0, "{name} failed at step {code}");
    }
}
