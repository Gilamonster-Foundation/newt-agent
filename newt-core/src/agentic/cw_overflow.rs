//! Context-window overflow detection + a last-resort recovery cap for the
//! agentic loop.
//!
//! Some backends reject an over-long prompt with a hard error the loop's
//! pre-send budget can't pre-empt: llama.cpp's OpenAI-compatible endpoint
//! returns `500 {"message":"Context size has been exceeded."}` — a **numberless**
//! body, so [`newt-tui`'s numbered `parse_context_window_error`] (which reads
//! the litellm `prompt is too long: N > M` form) matches nothing, and no
//! recovery fires. The failure is real: a single capped tool result appended
//! *after* the round's preflight can push the request past the server's
//! `--ctx-size`, and estimation slack (chars/4) hides it from the gate.
//!
//! This module supplies the missing piece: [`is_context_overflow`] recognizes
//! the overflow (numbered OR numberless), and [`core_recover_overflow`] derives
//! a tightened input cap to compress toward when the error carries no parseable
//! limit — feeding the loop's existing compress-and-retry machinery (mod.rs
//! OpenAI/Ollama recovery sites) so the turn self-heals instead of dying. It is
//! the headless counterpart to the interactive `recover_cw_400` fn-pointer, and
//! it also repairs the interactive TUI against llama.cpp's numberless body.
//!
//! Detection is deliberately TIGHT — only the two known overflow phrases — so an
//! unrelated 5xx that happens to echo the string can't lose its normal retry.

/// The stable anchor phrases that mark a context-window overflow, across the
/// backends newt drives. Kept as data (the three-Cs "knowledge in data" rule)
/// so a new backend's phrasing is one array entry, not a new branch.
///
/// - llama.cpp (`/v1/chat/completions`, server-side `--ctx-size`): numberless.
/// - litellm / OpenAI-compatible proxies (issue #223): the numbered form, whose
///   limits [`newt-tui`'s `parse_context_window_error`] extracts.
const OVERFLOW_PHRASES: [&str; 2] = ["Context size has been exceeded", "prompt is too long:"];

/// A sane floor for a derived cap — never compress toward less than this many
/// input tokens (the system/card floor is already irreducible below it).
const MIN_DERIVED_CAP: u32 = 1024;

/// The fraction (percent) of the known budget to shrink to on an overflow —
/// reserve headroom so the retried request clears the window with margin.
const SHRINK_PCT: u64 = 80;

/// Does `msg` look like a context-window overflow error from any backend newt
/// drives? Matches the numbered (litellm) and numberless (llama.cpp) bodies;
/// returns `false` for every other error so their normal retry is preserved.
pub fn is_context_overflow(msg: &str) -> bool {
    OVERFLOW_PHRASES.iter().any(|p| msg.contains(p))
}

