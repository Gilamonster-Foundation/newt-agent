//! **The wizard's credential path, and the secret-by-reference contract**
//! (D1b, #1892).
//!
//! Every secret `newt setup` asks for arrives here: the four hidden prompts
//! (two API keys on the custom-host path, one on the preset path, one
//! passphrase) and the records they produce. The contract this module exists
//! to hold is one sentence — **a secret is a value only in transit, and never
//! in anything durable.**
//!
//! ## Where a secret is allowed to be
//!
//! | Place | Carries |
//! |---|---|
//! | the [`InteractionDefinition`] shown to the operator | nothing — `ControlKind::Secret` is a unit variant |
//! | what the surface renders | the LABEL plus `(secret, not echoed)` |
//! | the terminal, while typing | one `*` per character ([`newt_core::tty::Echo::Stars`]) |
//! | the [`Response`] record | `ControlValue::Secret { reference }` — a handle |
//! | the config written to disk | `api_key_env` or `api_key_file` — a reference |
//! | memory, briefly | [`Secret`], whose `Debug` redacts and which is not `Serialize` |
//!
//! The plaintext exists exactly once, between the operator's keystrokes and
//! the age-encrypted file, and it is wrapped the whole time. It is never a
//! `String` field of a struct this module hands to anyone.
//!
//! ## Two `SecretRef` types, and why this uses both
//!
//! The workspace has two, and conflating them would have been the third:
//!
//! * [`newt_interaction::SecretRef`] — the PROTOCOL's opaque handle, what a
//!   `ControlValue::Secret` carries on the wire.
//! * `newt_core::SecretRef` — the EXISTING `{env|file|cmd}` scheme with
//!   `resolve()`, which `newt-core/src/mcp.rs` calls "the existing `SecretRef`
//!   secret-by-reference scheme".
//!
//! No vault was invented for this slice, because one already exists and the
//! wizard already produced its `file` form: `collapse_home(&path)` over the
//! age-encrypted token, recorded as `api_key_file`. The protocol handle IS
//! that reference. A fresh handle scheme would have been a third mechanism
//! for one idea.

use newt_core::agent_identity::Secret;
use newt_interaction::{
    Control, ControlId, ControlKind, ControlValue, InteractionDefinition, InteractionKind,
    Requirement, SecretRef,
};

/// The control id every secret prompt answers.
pub(crate) const SECRET_CONTROL: &str = "secret";

/// One hidden prompt, as a definition.
///
/// Three things this carries, none of them decoration:
///
/// * `ControlKind::Secret` — a unit variant. The definition **cannot** hold a
///   value, so no renderer can print one; `markup::plain` and
///   `interaction_view` both project it as `{label}: (secret, not echoed)`.
/// * `Requirement::Required` on the control. Together with the kind, that is
///   what makes a surface which cannot take hidden input REFUSE rather than
///   render the key as ordinary text: `plan_presentation` derives the
///   `secret-input` demand from a required `Secret` control, and
///   `markup::headless` fails closed on it (ADR law 5's required arm).
/// * The label on the CONTROL, not in the body. `markup::plain` gives a
///   labelled control its own line, so the label is what carries the
///   `(secret, not echoed)` marker to the operator.
///
/// **No hand-written `FeatureDemand`.** An explicit `secret-input` demand was
/// written here first and then deleted: `plan_presentation` already derives
/// it from the kind, so the two were one fact stated twice — and a red-run
/// that downgraded the explicit demand to `Optional` while leaving the kind
/// alone changed no behaviour and failed no test, which is precisely how two
/// statements of one rule drift apart. The kind is the statement; the mapping
/// from kind to capability belongs to the one place that owns it.
pub(crate) fn secret_prompt(label: &str) -> InteractionDefinition {
    InteractionDefinition::new(
        InteractionKind::Prompt,
        String::new(),
        vec![Control {
            id: ControlId::new(SECRET_CONTROL)
                .expect("`secret` is a valid control id; it is a const"),
            kind: ControlKind::Secret,
            label: label.to_string(),
            requirement: Requirement::Required,
        }],
    )
}

