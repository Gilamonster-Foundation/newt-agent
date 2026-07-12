//! **Nudger profiles** — named bundles of the harness's numeric persistence
//! knobs (the "disposition" of a turn: how hard the tool loop is pushed, how
//! much slack it's given). This module is the *schema + validation* layer; it
//! is pure and IO-free, and — for now — entirely dormant: nothing consumes it
//! yet, so it cannot change runtime behavior.
//!
//! Design: `docs/design/nudger.md`. The shape deliberately mirrors
//! [`crate::model_card`] — all-`Option`/defaultable fields, `deny_unknown_fields`
//! on the *structural* keys, a field-by-field overlay merge, TOML/YAML parsing,
//! and (later, in sibling PRs) `include_str!` seeds + `~/.newt/nudger` drop-ins
//! resolved by merge-by-name.
//!
//! Two rules that are load-bearing:
//!
//! - **The `[knobs]` table is OPEN.** A profile's knobs are a `key -> value`
//!   map, not a fixed struct, so a new harness dimension is *config, not a
//!   schema change*. [`KNOWN_KNOBS`] is the single source of truth mapping a
//!   knob key to where it plugs into per-turn resolution ([`KnobScope`]) and its
//!   valid range. Runtime resolution silently ignores an unknown key
//!   (forward/backward compat); [`NudgerProfile::validate`] surfaces it loudly
//!   (for `nudger validate` / hand-editing).
//! - **`rank` is an OPTIONAL projection.** It is a profile's position on the
//!   nudger ladder (the "effort axis" a future `/effort` projects labels onto).
//!   `None` means the profile is *off the axis*: usable by name, invisible to
//!   up/down/enumeration. This — not a separate "kind" field — is how the
//!   qualitatively-different postures (e.g. a future multi-agent `crew`/`ultra`)
//!   and unranked user drop-ins live off the slider.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where a knob plugs into per-turn resolution — which decides whether a
/// mid-session profile switch actually moves it, and where the resolver reads
/// it. (The resolver itself lands in a later PR; this tag is its wiring map.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobScope {
    /// Folded into the per-model `eff_*` block each turn — a mid-session switch
    /// moves it on the next turn.
    PerModelEff,
    /// Overlaid at `ChatCtx` build (a global-inline budget knob) — also moves
    /// per turn.
    GlobalInline,
    /// Seeded once per session (e.g. a stateful counter). A profile may set it,
    /// but it takes effect only at the *next* session start; a live switch does
    /// not move it.
    SessionOnce,
}

/// A knob the harness understands: its per-turn resolution scope and valid
/// range. Adding a new dimension the harness reads = add a row here **and** wire
/// its scope's resolution site — the profile schema itself never changes.
#[derive(Debug, Clone, Copy)]
pub struct KnownKnob {
    pub key: &'static str,
    pub scope: KnobScope,
    /// Inclusive valid range used by [`NudgerProfile::validate`] for range
    /// warnings. Not enforced at runtime (resolution saturates/skips).
    pub min: i64,
    pub max: i64,
    pub doc: &'static str,
}

/// The registry of knobs a nudger profile may set. Single source of truth for
/// key → (scope, range). Values outside `[min, max]` are a `validate` warning,
/// not a runtime error.
pub const KNOWN_KNOBS: &[KnownKnob] = &[
    KnownKnob {
        key: "max_tool_rounds",
        scope: KnobScope::PerModelEff,
        min: 1,
        max: 10_000,
        doc: "tool-call rounds before the loop stops (10000 = effectively unlimited)",
    },
    KnownKnob {
        key: "workflow_grace_rounds",
        scope: KnobScope::PerModelEff,
        min: 0,
        max: 1_000,
        doc: "extra rounds granted after a workflow reports done",
    },
    KnownKnob {
        key: "narration_nudge_cap",
        scope: KnobScope::PerModelEff,
        min: 0,
        max: 1_000,
        doc: "how many narration nudges the harness will emit in a turn",
    },
    KnownKnob {
        key: "mid_loop_trim_threshold",
        scope: KnobScope::PerModelEff,
        min: 0,
        max: 10_000,
        doc: "round count at which mid-loop context trim triggers (re-clamped to max_tool_rounds-3)",
    },
    KnownKnob {
        key: "mid_loop_trim_tokens",
        scope: KnobScope::PerModelEff,
        min: 0,
        max: 100_000_000,
        doc: "token count that triggers mid-loop trim (0 disables)",
    },
    KnownKnob {
        key: "input_ceiling_pct",
        scope: KnobScope::GlobalInline,
        min: 1,
        max: 100,
        doc: "percent of the context budget usable for input before trimming",
    },
    KnownKnob {
        key: "low_budget_pct",
        scope: KnobScope::GlobalInline,
        min: 1,
        max: 50,
        doc: "budget-remaining percent below which the low-budget nudge fires (consume-clamped to 1..50; 0 cannot disable it)",
    },
    KnownKnob {
        key: "note_nudge_interval",
        scope: KnobScope::SessionOnce,
        min: 0,
        max: 10_000,
        doc: "turns between note nudges (0 = off); session-fixed — takes effect next session",
    },
];