/// Derive a tightened input-token cap to compress toward when a context overflow
/// has **no parseable server limit** (llama.cpp's numberless body). Returns
/// `None` when the error is not an overflow, or when no budget is known to
/// shrink from — there is nothing to derive a cap from, so the caller keeps its
/// existing (non-recovering) behavior rather than inventing a number.
///
/// When a budget IS known, shrink to [`SHRINK_PCT`]% of the smallest known
/// ceiling (`send_budget` = the current input budget; `num_ctx_ceiling` = the
/// per-request `num_ctx` ceiling on wires that send one), but never below
/// [`MIN_DERIVED_CAP`] (a request can't be made viably smaller than the
/// irreducible system/card head).
///
/// The returned cap is ALWAYS strictly smaller than the budget that just
/// overflowed — so the caller, which stores it back into `send_budget`, tightens
/// monotonically. When even the floored value can't stay below `base` (i.e.
/// `base <= MIN_DERIVED_CAP`), there is no smaller viable budget to retry with,
/// so return `None` and let the error surface — rather than RAISE the budget to
/// the floor (which would enlarge the prompt after an overflow) or stall at a
/// no-progress fixed point.
pub fn core_recover_overflow(
    msg: &str,
    send_budget: Option<usize>,
    num_ctx_ceiling: Option<usize>,
) -> Option<u32> {
    if !is_context_overflow(msg) {
        return None;
    }
    let base = match (send_budget, num_ctx_ceiling) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    // Shrink to SHRINK_PCT% of the overflowed budget, but not below the floor.
    // saturating_mul guards the (unreachable) u64 overflow on a huge budget.
    let derived = ((base as u64).saturating_mul(SHRINK_PCT) / 100).max(MIN_DERIVED_CAP as u64);
    // The cap must STRICTLY tighten; if flooring pushed it up to/over `base`
    // there is nothing smaller to try — give up instead of raising the budget.
    if derived >= base as u64 {
        return None;
    }
    // Saturate rather than wrap for the (unreachable) multi-billion-token budget.
    Some(derived.min(u32::MAX as u64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_llamacpp_numberless_overflow() {
        // The exact dgx1 llama.cpp body observed on Terminal-Bench, wrapped in
        // the harness's `inference endpoint 500: <body>` string.
        let msg = r#"inference endpoint 500 Internal Server Error: {"error":{"code":500,"message":"Context size has been exceeded.","type":"server_error"}}"#;
        assert!(is_context_overflow(msg));
    }

    #[test]
    fn detects_litellm_numbered_overflow() {
        let msg = "inference endpoint 400: litellm.ContextWindowExceededError: prompt is too long: 5960028 tokens > 1000000 maximum";
        assert!(is_context_overflow(msg));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!is_context_overflow(
            "inference endpoint 400: invalid api key"
        ));
        assert!(!is_context_overflow(
            "inference endpoint 503 Service Unavailable"
        ));
        assert!(!is_context_overflow("request failed: connection reset"));
    }

    #[test]
    fn recovery_none_when_not_overflow() {
        assert_eq!(
            core_recover_overflow("invalid api key", Some(24_000), Some(24_000)),
            None
        );
    }

    #[test]
    fn recovery_shrinks_to_80pct_of_send_budget() {
        // llama.cpp numberless overflow, send_budget seeded at 80% of 32768.
        let msg = r#"{"message":"Context size has been exceeded."}"#;
        // 24_000 * 80 / 100 = 19_200.
        assert_eq!(core_recover_overflow(msg, Some(24_000), None), Some(19_200));
    }

    #[test]
    fn recovery_uses_the_smaller_of_budget_and_ceiling() {
        let msg = "Context size has been exceeded";
        // min(24_000, 20_000) = 20_000; *80% = 16_000.
        assert_eq!(
            core_recover_overflow(msg, Some(24_000), Some(20_000)),
            Some(16_000)
        );
        // Ceiling smaller the other way is symmetric.
        assert_eq!(
            core_recover_overflow(msg, Some(18_000), Some(30_000)),
            Some(14_400)
        );
    }

    #[test]
    fn recovery_pins_to_floor_just_above_it_then_gives_up() {
        let msg = "Context size has been exceeded";
        // Just above the floor band: 80% would dip under MIN_DERIVED_CAP, so the
        // cap is pinned to the floor — still a STRICT tighten (1024 < base),
        // never a raise. (Regression guard: the earlier `.max()` form and the
        // first "return None below the floor" form both got this band wrong.)
        assert_eq!(core_recover_overflow(msg, Some(1_279), None), Some(1_024));
        assert_eq!(core_recover_overflow(msg, Some(1_280), None), Some(1_024));
        assert_eq!(core_recover_overflow(msg, Some(1_025), None), Some(1_024));
        // At/below the floor there is no smaller viable budget → give up (None),
        // NEVER raise to 1024 (the cursor-bugbot case) or stall at a fixed point.
        assert_eq!(core_recover_overflow(msg, Some(1_024), None), None);
        assert_eq!(core_recover_overflow(msg, Some(100), None), None);
        assert_eq!(core_recover_overflow(msg, Some(0), None), None);
    }

    #[test]
    fn recovery_never_raises_the_budget() {
        // Invariant: every returned cap is STRICTLY less than the overflowed
        // budget and at least the floor — a recovery may only tighten.
        let msg = "Context size has been exceeded";
        for base in [
            1_025usize,
            1_280,
            2_000,
            8_000,
            24_000,
            100_000,
            u32::MAX as usize,
        ] {
            if let Some(cap) = core_recover_overflow(msg, Some(base), None) {
                assert!((cap as usize) < base, "cap {cap} !< base {base}");
                assert!(cap >= 1_024, "cap {cap} below floor for base {base}");
            }
        }
    }

    #[test]
    fn recovery_none_when_no_budget_known() {
        // An overflow with nothing to shrink from → no invented cap.
        let msg = "Context size has been exceeded";
        assert_eq!(core_recover_overflow(msg, None, None), None);
    }

    #[test]
    fn recovery_monotonically_tightens_on_repeat() {
        let msg = "Context size has been exceeded";
        let first = core_recover_overflow(msg, Some(24_000), None).unwrap();
        // Caller stores `first` back into send_budget; the next overflow shrinks
        // again from there — strictly smaller each time.
        let second = core_recover_overflow(msg, Some(first as usize), None).unwrap();
        assert!(second < first, "{second} !< {first}");
    }
}
