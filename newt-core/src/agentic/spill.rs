//! Tool-output offloading — the `tool_offload` context feature (Step 26.3, #584).
//!
//! When a single tool result exceeds [`TOOL_RESULT_SPILL_CAP`] **and** the
//! feature is on, the FULL payload is redacted via [`redact_secrets`] and stored
//! in a session [`SpillStore`] keyed by a short id; a head+tail excerpt plus a
//! `spill:<id>` handle is injected into context in its place. The model re-reads
//! the full (redacted) payload via `memory_fetch("spill:<id>")`.
//!
//! **Redact-on-store (the security contract):** the raw result is redacted
//! BEFORE anything is stored or shown, and the un-redacted string is dropped
//! immediately after — only the redacted copy is ever retained or displayed, so
//! no raw secret reaches disk-or-context. `redact_secrets` is a closed,
//! high-precision pattern table (it won't catch novel secret shapes — the same
//! accepted limitation as the summarizer path).

use crate::agentic::compress::redact_secrets;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Offload trigger: a tool result longer than this many chars spills. ~4k tokens
/// at the codebase's chars/4 heuristic (cf. `SUMMARY_INPUT_MSG_CAP` = 2_000).
pub const TOOL_RESULT_SPILL_CAP: usize = 16_000;

/// Chars kept from the head / tail of an offloaded payload in the teaser. Kept
/// well under [`TOOL_RESULT_SPILL_CAP`] so the teaser can never re-overflow.
const HEAD_CHARS: usize = 800;
const TAIL_CHARS: usize = 800;

/// A session store for offloaded (already-redacted) tool payloads (Step 26.3).
///
/// Methods take `&self` (interior mutability) so a single shared
/// `&dyn SpillStore` serves BOTH the loop's write path and the `memory_fetch`
/// read path without the `&mut dyn _` reborrow/invariance dance.
pub trait SpillStore: Send + Sync {
    /// Store an already-redacted payload; returns its `spill:` id.
    fn store(&self, redacted: String) -> String;
    /// Fetch a stored payload by id (`None` if unknown / expired).
    fn fetch(&self, id: &str) -> Option<String>;
    /// Number of payloads offloaded this session (for `/context stats`).
    fn spills(&self) -> u64;
    /// Total chars elided from context by offloading (for `/context stats`).
    fn offloaded_chars(&self) -> u64;
}

/// In-memory, session-scoped [`SpillStore`] — pure (no filesystem), discarded at
/// session end / `/new`. Ids are monotonic (`s0`, `s1`, …) so injected handles
/// are deterministic and unit-testable (no uuid, no clock).
#[derive(Default)]
pub struct SessionSpillStore {
    map: Mutex<HashMap<String, String>>,
    counter: AtomicU64,
    offloaded_chars: AtomicU64,
}

impl SpillStore for SessionSpillStore {
    fn store(&self, redacted: String) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("s{n}");
        self.offloaded_chars
            .fetch_add(redacted.chars().count() as u64, Ordering::Relaxed);
        self.map.lock().unwrap().insert(id.clone(), redacted);
        id
    }

    fn fetch(&self, id: &str) -> Option<String> {
        self.map.lock().unwrap().get(id).cloned()
    }

    fn spills(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    fn offloaded_chars(&self) -> u64 {
        self.offloaded_chars.load(Ordering::Relaxed)
    }
}

/// The teaser injected in place of an offloaded payload: head + a re-read marker
/// + tail. Already-redacted input; kept short so it cannot re-overflow.
fn head_tail_excerpt(redacted: &str, id: &str) -> String {
    let chars: Vec<char> = redacted.chars().collect();
    let total = chars.len();
    let head: String = chars.iter().take(HEAD_CHARS).collect();
    let tail: String = chars
        .iter()
        .skip(total.saturating_sub(TAIL_CHARS))
        .collect();
    format!(
        "{head}\n\n[… tool output truncated: {total} chars offloaded. Use \
         memory_fetch(\"spill:{id}\") to read the full (secret-redacted) payload …]\n\n{tail}"
    )
}

