//! **The canonical plain renderer** (C0a of epic #1803, #1856).
//!
//! One deterministic projection from an [`InteractionDefinition`] to the
//! bytes a plain surface prints. This is the renderer `Question::terminal_text`
//! used to be, moved off the semantic type — the epic's named C0 deletion
//! gate ("semantic core types no longer expose terminal rendering").
//!
//! ## Why it lives here and not in `newt-interaction`
//!
//! `newt-interaction` is the inward layer: pure data, no rendering, ever
//! (its `tests/guard.rs` arms that over the resolved dependency closure AND
//! the crate's own source, each half with an anti-vacuous twin). A renderer
//! is a VIEW, and ADR law 8 says views are replaceable projections. So the
//! projection lives one layer out, in `newt-core`, which already depends on
//! the protocol crate. `newt-tui` and `newt-web` both reach `newt-core`;
//! neither reaches the other, which is why a renderer in `newt-tui` could
//! not have served both.
//!
//! Placement is also what keeps the wyvern tier honest: this module is
//! compiled UNCONDITIONALLY (no `markdown` feature gate), like
//! `tty::widgets`, so `--no-default-features` still carries the canonical
//! fallback. It takes no `pulldown-cmark`, no `crossterm`, and no terminal —
//! it returns a `String` and touches no fd.
//!
//! ## What it renders, exactly
//!
//! Three parts, in order, joined by a single `\n`, with EMPTY parts dropped:
//!
//! 1. `definition.markdown` — the readable body, emitted verbatim. The plain
//!    surface deliberately does NOT run it through a Markdown pass; the web
//!    view does (`newt-web/src/shell.rs`). That divergence is recorded in
//!    the A0 inventory (§5.6) and is C3's to resolve, not this slice's — C0a
//!    is byte-identity or it is nothing.
//! 2. `definition.note` — the subordinate line (control hint, danger
//!    warning). Itself possibly multi-line; emitted verbatim.
//! 3. One line per option of each [`ControlKind::Choice`] control: each
//!    rendered as `label` with the FIRST occurrence of its `key` bracketed
//!    (`allow once` + key `a` ⇒ `[a]llow once`). C0c (#1907) gave every
//!    option its own line; they used to share one, joined by three spaces.
//!
//! Aliases are never rendered — they are hidden parse affordances, and
//! rendering one would advertise an input that the displayed set does not
//! carry (BHV-PROMPT-005).
//!
//! There is no trailing newline. Callers that draw an input line append
//! their own (`newt-tui`'s `MODAL_INPUT_GLYPH` on its own final line), and
//! `newt_core::tty::modal::render` repaints only the text after the LAST
//! `\n` — so emitting a trailing newline here would silently repaint the
//! wrong row.
//!
//! ## Scope (C0a)
//!
//! Only [`ControlKind::Choice`] renders CONTROLS. Every other control kind
//! contributes nothing to the plain form today, which is exactly what the
//! predecessor did — an actionless form rendered as body + note. Giving
//! `Text`/`Toggle`/`Secret` a plain affordance, and giving each
//! `InteractionKind` a headless fallback that never waits or chooses a
//! default, is **C0b**. Nothing here is new behaviour: every byte this
//! module emits is a byte the tree already emitted.

use newt_interaction::{ChoiceOption, Control, ControlKind, InteractionDefinition};

/// **Each option gets its own line** (C0c, #1907).
///
/// Options used to be joined by three spaces onto one row. That was written
/// for a short permission menu and did not survive contact with a variable
/// list: `newt setup`'s endpoint and provider pickers carry ten or more
/// options, and its backend menu carries three labels with URLs in them.
///
/// It did not really survive the permission menu either. Six options at
/// **130 display columns** wrap on any terminal narrower than that — which is
/// most of them — and they wrap *mid-option*: at 80 columns the break lands
/// inside `[d]eny (default)`, tearing the fail-closed choice of the most
/// security-sensitive prompt in the product across a line boundary with no
/// alignment. The frozen bytes preserved that; they did not prevent it.
///
/// **Unconditional, and that is the point.** Every conditional rule
/// considered was either a cliff or read a field that must not decide
/// behaviour — see this module's `c0c` tests and the PR that introduced
/// them. A rule that changes layout when a label crosses N characters means
/// one more character silently renders a different prompt, and nothing fails
/// at the moment it happens.
const CHOICE_SEPARATOR: &str = "\n";

