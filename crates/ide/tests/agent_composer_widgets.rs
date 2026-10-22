//! Active-agent context and composing a follow-up while the agent works.
use bondsymphonic_ide::testing::skip_without_qt;

extern "C" {
    fn bs_widget_test_begin();
    fn bs_widget_test_prompt_command_completion() -> i32;
    fn bs_widget_test_resume_command_selects_session() -> i32;
    fn bs_widget_test_working_agent_keeps_composer_editable() -> i32;
    fn bs_widget_test_agent_title_follows_selected_tab() -> i32;
}

#[test]
fn agent_header_and_working_composer() {
    if skip_without_qt("agent_composer_widgets") {
        return;
    }
    unsafe { bs_widget_test_begin() };
    let completion = unsafe { bs_widget_test_prompt_command_completion() };
    assert_eq!(
        completion, 0,
        "Command completion failed at step {completion}"
    );
    let resume = unsafe { bs_widget_test_resume_command_selects_session() };
    assert_eq!(resume, 0, "Resume command failed at step {resume}");
    let composer = unsafe { bs_widget_test_working_agent_keeps_composer_editable() };
    assert_eq!(composer, 0, "Working composer failed at step {composer}");
    let title = unsafe { bs_widget_test_agent_title_follows_selected_tab() };
    assert_eq!(title, 0, "Agent title failed at step {title}");
}
