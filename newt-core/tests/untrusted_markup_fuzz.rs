//! **Fuzzing the untrusted markup boundary** (epic #1803, slice G1, #1934).
//!
//! Law 11: a definition may come from untrusted markup. Everything reached
//! here is therefore attacker-authored — front matter, the envelope, the
//! Markdown body, a Mermaid block, a table cell — and the properties below
//! are the ones that must hold for *every* input rather than for the inputs
//! someone thought to write down.
//!
//! `proptest` rather than `cargo-fuzz`: it is already this crate's fuzz tool
//! (see the dev-dependency's own comment, #1528 B2), it needs no nightly,
//! and it runs inside the per-PR gate instead of beside it. A property that
//! only runs weekly is a property nobody's PR is measured against.
//!
//! **No wall-clock assertions and no I/O.** Every function under test here
//! is pure.
//!
//! Each property carries an anti-vacuous twin: a property over a function
//! that returns `Err` for everything, or never rejects anything, is true and
//! worthless. The twins pin that the generator actually reaches both sides.

use newt_core::markup::{
    assemble_newt_metadata, extension::mermaid, plain, split_newt_metadata, strip_newt_metadata,
    table, EnvelopeError, FENCE,
};
use newt_interaction::{
    Control, ControlId, ControlKind, InteractionDefinition, InteractionKind, Requirement,
};
use proptest::prelude::*;

/// A definition whose author-controlled text is `markdown` and whose one
/// control is labelled `label`. Both are attacker-supplied.
fn definition(markdown: &str, label: &str) -> InteractionDefinition {
    InteractionDefinition::new(
        InteractionKind::Form,
        markdown,
        vec![Control {
            id: ControlId::new("field").expect("a fixed, valid id"),
            kind: ControlKind::Text,
            label: label.to_string(),
            requirement: Requirement::Optional,
        }],
    )
}

proptest! {
    // No failure-persistence file. The unit tier does no filesystem I/O
    // (CLAUDE.md, "Testing strategy"), and a test that writes into the
    // source tree when it fails is a test that fails twice on a read-only
    // CI checkout. proptest prints the failing seed as a `cc` line either
    // way, which is what actually reproduces the case.
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    /// Stripping is total and byte-preserving: it never panics, and a body
    /// it returns is a slice of the input it was given. A stripper that
    /// synthesized bytes could smuggle content past a caller that believed
    /// it was reading the document.
    #[test]
    fn stripping_any_document_returns_a_slice_of_it(text in "\\PC*") {
        if let Ok(body) = strip_newt_metadata(&text) {
            prop_assert!(
                text.contains(body),
                "the body is not a slice of the input"
            );
            prop_assert!(body.len() <= text.len());
        }
    }

    /// **The round trip, fuzzed.** Whatever `assemble` accepts, `strip`
    /// returns verbatim — for any front matter and any body. This is the
    /// unforgeable-marker property (#1848) stated over all inputs rather
    /// than over the handful a test author imagines: no body, however
    /// adversarial, can make the envelope give back something else.
    #[test]
    fn assembling_then_stripping_yields_the_body_exactly(
        front in "\\PC*", body in "\\PC*",
    ) {
        if let Ok(document) = assemble_newt_metadata(&front, &body) {
            let stripped = strip_newt_metadata(&document)
                .expect("a document this module assembled must split");
            prop_assert_eq!(stripped, body.as_str());
        }
    }

    /// A split names spans OF ITS INPUT, and a document with no envelope
    /// comes back byte-identical. The second half is the one that matters:
    /// a splitter that quietly normalized an unenveloped document would
    /// change bytes nobody asked it to touch.
    #[test]
    fn a_split_names_spans_of_its_input(text in "\\PC*") {
        if let Ok(split) = split_newt_metadata(&text) {
            prop_assert!(text.contains(split.body));
            match split.front_matter {
                None => prop_assert_eq!(
                    split.body, text.as_str(),
                    "an unenveloped document was not returned byte-identically"
                ),
                Some(front) => {
                    prop_assert!(text.contains(front));
                    prop_assert!(front.len() + split.body.len() <= text.len());
                }
            }
        }
    }

    /// Markdown parsing is total. The dialect has exactly one parser
    /// constructor, so this covers every document any Newt surface reads.
    #[cfg(feature = "markdown")]
    #[test]
    fn parsing_any_document_terminates(text in "\\PC*") {
        let events = newt_core::markup::dialect::parse(&text).count();
        prop_assert!(events <= text.len() * 4 + 8, "event count is not bounded by input size");
    }

    /// **The plain projection emits no escape sequence, for any input.**
    ///
    /// The plain-scroller contract is what the piped, headless, and wyvern
    /// paths depend on. Author-controlled text reaches this renderer
    /// directly, so "no ANSI" has to hold for text chosen to break it.
    #[test]
    fn the_plain_projection_never_emits_an_escape(
        markdown in "\\PC*", label in "\\PC*",
    ) {
        let rendered = plain::render(&definition(&markdown, &label));
        prop_assert!(!rendered.contains('\x1b'), "an escape reached plain output");
        prop_assert!(!rendered.contains('\r'), "a carriage return reached plain output");
        // Deterministic and pure, as its doc comment claims.
        prop_assert_eq!(&rendered, &plain::render(&definition(&markdown, &label)));
    }

    /// Measuring a diagram is total, and its shape is bounded by the source.
    ///
    /// **The node bound is `edges + lines`, not `lines`** (#1956). The old
    /// bound was never true: a line splits into at most one more endpoint
    /// than it has arrows, so `A->B` is one line and two legitimate nodes
    /// and would have failed it. It survived only because a random `\PC*`
    /// string rarely contains an arrow token — the fuzzer needed 45 tries to
    /// reach `"->"`, and would have found `"A->B"` just as damning.
    ///
    /// This bound does NOT subsume the pinned cases in
    /// `mermaid::e0a_shape_scan`. `"->"` under the OLD scanner reports 2
    /// nodes, 1 edge, 1 line — which satisfies `2 <= 1 + 1`. So correcting
    /// the property alone would have turned this red into a green while the
    /// phantom nodes stayed, and only the pinned case holds the scanner
    /// honest.
    #[test]
    fn measuring_any_mermaid_source_terminates(source in "\\PC*") {
        let shape = mermaid::measure(&source);
        prop_assert!(shape.depth <= source.len());
        prop_assert!(shape.nodes <= shape.edges + source.lines().count());
    }

    /// A table cell can hold anything, and the table stays a table: one
    /// header row, one delimiter row, one row per row — and no cell ever
    /// introduces an unescaped pipe, which is the single way a cell could
    /// forge a column boundary.
    #[test]
    fn no_table_cell_can_forge_a_column(
        cells in prop::collection::vec("\\PC*", 1..4),
    ) {
        let columns: Vec<table::Column> = (0..cells.len())
            .map(|i| table::Column::new(format!("c{i}")))
            .collect();
        let rendered = table::render_table(&columns, std::slice::from_ref(&cells));
        prop_assert_eq!(rendered.lines().count(), 3);
        for line in rendered.lines() {
            let unescaped = line
                .char_indices()
                .filter(|(i, c)| *c == '|' && (*i == 0 || !line[..*i].ends_with('\\')))
                .count();
            prop_assert_eq!(
                unescaped,
                cells.len() + 1,
                "a cell forged a column boundary in `{}`",
                line
            );
        }
    }
}

