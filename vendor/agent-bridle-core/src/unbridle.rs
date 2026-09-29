//! The process-level **unbridled** marker (ADR 0018 D1/D5 — epic #139 I12).
//!
//! Unbridle is the operator's explicit escape hatch: drop the confinement
//! *mechanism* (the L3 OS sandbox) so the agent can use the host's native
//! shell/tools, while the OCAP authority grant (checked by the L2 interceptor,
//! now advisory) and the human step-up gate stay in force. This marker records
//! that decision **once, at startup**, after the loader has verified the two-key
//! acknowledgement (ADR 0018 D3). The tool layer reads it to (a) skip the OS
//! sandbox and (b) stamp `disclosure.unbridled` on every envelope.
//!
//! It is a **process mechanism, not per-invocation authority**: it deliberately
//! does *not* ride [`crate::ToolContext`] (authority≠mechanism, ADR 0017 D1). It
//! defaults to **bridled** and is *never* reachable by omission — only an
//! explicit [`set_unbridled`] from an acked loader path flips it.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The **human-gate posture** an unbridled run still enforces (ADR 0018 D9/D11) —
/// the step-up floor the host will demand for a HIGH-consequence act. Distinguishes
/// the two unbridled postures: `Passkey`/`Prompt` = *Supervised-free* (the human
/// leash remains); `None` = *Autonomous* (no human in the loop — reachable only via
/// the distinct second ack, D10). Disclosed on every envelope (R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HumanGate {
    /// No human gate — the **Autonomous** posture (unbridled *and* no-step-up acked).
    None,
    /// A lightweight interactive confirm.
    Prompt,
    /// A hardware passkey / biometric gesture — the default human gate when
    /// unbridled (the *Supervised-free* posture).
    #[default]
    Passkey,
}

static UNBRIDLED: OnceLock<bool> = OnceLock::new();
static HUMAN_GATE: OnceLock<HumanGate> = OnceLock::new();

/// Mark this process **unbridled** (confinement mechanism off). Call this once,
/// from the loader, and **only** after the acked two-key opt-in has been verified
/// (ADR 0018 D3). Idempotent: a second call is a no-op (the first wins).
///
/// This never widens *authority* — the granted [`crate::Caveats`] are unchanged;
/// it only signals that the L3 mechanism is off so the tool layer runs native and
/// discloses honestly.
pub fn set_unbridled() {
    let _ = UNBRIDLED.set(true);
}

/// Whether this process is running **unbridled**. Defaults to `false` (bridled) —
/// confinement stays on unless an acked loader path explicitly called
/// [`set_unbridled`].
#[must_use]
pub fn is_unbridled() -> bool {
    UNBRIDLED.get().copied().unwrap_or(false)
}

/// Set the process **human-gate posture** (ADR 0018 D10/D11). Call once, from the
/// loader; `HumanGate::None` (Autonomous) is legal **only** when unbridle is also
/// engaged *and* the distinct no-step-up ack was supplied — the loader enforces
/// that before calling this. Idempotent.
pub fn set_human_gate(gate: HumanGate) {
    let _ = HUMAN_GATE.set(gate);
}

/// The process human-gate posture. Defaults to [`HumanGate::Passkey`] — the human
/// leash is assumed **on** unless an acked path explicitly removed it, so the
/// Autonomous posture is never reached by omission.
#[must_use]
pub fn human_gate() -> HumanGate {
    HUMAN_GATE.get().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bridled_then_set_flips_once() {
        // Never unbridled by omission (the fail-closed default).
        assert!(!is_unbridled(), "must default to bridled");
        // The human gate defaults ON (Passkey) — Autonomous is never by omission.
        assert_eq!(human_gate(), HumanGate::Passkey, "human gate defaults on");
        set_unbridled();
        assert!(is_unbridled(), "set_unbridled flips the process marker");
        // Idempotent — a second call does not panic or change the state.
        set_unbridled();
        assert!(is_unbridled());
    }
}
