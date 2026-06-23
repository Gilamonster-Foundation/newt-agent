//! Pure planning logic for `newt dgx vllm` — stand up a vLLM OpenAI-compatible
//! server on a DGX node.
//!
//! This module is the standup twin of [`crate::dgx_pull`] and follows the same
//! non-negotiable rule: **every function here is deterministic and IO-free** so
//! the unit tier can exercise it with no network, filesystem, subprocess, or
//! clock. The CLI layer (`dgx.rs`) holds the only `async` IO and injects the SSH
//! executor + `/health` poller, exactly as the pull path does.
//!
//! Design: `docs/decisions/dgx_vllm_serve.md`.
//!
//! Reuses `dgx_pull::{FitVerdict, fit_check, ssh_argv, parse_free_bytes,
//! sh_quote}` rather than duplicating them.

use crate::dgx_pull::{fit_check, sanitize_ollama_name, sh_quote, staging_component, FitVerdict};

/// Default context window when the caller does not pass `--max-model-len` and no
/// KV-cost estimator is wired yet (deriving the true fit-clamped window from the
/// model's architecture is a follow-on — see `derive_max_model_len`).
pub const DEFAULT_MAX_MODEL_LEN: u32 = 262_144;

// ---------------------------------------------------------------------------
// Runtime + dtype
// ---------------------------------------------------------------------------

/// Which launcher stands the server up. Native (`vllm serve`) is the default;
/// the container path is opt-in and requires docker-socket access on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VllmRuntime {
    #[default]
    Native,
    Docker,
}

/// The weight/compute precision vLLM should load the checkpoint at.
///
/// The quantized variants map to vLLM's `--quantization` flag; `Bf16` is the
/// unquantized dtype (passed via `--dtype`); `Auto` lets vLLM infer from the
/// checkpoint and emits no flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    Auto,
    Nvfp4,
    Fp8,
    Bf16,
    Awq,
    Gptq,
}

impl Dtype {
    /// The value for vLLM's `--quantization` flag, or `None` when the dtype is
    /// not a quantization method (`Auto`, `Bf16`).
    ///
    /// NVFP4 on Blackwell is served via NVIDIA's ModelOpt path, hence
    /// `modelopt_fp4` rather than a bare `fp4` (which is not a vLLM value).
    pub fn quantization_arg(self) -> Option<&'static str> {
        match self {
            Self::Nvfp4 => Some("modelopt_fp4"),
            Self::Fp8 => Some("fp8"),
            Self::Awq => Some("awq"),
            Self::Gptq => Some("gptq"),
            Self::Auto | Self::Bf16 => None,
        }
    }

    /// Parse a `--dtype` CLI value. `None` for an unrecognized token (the caller
    /// turns that into a user-facing error naming the valid set).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "nvfp4" | "fp4" => Some(Self::Nvfp4),
            "fp8" => Some(Self::Fp8),
            "bf16" | "bfloat16" => Some(Self::Bf16),
            "awq" => Some(Self::Awq),
            "gptq" => Some(Self::Gptq),
            _ => None,
        }
    }
}

/// Infer the dtype for a checkpoint from its name and (optionally) the
/// `quantization_config` block of its HF `config.json`.
///
/// The name is the strongest signal (the `nvidia/*-NVFP4` convention); the HF
/// quant config is the fallback when the name is silent.
pub fn infer_dtype(checkpoint: &str, hf_quant_config: Option<&serde_json::Value>) -> Dtype {
    let lc = checkpoint.to_ascii_lowercase();
    // Name-based detection first — most explicit.
    if lc.contains("nvfp4") || lc.contains("-fp4") || lc.contains("_fp4") {
        return Dtype::Nvfp4;
    }
    if lc.contains("fp8") {
        return Dtype::Fp8;
    }
    if lc.contains("awq") {
        return Dtype::Awq;
    }
    if lc.contains("gptq") {
        return Dtype::Gptq;
    }
    if lc.contains("bf16") || lc.contains("bfloat16") {
        return Dtype::Bf16;
    }
    // Fall back to the HF quantization_config.quant_method field.
    if let Some(method) = hf_quant_config
        .and_then(|c| c.get("quant_method"))
        .and_then(|m| m.as_str())
    {
        let m = method.to_ascii_lowercase();
        if m.contains("modelopt") || m.contains("nvfp4") || m.contains("fp4") {
            return Dtype::Nvfp4;
        }
        if m.contains("fp8") {
            return Dtype::Fp8;
        }
        if m.contains("awq") {
            return Dtype::Awq;
        }
        if m.contains("gptq") {
            return Dtype::Gptq;
        }
    }
    Dtype::Auto
}

