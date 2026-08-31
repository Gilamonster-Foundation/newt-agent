//! Interactive OCAP decisions. One typed [`newt_core::Question`] supplies both
//! terminal and web rendering, parsing, and the set of actions that may pass.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::danger;
use crate::mint_operating_key;
use newt_core::agentic::{newt_line, print_newt};
use newt_core::interaction_adapter::{action_for_option, role_of, DECISION_CONTROL};
use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::markup::plain;
use newt_core::tty::{
    modal_prompt_controls, read_prompt_window_line, ControlReader, Echo, PromptLine as ModalLine,
    PromptWindow, Terminal, MODAL_CONTROL_HINT, MODAL_INPUT_GLYPH,
};
use newt_core::HumanQuestionOutcome;
pub(crate) use newt_core::PermissionAction as PromptChoice;
// D0 (#1878): the legacy form is not named here at all any more. C0a moved
// rendering off it; C3c removed the web card's reconstruction; D0 moved the
// decode onto `newt_interaction::binding::resolve_typed` and deleted the
// reverse adapter. The `#[cfg(test)]` that stood here belonged to the
// `newt_core::Question` import beside it — both are gone, and these protocol
// types are production imports.
use newt_interaction::{
    Audience, ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition,
    InteractionKind, OptionId, Requirement,
};

/// The reserved control id of the free-text form's single answer field.
///
/// Sibling of `interaction_adapter::DECISION_CONTROL`, kept here because
/// the free-text form is `newt-tui`'s, not the adapter's. D0 owns promoting
/// it when `request_user_input` moves onto the controller.
const ANSWER_CONTROL: &str = "answer";

// ---------------------------------------------------------------------------

/// Whether prompted permissions were explicitly enabled for this session.
pub(crate) fn permission_prompting_configured(
    env_flag: bool,
    tui: Option<&newt_core::TuiConfig>,
) -> bool {
    env_flag || tui.is_some_and(|t| t.permissions.prompt)
}

const INTERACTIVE_PROMPT_DEFAULT: bool = true;

/// Headless/non-TTY and explicit-off are fail-closed; interactive defaults on.
pub(crate) fn should_prompt_permissions(
    configured_on: bool,
    explicit_off: bool,
    interactive: bool,
    headless: bool,
) -> bool {
    if headless || !interactive {
        return false;
    }
    if explicit_off {
        return false;
    }
    configured_on || INTERACTIVE_PROMPT_DEFAULT
}
#[cfg(unix)]
pub(crate) use newt_core::tty::try_watch_stdin;

pub(crate) fn reason_is_model_authored(req: &newt_core::PermissionRequest) -> bool {
    req.tool == "request_permissions"
}

/// How long the gate waits for a web decision before failing closed.
///
/// **Load bearing, and until B0b-1 untested** (#1842): this must be
/// SHORTER than `ConversationStore::PERMISSION_REQUEST_TTL_NANOS`, or the
/// gate could still be waiting on an offer the store has already aged out
/// — and a decision could land against a row nothing will honour. The two
/// numbers used to live in different crates with nothing relating them
/// (a literal here, a constant there);
/// `b0b::the_gate_timeout_is_shorter_than_the_store_ttl` now asserts the
/// relationship instead of leaving it to coincidence.
pub(crate) const WEB_DECISION_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// One action this policy offers, already decided.
///
/// The POLICY half hands these to the MARSHALLING half. The split is what
/// lets tier and audience be consulted TOGETHER — they decide the action
/// list *and* the note text in the same branches, exactly as before — while
/// the construction of protocol records stays mechanical.
struct OfferedAction {
    action: PromptChoice,
    key: &'static str,
    label: &'static str,
}

/// **The POLICY half** (B0a, #1841): which actions this `(tier, audience)`
/// pair offers, and the note that accompanies them.
///
/// This is a RESTRUCTURING of the old `permission_question_for`, not a
/// relocation: the high-tier arms produce genuinely different per-surface
/// text, and the durable grants are terminal-only, so tier and audience
/// must still be known simultaneously.
fn permission_policy(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
    audience: Audience,
) -> (Vec<OfferedAction>, Option<String>) {
    use newt_core::DenialKind;
    let tier = danger.classify(req.kind, &req.target);
    // ONE audience test, bound once. The A0 shape asked it twice; the two
    // asks were always the same question, so the ratchet baseline goes
    // DOWN by one branch rather than up.
    let terminal = matches!(audience, Audience::Terminal);

    let mut actions = vec![OfferedAction {
        action: PromptChoice::AllowOnce,
        key: "a",
        label: "allow once",
    }];
    if tier == danger::DangerTier::Low {
        actions.push(OfferedAction {
            action: PromptChoice::AllowSession,
            key: "s",
            label: "session allow",
        });
        if req.kind == DenialKind::Net && terminal {
            actions.push(OfferedAction {
                action: PromptChoice::AllowPermanent,
                key: "A",
                label: "Allow permanently (adds host to config)",
            });
        }
    }
    actions.push(OfferedAction {
        action: PromptChoice::Deny,
        key: "d",
        label: "deny (default)",
    });
    if terminal {
        actions.push(OfferedAction {
            action: PromptChoice::DenyAlways,
            key: "D",
            label: "Deny always",
        });
        actions.push(OfferedAction {
            action: PromptChoice::DenyPermanent,
            key: "P",
            label: "Permanently deny",
        });
    }

    // `Audience` is `#[non_exhaustive]`, so this crate cannot match it
    // exhaustively and the wildcard is forced by the language rather than
    // chosen. The tripwire that a third variant needs thinking about lives
    // where the enum is DEFINED —
    // `newt_interaction::binding::model_fidelity` (#1837).
    let note = match (tier, audience) {
        (danger::DangerTier::High, Audience::Terminal) => Some(format!(
            "high-danger: session allow refused; key allow / step-up is the future path, P3\n{MODAL_CONTROL_HINT}"
        )),
        (danger::DangerTier::High, Audience::Web) => Some(format!(
            "High danger: session authorization is unavailable.\n{MODAL_CONTROL_HINT}"
        )),
        _ => Some(MODAL_CONTROL_HINT.into()),
    };
    (actions, note)
}