/// Ask for one secret, wrapped the moment it arrives.
///
/// Returns [`Secret`], never `String`. The plaintext is a bare `String` for
/// exactly the width of the `Console::ask_secret` call — a person has to type
/// it — and from here on it lives in a type whose `Debug` redacts and which
/// is deliberately not `Serialize`, so it cannot round-trip into a config
/// file or a log line by accident.
///
/// The console is the parameter the wizard already threads; what changed is
/// that its `ask_secret` now builds [`secret_prompt`] and reads through the
/// one terminal adapter, so the masking follows the DEFINITION rather than
/// the call site. Threading the raw seam here as well would mean two
/// parameters saying the same thing until D1b-2/3 retire the console.
///
/// `Ok(None)` is a deliberate skip — an empty answer. Every caller already
/// branches on "no key given", and collapsing "" into `None` here is what
/// keeps an empty `Secret` from being constructible downstream.
///
/// # Errors
///
/// Propagates the read failure: cancelled, EOF, or no operator.
pub(super) fn ask_secret(
    console: &mut dyn crate::line_console::Console,
    label: &str,
) -> std::io::Result<Option<Secret>> {
    let typed = console.ask_secret(label)?;
    let typed = typed.trim();
    Ok((!typed.is_empty()).then(|| Secret::new(typed)))
}

/// **A stored secret, as the only two things anyone may see of it.**
///
/// One value, two views that cannot drift: the `ControlValue` a record
/// carries, and the handle a config field writes. Both come from the same
/// [`SecretRef`], so a `backends/*.toml` can never name a different secret
/// than the response does.
///
/// Making it a type rather than a `String` buys two guarantees the loose form
/// did not have. `SecretRef::new` rejects the empty handle, so
/// `api_key_file: Some("")` — a reference that references nothing — stops
/// being constructible. And the only constructor takes a REFERENCE, so there
/// is no way to build one of these around a plaintext key: the type that
/// travels is structurally incapable of carrying the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SealedSecret(ControlValue);

impl SealedSecret {
    /// Seal a stored secret behind the handle the host can resolve.
    ///
    /// `reference` is the wizard's EXISTING `api_key_file` form —
    /// `collapse_home` over the age-encrypted token — which is also
    /// `newt_core::SecretRef`'s `file` source. No new handle scheme: the
    /// protocol's opaque handle and the one the config already used are the
    /// same string, which is why this slice needed no vault.
    ///
    /// # Errors
    ///
    /// [`newt_interaction::ProtocolError`] when `reference` is empty.
    pub(super) fn new(reference: &str) -> Result<Self, newt_interaction::ProtocolError> {
        Ok(Self(ControlValue::Secret {
            reference: SecretRef::new(reference)?,
        }))
    }

    /// The handle, for the config field that records where the key lives.
    pub(super) fn handle(&self) -> &str {
        // Read through `submitted()` rather than the field, so the handle the
        // config records is DERIVED from the value the record carries. That
        // is the "cannot drift" claim above, implemented rather than
        // asserted: there is no path that writes one without the other.
        match self.submitted() {
            ControlValue::Secret { reference } => reference.as_str(),
            // Unreachable by construction — the only constructor builds the
            // `Secret` variant. Matched exhaustively rather than with `_` so
            // that adding a plaintext-carrying variant to `ControlValue`
            // breaks HERE, which is the whole reason that enum is not
            // `#[non_exhaustive]`.
            ControlValue::Choice { .. }
            | ControlValue::Text { .. }
            | ControlValue::Toggle { .. } => {
                unreachable!("SealedSecret only ever holds ControlValue::Secret")
            }
        }
    }

