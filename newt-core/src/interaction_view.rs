//! **The interaction view model** (C2 of epic #1803, #1876).
//!
//! What a richer surface draws when an interaction arrives, and what it does
//! with a keystroke — rows, a selection, and an answer — expressed with **no
//! terminal and no renderer**.
//!
//! # Why it lives here and not in `newt-tui`
//!
//! Because here the boundary is UNREPRESENTABLE rather than linted.
//! `newt-interaction`'s guard is the precedent: `newt-core` has no `ratatui`
//! dependency at all, so a `Rect`, a `Style`, or a `Frame` in this file does
//! not compile. In `newt-tui` — where `ratatui` and `crossterm` are
//! NON-OPTIONAL deps — the same boundary could only ever be a source scan
//! that a reviewer has to keep honest.
//!
//! #1876 constraint 1 says no renderer type may reach the model and that the
//! guard is on the author. Moving the model one crate down hands that guard
//! to the compiler instead, and takes the same shape C1 used for the
//! thread-shaped exclusion: an invariant you cannot forget beats one you must
//! remember.
//!
//! The `rich-tui`-gated renderer that consumes this lives in
//! `newt_tui::interaction_view`, and is the only half that names a widget.
//!
//! # It is a VIEW of the definition, never a second source of truth
//!
//! `newt_core::markup::plain::render` is the canonical projection (C0a/C0b),
//! and this model is required to agree with it about CONTENT: the same body,
//! the same note, the same options in the same order. What it adds is
//! structure a richer surface can use — one row per option instead of one
//! joined line, `Emphasis` roles from the shared span projection, and a
//! selection cursor. [`InteractionView::fallback`] hands back the canonical
//! text for any surface that cannot draw the rest.
//!
//! # `modal` is not here, and that is the point
//!
//! C1 put `Attention` in the model — the asker says it needs the operator
//! now — and left "so draw a modal" to the view. Nothing in this file
//! decides modality; the terminal half reads
//! [`SurfaceInteraction::wants_attention`] and picks. A `modal: bool` on any
//! type in this module would be the regression #1876 constraint 2 names.
//!
//! [`SurfaceInteraction::wants_attention`]:
//!     newt_core::interaction_surface::SurfaceInteraction::wants_attention

use crate::interaction_surface::SurfaceInteraction;
use crate::markup::spans::{self, Emphasis, Span};
use newt_interaction::{ControlKind, InteractionDefinition};

/// What a rendered row IS — never how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The readable body.
    Body,
    /// The subordinate line: a control hint, a danger warning.
    Note,
    /// One selectable option of a choice control.
    Option {
        /// Index into [`InteractionView::options`].
        index: usize,
    },
    /// A field a surface would let the operator type into.
    Field,
}

/// One drawable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRow {
    pub kind: RowKind,
    pub spans: Vec<Span>,
}

impl ViewRow {
    /// The row's characters, roles dropped. Test-only: the renderer styles
    /// each span individually and never flattens a row.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// One selectable option, reduced to what answering needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    /// The WIRE name. This is what an answer submits, because it is the
    /// stable identity — `Question::parse` accepts it, and a key is a
    /// per-surface affordance that may be absent.
    pub id: String,
    /// The accelerator, if the definition offers one.
    pub key: String,
    pub label: String,
}

/// An interaction, as rows plus a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionView {
    rows: Vec<ViewRow>,
    options: Vec<OptionRow>,
    selected: usize,
    fallback: String,
}

impl InteractionView {
    /// Build the view for one interaction.
    pub fn new(interaction: &SurfaceInteraction) -> Self {
        Self::of_definition(&interaction.definition)
    }

