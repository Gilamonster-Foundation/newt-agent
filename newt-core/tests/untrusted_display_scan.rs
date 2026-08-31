//! **No surface renders a direction-forcing control** (#1941).
//!
//! ADR law 11: a definition may come from untrusted markup, so a definition's
//! `markdown`, a control's `label`, an option's `label`, and a note are all
//! attacker-chosen. `U+202E` in any of them renders `allow` and `deny` in
//! visually swapped order — spoofing the exact decision a permission prompt
//! exists to take, while everything else stays correct: the definition is
//! well-formed, its `ContentId` is honest, and `validate_response` accepts the
//! answer. Only what the human saw was wrong.
//!
//! # This table IS the surface inventory
//!
//! One row per way author text reaches a human. A new display producer belongs
//! here, and a row that is easy to add is the point — the first attempt at this
//! fix neutralised `spans::project`'s input, which covered the markdown body
//! and silently missed the option labels that carry `allow` and `deny`.
//!
//! `newt-web`'s `render_markdown` is the fifth surface. It cannot be reached
//! from this crate (a separate, workspace-excluded package) and is pinned by
//! `shell::tests::c3a_bidi` instead.
//!
//! # Both directions, every row
//!
//! Each surface is rendered twice: once with the hazard planted, once with
//! benign text in the same position. Without the second, every assertion here
//! would pass against a surface that rendered nothing at all.

use newt_core::markup::plain;
use newt_core::memory::MemMessage;
use newt_interaction::{
    ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
    OptionId, Requirement, SemanticRole,
};

/// RIGHT-TO-LEFT OVERRIDE — the primitive. `PDF` closes the run so the
/// fixture is a well-formed document rather than a truncated one.
const RLO: char = '\u{202E}';
const PDF: char = '\u{202C}';
/// What the benign twin puts where the hazard went.
const BENIGN: &str = "SENTINEL";

