//! Cost estimation for inference backends.
//!
//! `PricingConfig` is stored under `[pricing]` in `newt.toml`. It carries
//! user overrides and is consulted together with a built-in rate table to
//! produce best-effort USD cost estimates for each inference turn.
//!
//! Resolution: a `[pricing.overrides]` entry wins (exact id, then longest
//! prefix), regardless of where the call went; then the built-in table.
//!
//! A known local model family (gemma, llama, qwen, mistral, deepseek, …) is
//! free (`0.0`, displayed "free (local)") ONLY when the call was served
//! locally — the in-process embedded backend or an endpoint whose host the
//! operator owns
//! ([`crate::owned_hosts::inference_is_local`]). The same family-shaped id on a
//! hosted endpoint (`deepseek-chat` at a provider's API) is not a local model:
//! its price is unknown, `None`, displayed "cost unknown" — never a confirmed
//! zero (#2313). Hosted rate-table entries (gpt-4o, claude, gemini) apply
//! wherever the call went.
//!
//! **Known limit.** Locality is inferred from network ownership, so a gateway on
//! the operator's own network (an RFC-1918 address or an owned suffix) that
//! proxies to a paid provider counts as local, and a family-named id routed
//! through it still reads "free (local)". The remedy is an override with the
//! provider's real rate: an override wins over locality.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::metrics::TokenUsage;

/// Per-model pricing rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRate {
    /// USD per 1 000 input tokens.
    pub input_usd_per_1k: f64,
    /// USD per 1 000 output tokens.
    pub output_usd_per_1k: f64,
}

impl ModelRate {
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let input_cost = (usage.input_tokens as f64 / 1000.0) * self.input_usd_per_1k;
        let output_cost = (usage.output_tokens as f64 / 1000.0) * self.output_usd_per_1k;
        input_cost + output_cost
    }
}

/// Pricing configuration stored under `[pricing]` in `newt.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    /// Per-model overrides. Keys are exact model-id strings or prefix patterns.
    /// Example: `{ "my-private-gpt4" = { input_usd_per_1k = 0.01, output_usd_per_1k = 0.03 } }`
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, ModelRate>,
}

impl PricingConfig {
    /// Look up the rate for `model_id`, served locally or not (`local`, from
    /// [`crate::owned_hosts::inference_is_local`]).
    ///
    /// Resolution order:
    /// 1. Exact match in `overrides`
    /// 2. Prefix match in `overrides` (longest prefix wins)
    /// 3. Built-in rate table — a local model family is free only when `local`
    /// 4. `None` — unknown price; never a confirmed zero
    pub fn rate_for(&self, model_id: &str, local: bool) -> Option<ModelRate> {
        // 1. Exact override.
        if let Some(r) = self.overrides.get(model_id) {
            return Some(r.clone());
        }

        // 2. Longest prefix override.
        let prefix_match = self
            .overrides
            .iter()
            .filter(|(k, _)| model_id.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len());
        if let Some((_, r)) = prefix_match {
            return Some(r.clone());
        }

        // 3. Built-in table.
        builtin_rate(model_id, local)
    }

    /// Estimate cost for `usage` with `model_id`, served locally or not.
    /// Returns `None` when the price is unknown (not overridden, not in the
    /// table, and not a local family served locally).
    pub fn estimate_cost(
        &self,
        model_id: &str,
        local: bool,
        usage: Option<&TokenUsage>,
    ) -> Option<f64> {
        let usage = usage?;
        let rate = self.rate_for(model_id, local)?;
        Some(rate.cost(usage))
    }
}

// ---------------------------------------------------------------------------
// Built-in rate table
// ---------------------------------------------------------------------------
//
// Rates are approximate and updated periodically. Users can override any
// entry via `[pricing.overrides]` in `newt.toml`.
// All rates are USD per 1 000 tokens (input / output).

