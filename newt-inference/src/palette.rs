//! A curated **palette of mini models** for in-process inference on a small box
//! (the #639 summarizer-first use case; target Apple-Silicon M4 / 16 GB).
//!
//! This is pure data — a catalog of small, quantized (GGUF) instruct models that
//! fit alongside the agent and suit bounded auxiliary calls (summarization,
//! triage, classification), NOT the primary coding loop. It is available
//! regardless of the `embedded` cargo feature (so `newt models` / docs can list
//! it), while the in-process engine that *runs* one of these is feature-gated.
//!
//! RAM figures are the approximate resident size of the Q4_K_M weights; leave
//! headroom for the KV cache + the agent itself. On a 16 GB box, the 0.5B–1.5B
//! entries are the safe summarizer picks; the 3B entries are the upper bound.

/// Which quantized model implementation an entry maps to (the engine selects the
/// matching candle/llama.cpp loader by this). SmolLM2 is a Llama-architecture
/// model, so it uses [`ModelArch::Llama`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelArch {
    /// Qwen2 / Qwen2.5 family.
    Qwen2,
    /// Llama 3.x family (also SmolLM2).
    Llama,
    /// Gemma 2 family.
    Gemma2,
}

/// One entry in the mini-model palette.
#[derive(Debug, Clone, Copy)]
pub struct MiniModel {
    /// Stable short alias used to select the model (e.g. `qwen2.5-1.5b`).
    pub name: &'static str,
    /// Parameter count, human form (e.g. `1.5B`).
    pub params: &'static str,
    /// Quantization of the referenced GGUF (e.g. `Q4_K_M`).
    pub quant: &'static str,
    /// Approximate resident RAM of the weights, in GB (leave headroom for the KV
    /// cache + the agent).
    pub approx_ram_gb: f32,
    /// Hugging Face repo hosting the GGUF (for `newt models pull` / manual fetch;
    /// nothing is auto-downloaded into a small box).
    pub hf_repo: &'static str,
    /// The GGUF file within [`Self::hf_repo`].
    pub gguf_file: &'static str,
    /// Hugging Face repo serving the standalone `tokenizer.json`. Candle loads the
    /// HF fast-tokenizer file, which the quant GGUF repos above generally do NOT
    /// ship — it lives in the base instruct repo (Qwen) or an open mirror
    /// (unsloth / HuggingFaceTB). `newt models pull` fetches it next to the GGUF.
    pub tokenizer_repo: &'static str,
    /// The quantized model implementation the engine loads.
    pub arch: ModelArch,
    /// What this pick is good for.
    pub note: &'static str,
}

