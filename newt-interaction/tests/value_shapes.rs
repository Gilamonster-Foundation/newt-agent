//! **A kind says what a well-formed answer IS, once, for every surface.**
//!
//! The vocabulary grew from four kinds — choice, text, toggle, secret — to
//! cover the value shapes an operator actually has to type: a bounded number,
//! a scale, a date, a colour, a path. The rule that decided which of HTML's
//! twenty-two input types earned a variant is stated on [`ControlKind`]: a
//! kind is a MEANING, an affordance is not. `#c0ffee` is a colour and not a
//! date, and no surface may accept it as one; a slider and a typed number
//! accept the same answers, so the slider is not a kind.
//!
//! These tests pin that rule's consequences — the validation, the hint every
//! surface renders, and the two properties that make the widening safe:
//! **it is additive on the wire** (no existing definition re-addresses), and
//! **a malformed value is a distinct refusal** from a wrong-typed one.

use newt_interaction::{
    Control, ControlId, ControlKind, InteractionDefinition, InteractionKind, PathKind, Requirement,
    TemporalPrecision,
};

fn number(min: Option<i64>, max: Option<i64>, step: Option<i64>) -> ControlKind {
    ControlKind::Number { min, max, step }
}

/// A number is checked against its own bounds and its own grid.
#[test]
fn a_number_is_bounded_and_stepped() {
    let capped = number(Some(1), Some(10_000), None);
    assert!(capped.check_text("1").is_ok());
    assert!(capped.check_text("10000").is_ok());
    assert!(capped.check_text(" 42 ").is_ok(), "surrounding space trims");
    assert!(capped.check_text("0").is_err(), "below the floor");
    assert!(capped.check_text("10001").is_err(), "above the ceiling");
    assert!(capped.check_text("auto").is_err(), "not a number at all");
    assert!(capped.check_text("").is_err());
    assert!(capped.check_text("4.5").is_err(), "integers only");

    // The grid is counted from `min`, so an odd floor keeps odd values.
    let stepped = number(Some(1), Some(9), Some(2));
    for on_grid in ["1", "3", "9"] {
        assert!(stepped.check_text(on_grid).is_ok(), "{on_grid} is on grid");
    }
    assert!(stepped.check_text("2").is_err(), "2 is off a from-1 grid");

    // Unbounded is still a number, and says so.
    let free = number(None, None, None);
    assert!(free.check_text("-9000").is_ok());
    assert!(free.check_text("nine").is_err());
}

/// The refusal says what was expected, in words a person can act on.
#[test]
fn a_refusal_states_the_expectation() {
    assert_eq!(
        number(Some(1), Some(10), None).check_text("x"),
        Err("an integer in 1..=10".to_string())
    );
    assert_eq!(
        number(Some(2), None, Some(2)).check_text("x"),
        Err("an integer 2 or greater, in steps of 2".to_string())
    );
    assert_eq!(
        number(None, Some(9), None).check_text("x"),
        Err("an integer 9 or less".to_string())
    );
    assert_eq!(
        ControlKind::Color.check_text("blue"),
        Err("#rrggbb".to_string())
    );
    assert_eq!(
        ControlKind::Temporal {
            precision: TemporalPrecision::Week
        }
        .check_text("nope"),
        Err("YYYY-Www".to_string())
    );
}

/// A range's bounds are mandatory, so its check needs no `Option` arms.
#[test]
fn a_range_accepts_only_its_own_scale() {
    let volume = ControlKind::Range {
        min: 0,
        max: 100,
        step: 5,
    };
    assert!(volume.check_text("0").is_ok());
    assert!(volume.check_text("100").is_ok());
    assert!(volume.check_text("55").is_ok());
    assert!(volume.check_text("101").is_err());
    assert!(volume.check_text("-5").is_err());
    assert!(volume.check_text("7").is_err(), "off the step grid");
}

/// A hostile definition must not be able to panic or deny-all a parser.
///
/// A `step` of zero would divide by zero, and a `step` that refused every
/// value would let whoever authored the definition deny the operator their
/// own setting. Both are shrugged off: the grid is simply not applied.
#[test]
fn a_degenerate_step_is_ignored_rather_than_fatal() {
    for step in [Some(0), Some(-3), Some(1)] {
        let kind = number(Some(0), Some(10), step);
        assert!(kind.check_text("7").is_ok(), "step {step:?} accepts 7");
    }
    // And the grid arithmetic holds at the extremes, where `value - min`
    // overflows an `i64`. The verdicts below are the MATHEMATICALLY correct
    // ones, which is the point: an implementation that merely avoided the
    // panic by refusing what it could not compute would fail the third
    // assertion, having called an in-range, on-grid value malformed.
    //
    // `i64::MAX - i64::MIN` is `2^64 - 1`: odd, so off a step-2 grid, and
    // divisible by 3, so on a step-3 one.
    let by_two = number(Some(i64::MIN), Some(i64::MAX), Some(2));
    assert!(by_two.check_text(&i64::MIN.to_string()).is_ok(), "offset 0");
    assert!(
        by_two.check_text(&i64::MAX.to_string()).is_err(),
        "odd offset"
    );
    let by_three = number(Some(i64::MIN), Some(i64::MAX), Some(3));
    assert!(
        by_three.check_text(&i64::MAX.to_string()).is_ok(),
        "2^64-1 is divisible by 3: in range and on grid, so not malformed"
    );
}

