//! Multi-contributor commit attribution ledger (#1707/#1709 family, "the
//! contract").
//!
//! A single interactive session can hand material work to more than one
//! model, harness, or crew delegate between commits — a `/model` switch, a
//! `/backend` repoint, a loadout swap, a crew leaf landing its own edits.
//! Each of those is a genuine contributor to the work the NEXT commit
//! represents, and none of them may silently displace an earlier one.
//!
//! [`AttributionLedger`] is the pending-contributor set for one commit: an
//! ordered, deduplicated collection of [`Attribution`] identities, `add`ed to
//! as work lands and `clear`ed only once a commit built from it actually
//! succeeds. It is deliberately NOT `Option<Attribution>` — there is no
//! maximum contributor count, and a later contributor never overwrites an
//! earlier one.
//!
//! Identity is `(model, harness, email)` — never self-reported by the model.
//! Callers resolve `model`/`harness` from authoritative runtime state (the
//! active backend/loadout, the actual execution harness) and pass them to
//! [`AttributionLedger::record`]; `email` comes from the ledger's configured
//! default (ordinarily [`crate::agent_identity::AgentIdentity::email`]).

use std::collections::HashSet;

/// One AI collaborator credited on a commit: the model that did the work,
/// the harness it ran under, and the email the credit is attributed to.
///
/// Renders as a single `Co-authored-by:` trailer — see [`Attribution::trailer`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Attribution {
    /// The resolved model identifier (e.g. `"claude-opus-4-8"`,
    /// `"ornith_35b"`, `"gpt-5.6-sol"`). Never a model's self-report — the
    /// caller resolves this from the session's active backend/loadout.
    pub model: String,
    /// The execution harness the model ran under (e.g. `"newt-agent"`,
    /// `"newt-agent crew"`). Never a model's self-report.
    pub harness: String,
    /// The email the credit is attributed to — ordinarily the resolved
    /// [`crate::agent_identity::AgentIdentity`] email (config override, else
    /// [`crate::agent_identity::DEFAULT_AGENT_EMAIL`]).
    pub email: String,
}

impl Attribution {
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        harness: impl Into<String>,
        email: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            harness: harness.into(),
            email: email.into(),
        }
    }

    /// The single `Co-authored-by:` trailer line for this contributor:
    /// `Co-authored-by: <model> (<harness>) <email>`.
    #[must_use]
    pub fn trailer(&self) -> String {
        format!(
            "Co-authored-by: {} ({}) <{}>",
            self.model, self.harness, self.email
        )
    }
}

/// The pending multi-contributor set for one not-yet-committed unit of work.
///
/// Ordered (first-contribution order) and deduplicated on the full
/// `(model, harness, email)` identity — the same model through two different
/// harnesses, or the same model+harness under two different configured
/// emails, are distinct contributors. There is no maximum size.
#[derive(Debug, Clone)]
pub struct AttributionLedger {
    default_email: String,
    contributors: Vec<Attribution>,
    seen: HashSet<Attribution>,
}

impl AttributionLedger {
    /// A fresh, empty ledger. `default_email` is the email
    /// [`AttributionLedger::record`] stamps on every entry — resolve it once
    /// (ordinarily from [`crate::agent_identity::AgentIdentity::email`]) and
    /// reuse the same ledger for the life of the session; construct a new
    /// ledger only if the configured attribution email itself changes.
    #[must_use]
    pub fn new(default_email: impl Into<String>) -> Self {
        Self {
            default_email: default_email.into(),
            contributors: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Record a material contribution from `model` running under `harness`,
    /// attributed to this ledger's configured default email. A no-op if this
    /// exact `(model, harness, default_email)` identity is already pending —
    /// first-contribution order is preserved, not bumped to the end.
    pub fn record(&mut self, model: impl Into<String>, harness: impl Into<String>) {
        let attribution = Attribution::new(model, harness, self.default_email.clone());
        self.add(attribution);
    }

    /// Record a fully-specified [`Attribution`] (an explicit email, e.g. for
    /// a contributor attributed under a different configured identity than
    /// this ledger's default). Deduplicates and preserves order exactly like
    /// [`AttributionLedger::record`].
    pub fn add(&mut self, attribution: Attribution) {
        if self.seen.insert(attribution.clone()) {
            self.contributors.push(attribution);
        }
    }

    /// True iff no contributor has been recorded since the last
    /// [`AttributionLedger::clear`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contributors.is_empty()
    }

    /// The pending contributors, in first-contribution order.
    #[must_use]
    pub fn contributors(&self) -> &[Attribution] {
        &self.contributors
    }

    /// One `Co-authored-by:` trailer line per pending contributor, in
    /// first-contribution order. Empty when the ledger is empty.
    #[must_use]
    pub fn trailers(&self) -> Vec<String> {
        self.contributors.iter().map(Attribution::trailer).collect()
    }

    /// [`AttributionLedger::trailers`] joined with newlines — the block a
    /// commit-message signer appends verbatim. Empty string when the ledger
    /// has no pending contributors (callers should treat that as "no
    /// attribution to stamp", not as a literal empty trailer line).
    #[must_use]
    pub fn render(&self) -> String {
        self.trailers().join("\n")
    }

    /// Discard every pending contributor. Call only after a commit built
    /// from [`AttributionLedger::render`] actually succeeded — a failed
    /// commit attempt must leave the ledger untouched so the same
    /// contributors are credited on the next attempt.
    pub fn clear(&mut self) {
        self.contributors.clear();
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_EMAIL: &str = crate::agent_identity::DEFAULT_AGENT_EMAIL;

    /// Contract test 1: one contributor produces exactly one trailer.
    #[test]
    fn one_contributor_produces_exactly_one_trailer() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("GPT-5.6 Sol", "newt-agent");
        assert_eq!(ledger.contributors().len(), 1);
        assert_eq!(
            ledger.trailers(),
            vec![format!(
                "Co-authored-by: GPT-5.6 Sol (newt-agent) <{DEFAULT_EMAIL}>"
            )]
        );
    }

    /// Contract test 2: a model switch, both contributing, produces TWO
    /// trailers — not only the later model.
    #[test]
    fn model_switch_with_both_contributing_produces_two_trailers() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        ledger.record("Model B", "newt-agent");
        assert_eq!(ledger.contributors().len(), 2);
        assert_eq!(
            ledger.trailers(),
            vec![
                format!("Co-authored-by: Model A (newt-agent) <{DEFAULT_EMAIL}>"),
                format!("Co-authored-by: Model B (newt-agent) <{DEFAULT_EMAIL}>"),
            ],
            "the earlier contributor must not be discarded by the later one"
        );
    }

