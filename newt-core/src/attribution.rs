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
    /// The harness **version** the model ran under, captured at contribution
    /// time (e.g. [`crate::build_info::PACKAGE_VERSION`]). Pairs with
    /// [`Attribution::harness`] so a contributor is always credited under the
    /// *actual* harness build that ran its work — the same model under
    /// `v0.7.6` and `v0.8.0` is two distinct contributors, not one ambiguous
    /// identity (audit Q9). Renders inside the trailer's `(<harness> v<ver>)`
    /// qualifier, matching [`CommitAttribution::model_trailer`].
    pub harness_version: String,
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
        harness_version: impl Into<String>,
        email: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            harness: harness.into(),
            harness_version: harness_version.into(),
            email: email.into(),
        }
    }

    /// The single `Co-authored-by:` trailer line for this contributor:
    /// `Co-authored-by: <model> (<harness> v<harness_version>) <email>` —
    /// the SAME canonical shape [`CommitAttribution::model_trailer`] renders,
    /// so a ledger contributor and the active-at-commit model can never
    /// format attribution differently.
    ///
    /// [`CommitAttribution::model_trailer`]: CommitAttribution::model_trailer
    #[must_use]
    pub fn trailer(&self) -> String {
        format!(
            "Co-authored-by: {} ({} v{}) <{}>",
            self.model, self.harness, self.harness_version, self.email
        )
    }
}

/// The complete provenance of ONE commit: the model that drove it, the
/// harness build that ran it, and the operator + agent identities the
/// commit is attributed to. A snapshot of **authoritative runtime state**
/// at the moment a commit is about to be signed — never model-generated
/// text, never a session-startup cache.
///
/// Distinct from [`Attribution`] / [`AttributionLedger`], which are the
/// *multi-contributor AI-credit* set for a commit. [`CommitAttribution`] is
/// the single envelope around that set: the *active* model (one value, the
/// one driving this commit), the *harness build* (name + version + revision
/// + dirty state), and the *human operator* + *agent account* emails.
///
/// Every field is resolved from an existing authoritative source — see the
/// field docs and [`CommitAttribution::from_runtime`]. Construction is
/// **tool-less and deterministic** (no subprocess): harness fields come from
/// compile-time [`crate::build_info`] (plus the `NEWT_BRAND_NAME` env read),
/// and `model` / `operator` / `agent_email` are passed in from their live
/// sources, so a fresh [`CommitAttribution::from_runtime`] call always
/// reflects the *current* active model — a `/model` switch between commits
/// shows up in the next value, not the one captured at session boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAttribution {
    /// The active model that drove this commit — the **live** resolved model
    /// (the same `&str` threaded as the agent loop's `ChatCtx::model`), not a
    /// session-startup snapshot and not the model's prompt/system text. The
    /// caller passes the *current* value per commit, so a `/model` (or
    /// `/backend` / loadout) switch is reflected in the next construction.
    pub model: String,
    /// The execution harness name — [`crate::build_info::harness_name()`]:
    /// the `NEWT_BRAND_NAME` env override else the compiled-in
    /// [`crate::build_info::DEFAULT_BRAND_NAME`] (`"newt-agent"`). The single
    /// authoritative "which harness is this" source (no duplicate constant).
    pub harness_name: String,
    /// Harness SemVer version — [`crate::build_info::PACKAGE_VERSION`], from
    /// `CARGO_PKG_VERSION` at compile time.
    pub harness_version: String,
    /// Harness build revision + dirty state — [`crate::build_info::SOURCE_ID`]:
    /// the checked-out git commit captured at build time, with a `-dirty`
    /// suffix when the worktree had tracked or untracked changes.
    pub harness_build_revision: String,
    /// The human operator running the agent, when known. Reuses the
    /// configured [`crate::agent_identity::AgentIdentity::operator`] field.
    /// The host-git fallback ([`crate::agent_identity::AgentIdentity::operator_name`])
    /// shells out to `git config` and is therefore the *caller's*
    /// responsibility — kept out of this tool-less constructor.
    pub operator_name: Option<String>,
    /// The human operator's email. [`None`] **explicitly** when no operator
    /// email is configured — never invented. No operator-email source exists
    /// in `agent-identity.toml` today; the field is `Option` so a future
    /// source can populate it without reshaping the type.
    pub operator_email: Option<String>,
    /// The agent account's attribution email — the canonical Newt Agent
    /// GitHub noreply address
    /// ([`crate::agent_identity::DEFAULT_AGENT_EMAIL`]), overridable via a
    /// resolved [`crate::agent_identity::AgentIdentity::email`].
    pub agent_email: String,
    /// #1707/#1709 semantic B — the accumulated multi-contributor set for
    /// this commit: every model/harness/version that materially contributed
    /// to the committed work, NOT just the active-at-commit model. A snapshot
    /// of the session [`AttributionLedger`] captured at the latest practical
    /// point before the turn that may commit (see the session loop); empty
    /// (`Vec::new()`) is the single-active-model floor (semantic A).
    /// [`CommitAttribution::finalize_message`] merges this with the active
    /// model and renders one `Co-authored-by:` trailer per contributor.
    pub contributors: Vec<Attribution>,
}

