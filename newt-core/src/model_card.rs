//! Data-driven **model cards** (#853) — the central abstraction for standing a
//! model up. One card holds *all* the settings needed to serve exactly one model
//! on one backend (vLLM or ollama), plus its sampling `tuning` and reasoning
//! `capability` bits. Authoring a new card is how you add a model newt has never
//! heard of.
//!
//! Three Cs: knowledge lives in **data**, defaults are **overridable**. A card is
//! deserialized from TOML *or* YAML; every field is `Option` so a partial overlay
//! overrides only what it sets, giving the precedence
//! `built-in < ~/.newt/models/<name> < --card < CLI flag`. [`ModelCard::merge`],
//! [`ModelCard::validate`], and [`resolve`] are **pure / IO-free** (the file reads
//! in [`load_card_file`] / [`load_dropin_dir`] are the only IO) — mirroring the
//! `dgx_vllm` / `dgx_pull` discipline.
//!
//! **IDENTITIES ONLY** (public-repo rule, like `newt-cli::dgx_registry`): a card
//! carries model identities + serving *profiles* — never a host / IP / GPU / DNS.
//! The endpoint stays in the operator's local `[dgx]` config, resolved at runtime.
//! Fields that *look* hardware-ish but are not: `gpu_mem` is a
//! `--gpu-memory-utilization` **fraction**, `tensor_parallel` a topology knob,
//! `served_name` an OpenAI alias — none identify a machine. [`no_hardware_leak`]
//! enforces this and is applied to every built-in card.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The inference backend a card targets. newt-cli's `InferenceTool` is the
/// CLI-side sibling; this core-side enum lets the harness read a card without a
/// newt-cli dependency (crate boundary). A future pass can unify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// vLLM (OpenAI-compatible server; FP8/FP16 + multi-node tensor-parallel).
    Vllm,
    /// Ollama (GGUF single-node convenience runtime).
    Ollama,
    /// llama.cpp (GGUF experimental large-model single-node path).
    LlamaCpp,
}

impl Backend {
    /// Stable lowercase token (matches `InferenceTool::as_str`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama_cpp",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vllm" => Ok(Self::Vllm),
            "ollama" => Ok(Self::Ollama),
            "llama_cpp" | "llama.cpp" | "llamacpp" => Ok(Self::LlamaCpp),
            other => Err(format!(
                "unknown backend `{other}` (expected vllm | ollama | llama_cpp)"
            )),
        }
    }
}

/// vLLM serving profile — the `vllm serve` knobs. Every field `Option` for layered
/// override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VllmProfile {
    /// `--served-model-name` (the OpenAI alias clients send as `model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_name: Option<String>,
    /// `--max-model-len` (context window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<u32>,
    /// `--tensor-parallel-size` (topology knob — not a machine id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_parallel: Option<u8>,
    /// `--gpu-memory-utilization` **fraction** (0.0–1.0) — a knob, not a machine id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_mem: Option<f64>,
    /// `--reasoning-parser` (e.g. `qwen3`) so CoT lands in `reasoning_content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_parser: Option<String>,
    /// `--tool-call-parser` (e.g. `qwen3_xml`) so tool calls surface as `tool_calls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_parser: Option<String>,
    /// `--enable-auto-tool-choice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_auto_tool_choice: Option<bool>,
    /// Extra raw `vllm serve` argv appended verbatim (the escape hatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

impl VllmProfile {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            served_name: o.served_name.or(self.served_name),
            max_model_len: o.max_model_len.or(self.max_model_len),
            tensor_parallel: o.tensor_parallel.or(self.tensor_parallel),
            gpu_mem: o.gpu_mem.or(self.gpu_mem),
            reasoning_parser: o.reasoning_parser.or(self.reasoning_parser),
            tool_call_parser: o.tool_call_parser.or(self.tool_call_parser),
            enable_auto_tool_choice: o.enable_auto_tool_choice.or(self.enable_auto_tool_choice),
            extra: if o.extra.is_empty() {
                self.extra
            } else {
                o.extra
            },
        }
    }
}

