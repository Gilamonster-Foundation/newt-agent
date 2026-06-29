//! Static model registry + the strongest-fitting-model selection (issue #709,
//! PR1 — the read-only foundation for hardware-aware DGX deployment).
//!
//! This carries **model identities only** — names, weight formats, approximate
//! footprints, the serving tool, and a relative quality ranking. It deliberately
//! holds **no** host/IP/GPU/DNS specifics (this is a public repo); the node
//! address is resolved from the operator's local `[dgx]` config at runtime.
//!
//! Everything here is pure: a const table and a pure selection function over it.
//! Fully unit-tested, no IO. Pattern copied from `dgx_pull.rs` (pure, fully
//! mocked, fs-free).
//!
//! THREE-CS REFACTOR CANDIDATE: [`REGISTRY`] hardcodes domain knowledge (model
//! names, footprints, tools, quality). Per the repo's three-Cs rule (working
//! code first, then de-hardcode into pure-data **C**omposition / **C**onfiguration
//! / **C**onvention), this table should later become a droppable, override-able
//! `~/.newt/models/*.toml` config merged by name — the language-pack pattern
//! (`newt-core/src/api_surface.rs`). [`select_strongest`] already takes the table
//! as an argument (the composition seam), so swapping the source is a config
//! change, not a logic change. Hardcoded now to ship a working selector.

/// The inference runtime a variant is served with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceTool {
    /// Ollama (GGUF, single-node convenience runtime).
    Ollama,
    /// vLLM (OpenAI-compatible server; FP8/FP16 + multi-node tensor-parallel).
    Vllm,
    /// llama.cpp (GGUF, experimental large-model single-node path).
    LlamaCpp,
}

impl InferenceTool {
    /// Stable lowercase token (for display / JSON / config round-trips).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::LlamaCpp => "llama_cpp",
        }
    }
}

/// A known model variant: an identity plus its memory footprint and how it is
/// served. IDENTITIES ONLY — see the module docs on the public-repo constraint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelVariant {
    /// Model identity, e.g. `Ornith-1.0-35B`.
    pub name: &'static str,
    /// Weight format / quantization, e.g. `Q4_GGUF`, `FP8`, `FP16`, `Q2_K_GGUF`.
    pub format: &'static str,
    /// Approximate resident footprint in GiB.
    pub gib: f64,
    /// Runtime used to serve this variant.
    pub tool: InferenceTool,
    /// Relative quality ranking; **higher is stronger**. It combines parameter
    /// count (dominant) with format fidelity (`fp16 > fp8 > q4 > q2`), so the
    /// "strongest model that fits the budget" is the biggest one at its best
    /// affordable quantization. This is the sort key in [`select_strongest`].
    pub quality_score: u32,
}

/// Fraction of the raw memory budget that is usable for model weights; the rest
/// (15%) is headroom for the KV-cache, activations, and the OS. Issue #709 §6.
pub const HEADROOM_FACTOR: f64 = 0.85;

/// The static model registry (issue #709 §2). Hardcoded — see the module-level
/// THREE-CS REFACTOR CANDIDATE note. Quality scores are assigned so that a
/// larger model always outranks a smaller one, and within a model family the
/// format ordering `fp16 > fp8 > q4 > q2` holds.
pub const REGISTRY: &[ModelVariant] = &[
    ModelVariant {
        name: "Ornith-1.0-35B",
        format: "Q4_GGUF",
        gib: 21.0,
        tool: InferenceTool::Ollama,
        quality_score: 20,
    },
    ModelVariant {
        name: "Ornith-1.0-35B",
        format: "FP8",
        gib: 35.0,
        tool: InferenceTool::Vllm,
        quality_score: 25,
    },
    ModelVariant {
        name: "Ornith-1.0-35B",
        format: "FP16",
        gib: 70.0,
        tool: InferenceTool::Vllm,
        quality_score: 30,
    },
    ModelVariant {
        name: "Ornith-1.0-397B",
        format: "Q2_K_GGUF",
        gib: 104.0,
        tool: InferenceTool::LlamaCpp,
        quality_score: 50,
    },
    ModelVariant {
        name: "Ornith-1.0-397B",
        format: "Q4_GGUF",
        gib: 200.0,
        tool: InferenceTool::Vllm,
        quality_score: 55,
    },
    ModelVariant {
        name: "Ornith-1.0-397B",
        format: "FP8",
        gib: 400.0,
        tool: InferenceTool::Vllm,
        quality_score: 60,
    },
];

/// The usable budget in GiB after reserving [`HEADROOM_FACTOR`] headroom across
/// `node_count` nodes. Pure.
pub fn usable_budget_gib(budget_gib: f64, node_count: u32) -> f64 {
    budget_gib * f64::from(node_count) * HEADROOM_FACTOR
}

/// The strongest model in [`REGISTRY`] whose footprint fits the usable budget
/// across `node_count` nodes (issue #709 §6). `None` when nothing fits.
///
/// Convenience wrapper over [`select_strongest`] bound to the static registry.
pub fn strongest_model(budget_gib: f64, node_count: u32) -> Option<&'static ModelVariant> {
    select_strongest(REGISTRY, budget_gib, node_count)
}