impl CommitAttribution {
    /// Build a commit attribution from already-resolved runtime state.
    ///
    /// Tool-less and deterministic — no subprocess: `harness_name` /
    /// `harness_version` / `harness_build_revision` come from
    /// [`crate::build_info`] (compile-time constants plus the `NEWT_BRAND_NAME`
    /// env read), and `model` / `operator_name` / `agent_email` are passed in
    /// from their live sources. A fresh call therefore always reflects the
    /// *current* active model, not a session-boot snapshot.
    ///
    /// `operator_email` has no source today and is left [`None`] (see the
    /// field doc); set it directly on the returned value if a source is added.
    #[must_use]
    pub fn from_runtime(
        model: impl Into<String>,
        operator_name: Option<String>,
        agent_email: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            harness_name: crate::build_info::harness_name(),
            harness_version: crate::build_info::PACKAGE_VERSION.to_string(),
            harness_build_revision: crate::build_info::SOURCE_ID.to_string(),
            operator_name,
            operator_email: None,
            agent_email: agent_email.into(),
            contributors: Vec::new(),
        }
    }

    /// Convenience: pull `agent_email` and `operator_name` from a resolved
    /// [`crate::agent_identity::AgentIdentity`]. Uses the configured
    /// `operator` field directly (no host-git subprocess fallback) to keep
    /// construction tool-less; a caller that wants the `git config user.name`
    /// fallback should apply [`crate::agent_identity::AgentIdentity::operator_name`]
    /// itself and pass the result through [`CommitAttribution::from_runtime`].
    #[must_use]
    pub fn from_identity(
        model: impl Into<String>,
        identity: &crate::agent_identity::AgentIdentity,
    ) -> Self {
        Self::from_runtime(model, identity.operator.clone(), identity.email.clone())
    }

    // ---- deterministic commit-message finalization (#1709 family) ----
    //
    // The model is allowed to provide only a commit subject + body. The
    // harness owns attribution. [`CommitAttribution::finalize_message`] takes
    // the model-provided message and this typed provenance value, and renders
    // ONE canonical, idempotent commit message from them — replacing any
    // stale Newt-owned attribution a previous run left, preserving legitimate
    // third-party trailers, and never hard-coding a model or package version
    // (both come from this single typed value).

    /// The canonical Newt-managed model attribution trailer, rendered from
    /// this typed value:
    /// `Co-authored-by: <model> (<harness> v<version>) <agent-email>`.
    ///
    /// `model`, `harness_name`, `harness_version`, and `agent_email` are all
    /// fields of this one [`CommitAttribution`] (requirements 1, 3, 4) — no
    /// current-model or current-package-version constant is hard-coded.
    #[must_use]
    pub fn model_trailer(&self) -> String {
        format!(
            "Co-authored-by: {} ({} v{}) <{}>",
            self.model, self.harness_name, self.harness_version, self.agent_email
        )
    }

    /// The canonical provenance line, rendered from this same typed value
    /// (requirement 2):
    /// `Harness: <harness> v<version> (<revision>) | Model: <model> | Operator: <operator>`.
    ///
    /// `operator` is [`CommitAttribution::operator_name`] when known, else the
    /// deterministic sentinel `"unknown"`. No operator *email* is ever
    /// manufactured (requirement 12) — this line carries the operator *name*,
    /// and [`CommitAttribution::operator_email`] is intentionally absent from
    /// the rendered output.
    #[must_use]
    pub fn provenance_line(&self) -> String {
        format!(
            "Harness: {} v{} ({}) | Model: {} | Operator: {}",
            self.harness_name,
            self.harness_version,
            self.harness_build_revision,
            self.model,
            self.operator_name.as_deref().unwrap_or("unknown"),
        )
    }

    /// Finalize a commit message from a model-provided subject/body plus this
    /// typed provenance value — rendering **semantic B** (accumulated
    /// contributors) when [`CommitAttribution::contributors`] is non-empty,
    /// and the single-active-model floor (semantic A) when it is empty.
    ///
    /// Delegates to [`CommitAttribution::finalize_message_with`] with this
    /// value's `contributors` snapshot: the active-at-commit model is always
    /// merged in (it drove the commit), and every prior contributor in the
    /// snapshot gets its own `Co-authored-by:` trailer — so a `/model` switch
    /// mid-session credits BOTH models on the one commit (the contract),
    /// while an empty snapshot yields exactly the single active-model
    /// trailer (bit-for-bit the floor).
    #[must_use]
    pub fn finalize_message(&self, message: &str) -> String {
        self.finalize_message_with(message, &self.contributors)
    }

    /// Finalize a commit message from a model-provided subject/body plus this
    /// typed provenance value AND an accumulated contributor set — semantic B
    /// (#1707/#1709 "accumulated contributors").
    ///
    /// The model may provide any subject + body text; the harness owns the
    /// attribution. This:
    ///
    /// 1. Merges `contributors` with the active-at-commit model (this value):
    ///    the active model drove the commit, so it is always a contributor
    ///    even if the ledger is empty (the floor). Deduplicates on the full
    ///    `(model, harness, harness_version, email)` identity, preserving
    ///    first-contribution order with the active model appended if new — so
    ///    a `/model` switch mid-session ADDS a contributor and never discards
    ///    an earlier one (the contract), while an empty ledger yields exactly
    ///    the single active-model trailer (bit-for-bit the floor).
    /// 2. Splits the message into body + a trailing trailer block (git's
    ///    "blank line before trailers" convention).
    /// 3. Partitions the existing trailers into Newt-owned vs third-party.
    ///    Newt-owned = ANY `Co-authored-by:` line attributed to this value's
    ///    `agent_email` (there may be several from a prior multi-contributor
    ///    run — all are stale and replaced, not just one) and the provenance
    ///    line (any `Harness:` line).
    /// 4. Drops stale Newt-owned model attribution and stale provenance,
    ///    preserving legitimate third-party `Co-authored-by:` /
    ///    `Signed-off-by:` / … trailers verbatim and in order.
    /// 5. Appends a blank line, the preserved third-party trailers, then ONE
    ///    `Co-authored-by:` trailer per merged contributor (canonical
    ///    `<model> (<harness> v<version>) <email>` shape), then the single
    ///    provenance line rendered from this value.
    ///
    /// The provenance line stays single (one harness build drives the commit);
    /// the multi-contributor history lives in the per-contributor trailers,
    /// each paired with its own harness/version — matching the contract's
    /// "many `Co-authored-by:`, one `Harness:` provenance" shape.
    ///
    /// Repeated finalization is idempotent: running it on its own output
    /// yields the same bytes, because the freshly-rendered Newt trailers are
    /// recognized as Newt-owned on the next pass and replaced rather than
    /// duplicated.
    ///
    /// Rendering is deterministic: given the same typed value, the same
    /// contributor set, and the same input message, the output is
    /// byte-identical — no wall clock, no subprocess, no model text beyond
    /// what the caller passed.
    #[must_use]
    pub fn finalize_message_with(&self, message: &str, contributors: &[Attribution]) -> String {
        let (body, existing) = split_message(message);
        let agent_email_tag = format!("<{}>", self.agent_email);

        // Merge accumulated contributors with the active-at-commit model.
        // Dedup on the full identity; first-contribution order, active model
        // appended only if it is not already present.
        let mut merged: Vec<Attribution> = Vec::with_capacity(contributors.len() + 1);
        let mut seen: HashSet<Attribution> = HashSet::with_capacity(contributors.len() + 1);
        for c in contributors {
            if seen.insert(c.clone()) {
                merged.push(c.clone());
            }
        }
        let active = Attribution::new(
            &self.model,
            &self.harness_name,
            &self.harness_version,
            &self.agent_email,
        );
        if seen.insert(active.clone()) {
            merged.push(active);
        }

        let mut third_party: Vec<String> = Vec::new();
        for trailer in existing {
            if is_newt_model_trailer(&trailer, &agent_email_tag) || is_newt_provenance(&trailer) {
                continue; // stale Newt-owned — drop, re-render below
            }
            third_party.push(trailer);
        }

        let mut trailers = third_party;
        for c in &merged {
            trailers.push(c.trailer());
        }
        trailers.push(self.provenance_line());

        let mut out = body;
        out.push_str("\n\n");
        out.push_str(&trailers.join("\n"));
        out.push('\n');
        out
    }
}

