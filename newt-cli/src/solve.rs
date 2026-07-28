//! `newt solve` — the headless, non-interactive entry that drives the agentic
//! loop to solve one task and emits a trace, for Terminal-Bench (epic #1419 /
//! the release-champion ceremony, WS1).
//!
//! It is a THIN wrapper over the same [`TurnDriver`] / `chat_complete` loop the
//! interactive TUI runs — no second loop. Headless contract:
//! `permission_gate: None` (a capability denial fails the call, never hangs) and
//! caveats default to [`Caveats::top`] (unconfined). `--non-interactive` sets
//! `NEWT_FULL_ACCESS=1` so the host shell is used and no prompt can appear — the
//! `--yolo --full-access` bootstrap lane. (The flight-recorder-derived-Caveats
//! confined lane is the later OCAP arc.)

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use newt_core::{BackendKind, Config, TurnDriver, TurnDriverConfig, TurnStatus};

/// Parsed `newt solve` arguments (mirrors the `Command::Solve` fields).
pub struct SolveArgs {
    pub cwd: PathBuf,
    pub instruction_file: PathBuf,
    pub profile: Option<PathBuf>,
    pub non_interactive: bool,
    pub events: Option<PathBuf>,
    pub max_rounds: Option<usize>,
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

    // 2. Non-interactive ⇒ the `--yolo --full-access` bootstrap lane: full
    //    access (Caveats::top) AND OCAP disabled, so an UNRESTRICTED fs write
    //    auto-accepts instead of waiting on a (nonexistent) prompt gate and
    //    silently denying — the write path the benchmark depends on. SAFETY:
    //    single-threaded before the driver spawns its turn thread.
    if args.non_interactive {
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
    let (status, reply_chars, usage, halluc, error) = match &outcome {
        Ok(o) => (
            "completed",
            o.reply.len(),
            o.usage.as_ref().map(|u| u.total()),
            o.hallucinations,
            None,
        ),
        Err(e) => ("failed", 0, None, 0, Some(e.clone())),
    };
    // The per-tool trajectory — the material for the failure taxonomy. The
    // single highest-signal field is `write_calls`: a failed task with 0 writes
    // never ACTED (the tenacity target); with writes it acted but wrong.
    let (tool_calls, write_calls, end_reason, trajectory) = match &outcome {
        Ok(o) => {
            let names: Vec<&str> = o.tool_events.iter().map(|e| e.tool.as_str()).collect();
            let writes = names
                .iter()
                .filter(|n| {
                    matches!(
                        **n,
                        "write_file" | "edit_file" | "create_file" | "str_replace" | "apply_patch"
                    )
                })
                .count();
            (
                names.len(),
                writes,
                format!("{:?}", o.end_reason),
                serde_json::to_value(&o.tool_events).unwrap_or(serde_json::Value::Null),
            )
        }
        Err(_) => (0, 0, "None".to_string(), serde_json::Value::Null),
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

    Ok(if outcome.is_ok() { 0 } else { 1 })
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
}
