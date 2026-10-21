//! The transcript view's inline blocked-host line, offscreen. Its own binary
//! rather than another row in `qobject_smoke::cpp_widgets`, so it builds its
//! own offscreen `QApplication` and never touches the state store.

use bondsymphonic_ide::testing::skip_without_qt;

extern "C" {
    fn bs_widget_test_begin();
    fn bs_widget_test_transcript_shows_a_blocked_host_inline() -> i32;
}

#[test]
fn the_transcript_shows_a_blocked_host_inline() {
    if skip_without_qt("network_denial_widgets") {
        return;
    }
    unsafe { bs_widget_test_begin() };
    let code = unsafe { bs_widget_test_transcript_shows_a_blocked_host_inline() };
    assert_eq!(
        code, 0,
        "TranscriptView blocked-host line failed at step {code}"
    );
}
