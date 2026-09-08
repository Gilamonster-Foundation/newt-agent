//! Tests for the launch configuration.
//!
//! Fully mocked by construction — this crate has no dependencies, touches no
//! filesystem, and reads no environment, so the unit tier covers it completely.

use super::*;

// --- the exclusion, which is the whole point of the crate -------------------

/// Both modes at once is refused at the seam where a `Continuity` is built.
///
/// This is the rule `clap`'s `conflicts_with` gives the CLI for free and gives a
/// pyO3 caller not at all. It is asserted here so both consumers inherit it.
#[test]
fn both_modes_at_once_is_refused() {
    let err = LaunchConfig::from_flags(true, true, None).unwrap_err();
    assert_eq!(err, LaunchError::ConflictingContinuity);
}

/// The conflict is unrepresentable once a `Continuity` exists — there is no
/// variant carrying both, and no `Hermetic { from }`. This test documents that
/// the type is doing the work, so a future refactor that adds such a variant
/// fails here rather than silently widening what is legal.
#[test]
fn continuity_cannot_express_both_modes() {
    assert!(!Continuity::Hermetic.is_resumable());
    assert!(Continuity::Resume { from: None }.is_resumable());
    assert_eq!(Continuity::Hermetic.parent_frame(), None);
}

/// Naming a frame while hermetic is a contradiction, not a precedence question.
/// It is refused rather than resolved by dropping one side.
#[test]
fn a_frame_named_under_hermetic_is_refused_not_ignored() {
    let err = LaunchConfig::from_flags(true, false, Some("bafy123".into())).unwrap_err();
    assert_eq!(
        err,
        LaunchError::ResumeFromUnderHermetic {
            frame: "bafy123".into()
        }
    );
}

// --- defaults ---------------------------------------------------------------

/// The reproducible mode is the default: a run inherits nothing unless asked.
/// Resumption is opted into, never fallen into.
#[test]
fn the_default_is_hermetic() {
    let cfg = LaunchConfig::default();
    assert_eq!(cfg.continuity, Continuity::Hermetic);
    assert!(!cfg.continuity.is_resumable());
    cfg.validate().expect("the default must be legal");
}

/// Naming a frame implies resuming, without also passing the flag. A caller who
/// says where to continue from has said enough.
#[test]
fn a_named_frame_implies_resume() {
    let cfg = LaunchConfig::from_flags(false, false, Some("bafy999".into())).unwrap();
    assert!(cfg.continuity.is_resumable());
    assert_eq!(cfg.continuity.parent_frame(), Some("bafy999"));
}

/// A resumable run with no parent is legal and is the genesis of a chain — not
/// the same as hermetic, though both have no parent frame.
#[test]
fn resume_without_a_parent_is_a_genesis_frame_not_hermetic() {
    let cfg = LaunchConfig::from_flags(false, true, None).unwrap();
    assert!(cfg.continuity.is_resumable());
    assert_eq!(cfg.continuity.parent_frame(), None);
    assert_ne!(cfg.continuity, Continuity::Hermetic);
}

// --- validation -------------------------------------------------------------

/// An empty frame id is a caller error, not an implicit genesis. Silently
/// treating `--resume-from ""` as "start fresh" would lose a typo.
#[test]
fn an_empty_frame_id_is_refused_rather_than_treated_as_genesis() {
    for blank in ["", "   ", "\t"] {
        let cfg = LaunchConfig {
            continuity: Continuity::Resume {
                from: Some(blank.into()),
            },
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            LaunchError::EmptyResumeFrom,
            "blank {blank:?} must be refused"
        );
    }
}

// --- the record -------------------------------------------------------------

/// The description must let a reader tell whether a cap exit under this run was
/// a failure or a pause, without reading the invocation.
#[test]
fn describe_states_what_a_cap_exit_would_mean() {
    let hermetic = LaunchConfig::default();
    let d = hermetic.describe();
    assert!(d.contains("hermetic"), "{d}");
    assert!(d.contains("failure"), "{d}");

    let chained = LaunchConfig::from_flags(false, false, Some("bafyabc".into())).unwrap();
    let d = chained.describe();
    assert!(d.contains("bafyabc"), "{d}");
    assert!(
        !d.contains("failure"),
        "a resumable run's cap exit is not a failure: {d}"
    );
}

/// Every error renders a message that says what to do, not merely what is wrong.
/// A validation that fails startup is read by someone who is now blocked.
#[test]
fn every_error_message_names_a_remedy() {
    let cases = [
        LaunchError::ConflictingContinuity,
        LaunchError::ResumeFromUnderHermetic {
            frame: "bafy1".into(),
        },
        LaunchError::EmptyResumeFrom,
    ];
    for e in &cases {
        let msg = e.to_string();
        assert!(msg.len() > 40, "too terse to act on: {msg}");
        assert!(
            msg.contains("--") || msg.contains("must"),
            "names no flag and no requirement: {msg}"
        );
    }
}

/// The mode's wire spelling is stable, so a record and a report name it the same
/// way. Mirrors the reasoning behind `Outcome::as_str` in gilamonster-bench.
#[test]
fn mode_names_are_stable_wire_spellings() {
    assert_eq!(Continuity::Hermetic.as_str(), "hermetic");
    assert_eq!(Continuity::Resume { from: None }.as_str(), "resume");
    assert_eq!(
        Continuity::Resume {
            from: Some("x".into())
        }
        .as_str(),
        "resume"
    );
}
