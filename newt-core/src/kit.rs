//! The **component registry** — the model support kit's catalog of parts.
//!
//! `docs/design/model-support-kit.md`: *the kit* is the catalog of composable
//! support parts (a profile *assembles* a subset of them). This grows the flat
//! [`KNOWN_TECHNIQUES`](crate::config::KNOWN_TECHNIQUES) string list into a typed
//! registry: each part carries the four contract fields the kit needs to compose
//! parts honestly — its **axis**, its **kind** (where it mounts), what it
//! **presupposes**, and its **tier** (does it run headless).
//!
//! This PR is internal — it changes no behavior. It is the substrate the bundle +
//! loadout layers (and `Config::validate`'s presupposes check) build on.

/// Where a component mounts in the harness loop — tells a driver how to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// A system-prompt provider, mounted pre-loop (the `MemoryProvider` seam).
    Provider,
    /// A per-turn post-hook (runs after each turn).
    PerTurn,
    /// A loop-altering technique (re-enters/repeats the turn).
    Loop,
    /// A turn-reshaping mode (e.g. plan → approve → execute).
    Mode,
    /// A pre-send request-knob patch (e.g. `num_ctx`, `reasoning_effort`).
    RequestKnobs,
}

/// Which axis of support a component serves (`docs/design/model-support-kit.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// How the model thinks (`effort`, `think`).
    Reasoning,
    /// How work is decomposed (`plan`).
    Structure,
    /// What the model knows (`knowledge_base`).
    Grounding,
    /// Checking & fixing output (`verify_gate`, `review`, `retry`).
    GatingRepair,
}

/// Whether a component runs in the headless flight tier (`wyvern`, no TUI) or needs
/// the interactive surface — the amphibious split made a *checked* property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Runs anywhere, including headless `wyvern`.
    Headless,
    /// Needs the interactive TUI; skipped (not errored) when headless.
    TuiOnly,
}

/// One row in the [`COMPONENT_REGISTRY`] — a support part and its contract.
#[derive(Debug, Clone, Copy)]
pub struct RegistryEntry {
    /// The part's id — exactly the string a profile lists in `techniques`.
    pub id: &'static str,
    /// Where it mounts.
    pub kind: MountKind,
    /// Which axis it serves.
    pub axis: Axis,
    /// Parts that must also be enabled for this one to be valid (a profile listing
    /// this part without all of these is rejected by `Config::validate`).
    pub presupposes: &'static [&'static str],
    /// Whether it runs headless.
    pub tier: Tier,
}

/// The catalog of parts (the kit). The `id`s are exactly
/// [`KNOWN_TECHNIQUES`](crate::config::KNOWN_TECHNIQUES) — pinned by a test so the
/// two cannot drift.
pub const COMPONENT_REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        id: "knowledge_base", // R1 — inject the authoritative import surface (#74)
        kind: MountKind::Provider,
        axis: Axis::Grounding,
        presupposes: &[],
        tier: Tier::Headless,
    },
    RegistryEntry {
        id: "verify_gate", // R2 — flag/revert files with fabricated imports (#73)
        kind: MountKind::PerTurn,
        axis: Axis::GatingRepair,
        presupposes: &[],
        tier: Tier::Headless,
    },
    RegistryEntry {
        id: "retry", // the revert-retry loop — the gate's action arm
        kind: MountKind::Loop,
        axis: Axis::GatingRepair,
        // retry is verify_gate's action arm: a profile that reverts-on-fabrication
        // declares it is gating. (Operationally retry runs its own gate, so this is a
        // hygiene requirement, not a runtime dependency — see model-support-kit.md.)
        presupposes: &["verify_gate"],
        tier: Tier::Headless,
    },
];

/// Look up a component by its id, or `None` if unknown.
#[must_use]
pub fn component(id: &str) -> Option<&'static RegistryEntry> {
    COMPONENT_REGISTRY.iter().find(|e| e.id == id)
}

/// Whether `id` names a registered component.
#[must_use]
pub fn is_known(id: &str) -> bool {
    component(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KNOWN_TECHNIQUES;

    #[test]
    fn registry_ids_match_known_techniques() {
        let reg: Vec<&str> = COMPONENT_REGISTRY.iter().map(|e| e.id).collect();
        assert_eq!(
            reg, KNOWN_TECHNIQUES,
            "the registry and KNOWN_TECHNIQUES must not drift"
        );
    }

    #[test]
    fn presupposes_reference_real_components() {
        // No part may presuppose an id that isn't itself in the registry.
        for e in COMPONENT_REGISTRY {
            for pre in e.presupposes {
                assert!(
                    is_known(pre),
                    "{} presupposes unknown component {pre}",
                    e.id
                );
            }
        }
    }

    #[test]
    fn lookup_carries_the_contract() {
        let retry = component("retry").expect("retry is registered");
        assert_eq!(retry.kind, MountKind::Loop);
        assert_eq!(retry.axis, Axis::GatingRepair);
        assert_eq!(retry.tier, Tier::Headless);
        assert_eq!(retry.presupposes, &["verify_gate"]);
        assert!(component("nope").is_none());
    }
}
