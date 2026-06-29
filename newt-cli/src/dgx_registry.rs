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

// ---------------------------------------------------------------------------
// Deploy convention: slug + script name (issue #709 §4, Mode A)
// ---------------------------------------------------------------------------

/// The size token from a model name: the final `-`-delimited component,
/// lowercased — `Ornith-1.0-35B` → `35b`, `Ornith-1.0-397B` → `397b`. Pure.
fn size_token(name: &str) -> String {
    name.rsplit('-').next().unwrap_or(name).to_ascii_lowercase()
}

/// The format token from a weight format: the part before the first `_`,
/// lowercased — `Q4_GGUF` → `q4`, `Q2_K_GGUF` → `q2`, `FP8` → `fp8`. Pure.
fn format_token(format: &str) -> String {
    format
        .split('_')
        .next()
        .unwrap_or(format)
        .to_ascii_lowercase()
}

/// The convention slug for a variant — `ornith-{size}-{format}`, e.g.
/// `Ornith-1.0-35B` + `FP8` → `ornith-35b-fp8`. This is the token accepted by
/// `newt dgx deploy <slug>` and the basename (minus `~/` and `.sh`) of the
/// convention deploy script. Pure; derived from identity only (no host).
///
/// THREE-CS NOTE: this encodes the `ornith-{size}-{format}` script-naming
/// **Convention** (one of the three Cs). It lives with the identity it derives
/// from; a future config source would instead carry an explicit `script` field
/// per variant, dropping this derivation.
pub fn variant_slug(v: &ModelVariant) -> String {
    format!("ornith-{}-{}", size_token(v.name), format_token(v.format))
}

/// The convention deploy-script path for a variant: `~/{slug}.sh`, e.g.
/// `~/ornith-35b-fp8.sh` (issue #709 §4, Mode A). Pure. The leading `~` is a
/// shell convention expanded **on the node**, never a resolved home path here
/// (and so this leaks no host/user).
pub fn script_name(v: &ModelVariant) -> String {
    format!("~/{}.sh", variant_slug(v))
}

/// Find a registry variant by its convention slug (case-insensitive), e.g.
/// `ornith-35b-fp16`. `None` when no variant matches. Pure — the lookup table
/// for `newt dgx deploy <slug>`.
pub fn find_variant(slug: &str) -> Option<&'static ModelVariant> {
    let want = slug.trim().to_ascii_lowercase();
    REGISTRY.iter().find(|v| variant_slug(v) == want)
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

    // --- deploy convention: slug / script_name / find_variant --------

    #[test]
    fn variant_slug_follows_the_convention() {
        // The issue's worked example: Ornith-1.0-35B + FP8 → ornith-35b-fp8.
        let fp8 = REGISTRY
            .iter()
            .find(|v| v.name == "Ornith-1.0-35B" && v.format == "FP8")
            .expect("35B FP8 present");
        assert_eq!(variant_slug(fp8), "ornith-35b-fp8");
    }

    #[test]
    fn variant_slug_normalizes_size_and_format_tokens() {
        // size token = last `-` component lowercased; format token = part before
        // the first `_` lowercased (so the GGUF/_K suffixes are dropped).
        let cases = [
            ("Ornith-1.0-35B", "Q4_GGUF", "ornith-35b-q4"),
            ("Ornith-1.0-35B", "FP16", "ornith-35b-fp16"),
            ("Ornith-1.0-397B", "Q2_K_GGUF", "ornith-397b-q2"),
            ("Ornith-1.0-397B", "FP8", "ornith-397b-fp8"),
        ];
        for (name, format, want) in cases {
            let v = ModelVariant {
                name,
                format,
                gib: 1.0,
                tool: InferenceTool::Vllm,
                quality_score: 1,
            };
            assert_eq!(variant_slug(&v), want, "{name} {format}");
        }
    }

    #[test]
    fn every_registry_slug_is_unique() {
        // The slug is the deploy lookup key — collisions would make
        // `find_variant` ambiguous.
        let mut slugs: Vec<String> = REGISTRY.iter().map(variant_slug).collect();
        let count = slugs.len();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "registry slugs must be unique");
    }

    #[test]
    fn script_name_is_the_tilde_convention_path() {
        let fp8 = REGISTRY
            .iter()
            .find(|v| v.name == "Ornith-1.0-35B" && v.format == "FP8")
            .expect("35B FP8 present");
        assert_eq!(script_name(fp8), "~/ornith-35b-fp8.sh");
        // No leaked host/user/home — only the `~` shell convention + the slug.
        assert!(script_name(fp8).starts_with("~/"));
        assert!(script_name(fp8).ends_with(".sh"));
    }

    #[test]
    fn find_variant_round_trips_every_slug_case_insensitively() {
        for v in REGISTRY {
            let slug = variant_slug(v);
            assert_eq!(find_variant(&slug), Some(v), "lower {slug}");
            assert_eq!(
                find_variant(&slug.to_ascii_uppercase()),
                Some(v),
                "upper {slug}"
            );
            // Surrounding whitespace is tolerated.
            assert_eq!(
                find_variant(&format!("  {slug}  ")),
                Some(v),
                "padded {slug}"
            );
        }
    }

    #[test]
    fn find_variant_unknown_is_none() {
        assert!(find_variant("ornith-99b-fp4").is_none());
        assert!(find_variant("").is_none());
        assert!(find_variant("not-a-slug").is_none());
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