    fn of_definition(definition: &InteractionDefinition) -> Self {
        let mut rows = Vec::new();
        let mut options = Vec::new();

        // The body goes through the SHARED span projection, so the rich view
        // and the plain one read the same dialect (C3a left exactly one).
        for line in spans::project(&definition.markdown) {
            rows.push(ViewRow {
                kind: RowKind::Body,
                spans: line.spans,
            });
        }

        // The note is emitted VERBATIM, matching `plain::render`. It is a
        // control hint, not authored prose, and running it through Markdown
        // would let a `*` in a danger warning silently become emphasis.
        if let Some(note) = definition.note.as_deref() {
            for line in note.split('\n').filter(|l| !l.is_empty()) {
                rows.push(ViewRow {
                    kind: RowKind::Note,
                    spans: vec![Span::plain(line)],
                });
            }
        }

        for control in &definition.controls {
            match &control.kind {
                // NATIVE CONTROLS: one row per option, selectable. The plain
                // projection joins them onto a single line because that is
                // all a scrolled surface can do; a rich one can offer a
                // cursor, which is the difference C2 exists to make.
                ControlKind::Choice { options: offered } => {
                    for option in offered {
                        let index = options.len();
                        options.push(OptionRow {
                            id: option.id.as_str().to_string(),
                            key: option.key.clone(),
                            label: option.label.clone(),
                        });
                        rows.push(ViewRow {
                            kind: RowKind::Option { index },
                            spans: option_spans(&option.key, &option.label),
                        });
                    }
                }
                // Every non-choice kind shows its label plus what the KIND
                // advertises. The suffix table is `ControlKind::hint`, shared
                // with the plain projection — a secret says " (secret, not
                // echoed)" on BOTH surfaces because there is one table, not
                // because two lists happen to agree today (ADR D1).
                kind => push_field(&mut rows, &control.label, &kind.hint()),
            }
        }

        Self {
            rows,
            options,
            selected: 0,
            fallback: crate::markup::plain::render(definition),
        }
    }

    pub fn rows(&self) -> &[ViewRow] {
        &self.rows
    }

    /// Test-only: the renderer draws option ROWS and answers from the
    /// selection, so it never needs the reduced list.
    pub fn options(&self) -> &[OptionRow] {
        &self.options
    }

    /// Which option the cursor is on, if any.
    pub fn selected(&self) -> Option<usize> {
        (!self.options.is_empty()).then_some(self.selected)
    }

    /// The canonical plain projection of the same definition.
    ///
    /// What a surface prints when it cannot draw the rest, and what gets
    /// committed to scrollback when the interaction closes — so the durable
    /// record is the canonical form on every surface (#1876 constraint 7).
    pub fn fallback(&self) -> &str {
        &self.fallback
    }

    /// Move the cursor, saturating rather than wrapping.
    ///
    /// Saturating on purpose: wrapping from "deny" back to "allow" on one
    /// extra keypress is how an operator authorizes something they meant to
    /// refuse. A cursor that stops is a cursor you can hold a key against.
    pub fn move_selection(&mut self, delta: isize) {
        if self.options.is_empty() {
            return;
        }
        let last = self.options.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    /// The answer for the option the cursor is on.
    pub fn answer_for_selection(&self) -> Option<String> {
        self.options.get(self.selected).map(|o| o.id.clone())
    }

    /// The answer for a typed accelerator, if it names one.
    ///
    /// Exact match, never case-folded: the permission menu deliberately
    /// carries case-distinct keys (`a` allow-once vs `A` allow-permanently),
    /// and folding here would let a weaker keystroke select a stronger
    /// grant — the hazard `Question::parse` documents at its own alias rule.
    pub fn answer_for_key(&self, typed: char) -> Option<String> {
        let typed = typed.to_string();
        let mut hits = self.options.iter().filter(|o| o.key == typed);
        let first = hits.next()?;
        // Ambiguity denies, exactly as the parser does.
        hits.next().is_none().then(|| first.id.clone())
    }
}

/// `[k]ey`-bracketed label, matching the plain projection's affordance so the
/// two surfaces read the same.
fn option_spans(key: &str, label: &str) -> Vec<Span> {
    if key.is_empty() {
        return vec![Span::plain(label)];
    }
    match label.find(key) {
        Some(at) => {
            let mut out = Vec::new();
            if at > 0 {
                out.push(Span::plain(&label[..at]));
            }
            out.push(Span::styled(format!("[{key}]"), Emphasis::Strong));
            let rest = &label[at + key.len()..];
            if !rest.is_empty() {
                out.push(Span::plain(rest));
            }
            out
        }
        None => vec![Span::plain(label)],
    }
}

/// A labelled field row, or nothing when the control carries no label —
/// the rule that keeps A0's frozen goldens and the free-text form unmoved
/// (C0b), restated here so the rich view agrees with the plain one.
fn push_field(rows: &mut Vec<ViewRow>, label: &str, suffix: &str) {
    if label.is_empty() {
        return;
    }
    rows.push(ViewRow {
        kind: RowKind::Field,
        spans: vec![Span::plain(format!("{label}:{suffix}"))],
    });
}

#[cfg(test)]
mod c2 {
    use super::*;
    use newt_interaction::{
        ChoiceOption, Control, ControlId, InteractionKind, OptionId, Requirement, SemanticRole,
    };