/// The curated palette, smallest-first. Q4_K_M GGUFs from the vendor or a
/// well-known GGUF mirror.
pub const PALETTE: &[MiniModel] = &[
    MiniModel {
        name: "qwen2.5-0.5b",
        params: "0.5B",
        quant: "Q4_K_M",
        approx_ram_gb: 0.5,
        hf_repo: "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
        gguf_file: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-0.5B-Instruct",
        arch: ModelArch::Qwen2,
        note: "tiniest; fast summarizer/classifier, lowest RAM",
    },
    MiniModel {
        name: "llama-3.2-1b",
        params: "1B",
        quant: "Q4_K_M",
        approx_ram_gb: 0.8,
        hf_repo: "bartowski/Llama-3.2-1B-Instruct-GGUF",
        gguf_file: "Llama-3.2-1B-Instruct-Q4_K_M.gguf",
        tokenizer_repo: "unsloth/Llama-3.2-1B-Instruct",
        arch: ModelArch::Llama,
        note: "strong 1B; good summaries with tiny footprint",
    },
    MiniModel {
        name: "smollm2-1.7b",
        params: "1.7B",
        quant: "Q4_K_M",
        approx_ram_gb: 1.0,
        hf_repo: "bartowski/SmolLM2-1.7B-Instruct-GGUF",
        gguf_file: "SmolLM2-1.7B-Instruct-Q4_K_M.gguf",
        tokenizer_repo: "HuggingFaceTB/SmolLM2-1.7B-Instruct",
        arch: ModelArch::Llama,
        note: "instruction-tuned small; concise summarizer",
    },
    MiniModel {
        name: "qwen2.5-1.5b",
        params: "1.5B",
        quant: "Q4_K_M",
        approx_ram_gb: 1.0,
        hf_repo: "Qwen/Qwen2.5-1.5B-Instruct-GGUF",
        gguf_file: "qwen2.5-1.5b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-1.5B-Instruct",
        arch: ModelArch::Qwen2,
        note: "balanced default summarizer pick on 16 GB",
    },
    MiniModel {
        name: "gemma-2-2b",
        params: "2B",
        quant: "Q4_K_M",
        approx_ram_gb: 1.7,
        hf_repo: "bartowski/gemma-2-2b-it-GGUF",
        gguf_file: "gemma-2-2b-it-Q4_K_M.gguf",
        tokenizer_repo: "unsloth/gemma-2-2b-it",
        arch: ModelArch::Gemma2,
        note: "fluent 2B; nicer prose, more RAM",
    },
    MiniModel {
        name: "qwen2.5-3b",
        params: "3B",
        quant: "Q4_K_M",
        approx_ram_gb: 2.0,
        hf_repo: "Qwen/Qwen2.5-3B-Instruct-GGUF",
        gguf_file: "qwen2.5-3b-instruct-q4_k_m.gguf",
        tokenizer_repo: "Qwen/Qwen2.5-3B-Instruct",
        arch: ModelArch::Qwen2,
        note: "strongest small; upper bound for a 16 GB box",
    },
    MiniModel {
        name: "llama-3.2-3b",
        params: "3B",
        quant: "Q4_K_M",
        approx_ram_gb: 2.0,
        hf_repo: "bartowski/Llama-3.2-3B-Instruct-GGUF",
        gguf_file: "Llama-3.2-3B-Instruct-Q4_K_M.gguf",
        tokenizer_repo: "unsloth/Llama-3.2-3B-Instruct",
        arch: ModelArch::Llama,
        note: "capable 3B; upper bound on a 16 GB box",
    },
];

/// The whole palette (smallest-first).
#[must_use]
pub fn palette() -> &'static [MiniModel] {
    PALETTE
}

/// Look up a palette entry by its [`MiniModel::name`] alias.
#[must_use]
pub fn find(name: &str) -> Option<&'static MiniModel> {
    PALETTE.iter().find(|m| m.name == name)
}

/// Palette entries whose weights fit within `ram_budget_gb` (smallest-first).
/// A rough guide — the caller still leaves headroom for the KV cache + the agent.
#[must_use]
pub fn fitting(ram_budget_gb: f32) -> Vec<&'static MiniModel> {
    PALETTE
        .iter()
        .filter(|m| m.approx_ram_gb <= ram_budget_gb)
        .collect()
}

/// The designated DEFAULT summarizer model — the tiniest palette entry
/// (qwen2.5-0.5b), chosen for the smallest RAM + download footprint. The
/// context summarizer defaults to THIS running in-process on the host CPU
/// (#661 group C); a `[summarizer]` backend override is the only way off it.
#[must_use]
pub fn default_model() -> &'static MiniModel {
    &PALETTE[0]
}

/// `~/.newt/models` — where `newt models pull` stores palette GGUFs and where
/// the embedded summarizer looks for them. `None` if the home dir is unknown.
#[must_use]
pub fn models_dir() -> Option<std::path::PathBuf> {
    newt_core::Config::user_config_dir().map(|d| d.join("models"))
}

/// The on-disk GGUF path for a palette model: `~/.newt/models/<alias>/<file>`.
#[must_use]
pub fn local_gguf_path(m: &MiniModel) -> Option<std::path::PathBuf> {
    models_dir().map(|d| d.join(m.name).join(m.gguf_file))
}