// ---- message-splitting + Newt-owned-trailer recognition helpers ----
//
// Free functions (not methods): they take no `&self`, so they are pure
// functions of their arguments and trivially unit-testable in isolation.

/// A line is trailer-shaped if it is `Key: value` with an alphanumeric /
/// `-` / `_` key — the shape git's trailer parser recognizes. Continuation
/// lines and blank lines are NOT recognized (the canonical Newt trailers are
/// single-line); a multi-line trailer block therefore falls back to "no
/// trailers" and is preserved as body, which is safe (we never drop user
/// text).
fn looks_like_trailer(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) || line.is_empty() {
        return false;
    }
    match line.find(": ") {
        Some(idx) => {
            let key = &line[..idx];
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }
        None => false,
    }
}

/// Split a commit message into `(body, trailers)` on the last blank line.
/// The trailers are the maximal run of trailer-shaped lines after the final
/// blank line; if that run is empty or contains a non-trailer line, the whole
/// message is treated as body (no trailer block). Trailing blank lines are
/// stripped from the body; internal blank lines are preserved.
fn split_message(message: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = message.lines().collect();
    let last_blank = lines.iter().rposition(|l| l.trim().is_empty());
    let tail_start = match last_blank {
        Some(idx) => idx + 1,
        None => return (body_of(&lines), Vec::new()),
    };
    let tail = &lines[tail_start..];
    if tail.is_empty() || !tail.iter().all(|l| looks_like_trailer(l)) {
        return (body_of(&lines), Vec::new());
    }
    let body = body_of(&lines[..tail_start]);
    let trailers = tail.iter().map(|s| s.to_string()).collect();
    (body, trailers)
}

/// Join message lines into a body string with trailing blank lines stripped
/// (internal blank lines preserved).
fn body_of(lines: &[&str]) -> String {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines[..end].join("\n")
}

/// A Newt-owned model attribution trailer: a `Co-authored-by:` line credited
/// to this value's agent account email. The model and harness version in a
/// stale trailer may differ from the current value — matching on the
/// (stable) agent email is what lets us recognize and *replace* it rather
/// than duplicate (requirement 8).
fn is_newt_model_trailer(line: &str, agent_email_tag: &str) -> bool {
    line.starts_with("Co-authored-by: ") && line.ends_with(agent_email_tag)
}