    fn choice(pairs: &[(&str, &str, &str)]) -> InteractionDefinition {
        let mut d = InteractionDefinition::new(
            InteractionKind::Choice,
            "\u{2298} run_command wants to run `bash`",
            vec![Control {
                id: ControlId::new("decision").expect("valid"),
                kind: ControlKind::Choice {
                    options: pairs
                        .iter()
                        .map(|(id, key, label)| ChoiceOption {
                            id: OptionId::new(*id).expect("valid"),
                            role: SemanticRole::Allow,
                            label: (*label).to_string(),
                            key: (*key).to_string(),
                            aliases: Vec::new(),
                        })
                        .collect(),
                },
                label: String::new(),
                requirement: Requirement::Required,
            }],
        );
        d.note = Some("Esc=back \u{b7} Ctrl-C/Ctrl-D=exit".into());
        d
    }

    fn permission() -> InteractionView {
        InteractionView::of_definition(&choice(&[
            ("allow_once", "a", "allow once"),
            ("deny", "d", "deny (default)"),
        ]))
    }

    #[test]
    fn a_choice_renders_one_selectable_row_per_option() {
        // The difference C2 exists to make: the plain projection joins the
        // options onto ONE line because a scrolled surface can do no better.
        let view = permission();
        let option_rows: Vec<_> = view
            .rows()
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Option { .. }))
            .collect();
        assert_eq!(option_rows.len(), 2);
        assert_eq!(option_rows[0].text(), "[a]llow once");
        assert_eq!(option_rows[1].text(), "[d]eny (default)");
    }

    #[test]
    fn the_body_goes_through_the_shared_span_projection() {
        let view = permission();
        let body: Vec<_> = view
            .rows()
            .iter()
            .filter(|r| r.kind == RowKind::Body)
            .collect();
        assert_eq!(body.len(), 1);
        // `bash` is inline code in the source, so the shared projection
        // marks it — which is what a rich surface styles and a plain one
        // flattens away.
        assert!(
            body[0]
                .spans
                .iter()
                .any(|s| s.emphasis == Emphasis::Code && s.text == "bash"),
            "{:?}",
            body[0].spans
        );
    }

    #[test]
    fn the_note_is_verbatim_not_markdown() {
        // A `*` in a danger warning must not become emphasis.
        let mut d = choice(&[("deny", "d", "deny")]);
        d.note = Some("high-danger: *never* auto-approved".into());
        let view = InteractionView::of_definition(&d);
        let note = view
            .rows()
            .iter()
            .find(|r| r.kind == RowKind::Note)
            .expect("a note row");
        assert_eq!(note.text(), "high-danger: *never* auto-approved");
        assert!(note.spans.iter().all(|s| s.emphasis == Emphasis::Plain));
    }

    #[test]
    fn selection_saturates_rather_than_wrapping() {
        // Wrapping from deny back to allow on one extra keypress is how an
        // operator authorizes something they meant to refuse.
        let mut view = permission();
        assert_eq!(view.selected(), Some(0));
        view.move_selection(-1);
        assert_eq!(view.selected(), Some(0), "wrapped backwards off the top");
        view.move_selection(1);
        view.move_selection(1);
        view.move_selection(1);
        assert_eq!(view.selected(), Some(1), "wrapped forwards off the bottom");
        assert_eq!(view.answer_for_selection().as_deref(), Some("deny"));
    }

    #[test]
    fn an_answer_submits_the_wire_id_not_the_key() {
        // The wire name is the stable identity; a key is a per-surface
        // affordance that may be absent entirely.
        let view = permission();
        assert_eq!(view.answer_for_selection().as_deref(), Some("allow_once"));
        assert_eq!(view.answer_for_key('d').as_deref(), Some("deny"));
    }

    #[test]
    fn accelerators_are_case_exact_and_ambiguity_denies() {
        // `a` vs `A` is allow-once vs allow-permanently on the real menu;
        // folding here would let a weaker keystroke select a stronger grant.
        let view = InteractionView::of_definition(&choice(&[
            ("allow_once", "a", "allow once"),
            ("allow_permanent", "A", "Allow permanently"),
        ]));
        assert_eq!(view.answer_for_key('a').as_deref(), Some("allow_once"));
        assert_eq!(view.answer_for_key('A').as_deref(), Some("allow_permanent"));
        assert_eq!(view.answer_for_key('z'), None);

        // Two options sharing a key deny rather than selecting the earlier,
        // exactly as `Question::parse` does.
        let ambiguous = InteractionView::of_definition(&choice(&[
            ("allow_once", "x", "one"),
            ("deny", "x", "two"),
        ]));
        assert_eq!(ambiguous.answer_for_key('x'), None);
    }

    #[test]
    fn a_form_with_no_options_offers_no_selection() {
        let d = InteractionDefinition::new(
            InteractionKind::Prompt,
            "? which file",
            vec![Control {
                id: ControlId::new("answer").expect("valid"),
                kind: ControlKind::Text,
                label: String::new(),
                requirement: Requirement::Required,
            }],
        );
        let mut view = InteractionView::of_definition(&d);
        assert_eq!(view.selected(), None);
        assert_eq!(view.answer_for_selection(), None);
        view.move_selection(1); // must not panic on an empty option list
        assert_eq!(view.selected(), None);
    }

    #[test]
    fn every_control_kind_produces_a_labelled_field_and_an_unlabelled_one_produces_none() {
        for (kind, expected) in [
            (ControlKind::Text, "path:"),
            (ControlKind::Toggle, "path: [y/n]"),
            (ControlKind::Secret, "path: (secret, not echoed)"),
            (
                ControlKind::Number {
                    min: Some(1),
                    max: Some(10),
                    step: None,
                },
                "path: [an integer in 1..=10]",
            ),
            (
                ControlKind::Range {
                    min: 0,
                    max: 100,
                    step: 5,
                },
                "path: [0..=100]",
            ),
            (
                ControlKind::Temporal {
                    precision: newt_interaction::TemporalPrecision::Date,
                },
                "path: [YYYY-MM-DD]",
            ),
            (ControlKind::Color, "path: [#rrggbb]"),
            (
                ControlKind::Path {
                    kind: newt_interaction::PathKind::File,
                    accept: Vec::new(),
                },
                "path: [file path]",
            ),
        ] {
            let d = InteractionDefinition::new(
                InteractionKind::Form,
                "body",
                vec![Control {
                    id: ControlId::new("c").expect("valid"),
                    kind,
                    label: "path".into(),
                    requirement: Requirement::Required,
                }],
            );
            let view = InteractionView::of_definition(&d);
            let field = view
                .rows()
                .iter()
                .find(|r| r.kind == RowKind::Field)
                .expect("a field row");
            assert_eq!(field.text(), expected);
        }
    }

    /// **The view agrees with the canonical projection about CONTENT.**
    ///
    /// RichTUI is a richer view of the same definition, not a second source
    /// of truth (#1876 constraint 7). Every option label the plain form
    /// shows must appear in the rich rows, and the fallback must be exactly
    /// what `plain::render` produces.
    #[test]
    fn the_rich_view_and_the_plain_projection_agree_on_content() {
        let definition = choice(&[
            ("allow_once", "a", "allow once"),
            ("deny", "d", "deny (default)"),
        ]);
        let view = InteractionView::of_definition(&definition);
        assert_eq!(view.fallback(), crate::markup::plain::render(&definition));
        // Compare the RENDERED forms, not the raw labels: both surfaces
        // bracket the accelerator, so `allow once` appears in neither as
        // written. The first cut of this test compared raw labels and
        // failed against correct code — the same reformat-fragile shape as
        // C1's wiring needle, caught here by the test rather than by review.
        for row in view
            .rows()
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Option { .. }))
        {
            assert!(
                view.fallback().contains(&row.text()),
                "the rich option row {:?} does not appear in the canonical \
                 projection {:?}",
                row.text(),
                view.fallback()
            );
        }
    }

    /// **Anti-vacuous twin.** The agreement test is a containment check, so
    /// it would pass on a view that rendered NOTHING. This proves the view
    /// actually produced the rows whose content is being compared.
    #[test]
    fn the_agreement_check_is_over_a_view_that_rendered_something() {
        let view = permission();
        assert!(view.rows().len() >= 4, "{:?}", view.rows());
        assert_eq!(view.options().len(), 2);
        assert!(!view.fallback().is_empty());
    }

    /// **No renderer can reach this model, and the compiler is what says
    /// so.**
    ///
    /// The source scan is a belt; the braces are the dependency graph.
    /// `newt-core` declares no `ratatui` and no `crossterm`, so a widget
    /// type here is a compile error rather than a review catch — which is
    /// why this model moved down a crate rather than staying in `newt-tui`,
    /// where both are non-optional deps and only a scan was possible.
    #[test]
    fn the_view_model_names_no_renderer() {
        let code = production_code(include_str!("interaction_view.rs"));
        for renderer in ["ratatui", "crossterm", "Rect", "Frame", "Widget"] {
            assert!(
                !code.contains(renderer),
                "the view model names `{renderer}` — a renderer type in the \
                 model is exactly what C2's boundary forbids"
            );
        }
        assert!(code.contains("InteractionView"), "the scan read nothing");
    }

    /// **Anti-vacuous twin for the renderer scan.** Prose must not count as
    /// code, and a real reference must.
    #[test]
    fn the_renderer_scan_sees_code_and_ignores_prose() {
        let prose = production_code("//! ratatui is mentioned here\npub fn f() {}");
        assert!(!prose.contains("ratatui"), "prose counted as code");
        let real = production_code("use ratatui::layout::Rect;\npub fn f() {}");
        assert!(real.contains("ratatui"), "a real reference was missed");
        assert!(real.contains("Rect"), "a real type was missed");
    }

    /// The VIEW MODEL's own code, comment lines removed.
    ///
    /// Split at this test module rather than at any `#[cfg(test)]`: gating a
    /// test-only accessor put such an attribute EARLY in the file, so
    /// splitting on the attribute silently reduced the scan to the first ~60
    /// lines. The renderer check then passed over a fragment that contains no
    /// renderer because it contains almost nothing — caught only by the
    /// `contains("InteractionView")` assertion below, which is why that line
    /// is not decoration.
    ///
    /// Splitting here also correctly excludes the `terminal` half, which
    /// follows this module and legitimately names ratatui.
    fn production_code(source: &str) -> String {
        source
            .split("mod c2 {")
            .next()
            .unwrap_or(source)
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
