//! The step-up policy the crew `attest` decision reads: the presence ladder, a requirement, and
//! selector rules where the longest match wins.
//!
//! Copied from agent-bridle-core 0.8.0-rc.7 (`vendor/agent-bridle-core/src/step_up.rs`) so the
//! crew decision needs no confinement backend (#3160, the `confine` feature). The items and both tests
//! in `tests` are bridle's lines; the only edits are three doc links to items left behind, now
//! plain code spans. Left in bridle because nothing here calls them: the discharge, challenge,
//! attestation and verifier types; `CallRequest::unspecified` and `content_id` (with `ContentId`
//! and its hand-rolled canonical JSON); `AttestRequirement::passkey_recorded`;
//! `StepUpPolicy::EMPTY` and its `Default`. The crew decision reads only `presence`; `record`
//! and `freshness_generations` ride along so the shape stays bridle's. `step_up_parity_tests`
//! holds this copy equal to bridle's while bridle is a dependency.

use serde::{Deserialize, Serialize};

/// The strength of human gesture an action demands, weakest to strongest.
///
/// The ordering is **load-bearing**: a discharge satisfies a requirement iff its
/// presence is `>=` the required presence (a `Passkey` over-satisfies a
/// `Prompt`; a `Prompt` never satisfies a `Passkey`). Because attenuation may
/// only *raise* a required presence, this keeps "you can get more restrictive,
/// never less" true for the presence axis too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// No human gesture required.
    #[default]
    None,
    /// A soft prompt: any UI affirmation (a typed "yes", a click). Advisory —
    /// it proves a human *chose*, not *who*. (Charter: Tether.)
    Prompt,
    /// A hardware human gesture: a WebAuthn/FIDO2 user-presence (and optional
    /// user-verification) assertion from an authenticator the human controls.
    Passkey,
}

/// The action a leash decision is about: a tool name, its arguments, and the
/// resolved resource the policy keys on.
///
/// **Resolve before constructing.** A `resource` (and any path-bearing `args`)
/// must already be canonicalized (realpath, normalized refspec) so the human
/// approves the *resolved* effect, not a `..`-bearing alias.
#[derive(Debug, Clone)]
pub struct CallRequest {
    /// The dispatch name of the tool (e.g. `git.push`, `email.send`).
    pub tool: String,
    /// The tool arguments (the MCP `arguments` object).
    pub args: serde_json::Value,
    /// The resolved, policy-relevant resource (e.g. `github.com/org/repo`,
    /// a realpath, a recipient set).
    pub resource: String,
}

impl CallRequest {
    /// Construct a request.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        args: serde_json::Value,
        resource: impl Into<String>,
    ) -> Self {
        Self {
            tool: tool.into(),
            args,
            resource: resource.into(),
        }
    }
}

/// What step-up an action demands before the gate will mint a context for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestRequirement {
    /// Minimum gesture strength this action demands.
    pub presence: Presence,
    /// Whether a verified gesture must be recorded as a provenance
    /// `Attestation` (the `passkey+record` policy decision).
    pub record: bool,
    /// Maximum age, in causal generations (never wall-clock), a discharge may
    /// have: the gate accepts a discharge bound to any generation in
    /// `[current - freshness_generations, current]`. `0` ⇒ it must be bound to
    /// the current generation (fresh-per-act, HIGH-consequence); `N` ⇒ a gesture
    /// may be reused for up to `N` generations (LOW-consequence amortization).
    /// Enforced in `Gate::authorize_with_discharge` by recomputing
    /// the bound `Challenge` across the window, combined with single-use
    /// consumption so one gesture authorizes exactly one act (ADR 0007 D4). The
    /// window is capped defensively (fail-closed) to bound the scan.
    pub freshness_generations: u64,
}

impl AttestRequirement {
    /// The empty requirement: no gesture, no record. The base-case for actions
    /// with no step-up policy.
    pub const NONE: Self = Self {
        presence: Presence::None,
        record: false,
        freshness_generations: 0,
    };

