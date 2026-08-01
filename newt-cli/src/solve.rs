//! `newt solve` — the headless, non-interactive entry that drives the agentic
//! loop to solve one task and emits a trace, for Terminal-Bench (epic #1419 /
//! the release-champion ceremony, WS1).
//!
//! It is a THIN wrapper over the same [`TurnDriver`] / `chat_complete` loop the
//! interactive TUI runs — no second loop. Headless contract:
//! `permission_gate: None` (a capability denial fails the call, never hangs) and
//! caveats default to [`Caveats::top`] (unconfined).
//!
//! Two headless lanes, selected up front:
//!
//! - **`--non-interactive` (the `--yolo` lane, default):** sets
//!   `NEWT_FULL_ACCESS=1` + `NEWT_DISABLE_OCAP=1` so the host shell runs and no
//!   prompt can appear — the bootstrap lane that isolated the agentic variable
//!   from the confinement variable while the bench floor was established.
//! - **`--confined` / `NEWT_BENCH_OCAP=on` (the OCAP-on lane):** OCAP stays ON.
//!   Instead of full access, [`confined_bench_caveats`] seeds a workspace-fenced
//!   authority — reads/exec/net stay open, but writes are confined to the
//!   workspace and the container's mutable system roots (a `Scope::Only`
//!   fs_write, never `Scope::All`). A `Scope::Only` write auto-consents at the
//!   tool gate (the preset IS the operator's consent — see
//!   `tools::confirm_unrestricted_fs_mutation`), so in-fence writes run with no
//!   prompt. This is the lane the 0.7.6 OCAP-parity gate measures against the
//!   `--yolo` scores.
//!
//! **Scope of the write fence (be honest about it).** The fence enforces newt's
//! own `write_file`/`edit_file` tool gate (`tools::tui_permits_path`), where an
//! out-of-fence write is denied. It does NOT confine writes performed by
//! programs the agent *spawns* (`exec` is `Scope::All`): confining those needs
//! the kernel L3 fence (Landlock), and this lane forces the brush engine even
//! when Landlock is absent, so a spawned command's writes are then advisory, not
//! kernel-enforced. Combined with `fs_read = All` + `net = All`, this lane is a
//! **bench isolation control for disposable containers, not a security sandbox**
//! against a hostile agent. The fence is also deliberately broad on this first
//! cut (workspace + standard mutable roots); tightening it is a later ratchet.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::{BackendKind, Config, TurnDriver, TurnDriverConfig, TurnStatus};

use crate::solve_contract;

/// Parsed `newt solve` arguments (mirrors the `Command::Solve` fields).
pub struct SolveArgs {
    pub cwd: PathBuf,
    pub instruction_file: PathBuf,
    pub profile: Option<PathBuf>,
    pub non_interactive: bool,
    /// OCAP-ON confined bench lane: keep OCAP enabled and seed a workspace-fenced
    /// caveat (see [`confined_bench_caveats`]) instead of the `--yolo` full-access
    /// lane. Also enabled by the `NEWT_BENCH_OCAP=on` env twin (so the Harbor
    /// adapter can flip it without a flag). Supersedes `non_interactive`'s
    /// OCAP-off behaviour when set.
    pub confined: bool,
    pub events: Option<PathBuf>,
    pub max_rounds: Option<usize>,
    /// The served model's FULL context window (e.g. llama.cpp `--ctx-size`).
    /// newt reserves ~20% for the reply and gates input at 80% of it, so a
    /// long turn compacts under the window instead of overrunning it during
    /// generation (the "Context size has been exceeded" 500s). None keeps
    /// newt's default.
    pub context_window: Option<usize>,
    /// Operator-supplied sha256 of the weights actually served, for the
    /// contract record's `model_digest` (W0 #1511). Also settable via the
    /// `NEWT_MODEL_DIGEST` env twin. `None` ⇒ the field is OMITTED from the
    /// record — never fabricated (a name is not an identity, and a made-up
    /// digest would defeat the silent-re-upload detection the field exists
    /// for). Local-weights derivation would only apply to the embedded
    /// backend, which `solve` cannot drive (it needs an HTTP endpoint).
    pub model_digest: Option<String>,
}

