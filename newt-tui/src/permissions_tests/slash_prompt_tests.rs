use super::{apply_chat_prompt_policy, is_slash_command_at_prompt, SLASH_COMMAND_PROMPT_NOTICE};
use newt_core::HumanQuestionOutcome;

#[test]
fn refuses_slash_commands_as_tool_answers() {
    // Regression: `/exit` typed at a `request_user_input` prompt was sent to
    // the model, which ran it as a shell command -> OCAP denial.
    assert!(is_slash_command_at_prompt("/exit"));
    assert!(is_slash_command_at_prompt("  /quit"));
    assert!(is_slash_command_at_prompt("/model qwen2.5-coder:7b"));
    // A plain answer (even one containing a slash) is a real answer.
    assert!(!is_slash_command_at_prompt("qwen2.5-coder:7b"));
    assert!(!is_slash_command_at_prompt("use a/b testing"));
    assert!(!is_slash_command_at_prompt(""));
    // A whitespace-padded non-slash answer is NOT a command, so
    // `prompt_user_input` returns it verbatim (whitespace preserved) rather
    // than backing out. Detection reads a trim_start view; it never trims the
    // answer itself.
    assert!(!is_slash_command_at_prompt("  indented answer  "));
    assert!(!is_slash_command_at_prompt("   "));
}

#[test]
fn slash_policy_returns_guidance_for_the_surface_that_owns_repaint() {
    let (outcome, notice) = apply_chat_prompt_policy(HumanQuestionOutcome::Answer("/help".into()));
    assert_eq!(outcome, HumanQuestionOutcome::Cancelled);
    assert_eq!(notice, Some(SLASH_COMMAND_PROMPT_NOTICE));

    let answer = HumanQuestionOutcome::Answer("  ordinary answer  ".into());
    let (outcome, notice) = apply_chat_prompt_policy(answer.clone());
    assert_eq!(outcome, answer, "ordinary answers remain byte-exact");
    assert_eq!(notice, None);
}