/// Render `definition` as canonical plain text.
///
/// Deterministic and pure: the same definition always yields the same
/// bytes, on every platform, with no ambient state consulted.
#[must_use]
pub fn render(definition: &InteractionDefinition) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    parts.push(&definition.markdown);
    if let Some(note) = definition.note.as_deref() {
        parts.push(note);
    }

    // One line per control that has something to show, in the definition's
    // presentation order. C0a emitted only `Choice`; C0b (#1860) gives every
    // control kind a projection, because a form asking for a username
    // rendered the operator no field at all.
    let control_lines: Vec<String> = definition
        .controls
        .iter()
        .filter_map(control_line)
        .filter(|line| !line.is_empty())
        .collect();
    parts.extend(control_lines.iter().map(String::as_str));

    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What one control contributes to the plain form, if anything.
///
/// The match is EXHAUSTIVE rather than `_ => None`: a new [`ControlKind`]
/// must fail to compile here instead of silently rendering nothing, which is
/// the defect C0b exists to fix.
///
/// **An unlabelled control contributes no line**, and that rule is load
/// bearing rather than cosmetic. Both frozen forms carry `label: ""` — the
/// permission menu's decision control (its options carry the text) and
/// `prompt_user_input`'s answer field (its question is the body) — so giving
/// the other kinds an affordance cannot move A0's goldens or C0a's free-text
/// bytes. `c0b::an_unlabelled_control_adds_no_line` pins it.
fn control_line(control: &Control) -> Option<String> {
    match &control.kind {
        ControlKind::Choice { options } => Some(choice_lines(options)),
        ControlKind::Text => labelled_field(control, ""),
        // `[y/n]`, never `[y/N]`. The house style elsewhere used to
        // capitalize the default — `sas_confirm` did until F0c (#1928)
        // retired its builder — and this
        // projection must advertise none: the epic's global acceptance
        // criterion is that headless modes never CHOOSE A DEFAULT, and a
        // rendered default is how one gets chosen by accident.
        ControlKind::Toggle => labelled_field(control, " [y/n]"),
        // A secret shows its LABEL and never its value (ADR D1). The value
        // cannot leak here by construction — `ControlKind::Secret` carries
        // none, and a submitted secret travels as `ControlValue::Secret {
        // reference }` on the RESPONSE, which this renderer never sees. The
        // marker exists so no surface treats the field as ordinary text.
        ControlKind::Secret => labelled_field(control, " (secret, not echoed)"),
    }
}

/// `{label}:{suffix}`, or nothing when the control has no label.
fn labelled_field(control: &Control, suffix: &str) -> Option<String> {
    (!control.label.is_empty()).then(|| format!("{}:{suffix}", control.label))
}

/// One control's options, one per line.
fn choice_lines(options: &[ChoiceOption]) -> String {
    options
        .iter()
        .map(bracket_key)
        .collect::<Vec<_>>()
        .join(CHOICE_SEPARATOR)
}