/// The strongest fitting variant from an arbitrary `table` (the composition seam
/// for tests and the future three-Cs config source).
///
/// Filters by `gib <= budget_gib * node_count * HEADROOM_FACTOR`, then returns
/// the highest [`ModelVariant::quality_score`]. Pure; `None` when nothing fits
/// (including `node_count == 0` or an empty table).
pub fn select_strongest(
    table: &[ModelVariant],
    budget_gib: f64,
    node_count: u32,
) -> Option<&ModelVariant> {
    let usable = usable_budget_gib(budget_gib, node_count);
    table
        .iter()
        .filter(|v| v.gib <= usable)
        .max_by(|a, b| a.quality_score.cmp(&b.quality_score))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- registry well-formedness -------------------------------------

    #[test]
    fn registry_is_nonempty() {
        assert!(!REGISTRY.is_empty());
    }

    #[test]
    fn registry_entries_are_well_formed() {
        for v in REGISTRY {
            assert!(!v.name.is_empty(), "empty name: {v:?}");
            assert!(!v.format.is_empty(), "empty format: {v:?}");
            assert!(v.gib > 0.0, "non-positive gib: {v:?}");
            assert!(v.quality_score > 0, "non-positive quality: {v:?}");
            // Model identities only — never a host/IP/GPU here.
            assert!(v.name.starts_with("Ornith"), "unexpected identity: {v:?}");
        }
    }

    #[test]
    fn registry_quality_scores_are_unique() {
        let mut scores: Vec<u32> = REGISTRY.iter().map(|v| v.quality_score).collect();
        scores.sort_unstable();
        scores.dedup();
        assert_eq!(
            scores.len(),
            REGISTRY.len(),
            "quality scores must be unique"
        );
    }

    #[test]
    fn inference_tool_tokens_are_stable() {
        assert_eq!(InferenceTool::Ollama.as_str(), "ollama");
        assert_eq!(InferenceTool::Vllm.as_str(), "vllm");
        assert_eq!(InferenceTool::LlamaCpp.as_str(), "llama_cpp");
    }

    // --- usable budget / headroom -------------------------------------

    #[test]
    fn usable_budget_applies_headroom_and_nodes() {
        // 100 GiB on a single node → 85 GiB usable.
        assert!((usable_budget_gib(100.0, 1) - 85.0).abs() < 1e-9);
        // Three nodes sum, then 15% headroom: 100 * 3 * 0.85 = 255.
        assert!((usable_budget_gib(100.0, 3) - 255.0).abs() < 1e-9);
    }

    #[test]
    fn usable_budget_zero_nodes_is_zero() {
        assert_eq!(usable_budget_gib(128.0, 0), 0.0);
    }

    // --- selection ----------------------------------------------------

    #[test]
    fn small_budget_picks_smallest_fitting() {
        // 25 GiB, 1 node → 21.25 usable → only the 21 GiB Q4 fits.
        let v = strongest_model(25.0, 1).expect("Q4 should fit");
        assert_eq!(v.name, "Ornith-1.0-35B");
        assert_eq!(v.format, "Q4_GGUF");
        assert_eq!(v.tool, InferenceTool::Ollama);
    }

    #[test]
    fn mid_budget_picks_best_format_of_fitting_family() {
        // 85 GiB, 1 node → 72.25 usable → 21/35/70 fit, 104 does not.
        // The strongest of those is the 35B FP16 (70 GiB).
        let v = strongest_model(85.0, 1).expect("FP16 should fit");
        assert_eq!(v.name, "Ornith-1.0-35B");
        assert_eq!(v.format, "FP16");
    }

    #[test]
    fn large_budget_many_nodes_picks_strongest_overall() {
        // 200 GiB across 3 nodes → 510 usable → everything fits → the 397B FP8
        // (the largest/strongest) wins. This is the "when we get rich" path.
        let v = strongest_model(200.0, 3).expect("everything fits");
        assert_eq!(v.name, "Ornith-1.0-397B");
        assert_eq!(v.format, "FP8");
        assert_eq!(v.quality_score, 60);
    }

    #[test]
    fn headroom_rejects_a_model_that_would_fit_without_it() {
        // 22 GiB raw >= 21 GiB (would fit with no headroom), but 22 * 0.85 =
        // 18.7 < 21 → nothing fits. Proves the 15% headroom is actually applied.
        assert!(strongest_model(22.0, 1).is_none());
    }

    #[test]
    fn headroom_boundary_just_fits_and_just_misses() {
        // Just under: 24 * 0.85 = 20.4 < 21 → nothing fits.
        assert!(strongest_model(24.0, 1).is_none());
        // Just over: 24.8 * 0.85 = 21.08 >= 21 → the Q4 fits.
        let v = strongest_model(24.8, 1).expect("just over the boundary");
        assert_eq!(v.format, "Q4_GGUF");
    }

    #[test]
    fn nothing_fits_returns_none() {
        // Below the smallest model's headroom-adjusted need.
        assert!(strongest_model(10.0, 1).is_none());
    }

    #[test]
    fn zero_nodes_returns_none() {
        assert!(strongest_model(1000.0, 0).is_none());
    }

    #[test]
    fn empty_table_returns_none() {
        assert!(select_strongest(&[], 1000.0, 10).is_none());
    }

    #[test]
    fn select_strongest_is_pure_over_a_custom_table() {
        // Composition seam: selection works over any table, proving it does not
        // depend on the static REGISTRY.
        let table = [
            ModelVariant {
                name: "tiny",
                format: "Q4",
                gib: 1.0,
                tool: InferenceTool::Ollama,
                quality_score: 1,
            },
            ModelVariant {
                name: "huge",
                format: "FP16",
                gib: 1000.0,
                tool: InferenceTool::Vllm,
                quality_score: 99,
            },
        ];
        // Budget fits only "tiny".
        let v = select_strongest(&table, 2.0, 1).expect("tiny fits");
        assert_eq!(v.name, "tiny");
        // Budget fits both → the higher quality_score wins.
        let v = select_strongest(&table, 2000.0, 1).expect("both fit");
        assert_eq!(v.name, "huge");
    }
}