/// **Anti-vacuous twin for the envelope properties.**
///
/// `assembling_then_stripping_yields_the_body_exactly` is guarded by
/// `if let Ok(...)`, so an `assemble` that refused everything would satisfy
/// it silently. These pin both sides: the accepting path really produces a
/// document, and the refusing path really refuses.
#[test]
fn the_envelope_generator_reaches_both_outcomes() {
    let accepted = assemble_newt_metadata("kind = \"form\"", "body text")
        .expect("ordinary front matter and body assemble");
    assert_eq!(
        strip_newt_metadata(&accepted).expect("splits"),
        "body text",
        "the accepting path does not round-trip; the fuzz property is vacuous"
    );

    // A front matter carrying the closing fence would truncate the split.
    assert!(matches!(
        assemble_newt_metadata(FENCE, "body"),
        Err(EnvelopeError::FrontMatterContainsFence)
    ));
    // A body that opens its own envelope breaks strip idempotence.
    assert!(matches!(
        assemble_newt_metadata("a = 1", &format!("{FENCE}\nb = 2\n{FENCE}\nx")),
        Err(EnvelopeError::BodyOpensAnEnvelope)
    ));
}

/// **Anti-vacuous twin for the split property.**
///
/// Its `match` has two arms, and a generator that only ever reached one of
/// them would leave half the property untested. Both are reachable.
#[test]
fn a_split_reaches_both_the_enveloped_and_unenveloped_arms() {
    let bare = split_newt_metadata("just a body").expect("splits");
    assert_eq!(bare.front_matter, None);
    assert_eq!(bare.body, "just a body");

    let document = format!("{FENCE}\nkind = \"form\"\n{FENCE}\nbody");
    let enveloped = split_newt_metadata(&document).expect("splits");
    assert_eq!(enveloped.front_matter, Some("kind = \"form\"\n"));
    assert_eq!(enveloped.body, "body");
}

/// **Anti-vacuous twin for the no-escape property.**
///
/// `the_plain_projection_never_emits_an_escape` would hold trivially over a
/// renderer that returned the empty string. It does not: the author's text
/// and the control's label both reach the output.
#[test]
fn the_plain_projection_actually_renders_its_input() {
    let rendered = plain::render(&definition("a decision", "a field"));
    assert!(
        rendered.contains("a decision"),
        "the markdown did not render"
    );
    assert!(rendered.contains("a field"), "the label did not render");
}

/// **Anti-vacuous twin for the table property.**
///
/// The pipe count is only meaningful if a cell containing a pipe is
/// actually escaped rather than rejected or dropped.
#[test]
fn a_pipe_in_a_cell_is_escaped_and_kept() {
    let rendered = table::render_table(&[table::Column::new("c0")], &[vec!["a|b".to_string()]]);
    assert!(
        rendered.contains("a\\|b"),
        "a pipe was not escaped: {rendered}"
    );
}
