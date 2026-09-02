//! **A slash form asks through the SESSION's seam, never its own terminal.**
//!
//! The regression these pin is visible rather than subtle. `/settings` used to
//! acquire its own prompt window with `Terminal::suspend_for_prompt()` while
//! the cockpit still had a chat editor mounted below it, which produced three
//! defects at once:
//!
//! 1. **Two active prompts.** The form's chevron and the mounted chat chevron
//!    were both painted in the live accent, so nothing on screen said which
//!    one the keyboard belonged to.
//! 2. **A modal muddled into the surface.** No rows were reserved for the
//!    question, and the mounted header kept repainting its clock and token
//!    counter underneath (and through) it every 250 ms.
//! 3. **A corrupted surface afterwards.** The form's bytes landed outside
//!    ratatui's diff, so the header row never came back — the clock was simply
//!    gone until the next full redraw.
//!
//! All three are one cause: a second console path beside the seam that already
//! exists. `SurfaceRequest::Interact` dims the mounted chevron
//! (`chat_inactive`), reserves the modal's rows, blocks the repaint loop for
//! the duration, and clears + repaints on teardown. Routing the form there
//! fixes all three, and these tests keep it routed as the code is refactored.

use std::cell::RefCell;

use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::HumanQuestionOutcome;

/// A fully mocked operator: records every interaction it is shown and answers
/// with `Cancelled`, so the form unwinds without writing a setting anywhere.
/// No terminal, no filesystem, no clock — the unit tier's contract.
struct AskSpy {
    seen: RefCell<Vec<SurfaceInteraction>>,
}

impl AskSpy {
    fn new() -> Self {
        Self {
            seen: RefCell::new(Vec::new()),
        }
    }

    fn ask(&self, interaction: &SurfaceInteraction) -> HumanQuestionOutcome {
        self.seen.borrow_mut().push(interaction.clone());
        HumanQuestionOutcome::Cancelled
    }
}

/// **The injected seam is the one that gets asked.** If a refactor reinstates a
/// private console path inside the form, the spy sees nothing and this fails.
#[test]
fn a_settings_form_question_goes_to_the_injected_seam() {
    let spy = AskSpy::new();
    let ask = |interaction: &SurfaceInteraction| spy.ask(interaction);

    let alive = super::dispatch_slash_with_ask("/settings", "/ws", false, false, false, Some(&ask))
        .expect("dispatch succeeds");

    assert!(alive, "a cancelled form keeps the session alive");
    let seen = spy.seen.borrow();
    assert_eq!(seen.len(), 1, "the field menu was asked exactly once");
    assert!(
        seen[0].is_blocking(),
        "a form question blocks the session until answered"
    );
}

/// The deep-link form (`/settings <field>`) asks for the VALUE through the same
/// seam — the second entry point into the form, and the one a refactor is most
/// likely to leave behind.
#[test]
fn a_settings_deep_link_asks_through_the_same_seam() {
    let spy = AskSpy::new();
    let ask = |interaction: &SurfaceInteraction| spy.ask(interaction);

    super::dispatch_slash_with_ask("/settings tenacity", "/ws", false, false, false, Some(&ask))
        .expect("dispatch succeeds");

    assert_eq!(
        spy.seen.borrow().len(),
        1,
        "the value question went through the seam"
    );
}

/// **No seam, no session.** The terminal-owning form (plain CLI, `newt` without
/// a cockpit) still has an ask — `ask_on_this_terminal` — so the fallback is a
/// named path rather than an absent one. Asserted at the type level: a missing
/// fallback would not compile.
#[test]
fn the_fallback_ask_exists_for_terminal_owning_callers() {
    let fallback: super::SlashAsk<'_> = &super::ask_on_this_terminal;
    let _ = fallback;
}

/// **The session's slash dispatch never opens its own prompt window.**
///
/// A behavioral test cannot see a call site that a future refactor adds, so
/// this reads the source of the two files that own the routing. `chat.rs` must
/// pass its surface seam, and the `/settings` arm must not acquire a terminal
/// of its own. Same shape as `session_worker`'s wiring guard, for the same
/// reason: the defect is a MISSING argument, which compiles fine.
#[test]
fn the_session_slash_call_site_passes_its_surface_seam() {
    let chat = include_str!("../chat.rs").replace([' ', '\n'], "");
    assert!(
        chat.contains("dispatch_slash_with_ask(&task,workspace,color,verbose,")
            && chat.contains("Some(&ask_surface),"),
        "chat.rs must dispatch slash commands with the surface seam wired in"
    );

    let lib = include_str!("../lib.rs");
    let arm = lib
        .split("\"settings\" => {")
        .nth(1)
        .expect("the /settings dispatch arm exists");
    let arm = &arm[..arm.find("\"crew\" =>").expect("the arm ends at /crew")];
    assert!(
        !arm.contains("suspend_for_prompt"),
        "the /settings arm must ask through the seam, not its own terminal: {arm}"
    );
}

/// **`/crew edit` shares the fix, not just the diagnosis.** It ran the same
/// private console path `/settings` did, so the same three defects were one
/// keystroke away. The form now takes the seam; the terminal-owning `newt crew
/// edit` CLI supplies the fallback.
#[test]
fn the_crew_form_asks_through_a_seam_rather_than_its_own_terminal() {
    let form = include_str!("../crew_form/mod.rs");
    assert!(
        !form.contains("suspend_for_prompt"),
        "crew_form must not acquire a prompt window of its own"
    );
    assert!(
        form.contains("pub fn run_edit_with_ask("),
        "the seam-taking entry point is what an in-session /crew edit calls"
    );

    let lib = include_str!("../lib.rs").replace([' ', '\n'], "");
    assert!(
        lib.contains("commands::crew::dispatch(arg1,arg2,color,verbose,ask.unwrap_or(&fallback))"),
        "the /crew arm must forward the session's seam"
    );
}
