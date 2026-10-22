//! The reviewed file selection and ignore-rule preview used by Changes actions.
use bondsymphonic_ide::testing::skip_without_qt;

extern "C" {
    fn bs_widget_test_begin();
    fn bs_widget_test_commit_dialog_selects_pending_paths() -> i32;
    fn bs_widget_test_ignore_dialog_previews_selected_rule() -> i32;
    fn bs_widget_test_changes_toolbar_commit_selection() -> i32;
    fn bs_widget_test_changes_toolbar_write_switch_safety() -> i32;
    fn bs_widget_test_changes_toolbar_pr_preflight() -> i32;
    fn bs_widget_test_explorer_editing_selection() -> i32;
}

#[test]
fn changes_editing_dialogs() {
    if skip_without_qt("changes_editing_widgets") {
        return;
    }
    unsafe { bs_widget_test_begin() };
    let commit = unsafe { bs_widget_test_commit_dialog_selects_pending_paths() };
    assert_eq!(commit, 0, "Commit dialog failed at step {commit}");
    let ignore = unsafe { bs_widget_test_ignore_dialog_previews_selected_rule() };
    assert_eq!(ignore, 0, "Ignore dialog failed at step {ignore}");
    let selection = unsafe { bs_widget_test_changes_toolbar_commit_selection() };
    assert_eq!(selection, 0, "Commit selection failed at step {selection}");
    let switching = unsafe { bs_widget_test_changes_toolbar_write_switch_safety() };
    assert_eq!(
        switching, 0,
        "Workspace switching failed at step {switching}"
    );
    let preflight = unsafe { bs_widget_test_changes_toolbar_pr_preflight() };
    assert_eq!(preflight, 0, "PR preflight failed at step {preflight}");
    let explorer = unsafe { bs_widget_test_explorer_editing_selection() };
    assert_eq!(explorer, 0, "Explorer selection failed at step {explorer}");
}
