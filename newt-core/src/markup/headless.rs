//! **Headless / noninteractive execution of an interaction** (C0b of epic
//! #1803, #1860).
//!
//! The epic's global acceptance criterion is that *"Headless/protocol modes
//! never wait, choose defaults, or emit terminal bytes."* This module is the
//! seam that makes that checkable for an [`InteractionDefinition`]: a
//! headless caller hands one in, gets back the plain fallback to print or
//! transport, and **cannot obtain a response from it at all**.
//!
//! # Three states, not two
//!
//! `!is_terminal()` is a property of a file descriptor, not a mode, and
//! conflating the two is the trap this module exists to keep out of the
//! codebase. The tree distinguishes:
//!
//! 1. **Interactive TTY** — `tty::modal`'s key-by-key reader.
//! 2. **Piped but ANSWERED** — `read_prompt_window_line`'s `!is_terminal()`
//!    branch: the eval harness, `printf … | newt solve`. A writer is present
//!    and an answer is coming, so reading a line is correct. Its convention
//!    is an A0 freeze (#1823, `tty::modal::headless_line_tests`) and this
//!    slice does not touch it.
//! 3. **Headless / protocol** — no operator at all. Such callers never
//!    construct a [`PromptWindow`](crate::tty::PromptWindow): the chat loop
//!    gates on `interactive` (both fds are ttys) and the worker/ACP/MCP
//!    loops carry `permission_gate: None`. **This module serves state 3.**
//!
//! The epic's criterion governs state 3; the A0 freeze pins state 2. They are
//! disjoint, which is why C0b needed to unfreeze nothing — and the epic's own
//! C0 bullet, which in the same breath says to *preserve pipe behavior*, only
//! coheres under that reading.
//!
//! # Fail-closed means producing NOTHING, not producing a deny
//!
//! A3 settled this and it is the security property here. `SemanticRole` and
//! `ChoiceOption.role` are **author-assigned**, and a definition may come
//! from untrusted markup (ADR law 11). So a headless path that "failed
//! closed" by scanning for `role == Deny` and submitting it would let the
//! definition's author choose what failing closed means — an attacker
//! labels their allow option `Deny` and headless submits it.
//!
//! Fail-closed here is **refusing to produce a [`Response`] at all**, and
//! that is enforced by construction rather than by discipline: nothing in
//! this module can build one. [`Fallback`] carries text and shortfalls; it
//! has no variant, field, or constructor through which an action could
//! travel. `c0b::headless_never_synthesizes_a_response_from_an_authored_role`
//! and its source-scanned twin hold the line.
//!
//! [`Response`]: newt_interaction::Response
//!
//! # Placement
//!
//! Beside [`plain`](super::plain) and for its reasons: compiled
//! unconditionally so the wyvern tier carries it, no `pulldown-cmark`, no
//! `crossterm`, no fd. It returns values; **printing or transporting them is
//! the caller's**, which is what keeps "never emits terminal bytes" a
//! property the caller's own `LineCaps`/protocol-mode veto decides.

use newt_interaction::{
    plan_presentation, InteractionDefinition, ProtocolError, Requirement, SurfaceFeature,
};

/// What a headless surface may do with a definition it can present.
///
/// Deliberately not `String`: a shortfall that vanished into the text would
/// be indistinguishable from a faithful render, and ADR law 5 requires an
/// unsatisfied OPTIONAL demand to degrade **visibly**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    /// The plain projection, to print or transport verbatim.
    text: String,
    /// Optional demands this surface could not meet, in the order the
    /// definition asked for them.
    unmet_optional: Vec<String>,
}

impl Fallback {
    /// The bytes to print or transport.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Optional capabilities the surface lacked. Empty means faithful.
    #[must_use]
    pub fn unmet_optional(&self) -> &[String] {
        &self.unmet_optional
    }

    /// Whether everything the definition asked for was satisfiable.
    #[must_use]
    pub fn is_faithful(&self) -> bool {
        self.unmet_optional.is_empty()
    }
}