    /// What a response submits for this control: the handle, never the bytes.
    ///
    /// A `Response` is content-addressed and durable, which is exactly what
    /// makes a secret inside one a permanent disclosure. `ControlValue` has
    /// no plaintext-carrying variant, so "this record cannot contain the key"
    /// is provable by exhaustive match rather than by review.
    pub(super) fn submitted(&self) -> &ControlValue {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// The contract: plant a secret, prove it does not land
// ---------------------------------------------------------------------------

#[cfg(test)]
mod contract {
    use super::{ask_secret, secret_prompt, SealedSecret, SECRET_CONTROL};
    use newt_core::agent_identity::Secret;
    use newt_core::markup::{headless, plain};
    use newt_core::tty::Echo;
    use newt_interaction::{
        ControlId, ControlKind, ControlValue, ProtocolError, Requirement, Submission,
        SurfaceFeature,
    };

    /// **The planted secret.** Distinctive enough that a substring hit cannot
    /// be a coincidence, and shaped like a real key so nothing along the path
    /// treats it as a special case.
    const PLANTED: &str = "sk-live-PLANTED-DO-NOT-LEAK-4f9a2c";

    /// Every place this slice can put bytes in front of a human or into a
    /// record. A leak in ANY of them is the defect.
    fn surfaces_for(label: &str, sealed: &SealedSecret) -> Vec<(&'static str, String)> {
        let definition = secret_prompt(label);
        let submission = Submission {
            control: ControlId::new(SECRET_CONTROL).expect("valid"),
            value: sealed.submitted().clone(),
        };
        vec![
            ("the plain projection", plain::render(&definition)),
            (
                "the definition record",
                serde_json::to_string(&definition).expect("a definition serializes"),
            ),
            (
                "the submitted record",
                serde_json::to_string(&submission).expect("a submission serializes"),
            ),
            ("the terminal while typing", Echo::Stars.display(PLANTED)),
            ("a debug/log line", format!("{:?}", Secret::new(PLANTED))),
        ]
    }

    /// **Plant a secret and prove the path refuses it.**
    ///
    /// The label is deliberately the prompt text the wizard really uses, and
    /// the reference is the real `api_key_file` shape, so nothing here is a
    /// stand-in for the production path.
    #[test]
    fn a_planted_secret_reaches_no_surface_and_no_record() {
        let sealed = SealedSecret::new("~/.newt/backends/openai.token.abc123.age")
            .expect("a non-empty reference");
        for (where_, rendered) in surfaces_for("API key (echoes as *, Enter to skip)", &sealed) {
            assert!(
                !rendered.contains(PLANTED),
                "the planted secret reached {where_}: {rendered}"
            );
        }
    }

    /// **The anti-vacuous twin, and the one that matters most.**
    ///
    /// Every assertion above is a `!contains`, which is exactly the shape that
    /// passes when the haystack is empty, when the needle is misspelled, or
    /// when the serializer silently produced nothing. So: put the secret
    /// somewhere it genuinely could go — `ControlValue::Text`, the variant a
    /// careless migration would have reached for — and prove the SAME
    /// detector, over the SAME serializer, finds it.
    ///
    /// If this test ever fails, the test above is worthless.
    #[test]
    fn the_detector_finds_a_secret_that_is_actually_there() {
        let careless = Submission {
            control: ControlId::new(SECRET_CONTROL).expect("valid"),
            value: ControlValue::Text {
                text: PLANTED.to_string(),
            },
        };
        let rendered = serde_json::to_string(&careless).expect("serializes");
        assert!(
            rendered.contains(PLANTED),
            "the detector cannot see a plaintext secret in a record it IS in: {rendered}"
        );
        // ...and the same needle in a rendered form.
        assert!(Echo::Chars.display(PLANTED).contains(PLANTED));
        // The haystacks in the test above are non-empty, so `!contains` there
        // is a real negative rather than a vacuous one.
        let sealed = SealedSecret::new("ref").expect("valid");
        for (where_, rendered) in surfaces_for("API key", &sealed) {
            assert!(!rendered.is_empty(), "{where_} rendered nothing at all");
        }
    }

    /// The operator sees the field and its marker — never a value, because
    /// the definition has nowhere to put one.
    #[test]
    fn the_prompt_shows_the_field_and_says_it_is_not_echoed() {
        let shown = plain::render(&secret_prompt("API key"));
        assert_eq!(shown, "API key: (secret, not echoed)");
    }

    /// A surface that cannot take hidden input REFUSES rather than rendering
    /// the key as ordinary text. That is ADR law 5's required arm, and it is
    /// why the demand is `Required` rather than `Optional`.
    #[test]
    fn a_surface_that_cannot_hide_input_refuses_the_prompt() {
        let definition = secret_prompt("API key");
        let err = headless::present(&definition, &[]).expect_err("must fail closed");
        assert!(
            matches!(err, ProtocolError::UnsupportedFeature { ref feature, .. }
                     if feature == SurfaceFeature::SECRET_INPUT),
            "unexpected refusal: {err:?}"
        );
        // The twin: a capable surface DOES present it, so the refusal above
        // is conditional rather than a prompt that never works anywhere.
        let capable = [SurfaceFeature::new(SurfaceFeature::SECRET_INPUT).expect("valid")];
        let ok = headless::present(&definition, &capable).expect("a capable surface presents it");
        assert_eq!(ok.text(), "API key: (secret, not echoed)");
    }

    /// The sealed value carries a handle, and cannot be built around one that
    /// references nothing.
    #[test]
    fn a_sealed_secret_is_a_handle_and_never_empty() {
        let sealed = SealedSecret::new("~/.newt/backends/x.token.age").expect("valid");
        assert_eq!(sealed.handle(), "~/.newt/backends/x.token.age");
        assert!(
            matches!(sealed.submitted(), ControlValue::Secret { .. }),
            "the submitted value is a reference: {:?}",
            sealed.submitted()
        );
        // `api_key_file: Some("")` — a reference that references nothing —
        // is not constructible.
        assert!(matches!(
            SealedSecret::new(""),
            Err(ProtocolError::InvalidId { .. })
        ));
    }

    /// An empty answer is a SKIP, not an empty secret: the wizard's "Enter to
    /// skip" affordance, and what keeps a zero-length key from being sealed
    /// and written as if it were real.
    #[test]
    fn an_empty_answer_is_a_skip_not_an_empty_secret() {
        struct Scripted(&'static str);
        impl crate::line_console::Console for Scripted {
            fn ask(&mut self, _: &str) -> std::io::Result<String> {
                Ok(self.0.to_string())
            }
            fn say(&mut self, _: &str) {}
        }
        assert!(ask_secret(&mut Scripted(""), "API key").unwrap().is_none());
        assert!(ask_secret(&mut Scripted("   "), "API key")
            .unwrap()
            .is_none());
        let got = ask_secret(&mut Scripted(PLANTED), "API key")
            .unwrap()
            .expect("a real key is returned");
        assert_eq!(got.expose(), PLANTED, "and it is returned intact");
    }

    /// The control is `Secret`, so a renderer has no value to reach for. This
    /// pins the KIND rather than the rendering, because the rendering is only
    /// safe as a consequence of the kind.
    #[test]
    fn the_control_kind_is_secret() {
        let definition = secret_prompt("API key");
        assert!(matches!(definition.controls[0].kind, ControlKind::Secret));
        assert_eq!(definition.controls[0].requirement, Requirement::Required);
    }
}

#[cfg(test)]
mod no_duplicate_demand {
    use super::secret_prompt;

    /// **The `secret-input` demand is DERIVED, and stays derived.**
    ///
    /// A hand-written `FeatureDemand` here was removed after a red run showed
    /// it was inert: downgrading it to `Optional` while leaving
    /// `ControlKind::Secret` alone changed no behaviour and failed no test,
    /// because `plan_presentation` derives the demand from the kind. Two
    /// statements of one rule, one of which nothing reads.
    ///
    /// This pins the deletion. If a demand list comes back, either it is
    /// still inert — dead weight that can silently contradict the kind — or
    /// it is not, and then there are two rules for one fact again.
    #[test]
    fn the_prompt_hand_writes_no_feature_demand() {
        assert!(
            secret_prompt("API key").features.is_empty(),
            "the capability follows from ControlKind::Secret; restating it \
             here is the drift a red run already demonstrated"
        );
    }
}