/// Which headless lane `newt solve` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadlessLane {
    /// OCAP on, workspace-fenced tool writes (`--confined` / `NEWT_BENCH_OCAP=on`).
    Confined,
    /// OCAP off, full host access (`--non-interactive`, the default).
    Yolo,
    /// Neither — OCAP on with no gate; out-of-grant writes deny silently.
    Neither,
}

/// Resolve the headless lane purely from the flags + the `NEWT_BENCH_OCAP` env
/// twin, so precedence is unit-tested without a live run. `--confined` (or a
/// trimmed, case-insensitive `NEWT_BENCH_OCAP=on`) wins over the `--yolo`
/// default — the parity run flips the env from the adapter without also
/// unsetting `--non-interactive`. The `.trim()` keeps a `"on "` from a shell or
/// YAML value from silently missing the lane.
fn resolve_lane(
    confined_flag: bool,
    ocap_env: Option<&str>,
    non_interactive: bool,
) -> HeadlessLane {
    let confined = confined_flag || ocap_env.is_some_and(|v| v.trim().eq_ignore_ascii_case("on"));
    if confined {
        HeadlessLane::Confined
    } else if non_interactive {
        HeadlessLane::Yolo
    } else {
        HeadlessLane::Neither
    }
}

/// Resolve the operator-supplied model digest: the `--model-digest` flag wins
/// over the `NEWT_MODEL_DIGEST` env twin (the same flag/env pattern as the
/// lane); blank values fall through. Pure so precedence is unit-tested
/// without racing the process environment. NEVER derives or invents a digest.
fn resolve_model_digest(flag: Option<&str>, env: Option<&str>) -> Option<String> {
    [flag, env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|d| !d.is_empty())
        .map(str::to_string)
}