/// **The MARSHALLING half** (B0a, #1841): the policy's decisions as ONE
/// [`InteractionDefinition`].
///
/// This is the single form both surfaces build. The prompt text itself is
/// audience-independent; only the action list and the note are not.
pub(crate) fn permission_definition(
    req: &newt_core::PermissionRequest,
    danger: &danger::DangerTable,
    audience: Audience,
) -> InteractionDefinition {
    use newt_core::DenialKind;
    let (verb, axis) = match req.kind {
        DenialKind::Exec => ("run", "outside the granted exec allowlist"),
        DenialKind::FsRead => ("read", "outside the granted fs_read scope"),
        DenialKind::FsWrite => ("write", "outside the granted fs_write scope"),
        DenialKind::Net => ("reach", "outside the granted net allowlist"),
        DenialKind::RemoteTool => ("call", "not in the active persona's tool allow-list"),
        DenialKind::GitWrite => (
            "commit/stage via git",
            "outside the granted git-write authority",
        ),
    };

    let blast = match danger.blast_radius(req.kind, &req.target) {
        Some(line) => format!("{line}\n"),
        None => String::new(),
    };
    let reason = if req.reason.is_empty() {
        String::new()
    } else if reason_is_model_authored(req) {
        format!(
            "  model says (model-authored, unverified): \"{}\"\n",
            req.reason
        )
    } else {
        format!("  ({})\n", req.reason)
    };

    let (offered, note) = permission_policy(req, danger, audience);
    let options = offered
        .into_iter()
        .map(|offer| ChoiceOption {
            // The WIRE name is the option's identity; the key is a
            // presentation affordance. Both survive, which is what makes
            // the adapter round trip field-identical.
            id: OptionId::new(offer.action.as_str()).expect(WIRE_NAMES_ARE_OPTION_IDS),
            role: role_of(offer.action),
            label: offer.label.to_string(),
            key: offer.key.to_string(),
            aliases: Vec::new(),
        })
        .collect();

    let controls = vec![Control {
        id: ControlId::new(DECISION_CONTROL).expect(WIRE_NAMES_ARE_OPTION_IDS),
        kind: ControlKind::Choice { options },
        label: String::new(),
        // A permission prompt must be answered: an unanswered one
        // denies by default, which is a decision, not an absence.
        requirement: Requirement::Required,
    }];
    // DERIVED, not hardcoded (#1912). The offered actions vary by denial kind,
    // tier and audience, so a permission question may carry two options or
    // five. Hardcoding `Choice` labelled every TWO-action permission as a pick
    // from a displayed set when it is a binary decision — and the kind is
    // bound into the definition's identity, so the label was wrong in the
    // content-addressed record too.
    let kind = if newt_interaction::controls_are_decision_shaped(&controls) {
        InteractionKind::Confirm
    } else {
        InteractionKind::Choice
    };
    let mut definition = InteractionDefinition::new(
        kind,
        format!(
            "\u{2298} {} wants to {verb} `{}` \u{2014} {axis}.\n{blast}{reason}",
            req.tool, req.target
        )
        .trim_end(),
        controls,
    );
    definition.note = note;
    definition
}

/// Why the `expect`s above cannot fire.
///
/// Every `PermissionAction` wire name, and `DECISION_CONTROL`, is drawn
/// from `[A-Za-z0-9_-]`, and that set is frozen.
/// `b0a::every_offered_action_is_a_valid_option_id` walks every
/// `(DenialKind, tier, audience)` combination this policy can produce and
/// asserts the definition builds, so this branch is unreachable rather
/// than merely unlikely.
const WIRE_NAMES_ARE_OPTION_IDS: &str =
    "every PermissionAction wire name is a valid option id (frozen set; see \
     b0a::every_offered_action_is_a_valid_option_id)";