// ---------------------------------------------------------------------------
// The launch plan
// ---------------------------------------------------------------------------

/// A fully-resolved vLLM launch plan. Every field has a derived default upstream;
/// by the time a `VllmPlan` exists, nothing is left to guess at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VllmPlan {
    pub model: String,
    pub served_name: String,
    pub dtype: Dtype,
    pub tensor_parallel: u8,
    pub max_model_len: u32,
    /// `--gpu-memory-utilization`, scaled by 1000 to stay `Eq`/hashable and
    /// avoid float-in-struct comparison hazards (e.g. 900 == 0.90).
    pub gpu_mem_util_milli: u16,
    pub port: u16,
    pub runtime: VllmRuntime,
    pub extra: Vec<String>,
}

impl VllmPlan {
    /// The `--gpu-memory-utilization` value as a `0.0..=1.0` fraction.
    pub fn gpu_mem_util(&self) -> f64 {
        self.gpu_mem_util_milli as f64 / 1000.0
    }
}

/// Raw, mostly-CLI-shaped inputs for [`resolve_plan`]. Optional fields carry the
/// "derive a default" intent (`None` → inferred/defaulted inside `resolve_plan`).
pub struct PlanInputs<'a> {
    pub model: &'a str,
    pub served_name: Option<&'a str>,
    pub dtype: Option<Dtype>,
    pub tensor_parallel: u8,
    pub max_model_len: Option<u32>,
    pub gpu_mem_util: f64,
    pub port: u16,
    pub runtime: VllmRuntime,
    pub extra: Vec<String>,
}

/// Resolve every derived default into a concrete [`VllmPlan`]: served name from
/// the model's short name, dtype inferred from the checkpoint when not forced,
/// context window defaulted, and the gpu-mem-util fraction stored as milli.
/// Pure — no IO.
pub fn resolve_plan(inp: PlanInputs) -> VllmPlan {
    let served_name = inp
        .served_name
        .map(str::to_string)
        .unwrap_or_else(|| default_served_name(inp.model));
    let dtype = inp.dtype.unwrap_or_else(|| infer_dtype(inp.model, None));
    let max_model_len = inp.max_model_len.unwrap_or(DEFAULT_MAX_MODEL_LEN);
    let gpu_mem_util_milli = (inp.gpu_mem_util.clamp(0.0, 1.0) * 1000.0).round() as u16;
    VllmPlan {
        model: inp.model.to_string(),
        served_name,
        dtype,
        tensor_parallel: inp.tensor_parallel,
        max_model_len,
        gpu_mem_util_milli,
        port: inp.port,
        runtime: inp.runtime,
        extra: inp.extra,
    }
}

/// Derive the OpenAI `served-model-name` from a checkpoint id: the part after
/// the last `/`, sanitized to a safe token (reusing the pull path's sanitizer).
pub fn default_served_name(model: &str) -> String {
    let last = model.rsplit('/').next().unwrap_or(model);
    sanitize_ollama_name(last)
}

/// Treat `model` as an HF `<org>/<repo>` when it has exactly one `/` and is not
/// a filesystem path. Returns `None` for absolute/relative/home paths or any id
/// with extra slashes — those size as `Undetectable` (a local on-node path).
pub fn hf_repo_parts(model: &str) -> Option<(String, String)> {
    if model.starts_with('/') || model.starts_with('.') || model.starts_with('~') {
        return None;
    }
    let (org, repo) = model.split_once('/')?;
    if org.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((org.to_string(), repo.to_string()))
}

