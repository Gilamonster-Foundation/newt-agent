//! §6.6(a) — **the primary guarantee, checked by the compiler.**
//!
//! The whole design rests on one claim: a prompt that has not suspended the
//! terminal's ephemeral writers *cannot be written*. That holds only if
//! `PromptWindow` is genuinely unforgeable — if a downstream crate can conjure
//! one, every blocking prompt's `&PromptWindow` parameter degrades from a proof
//! obligation into a decorative argument, and the bug walks straight back in at
//! the next `gate.ask` call site.
//!
//! So the seal is not documented, it is *tested*: these cases must FAIL TO
//! COMPILE. A refactor that accidentally makes `PromptWindow` constructible
//! (adding a `Default`, making the field `pub`, exposing `test_stub` outside
//! its feature) turns this test red rather than shipping a silently
//! re-introducible hang.

#[test]
fn a_prompt_window_cannot_be_constructed_outside_newt_core_tty() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/prompt_window_*.rs");
}