/// Each precision accepts its own shape and refuses the others'.
#[test]
fn a_temporal_control_accepts_its_own_precision() {
    use TemporalPrecision::{Date, DateTime, Month, Time, Week};
    let cases: &[(TemporalPrecision, &[&str], &[&str])] = &[
        (
            Date,
            &["2026-09-01", "2026-12-31"],
            &["2026-09", "2026-13-01", "26-09-01", "2026-09-01T10:00"],
        ),
        (
            Time,
            &["09:30", "23:59:59"],
            &["24:00", "9:30", "09:60", "09"],
        ),
        (
            DateTime,
            &["2026-09-01T09:30", "2026-09-01T09:30:15"],
            &["2026-09-01", "09:30", "2026-09-01 09:30"],
        ),
        (Month, &["2026-09"], &["2026-09-01", "2026-13", "2026"]),
        (Week, &["2026-W01", "2026-W53"], &["2026-W54", "2026-01"]),
    ];
    for (precision, good, bad) in cases {
        let kind = ControlKind::Temporal {
            precision: *precision,
        };
        for text in *good {
            assert!(
                kind.check_text(text).is_ok(),
                "{precision:?} accepts {text}"
            );
        }
        for text in *bad {
            assert!(
                kind.check_text(text).is_err(),
                "{precision:?} must refuse {text}"
            );
        }
    }
}

/// Shape, not calendar truth — stated because the boundary is easy to
/// mistake for a bug. A February 31st is a real date-shaped string; whether
/// it exists is the host's question, and answering it here would put a
/// calendar (and a leap-second argument) in a wire vocabulary.
#[test]
fn a_temporal_check_is_a_shape_not_a_calendar() {
    let date = ControlKind::Temporal {
        precision: TemporalPrecision::Date,
    };
    assert!(date.check_text("2026-02-31").is_ok());
    assert!(
        date.check_text("2026-00-01").is_err(),
        "month 0 is no shape"
    );
}

#[test]
fn a_colour_is_six_hex_digits_behind_a_hash() {
    for good in ["#c0ffee", "#C0FFEE", "#000000", "#ffffff"] {
        assert!(ControlKind::Color.check_text(good).is_ok(), "{good}");
    }
    for bad in ["c0ffee", "#c0ffe", "#c0ffeee", "#gggggg", "#", ""] {
        assert!(ControlKind::Color.check_text(bad).is_err(), "{bad}");
    }
}

/// A path is NAMED here; whether it exists is the host's business — an
/// existence check is racy by nature and would make a definition's validity
/// depend on the machine reading it.
#[test]
fn a_path_is_named_never_resolved() {
    let kind = ControlKind::Path {
        kind: PathKind::File,
        accept: vec![".toml".to_string()],
    };
    assert!(kind.check_text("/no/such/file/anywhere").is_ok());
    assert!(kind.check_text("relative.toml").is_ok());
    assert!(kind.check_text("   ").is_err(), "nothing was named");
}

/// The three kinds that do not travel as text answer `Ok` rather than
/// pretending to judge a value they never receive. A choice is resolved by
/// `binding::resolve_typed` — canonical-first, aliases second, ambiguity
/// denies — and a second, weaker check here is the duplicate that rule
/// exists to prevent.
#[test]
fn the_non_text_kinds_defer_rather_than_guess() {
    for kind in [
        ControlKind::Toggle,
        ControlKind::Secret,
        ControlKind::Choice {
            options: Vec::new(),
        },
    ] {
        assert!(kind.check_text("anything at all").is_ok());
        assert!(!kind.travels_as_text(), "{kind:?} has its own value shape");
    }
    for kind in [
        ControlKind::Text,
        ControlKind::Color,
        number(None, None, None),
    ] {
        assert!(kind.travels_as_text(), "{kind:?} is answered as text");
    }
}

