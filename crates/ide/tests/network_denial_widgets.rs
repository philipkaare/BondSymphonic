//! The network bar the transcript view pins above the prompt box, and the
//! network rules dialog, offscreen. Their own binary rather than more rows in
//! `qobject_smoke::cpp_widgets`, so it builds its own offscreen
//! `QApplication` and never touches the state store.

use bondsymphonic_ide::testing::skip_without_qt;

extern "C" {
    fn bs_widget_test_begin();
    fn bs_widget_test_network_rules_dialog() -> i32;
    fn bs_widget_test_network_bar_lists_waiting_requests() -> i32;
    fn bs_widget_test_transcript_shows_waiting_requests_in_the_network_bar() -> i32;
}

#[test]
fn waiting_network_requests_are_pinned_on_the_network_bar() {
    if skip_without_qt("network_denial_widgets") {
        return;
    }
    unsafe { bs_widget_test_begin() };
    let code = unsafe { bs_widget_test_network_bar_lists_waiting_requests() };
    assert_eq!(code, 0, "NetworkBar failed at step {code}");
    let code = unsafe { bs_widget_test_transcript_shows_waiting_requests_in_the_network_bar() };
    assert_eq!(
        code, 0,
        "TranscriptView's network bar failed at step {code}"
    );
    let code = unsafe { bs_widget_test_network_rules_dialog() };
    assert_eq!(code, 0, "Network rules dialog failed at step {code}");
}