/// An option's label with its accelerator bracketed in place.
///
/// Replaces the FIRST occurrence of the key as a SUBSTRING of the label —
/// `deny (default)` + key `d` is `[d]eny (default)`, not `deny (`[d]`efault)`.
/// Two consequences are inherited verbatim from the predecessor and pinned
/// by tests rather than left to be rediscovered:
///
/// - a key that does not occur in its label produces NO bracket at all, and
/// - an EMPTY key brackets at position zero (`[]label`), because
///   `replacen("", …, 1)` matches the empty prefix.
///
/// Neither is reachable from the permission matrices (every key is the
/// label's own initial), and CHANGING either would move frozen bytes, which
/// is out of scope for a byte-identity slice. D0/C0b own deciding whether
/// the affordance should be modelled instead of derived.
fn bracket_key(option: &ChoiceOption) -> String {
    option
        .label
        .replacen(&option.key, &format!("[{}]", option.key), 1)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use newt_interaction::{
        Control, ControlId, InteractionKind, OptionId, Requirement, SemanticRole,
    };

    pub(super) fn option(id: &str, key: &str, label: &str) -> ChoiceOption {
        ChoiceOption {
            id: OptionId::new(id).expect("valid option id"),
            role: SemanticRole::Allow,
            label: label.to_string(),
            key: key.to_string(),
            aliases: Vec::new(),
        }
    }

    pub(super) fn choice(options: Vec<ChoiceOption>) -> Control {
        Control {
            id: ControlId::new("decision").expect("valid control id"),
            kind: ControlKind::Choice { options },
            label: String::new(),
            requirement: Requirement::Required,
        }
    }

    pub(super) fn definition(controls: Vec<Control>) -> InteractionDefinition {
        InteractionDefinition::new(InteractionKind::Choice, "body", controls)
    }

    #[test]
    fn body_note_and_choices_join_with_single_newlines_in_that_order() {
        let mut d = definition(vec![choice(vec![
            option("allow_once", "a", "allow once"),
            option("deny", "d", "deny (default)"),
        ])]);
        d.note = Some("hint".to_string());
        assert_eq!(render(&d), "body\nhint\n[a]llow once\n[d]eny (default)");
    }

    #[test]
    fn an_absent_note_is_dropped_rather_than_rendered_as_a_blank_line() {
        let d = definition(vec![choice(vec![option("deny", "d", "deny")])]);
        assert_eq!(render(&d), "body\n[d]eny");
    }

    #[test]
    fn an_empty_note_is_dropped_too() {
        // `Some(String::new())` and `None` must render identically — the
        // predecessor filtered on emptiness, not on presence.
        let mut d = definition(vec![choice(vec![option("deny", "d", "deny")])]);
        d.note = Some(String::new());
        assert_eq!(render(&d), "body\n[d]eny");
    }

    #[test]
    fn a_form_with_no_options_renders_body_and_note_only() {
        // The `request_user_input` shape: a prompt with nothing to pick.
        let mut d = InteractionDefinition::new(
            InteractionKind::Prompt,
            "? what is your name",
            vec![Control {
                id: ControlId::new("answer").expect("valid control id"),
                kind: ControlKind::Text,
                label: String::new(),
                requirement: Requirement::Required,
            }],
        );
        d.note = Some("Esc=back".to_string());
        assert_eq!(render(&d), "? what is your name\nEsc=back");
    }

    #[test]
    fn a_choice_control_with_zero_options_adds_no_line() {
        let d = definition(vec![choice(Vec::new())]);
        assert_eq!(render(&d), "body");
    }

    /// **C0c (#1907): every option gets its own line.** This test used to be
    /// `options_are_separated_by_exactly_three_spaces` and asserted
    /// `"body\n[a]a   [b]b   [c]c"`. The change is deliberate and is
    /// documented with its old and new bytes in the amendment commit.
    #[test]
    fn options_get_one_line_each() {
        let d = definition(vec![choice(vec![
            option("a", "a", "aa"),
            option("b", "b", "bb"),
            option("c", "c", "cc"),
        ])]);
        assert_eq!(render(&d), "body\n[a]a\n[b]b\n[c]c");
        assert_eq!(CHOICE_SEPARATOR, "\n");
    }

    #[test]
    fn aliases_are_never_rendered() {
        // BHV-PROMPT-005: an alias parses but must never be advertised.
        let mut opt = option("deny", "n", "n to skip");
        opt.aliases = vec!["N".to_string(), "no".to_string()];
        let d = definition(vec![choice(vec![opt])]);
        let out = render(&d);
        assert_eq!(out, "body\n[n] to skip");
        assert!(
            !out.contains('N'),
            "an alias reached the rendered form: {out}"
        );
        assert!(
            !out.contains("no"),
            "an alias reached the rendered form: {out}"
        );
    }

    #[test]
    fn the_key_is_bracketed_at_its_first_occurrence_only() {
        let d = definition(vec![choice(vec![option("x", "a", "banana")])]);
        assert_eq!(render(&d), "body\nb[a]nana");
    }

    /// **Inherited quirks, pinned rather than fixed.** Changing either
    /// would move bytes, which C0a may not do. Neither is reachable from
    /// the permission matrices.
    #[test]
    fn a_key_absent_from_its_label_brackets_nothing_and_an_empty_key_brackets_at_zero() {
        let missing = definition(vec![choice(vec![option("x", "z", "allow once")])]);
        assert_eq!(render(&missing), "body\nallow once");

        let empty = definition(vec![choice(vec![option("x", "", "allow once")])]);
        assert_eq!(render(&empty), "body\n[]allow once");
    }

    #[test]
    fn there_is_no_trailing_newline() {
        // `tty::modal::render` repaints only the text after the last `\n`;
        // a trailing newline here would repaint the wrong row.
        let d = definition(vec![choice(vec![option("deny", "d", "deny")])]);
        assert!(!render(&d).ends_with('\n'));
    }

    #[test]
    fn rendering_is_deterministic() {
        let mut d = definition(vec![choice(vec![
            option("allow_once", "a", "allow once"),
            option("deny", "d", "deny (default)"),
        ])]);
        d.note = Some("hint\nsecond line".to_string());
        assert_eq!(render(&d), render(&d));
    }

    #[test]
    fn a_multi_line_note_is_emitted_verbatim() {
        // The high-danger permission note is two lines (danger + control
        // hint) and must not be reflowed.
        let mut d = definition(vec![choice(vec![option("deny", "d", "deny")])]);
        d.note = Some("high-danger: refused\nEsc=back".to_string());
        assert_eq!(render(&d), "body\nhigh-danger: refused\nEsc=back\n[d]eny");
    }

    #[test]
    fn every_choice_control_contributes_its_own_line() {
        // Unreachable today (every definition carries one control), but the
        // rule is "one line per choice control", not "the first one wins".
        let d = definition(vec![
            choice(vec![option("a", "a", "aa")]),
            Control {
                id: ControlId::new("second").expect("valid control id"),
                kind: ControlKind::Choice {
                    options: vec![option("b", "b", "bb")],
                },
                label: String::new(),
                requirement: Requirement::Optional,
            },
        ]);
        assert_eq!(render(&d), "body\n[a]a\n[b]b");
    }
}