/// Ollama serving profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OllamaProfile {
    /// Ollama tag, e.g. `ornith:35b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// `num_ctx` — set EXPLICITLY (a large auto value can OOM the KV cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Optional Modelfile template override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl OllamaProfile {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            tag: o.tag.or(self.tag),
            num_ctx: o.num_ctx.or(self.num_ctx),
            template: o.template.or(self.template),
        }
    }
}

/// Per-model sampling tuning (seeds a `[[model_tuning]]` entry on `setup`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tuning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// The context-token budget the harness should plan against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

impl Tuning {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            temperature: o.temperature.or(self.temperature),
            top_p: o.top_p.or(self.top_p),
            top_k: o.top_k.or(self.top_k),
            context_tokens: o.context_tokens.or(self.context_tokens),
        }
    }
}

/// Reasoning capability bits the harness reads (retires the `reasoning.rs`
/// name-match in a later issue).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    /// The model opens its turn with a reasoning / `<think>` block that must be
    /// split out of the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emits_leading_reasoning: Option<bool>,
    /// Thinking is on by default (the per-turn toggle's fail-safe value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_default: Option<bool>,
    /// The server returns CoT in a separate response field of this name (e.g.
    /// `reasoning_content`); `None` = inline `<think>` inside `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content_field: Option<String>,
}

impl Capability {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            emits_leading_reasoning: o.emits_leading_reasoning.or(self.emits_leading_reasoning),
            thinking_default: o.thinking_default.or(self.thinking_default),
            reasoning_content_field: o.reasoning_content_field.or(self.reasoning_content_field),
        }
    }
}

/// A model card: everything needed to stand up one model. `name` is required (it
/// is the merge key); every other field is `Option` so a partial overlay overrides
/// only what it sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCard {
    /// Model identity, e.g. `Ornith-1.0-35B`. The merge / drop-in key.
    pub name: String,
    /// Which backend `setup` stands up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Approximate resident footprint (GiB) — a serving knob (like
    /// `ModelVariant.gib`), not a machine id; used by the hardware-fit gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_gib: Option<f64>,
    /// Likely exceeds a typical node (e.g. the 397B) — `setup` warns / requires
    /// `--force`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vllm: Option<VllmProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama: Option<OllamaProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning: Option<Tuning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
}

fn merge_opt<T>(base: Option<T>, overlay: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (base, overlay) {
        (Some(b), Some(o)) => Some(f(b, o)),
        (b, None) => b,
        (None, o) => o,
    }
}

impl ModelCard {
    /// Overlay `self` under `overlay` — overlay wins field-by-field, deep-merging
    /// the nested tables. Pure. Precedence chains left-to-right:
    /// `builtin.merge(dropin).merge(one_off).merge(flags)`.
    #[must_use]
    pub fn merge(self, overlay: Self) -> Self {
        Self {
            name: if overlay.name.trim().is_empty() {
                self.name
            } else {
                overlay.name
            },
            backend: overlay.backend.or(self.backend),
            footprint_gib: overlay.footprint_gib.or(self.footprint_gib),
            gated: overlay.gated.or(self.gated),
            vllm: merge_opt(self.vllm, overlay.vllm, VllmProfile::merge),
            ollama: merge_opt(self.ollama, overlay.ollama, OllamaProfile::merge),
            tuning: merge_opt(self.tuning, overlay.tuning, Tuning::merge),
            capability: merge_opt(self.capability, overlay.capability, Capability::merge),
        }
    }

    /// Reject a structurally-invalid card **loudly** (never silently): empty name,
    /// no backend, or a backend whose serving block is absent.
    ///
    /// # Errors
    /// Returns a human-readable reason when the card cannot be stood up.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("model card: `name` is empty".to_string());
        }
        match self.backend {
            None => Err(format!(
                "model card `{}`: no `backend` set (vllm | ollama | llama_cpp)",
                self.name
            )),
            Some(Backend::Vllm) if self.vllm.is_none() => Err(format!(
                "model card `{}`: backend=vllm but no [vllm] serving block",
                self.name
            )),
            Some(Backend::Ollama) if self.ollama.is_none() => Err(format!(
                "model card `{}`: backend=ollama but no [ollama] serving block",
                self.name
            )),
            Some(_) => Ok(()),
        }
    }

    /// The card as canonical TOML (for `card show` / seeding `~/.newt`).
    ///
    /// # Errors
    /// Propagates a serialization error (should not occur for a valid card).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("card serialize: {e}"))
    }
}