fn builtin_rate(model_id: &str, local: bool) -> Option<ModelRate> {
    let m = model_id.to_lowercase();

    // --- Known local model families → free ---
    for prefix in &[
        "gemma",
        "llama",
        "qwen",
        "mistral",
        "phi",
        "orca",
        "codellama",
        "deepseek",
        "starcoder",
        "falcon",
        "vicuna",
        "neural-chat",
        "openhermes",
        "nous-hermes",
        "dolphin",
        "bakllava",
        "llava",
        "yi:",
        "yi-",
        "zephyr",
        "tinyllama",
    ] {
        if local && m.starts_with(prefix) {
            return Some(ModelRate {
                input_usd_per_1k: 0.0,
                output_usd_per_1k: 0.0,
            });
        }
    }

    // --- OpenAI ---
    if m.starts_with("gpt-4o-mini") {
        return Some(ModelRate {
            input_usd_per_1k: 0.00015,
            output_usd_per_1k: 0.0006,
        });
    }
    if m.starts_with("gpt-4o") {
        return Some(ModelRate {
            input_usd_per_1k: 0.005,
            output_usd_per_1k: 0.015,
        });
    }
    if m.starts_with("gpt-4-turbo") || m.starts_with("gpt-4") {
        return Some(ModelRate {
            input_usd_per_1k: 0.01,
            output_usd_per_1k: 0.03,
        });
    }
    if m.starts_with("gpt-3.5-turbo") {
        return Some(ModelRate {
            input_usd_per_1k: 0.0005,
            output_usd_per_1k: 0.0015,
        });
    }
    if m.starts_with("o1-mini") {
        return Some(ModelRate {
            input_usd_per_1k: 0.003,
            output_usd_per_1k: 0.012,
        });
    }
    if m.starts_with("o1") {
        return Some(ModelRate {
            input_usd_per_1k: 0.015,
            output_usd_per_1k: 0.06,
        });
    }

    // --- Anthropic ---
    if m.contains("claude-3-5-sonnet") || m.contains("claude-3.5-sonnet") {
        return Some(ModelRate {
            input_usd_per_1k: 0.003,
            output_usd_per_1k: 0.015,
        });
    }
    if m.contains("claude-3-5-haiku") || m.contains("claude-3.5-haiku") {
        return Some(ModelRate {
            input_usd_per_1k: 0.0008,
            output_usd_per_1k: 0.004,
        });
    }
    if m.contains("claude-3-opus") {
        return Some(ModelRate {
            input_usd_per_1k: 0.015,
            output_usd_per_1k: 0.075,
        });
    }
    if m.contains("claude-3-sonnet") {
        return Some(ModelRate {
            input_usd_per_1k: 0.003,
            output_usd_per_1k: 0.015,
        });
    }
    if m.contains("claude-3-haiku") {
        return Some(ModelRate {
            input_usd_per_1k: 0.00025,
            output_usd_per_1k: 0.00125,
        });
    }
    if m.contains("claude-2") {
        return Some(ModelRate {
            input_usd_per_1k: 0.008,
            output_usd_per_1k: 0.024,
        });
    }

    // --- Google Gemini (API, not local Ollama) ---
    if m.contains("gemini-1.5-pro") {
        return Some(ModelRate {
            input_usd_per_1k: 0.0035,
            output_usd_per_1k: 0.0105,
        });
    }
    if m.contains("gemini-1.5-flash") {
        return Some(ModelRate {
            input_usd_per_1k: 0.000075,
            output_usd_per_1k: 0.0003,
        });
    }

    // Unknown — return None so callers display nothing.
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(inp: u32, out: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: inp,
            output_tokens: out,
        }
    }

    #[test]
    fn local_models_are_free() {
        let cfg = PricingConfig::default();
        for model in &[
            "gemma4:e2b",
            "llama3.1:8b",
            "qwen2.5-coder:32b",
            "mistral:7b",
        ] {
            let cost = cfg.estimate_cost(model, true, Some(&usage(1000, 500)));
            assert_eq!(cost, Some(0.0), "expected free for {model}");
        }
    }

    /// #2313: a family-shaped model id on a HOSTED endpoint is not a local
    /// model, so its price is unknown — never a confirmed zero.
    #[test]
    fn a_hosted_family_named_model_has_unknown_cost() {
        let cfg = PricingConfig::default();
        for model in ["deepseek-chat", "mistral-large-latest"] {
            assert_eq!(
                cfg.estimate_cost(model, false, Some(&usage(1000, 500))),
                None,
                "{model} on a hosted endpoint"
            );
        }
        // Twin: the same family served locally is a confirmed zero.
        assert_eq!(
            cfg.estimate_cost("qwen3:8b", true, Some(&usage(1000, 500))),
            Some(0.0)
        );
    }

    /// #2313: an explicit override is the operator's declaration, so it wins
    /// over locality in both directions — a zero override is free even when
    /// hosted, and a real rate is charged even on an owned gateway that
    /// proxies to a paid provider (the documented limit's remedy).
    #[test]
    fn an_override_wins_over_locality_in_both_directions() {
        let mut cfg = PricingConfig::default();
        cfg.overrides.insert(
            "deepseek-chat".into(),
            ModelRate {
                input_usd_per_1k: 0.0,
                output_usd_per_1k: 0.0,
            },
        );
        assert_eq!(
            cfg.estimate_cost("deepseek-chat", false, Some(&usage(1000, 500))),
            Some(0.0)
        );

        cfg.overrides.insert(
            "qwen3".into(),
            ModelRate {
                input_usd_per_1k: 0.5,
                output_usd_per_1k: 1.0,
            },
        );
        let gateway = cfg
            .estimate_cost("qwen3:8b", true, Some(&usage(2000, 1000)))
            .unwrap();
        // 0.5 * 2 + 1.0 * 1 = 2.0, not the local family's zero.
        assert!((gateway - 2.0).abs() < 0.0001, "got {gateway}");
    }

    #[test]
    fn gpt4o_has_cost() {
        let cfg = PricingConfig::default();
        let cost = cfg.estimate_cost("gpt-4o", false, Some(&usage(1000, 500)));
        assert!(cost.is_some(), "gpt-4o should have a rate");
        assert!(cost.unwrap() > 0.0);
    }

    #[test]
    fn unknown_model_returns_none() {
        let cfg = PricingConfig::default();
        let cost = cfg.estimate_cost("some-private-model-v99", true, Some(&usage(100, 50)));
        assert!(cost.is_none(), "unknown model should return None");
    }

    #[test]
    fn no_usage_returns_none() {
        let cfg = PricingConfig::default();
        assert!(cfg.estimate_cost("gpt-4o", false, None).is_none());
    }

    #[test]
    fn override_takes_precedence() {
        let mut cfg = PricingConfig::default();
        cfg.overrides.insert(
            "gemma4:e2b".into(),
            ModelRate {
                input_usd_per_1k: 1.0,
                output_usd_per_1k: 2.0,
            },
        );
        let cost = cfg.estimate_cost("gemma4:e2b", true, Some(&usage(1000, 1000)));
        // 1.0 * 1 + 2.0 * 1 = 3.0
        assert!((cost.unwrap() - 3.0).abs() < 0.0001);
    }

    #[test]
    fn prefix_override() {
        let mut cfg = PricingConfig::default();
        cfg.overrides.insert(
            "my-model".into(),
            ModelRate {
                input_usd_per_1k: 0.5,
                output_usd_per_1k: 1.0,
            },
        );
        let cost = cfg.estimate_cost("my-model-v2", false, Some(&usage(2000, 1000)));
        // 0.5 * 2 + 1.0 * 1 = 2.0
        assert!((cost.unwrap() - 2.0).abs() < 0.0001);
    }

    #[test]
    fn claude_models_have_rates() {
        let cfg = PricingConfig::default();
        for m in &[
            "claude-3-5-sonnet-20241022",
            "claude-3-haiku-20240307",
            "claude-3-opus-20240229",
        ] {
            assert!(
                cfg.estimate_cost(m, false, Some(&usage(100, 50))).unwrap() > 0.0,
                "{m} should have a positive rate"
            );
        }
    }

    #[test]
    fn pricing_config_toml_roundtrip() {
        let mut cfg = PricingConfig::default();
        cfg.overrides.insert(
            "test-model".into(),
            ModelRate {
                input_usd_per_1k: 0.01,
                output_usd_per_1k: 0.02,
            },
        );
        let s = toml::to_string(&cfg).unwrap();
        let back: PricingConfig = toml::from_str(&s).unwrap();
        assert!(back.overrides.contains_key("test-model"));
    }
}