/// Run one task headless and emit its trace. Returns the process exit code:
/// `0` when the turn completed, `1` on an infrastructure/turn failure. (Task
/// pass/fail is Terminal-Bench's job via the task's own verification — this exit
/// code is only "did the agent run cleanly".)
pub async fn run(args: SolveArgs) -> Result<i32> {
    // 1. Config: an explicit --profile is a FILE (Config::load); else the normal
    //    search order (Config::resolve — honors disk drop-ins + --backend-*).
    let cfg = match &args.profile {
        Some(path) => {
            Config::load(path).with_context(|| format!("loading --profile {}", path.display()))?
        }
        None => Config::resolve().context("resolving config")?,
    };

    // 2. Choose the headless lane (pure resolution → unit-tested precedence),
    //    then apply its process-env setup.
    //    SAFETY: single-threaded before the driver spawns its turn thread.
    let ocap_env = std::env::var("NEWT_BENCH_OCAP").ok();
    let lane = resolve_lane(args.confined, ocap_env.as_deref(), args.non_interactive);
    match lane {
        HeadlessLane::Confined => {
            // Keep OCAP enabled. DEFENSIVELY clear any inherited
            // `NEWT_DISABLE_OCAP` / `NEWT_FULL_ACCESS` — `ocap_disabled()` reads
            // those straight from the process env, so an inherited `=1` (a
            // wrapper/pod that also runs the `--yolo` lane) would silently route
            // every command to the host shell and void this lane's confinement
            // contract. Then pin the brush shell engine so compound commands run
            // even when the Landlock L3 fence is unavailable in the container (the
            // SafeSubset fallback refuses that grammar). Caveat seeded on `dc`.
            unsafe {
                std::env::remove_var("NEWT_DISABLE_OCAP");
                std::env::remove_var("NEWT_FULL_ACCESS");
                std::env::set_var("NEWT_SHELL_ENGINE", "brush");
            }
        }
        HeadlessLane::Yolo => {
            // The `--yolo --full-access` bootstrap lane: full access
            // (Caveats::top) AND OCAP disabled, so an UNRESTRICTED fs write
            // auto-accepts instead of waiting on a (nonexistent) prompt gate and
            // silently denying — the write path the benchmark depends on.
            unsafe {
                std::env::set_var("NEWT_FULL_ACCESS", "1");
                std::env::set_var("NEWT_DISABLE_OCAP", "1");
            }
        }
        HeadlessLane::Neither => {
            // OCAP on with no headless prompt gate, so out-of-grant writes deny
            // silently (the WS1 trap). Only reachable via `--non-interactive
            // false` without `--confined`; warn rather than fail mysteriously.
            tracing::warn!(
                "newt solve: neither --confined nor --non-interactive is set; OCAP \
                 is on with no prompt gate, so writes outside the default grant \
                 will be denied silently. Pass --confined for the fenced lane."
            );
        }
    }

    // 3. Backend: honor NEWT_PROVIDER, else the first backend with an endpoint.
    let backend = pick_backend(&cfg)
        .context("no usable backend in config (set one in the --profile [[backends]] or via --backend-endpoint)")?;
    let url = backend.endpoint.clone();
    let model = backend
        .effective_model()
        .context("backend has no model (set model = in the [[backends]] entry)")?
        .to_string();
    let kind = backend.kind.unwrap_or(BackendKind::Openai);
    let api_key = backend.resolve_api_key();
    // Surface the backend's wire API (`api = "responses"`) to the agentic loop,
    // exactly as the chat path does — without this, a responses-only model like
    // gpt-5.6-sol is driven over /v1/chat/completions and 400s on function tools.
    newt_tui::apply_openai_api_env(backend.api.unwrap_or_default());

    // #tenacity: attribute the model's family so a per-family `[tenacity]` config
    // default applies to this run (an explicit `--tenacity` still supersedes it).
    // The card's `family` if a built-in card names one, else a family inferred
    // from the model NAME against the configured `[tenacity.families]` keys — so
    // the model matrix (qwen3/gemma/nemotron/…) works from config without a card
    // per model.
    let card_family = newt_core::model_card::builtin_card(&model).and_then(|c| c.family);
    let family = cfg
        .tenacity
        .as_ref()
        .and_then(|t| t.family_for(&model, card_family.as_deref()))
        .or(card_family);
    // W0 (#1511): the LEVEL this run resolves to, recorded verbatim in the
    // contract's effective_config — the bench never re-derives it from a
    // profile (contract requirement 5).
    let tenacity_level = cfg
        .tenacity
        .clone()
        .unwrap_or_default()
        .resolve(family.as_deref())
        .to_string();
    newt_core::tenacity::set_active_model_family(family);

    // 4. The task instruction.
    let instruction = std::fs::read_to_string(&args.instruction_file).with_context(|| {
        format!(
            "reading --instruction-file {}",
            args.instruction_file.display()
        )
    })?;
    let workspace = args
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| args.cwd.clone())
        .to_string_lossy()
        .into_owned();

    // 5. Drive one full turn (== a complete multi-round agentic solve).
    let mut dc = TurnDriverConfig::new(&url, &model, kind, &workspace);
    dc.api_key = api_key;
    dc.chat_completions_capability = backend.chat_completions_capability();
    dc.reasoning_replay_scope = backend.reasoning_replay_scope();
    if let Some(r) = args.max_rounds {
        dc.max_tool_rounds = r;
    }
    // OCAP-ON confined lane: replace the default unconfined caveat with a
    // workspace-fenced authority. The tool gate consults `dc.caveats` and the
    // permission_gate stays `None` — an in-fence write auto-consents; an
    // out-of-fence tool write is denied. This is the only seam the confined lane
    // touches; the driver + tool layer are unchanged.
    if lane == HeadlessLane::Confined {
        dc.caveats = confined_bench_caveats(&workspace);
    }
    // Pin the model's served context window so the loop's pre-send guard +
    // compaction keep each request under the backend's `--ctx-size` (e.g. dgx1
    // llama.cpp serves qwen3-coder at 32768). `--context-window` is the FULL
    // served window; the input budget is 80% of it, RESERVING ~20% for the
    // reply — the server's KV window is shared by input+output, so gating on the
    // full window (no headroom) overruns it during generation and 500s (that was
    // the leak). This matches the workspace convention that `safe_context` is
    // the 80%-discounted window (mirrors the Ollama input-ceiling path). num_ctx
    // is inert on the OpenAI wire but kept for the Ollama path.
    if let Some(cw) = args.context_window {
        let cw = cw as u32;
        let input_budget = (u64::from(cw) * 80 / 100) as u32;
        dc.safe_context = Some(input_budget);
        dc.max_ok_input = Some(input_budget);
        dc.num_ctx = Some(cw);
    }
    // Captured before `dc` moves into the driver: the cap the run ACTUALLY
    // uses (post `--max-rounds`), for the contract's effective_config.
    let max_rounds = dc.max_tool_rounds as u32;
    let mut driver = TurnDriver::new(dc);
    let started = Instant::now();
    driver
        .submit(instruction.trim())
        .map_err(|e| anyhow::anyhow!("submit failed: {e:?}"))?;

    let outcome = loop {
        match driver.poll() {
            TurnStatus::Completed(o) => break Ok(o),
            TurnStatus::Failed(e) => break Err(e),
            TurnStatus::Idle | TurnStatus::Running => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };
    let elapsed = started.elapsed();
    let wall_secs = elapsed.as_secs_f64();
    // Contract timing is integral milliseconds.
    let wall_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

    // 6. Emit the trace record (one JSONL line), including the per-tool-call
    //    trajectory (name/args-digest/ok/duration) the TurnDriver now lends —
    //    the material for the failure taxonomy.
    // A part-way inference failure now arrives as `Ok(o)` with `o.error` set —
    // so its PARTIAL trajectory is preserved (an infra failure must not report
    // the agent as having done nothing). `Err` is only a spawn/thread failure
    // with no trajectory at all.
    let o_opt = outcome.as_ref().ok();
    let clean = matches!(&outcome, Ok(o) if o.error.is_none());
    let (status, error) = match &outcome {
        Ok(o) if o.error.is_none() => ("completed", None),
        Ok(o) => ("failed", o.error.clone()),
        Err(e) => ("failed", Some(e.clone())),
    };
    let reply_chars = o_opt.map(|o| o.reply.len()).unwrap_or(0);
    let usage = o_opt.and_then(|o| o.usage.as_ref().map(|u| u.total()));
    let halluc = o_opt.map(|o| o.hallucinations).unwrap_or(0);
    // The per-tool trajectory — the material for the failure taxonomy. The
    // single highest-signal field is `write_calls`: a failed task with 0 writes
    // never ACTED (the tenacity target); with writes it acted but wrong. Only
    // newt's real workspace-write tools count — `write_file`/`edit_file` (the
    // `is_workspace_write_call` set); aliases like `create_file`/`str_replace`/
    // `apply_patch` get a coaching reply and never modify the tree.
    let (tool_calls, write_calls, end_reason, trajectory) = match o_opt {
        Some(o) => {
            let names: Vec<&str> = o.tool_events.iter().map(|e| e.tool.as_str()).collect();
            let writes = names
                .iter()
                .filter(|n| matches!(**n, "write_file" | "edit_file"))
                .count();
            (
                names.len(),
                writes,
                format!("{:?}", o.end_reason),
                serde_json::to_value(&o.tool_events).unwrap_or(serde_json::Value::Null),
            )
        }
        None => (0, 0, "None".to_string(), serde_json::Value::Null),
    };
    let record = serde_json::json!({
        "kind": "solve_result",
        "task_file": args.instruction_file.to_string_lossy(),
        "cwd": workspace,
        "model": model,
        "endpoint": url,
        "backend_kind": kind.label(),
        "status": status,
        "reply_chars": reply_chars,
        "usage_total_tokens": usage,
        "hallucinations": halluc,
        "wall_secs": wall_secs,
        "tool_calls": tool_calls,
        "write_calls": write_calls,
        "end_reason": end_reason,
        "trajectory": trajectory,
        "error": error,
    });
    // 7. W0 (#1511): the per-round parse-signal trace events plus EXACTLY ONE
    //    contract record (the `contract_version` key marks it — the external
    //    evaluator rejects a trace with zero or several), appended alongside
    //    the solve_result line above, never replacing it.
    let mut trace_lines: Vec<serde_json::Value> = vec![record];
    if let Some(o) = o_opt {
        trace_lines.extend(
            o.parse_signals
                .iter()
                .map(solve_contract::parse_signal_line),
        );
    }
    // Outcome: structural, from the TYPED class the driver carried over. A
    // spawn/thread `Err` never reached a dispatch → no class → harness_error.
    let outcome_label = solve_contract::outcome_label(
        clean,
        match &outcome {
            Ok(o) => o.error_class,
            Err(_) => None,
        },
    );
    // effective_model: the response body's `model` field when the backend
    // reported one (the served reality), else the request model — a turn
    // that never got a response body has nothing truer to report.
    let effective_model = o_opt
        .and_then(|o| o.served_model.clone())
        .unwrap_or_else(|| model.clone());
    // Digest: operator-supplied only (flag > NEWT_MODEL_DIGEST env twin);
    // absent ⇒ the field is omitted — never fabricated.
    let digest_env = std::env::var("NEWT_MODEL_DIGEST").ok();
    let model_digest = resolve_model_digest(args.model_digest.as_deref(), digest_env.as_deref());
    trace_lines.push(solve_contract::contract_record(
        &solve_contract::ContractInputs {
            requested_model: &model,
            effective_model: &effective_model,
            model_digest: model_digest.as_deref(),
            backend_name: &backend.name,
            backend_kind: kind.label(),
            outcome: outcome_label,
            context_window: args.context_window.map(|c| c as u32),
            tenacity: &tenacity_level,
            ocap: if lane == HeadlessLane::Yolo {
                "off"
            } else {
                "on"
            },
            max_rounds,
            wall_ms,
            gen_tokens: o_opt
                .and_then(|o| o.usage.as_ref())
                .map(|u| u64::from(u.output_tokens)),
        },
    ));
    if let Some(path) = &args.events {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening --events {}", path.display()))?;
        for line in &trace_lines {
            writeln!(f, "{line}").context("writing events line")?;
        }
    }
    // Always echo the trace to stdout too, so a manual bootstrap run is legible.
    for line in &trace_lines {
        println!("{line}");
    }

    // Clean completion → 0; any failure (inference error carried on the outcome,
    // or a spawn/thread Err) → 1.
    Ok(if clean { 0 } else { 1 })
}