/// **One hint table, rendered by every surface.**
///
/// The plain projection and the RichTUI view model each carried this list
/// before; two copies is how a kind renders as a bare label on one surface
/// only. The pre-existing strings are exact, because the plain projection is
/// a byte-identity contract — `[y/n]`, never `[y/N]`, since a rendered
/// default is how a headless surface chooses one by accident.
#[test]
fn every_kind_advertises_its_shape() {
    assert_eq!(ControlKind::Text.hint(), "");
    assert_eq!(ControlKind::Toggle.hint(), " [y/n]");
    assert_eq!(ControlKind::Secret.hint(), " (secret, not echoed)");
    assert_eq!(
        ControlKind::Choice {
            options: Vec::new()
        }
        .hint(),
        "",
        "a choice renders its options as lines, not as a suffix"
    );
    assert_eq!(number(None, None, None).hint(), " [number]");
    assert_eq!(
        number(Some(1), Some(10), None).hint(),
        " [an integer in 1..=10]"
    );
    assert_eq!(
        ControlKind::Range {
            min: 0,
            max: 100,
            step: 5
        }
        .hint(),
        " [0..=100]"
    );
    assert_eq!(
        ControlKind::Temporal {
            precision: TemporalPrecision::DateTime
        }
        .hint(),
        " [YYYY-MM-DDTHH:MM[:SS]]"
    );
    assert_eq!(ControlKind::Color.hint(), " [#rrggbb]");
    assert_eq!(
        ControlKind::Path {
            kind: PathKind::Directory,
            accept: Vec::new()
        }
        .hint(),
        " [directory path]"
    );
}

/// **The wire names are pinned**, so a rename is a visible change rather
/// than a silent one — the same guard the existing vocabularies carry.
#[test]
fn the_wire_vocabulary_is_pinned() {
    let json = |kind: &ControlKind| serde_json::to_string(kind).expect("serializes");
    assert_eq!(json(&ControlKind::Text), "\"text\"");
    assert_eq!(json(&ControlKind::Color), "\"color\"");
    assert_eq!(
        json(&number(Some(1), None, None)),
        r#"{"number":{"min":1}}"#,
        "absent bounds are omitted, not written as null"
    );
    assert_eq!(
        json(&ControlKind::Temporal {
            precision: TemporalPrecision::DateTime
        }),
        r#"{"temporal":{"precision":"date-time"}}"#
    );
    assert_eq!(
        json(&ControlKind::Path {
            kind: PathKind::Directory,
            accept: Vec::new()
        }),
        r#"{"path":{"kind":"directory"}}"#,
        "an empty accept list is omitted"
    );
}

/// **The widening is ADDITIVE: no definition written before it re-addresses.**
///
/// This is the property that let the vocabulary grow at all. `Text` stayed a
/// unit variant for exactly this reason — as `Text { format }` it would encode
/// as a dag-cbor map instead of the string `"text"`, changing the id of every
/// definition that has ever carried a text field, and breaking the frozen
/// vectors in `tests/data/` that are DECODED rather than rebuilt.
///
/// `tests/vectors.rs` and `tests/external_consumer.rs` prove it end to end
/// against those bytes. This proves the mechanism directly, so a future
/// widening that reshapes an existing variant fails HERE, with the reason
/// attached, instead of in a corpus test whose message is a cbor major-type
/// mismatch.
#[test]
fn the_pre_existing_kinds_still_encode_as_bare_tags() {
    for (kind, tag) in [
        (ControlKind::Text, "\"text\""),
        (ControlKind::Toggle, "\"toggle\""),
        (ControlKind::Secret, "\"secret\""),
    ] {
        assert_eq!(
            serde_json::to_string(&kind).expect("serializes"),
            tag,
            "{kind:?} must stay a bare tag: a struct variant re-addresses every \
             definition that carries one"
        );
    }
}

/// A definition carrying the new kinds is still a definition: it addresses,
/// and it round-trips through the canonical form.
#[test]
fn a_definition_of_new_kinds_round_trips() {
    let control = |id: &str, kind: ControlKind| Control {
        id: ControlId::new(id).expect("valid control id"),
        kind,
        label: id.to_string(),
        requirement: Requirement::Optional,
    };
    let definition = InteractionDefinition::new(
        InteractionKind::Form,
        "settings",
        vec![
            control("rounds", number(Some(1), Some(10_000), None)),
            control(
                "volume",
                ControlKind::Range {
                    min: 0,
                    max: 100,
                    step: 5,
                },
            ),
            control(
                "when",
                ControlKind::Temporal {
                    precision: TemporalPrecision::Date,
                },
            ),
            control("accent", ControlKind::Color),
            control(
                "config",
                ControlKind::Path {
                    kind: PathKind::File,
                    accept: vec![".toml".to_string()],
                },
            ),
        ],
    );
    let json = serde_json::to_string(&definition).expect("serializes");
    let back: InteractionDefinition = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back, definition);
    assert_eq!(back.controls.len(), 5);
}