/// Parse a card from `contents` as TOML or YAML, dispatched by `ext` (with or
/// without a leading dot). Pure over the bytes + extension.
///
/// # Errors
/// Returns a parse error, or an "unknown extension" error for anything other than
/// `toml` / `yaml` / `yml`.
pub fn parse_card(contents: &str, ext: &str) -> Result<ModelCard, String> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "toml" => toml::from_str(contents).map_err(|e| format!("card TOML: {e}")),
        "yaml" | "yml" => serde_yaml::from_str(contents).map_err(|e| format!("card YAML: {e}")),
        other => Err(format!(
            "card: unknown extension `.{other}` (expected .toml / .yaml / .yml)"
        )),
    }
}

/// Load a single card file (`--card <path>`), dispatching TOML/YAML on extension.
///
/// # Errors
/// Returns a read error or a parse error.
pub fn load_card_file(path: &Path) -> Result<ModelCard, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    parse_card(&contents, ext)
}

/// Load every droppable card in `dir` (`*.toml` / `*.yaml` / `*.yml`). A missing
/// dir yields an empty list; an unreadable/invalid file is skipped (best-effort,
/// like the language-pack loader). The merge itself stays pure via [`resolve`].
#[must_use]
pub fn load_dropin_dir(dir: &Path) -> Vec<ModelCard> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(ext, "toml" | "yaml" | "yml") {
            if let Ok(card) = load_card_file(&p) {
                out.push(card);
            }
        }
    }
    out
}

/// Resolve the effective card for a `builtin`: overlay every drop-in whose `name`
/// matches (merge-by-name, the language-pack rule), then an optional one-off
/// (`--card`). **Pure** over the inputs.
#[must_use]
pub fn resolve(builtin: ModelCard, dropins: &[ModelCard], one_off: Option<ModelCard>) -> ModelCard {
    let mut card = builtin;
    let name = card.name.clone();
    for d in dropins.iter().filter(|d| d.name == name) {
        card = card.merge(d.clone());
    }
    if let Some(o) = one_off {
        card = card.merge(o);
    }
    card
}

/// The built-in model cards shipped with newt (embedded DATA) — the base layer of
/// the precedence chain. A drop-in `~/.newt/models/<name>.toml` overrides one by
/// name; `--card <path>` overlays a one-off. Ornith-1.0-35B is the reference card;
/// the 397B ships `gated` (it almost certainly exceeds a single node).
#[must_use]
pub fn builtin_cards() -> Vec<ModelCard> {
    const EMBEDDED: &[&str] = &[
        include_str!("cards/ornith-1.0-35b.toml"),
        include_str!("cards/ornith-1.0-397b.toml"),
    ];
    EMBEDDED
        .iter()
        .map(|s| parse_card(s, "toml").expect("built-in card is valid TOML"))
        .collect()
}

/// The built-in card whose `name` matches (case-insensitive), if any.
#[must_use]
pub fn builtin_card(name: &str) -> Option<ModelCard> {
    let want = name.trim().to_ascii_lowercase();
    builtin_cards()
        .into_iter()
        .find(|c| c.name.to_ascii_lowercase() == want)
}

/// Scan a card for a leaked machine identity — an RFC1918 / CGNAT IPv4 literal in
/// any string field. Built-in cards MUST pass (identities only; the endpoint lives
/// in local `[dgx]` config). Returns the offending values (empty = clean).
///
/// A pragmatic guard, not a full network parser: it flags dotted-quad literals in
/// the private / carrier-grade ranges, which is what a stray endpoint looks like.
#[must_use]
pub fn no_hardware_leak(card: &ModelCard) -> Vec<String> {
    // Serialize to TOML and scan the text — covers every string field uniformly.
    let text = card.to_toml().unwrap_or_default();
    let mut hits = Vec::new();
    for tok in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if is_private_ipv4(tok) {
            hits.push(tok.to_string());
        }
    }
    hits
}