    /// A requirement for the given presence (no recording, current-generation
    /// freshness).
    #[must_use]
    pub fn presence(presence: Presence) -> Self {
        Self {
            presence,
            record: false,
            freshness_generations: 0,
        }
    }

    /// Does this action demand any gesture at all?
    #[must_use]
    pub fn demands_gesture(&self) -> bool {
        self.presence > Presence::None
    }
}

/// One policy rule mapping an action selector to a required step-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// `"<tool>"` or `"<tool>:<resource-glob>"`. The glob supports a single
    /// trailing `*` (or `**`) meaning prefix-match.
    pub selector: String,
    /// The step-up this rule demands when it matches.
    pub requirement: AttestRequirement,
}

impl Rule {
    fn matches(&self, request: &CallRequest) -> bool {
        let (tool, resource_glob) = match self.selector.split_once(':') {
            Some((tool, glob)) => (tool, Some(glob)),
            None => (self.selector.as_str(), None),
        };
        if tool != request.tool {
            return false;
        }
        match resource_glob {
            None => true,
            Some(glob) => glob_prefix_match(glob, &request.resource),
        }
    }
}

/// Exact match, or prefix-match when `pattern` ends in `*` / `**`.
fn glob_prefix_match(pattern: &str, text: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        let prefix = prefix.strip_suffix('*').unwrap_or(prefix);
        text.starts_with(prefix)
    } else {
        pattern == text
    }
}

/// The per-action step-up policy: a set of selector rules plus a fall-through
/// default. Most-specific (longest matching selector) wins. This is the
/// authoring surface behind the operator menu (*yes once / yes always / yes on
/// passkey / no*); it composes *on top of* the `Caveats` grant — `Caveats`
/// decides whether the authority exists, this decides what gesture admits its
/// use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpPolicy {
    /// Selector rules, evaluated most-specific-wins.
    pub rules: Vec<Rule>,
    /// The requirement for any action no rule matches.
    pub default: AttestRequirement,
}

impl StepUpPolicy {
    /// A policy with the given rules and default.
    #[must_use]
    pub fn new(rules: Vec<Rule>, default: AttestRequirement) -> Self {
        Self { rules, default }
    }

    /// The strongest requirement matching `request` (longest selector wins), or
    /// the policy default when none match.
    #[must_use]
    pub fn required_for(&self, request: &CallRequest) -> AttestRequirement {
        let mut best: Option<&Rule> = None;
        for rule in &self.rules {
            if rule.matches(request) && best.is_none_or(|b| rule.selector.len() > b.selector.len())
            {
                best = Some(rule);
            }
        }
        best.map_or_else(|| self.default.clone(), |r| r.requirement.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_is_totally_ordered_none_prompt_passkey() {
        assert!(Presence::None < Presence::Prompt);
        assert!(Presence::Prompt < Presence::Passkey);
    }

    #[test]
    fn policy_most_specific_wins_and_default_applies() {
        let policy = StepUpPolicy::new(
            vec![
                Rule {
                    selector: "fs.delete:/tmp/*".to_string(),
                    requirement: AttestRequirement::presence(Presence::Prompt),
                },
                Rule {
                    selector: "fs.delete:/tmp/important/*".to_string(),
                    requirement: AttestRequirement::presence(Presence::Passkey),
                },
            ],
            AttestRequirement::NONE,
        );
        // Longer selector wins for the nested path.
        let nested = CallRequest::new("fs.delete", serde_json::Value::Null, "/tmp/important/x");
        assert_eq!(policy.required_for(&nested).presence, Presence::Passkey);
        // Broad path matches only the short rule.
        let broad = CallRequest::new("fs.delete", serde_json::Value::Null, "/tmp/scratch");
        assert_eq!(policy.required_for(&broad).presence, Presence::Prompt);
        // Unmatched tool falls through to the default.
        let other = CallRequest::new("email.read", serde_json::Value::Null, "inbox");
        assert_eq!(policy.required_for(&other).presence, Presence::None);
    }
}

#[cfg(all(test, feature = "confine"))]
#[path = "step_up_parity_tests.rs"]
mod parity_tests;
