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
//!   prompt while out-of-fence writes fail closed. This is the lane the 0.7.6
//!   OCAP-parity gate measures against the `--yolo` scores.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use newt_core::caveats::{Caveats, CountBound, Scope};
use newt_core::{BackendKind, Config, TurnDriver, TurnDriverConfig, TurnStatus};

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

    // 2. Choose the headless lane. `--confined` / `NEWT_BENCH_OCAP=on` wins over
    //    the `--yolo` default so the OCAP-parity run can flip the variable from
    //    the adapter without also unsetting `--non-interactive`.
    //    SAFETY: single-threaded before the driver spawns its turn thread.
    let confined = args.confined
        || std::env::var("NEWT_BENCH_OCAP")
            .ok()
            .is_some_and(|v| v.eq_ignore_ascii_case("on"));
    if confined {
        // OCAP-ON lane: do NOT disable OCAP or grant full access. Pin the brush
        // shell engine so compound commands (pipes / `&&` / `$()`) run even when
        // the Landlock L3 fence is unavailable in the container — the SafeSubset
        // fallback engine structurally refuses that grammar. The workspace-fenced
        // caveat is seeded onto `dc` below (needs the canonical workspace).
        unsafe {
            std::env::set_var("NEWT_SHELL_ENGINE", "brush");
        }
    } else if args.non_interactive {
        // The `--yolo --full-access` bootstrap lane: full access (Caveats::top)
        // AND OCAP disabled, so an UNRESTRICTED fs write auto-accepts instead of
        // waiting on a (nonexistent) prompt gate and silently denying — the write
        // path the benchmark depends on.
        unsafe {
            std::env::set_var("NEWT_FULL_ACCESS", "1");
            std::env::set_var("NEWT_DISABLE_OCAP", "1");
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
    if let Some(r) = args.max_rounds {
        dc.max_tool_rounds = r;
    }
    // OCAP-ON confined lane: replace the default unconfined `Caveats::top()` with
    // a workspace-fenced authority. The tool gate consults `dc.caveats` and the
    // permission_gate stays `None` — an in-fence write auto-consents, an
    // out-of-fence one fails closed (never hangs). This is the only seam the
    // confined lane touches; the driver + tool layer are unchanged.
    if confined {
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
    let wall_secs = started.elapsed().as_secs_f64();

    // 6. Emit the trace record (one JSONL line), including the per-tool-call
    //    trajectory (name/args-digest/ok/duration) the TurnDriver now lends —
    //    the material for the failure taxonomy.
    // A part-way inference failure now arrives as `Ok(o)` with `o.error` set —
    // so its PARTIAL trajectory is preserved (an infra failure must not report
    // the agent as having done nothing). `Err` is only a spawn/thread failure
    // with no trajectory at all.
    let o_opt = outcome.as_ref().ok();
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
    if let Some(path) = &args.events {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening --events {}", path.display()))?;
        writeln!(f, "{record}").context("writing events line")?;
    }
    // Always echo the record to stdout too, so a manual bootstrap run is legible.
    println!("{record}");

    // Clean completion → 0; any failure (inference error carried on the outcome,
    // or a spawn/thread Err) → 1.
    let clean = matches!(&outcome, Ok(o) if o.error.is_none());
    Ok(if clean { 0 } else { 1 })
}

/// Pick the backend to drive: `NEWT_PROVIDER` by name if set and present, else
/// the first configured backend that has an endpoint.
fn pick_backend(cfg: &Config) -> Option<&newt_core::config::BackendConfig> {
    if let Ok(name) = std::env::var("NEWT_PROVIDER") {
        if let Some(b) = cfg.backends.iter().find(|b| b.name == name) {
            return Some(b);
        }
    }
    cfg.backends.iter().find(|b| !b.endpoint.is_empty())
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