/// The legacy typed form for one request, **for tests only**.
///
/// B0b-2 (#1846) took this off the production path; C0a (#1856) took
/// RENDERING off it too. What remains is a convenience for the tests that
/// assert on the adapted form — the alias/ambiguity parse properties, and
/// the web card's model reconstruction. Production reaches
/// `permission_definition` directly and renders it with
/// [`plain::render`].
/// The actions a definition offers, in presentation order.
#[cfg(test)]
pub(crate) fn offered_actions(definition: &InteractionDefinition) -> Vec<PromptChoice> {
    definition
        .controls
        .iter()
        .find_map(|c| match &c.kind {
            ControlKind::Choice { options } => Some(options),
            _ => None,
        })
        .map(|options| {
            options
                .iter()
                .filter_map(|o| action_for_option(o.id.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a definition offers `action`, by wire id.
///
/// D0 (#1878): replaces the `question_for` test helper, which reconstructed a
/// legacy `Question` through the now-deleted reverse adapter purely so a test
/// could ask "is this action offered". The definition answers that directly.
#[cfg(test)]
pub(crate) fn offers(definition: &InteractionDefinition, action: PromptChoice) -> bool {
    definition.controls.iter().any(|c| match &c.kind {
        ControlKind::Choice { options } => options.iter().any(|o| o.id.as_str() == action.as_str()),
        _ => false,
    })
}

/// Facade P1b: the production [`danger::DangerTable`] — the built-in
/// interpreter set plus this environment's broad fs roots (`$HOME` and the
/// current workspace dir), which a plain `[s]ession allow` must never grant
/// wholesale (§7-F3/F4). The env read happens once, here, at gate construction;
/// unit tests build a table by hand (`DangerTable::builtin().with_fs_root(...)`)
/// and never touch the real env (the no-real-fs-in-unit-tests rule).
pub(crate) fn production_danger_table() -> danger::DangerTable {
    let mut table = danger::DangerTable::builtin();
    if let Some(home) = std::env::var_os("HOME") {
        table = table.with_fs_root(std::path::PathBuf::from(home));
    }
    if let Ok(cwd) = std::env::current_dir() {
        table = table.with_fs_root(cwd);
    }
    table
}

/// The #1207 blessing judgment, exported as the ONE narrow seam `newt doctor
/// --sign-ocap` (newt-cli) needs from this subsystem: is an ocap-store target
/// high-danger by the production danger table? Passed to
/// [`newt_core::ocap_store::sign_approves`] as its `validate_approve`
/// predicate, so the blessing ceremony can never launder an interpreter or a
/// broad fs root into `approve.toml`. Fs classifies as a WRITE — the
/// conservative reading of a durable fs grant, and the table's High tier is
/// about broad roots on either axis.
pub fn ocap_high_danger_predicate() -> impl Fn(newt_core::ocap_store::CapabilityClass, &str) -> bool
{
    let table = production_danger_table();
    move |class, target| {
        let kind = match class {
            newt_core::ocap_store::CapabilityClass::Exec => newt_core::DenialKind::Exec,
            newt_core::ocap_store::CapabilityClass::Fs => newt_core::DenialKind::FsWrite,
            newt_core::ocap_store::CapabilityClass::Net => newt_core::DenialKind::Net,
        };
        table.classify(kind, target) == danger::DangerTier::High
    }
}

/// Read a terminal form. Unknown input and I/O failure fail closed.
///
/// **C0a (#1856): renders through [`plain::render`], not through the
/// semantic type.** The definition itself is what reaches the operator, so
/// the bytes displayed and the authority the answer is checked against are
/// the same value — there is no longer an intermediate form that could
/// drift from it.
///
/// The input glyph gets its OWN final line. `tty::modal::render` repaints
/// only the text after the last `\n`, so this composition is load bearing:
/// it is what makes a keystroke redraw the answer row rather than the menu.
pub(crate) fn prompt_permission_choice(
    w: &PromptWindow,
    definition: &InteractionDefinition,
) -> PromptChoice {
    let prompt = format!("{}\n{MODAL_INPUT_GLYPH}", plain::render(definition));
    // A permission menu offers visible options; there is nothing to mask.
    match read_prompt_window_line(w, &prompt, Echo::Chars) {
        Ok(ModalLine::Line(answer)) => decode_answer(definition, &answer),
        Ok(ModalLine::Back) => PromptChoice::Back,
        Ok(ModalLine::Exit) => PromptChoice::Exit,
        Ok(ModalLine::Eof) | Err(_) => PromptChoice::Deny,
    }
}

/// Decode one operator keystroke to a candidate action.
///
/// **Deliberately still the adapter.** C0a extracts RENDERING; decoding
/// stays exactly where B0b-1 put it — `Question::parse`, reached through
/// `newt_interaction::binding::resolve_typed` — the ONE implementation of the
/// canonical-first / alias / ambiguity-denial rules, and the half
/// `NewtPolicy.PromptForm`'s Lean theorems and TLA+ `AuthorizationDisplayed`
/// govern (BHV-PROMPT-001).
///
/// D0 (#1878) moved that resolution out of `Question::parse` and into the
/// binding module rather than writing a second decoder against
/// `InteractionDefinition` — which would have been the third answer-parser
/// this slice's deletion gate exists to prevent. The rules did not change;
/// only where they live did, and now one place holds them.
///
/// **Fails closed with a CONSTANT, not with a role.** An unresolvable answer
/// denies, and the deny is `PromptChoice::Deny` written here — never an option
/// picked out of the definition by looking for `role == Deny`. A definition can
/// come from untrusted markup and `role` is author-assigned, so deriving the
/// failure mode from it would hand the author the failure mode (A3).
///
/// It denies rather than panicking because this runs with the operator's
/// answer already in hand; a panic here would take the session down
/// mid-decision.
/// The typed line resolved against the definition's own option set, or
/// `None` when it names no offered option.
///
/// Split out of [`decode_answer`] rather than duplicated: the key policy
/// ("a" is allow-once, "A" is allow-permanently, case-exact) lives in
/// `resolve_typed` and must have exactly one reader. The two callers differ
/// only in what they do with an unrecognized line.
fn resolve_answer(definition: &InteractionDefinition, answer: &str) -> Option<PromptChoice> {
    let options = definition.controls.iter().find_map(|c| match &c.kind {
        newt_core::ControlKind::Choice { options } => Some(options),
        _ => None,
    })?;
    newt_interaction::binding::resolve_typed(options, answer)
        .and_then(|option| action_for_option(option.as_str()))
}

fn decode_answer(definition: &InteractionDefinition, answer: &str) -> PromptChoice {
    // The TERMINAL-ONLY path, where the operator is blocked on answering and
    // a bare Enter is the documented "deny (default)".
    resolve_answer(definition, answer).unwrap_or(PromptChoice::Deny)
}

/// Everything `run_web_wait` touches outside the store and the window.
///
/// Bundled rather than passed loose because there are now four of them and
/// they are one idea: the world the wait loop runs against. Each exists so a
/// test can drive the loop with no terminal, no wall clock and no real sleep
/// — and, since C4b, no real stdout either, so "the losing operator was
/// told" is provable in the mocked tier rather than only in a PTY.
struct WebWaitIo<'a, 'w> {
    /// Re-arm the control reader; a fresh one after a transient failure.
    reacquire: &'a mut dyn FnMut() -> io::Result<Box<dyn ControlReader + 'w>>,
    /// The clock. Injected so deadline tests need no wall clock (#1953).
    now: &'a dyn Fn() -> Instant,
    /// Pace the loop.
    sleep: &'a mut dyn FnMut(Duration),
    /// Speak to the operator.
    notify: &'a mut dyn FnMut(&str),
}

/// What a losing terminal operator is told.
///
/// Pure, so the wording is testable without a terminal; the EMISSION is
/// proven separately by injecting the sink, because a message nobody sends
/// is the same defect as no message at all.
fn lost_to_web_message(mine: PromptChoice, winner: PromptChoice) -> String {
    format!(
        "the web answered first — `{}` was applied, not your `{}`",
        winner.as_str(),
        mine.as_str()
    )
}

fn decision_scope(choice: PromptChoice) -> &'static str {
    match choice {
        PromptChoice::AllowOnce => "once",
        PromptChoice::AllowSession => "session",
        PromptChoice::AllowPermanent => "permanent",
        PromptChoice::Deny => "once",
        PromptChoice::DenyAlways => "session",
        PromptChoice::DenyPermanent => "permanent",
        PromptChoice::Back | PromptChoice::Exit => "control",
    }
}

/// The free-text form for one question, as a definition.
///
/// Split out by C1 (#1862) so the SESSION can build the semantic form while
/// the TERMINAL renders it — the two now happen on different threads.
pub(crate) fn free_text_form(question: &str) -> InteractionDefinition {
    // C0a (#1856): the free-text form is an `InteractionDefinition` too, so
    // it renders through the ONE plain projection rather than through a
    // second type. Byte-identical to the actionless `Question` it replaces —
    // a `Text` control contributes no choices line, so the rendering is
    // body + note, exactly as before (`c0a::the_free_text_form_renders_
    // exactly_as_it_did`). The interaction MODEL is D0's to migrate; only
    // the rendering moved here.
    InteractionDefinition {
        note: Some(MODAL_CONTROL_HINT.into()),
        ..InteractionDefinition::new(
            InteractionKind::Prompt,
            format!("? {question}"),
            vec![Control {
                id: ControlId::new(ANSWER_CONTROL).expect(
                    "`answer` is a valid control id (non-empty, drawn from \
                     [A-Za-z0-9_-]); it is a const, so this cannot vary at \
                     runtime",
                ),
                kind: ControlKind::Text,
                label: String::new(),
                requirement: Requirement::Required,
            }],
        )
    }
}

/// **The chat surface's terminal adapter**: `newt_core`'s one adapter, plus
/// the slash-command back-out.
///
/// F0a (#1922) moved the adapter itself down to
/// [`newt_core::interaction_terminal`], because it needed nothing from this
/// crate and `newt-cli`'s command flows had no route to the typed path while
/// it was `pub(crate)` here. What stayed is the only part that was ever
/// TUI-specific: a leading slash at a prompt is a COMMAND, not an answer, and
/// the operator is sent back to the chat prompt to use it. `newt dock
/// approve` has no chat prompt to be sent back to, which is exactly why this
/// half did not travel.
///
/// Back-out rather than refusal: `Cancelled` is what the core adapter already
/// reports for Esc, and a slash typed at a prompt means the same thing —
/// "not this, take me out".
pub(crate) fn present_on_terminal(
    w: &PromptWindow,
    interaction: &SurfaceInteraction,
) -> HumanQuestionOutcome {
    let outcome = newt_core::interaction_terminal::present_on_terminal(w, interaction);
    // The answer is inspected VERBATIM — leading/trailing whitespace can be
    // meaningful (an indented code line, a spacing-sensitive value, an
    // intentionally blank-but-submitted answer). Detection reads a trim_start
    // VIEW without mutating the answer.
    if let HumanQuestionOutcome::Answer(line) = &outcome {
        if is_slash_command_at_prompt(line) {
            w.notice(
                "(slash commands aren't answers; press Esc, then use the command at the chat prompt)",
            )
            .ok();
            return HumanQuestionOutcome::Cancelled;
        }
    }
    outcome
}

/// A leading-slash answer at a `request_user_input` prompt is a TUI command
/// intent, not an answer to hand to the model. Pure, so it's unit-testable.
fn is_slash_command_at_prompt(answer: &str) -> bool {
    answer.trim_start().starts_with('/')
}

#[cfg(test)]
#[path = "permissions_tests/slash_prompt_tests.rs"]
mod slash_prompt_tests;

/// Session decisions remain separate from the never-widened operating key.
#[derive(Default)]
pub(crate) struct PermissionPromptState {
    /// Opt-in attach-surface decision channel; `None` uses the terminal.
    pub(crate) web_store: Option<newt_core::ConversationStore>,
    /// The workspace fence this session is confined to (B0b-1, #1842).
    ///
    /// Supplied by the CALLER and compared against the fence the offer
    /// carries, so `Refusal::WorkspaceMismatch` is a check that can fail
    /// rather than a tautology. Empty in tests that do not exercise the
    /// fence, which is why `authorize` compares two independently
    /// supplied values instead of one value with itself.
    pub(crate) workspace_key: String,
    session_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    session_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// Durable deny-only entries loaded at session start.
    persistent_denials: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    /// One-shot grants carried from `request_permissions` to its retry.
    pending_once_grants: std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    pub(crate) decisions: Vec<newt_core::PermissionRecord>,
    /// Durable approve/deny policy, read-only during the session.
    pub(crate) ocap_policy: newt_core::ocap_store::PolicySet,
}

/// Normalize only prompt memos; the authority check itself stays exact.
pub(crate) fn exec_grant_basename(cmd: &str) -> &str {
    cmd.rsplit(['/', '\\'])
        .find(|p| !p.is_empty())
        .unwrap_or(cmd)
}

/// Match exact grants, plus a basename-normalized exec request.
pub(crate) fn session_grant_covers(
    grants: &std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    req: &newt_core::PermissionRequest,
) -> bool {
    if grants.contains(&(req.kind, req.target.clone())) {
        return true;
    }
    req.kind == newt_core::DenialKind::Exec
        && grants.contains(&(
            newt_core::DenialKind::Exec,
            exec_grant_basename(&req.target).to_string(),
        ))
}

/// Consume an exact or basename-normalized one-shot grant.
pub(crate) fn take_pending_once(
    pending: &mut std::collections::BTreeSet<(newt_core::DenialKind, String)>,
    req: &newt_core::PermissionRequest,
) -> Option<(newt_core::DenialKind, String)> {
    let exact = (req.kind, req.target.clone());
    if pending.remove(&exact) {
        return Some(exact);
    }
    if req.kind == newt_core::DenialKind::Exec {
        let base = (
            newt_core::DenialKind::Exec,
            exec_grant_basename(&req.target).to_string(),
        );
        if pending.remove(&base) {
            return Some(base);
        }
    }
    None
}

impl PermissionPromptState {
    /// Load the persistent denylist from `path` into a fresh state (#904). A
    /// missing file yields an empty denylist. Called once at session start.
    pub(crate) fn with_persistent_denials(path: Option<&std::path::Path>) -> Self {
        let persistent_denials = path
            .map(|p| newt_core::load_denials(p).into_iter().collect())
            .unwrap_or_default();
        Self {
            persistent_denials,
            ..Self::default()
        }
    }
}

/// Prompts, records, and re-mints from the user root without widening the live key.
pub(crate) struct PromptPermissionGate<
    'a,
    F: FnMut(&PromptWindow, &InteractionDefinition) -> PromptChoice,
> {
    pub(crate) state: &'a mut PermissionPromptState,
    /// Enforced caveats at turn start.
    pub(crate) base: newt_core::Caveats,
    pub(crate) key_path: Option<std::path::PathBuf>,
    pub(crate) conversation_id: String,
    pub(crate) log_path: Option<std::path::PathBuf>,
    pub(crate) denials_path: Option<std::path::PathBuf>,
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// Re-applied after widening so grants cannot pierce a named preset.
    pub(crate) preset_clamp: Option<newt_core::Caveats>,
    pub(crate) danger: danger::DangerTable,
    pub(crate) color: bool,
    pub(crate) verbose: bool,
    /// Whether interactive AUTHORIZATION prompting is enabled this session. This
    /// is ORTHOGONAL to whether a human is present: the gate is built whenever
    /// the session has a usable TTY, so `ask_question` (the human-question seam)
    /// stays available even when this is false. When false, [`Self::ask`] denies
    /// every request immediately WITHOUT opening a prompt — disabling permission
    /// prompts must never erase the operator from `request_user_input`.
    pub(crate) authorization_prompts_enabled: bool,
    pub(crate) web_decision_timeout: Duration,
    /// Nested-form controls feed the same turn cancellation path as the watcher.
    pub(crate) cancel: Option<&'a AtomicBool>,
    pub(crate) exit: Option<&'a AtomicBool>,
    pub(crate) ask_human: F,
    /// **C1 (#1862): how this gate reaches the surface that owns the terminal.**
    ///
    /// `Some` under the two-thread split (`run_chat`): the session asks the UI
    /// thread, which acquires the `PromptWindow`. `None` where the gate IS on
    /// the terminal-owning thread — the single-threaded CLI entry points —
    /// and may take it directly. Those are two situations, not two
    /// implementations of one situation: the rendering and the read happen in
    /// `present_on_terminal` either way.
    ///
    /// `&dyn Fn` rather than a second generic parameter, so adding the seam
    /// does not propagate a type parameter through every construction site.
    pub(crate) ask_surface: Option<&'a dyn Fn(&SurfaceInteraction) -> HumanQuestionOutcome>,
}

impl<F: FnMut(&PromptWindow, &InteractionDefinition) -> PromptChoice> PromptPermissionGate<'_, F> {
    /// Record one decision: into the session list (for `/permissions`) and
    /// appended to the durable log. A log-write failure is reported but
    /// never blocks the decision — the record is a review artifact, not a
    /// gate.
    fn record(&mut self, req: &newt_core::PermissionRequest, decision: &str, scope: &str) {
        let rec = newt_core::PermissionRecord::new(
            &self.conversation_id,
            &req.tool,
            req.kind,
            &req.target,
            decision,
            scope,
        );
        if let Some(path) = self.log_path.as_deref() {
            if let Err(e) = rec.append_jsonl(path) {
                print_newt(
                    &format!("warning: permission log write failed: {e}"),
                    self.color,
                    self.verbose,
                );
            }
        }
        self.state.decisions.push(rec);
    }

    /// Re-mint baseline plus grants from the root; never widen the live key.
    fn mint(&self, once_grants: &[(newt_core::DenialKind, String)]) -> newt_core::Caveats {
        let mut grants: Vec<(newt_core::DenialKind, String)> =
            self.state.session_grants.iter().cloned().collect();
        grants.extend(once_grants.iter().cloned());
        let mut policy = newt_core::widen_caveats(&self.base, &grants);
        // Re-clamping is load-bearing: widening may repopulate an emptied scope.
        if let Some(clamp) = &self.preset_clamp {
            policy = policy.meet(clamp);
        }
        match self
            .key_path
            .as_deref()
            .and_then(|p| mint_operating_key(p, &policy).ok())
        {
            Some(key) => newt_identity::enforced_caveats(&key).unwrap_or(policy),
            None => policy,
        }
    }

    /// Publish the same typed form the web renders and poll for its answer.
    fn await_web_decision(
        &self,
        store: &newt_core::ConversationStore,
        w: &newt_core::tty::PromptWindow,
        req: &newt_core::PermissionRequest,
    ) -> (PromptChoice, &'static str) {
        // B0b-2 (#1846): publish the OFFER — the definition AND the instance
        // that binds it — so the answer is validated against the offer that
        // was actually published, not one minted at answer time.
        // C4b (#1944): the offer is open to BOTH surfaces, and its option set
        // is scoped to the NARROWER of them.
        //
        // #1894 framed this as publishing the terminal SUPERSET and having
        // each view filter what it renders. The inverse is smaller and
        // safer. One instance means one definition, because the definition
        // digest is inside the instance's `ContentId` — so the choice is
        // which audience's option set both surfaces share. Scoping to the
        // INTERSECTION makes every rendered option answerable by whoever is
        // looking at it, which turns `binding.rs:462`'s `ActionNotEligible`
        // from a backstop that fires in normal use into a refusal that is
        // unreachable by construction. The superset would instead put
        // `Allow permanently` on the web card and refuse it on submit — a
        // button that always fails, which is #1536's defect, not a cosmetic
        // one.
        //
        // Nothing regresses. Until now the terminal operator could not
        // answer AT ALL while the web was attached — only Back/Exit — so
        // gaining allow-once / session / deny is a strict improvement over
        // zero. The durable actions (`A`, `D`, `P`) remain available on the
        // web-detached path, where they always were. Widening the offer to
        // carry them for BOTH surfaces means a per-view render filter and
        // moving the web card's goldens; that is a separate slice with its
        // own visible change, not a rider on this one.
        let definition = permission_definition(req, &self.danger, Audience::Web);
        // D0 (#1878): the renderability precondition that stood here is GONE,
        // on the authority of its own comment — "this is a renderability
        // PRECONDITION, and it retires with C3's removal of the
        // reconstruction". C3c (#1870) removed that reconstruction: the web
        // builds its card from the `InteractionDefinition` directly, so there
        // is no longer a legacy form it could fail to reconstruct. The check
        // was guarding a rendering path that no longer exists.
        let tier = if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
            newt_core::interaction_offer::OfferDanger::High
        } else {
            newt_core::interaction_offer::OfferDanger::Low
        };
        let request_id = match store.publish_interaction_offer(
            &self.conversation_id,
            &definition,
            tier,
            &[Audience::Terminal, Audience::Web],
        ) {
            Ok(id) => id,
            Err(_) => return (PromptChoice::Deny, "web-unavailable"),
        };
        let note = definition
            .note
            .as_deref()
            .map_or(MODAL_CONTROL_HINT.to_string(), |note| {
                if note.contains(MODAL_CONTROL_HINT) {
                    note.to_string()
                } else {
                    format!("{note}\n{MODAL_CONTROL_HINT}")
                }
            });
        // C4b (#1944): RENDER the offer, not just an "awaiting…" line.
        //
        // The operator can now answer this, and a capability nobody can see
        // is the failure this epic keeps paying for. Until now the terminal
        // showed only that it was waiting, because waiting was all it could
        // do. It shows the options because it can now act on them, through
        // the same canonical projection the terminal-only path renders (C0a)
        // — one definition, one rendering, whichever surface is looking.
        w.notice(&newt_line(
            &format!(
                "{}\n`{}` — you or the web; whoever answers first decides.\n{note}",
                plain::render(&definition),
                req.target
            ),
            self.color,
            self.verbose,
        ))
        .ok();
        // Delegate the wait loop to the injectable core so reader-recovery is
        // unit-testable with a scripted reader, a fake clock, and a no-op sleep.
        self.run_web_wait(
            store,
            &request_id,
            &definition,
            w,
            &mut WebWaitIo {
                reacquire: &mut move || {
                    modal_prompt_controls(w).map(|r| Box::new(r) as Box<dyn ControlReader + '_>)
                },
                now: &Instant::now,
                sleep: &mut std::thread::sleep,
                notify: &mut |message| {
                    w.notice(&newt_line(message, self.color, self.verbose)).ok();
                },
            },
        )
    }

    /// The web-decision wait loop with an INJECTABLE control-reader lifecycle.
    ///
    /// A transient terminal read error must never permanently disable local
    /// controls (the defect this replaces set `controls = None` forever after a
    /// single error). Here the reader runs a small state machine:
    /// `Live` → poll it; an [`io::ErrorKind::Interrupted`] error is transient and
    /// retries the SAME reader; any other error drops it and moves to `Retrying`
    /// (emitting at most one concise warning), which re-arms via `reacquire` at a
    /// bounded `reacquire_backoff` cadence — never busy-spinning. The gate is
    /// only built for an interactive session, so ANY acquisition failure —
    /// including `Unsupported` (a terminal-loss race between session setup and
    /// the first prompt) — also enters paced `Retrying`, never a permanent dead
    /// end; there is no "genuinely headless" path here to latch off.
    ///
    /// Through every reader state the web store poll and the fail-closed
    /// `deadline` keep running, and a recovered `Back`/`Exit` still resolves
    /// through [`Self::web_abort_choice`]'s exactly-once CAS — so a local abort
    /// and a web verdict can never both win. `reacquire`/`now`/`sleep` are
    /// injected so tests drive the loop deterministically without a real
    /// terminal or wall clock.
    fn run_web_wait<'w>(
        &self,
        store: &newt_core::ConversationStore,
        request_id: &str,
        definition: &InteractionDefinition,
        w: &'w newt_core::tty::PromptWindow,
        io_: &mut WebWaitIo<'_, 'w>,
    ) -> (PromptChoice, &'static str) {
        let WebWaitIo {
            reacquire,
            now,
            sleep,
            notify,
        } = io_;
        enum ReaderState<'a> {
            Live(Box<dyn ControlReader + 'a>),
            Retrying { next_attempt: Instant },
        }

        let control_poll_timeout = Duration::from_millis(200);
        let reacquire_backoff = Duration::from_millis(200);
        let deadline = now() + self.web_decision_timeout;

        let mut warned = false;
        let mut state: ReaderState<'w> = match reacquire() {
            Ok(reader) => ReaderState::Live(reader),
            // ANY acquisition failure — including `Unsupported`, which here is a
            // terminal-loss race between the interactive session's setup and the
            // first prompt, NOT a genuinely headless session — enters paced retry
            // rather than permanently disabling the local controls. Warn once.
            Err(_) => {
                warned = true;
                self.notice_control_warning(w);
                ReaderState::Retrying {
                    next_attempt: now() + reacquire_backoff,
                }
            }
        };

        loop {
            // Advance the reader. `blocked` = we spent ~one poll timeout blocking,
            // so we should NOT also sleep this tick.
            let poll_result = match &mut state {
                ReaderState::Live(reader) => Some(reader.poll(control_poll_timeout)),
                _ => None,
            };
            let mut blocked = false;
            match poll_result {
                Some(Ok(Some(ModalLine::Back))) => {
                    return self
                        .web_abort_choice(
                            store,
                            &self.conversation_id,
                            request_id,
                            PromptChoice::Back,
                        )
                        .unwrap_or((PromptChoice::Back, "web-aborted"));
                }
                Some(Ok(Some(ModalLine::Exit))) => {
                    return self
                        .web_abort_choice(
                            store,
                            &self.conversation_id,
                            request_id,
                            PromptChoice::Exit,
                        )
                        .unwrap_or((PromptChoice::Exit, "web-aborted"));
                }
                // C4b (#1944): a typed line that names an offered option is
                // the operator DECIDING, and it contends for the same CAS the
                // web does.
                //
                // Only a RESOLVABLE line decides. An empty line or an
                // unrecognized key stays ignored, as it is today — this path
                // deliberately does NOT reuse `decode_answer`'s fail-closed
                // default. At a terminal-only prompt the operator is blocked
                // on answering and a bare Enter meaning "deny (default)" is
                // the documented contract; here there is a second responder
                // and a deadline, so a bumped Enter silently denying a
                // request would be a new way to lose work.
                Some(Ok(Some(ModalLine::Line(answer)))) => {
                    if let Some(action) = resolve_answer(definition, &answer) {
                        return self.terminal_answer_choice(store, notify, request_id, action);
                    }
                    blocked = true;
                }
                // EOF, or a control we do not act on here; we polled.
                Some(Ok(_)) => blocked = true,
                Some(Err(e)) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // Transient (EINTR): keep the SAME reader and retry. This
                        // error returns IMMEDIATELY (it does not consume the poll
                        // timeout), so leave `blocked` false and let the paced
                        // sleep below run — otherwise repeated EINTR busy-spins.
                    } else {
                        // Broken reader: drop it, re-arm at a bounded cadence.
                        if !warned {
                            warned = true;
                            self.notice_control_warning(w);
                        }
                        state = ReaderState::Retrying {
                            next_attempt: now() + reacquire_backoff,
                        };
                    }
                }
                None => {
                    // Not live: try to re-arm when the backoff elapses. This gate
                    // was built for an interactive session, so a reader that once
                    // worked may return again (detach/reattach, PTY swap) — an
                    // `Unsupported` reacquire here is NOT terminal; keep retrying
                    // (bounded) until the deadline. There is no permanent
                    // "no terminal" state to latch: one reader-state transition
                    // must never permanently remove the escape hatch.
                    if let ReaderState::Retrying { next_attempt } = &mut state {
                        if now() >= *next_attempt {
                            match reacquire() {
                                Ok(reader) => state = ReaderState::Live(reader),
                                Err(_) => *next_attempt = now() + reacquire_backoff,
                            }
                        }
                    }
                }
            }

            match store.take_interaction_decision(&self.conversation_id, request_id) {
                Ok(Some(action)) => return (action, decision_scope(action)),
                Ok(None) if now() >= deadline => {
                    // Claim through the SAME CAS a web answer contends for.
                    // If a web answer won the race, take that answer;
                    // otherwise the timeout is a fail-closed denial. Expiry
                    // itself synthesizes nothing — the Deny below is the
                    // gate's fixed default, not a decision read out of the
                    // offer.
                    return match store.cancel_interaction_offer(&self.conversation_id, request_id) {
                        Ok(true) => (PromptChoice::Deny, "web-timeout"),
                        Ok(false) => match store
                            .take_interaction_decision(&self.conversation_id, request_id)
                        {
                            Ok(Some(action)) => (action, decision_scope(action)),
                            Ok(None) | Err(_) => (PromptChoice::Deny, "web-timeout"),
                        },
                        Err(_) => (PromptChoice::Deny, "web-timeout"),
                    };
                }
                Ok(None) => {}
                Err(_) => return (PromptChoice::Deny, "web-store-error"),
            }

            if !blocked {
                let remaining = deadline.saturating_duration_since(now());
                sleep(remaining.min(Duration::from_millis(200)));
            }
        }
    }

    /// Emit the single, concise "controls interrupted, retrying" warning used by
    /// [`Self::run_web_wait`] when a live control reader breaks mid-wait.
    fn notice_control_warning(&self, w: &newt_core::tty::PromptWindow) {
        w.notice(&newt_line(
            "terminal controls temporarily unavailable — retrying; the web decision and timeout still apply",
            self.color,
            self.verbose,
        ))
        .ok();
    }

    /// **The accept/deny decision** (B0b-1, #1842).
    ///
    /// `Question::parse` has already DECODED the operator's keystroke into
    /// a candidate action — aliases, ambiguity denial, case-distinct keys.
    /// It no longer authorizes. This does, through
    /// `newt_interaction::validate_response`, against the definition the
    /// form was rendered from and a registry of executable actions the
    /// definition cannot influence.
    ///
    /// Fails CLOSED: any refusal, and any failure to mint the offer, is a
    /// deny. There is no second opinion to consult.
    fn authorize(
        &self,
        definition: &InteractionDefinition,
        audience: Audience,
        decoded: PromptChoice,
    ) -> Result<PromptChoice, newt_interaction::Refusal> {
        // Back and Exit are local CONTROLS, not decisions: they are not
        // options of the form, they authorize nothing, and routing them
        // through the authorizer would refuse them as unknown options and
        // silently turn a cancellation into a denial.
        if matches!(decoded, PromptChoice::Back | PromptChoice::Exit) {
            return Ok(decoded);
        }
        let Ok((instance, lifecycle)) = newt_core::interaction_gate::mint_offer(
            definition,
            &self.state.workspace_key,
            &self.conversation_id,
            std::slice::from_ref(&audience),
            newt_core::interaction_gate::now_tick(),
        ) else {
            return Err(newt_interaction::Refusal::MissingRequiredControl {
                control: "decision".to_string(),
            });
        };
        newt_core::interaction_gate::authorize_action(
            definition,
            &instance,
            &lifecycle,
            &self.state.workspace_key,
            &newt_core::interaction_gate::permission_registry(audience.clone()),
            decoded,
            audience,
        )
    }

    /// Record an authorizer refusal, keeping the operator-facing message
    /// the fixed-enum code produced for the cases that have one.
    ///
    /// The DECISION is now `validate_response`'s — a high-danger session
    /// allow is refused because the form does not offer that option, not
    /// because a downstream `match` arm re-checked the tier. But the
    /// operator still gets the same sentence, and the audit record still
    /// carries the same specific reason, because a refusal that reads as
    /// a generic deny is a worse refusal.
    fn note_refusal(
        &mut self,
        w: &PromptWindow,
        req: &newt_core::PermissionRequest,
        decoded: PromptChoice,
        refusal: &newt_interaction::Refusal,
    ) {
        let high = self.danger.classify(req.kind, &req.target) == danger::DangerTier::High;
        let (scope, message) = match decoded {
            PromptChoice::AllowSession if high => (
                "session-allow-refused-high-danger",
                Some(format!(
                    "session allow refused for high-danger `{}` — \
                     allow once per op or deny (step-up is the future path)",
                    req.target
                )),
            ),
            PromptChoice::AllowPermanent if high => (
                "permanent-allow-refused-high-danger",
                Some(format!(
                    "permanent allow refused for high-danger `{}`",
                    req.target
                )),
            ),
            _ => ("unauthorized", None),
        };
        let _ = refusal;
        self.record(req, "deny", scope);
        if let Some(message) = message {
            w.notice(&newt_line(&message, self.color, self.verbose))
                .ok();
        }
    }

    fn web_abort_choice(
        &self,
        store: &newt_core::ConversationStore,
        conversation_id: &str,
        request_id: &str,
        fallback: PromptChoice,
    ) -> Option<(PromptChoice, &'static str)> {
        // The SAME single CAS the web answer contends for: whoever flips
        // `outcome` from NULL wins, and the loser reads the winner's answer.
        if store
            .cancel_interaction_offer(conversation_id, request_id)
            .ok()?
        {
            return Some((fallback, decision_scope(fallback)));
        }
        store
            .take_interaction_decision(conversation_id, request_id)
            .ok()
            .and_then(|action| action.map(|a| (a, decision_scope(a))))
    }

    /// Answer the offer AS THE TERMINAL, through the one CAS the web
    /// contends for.
    ///
    /// The mirror of [`Self::web_abort_choice`], and deliberately built the
    /// same way: try the transition, and if it was already resolved read the
    /// winner rather than inventing an outcome. What differs is the second
    /// half — a losing ABORT can hand back the winner's action silently,
    /// because the operator asked to leave rather than to decide. A losing
    /// ANSWER cannot: the operator made a decision, and letting the winner's
    /// verdict pass as theirs is C3b's defect (#1536) with the surfaces
    /// swapped. So the loser is told, on the terminal, before the value is
    /// returned.
    fn terminal_answer_choice(
        &self,
        store: &newt_core::ConversationStore,
        notify: &mut dyn FnMut(&str),
        request_id: &str,
        action: PromptChoice,
    ) -> (PromptChoice, &'static str) {
        let outcome = store.answer_interaction_offer(
            &self.conversation_id,
            request_id,
            action,
            Audience::Terminal,
        );
        match outcome {
            Ok(newt_core::store::AnswerOutcome::Answered) => (action, decision_scope(action)),
            Ok(_) => {
                // Lost the race, or the offer expired underneath us. Read the
                // winner in the same way the abort path does.
                match store.take_interaction_decision(&self.conversation_id, request_id) {
                    Ok(Some(winner)) => {
                        notify(&lost_to_web_message(action, winner));
                        (winner, decision_scope(winner))
                    }
                    // Resolved with no readable answer (a cancel, or an
                    // expiry): fail closed rather than guess.
                    _ => (PromptChoice::Deny, "web-raced"),
                }
            }
            Err(_) => (PromptChoice::Deny, "web-store-error"),
        }
    }

    fn apply_control(&self, action: PromptChoice) {
        if let Some(cancel) = self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if action == PromptChoice::Exit {
            if let Some(exit) = self.exit {
                exit.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl<F: FnMut(&PromptWindow, &InteractionDefinition) -> PromptChoice> newt_core::PermissionGate
    for PromptPermissionGate<'_, F>
{
    fn ask(&mut self, requests: &[newt_core::PermissionRequest]) -> newt_core::PermissionDecision {
        use newt_core::PermissionDecision::{Allow, Deny};
        if requests.is_empty() {
            return Deny;
        }
        // Authorization prompting disabled (`--no-prompt-for-permissions`): fail
        // closed WITHOUT opening a prompt. This gate still exists (the session is
        // interactive) so `ask_question` keeps working — disabling permission
        // prompts must not turn a present operator into "headless".
        if !self.authorization_prompts_enabled {
            for req in requests {
                self.record(req, "deny", "authorization-prompts-disabled");
            }
            return Deny;
        }
        // Previously recorded session, permanent, and OCAP denials short-circuit.
        if requests.iter().any(|r| {
            let key = (r.kind, r.target.clone());
            self.state.session_denials.contains(&key)
                || self.state.persistent_denials.contains(&key)
                || newt_core::ocap_store::evaluate_request(
                    &self.state.ocap_policy,
                    r.kind,
                    &r.target,
                ) == Some(newt_core::ocap_store::Verdict::Deny)
        }) {
            return Deny;
        }
        let mut once_grants: Vec<(newt_core::DenialKind, String)> = Vec::new();
        let web = self.state.web_store.clone();
        for req in requests {
            // GitWrite is non-axis, so enforce the readonly preset before minting.
            if req.kind == newt_core::DenialKind::GitWrite {
                if let Some(clamp) = &self.preset_clamp {
                    if !newt_core::git_caveats::GitCaveats::from_session(clamp).permits_commit() {
                        self.record(req, "deny", "preset-floor-git-readonly");
                        return Deny;
                    }
                }
            }
            if session_grant_covers(&self.state.session_grants, req) {
                continue;
            }
            // Durable approval pre-answers only a low-danger request.
            if newt_core::ocap_store::evaluate_request(
                &self.state.ocap_policy,
                req.kind,
                &req.target,
            ) == Some(newt_core::ocap_store::Verdict::Approve)
                && self.danger.classify(req.kind, &req.target) != danger::DangerTier::High
            {
                self.record(req, "allow", "ocap-approve");
                once_grants.push((req.kind, req.target.clone()));
                continue;
            }
            // Only the eventual operation consumes a request_permissions grant.
            if req.tool != "request_permissions" {
                if let Some(key) = take_pending_once(&mut self.state.pending_once_grants, req) {
                    self.record(req, "allow", "once");
                    once_grants.push(key);
                    continue;
                }
            }
            // The sole prompt seam suspends every competing terminal writer.
            let w = Terminal::suspend_for_prompt();
            let (choice, scope) = match &web {
                Some(store) => self.await_web_decision(store, &w, req),
                None => {
                    // ONE definition: rendered to the operator, and the
                    // authority the answer is checked against. No store is
                    // touched — see `b0b::the_default_terminal_path_
                    // performs_no_store_write`.
                    //
                    // C0a (#1856): the definition goes to the reader whole.
                    // It used to be flattened to a `Question` here first,
                    // which meant the value displayed and the value
                    // authorized against were two objects that happened to
                    // agree; now they are one.
                    let definition = permission_definition(req, &self.danger, Audience::Terminal);
                    let decoded = (self.ask_human)(&w, &definition);
                    match self.authorize(&definition, Audience::Terminal, decoded) {
                        Ok(choice) => (choice, decision_scope(choice)),
                        Err(refusal) => {
                            self.note_refusal(&w, req, decoded, &refusal);
                            return Deny;
                        }
                    }
                }
            };
            match choice {
                PromptChoice::AllowOnce => {
                    self.record(req, "allow", scope);
                    once_grants.push((req.kind, req.target.clone()));
                    // Carry proactive allow-once to the model's separate retry.
                    if req.tool == "request_permissions" {
                        let key = if req.kind == newt_core::DenialKind::Exec {
                            (req.kind, exec_grant_basename(&req.target).to_string())
                        } else {
                            (req.kind, req.target.clone())
                        };
                        self.state.pending_once_grants.insert(key);
                    }
                }
                PromptChoice::AllowSession => {
                    // The form omits this action for high danger; enforce it too.
                    if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
                        self.record(req, "deny", "session-allow-refused-high-danger");
                        w.notice(&newt_line(
                            &format!(
                                "session allow refused for high-danger `{}` — \
                                 allow once per op or deny (step-up is the future path)",
                                req.target
                            ),
                            self.color,
                            self.verbose,
                        ))
                        .ok();
                        return Deny;
                    }
                    self.record(req, "allow", scope);
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::AllowPermanent => {
                    // Only net has a durable per-target config allowlist.
                    if req.kind != newt_core::DenialKind::Net {
                        self.record(req, "allow", scope);
                        self.state
                            .session_grants
                            .insert((req.kind, req.target.clone()));
                        continue;
                    }
                    if self.danger.classify(req.kind, &req.target) == danger::DangerTier::High {
                        self.record(req, "deny", "permanent-allow-refused-high-danger");
                        w.notice(&newt_line(
                            &format!("permanent allow refused for high-danger `{}`", req.target),
                            self.color,
                            self.verbose,
                        ))
                        .ok();
                        return Deny;
                    }
                    let persistent_scope = match self.config_path.as_deref() {
                        Some(path) => {
                            match newt_core::Config::append_permission_net_host(path, &req.target) {
                                Ok(()) => "permanent",
                                Err(e) => {
                                    w.notice(&newt_line(
                                        &format!(
                                            "warning: could not persist net grant to config: {e} \
                                             (granted for this session only)"
                                        ),
                                        self.color,
                                        self.verbose,
                                    ))
                                    .ok();
                                    "permanent-persist-failed"
                                }
                            }
                        }
                        None => {
                            w.notice(&newt_line(
                                "no config path this session — net grant is session-only",
                                self.color,
                                self.verbose,
                            ))
                            .ok();
                            "session"
                        }
                    };
                    self.record(req, "allow", persistent_scope);
                    if persistent_scope == "permanent" {
                        w.notice(&newt_line(
                            &format!(
                                "added `{}` to [tui.permissions] net — future sessions \
                                 will not prompt for it",
                                req.target
                            ),
                            self.color,
                            self.verbose,
                        ))
                        .ok();
                    }
                    self.state
                        .session_grants
                        .insert((req.kind, req.target.clone()));
                }
                PromptChoice::Deny => {
                    self.record(req, "deny", scope);
                    return Deny;
                }
                PromptChoice::DenyAlways => {
                    self.record(req, "deny", "session");
                    self.state
                        .session_denials
                        .insert((req.kind, req.target.clone()));
                    return Deny;
                }
                PromptChoice::DenyPermanent => {
                    self.record(req, "deny", "permanent");
                    if let Some(path) = self.denials_path.as_deref() {
                        if let Err(e) = newt_core::append_denial(path, req.kind, &req.target) {
                            print_newt(
                                &format!("warning: permission denylist write failed: {e}"),
                                self.color,
                                self.verbose,
                            );
                        }
                    }
                    self.state
                        .persistent_denials
                        .insert((req.kind, req.target.clone()));
                    return Deny;
                }
                control @ (PromptChoice::Back | PromptChoice::Exit) => {
                    self.apply_control(control);
                    return Deny;
                }
            }
        }
        Allow(self.mint(&once_grants))
    }

    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        // C1 (#1862): the SESSION builds the semantic form; whichever surface
        // owns the terminal renders and reads it. Previously this thread called
        // `Terminal::suspend_for_prompt` itself — taking stdin and writing
        // prompt bytes from the session thread, which is what
        // `session_worker`'s law 1 ("a worker never writes terminal bytes")
        // forbids and what the cockpit made observable.
        //
        // Blocking on the operator still surfaces to the cockpit through the
        // tty arbiter's Blocked/Unblocked, because `suspend_for_prompt` still
        // happens — just on the other side of the seam.
        let interaction = SurfaceInteraction::blocking(free_text_form(question));
        let outcome = match self.ask_surface {
            Some(ask) => ask(&interaction),
            None => {
                let w = Terminal::suspend_for_prompt();
                present_on_terminal(&w, &interaction)
            }
        };
        // The CONTROL side effects are the session's, not the terminal's: only
        // this side owns the turn's cancel/exit flags. `present_on_terminal`
        // reports what happened and applies nothing.
        match outcome {
            // Esc / slash-command back-out: cancel the turn, report Cancelled
            // (never "headless"). The adapter rewrites a typed `/cmd` into a
            // back-out, so that lands here too.
            HumanQuestionOutcome::Cancelled => {
                self.apply_control(PromptChoice::Back);
                HumanQuestionOutcome::Cancelled
            }
            // Ctrl-C / Ctrl-D: cancel the turn AND request exit.
            HumanQuestionOutcome::ExitRequested => {
                self.apply_control(PromptChoice::Exit);
                HumanQuestionOutcome::ExitRequested
            }
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "permissions_tests/permission_prompt_tests.rs"]
mod permission_prompt_tests;

/// **A0 byte goldens (#1823, epic #1803): the plain permission-prompt
/// rendering, frozen verbatim.** These strings ARE the current contract —
/// including the deliberately DIFFERENT terminal/web action matrices (web
/// omits every durable grant; net+low+terminal is the only permanent-allow) —
/// and epic law 1 says a later slice must keep the plain fallback. C0a
/// (#1856) extracted the rendering out of the semantic type and these
/// strings did not move, which is what "extracted" is allowed to mean —
/// they are now produced by `markup::plain::render`. An intentional
/// rendering change updates these strings in the same PR, listed as an
/// intentional diff (unlisted golden drift is a bug, per the epic's F0 rule).
#[cfg(test)]
#[path = "permissions_tests/a0_freeze_goldens.rs"]
mod a0_freeze_goldens;

/// **C0a (#1856, epic #1803): the plain renderer reproduces the frozen
/// bytes, and the operator-visible prompt is pinned for the first time.**
///
/// A0 froze `terminal_text()`'s output; C0a moves that rendering to
/// `newt_core::markup::plain::render` and deletes the method. The whole
/// proof of correctness is byte-identity, so the goldens are restated here
/// a third time, deliberately, under A0's own duplication discipline: each
/// copy independently catches an accidental edit to another.
///
/// What this copy adds that the other two do not: it pins the **composed**
/// string `prompt_permission_choice` actually hands the terminal — the
/// rendering plus `MODAL_INPUT_GLYPH` on its own final line. The A0 sweep
/// recorded that nothing per-PR covered that composition (the only test
/// that saw it end to end is the real-PTY one, `#[ignore]`d to the weekly
/// tier), and it is exactly the shape `tty::modal::render` depends on when
/// it repaints only the text after the last `\n`.
#[cfg(test)]
#[path = "permissions_tests/c0a.rs"]
mod c0a;

/// **B0a (#1841): one definition builder for both surfaces.**
///
/// A0 froze the rendered strings; these tests prove the DEFINITION path
/// reproduces them and that the per-surface action matrices live in the
/// definition rather than in a renderer. The golden strings are repeated
/// here deliberately: A0's copy freezes the contract, this copy proves the
/// new construction path reaches it, and an accidental edit to one copy is
/// caught by the other.
///
/// **`Question::parse` remains the authoritative accept/deny path.**
/// Nothing here uses `binding::validate_response` — that move, with the
/// behavior-map and formal-model updates it requires, is #1842.
#[cfg(test)]
#[path = "permissions_tests/b0a.rs"]
mod b0a;

/// **B0b-1 (#1842): the accept/deny decision, on the A3 controller.**
///
/// `Question::parse` is KEPT and demoted to input decoding — aliases,
/// ambiguity denial, case-distinct keys. `validate_response` authorizes.
/// These tests pin the four facts that move, and the two that must not.
#[cfg(test)]
#[path = "permissions_tests/b0b.rs"]
mod b0b;