    /// Contract test 3: a harness switch (same model) produces TWO trailers
    /// — the same model through two harnesses is two distinct identities.
    #[test]
    fn harness_switch_with_same_model_produces_two_trailers() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        ledger.record("Model A", "Codex");
        assert_eq!(ledger.contributors().len(), 2);
        assert_eq!(
            ledger.trailers(),
            vec![
                format!("Co-authored-by: Model A (newt-agent) <{DEFAULT_EMAIL}>"),
                format!("Co-authored-by: Model A (Codex) <{DEFAULT_EMAIL}>"),
            ]
        );
    }

    /// Contract test 4: repeated identical contribution produces ONE
    /// trailer, not three.
    #[test]
    fn duplicate_contribution_produces_one_trailer() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        ledger.record("Model A", "newt-agent");
        ledger.record("Model A", "newt-agent");
        assert_eq!(ledger.contributors().len(), 1);
    }

    /// Contract test 5: many contributors (10+) — none truncated, no hidden
    /// cap.
    #[test]
    fn many_contributors_are_not_truncated() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        for i in 0..25 {
            ledger.record(format!("Model-{i}"), format!("Harness-{i}"));
        }
        assert_eq!(ledger.contributors().len(), 25);
        assert_eq!(ledger.trailers().len(), 25);
        // First-contribution order preserved end to end.
        assert!(ledger.trailers()[0].contains("Model-0"));
        assert!(ledger.trailers()[24].contains("Model-24"));
    }

    /// Contract test 6: with no explicit email configuration, every
    /// generated attribution uses the default Newt Agent noreply address.
    #[test]
    fn default_email_is_the_newt_agent_noreply_address() {
        let mut ledger =
            AttributionLedger::new(crate::agent_identity::AgentIdentity::default().email);
        ledger.record("Model A", "newt-agent");
        assert!(ledger.trailers()[0].ends_with("<309460085+newt-agent@users.noreply.github.com>"));
    }

    /// Contract test 7: an explicitly configured attribution email still
    /// works — no provider-specific email is required.
    #[test]
    fn configured_email_override_is_used_for_every_contributor() {
        let mut ledger = AttributionLedger::new("custom-agent@example.com");
        ledger.record("Model A", "newt-agent");
        ledger.record("Model B", "Codex");
        for trailer in ledger.trailers() {
            assert!(trailer.ends_with("<custom-agent@example.com>"), "{trailer}");
        }
    }

    /// Contract test 8: after a successful commit, pending contributors are
    /// cleared.
    #[test]
    fn clear_empties_the_ledger_for_the_next_commit() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        assert!(!ledger.is_empty());
        ledger.clear();
        assert!(ledger.is_empty());
        assert!(ledger.trailers().is_empty());
        assert_eq!(ledger.render(), "");
    }

    /// Contract test 9: on a failed commit, pending contributors remain —
    /// callers must not clear before a commit is known to have succeeded.
    #[test]
    fn failed_commit_leaves_pending_contributors_untouched() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        // Simulated commit failure: the caller simply never calls `clear`.
        let commit_result: Result<(), &str> = Err("commit rejected");
        if commit_result.is_ok() {
            ledger.clear();
        }
        assert_eq!(ledger.contributors().len(), 1);
    }

    /// A harness switch under an otherwise-identical model+email is not
    /// silently merged with a same-named different-harness contributor from
    /// a DIFFERENT default email either — full triple identity.
    #[test]
    fn full_triple_identity_distinguishes_same_model_and_harness_different_email() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        ledger.add(Attribution::new(
            "Model A",
            "newt-agent",
            "someone-else@example.com",
        ));
        assert_eq!(
            ledger.contributors().len(),
            2,
            "a different email is a different provenance identity"
        );
    }

    #[test]
    fn render_joins_trailers_with_newlines() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent");
        ledger.record("Model B", "Codex");
        let rendered = ledger.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Co-authored-by: Model A"));
        assert!(lines[1].starts_with("Co-authored-by: Model B"));
    }
}