/// Present `definition` on a headless surface with the given capabilities.
///
/// Returns the plain fallback to print or transport. **Never returns a
/// response, an action, or a default** — a headless run has nobody to answer,
/// and the epic's acceptance criterion is that it never waits or chooses one.
///
/// # Errors
///
/// [`ProtocolError::UnsupportedFeature`] when the definition makes a
/// **required** demand this surface cannot meet — including a required
/// control whose kind implies a capability, such as a `Secret` needing
/// `secret-input`. That is the fail-closed case: the interaction cannot be
/// presented faithfully, and guessing is not on offer.
///
/// The decision is delegated to [`plan_presentation`], the A2 machinery that
/// already encodes law 5's required/optional split — and which, until this
/// slice, had no production caller at all.
pub fn present(
    definition: &InteractionDefinition,
    supported: &[SurfaceFeature],
) -> Result<Fallback, ProtocolError> {
    let presentation = plan_presentation(definition, supported)?;
    Ok(Fallback {
        text: super::plain::render(definition),
        unmet_optional: presentation
            .degradations()
            .iter()
            .map(|d| d.feature().to_string())
            .collect(),
    })
}

/// Whether `definition` demands anything a surface with `supported` cannot
/// meet and is not allowed to skip.
///
/// The question a caller asks BEFORE deciding to offer an interaction at all.
/// Answering it does not render, so it is cheap enough to gate on.
#[must_use]
pub fn has_unsatisfiable_requirement(
    definition: &InteractionDefinition,
    supported: &[SurfaceFeature],
) -> bool {
    plan_presentation(definition, supported).is_err()
}

/// The capabilities a plain, noninteractive surface has.
///
/// Deliberately EMPTY, and deliberately a function rather than a constant so
/// the reason is documented where it is used: a headless run can echo
/// nothing, draw nothing, and accept nothing, so it advertises no
/// [`SurfaceFeature`] at all. A definition with a required `Secret` control
/// therefore fails closed here, which is the intended reading of "resolves
/// unsupported required controls fail-closed".
#[must_use]
pub fn headless_capabilities() -> Vec<SurfaceFeature> {
    Vec::new()
}

/// Whether the definition expects a response at all.
///
/// True when any control is [`Requirement::Required`]. A caller asks this to
/// tell "present it and move on" from "this needs an answer I cannot get".
///
/// A [`Notice`](newt_interaction::InteractionKind::Notice) expects none, so a
/// headless surface can present it and move on. Anything with a required
/// control cannot be completed headlessly, and the caller must not pretend
/// otherwise.
#[must_use]
pub fn expects_a_response(definition: &InteractionDefinition) -> bool {
    definition
        .controls
        .iter()
        .any(|c| c.requirement == Requirement::Required)
}

#[cfg(test)]
mod c0b {
    use super::*;
    use newt_interaction::{
        ChoiceOption, Control, ControlId, ControlKind, FeatureDemand, InteractionKind, OptionId,
        SemanticRole,
    };

    fn control(id: &str, kind: ControlKind, label: &str, requirement: Requirement) -> Control {
        Control {
            id: ControlId::new(id).expect("valid control id"),
            kind,
            label: label.to_string(),
            requirement,
        }
    }

    fn notice() -> InteractionDefinition {
        InteractionDefinition::new(InteractionKind::Notice, "the build finished", Vec::new())
    }

    /// A definition whose ONLY option is author-labelled `Deny` — the shape
    /// an attacker supplies when they want a "fail-closed" path to submit
    /// something for them.
    fn authored_deny() -> InteractionDefinition {
        InteractionDefinition::new(
            InteractionKind::Choice,
            "proceed?",
            vec![control(
                "decision",
                ControlKind::Choice {
                    options: vec![ChoiceOption {
                        id: OptionId::new("deny").expect("valid option id"),
                        role: SemanticRole::Deny,
                        label: "deny".to_string(),
                        key: "d".to_string(),
                        aliases: Vec::new(),
                    }],
                },
                "",
                Requirement::Required,
            )],
        )
    }