/// Offload an oversized tool result (Step 26.3). Returns `result` UNCHANGED when
/// the feature is off, no spill store is provided, or the result is under the
/// cap (the bit-for-bit OFF path). Otherwise redacts → stores → returns a
/// head+tail teaser carrying the `spill:<id>` handle. The raw `result` is
/// consumed and dropped; only its redacted form is retained or shown.
pub fn maybe_offload(result: String, tool_offload: bool, spill: Option<&dyn SpillStore>) -> String {
    let Some(store) = spill else {
        return result;
    };
    if !tool_offload || result.chars().count() <= TOOL_RESULT_SPILL_CAP {
        return result;
    }
    let redacted = redact_secrets(&result);
    let id = store.store(redacted.clone());
    head_tail_excerpt(&redacted, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_round_trips_with_monotonic_ids() {
        let s = SessionSpillStore::default();
        let id0 = s.store("alpha".to_string());
        let id1 = s.store("beta".to_string());
        assert_eq!(id0, "s0");
        assert_eq!(id1, "s1");
        assert_eq!(s.fetch("s0").as_deref(), Some("alpha"));
        assert_eq!(s.fetch("s1").as_deref(), Some("beta"));
        assert_eq!(s.fetch("s99"), None, "unknown id → None, no panic");
        assert_eq!(s.spills(), 2);
        assert_eq!(s.offloaded_chars(), 9); // "alpha"(5) + "beta"(4)
    }

    #[test]
    fn maybe_offload_truth_table() {
        let big = "x".repeat(TOOL_RESULT_SPILL_CAP + 1);

        // (a) feature OFF + over-cap → unchanged, store untouched
        let s = SessionSpillStore::default();
        assert_eq!(maybe_offload(big.clone(), false, Some(&s)), big);
        assert_eq!(s.spills(), 0);

        // (b) no store + over-cap → unchanged, no panic
        assert_eq!(maybe_offload(big.clone(), true, None), big);

        // (c) ON + Some + UNDER cap → unchanged, store untouched
        let s = SessionSpillStore::default();
        let small = "x".repeat(TOOL_RESULT_SPILL_CAP); // == cap is NOT over
        assert_eq!(maybe_offload(small.clone(), true, Some(&s)), small);
        assert_eq!(s.spills(), 0);

        // (d) ON + Some + OVER cap → teaser with spill: handle, shorter, stored
        let s = SessionSpillStore::default();
        let out = maybe_offload(big.clone(), true, Some(&s));
        assert!(
            out.contains("spill:s0"),
            "teaser carries the handle: {out:.80}"
        );
        assert!(out.contains("memory_fetch"), "teaser coaches re-read");
        assert!(
            out.chars().count() < big.chars().count(),
            "teaser is shorter"
        );
        assert_eq!(s.spills(), 1);
        // the STORED value is the redacted full payload (here no secret → == big)
        assert_eq!(s.fetch("s0").as_deref(), Some(big.as_str()));
    }

    #[test]
    fn offload_redacts_before_store_and_in_teaser() {
        // A planted secret in an over-cap payload must never survive raw.
        let secret = "sk-ABCDEFGHIJKLMNOPQRST0123";
        let payload = format!(
            "{}\n{secret}\n{}",
            "head ".repeat(2_000),
            "tail ".repeat(2_000)
        );
        assert!(payload.chars().count() > TOOL_RESULT_SPILL_CAP);
        let s = SessionSpillStore::default();
        let teaser = maybe_offload(payload, true, Some(&s));
        let stored = s.fetch("s0").expect("payload was stored");
        assert!(stored.contains("[REDACTED]"), "stored payload is redacted");
        assert!(
            !stored.contains(secret),
            "raw secret NOT retained in the store"
        );
        assert!(
            !teaser.contains(secret),
            "raw secret NOT shown in the teaser"
        );
    }
}