/// **C0b (#1860): the plain projection is the conformance baseline for every
/// interaction kind and every control kind.**
///
/// C0a proved ONE kind (`Choice`) byte-identical to its predecessor. These
/// tests state the projection for the other four and for the three control
/// kinds that previously rendered nothing, so "plain fallback" is a defined
/// contract rather than whatever falls out of the `filter_map`.
#[cfg(test)]
mod c0b {
    use super::*;
    use newt_interaction::{
        Control, ControlId, InteractionKind, OptionId, Requirement, SemanticRole,
    };

    fn control(id: &str, kind: ControlKind, label: &str) -> Control {
        Control {
            id: ControlId::new(id).expect("valid control id"),
            kind,
            label: label.to_string(),
            requirement: Requirement::Required,
        }
    }

    fn choice_of(pairs: &[(&str, &str, &str)]) -> ControlKind {
        ControlKind::Choice {
            options: pairs
                .iter()
                .map(|(id, key, label)| ChoiceOption {
                    id: OptionId::new(*id).expect("valid option id"),
                    role: SemanticRole::Allow,
                    label: (*label).to_string(),
                    key: (*key).to_string(),
                    aliases: Vec::new(),
                })
                .collect(),
        }
    }

    fn definition(
        kind: InteractionKind,
        body: &str,
        controls: Vec<Control>,
    ) -> InteractionDefinition {
        InteractionDefinition::new(kind, body, controls)
    }