    /// **Fail-closed is refusing to produce a response, not synthesizing a
    /// deny.** Roles are author-assigned and markup is untrusted (ADR law
    /// 11), so resolving by scanning for `role == Deny` would let the
    /// definition's author decide what failing closed means.
    ///
    /// The property is enforced by construction — `Fallback` has no field
    /// through which an action could travel — so this test states the
    /// behaviour and `the_headless_module_cannot_build_a_response` scans the
    /// source for the constructor that would break it.
    #[test]
    fn headless_never_synthesizes_a_response_from_an_authored_role() {
        let definition = authored_deny();
        let fallback = present(&definition, &headless_capabilities()).expect("presentable");

        // What comes back is text, and only text.
        assert_eq!(fallback.text(), "proceed?\n[d]eny");
        assert!(fallback.is_faithful());

        // The definition DOES expect a response — and headless still does
        // not produce one. "It expects an answer" must never become "so
        // supply one".
        assert!(
            expects_a_response(&definition),
            "the fixture must be one that expects an answer, or this proves nothing"
        );

        // An author-chosen role is visible in the definition and changes
        // nothing about the outcome: presenting a Deny-roled form and an
        // Allow-roled form yields the same KIND of result — text, no action.
        let mut allow = authored_deny();
        let ControlKind::Choice { options } = &mut allow.controls[0].kind else {
            panic!("fixture is a choice");
        };
        options[0].role = SemanticRole::Allow;
        let allowed = present(&allow, &headless_capabilities()).expect("presentable");
        assert_eq!(
            allowed.unmet_optional(),
            fallback.unmet_optional(),
            "the authored role changed the headless outcome"
        );
    }

    /// **Anti-vacuous twin: the module cannot build a `Response`.** The test
    /// above asserts a behaviour; this asserts the property that makes the
    /// behaviour unfalsifiable. A source scan that looked for nothing would
    /// pass on any file, so it also proves it FIRES on a seeded constructor.
    #[test]
    fn the_headless_module_cannot_build_a_response() {
        let source = include_str!("headless.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the production half");
        for forbidden in ["Response {", "Response::", "Submission {", "Submission::"] {
            assert!(
                !production.contains(forbidden),
                "the headless module names `{forbidden}` — fail-closed means it \
                 cannot build a response, not that it chooses not to"
            );
        }
        // ...and the scan would notice one.
        let seeded = "fn f() { Response::new(x) }";
        assert!(
            ["Response {", "Response::"]
                .iter()
                .any(|n| seeded.contains(n)),
            "the needle set cannot see a response constructor at all"
        );
    }

    /// **A required demand this surface cannot meet fails closed.** A
    /// headless surface advertises no capability, so a required `Secret`
    /// control — which intrinsically demands `secret-input` — cannot be
    /// presented faithfully and is refused rather than rendered without its
    /// field.
    #[test]
    fn headless_refuses_an_unsupported_required_control() {
        let definition = InteractionDefinition::new(
            InteractionKind::Form,
            "credentials",
            vec![control(
                "key",
                ControlKind::Secret,
                "API key",
                Requirement::Required,
            )],
        );
        assert!(has_unsatisfiable_requirement(
            &definition,
            &headless_capabilities()
        ));
        let err = present(&definition, &headless_capabilities()).expect_err("must fail closed");
        assert!(
            matches!(err, ProtocolError::UnsupportedFeature { ref feature, .. }
                     if feature == SurfaceFeature::SECRET_INPUT),
            "unexpected refusal: {err:?}"
        );

        // ...and a surface that CAN take secret input presents it. Without
        // this half the refusal could be unconditional and still pass.
        let capable = [SurfaceFeature::new(SurfaceFeature::SECRET_INPUT).expect("valid")];
        let ok = present(&definition, &capable).expect("a capable surface presents it");
        assert_eq!(ok.text(), "credentials\nAPI key: (secret, not echoed)");
    }