/// Pick the backend to drive. Delegates to the **shared** config precedence
/// (#1320, PR-3) so `solve` selects exactly as chat + the worker do:
/// `NEWT_PROVIDER` > `default_backend` > sole > prefer-OpenAI, else first usable.
fn pick_backend(cfg: &Config) -> Option<&newt_core::config::BackendConfig> {
    cfg.select_configured_backend()
}

/// The workspace-fenced authority for the OCAP-ON bench lane.
///
/// Reads / exec / net stay fully open (a bench task legitimately reads the
/// whole tree, runs arbitrary toolchains, and installs packages over the
/// network), but **writes are fenced** to the workspace plus the mutable system
/// roots a container's package managers and toolchains need (`/tmp`, `/usr`,
/// `/usr/local`, `/var`, `/etc`, `/opt`, `/root`, `/home`) — plus any per-task
/// `NEWT_WRITE_PATHS` grant. The fence is a `Scope::Only`, **never**
/// `Scope::All`: that is the whole point of the lane (a `Scope::Only` write
/// auto-consents at the tool gate, so in-fence writes run promptless while a
/// write to an un-granted absolute path fails closed), and it is what the 0.7.6
/// OCAP-parity gate measures. The fence is deliberately broad on this first cut
/// so parity isolates *"does routing every op through the caveat lattice + the
/// bridled shell break tasks?"* from *"is the fence too tight?"*; tightening
/// per-task is a later ratchet.
///
/// Matching at the enforcement site (`tools::tui_permits_path`) is by
/// lexically-normalized path **prefix**, so a root entry covers everything
/// beneath it (`/usr` grants `/usr/lib/python3/...`).
fn confined_bench_caveats(workspace: &str) -> Caveats {
    // The per-task extra write grants the harness may pass (same env the
    // interactive `--write` grants flow through). `split_paths` keeps a Windows
    // drive-letter grant intact rather than shattering it on `:`. Read here so
    // the pure core below stays env-free (and unit-testable without racing the
    // process-global environment).
    let extra: Vec<String> = std::env::var_os("NEWT_WRITE_PATHS")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    confined_bench_caveats_with_grants(workspace, &extra)
}