/// Look up a knob by key, if the harness knows it.
pub fn known_knob(key: &str) -> Option<&'static KnownKnob> {
    KNOWN_KNOBS.iter().find(|k| k.key == key)
}

/// A named nudger profile: an optional ladder `rank` plus an open overlay of
/// numeric knob values. The merge / drop-in key is `name`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NudgerProfile {
    /// Profile identity, e.g. `effort-high`. The merge / drop-in key.
    pub name: String,
    /// Position on the nudger ladder — sparse (10, 20, 30…) so profiles can be
    /// inserted between. `None` = OFF the axis (usable by name, invisible to
    /// up/down/enumeration). Named `rank`, deliberately NOT `effort_order`:
    /// `/effort` is a later label-projection onto ranks and must not be welded
    /// into the fundamental.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<i32>,
    /// One-line human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The knob overlay: `key -> value`. OPEN by design — any key is accepted
    /// (extensibility is config, not code). Unknown or out-of-range keys are
    /// tolerated at runtime and flagged by [`validate`](Self::validate).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub knobs: BTreeMap<String, i64>,
}

impl NudgerProfile {
    /// Overlay `o` onto `self`; `o` wins per field and per knob key (the
    /// drop-in / higher-precedence layer overrides). Mirrors the model-card
    /// `.or()` merge, extended with a per-key knob overlay so a partial profile
    /// overrides only the knobs it sets. `name` is the merge key and is kept.
    #[must_use]
    pub fn merge(mut self, o: Self) -> Self {
        self.rank = o.rank.or(self.rank);
        self.description = o.description.or(self.description);
        for (k, v) in o.knobs {
            self.knobs.insert(k, v);
        }
        self
    }

    /// Loud validation for authoring / `nudger validate` — NOT the runtime path
    /// (resolution silently tolerates unknown/out-of-range knobs for
    /// forward-compat). An empty `name` is a hard **error**; unknown knob keys
    /// and out-of-range values are **warnings** (a value valid on a newer newt
    /// build should not hard-fail an older one).
    pub fn validate(&self) -> ValidationReport {
        let mut report = ValidationReport::default();
        if self.name.trim().is_empty() {
            report.errors.push("`name` is empty".to_string());
        }
        for (key, &val) in &self.knobs {
            match known_knob(key) {
                None => report
                    .warnings
                    .push(format!("unknown knob `{key}` (ignored at runtime)")),
                Some(k) if val < k.min || val > k.max => report.warnings.push(format!(
                    "knob `{key}` = {val} is out of range [{}, {}]",
                    k.min, k.max
                )),
                Some(_) => {}
            }
        }
        report
    }
}

/// Parse a nudger profile from TOML or YAML, dispatched by file extension —
/// mirrors [`crate::model_card::parse_card`].
pub fn parse_profile(contents: &str, ext: &str) -> Result<NudgerProfile, String> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "toml" => toml::from_str(contents).map_err(|e| format!("nudger profile TOML: {e}")),
        "yaml" | "yml" => {
            serde_yaml::from_str(contents).map_err(|e| format!("nudger profile YAML: {e}"))
        }
        other => Err(format!(
            "nudger profile: unknown extension `.{other}` (expected .toml / .yaml / .yml)"
        )),
    }
}