    /// **Every `InteractionKind` has a proven plain projection.**
    ///
    /// One representative definition per kind, asserted to exact bytes. The
    /// kind is SEMANTIC — it does not branch the renderer (ADR: `modal` is a
    /// view decision, not a kind) — so what this pins is that each kind's
    /// natural shape projects to something a plain surface can print, and
    /// that `Notice` in particular is defined rather than incidental.
    #[test]
    fn every_interaction_kind_has_a_plain_projection() {
        let cases: Vec<(InteractionKind, InteractionDefinition, &str)> = vec![
            (
                InteractionKind::Choice,
                definition(
                    InteractionKind::Choice,
                    "pick one",
                    vec![control(
                        "decision",
                        choice_of(&[("a", "a", "allow once"), ("d", "d", "deny")]),
                        "",
                    )],
                ),
                "pick one\n[a]llow once\n[d]eny",
            ),
            (
                InteractionKind::Prompt,
                definition(
                    InteractionKind::Prompt,
                    "? which file",
                    vec![control("answer", ControlKind::Text, "path")],
                ),
                "? which file\npath:",
            ),
            (
                InteractionKind::Confirm,
                definition(
                    InteractionKind::Confirm,
                    "delete it?",
                    vec![control("confirm", ControlKind::Toggle, "delete")],
                ),
                "delete it?\ndelete: [y/n]",
            ),
            (
                InteractionKind::Form,
                definition(
                    InteractionKind::Form,
                    "credentials",
                    vec![
                        control("user", ControlKind::Text, "username"),
                        control("pass", ControlKind::Secret, "API key"),
                    ],
                ),
                "credentials\nusername:\nAPI key: (secret, not echoed)",
            ),
            (
                InteractionKind::Notice,
                // A Notice carries NO controls and expects no response. Its
                // projection is the body (and note); stated here so it is a
                // contract rather than a consequence of an empty vector.
                definition(InteractionKind::Notice, "the build finished", Vec::new()),
                "the build finished",
            ),
        ];
        assert_eq!(cases.len(), 5, "a kind was added without a projection");
        for (kind, def, expected) in cases {
            assert_eq!(render(&def), expected, "{kind:?} projected wrongly");
        }
    }

    /// **Every `ControlKind` renders.** Before C0b only `Choice` did; `Text`,
    /// `Toggle`, and `Secret` contributed nothing, so a form asking for a
    /// username showed the operator no field at all.
    #[test]
    fn every_control_kind_renders() {
        for (kind, expected) in [
            (ControlKind::Text, "body\nfield:"),
            (ControlKind::Toggle, "body\nfield: [y/n]"),
            (ControlKind::Secret, "body\nfield: (secret, not echoed)"),
        ] {
            let def = definition(
                InteractionKind::Form,
                "body",
                vec![control("c", kind.clone(), "field")],
            );
            assert_eq!(render(&def), expected);
        }
        // Choice is covered by the A0 goldens; asserted here too so the set
        // is visibly exhaustive.
        let def = definition(
            InteractionKind::Choice,
            "body",
            vec![control("c", choice_of(&[("y", "y", "yes")]), "")],
        );
        assert_eq!(render(&def), "body\n[y]es");
    }

    /// **A `Secret` renders its LABEL and never its value**, and says so in a
    /// form that does not invite an echo (ADR D1). The value cannot leak by
    /// construction — `ControlKind::Secret` is a unit variant carrying no
    /// value, and a submitted secret travels as `ControlValue::Secret {
    /// reference }` in the RESPONSE, which this renderer never sees. This
    /// test pins the label half and the no-echo wording.
    #[test]
    fn a_secret_renders_its_label_and_marks_itself_unechoed() {
        let def = definition(
            InteractionKind::Form,
            "auth",
            vec![control("k", ControlKind::Secret, "API key")],
        );
        let out = render(&def);
        assert_eq!(out, "auth\nAPI key: (secret, not echoed)");
        assert!(out.contains("API key"), "the label must be shown: {out}");
        assert!(
            out.contains("not echoed"),
            "a secret field must say it is not echoed: {out}"
        );
    }

    /// **An empty label contributes no line.** This is what keeps A0's frozen
    /// goldens and C0a's free-text assertion byte-identical: the permission
    /// menu's decision control and `prompt_user_input`'s answer field both
    /// carry `label: ""`, so giving the other control kinds an affordance
    /// cannot move either one's bytes.
    #[test]
    fn an_unlabelled_control_adds_no_line() {
        for kind in [ControlKind::Text, ControlKind::Toggle, ControlKind::Secret] {
            let def = definition(
                InteractionKind::Prompt,
                "? ask",
                vec![control("c", kind, "")],
            );
            assert_eq!(render(&def), "? ask");
        }
    }

    /// **The projection never implies a default.** The epic's C0 bullet says
    /// headless "never ... chooses a default"; a rendered `[y/N]` (the
    /// `sas_confirm` house style, where the capital marks the default) would
    /// advertise one the model does not carry.
    #[test]
    fn a_toggle_advertises_no_default() {
        let def = definition(
            InteractionKind::Confirm,
            "go?",
            vec![control("c", ControlKind::Toggle, "proceed")],
        );
        let out = render(&def);
        assert!(out.contains("[y/n]"), "{out}");
        assert!(!out.contains("[y/N]"), "a default was advertised: {out}");
        assert!(!out.contains("[Y/n]"), "a default was advertised: {out}");
    }
}