    /// **An unmet OPTIONAL demand degrades visibly rather than refusing**
    /// (ADR law 5's other half). If this collapsed into the required case,
    /// headless would refuse every document that merely wanted a diagram.
    #[test]
    fn an_unmet_optional_demand_degrades_visibly() {
        let mut definition = notice();
        definition.features.push(FeatureDemand {
            feature: SurfaceFeature::new(SurfaceFeature::DIAGRAMS).expect("valid"),
            requirement: Requirement::Optional,
        });
        let fallback = present(&definition, &headless_capabilities()).expect("presents");
        assert_eq!(fallback.unmet_optional(), [SurfaceFeature::DIAGRAMS]);
        assert!(
            !fallback.is_faithful(),
            "a dropped diagram must be visible, not inferred from an empty list"
        );
        assert_eq!(fallback.text(), "the build finished");
    }

    /// **A `Notice` is presentable headlessly and expects no response.** It
    /// is the one kind a headless run can complete, and the reason
    /// `expects_a_response` exists rather than being assumed true.
    #[test]
    fn a_notice_is_presentable_and_expects_no_response() {
        let definition = notice();
        assert!(!expects_a_response(&definition));
        let fallback = present(&definition, &headless_capabilities()).expect("presents");
        assert_eq!(fallback.text(), "the build finished");
        assert!(fallback.is_faithful());
    }

    /// **An OPTIONAL control does not make a definition answer-expecting.**
    /// The twin for `expects_a_response`: without it the function could
    /// return `true` unconditionally and every test above would still pass.
    #[test]
    fn only_a_required_control_makes_a_definition_expect_an_answer() {
        let optional = InteractionDefinition::new(
            InteractionKind::Form,
            "notes",
            vec![control(
                "note",
                ControlKind::Text,
                "note",
                Requirement::Optional,
            )],
        );
        assert!(!expects_a_response(&optional));

        let required = InteractionDefinition::new(
            InteractionKind::Form,
            "notes",
            vec![control(
                "note",
                ControlKind::Text,
                "note",
                Requirement::Required,
            )],
        );
        assert!(expects_a_response(&required));
    }

    /// **State 3 never becomes state 2.** The module header claims headless
    /// callers never wait; this is what makes the claim checkable rather
    /// than aspirational. A headless path that grew a `PromptWindow`, a
    /// `stdin` read, or an `is_terminal()` branch would be waiting — the
    /// exact thing the epic's global acceptance criterion forbids — and it
    /// would do so while every behavioural test above still passed.
    ///
    /// Comment lines are stripped before the scan, so the module's own prose
    /// about `PromptWindow` is not a hit; the twin proves the scan still
    /// sees a real one.
    #[test]
    fn the_headless_path_never_waits_on_a_terminal() {
        let production = code_without_comments(
            include_str!("headless.rs")
                .split("#[cfg(test)]")
                .next()
                .expect("the production half"),
        );
        for waiting in ["PromptWindow", "stdin", "is_terminal", "read_line"] {
            assert!(
                !production.contains(waiting),
                "the headless module reaches `{waiting}` — headless must never \
                 wait for input it has nobody to receive"
            );
        }
        assert!(
            production.contains("plan_presentation"),
            "the scan is not reading this module's code at all"
        );
    }

    /// **Anti-vacuous twin for the no-waiting scan.**
    #[test]
    fn the_no_waiting_scan_sees_a_real_terminal_read() {
        let seeded = code_without_comments(
            "// a comment mentioning PromptWindow and stdin is not a hit\n\
             pub fn f() { let w = PromptWindow::x(); }\n",
        );
        assert!(
            seeded.contains("PromptWindow"),
            "the scan cannot see a real terminal reference"
        );
        let only_prose = code_without_comments(
            "// PromptWindow, stdin, is_terminal, read_line\npub fn f() {}\n",
        );
        for waiting in ["PromptWindow", "stdin", "is_terminal", "read_line"] {
            assert!(
                !only_prose.contains(waiting),
                "prose was counted as code: `{waiting}`"
            );
        }
    }

    /// Source with `//`-comment lines removed, so prose is not a hit.
    fn code_without_comments(source: &str) -> String {
        source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **Headless capabilities are empty, and that is the point.** Stated as
    /// a test so a later slice that grants the headless tier a capability has
    /// to say so here rather than silently widening what it will present.
    #[test]
    fn a_headless_surface_advertises_no_capability() {
        assert!(headless_capabilities().is_empty());
    }
}