/// The outcome of [`NudgerProfile::validate`]: hard `errors` (a malformed
/// profile) vs soft `warnings` (unknown / out-of-range knobs). The command
/// layer decides exit codes; the runtime never calls this.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    /// No hard errors — the profile is structurally usable (warnings may remain).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// No errors AND no warnings — nothing to report at all.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_toml_profile() {
        let toml = r#"
            name = "effort-high"
            rank = 30
            description = "push harder"
            [knobs]
            max_tool_rounds = 40
            workflow_grace_rounds = 5
        "#;
        let p = parse_profile(toml, "toml").expect("valid profile");
        assert_eq!(p.name, "effort-high");
        assert_eq!(p.rank, Some(30));
        assert_eq!(p.description.as_deref(), Some("push harder"));
        assert_eq!(p.knobs.get("max_tool_rounds"), Some(&40));
        assert_eq!(p.knobs.get("workflow_grace_rounds"), Some(&5));
    }

    #[test]
    fn rank_is_optional_off_the_axis() {
        // A profile with no rank is valid and lives off the ladder — this is how
        // crew/ultra and unranked drop-ins work, with no special "kind" field.
        let p = parse_profile(
            "name = \"effort-crew\"\n[knobs]\nmax_tool_rounds = 60\n",
            "toml",
        )
        .expect("valid");
        assert_eq!(p.rank, None);
        assert_eq!(p.knobs.get("max_tool_rounds"), Some(&60));
    }

    #[test]
    fn deny_unknown_fields_catches_a_structural_typo() {
        // `effort_order` is a typo of `rank`; a top-level unknown key is a hard
        // parse error (loud, by design) — the model-card discipline.
        let err = parse_profile("name = \"x\"\neffort_order = 10\n", "toml").unwrap_err();
        assert!(err.contains("nudger profile TOML"), "got: {err}");
    }

    #[test]
    fn the_knobs_table_is_open() {
        // A key inside [knobs] that the harness does not know still PARSES — the
        // map is open; the loudness is deferred to validate().
        let p = parse_profile("name = \"x\"\n[knobs]\nsome_future_knob = 7\n", "toml")
            .expect("open map accepts any knob key");
        assert_eq!(p.knobs.get("some_future_knob"), Some(&7));
    }

    #[test]
    fn merge_overlays_per_knob_key_and_field() {
        let base = parse_profile(
            "name = \"base\"\nrank = 10\n[knobs]\nmax_tool_rounds = 20\nnarration_nudge_cap = 3\n",
            "toml",
        )
        .unwrap();
        let overlay = parse_profile(
            "name = \"base\"\nrank = 15\n[knobs]\nmax_tool_rounds = 40\n",
            "toml",
        )
        .unwrap();
        let merged = base.merge(overlay);
        assert_eq!(merged.rank, Some(15), "overlay rank wins");
        assert_eq!(
            merged.knobs.get("max_tool_rounds"),
            Some(&40),
            "overlay knob wins"
        );
        assert_eq!(
            merged.knobs.get("narration_nudge_cap"),
            Some(&3),
            "base-only knob survives"
        );
    }

    #[test]
    fn validate_empty_name_is_a_hard_error() {
        let p = NudgerProfile::default();
        let r = p.validate();
        assert!(!r.is_ok(), "empty name must be an error");
        assert!(r.errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn validate_unknown_and_out_of_range_knobs_are_warnings() {
        let p = parse_profile(
            "name = \"x\"\n[knobs]\nbogus_knob = 1\nmax_tool_rounds = 0\n",
            "toml",
        )
        .unwrap();
        let r = p.validate();
        assert!(r.is_ok(), "warnings are not hard errors");
        assert!(!r.is_clean(), "there are warnings");
        assert!(r.warnings.iter().any(|w| w.contains("bogus_knob")));
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("max_tool_rounds") && w.contains("range")),
            "0 is below max_tool_rounds min of 1"
        );
    }

    #[test]
    fn a_clean_profile_reports_clean() {
        let p = parse_profile(
            "name = \"effort-medium\"\nrank = 20\n[knobs]\nmax_tool_rounds = 20\n",
            "toml",
        )
        .unwrap();
        assert!(p.validate().is_clean());
    }

    #[test]
    fn known_knobs_carry_scope_tags() {
        assert_eq!(
            known_knob("max_tool_rounds").unwrap().scope,
            KnobScope::PerModelEff
        );
        assert_eq!(
            known_knob("low_budget_pct").unwrap().scope,
            KnobScope::GlobalInline
        );
        assert_eq!(
            known_knob("note_nudge_interval").unwrap().scope,
            KnobScope::SessionOnce
        );
        assert!(known_knob("not_a_knob").is_none());
    }
}