/// A permission-shaped definition, with `text` in one author-controlled slot.
fn definition(slot: Slot, text: &str) -> InteractionDefinition {
    let (markdown, option_label, note) = match slot {
        Slot::Markdown => (text.to_string(), "allow once".to_string(), None),
        Slot::OptionLabel => ("run bash?".to_string(), text.to_string(), None),
        Slot::Note => (
            "run bash?".to_string(),
            "allow once".to_string(),
            Some(text.to_string()),
        ),
    };
    let mut definition = InteractionDefinition::new(
        InteractionKind::Confirm,
        markdown,
        vec![Control {
            id: ControlId::new("decision").expect("valid"),
            kind: ControlKind::Choice {
                options: vec![
                    ChoiceOption {
                        id: OptionId::new("allow").expect("valid"),
                        role: SemanticRole::Allow,
                        label: option_label,
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                    ChoiceOption {
                        id: OptionId::new("deny").expect("valid"),
                        role: SemanticRole::Deny,
                        label: "deny".to_string(),
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                ],
            },
            label: String::new(),
            requirement: Requirement::Required,
        }],
    );
    definition.note = note;
    definition
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    Markdown,
    OptionLabel,
    Note,
}

/// Flatten the rich view's rows to the text a terminal would print.
///
/// `markdown`-gated, like `markup::spans` and `interaction_view` themselves.
/// The plain and transcript rows below are UNCONDITIONAL, because they are
/// what the lean / headless / wyvern tier renders — the tier where a spoofed
/// prompt would be the only thing an operator sees.
#[cfg(feature = "markdown")]
fn rich_text(definition: &InteractionDefinition) -> String {
    let interaction =
        newt_core::interaction_surface::SurfaceInteraction::blocking(definition.clone());
    newt_core::interaction_view::InteractionView::new(&interaction)
        .rows()
        .iter()
        .flat_map(|row| row.spans.iter())
        .map(|span| span.text.as_str())
        .collect()
}

/// One render path: a name for the failure message, and the function that
/// turns author text into what a human sees.
type Surface = (&'static str, fn(&str) -> String);

/// Every way author-controlled text reaches a human.
/// The surfaces the `markdown` tier adds — empty when it is off, which is why
/// this is a function rather than a `#[cfg]` on an `extend` call: `out` is
/// then always mutated, and the lean build needs no `allow(unused_mut)`.
fn markdown_surfaces() -> Vec<Surface> {
    #[cfg(feature = "markdown")]
    {
        vec![
            ("rich spans / markdown body", |t| {
                rich_text(&definition(Slot::Markdown, t))
            }),
            // The one the first attempt missed: an option label becomes a
            // span WITHOUT passing through `spans::project`, so neutralising
            // that function's input covered the body and left `allow` /
            // `deny` spoofable.
            ("rich spans / option label", |t| {
                rich_text(&definition(Slot::OptionLabel, t))
            }),
            ("rich spans / note", |t| {
                rich_text(&definition(Slot::Note, t))
            }),
            ("span projection", |t| {
                newt_core::markup::spans::project(t)
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|s| s.text.as_str())
                    .collect()
            }),
        ]
    }
    #[cfg(not(feature = "markdown"))]
    {
        Vec::new()
    }
}

fn surfaces() -> Vec<Surface> {
    let mut out: Vec<Surface> = vec![
        ("plain / markdown body", |t| {
            plain::render(&definition(Slot::Markdown, t))
        }),
        ("plain / option label", |t| {
            plain::render(&definition(Slot::OptionLabel, t))
        }),
        ("plain / note", |t| {
            plain::render(&definition(Slot::Note, t))
        }),
        ("transcript", |t| {
            newt_core::agentic::transcript_lines(&[MemMessage::assistant(t)], 200)
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }),
    ];
    out.extend(markdown_surfaces());
    out
}

/// **The guard.** No surface emits a direction-forcing control.
#[test]
fn no_surface_renders_a_direction_forcing_control() {
    let planted = format!("allow {RLO}ynedD{PDF} once");
    for (name, render) in surfaces() {
        let out = render(&planted);
        assert!(
            !out.contains(RLO) && !out.contains(PDF),
            "`{name}` rendered a bidi override verbatim — a permission prompt \
             on this surface can show `allow` and `deny` swapped: {out:?}"
        );
        assert!(
            out.contains("<U+202E>"),
            "`{name}` did not make the hidden character VISIBLE. Dropping it \
             silently hides that anything was there, which is the failure \
             being fixed rather than a fix for it: {out:?}"
        );
    }
}

/// **The anti-vacuous twin.** Every assertion above would hold over a surface
/// that rendered nothing at all. Each one really does render the author text
/// it was given, in the same slot the hazard was planted in.
#[test]
fn every_surface_actually_renders_the_slot_that_was_scanned() {
    for (name, render) in surfaces() {
        let out = render(&format!("allow {BENIGN} once"));
        assert!(
            out.contains(BENIGN),
            "`{name}` did not render its author text at all, so the scan \
             assertion for it proves nothing: {out:?}"
        );
        assert!(
            !out.contains("<U+"),
            "`{name}` neutralised benign text — the policy refuses everything: {out:?}"
        );
    }
}

/// An `ESC` in the same position repaints the terminal rather than reordering
/// it. Same slot, same policy, and it was found by the same reproduction.
#[test]
fn no_surface_renders_a_raw_escape() {
    for (name, render) in surfaces() {
        let out = render("run \u{1b}[2K\u{1b}[Aoops");
        assert!(
            !out.contains('\u{1b}'),
            "`{name}` passed a raw ESC to the terminal: {out:?}"
        );
        assert!(out.contains("<U+001B>"), "`{name}`: {out:?}");
    }
}

/// **Legitimate RTL still works, on every surface.**
///
/// The constraint that ruled out "reject everything invisible". Arabic and
/// Hebrew need no controls at all — the bidi algorithm orders strong
/// characters from their own class — and the marks and isolates real RTL text
/// does carry are permitted, so they arrive unaltered.
#[test]
fn every_surface_passes_legitimate_rtl_untouched() {
    for (name, render) in surfaces() {
        for text in ["تشغيل bash؟", "האם להריץ bash?", "run \u{200F}عربى now"] {
            let out = render(text);
            assert!(
                out.contains(text),
                "`{name}` altered legitimate RTL text — a scan that rejects all \
                 bidi is not fail-closed, it is broken for two scripts: {out:?}"
            );
        }
    }
}