/// The on-disk `tokenizer.json` path for a palette model, beside its GGUF:
/// `~/.newt/models/<alias>/tokenizer.json`. Candle loads this next to the GGUF,
/// so `newt models pull` fetches it from [`MiniModel::tokenizer_repo`].
#[must_use]
pub fn local_tokenizer_path(m: &MiniModel) -> Option<std::path::PathBuf> {
    models_dir().map(|d| d.join(m.name).join("tokenizer.json"))
}

/// The local GGUF for `alias` IFF the model is FULLY provisioned — both the GGUF
/// **and** its `tokenizer.json` (candle needs both). A GGUF-only install returns
/// `None`, so the summarizer resolver degrades cleanly at startup (with a warning
/// to run `newt models pull`) instead of the embedded backend failing to init
/// mid-compaction — the exact failure that fell back to a static marker.
#[must_use]
pub fn resolve_local(alias: &str) -> Option<std::path::PathBuf> {
    let m = find(alias)?;
    let gguf = local_gguf_path(m)?;
    let tok = local_tokenizer_path(m)?;
    (gguf.is_file() && tok.is_file()).then_some(gguf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_nonempty_and_well_formed() {
        assert!(!PALETTE.is_empty());
        for m in PALETTE {
            assert!(!m.name.is_empty());
            assert!(!m.hf_repo.is_empty() && m.gguf_file.ends_with(".gguf"));
            // Candle needs a standalone tokenizer.json; every entry must name a
            // repo that serves one (the GGUF quant repos generally do not).
            assert!(
                !m.tokenizer_repo.is_empty(),
                "{}: no tokenizer_repo",
                m.name
            );
            // Summarizer-class: every entry must be small enough for a 16 GB box.
            assert!(
                m.approx_ram_gb > 0.0 && m.approx_ram_gb <= 3.0,
                "{} is too large for the mini palette: {} GB",
                m.name,
                m.approx_ram_gb
            );
        }
    }

    #[test]
    fn palette_is_smallest_first_and_names_unique() {
        let mut seen = std::collections::HashSet::new();
        let mut last = 0.0f32;
        for m in PALETTE {
            assert!(seen.insert(m.name), "duplicate palette name: {}", m.name);
            assert!(m.approx_ram_gb >= last, "palette must be smallest-first");
            last = m.approx_ram_gb;
        }
    }

    #[test]
    fn tokenizer_json_sits_beside_the_gguf() {
        // Regression (#661 group C): candle loads tokenizer.json NEXT TO the GGUF,
        // and the quant GGUF repos do not ship one — an earlier pull/auto-provision
        // fetched only the GGUF, so init failed ("tokenizer not found") and the
        // summarizer fell back to a static marker. Both files must resolve into the
        // same per-alias dir, with the tokenizer named exactly `tokenizer.json`.
        for m in PALETTE {
            let (Some(g), Some(t)) = (local_gguf_path(m), local_tokenizer_path(m)) else {
                continue; // no home dir in this env — nothing to check structurally
            };
            assert_eq!(g.parent(), t.parent(), "{}: tokenizer not beside gguf", m.name);
            assert_eq!(
                t.file_name().and_then(|s| s.to_str()),
                Some("tokenizer.json"),
                "{}: tokenizer must be tokenizer.json",
                m.name
            );
        }
    }

    #[test]
    fn find_resolves_by_alias() {
        assert_eq!(find("qwen2.5-1.5b").unwrap().arch, ModelArch::Qwen2);
        assert_eq!(find("llama-3.2-1b").unwrap().arch, ModelArch::Llama);
        assert!(find("does-not-exist").is_none());
    }

    #[test]
    fn fitting_respects_a_ram_budget() {
        let small = fitting(1.0);
        assert!(small.iter().all(|m| m.approx_ram_gb <= 1.0));
        assert!(
            small.len() < PALETTE.len(),
            "a 1 GB budget should exclude the larger entries"
        );
        // The tiniest model fits any sane budget.
        assert!(fitting(0.5).iter().any(|m| m.name == "qwen2.5-0.5b"));
    }
}