/// Pure core of [`confined_bench_caveats`]: the workspace fence plus explicit
/// extra write roots, with no environment access (so it is deterministic and
/// parallel-safe to test).
fn confined_bench_caveats_with_grants(workspace: &str, extra_write_roots: &[String]) -> Caveats {
    let mut write_roots: Vec<String> = [
        workspace,
        "/tmp",
        "/usr",
        "/usr/local",
        "/var",
        "/etc",
        "/opt",
        "/root",
        "/home",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    write_roots.extend(extra_write_roots.iter().cloned());
    // Built axis-by-axis rather than narrowing `Caveats::top()`: a headless
    // dispatch path must not even MENTION `Caveats::top()` (the `#94` no-top-leak
    // guard, `newt-acp-worker/tests/no_top_leak.rs`). Reads/exec/net and the call
    // budget are the open top of their axes; ONLY fs_write is fenced.
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only(write_roots),
        exec: Scope::All,
        net: Scope::All,
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::BackendConfig;

    fn backend(name: &str, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            ..Default::default()
        }
    }

    #[test]
    fn pick_backend_skips_endpointless_and_takes_the_first_usable() {
        // Deterministic: no NEWT_PROVIDER selection path.
        // SAFETY: single-threaded test; restore is not needed (we only remove).
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            backends: vec![
                backend("embedded-no-endpoint", ""),
                backend("dgx", "http://router:8080"),
                backend("other", "http://other:9000"),
            ],
            ..Default::default()
        };
        let chosen = pick_backend(&cfg).expect("a usable backend exists");
        assert_eq!(
            chosen.name, "dgx",
            "skip the endpointless one, take the first usable"
        );
    }

    #[test]
    fn pick_backend_none_when_no_endpoints() {
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            backends: vec![backend("a", ""), backend("b", "")],
            ..Default::default()
        };
        assert!(pick_backend(&cfg).is_none());
    }

    /// W0 (#1511): digest resolution is flag > env twin, blank falls through,
    /// and NOTHING is ever derived — no value in ⇒ no digest out.
    #[test]
    fn resolve_model_digest_flag_beats_env_and_never_invents() {
        assert_eq!(
            resolve_model_digest(Some("sha-flag"), Some("sha-env")).as_deref(),
            Some("sha-flag")
        );
        assert_eq!(
            resolve_model_digest(None, Some(" sha-env ")).as_deref(),
            Some("sha-env"),
            "env twin used when no flag; whitespace trimmed"
        );
        assert_eq!(
            resolve_model_digest(Some("  "), Some("sha-env")).as_deref(),
            Some("sha-env"),
            "a blank flag falls through to the env twin"
        );
        assert_eq!(resolve_model_digest(None, None), None);
        assert_eq!(resolve_model_digest(Some(""), Some("")), None);
    }

    #[test]
    fn pick_backend_honors_default_backend_over_first_endpoint() {
        // #1320: `--config X` sets `default_backend`; solve must route to it, not to
        // the first endpoint-bearing entry (the coincidence that masked the bug).
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            default_backend: Some("sol".to_string()),
            backends: vec![
                backend("other", "https://other:9000"), // first + endpoint-bearing
                backend("sol", "https://api.openai.com"),
            ],
            ..Default::default()
        };
        assert_eq!(
            pick_backend(&cfg).expect("a backend").name,
            "sol",
            "default_backend wins over the first endpoint-bearing entry"
        );
    }

    #[test]
    fn pick_backend_default_backend_falls_through_when_unusable() {
        // A `default_backend` naming an endpointless entry is skipped for the first
        // usable one, rather than returning an unreachable backend.
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let cfg = Config {
            default_backend: Some("ghost".to_string()),
            backends: vec![backend("ghost", ""), backend("real", "http://r:1")],
            ..Default::default()
        };
        assert_eq!(pick_backend(&cfg).expect("a backend").name, "real");
    }

    #[test]
    fn resolve_lane_precedence_and_trim() {
        use HeadlessLane::*;
        // --confined flag wins regardless of the others.
        assert_eq!(resolve_lane(true, None, true), Confined);
        assert_eq!(resolve_lane(true, Some("off"), false), Confined);
        // NEWT_BENCH_OCAP=on (trimmed, case-insensitive) selects the confined lane.
        assert_eq!(resolve_lane(false, Some("on"), true), Confined);
        assert_eq!(resolve_lane(false, Some(" ON "), true), Confined);
        assert_eq!(resolve_lane(false, Some("On"), false), Confined);
        // Any non-`on` env value is NOT confined — it does not silently confine,
        // and (with --non-interactive) it is the yolo lane.
        assert_eq!(resolve_lane(false, Some("1"), true), Yolo);
        assert_eq!(resolve_lane(false, Some("true"), true), Yolo);
        assert_eq!(resolve_lane(false, None, true), Yolo);
        // Neither flag nor env → the warn lane.
        assert_eq!(resolve_lane(false, None, false), Neither);
        assert_eq!(resolve_lane(false, Some("off"), false), Neither);
    }

    use newt_core::caveats::CaveatsExt;

    /// The load-bearing invariant of the OCAP-on lane: writes are FENCED, never
    /// unrestricted. A `Scope::All` fs_write would (a) drop us back to the
    /// unconfined `--yolo` behaviour and (b) route through the y/N-gated
    /// `confirm_unrestricted_fs_mutation` path (which, with `permission_gate:
    /// None`, silently DENIES) — the exact WS1 trap the lane exists to avoid.
    #[test]
    fn confined_caveats_fence_writes_never_all() {
        let cv = confined_bench_caveats_with_grants("/app/task", &[]);
        assert!(
            !matches!(cv.fs_write, Scope::All),
            "confined lane must NEVER grant fs_write = Scope::All"
        );
        assert!(
            matches!(cv.fs_write, Scope::Only(_)),
            "confined fs_write must be an explicit Scope::Only fence"
        );
        // Reads / exec / net stay wide so the bench isn't crippled — parity
        // isolates the enforcement PATH, not fence tightness.
        assert!(matches!(cv.fs_read, Scope::All), "reads stay open");
        assert!(matches!(cv.exec, Scope::All), "exec stays open");
        assert!(matches!(cv.net, Scope::All), "net stays open");
    }

    #[test]
    fn confined_caveats_permit_workspace_and_scratch_writes() {
        let cv = confined_bench_caveats_with_grants("/app/task", &[]);
        // The workspace root and the standard mutable roots are writable
        // (exact-match here; the enforcement site adds prefix coverage).
        assert!(cv.permits_fs_write("/app/task"), "workspace root writable");
        assert!(cv.permits_fs_write("/tmp"), "scratch writable");
        assert!(cv.permits_fs_write("/usr"), "package-manager root writable");
        // An un-granted absolute path outside every root is NOT writable.
        assert!(
            !cv.permits_fs_write("/boot/vmlinuz"),
            "a path outside every granted root fails closed"
        );
    }

    #[test]
    fn confined_caveats_honor_write_paths_grant() {
        let cv = confined_bench_caveats_with_grants("/app/task", &["/data/scratch".to_string()]);
        assert!(
            cv.permits_fs_write("/data/scratch"),
            "a per-task NEWT_WRITE_PATHS grant joins the fence"
        );
    }
}
