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

use crate::dgx_pull::{fit_check, sh_quote, FitVerdict};

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

/// The on-node pidfile path for a served model (under `~/.newt/dgx/vllm`).
pub fn vllm_pidfile(served_name: &str) -> String {
    format!(
        "$HOME/.newt/dgx/vllm/{}.pid",
        crate::dgx_pull::staging_component(served_name)
    )
}

/// Generate the native remote script: ensure the state dir, launch the server
/// detached with `nohup`, and record its PID. `log_path` is where stdout/stderr
/// is redirected (the `logs` verb tails it). The argv is shell-quoted so a model
/// id or `--extra` arg with spaces survives the remote shell.
pub fn vllm_remote_script(plan: &VllmPlan, log_path: &str) -> String {
    let argv = render_vllm_argv(plan);
    let quoted: Vec<String> = argv.iter().map(|a| sh_quote(a)).collect();
    let pidfile = vllm_pidfile(&plan.served_name);
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str("mkdir -p \"$HOME/.newt/dgx/vllm\"\n");
    s.push_str(&format!(
        "nohup {cmd} > {log} 2>&1 &\n",
        cmd = quoted.join(" "),
        log = sh_quote(log_path),
    ));
    s.push_str(&format!("echo $! > {pid}\n", pid = sh_quote(&pidfile)));
    s
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
        let s = vllm_remote_script(&sample_plan(), "$HOME/.newt/dgx/vllm/qwen3.6-35b.log");
        assert!(s.starts_with("set -eu\n"));
        assert!(s.contains("mkdir -p \"$HOME/.newt/dgx/vllm\""));
        assert!(s.contains("nohup "));
        assert!(s.contains("echo $! >"));
        // Pidfile derives from the served name.
        assert!(s.contains("qwen3.6-35b.pid"));
        // The model id is shell-quoted inside the nohup line.
        assert!(s.contains("'nvidia/Qwen3.6-35B-A3B-NVFP4'"));
    }

    #[test]
    fn pidfile_sanitizes_served_name() {
        // Slashes / metachars in the served name must not escape the path: the
        // filename component (after the last '/') is the sanitized name + .pid.
        let p = vllm_pidfile("org/weird name");
        assert!(p.ends_with(".pid"));
        assert!(p.contains(".newt/dgx/vllm/"));
        let filename = p.rsplit('/').next().unwrap();
        assert!(
            !filename.contains(' '),
            "name must be sanitized: {filename}"
        );
        let stem = filename.trim_end_matches(".pid");
        assert!(!stem.is_empty());
    }
}