/// A Newt-owned provenance line: any `Harness:` line. Replaced wholesale on
/// re-finalization so a stale revision/version does not linger (requirement
/// 9).
fn is_newt_provenance(line: &str) -> bool {
    line.starts_with("Harness: ")
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

    /// Record a material contribution from `model` running under `harness` at
    /// `harness_version`, attributed to this ledger's configured default email.
    /// `harness_version` is captured at contribution time (ordinarily
    /// [`crate::build_info::PACKAGE_VERSION`]) so the contributor stays paired
    /// with the harness build that actually ran its work (audit Q9). A no-op if
    /// this exact `(model, harness, harness_version, default_email)` identity
    /// is already pending — first-contribution order is preserved, not bumped
    /// to the end.
    pub fn record(
        &mut self,
        model: impl Into<String>,
        harness: impl Into<String>,
        harness_version: impl Into<String>,
    ) {
        let attribution =
            Attribution::new(model, harness, harness_version, self.default_email.clone());
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
    /// Deterministic harness version for ledger tests — the version is part
    /// of the contributor identity now (audit Q9), so tests pin it rather
    /// than reading the live `PACKAGE_VERSION` (which would make assertions
    /// drift on every release).
    const TEST_VER: &str = "0.0.0-test";

    /// Contract test 1: one contributor produces exactly one trailer.
    #[test]
    fn one_contributor_produces_exactly_one_trailer() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("GPT-5.6 Sol", "newt-agent", TEST_VER);
        assert_eq!(ledger.contributors().len(), 1);
        assert_eq!(
            ledger.trailers(),
            vec![format!(
                "Co-authored-by: GPT-5.6 Sol (newt-agent v{TEST_VER}) <{DEFAULT_EMAIL}>"
            )]
        );
    }

    /// Contract test 2: a model switch, both contributing, produces TWO
    /// trailers — not only the later model.
    #[test]
    fn model_switch_with_both_contributing_produces_two_trailers() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model B", "newt-agent", TEST_VER);
        assert_eq!(ledger.contributors().len(), 2);
        assert_eq!(
            ledger.trailers(),
            vec![
                format!("Co-authored-by: Model A (newt-agent v{TEST_VER}) <{DEFAULT_EMAIL}>"),
                format!("Co-authored-by: Model B (newt-agent v{TEST_VER}) <{DEFAULT_EMAIL}>"),
            ],
            "the earlier contributor must not be discarded by the later one"
        );
    }

    /// Contract test 3: a harness switch (same model) produces TWO trailers
    /// — the same model through two harnesses is two distinct identities.
    #[test]
    fn harness_switch_with_same_model_produces_two_trailers() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model A", "Codex", TEST_VER);
        assert_eq!(ledger.contributors().len(), 2);
        assert_eq!(
            ledger.trailers(),
            vec![
                format!("Co-authored-by: Model A (newt-agent v{TEST_VER}) <{DEFAULT_EMAIL}>"),
                format!("Co-authored-by: Model A (Codex v{TEST_VER}) <{DEFAULT_EMAIL}>"),
            ]
        );
    }

    /// Contract test 4: repeated identical contribution produces ONE
    /// trailer, not three.
    #[test]
    fn duplicate_contribution_produces_one_trailer() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model A", "newt-agent", TEST_VER);
        assert_eq!(ledger.contributors().len(), 1);
    }

    /// Contract test 4b: the same model+harness under TWO different versions
    /// is TWO contributors — version is part of the identity (audit Q9), so
    /// a harness bump mid-session credits the work under the build that
    /// actually ran it rather than silently merging it into the old one.
    #[test]
    fn version_switch_with_same_model_and_harness_produces_two_trailers() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", "0.7.6");
        ledger.record("Model A", "newt-agent", "0.8.0");
        assert_eq!(ledger.contributors().len(), 2);
        assert_eq!(
            ledger.trailers(),
            vec![
                format!("Co-authored-by: Model A (newt-agent v0.7.6) <{DEFAULT_EMAIL}>"),
                format!("Co-authored-by: Model A (newt-agent v0.8.0) <{DEFAULT_EMAIL}>"),
            ]
        );
    }

    /// Contract test 5: many contributors (10+) — none truncated, no hidden
    /// cap.
    #[test]
    fn many_contributors_are_not_truncated() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        for i in 0..25 {
            ledger.record(format!("Model-{i}"), format!("Harness-{i}"), TEST_VER);
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
        ledger.record("Model A", "newt-agent", TEST_VER);
        assert!(ledger.trailers()[0].ends_with("<309460085+newt-agent@users.noreply.github.com>"));
    }

    /// Contract test 7: an explicitly configured attribution email still
    /// works — no provider-specific email is required.
    #[test]
    fn configured_email_override_is_used_for_every_contributor() {
        let mut ledger = AttributionLedger::new("custom-agent@example.com");
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model B", "Codex", TEST_VER);
        for trailer in ledger.trailers() {
            assert!(trailer.ends_with("<custom-agent@example.com>"), "{trailer}");
        }
    }

    /// Contract test 8: after a successful commit, pending contributors are
    /// cleared.
    #[test]
    fn clear_empties_the_ledger_for_the_next_commit() {
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", TEST_VER);
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
        ledger.record("Model A", "newt-agent", TEST_VER);
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
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.add(Attribution::new(
            "Model A",
            "newt-agent",
            TEST_VER,
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
        ledger.record("Model A", "newt-agent", TEST_VER);
        ledger.record("Model B", "Codex", TEST_VER);
        let rendered = ledger.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Co-authored-by: Model A"));
        assert!(lines[1].starts_with("Co-authored-by: Model B"));
    }

    // ---- CommitAttribution: constructing the value from runtime state ----

    use super::CommitAttribution;

    /// Every field of `from_runtime` is sourced from the existing
    /// authoritative runtime/compile-time state, not invented: harness fields
    /// mirror `build_info`, the agent email is the caller-passed canonical
    /// noreply address, and the model is the live value the caller threaded
    /// in. Comparing against the same source functions (not hard-coded
    /// literals) keeps this independent of the build environment.
    #[test]
    fn from_runtime_sources_every_field_from_existing_state() {
        let ca = CommitAttribution::from_runtime("ornith-1.0-35b", None, DEFAULT_EMAIL);
        assert_eq!(ca.model, "ornith-1.0-35b");
        assert_eq!(ca.harness_name, crate::build_info::harness_name());
        assert_eq!(ca.harness_version, crate::build_info::PACKAGE_VERSION);
        assert_eq!(ca.harness_build_revision, crate::build_info::SOURCE_ID);
        assert_eq!(ca.agent_email, DEFAULT_EMAIL);
        assert!(ca.operator_name.is_none());
        assert!(
            ca.operator_email.is_none(),
            "no operator-email source today"
        );
    }

    /// Harness name + version + build revision come from `build_info`
    /// verbatim — no duplicate constants for version/name/revision state
    /// (decision 12). The version is a prefix of the build-revision-bearing
    /// version string the footer already uses, so they cannot drift.
    #[test]
    fn harness_fields_reuse_build_info_with_no_duplicate_constants() {
        let ca = CommitAttribution::from_runtime("m", None, DEFAULT_EMAIL);
        // PACKAGE_VERSION is a compile-time const from CARGO_PKG_VERSION.
        assert_eq!(ca.harness_version, env!("CARGO_PKG_VERSION"));
        // SOURCE_ID is the git commit (+ -dirty); it must start with the
        // captured commit, proving revision + dirty-state reuse (decision 6).
        assert!(
            ca.harness_build_revision
                .starts_with(crate::build_info::GIT_COMMIT),
            "build revision must carry the captured git commit"
        );
    }

    /// A `/model` switch between two commits is reflected when constructing a
    /// FRESH value (decision 14): the second `from_runtime` call sees the
    /// *current* model, not one cached at session startup. Only `model`
    /// differs; harness provenance is stable across the two constructions.
    #[test]
    fn fresh_construction_reflects_a_model_switch() {
        let before = CommitAttribution::from_runtime("model-a", None, DEFAULT_EMAIL);
        // Simulate the operator running `/model model-b` between commits.
        let after = CommitAttribution::from_runtime("model-b", None, DEFAULT_EMAIL);
        assert_ne!(before.model, after.model, "the switch must be visible");
        assert_eq!(after.model, "model-b");
        // Harness provenance is build-derived, not model-derived: unchanged.
        assert_eq!(before.harness_name, after.harness_name);
        assert_eq!(before.harness_version, after.harness_version);
        assert_eq!(before.harness_build_revision, after.harness_build_revision);
        assert_eq!(before.agent_email, after.agent_email);
    }

    /// The active model comes from the live resolved source threaded into the
    /// constructor — never from model prompt/system text (decision 11). The
    /// type carries no prompt field and is independent of the prompt pipeline.
    #[test]
    fn model_is_a_runtime_parameter_independent_of_prompt_text() {
        // Two commits under the same model but with arbitrary prompt text
        // the type never sees — the attribution is identical, proving the
        // type does not depend on prompt content.
        let a = CommitAttribution::from_runtime("shared-model", None, DEFAULT_EMAIL);
        let b = CommitAttribution::from_runtime("shared-model", None, DEFAULT_EMAIL);
        assert_eq!(a, b);
    }

    /// `from_identity` reuses a resolved `AgentIdentity`: the agent email and
    /// the configured operator name flow through, and the default identity
    /// yields the canonical Newt Agent noreply address (decision 7) with no
    /// operator (decision 9: explicit `None`, never an invented name).
    #[test]
    fn from_identity_reuses_resolved_agent_identity() {
        let id = crate::agent_identity::AgentIdentity::default();
        let ca = CommitAttribution::from_identity("ornith", &id);
        assert_eq!(
            ca.agent_email,
            crate::agent_identity::DEFAULT_AGENT_EMAIL,
            "default identity → canonical newt-agent noreply"
        );
        assert!(ca.operator_name.is_none());
        assert!(ca.operator_email.is_none());
        // Harness fields are still sourced from build_info, not the identity.
        assert_eq!(ca.harness_name, crate::build_info::harness_name());
    }

    /// A configured operator name flows through `from_identity`; a configured
    /// agent-email override is used verbatim (decision 7: reuse the canonical
    /// email via the identity, overrideable). Construction stays tool-less —
    /// no `git config` subprocess is spawned (decision 10).
    #[test]
    fn from_identity_carries_configured_operator_and_email_override() {
        let id = crate::agent_identity::AgentIdentity {
            operator: Some("shawn".to_string()),
            email: "custom-agent@example.com".to_string(),
            ..Default::default()
        };
        let ca = CommitAttribution::from_identity("m", &id);
        assert_eq!(ca.operator_name.as_deref(), Some("shawn"));
        assert_eq!(ca.agent_email, "custom-agent@example.com");
        // operator_email stays explicitly None — never invented.
        assert!(ca.operator_email.is_none());
    }

    // ---- finalize_message: the deterministic commit-message finalizer ----

    /// A fixed typed value the finalizer tests build against. Every field is
    /// explicit so the assertions are independent of the real build
    /// environment (no real git revision / package version in the test
    /// expectations — those are *rendered from the value*, proving
    /// requirements 1, 2, 3, 4).
    fn sample_ca() -> CommitAttribution {
        CommitAttribution {
            model: "glm-5.2".to_string(),
            harness_name: "newt-agent".to_string(),
            harness_version: "0.8.0".to_string(),
            harness_build_revision: "ba56944bd262-dirty".to_string(),
            operator_name: Some("shawn".to_string()),
            operator_email: None,
            agent_email: DEFAULT_EMAIL.to_string(),
            contributors: Vec::new(),
        }
    }

    /// Requirement 1 + 3 + 4: the model trailer renders model + harness +
    /// harness version all from the SAME typed value, with no hard-coded
    /// current-model or current-package-version constant.
    #[test]
    fn model_trailer_renders_model_harness_version_from_the_value() {
        let ca = sample_ca();
        assert_eq!(
            ca.model_trailer(),
            format!("Co-authored-by: glm-5.2 (newt-agent v0.8.0) <{DEFAULT_EMAIL}>")
        );
    }

    /// Requirement 2: the provenance line is rendered from the same value —
    /// harness, version, revision, model, and operator all flow from the one
    /// typed value. No operator email appears (requirement 12).
    #[test]
    fn provenance_line_renders_from_the_same_value() {
        let ca = sample_ca();
        assert_eq!(
            ca.provenance_line(),
            "Harness: newt-agent v0.8.0 (ba56944bd262-dirty) | Model: glm-5.2 | Operator: shawn"
        );
    }

    /// Requirement 12: when no operator name is known, the provenance line
    /// uses the deterministic `"unknown"` sentinel — and never an invented
    /// operator email.
    #[test]
    fn provenance_line_uses_unknown_when_no_operator_and_never_an_email() {
        let ca = CommitAttribution {
            operator_name: None,
            ..sample_ca()
        };
        assert_eq!(
            ca.provenance_line(),
            "Harness: newt-agent v0.8.0 (ba56944bd262-dirty) | Model: glm-5.2 | Operator: unknown"
        );
        assert!(
            !ca.provenance_line().contains('@'),
            "no operator email is ever manufactured"
        );
    }

    /// Requirement 5: arbitrary user subject + body text is preserved
    /// verbatim, and a blank line separates body from trailers (req 11).
    #[test]
    fn finalize_preserves_arbitrary_subject_and_body() {
        let ca = sample_ca();
        let input = "fix(parser): handle trailing whitespace\n\nBody para one.\n\nBody para two with `code`.\n";
        let out = ca.finalize_message(input);
        assert!(
            out.starts_with("fix(parser): handle trailing whitespace\n\nBody para one.\n\nBody para two with `code`.\n\n"),
            "subject + body must be preserved verbatim, then a blank line"
        );
        // The canonical trailers are appended after the body.
        assert!(out.contains(&ca.model_trailer()));
        assert!(out.contains(&ca.provenance_line()));
    }

    /// Requirement 6: a legitimate third-party `Co-authored-by:` trailer (a
    /// real person, different email) is preserved through finalization.
    #[test]
    fn finalize_preserves_third_party_co_authored_by() {
        let ca = sample_ca();
        let input = "feat: thing\n\nBody.\n\nCo-authored-by: Alice <alice@example.com>\nSigned-off-by: Bob <bob@example.com>\n";
        let out = ca.finalize_message(input);
        assert!(
            out.contains("Co-authored-by: Alice <alice@example.com>"),
            "third-party Co-authored-by must be preserved"
        );
        assert!(
            out.contains("Signed-off-by: Bob <bob@example.com>"),
            "other third-party trailers must be preserved"
        );
    }

    /// Requirement 8: a STALE Newt-owned model attribution trailer (old model
    /// / old harness version, but same agent email) is REPLACED, not
    /// duplicated.
    #[test]
    fn finalize_replaces_stale_newt_owned_model_trailer() {
        let ca = sample_ca();
        let input = format!(
            "feat: thing\n\nBody.\n\nCo-authored-by: old-model (newt-agent v0.7.5) <{DEFAULT_EMAIL}>\n"
        );
        let out = ca.finalize_message(&input);
        assert!(
            !out.contains("old-model"),
            "stale model trailer must be gone"
        );
        assert!(
            !out.contains("v0.7.5"),
            "stale harness version must be gone"
        );
        // Exactly one model trailer in the output — the fresh one.
        let model_trailers: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.starts_with("Co-authored-by: ") && l.ends_with(&format!("<{DEFAULT_EMAIL}>"))
            })
            .collect();
        assert_eq!(model_trailers.len(), 1, "no duplicate model trailer");
        assert_eq!(model_trailers[0], ca.model_trailer());
    }

    /// Requirement 9: a STALE Newt-owned provenance line (old revision) is
    /// replaced, not duplicated.
    #[test]
    fn finalize_replaces_stale_newt_owned_provenance() {
        let ca = sample_ca();
        let input = "feat: thing\n\nBody.\n\nHarness: newt-agent v0.7.5 (deadbeef) | Model: old | Operator: old\n";
        let out = ca.finalize_message(input);
        assert!(
            !out.contains("deadbeef"),
            "stale provenance revision must be gone"
        );
        let provenance: Vec<&str> = out.lines().filter(|l| l.starts_with("Harness: ")).collect();
        assert_eq!(provenance.len(), 1, "exactly one provenance line");
        assert_eq!(provenance[0], ca.provenance_line());
    }

    /// Requirement 10: repeated finalization is idempotent — running the
    /// finalizer on its own output yields byte-identical bytes.
    #[test]
    fn finalize_is_idempotent() {
        let ca = sample_ca();
        let input = "feat: thing\n\nBody.\n\nCo-authored-by: Alice <alice@example.com>\n";
        let once = ca.finalize_message(input);
        let twice = ca.finalize_message(&once);
        assert_eq!(once, twice, "re-finalization must be a no-op");
        let thrice = ca.finalize_message(&twice);
        assert_eq!(twice, thrice, "idempotent across repeated calls");
    }

    /// Requirement 10 (negative): idempotence survives a model switch
    /// between finalizations only if the value is the same; with a DIFFERENT
    /// value the stale trailer is replaced — proving the idempotence above is
    /// genuine recognition, not "output already matched by luck".
    #[test]
    fn a_model_switch_replaces_the_stale_trailer() {
        let before = sample_ca();
        let msg = "feat: thing\n\nBody.\n";
        let once = before.finalize_message(msg);
        let after = CommitAttribution {
            model: "glm-5.3".to_string(),
            harness_version: "0.8.1".to_string(),
            harness_build_revision: "cef01234-clean".to_string(),
            ..sample_ca()
        };
        let twice = after.finalize_message(&once);
        assert!(twice.contains("glm-5.3 (newt-agent v0.8.1)"));
        assert!(twice.contains("Harness: newt-agent v0.8.1 (cef01234-clean)"));
        assert!(
            !twice.contains("glm-5.2 (newt-agent v0.8.0)"),
            "old model trailer replaced"
        );
    }

    /// Requirement 11: the blank line before the trailer block is present,
    /// and there is exactly one (not two, not zero).
    #[test]
    fn finalize_emits_exactly_one_blank_line_before_trailers() {
        let ca = sample_ca();
        let out = ca.finalize_message("feat: thing\n\nBody.\n");
        // The body ends, then exactly one blank line, then the trailer block.
        let body_end = out.find("\n\nCo-authored-by: ").unwrap();
        let _ = body_end; // present
                          // No doubled blank line right before the trailer block.
        assert!(
            !out.contains("\n\n\nCo-authored-by: "),
            "exactly one blank line before trailers"
        );
        // The trailer block is the last paragraph: a line after the trailers
        // would break git's trailer parsing.
        let mut lines = out.lines();
        let mut seen_trailer = false;
        for l in lines.by_ref() {
            if l.starts_with("Co-authored-by: ") || l.starts_with("Harness: ") {
                seen_trailer = true;
            } else if seen_trailer {
                panic!("non-trailer line after trailer block: {l:?}");
            }
        }
    }

    /// Requirement 13: rendering is deterministic — the same value + input
    /// always produce byte-identical output.
    #[test]
    fn finalize_is_deterministic() {
        let ca = sample_ca();
        let input = "feat: thing\n\nBody.\n\nCo-authored-by: Alice <alice@example.com>\n";
        let a = ca.finalize_message(input);
        let b = ca.finalize_message(input);
        assert_eq!(a, b);
    }

    /// Representative before/after, asserted as a single golden snapshot so a
    /// future change to the format is loudly visible. This is the
    /// "representative before/after" deliverable expressed as a test.
    #[test]
    fn finalize_before_after_snapshot() {
        let ca = sample_ca();
        let before = "feat(attribution): finalizer\n\nDrives the commit.\n\nCo-authored-by: glm-4.7 (newt-agent v0.7.6) <309460085+newt-agent@users.noreply.github.com>\nHarness: newt-agent v0.7.6 (aabbccdd) | Model: glm-4.7 | Operator: shawn\nCo-authored-by: Reviewer <reviewer@example.com>\n";
        let after = ca.finalize_message(before);
        let expected = format!(
            "feat(attribution): finalizer\n\nDrives the commit.\n\n\
             Co-authored-by: Reviewer <reviewer@example.com>\n\
             Co-authored-by: glm-5.2 (newt-agent v0.8.0) <{DEFAULT_EMAIL}>\n\
             Harness: newt-agent v0.8.0 (ba56944bd262-dirty) | Model: glm-5.2 | Operator: shawn\n"
        );
        assert_eq!(after, expected);
    }

    // ---- semantic B: accumulated multi-contributor finalization ----
    //
    // `finalize_message_with` merges an AttributionLedger's accumulated
    // contributors with the active-at-commit model, so a `/model` switch
    // mid-session credits BOTH models on the one commit — the contract —
    // rather than only the model driving the commit (semantic A, the floor).

    /// Two accumulated contributors + the active model = THREE trailers, in
    /// first-contribution order with the active model appended. The
    /// provenance line stays single (one harness build drives the commit).
    #[test]
    fn finalize_with_ledger_credits_every_accumulated_contributor() {
        let ca = sample_ca(); // active model = glm-5.2 under newt-agent v0.8.0
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", "0.7.6");
        ledger.record("Model B", "Codex", "0.8.0");
        let out = ca.finalize_message_with("fix the parser", ledger.contributors());
        let lines: Vec<&str> = out.lines().collect();
        // body, blank, then trailers: Model A, Model B, active glm-5.2, provenance.
        assert_eq!(lines[0], "fix the parser");
        assert_eq!(lines[1], "");
        assert_eq!(
            lines[2],
            format!("Co-authored-by: Model A (newt-agent v0.7.6) <{DEFAULT_EMAIL}>")
        );
        assert_eq!(
            lines[3],
            format!("Co-authored-by: Model B (Codex v0.8.0) <{DEFAULT_EMAIL}>")
        );
        assert_eq!(
            lines[4],
            format!("Co-authored-by: glm-5.2 (newt-agent v0.8.0) <{DEFAULT_EMAIL}>")
        );
        assert!(lines[5].starts_with("Harness: "));
        assert_eq!(lines.len(), 6);
    }

    /// The active model is merged and DEDUPED against the ledger: if it
    /// already contributed (same model+harness+version+email), it is not
    /// stamped twice. This is the `/model` switch back to a previous model —
    /// credit it once.
    #[test]
    fn finalize_with_ledger_dedupes_active_model_already_present() {
        let ca = sample_ca(); // active = glm-5.2 / newt-agent v0.8.0
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("glm-5.2", "newt-agent", "0.8.0"); // identical to active
        ledger.record("Model A", "newt-agent", "0.7.6");
        let out = ca.finalize_message_with("fix the parser", ledger.contributors());
        let coauth: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("Co-authored-by: "))
            .collect();
        assert_eq!(
            coauth.len(),
            2,
            "active model dedupes against the ledger — no double trailer"
        );
        // First-contribution order: Model A was NOT in the ledger before the
        // active identity, so the active identity (recorded first into the
        // ledger) leads, then Model A.
        assert!(coauth[0].contains("glm-5.2"));
        assert!(coauth[1].contains("Model A"));
    }

    /// An empty contributor set is the floor: `finalize_message_with(&[])`
    /// is byte-identical to `finalize_message` — the single active-model
    /// trailer + provenance. B never regresses the A floor.
    #[test]
    fn finalize_with_empty_ledger_matches_the_single_model_floor() {
        let ca = sample_ca();
        let msg = "feat(x): y\n\nbody text\n";
        assert_eq!(ca.finalize_message(msg), ca.finalize_message_with(msg, &[]));
    }

    /// Re-finalization is idempotent under B: running `finalize_message_with`
    /// on its own output with the SAME contributors yields the same bytes —
    /// the freshly-rendered Newt trailers are recognized as Newt-owned
    /// (agent-email-tagged) on the next pass and replaced, not duplicated.
    #[test]
    fn finalize_with_ledger_is_idempotent() {
        let ca = sample_ca();
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", "0.7.6");
        let contributors = ledger.contributors().to_vec();
        let once = ca.finalize_message_with("fix the parser\n\nbody", &contributors);
        let twice = ca.finalize_message_with(&once, &contributors);
        assert_eq!(
            once, twice,
            "re-finalization must not duplicate contributors"
        );
    }

    /// Stale multi-contributor Newt trailers from a prior run are ALL
    /// replaced, not just one: a previous B finalization may have left
    /// several `Co-authored-by:` lines tagged with the agent email, and
    /// re-finalizing with a fresh contributor set drops every one rather
    /// than appending duplicates alongside them.
    #[test]
    fn finalize_with_ledger_replaces_every_stale_newt_model_trailer() {
        let ca = sample_ca();
        // A prior run left TWO stale Newt model trailers (both agent-email-
        // tagged) plus a third-party one.
        let stale = format!(
            "fix the parser\n\n\
             Co-authored-by: Old Model (newt-agent v0.1.0) <{DEFAULT_EMAIL}>\n\
             Co-authored-by: Older Model (newt-agent v0.0.1) <{DEFAULT_EMAIL}>\n\
             Co-authored-by: Reviewer <reviewer@example.com>\n\
             Harness: newt-agent v0.1.0 (deadbeef) | Model: old | Operator: shawn\n"
        );
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        ledger.record("Model A", "newt-agent", "0.7.6");
        let out = ca.finalize_message_with(&stale, ledger.contributors());
        let coauth: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("Co-authored-by: "))
            .collect();
        // Third-party Reviewer preserved; stale Newt ones gone; fresh set =
        // Model A + active glm-5.2 = 3 total.
        assert_eq!(coauth.len(), 3);
        assert!(coauth.iter().any(|l| l.contains("Reviewer")));
        assert!(
            !coauth
                .iter()
                .any(|l| l.contains("Old Model") || l.contains("Older Model")),
            "stale Newt trailers must all be dropped"
        );
        // Stale provenance replaced with the fresh single line.
        assert!(
            out.contains("Harness: newt-agent v0.8.0 (ba56944bd262-dirty)"),
            "stale provenance replaced"
        );
        assert!(!out.contains("deadbeef"));
    }

    /// Representative B before/after as a golden snapshot: a model-switch
    /// session (Model A → Model B) committing under Model B yields TWO
    /// `Co-authored-by:` trailers — the contract's "ADD a contributor, never
    /// discard" — plus the single Harness provenance, with third-party
    /// trailers preserved.
    #[test]
    fn finalize_with_ledger_before_after_snapshot() {
        let ca = sample_ca(); // active at commit = glm-5.2 / newt-agent v0.8.0
        let mut ledger = AttributionLedger::new(DEFAULT_EMAIL);
        // Model A did earlier work; the active glm-5.2 drives the commit.
        ledger.record("Model A", "newt-agent", "0.7.6");
        let before = "fix the parser";
        let after = ca.finalize_message_with(before, ledger.contributors());
        let expected = format!(
            "fix the parser\n\n\
             Co-authored-by: Model A (newt-agent v0.7.6) <{DEFAULT_EMAIL}>\n\
             Co-authored-by: glm-5.2 (newt-agent v0.8.0) <{DEFAULT_EMAIL}>\n\
             Harness: newt-agent v0.8.0 (ba56944bd262-dirty) | Model: glm-5.2 | Operator: shawn\n"
        );
        assert_eq!(after, expected);
    }
}