/// **C0c (#1907): one option per line, unconditionally.**
///
/// The rule is uniform because every conditional version considered was
/// either a cliff or read a field that must not decide layout. Those are
/// design arguments; these are the assertions that keep the code matching
/// them.
#[cfg(test)]
mod c0c {
    use super::render;
    use super::tests::{choice, definition, option};

    fn menu(labels: &[&str]) -> String {
        let options = labels
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let key = (b'1' + u8::try_from(i).expect("small")) as char;
                option(
                    &key.to_string(),
                    &key.to_string(),
                    &format!("{key} {label}"),
                )
            })
            .collect();
        render(&definition(vec![choice(options)]))
    }

    /// **No size makes the layout join.** Two short options and nine long
    /// ones project the same way, so there is no count and no label length at
    /// which the rendering silently becomes something else.
    ///
    /// This is the assertion that distinguishes C0c's rule from the width
    /// threshold rejected in #1906: a threshold is defined by the value at
    /// which it flips, and this has none to test.
    #[test]
    fn the_rule_is_unconditional_and_has_no_threshold() {
        let short = menu(&["a", "b"]);
        assert_eq!(short, "body\n[1] a\n[2] b");

        let long = menu(&[
            "Ollama on this machine (http://127.0.0.1:11434)",
            "Custom host or URL (llama.cpp, vLLM, a gateway)",
            "A hosted provider (OpenAI, Anthropic, OpenRouter, NVIDIA)",
            "d",
            "e",
            "f",
            "g",
            "h",
            "i",
        ]);
        assert_eq!(long.lines().count(), 10, "body plus one line per option");
        assert!(
            long.lines().skip(1).all(|l| l.starts_with('[')),
            "each option starts its own line: {long}"
        );
    }

    /// **The anti-vacuous twin.** Every assertion above is satisfied by a
    /// renderer that emits one line per option — and also by one that emits
    /// nothing but the body, since `lines().skip(1)` over an empty tail is
    /// vacuously all-`starts_with`. So: the option count must actually drive
    /// the line count, and the old joined form must be distinguishable from
    /// the new one rather than merely absent.
    #[test]
    fn the_option_count_drives_the_line_count() {
        for n in 1..6 {
            let labels: Vec<String> = (0..n).map(|i| format!("opt{i}")).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            assert_eq!(
                menu(&refs).lines().count(),
                n + 1,
                "{n} options must render {n} lines under the body"
            );
        }
        // The shape this replaced is genuinely different, not a formatting
        // nicety: joined-on-one-line and one-per-line disagree byte for byte.
        let rendered = menu(&["a", "b"]);
        assert_ne!(rendered, "body\n[1] a   [2] b");
        assert!(!rendered.contains("   "), "no three-space join survives");
    }

    /// **The defect the frozen bytes preserved.** Six options at 130 display
    /// columns wrap on any narrower terminal, and at 80 the break lands
    /// inside `[d]eny (default)` — tearing the fail-closed choice of the
    /// permission prompt across a line boundary. Each option now owns a line,
    /// so no option can be split by a terminal narrower than itself.
    #[test]
    fn no_option_is_torn_by_a_narrow_terminal() {
        let rendered = render(&definition(vec![choice(vec![
            option("allow-once", "a", "allow once"),
            option("session-allow", "s", "session allow"),
            option(
                "allow-permanent",
                "A",
                "Allow permanently (adds host to config)",
            ),
            option("deny", "d", "deny (default)"),
            option("deny-always", "D", "Deny always"),
            option("deny-permanent", "P", "Permanently deny"),
        ])]));
        assert_eq!(rendered.lines().count(), 7, "body plus six options");
        assert!(
            rendered.lines().any(|l| l == "[d]eny (default)"),
            "the fail-closed option is whole, on its own line: {rendered}"
        );
        // The widest option is 41 columns, so every line fits a terminal that
        // could not fit the old 130-column row.
        assert!(
            rendered.lines().skip(1).all(|l| l.chars().count() <= 41),
            "no option line exceeds the widest option: {rendered}"
        );
    }
}