/// Sum the byte sizes of every weight sibling (`*.safetensors` or PyTorch
/// `*.bin`) in an HF model-API JSON (`GET /api/models/<org>/<repo>?blobs=true`).
/// Tolerates `size` or `lfs.size`, mirroring
/// [`crate::dgx_pull::parse_gguf_siblings`]. Returns 0 when the repo has no
/// sized weight files (→ the caller treats it as undetectable). Counting `.bin`
/// matters because a PyTorch-only checkpoint would otherwise size as 0 and
/// silently skip the fit pre-flight.
pub fn sum_weight_bytes(json: &serde_json::Value) -> u64 {
    let Some(siblings) = json.get("siblings").and_then(|s| s.as_array()) else {
        return 0;
    };
    siblings
        .iter()
        .filter_map(|s| {
            let path = s
                .get("rfilename")
                .and_then(|v| v.as_str())?
                .to_ascii_lowercase();
            if !(path.ends_with(".safetensors") || path.ends_with(".bin")) {
                return None;
            }
            s.get("size").and_then(|v| v.as_u64()).or_else(|| {
                s.get("lfs")
                    .and_then(|l| l.get("size"))
                    .and_then(|v| v.as_u64())
            })
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Fit pre-flight (the GLM-5.2 lesson, ported to the server case)
// ---------------------------------------------------------------------------

/// Refuse a model whose weights won't fit the *memory budget*
/// (`gpu_mem_util * ram`), reusing the pull path's [`FitVerdict`].
///
/// On the unified-memory GB10 the caller MUST pass `MemAvailable` (not
/// `MemTotal`) as `ram_bytes`, so the budget already nets out whatever the
/// other engine (Ollama) holds resident — see the design doc's cross-engine
/// contention section. `ram_bytes = None` means detection failed (best-effort).
pub fn vllm_fit_check(weight_bytes: u64, ram_bytes: Option<u64>, gpu_mem_util: f64) -> FitVerdict {
    match ram_bytes {
        None => FitVerdict::Undetectable {
            model_bytes: weight_bytes,
        },
        Some(ram) => {
            let budget = (ram as f64 * gpu_mem_util).floor() as u64;
            // Compare weights against the budget, reusing pull's verdict logic.
            fit_check(weight_bytes, Some(budget))
        }
    }
}

/// Shrink `max_model_len` until `weights + KV-cache(len)` fit under the memory
/// budget. Pure arithmetic — `kv_bytes_per_token` is supplied by the caller
/// (derived from the model's architecture), keeping this IO-free.
///
/// Returns the largest context window that fits, capped at `requested` when
/// given. Returns `0` when the weights alone already exceed the budget (the
/// caller refuses with a fit error before ever launching).
pub fn derive_max_model_len(
    weight_bytes: u64,
    ram_bytes: u64,
    gpu_mem_util: f64,
    kv_bytes_per_token: u64,
    requested: Option<u32>,
) -> u32 {
    let budget = (ram_bytes as f64 * gpu_mem_util).floor() as u64;
    if weight_bytes >= budget || kv_bytes_per_token == 0 {
        // No room for any KV cache (or no per-token cost to divide by).
        return 0;
    }
    let kv_budget = budget - weight_bytes;
    let fit_tokens = (kv_budget / kv_bytes_per_token).min(u32::MAX as u64) as u32;
    match requested {
        Some(r) => r.min(fit_tokens),
        None => fit_tokens,
    }
}

// ---------------------------------------------------------------------------
// Argv / script rendering (pure)
// ---------------------------------------------------------------------------

/// Format the gpu-memory-utilization fraction without trailing-zero noise
/// (`900 -> "0.9"`, `950 -> "0.95"`).
fn fmt_util(milli: u16) -> String {
    let s = format!("{:.3}", milli as f64 / 1000.0);
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

/// The engine flags shared by the native and container launch paths (everything
/// after the model is named). Order is stable for deterministic tests.
fn engine_args(plan: &VllmPlan) -> Vec<String> {
    let mut a = Vec::new();
    a.push("--served-model-name".to_string());
    a.push(plan.served_name.clone());
    match plan.dtype {
        Dtype::Bf16 => {
            a.push("--dtype".to_string());
            a.push("bfloat16".to_string());
        }
        other => {
            if let Some(q) = other.quantization_arg() {
                a.push("--quantization".to_string());
                a.push(q.to_string());
            }
        }
    }
    a.push("--tensor-parallel-size".to_string());
    a.push(plan.tensor_parallel.to_string());
    a.push("--max-model-len".to_string());
    a.push(plan.max_model_len.to_string());
    a.push("--gpu-memory-utilization".to_string());
    a.push(fmt_util(plan.gpu_mem_util_milli));
    a.push("--port".to_string());
    a.push(plan.port.to_string());
    a.extend(plan.extra.iter().cloned());
    a
}

/// Render the native `vllm serve …` argv.
pub fn render_vllm_argv(plan: &VllmPlan) -> Vec<String> {
    let mut argv = vec!["vllm".to_string(), "serve".to_string(), plan.model.clone()];
    argv.extend(engine_args(plan));
    argv
}

/// Render the `docker run … vllm/vllm-openai` argv. The image's entrypoint is
/// the OpenAI API server, so the model is passed as `--model` (not `serve <m>`).
pub fn vllm_docker_argv(plan: &VllmPlan) -> Vec<String> {
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--gpus".to_string(),
        "all".to_string(),
        "--ipc=host".to_string(),
        "-p".to_string(),
        format!("{p}:{p}", p = plan.port),
        "-v".to_string(),
        "$HOME/.cache/huggingface:/root/.cache/huggingface".to_string(),
        "vllm/vllm-openai:latest".to_string(),
        "--model".to_string(),
        plan.model.clone(),
    ];
    argv.extend(engine_args(plan));
    argv
}

/// The on-node state directory for vLLM pidfiles + logs. Returned unquoted with
/// a literal `$HOME` so the *remote* shell expands it; callers wrap it in
/// [`dq`] (double quotes) — never [`sh_quote`] (single quotes would freeze
/// `$HOME` as a literal directory name).
pub fn vllm_state_dir() -> &'static str {
    "$HOME/.newt/dgx/vllm"
}

/// The on-node pidfile path for a served model (under [`vllm_state_dir`]).
pub fn vllm_pidfile(served_name: &str) -> String {
    format!(
        "{}/{}.pid",
        vllm_state_dir(),
        staging_component(served_name)
    )
}

/// The on-node log path for a served model (under [`vllm_state_dir`]). `up`
/// redirects into it; `logs` tails it — both derive it from the served name so
/// they always agree.
pub fn vllm_log_path(served_name: &str) -> String {
    format!(
        "{}/{}.log",
        vllm_state_dir(),
        staging_component(served_name)
    )
}

/// Double-quote a path that contains a literal `$HOME` (or other intended shell
/// expansion) and whose remaining components are already shell-safe (the
/// sanitized served name). Unlike [`sh_quote`], this lets `$HOME` expand.
fn dq(path: &str) -> String {
    format!("\"{path}\"")
}

/// Generate the native remote script: ensure the state dir, launch the server
/// detached with `nohup`, redirect into the served-model log, and record its
/// PID. The vLLM argv is single-quoted (a model id / `--extra` arg may contain
/// spaces and has no `$` to expand); the `$HOME`-rooted state paths are
/// double-quoted so the remote shell expands `$HOME`.
pub fn vllm_remote_script(plan: &VllmPlan) -> String {
    let argv = render_vllm_argv(plan);
    let quoted: Vec<String> = argv.iter().map(|a| sh_quote(a)).collect();
    let pidfile = vllm_pidfile(&plan.served_name);
    let log_path = vllm_log_path(&plan.served_name);
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str(&format!("mkdir -p {}\n", dq(vllm_state_dir())));
    s.push_str(&format!(
        "nohup {cmd} > {log} 2>&1 &\n",
        cmd = quoted.join(" "),
        log = dq(&log_path),
    ));
    s.push_str(&format!("echo $! > {pid}\n", pid = dq(&pidfile)));
    s
}

/// Remote command for `dgx vllm down`: kill the recorded PID and remove the
/// pidfile, tolerating an already-stopped server (no pidfile → a clear message,
/// not an error).
pub fn vllm_stop_command(served_name: &str) -> String {
    let pid = dq(&vllm_pidfile(served_name));
    format!(
        "if [ -f {pid} ]; then kill \"$(cat {pid})\" 2>/dev/null || true; rm -f {pid}; \
         echo 'stopped vllm server'; else echo 'no vllm pidfile (already stopped?)'; fi"
    )
}

/// Remote command for `dgx vllm logs`: tail the served-model log and follow.
pub fn vllm_logs_command(served_name: &str, lines: u32) -> String {
    format!("tail -n {lines} -f {}", dq(&vllm_log_path(served_name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- infer_dtype -------------------------------------------------------

    #[test]
    fn infer_dtype_from_name_nvfp4() {
        assert_eq!(
            infer_dtype("nvidia/Qwen3.6-35B-A3B-NVFP4", None),
            Dtype::Nvfp4
        );
        assert_eq!(infer_dtype("some/model-fp4", None), Dtype::Nvfp4);
    }

    #[test]
    fn infer_dtype_from_name_other_quants() {
        assert_eq!(infer_dtype("org/model-FP8", None), Dtype::Fp8);
        assert_eq!(infer_dtype("org/model-AWQ", None), Dtype::Awq);
        assert_eq!(infer_dtype("org/model-GPTQ-Int4", None), Dtype::Gptq);
        assert_eq!(infer_dtype("org/model-bf16", None), Dtype::Bf16);
    }

    #[test]
    fn infer_dtype_falls_back_to_hf_config() {
        let cfg = json!({"quant_method": "modelopt"});
        assert_eq!(infer_dtype("org/plainname", Some(&cfg)), Dtype::Nvfp4);
        let cfg = json!({"quant_method": "fp8"});
        assert_eq!(infer_dtype("org/plainname", Some(&cfg)), Dtype::Fp8);
    }

    #[test]
    fn infer_dtype_name_beats_config() {
        // Explicit name wins over a conflicting config block.
        let cfg = json!({"quant_method": "fp8"});
        assert_eq!(infer_dtype("org/model-NVFP4", Some(&cfg)), Dtype::Nvfp4);
    }

    #[test]
    fn infer_dtype_defaults_to_auto() {
        assert_eq!(infer_dtype("org/vanilla-model", None), Dtype::Auto);
        let cfg = json!({"something_else": true});
        assert_eq!(infer_dtype("org/vanilla-model", Some(&cfg)), Dtype::Auto);
    }

    #[test]
    fn quantization_arg_mapping() {
        assert_eq!(Dtype::Nvfp4.quantization_arg(), Some("modelopt_fp4"));
        assert_eq!(Dtype::Fp8.quantization_arg(), Some("fp8"));
        assert_eq!(Dtype::Awq.quantization_arg(), Some("awq"));
        assert_eq!(Dtype::Gptq.quantization_arg(), Some("gptq"));
        assert_eq!(Dtype::Bf16.quantization_arg(), None);
        assert_eq!(Dtype::Auto.quantization_arg(), None);
    }

    // --- vllm_fit_check ----------------------------------------------------

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn fit_check_fits_under_budget() {
        // 22 GiB weights, 117 GiB RAM, 0.90 util -> budget ~105 GiB -> fits.
        let v = vllm_fit_check(22 * GIB, Some(117 * GIB), 0.90);
        assert!(!v.should_refuse());
        assert!(matches!(v, FitVerdict::Fits { .. }));
    }

    #[test]
    fn fit_check_exceeds_budget_even_when_under_total_ram() {
        // 110 GiB weights < 117 GiB total RAM, but > 0.90*117 ~= 105 GiB budget.
        // This is the case a MemTotal-based check would wrongly pass.
        let v = vllm_fit_check(110 * GIB, Some(117 * GIB), 0.90);
        assert!(v.should_refuse());
        assert!(matches!(v, FitVerdict::Exceeds { .. }));
    }

    #[test]
    fn fit_check_refuses_when_other_engine_holds_memory() {
        // 22 GiB weights would fit 117 GiB total, but only 30 GiB is *available*
        // (Ollama resident). Passing MemAvailable makes the check cross-engine
        // honest: budget = 0.90*30 = 27 GiB < 22? no -> still fits. Use a
        // tighter case: 25 GiB weights vs 27 GiB budget fits; 26 vs 27 fits;
        // raise weights to exceed.
        let avail = 20 * GIB; // Ollama ate most of the node.
        let v = vllm_fit_check(22 * GIB, Some(avail), 0.90);
        assert!(v.should_refuse(), "weights must exceed the shrunken budget");
    }

    #[test]
    fn fit_check_undetectable_when_ram_unknown() {
        let v = vllm_fit_check(22 * GIB, None, 0.90);
        assert!(!v.should_refuse());
        assert!(matches!(v, FitVerdict::Undetectable { .. }));
    }

    // --- derive_max_model_len ---------------------------------------------

    #[test]
    fn derive_clamps_window_to_fit_kv() {
        // budget = 0.90 * 117 GiB ; weights = 22 GiB ; kv = 1 MiB/token.
        // kv_budget ~= 83 GiB -> ~87k tokens.
        let budget = (117.0 * GIB as f64 * 0.90).floor() as u64;
        let kv_per_tok = 1024 * 1024; // 1 MiB/token
        let expected = ((budget - 22 * GIB) / kv_per_tok) as u32;
        let got = derive_max_model_len(22 * GIB, 117 * GIB, 0.90, kv_per_tok, None);
        assert_eq!(got, expected);
        assert!(got < 1_048_576, "1M request would not fit -> clamped");
    }

    #[test]
    fn derive_honors_requested_cap_when_it_fits() {
        // Tiny KV cost: a 128k request fits comfortably and is returned as-is.
        let got = derive_max_model_len(5 * GIB, 117 * GIB, 0.90, 4096, Some(131072));
        assert_eq!(got, 131072);
    }

    #[test]
    fn derive_returns_zero_when_weights_exceed_budget() {
        let got = derive_max_model_len(120 * GIB, 117 * GIB, 0.90, 1024 * 1024, None);
        assert_eq!(got, 0);
    }

    #[test]
    fn derive_returns_zero_on_zero_kv_cost() {
        let got = derive_max_model_len(10 * GIB, 117 * GIB, 0.90, 0, Some(1000));
        assert_eq!(got, 0);
    }

    // --- argv / script rendering ------------------------------------------

    fn sample_plan() -> VllmPlan {
        VllmPlan {
            model: "nvidia/Qwen3.6-35B-A3B-NVFP4".to_string(),
            served_name: "qwen3.6-35b".to_string(),
            dtype: Dtype::Nvfp4,
            tensor_parallel: 1,
            max_model_len: 262144,
            gpu_mem_util_milli: 900,
            port: 8000,
            runtime: VllmRuntime::Native,
            extra: vec![],
        }
    }

    #[test]
    fn fmt_util_trims_trailing_zeros() {
        assert_eq!(fmt_util(900), "0.9");
        assert_eq!(fmt_util(950), "0.95");
        assert_eq!(fmt_util(1000), "1");
        assert_eq!(fmt_util(875), "0.875");
    }

    #[test]
    fn render_native_argv_has_quantization_and_core_flags() {
        let argv = render_vllm_argv(&sample_plan());
        assert_eq!(argv[0], "vllm");
        assert_eq!(argv[1], "serve");
        assert_eq!(argv[2], "nvidia/Qwen3.6-35B-A3B-NVFP4");
        assert!(argv
            .windows(2)
            .any(|w| w == ["--quantization", "modelopt_fp4"]));
        assert!(argv
            .windows(2)
            .any(|w| w == ["--served-model-name", "qwen3.6-35b"]));
        assert!(argv.windows(2).any(|w| w == ["--max-model-len", "262144"]));
        assert!(argv
            .windows(2)
            .any(|w| w == ["--gpu-memory-utilization", "0.9"]));
        assert!(argv
            .windows(2)
            .any(|w| w == ["--tensor-parallel-size", "1"]));
        assert!(argv.windows(2).any(|w| w == ["--port", "8000"]));
    }

    #[test]
    fn render_argv_bf16_uses_dtype_not_quantization() {
        let mut plan = sample_plan();
        plan.dtype = Dtype::Bf16;
        let argv = render_vllm_argv(&plan);
        assert!(argv.windows(2).any(|w| w == ["--dtype", "bfloat16"]));
        assert!(!argv.iter().any(|a| a == "--quantization"));
    }

    #[test]
    fn render_argv_auto_emits_no_dtype_flag() {
        let mut plan = sample_plan();
        plan.dtype = Dtype::Auto;
        let argv = render_vllm_argv(&plan);
        assert!(!argv.iter().any(|a| a == "--quantization"));
        assert!(!argv.iter().any(|a| a == "--dtype"));
    }

    #[test]
    fn render_argv_appends_extra_verbatim() {
        let mut plan = sample_plan();
        plan.extra = vec!["--enable-chunked-prefill".to_string()];
        let argv = render_vllm_argv(&plan);
        assert_eq!(argv.last().unwrap(), "--enable-chunked-prefill");
    }

    #[test]
    fn docker_argv_uses_model_flag_and_image() {
        let argv = vllm_docker_argv(&sample_plan());
        assert_eq!(argv[0], "docker");
        assert!(argv.iter().any(|a| a == "vllm/vllm-openai:latest"));
        assert!(argv
            .windows(2)
            .any(|w| w == ["--model", "nvidia/Qwen3.6-35B-A3B-NVFP4"]));
        assert!(argv.windows(2).any(|w| w == ["-p", "8000:8000"]));
        // The native `serve <model>` shape must NOT appear in the docker path.
        assert!(!argv.iter().any(|a| a == "serve"));
    }

    #[test]
    fn remote_script_launches_detached_and_records_pid() {
        let s = vllm_remote_script(&sample_plan());
        assert!(s.starts_with("set -eu\n"));
        assert!(s.contains("mkdir -p \"$HOME/.newt/dgx/vllm\""));
        assert!(s.contains("nohup "));
        assert!(s.contains("echo $! >"));
        // Pidfile + log derive from the served name and live under the state dir.
        assert!(s.contains("qwen3.6-35b.pid"));
        assert!(s.contains("qwen3.6-35b.log"));
        // The model id is shell-quoted inside the nohup line.
        assert!(s.contains("'nvidia/Qwen3.6-35B-A3B-NVFP4'"));
    }

    #[test]
    fn remote_script_double_quotes_home_so_it_expands() {
        // Regression: the state paths MUST be double-quoted, never single-quoted
        // — single quotes freeze `$HOME` as a literal directory name and the
        // redirect/pidfile write fails on the node.
        let s = vllm_remote_script(&sample_plan());
        assert!(
            s.contains("\"$HOME/.newt/dgx/vllm/qwen3.6-35b.log\""),
            "log path must be double-quoted for $HOME expansion: {s}"
        );
        assert!(
            s.contains("\"$HOME/.newt/dgx/vllm/qwen3.6-35b.pid\""),
            "pidfile must be double-quoted for $HOME expansion: {s}"
        );
        // The single-quoted (frozen) form must NOT appear.
        assert!(!s.contains("'$HOME"), "must not single-quote $HOME: {s}");
    }

    #[test]
    fn pidfile_and_log_share_state_dir_and_sanitize() {
        // Slashes / metachars in the served name must not escape the path.
        let p = vllm_pidfile("org/weird name");
        let l = vllm_log_path("org/weird name");
        assert!(p.ends_with(".pid") && l.ends_with(".log"));
        assert!(p.starts_with(vllm_state_dir()) && l.starts_with(vllm_state_dir()));
        let filename = p.rsplit('/').next().unwrap();
        assert!(
            !filename.contains(' '),
            "name must be sanitized: {filename}"
        );
        assert!(!filename.trim_end_matches(".pid").is_empty());
    }

    #[test]
    fn stop_command_guards_missing_pidfile() {
        let c = vllm_stop_command("qwen3.6-35b");
        assert!(c.contains("if [ -f \"$HOME/.newt/dgx/vllm/qwen3.6-35b.pid\" ]"));
        assert!(c.contains("kill \"$(cat "));
        assert!(c.contains("rm -f"));
        // No error when already stopped.
        assert!(c.contains("already stopped"));
        assert!(!c.contains("'$HOME"));
    }

    #[test]
    fn logs_command_tails_and_follows() {
        let c = vllm_logs_command("qwen3.6-35b", 50);
        assert!(c.contains("tail -n 50 -f \"$HOME/.newt/dgx/vllm/qwen3.6-35b.log\""));
        assert!(!c.contains("'$HOME"));
    }

    // --- dtype parsing / plan resolution ----------------------------------

    #[test]
    fn dtype_parse_accepts_known_tokens() {
        assert_eq!(Dtype::parse("auto"), Some(Dtype::Auto));
        assert_eq!(Dtype::parse("NVFP4"), Some(Dtype::Nvfp4));
        assert_eq!(Dtype::parse("fp4"), Some(Dtype::Nvfp4));
        assert_eq!(Dtype::parse("bfloat16"), Some(Dtype::Bf16));
        assert_eq!(Dtype::parse("gptq"), Some(Dtype::Gptq));
        assert_eq!(Dtype::parse("nonsense"), None);
    }

    #[test]
    fn resolve_plan_defaults_served_name_and_infers_dtype() {
        let plan = resolve_plan(PlanInputs {
            model: "nvidia/Qwen3.6-35B-A3B-NVFP4",
            served_name: None,
            dtype: None,
            tensor_parallel: 1,
            max_model_len: None,
            gpu_mem_util: 0.90,
            port: 8000,
            runtime: VllmRuntime::Native,
            extra: vec![],
        });
        // Served name derived from the repo short name (sanitized).
        assert_eq!(
            plan.served_name,
            default_served_name("nvidia/Qwen3.6-35B-A3B-NVFP4")
        );
        // Dtype inferred NVFP4 from the name.
        assert_eq!(plan.dtype, Dtype::Nvfp4);
        // Window defaulted; util stored as milli.
        assert_eq!(plan.max_model_len, DEFAULT_MAX_MODEL_LEN);
        assert_eq!(plan.gpu_mem_util_milli, 900);
    }

    #[test]
    fn resolve_plan_honors_explicit_overrides() {
        let plan = resolve_plan(PlanInputs {
            model: "org/whatever",
            served_name: Some("my-name"),
            dtype: Some(Dtype::Fp8),
            tensor_parallel: 2,
            max_model_len: Some(131072),
            gpu_mem_util: 0.95,
            port: 9001,
            runtime: VllmRuntime::Docker,
            extra: vec!["--x".to_string()],
        });
        assert_eq!(plan.served_name, "my-name");
        assert_eq!(plan.dtype, Dtype::Fp8);
        assert_eq!(plan.tensor_parallel, 2);
        assert_eq!(plan.max_model_len, 131072);
        assert_eq!(plan.gpu_mem_util_milli, 950);
        assert_eq!(plan.port, 9001);
        assert_eq!(plan.runtime, VllmRuntime::Docker);
    }

    #[test]
    fn gpu_mem_util_is_clamped_to_unit_interval() {
        let plan = resolve_plan(PlanInputs {
            model: "org/m",
            served_name: None,
            dtype: Some(Dtype::Auto),
            tensor_parallel: 1,
            max_model_len: Some(1),
            gpu_mem_util: 1.5, // nonsense > 1.0
            port: 8000,
            runtime: VllmRuntime::Native,
            extra: vec![],
        });
        assert_eq!(plan.gpu_mem_util_milli, 1000);
    }

    // --- HF repo detection / weight sizing --------------------------------

    #[test]
    fn hf_repo_parts_accepts_org_repo_only() {
        assert_eq!(
            hf_repo_parts("nvidia/Qwen3.6-35B-A3B-NVFP4"),
            Some(("nvidia".to_string(), "Qwen3.6-35B-A3B-NVFP4".to_string()))
        );
        // Filesystem paths and extra-slash ids are not HF repos.
        assert_eq!(hf_repo_parts("/data/models/foo"), None);
        assert_eq!(hf_repo_parts("./local"), None);
        assert_eq!(hf_repo_parts("~/m"), None);
        assert_eq!(hf_repo_parts("org/sub/repo"), None);
        assert_eq!(hf_repo_parts("bareword"), None);
    }

    #[test]
    fn sum_weight_bytes_sums_safetensors_and_bin() {
        let json = json!({
            "siblings": [
                {"rfilename": "model-00001-of-00002.safetensors", "size": 1000},
                {"rfilename": "model-00002-of-00002.safetensors", "lfs": {"size": 2000}},
                {"rfilename": "config.json", "size": 50},
                {"rfilename": "tokenizer.json", "size": 99},
            ]
        });
        assert_eq!(sum_weight_bytes(&json), 3000);
    }

    #[test]
    fn sum_weight_bytes_counts_pytorch_bin_only_repo() {
        // A PyTorch-only checkpoint (no safetensors) must still size, or the
        // fit pre-flight would silently skip it.
        let json = json!({
            "siblings": [
                {"rfilename": "pytorch_model-00001-of-00002.bin", "size": 4000},
                {"rfilename": "pytorch_model-00002-of-00002.bin", "lfs": {"size": 5000}},
                {"rfilename": "config.json", "size": 50},
            ]
        });
        assert_eq!(sum_weight_bytes(&json), 9000);
    }

    #[test]
    fn sum_weight_bytes_zero_when_none() {
        let json = json!({"siblings": [{"rfilename": "README.md", "size": 10}]});
        assert_eq!(sum_weight_bytes(&json), 0);
        assert_eq!(sum_weight_bytes(&json!({})), 0);
    }
}