/// True for an RFC1918 (`10.`, `172.16–31.`, `192.168.`) or CGNAT (`100.64–127.`)
/// dotted-quad. Pure.
fn is_private_ipv4(s: &str) -> bool {
    let octets: Vec<&str> = s.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    let mut o = [0u16; 4];
    for (i, part) in octets.iter().enumerate() {
        match part.parse::<u16>() {
            Ok(v) if v <= 255 && (part.len() == 1 || !part.starts_with('0')) => o[i] = v,
            _ => return false,
        }
    }
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ornith_toml() -> &'static str {
        r#"
name = "Ornith-1.0-35B"
backend = "vllm"
footprint_gib = 35.0

[vllm]
served_name = "Ornith-1.0-35B"
max_model_len = 262144
reasoning_parser = "qwen3"
tool_call_parser = "qwen3_xml"
enable_auto_tool_choice = true

[tuning]
temperature = 0.6
top_p = 0.95
top_k = 20
context_tokens = 262144

[capability]
emits_leading_reasoning = true
thinking_default = true
reasoning_content_field = "reasoning_content"
"#
    }

    // ── Backend vocabulary ────────────────────────────────────────────────
    #[test]
    fn backend_tokens_round_trip() {
        for (tok, b) in [
            ("vllm", Backend::Vllm),
            ("ollama", Backend::Ollama),
            ("llama_cpp", Backend::LlamaCpp),
        ] {
            assert_eq!(Backend::from_str(tok).unwrap(), b);
            assert_eq!(b.as_str(), tok);
        }
        // Tolerant aliases + case + whitespace.
        assert_eq!(Backend::from_str("  VLLM ").unwrap(), Backend::Vllm);
        assert_eq!(Backend::from_str("llama.cpp").unwrap(), Backend::LlamaCpp);
        // Unknown fails loudly.
        assert!(Backend::from_str("tgi").is_err());
    }

    // ── TOML / YAML parity ────────────────────────────────────────────────
    #[test]
    fn parses_from_toml_and_yaml_identically() {
        let from_toml = parse_card(ornith_toml(), "toml").expect("toml");
        // The same card in YAML.
        let yaml = r#"
name: Ornith-1.0-35B
backend: vllm
footprint_gib: 35.0
vllm:
  served_name: Ornith-1.0-35B
  max_model_len: 262144
  reasoning_parser: qwen3
  tool_call_parser: qwen3_xml
  enable_auto_tool_choice: true
tuning:
  temperature: 0.6
  top_p: 0.95
  top_k: 20
  context_tokens: 262144
capability:
  emits_leading_reasoning: true
  thinking_default: true
  reasoning_content_field: reasoning_content
"#;
        let from_yaml = parse_card(yaml, "yml").expect("yaml");
        assert_eq!(from_toml, from_yaml, "TOML and YAML must parse identically");
        // And a TOML round-trip is stable.
        let reparsed = parse_card(&from_toml.to_toml().unwrap(), "toml").unwrap();
        assert_eq!(reparsed, from_toml);
    }

    #[test]
    fn parse_rejects_unknown_extension_and_bad_syntax() {
        assert!(parse_card(ornith_toml(), "json").is_err());
        assert!(parse_card("name = \"x\"\nbogus_field = 1", "toml").is_err()); // deny_unknown_fields
    }

    // ── Merge / layered override ──────────────────────────────────────────
    #[test]
    fn overlay_wins_field_by_field_and_deep_merges() {
        let base = parse_card(ornith_toml(), "toml").unwrap();
        // An overlay that sets ONLY vllm.max_model_len + tuning.temperature.
        let overlay: ModelCard = toml::from_str(
            "name = \"Ornith-1.0-35B\"\n[vllm]\nmax_model_len = 65536\n[tuning]\ntemperature = 1.0",
        )
        .unwrap();
        let merged = base.clone().merge(overlay);
        let v = merged.vllm.as_ref().unwrap();
        // The overlaid field wins…
        assert_eq!(v.max_model_len, Some(65536));
        assert_eq!(merged.tuning.as_ref().unwrap().temperature, Some(1.0));
        // …and every un-set field is INHERITED from the base (deep merge).
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
        assert_eq!(merged.tuning.as_ref().unwrap().top_p, Some(0.95));
        assert_eq!(
            merged.capability.as_ref().unwrap().thinking_default,
            Some(true)
        );
    }

    #[test]
    fn resolve_applies_precedence_builtin_dropin_oneoff() {
        let builtin = parse_card(ornith_toml(), "toml").unwrap();
        // Drop-in bumps gpu_mem; matched by name.
        let dropin: ModelCard =
            toml::from_str("name = \"Ornith-1.0-35B\"\n[vllm]\ngpu_mem = 0.9").unwrap();
        // A drop-in for a DIFFERENT model must be ignored.
        let other: ModelCard =
            toml::from_str("name = \"Other\"\n[vllm]\nmax_model_len = 1").unwrap();
        // One-off (--card) overrides max_model_len last.
        let one_off: ModelCard =
            toml::from_str("name = \"Ornith-1.0-35B\"\n[vllm]\nmax_model_len = 131072").unwrap();
        let card = resolve(builtin, &[dropin, other], Some(one_off));
        let v = card.vllm.unwrap();
        assert_eq!(v.gpu_mem, Some(0.9), "drop-in applied");
        assert_eq!(v.max_model_len, Some(131072), "one-off wins over built-in");
        assert_eq!(
            v.reasoning_parser.as_deref(),
            Some("qwen3"),
            "base inherited"
        );
    }

    // ── Validate ──────────────────────────────────────────────────────────
    #[test]
    fn validate_accepts_a_well_formed_card() {
        assert!(parse_card(ornith_toml(), "toml")
            .unwrap()
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_rejects_empty_name_missing_backend_and_missing_block() {
        let mut c = parse_card(ornith_toml(), "toml").unwrap();
        c.name = "  ".into();
        assert!(c.validate().is_err(), "empty name");

        let no_backend: ModelCard = toml::from_str("name = \"X\"").unwrap();
        assert!(no_backend.validate().is_err(), "no backend");

        let vllm_no_block: ModelCard = toml::from_str("name = \"X\"\nbackend = \"vllm\"").unwrap();
        assert!(vllm_no_block.validate().is_err(), "backend=vllm, no [vllm]");

        let ollama_no_block: ModelCard =
            toml::from_str("name = \"X\"\nbackend = \"ollama\"").unwrap();
        assert!(
            ollama_no_block.validate().is_err(),
            "backend=ollama, no [ollama]"
        );
    }

    // ── No hardware leak ──────────────────────────────────────────────────
    #[test]
    fn no_hardware_leak_flags_a_private_ip_and_passes_clean_cards() {
        // Clean card: no leak.
        let clean = parse_card(ornith_toml(), "toml").unwrap();
        assert!(
            no_hardware_leak(&clean).is_empty(),
            "identities-only card is clean"
        );

        // A card that smuggled an endpoint into served_name is caught. Build the
        // private IP from octets so no literal RFC1918 dotted-quad sits in this
        // public source (the pre-push network-leak guard).
        let ip = format!("{}.{}.{}.{}", 192, 168, 1, 100);
        let leaky: ModelCard = toml::from_str(&format!(
            "name = \"X\"\nbackend = \"vllm\"\n[vllm]\nserved_name = \"http://{ip}:8000\""
        ))
        .unwrap();
        assert_eq!(no_hardware_leak(&leaky), vec![ip]);
    }

    #[test]
    fn is_private_ipv4_ranges() {
        // Build the private-range samples from octets so no literal RFC1918/CGNAT
        // dotted-quad appears in this public source (the pre-push leak guard).
        for o in [
            [10, 0, 0, 1],
            [172, 16, 0, 1],
            [172, 31, 255, 255],
            [192, 168, 1, 1],
            [100, 64, 0, 1],
        ] {
            let ip = format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]);
            assert!(is_private_ipv4(&ip), "{ip} is private");
        }
        for ip in [
            "8.8.8.8",
            "172.15.0.1",
            "172.32.0.1",
            "1.2.3",
            "256.1.1.1",
            "01.2.3.4",
        ] {
            assert!(!is_private_ipv4(ip), "{ip} is NOT flagged");
        }
    }

    // ── #854: built-in Ornith cards ───────────────────────────────────────
    #[test]
    fn builtin_cards_parse_validate_and_are_leak_free() {
        let cards = builtin_cards();
        assert!(!cards.is_empty(), "built-in cards present");
        for c in &cards {
            c.validate()
                .unwrap_or_else(|e| panic!("built-in `{}` invalid: {e}", c.name));
            assert!(
                no_hardware_leak(c).is_empty(),
                "built-in `{}` leaks a private IP",
                c.name
            );
            // Identities only: a built-in card pins NO node-specific knob.
            if let Some(v) = c.vllm.as_ref() {
                assert!(v.gpu_mem.is_none(), "{}: gpu_mem is node-specific", c.name);
                assert!(
                    v.tensor_parallel.is_none(),
                    "{}: tensor_parallel is node-specific",
                    c.name
                );
            }
        }
    }

    #[test]
    fn ornith_35b_card_has_the_expected_settings() {
        let c = builtin_card("Ornith-1.0-35B").expect("Ornith-1.0-35B present");
        assert_eq!(c.backend, Some(Backend::Vllm));
        assert_eq!(c.gated, None, "the 35B is runnable, not gated");
        let v = c.vllm.unwrap();
        assert_eq!(v.max_model_len, Some(262_144));
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
        assert_eq!(v.enable_auto_tool_choice, Some(true));
        assert!(v.extra.iter().any(|a| a == "--enable-prefix-caching"));
        assert!(v.extra.iter().any(|a| a == "--trust-remote-code"));
        let t = c.tuning.unwrap();
        assert_eq!(t.temperature, Some(0.6));
        assert_eq!(t.top_p, Some(0.95));
        assert_eq!(t.top_k, Some(20));
        assert_eq!(t.context_tokens, Some(262_144));
        let cap = c.capability.unwrap();
        assert_eq!(cap.emits_leading_reasoning, Some(true));
        assert_eq!(cap.thinking_default, Some(true));
        assert_eq!(
            cap.reasoning_content_field.as_deref(),
            Some("reasoning_content")
        );
        let o = c.ollama.unwrap();
        assert_eq!(o.tag.as_deref(), Some("ornith:35b"));
        assert!(
            o.num_ctx.unwrap() < 262_144,
            "ollama num_ctx is capped below the native window (OOM guard)"
        );
    }

    #[test]
    fn ornith_397b_is_gated() {
        let c = builtin_card("Ornith-1.0-397B").expect("397B present as a gated card");
        assert_eq!(c.gated, Some(true), "the 397B must be hardware-gated");
    }

    #[test]
    fn builtin_card_is_overridable_by_name() {
        // A drop-in overrides only what it sets; the rest inherits from the built-in.
        let base = builtin_card("Ornith-1.0-35B").unwrap();
        let dropin: ModelCard =
            toml::from_str("name = \"Ornith-1.0-35B\"\n[ollama]\nnum_ctx = 65536").unwrap();
        let resolved = resolve(base, std::slice::from_ref(&dropin), None);
        assert_eq!(
            resolved.ollama.unwrap().num_ctx,
            Some(65536),
            "drop-in wins"
        );
        assert_eq!(
            resolved.vllm.unwrap().reasoning_parser.as_deref(),
            Some("qwen3"),
            "un-set fields inherit from the built-in"
        );
    }
}
