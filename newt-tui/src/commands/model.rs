//! `/models` · `/probe` · `/model` · `/backend` · `/backends` · `/summarizer` ·
//! `/dgx` — the model / backend / inference command family. Moved verbatim from
//! the `dispatch_slash` match in `lib.rs`; the arm bodies are unchanged.

use std::io::{self, IsTerminal};

use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetForegroundColor};
use newt_core::agentic::{print_newt, warmup_if_cold, NEWT_ORANGE_CT};

use crate::probe;
use crate::{
    active_backend_name, backends_list_items, fetch_models_from_url, fetch_openai_models,
    is_ephemeral_session, keep_alive_str, resolve_backend_choice, run_newt_subcmd, today_date,
    with_interrupt_watch, BackendChoice,
};

/// Handle the model/backend command family. `dispatch_slash` routes exactly the
/// command names listed in this module's doc here; any other `cmd` is a router
/// bug. Mirrors `dispatch_slash`'s "handled ⇒ `Ok(true)`" contract (none of
/// these commands end the session).
pub(crate) fn dispatch(
    cmd: &str,
    arg1: &str,
    arg2: &str,
    color: bool,
    verbose: bool,
) -> anyhow::Result<bool> {
    match cmd {
        "models" => {
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            let choice = resolve_backend_choice(&cfg);
            let url = choice.url;
            let current = choice.model;

            if arg1 == "capabilities" {
                // Full tool-conformance matrix from the capability cache.
                match probe::fetch_ollama_models(&url) {
                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                    Ok(models) => {
                        let cache = probe::load_cache();
                        probe::print_capabilities_table(&models, &cache, &current, &url, color);
                    }
                }
            } else {
                // Plain list, with cached conformance symbol where known.
                let fetched = if choice.kind == newt_core::BackendKind::Openai {
                    fetch_openai_models(&url, choice.api_key.as_deref())
                } else {
                    fetch_models_from_url(&url)
                };
                match fetched {
                    Ok(names) if names.is_empty() => {
                        print_newt(&format!("No models found on {url}"), color, verbose);
                    }
                    Ok(names) => {
                        let cache = probe::load_cache();
                        print_newt(&format!("Models on {url}:"), color, verbose);
                        for name in &names {
                            let conformance_tag = cache
                                .get(name)
                                .map(|e| format!("  {}", e.conformance.symbol()))
                                .unwrap_or_default();
                            if *name == current {
                                if color {
                                    execute!(
                                        io::stdout(),
                                        Print(format!("  {name}{conformance_tag}")),
                                        SetForegroundColor(NEWT_ORANGE_CT),
                                        Print(" ◀ active"),
                                        ResetColor,
                                        Print("\n"),
                                    )
                                    .ok();
                                } else {
                                    println!("  {name}{conformance_tag} ◀ active");
                                }
                            } else {
                                println!("  {name}{conformance_tag}");
                            }
                        }
                        let tested = names.iter().filter(|n| cache.contains_key(*n)).count();
                        if tested < names.len() {
                            println!(
                                "\n  {}/{} tested — /models capabilities for the full matrix",
                                tested,
                                names.len()
                            );
                        }
                    }
                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                }
            }
        }

        "probe" => {
            // Test tool conformance for one model, or every model (`all`).
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            let choice = resolve_backend_choice(&cfg);

            if arg1 == "reset" {
                // Wipe learned conformance, context windows, and calibration so
                // the next /probe re-learns from scratch (works on any backend —
                // the cache is local).
                probe::save_cache(&probe::CapabilityCache::default());
                print_newt(
                    "probe cache reset — conformance, context windows, and calibration cleared. \
                     Re-test with /probe all (Esc to cancel).",
                    color,
                    verbose,
                );
            } else if choice.kind != newt_core::BackendKind::Ollama {
                print_newt(
                    "/probe only works with Ollama endpoints (vLLM/OpenAI keep models resident)",
                    color,
                    verbose,
                );
            } else {
                let endpoint = &choice.url;
                let mut cache = probe::load_cache();

                // Step 20.2 (docs/design/model-self-tuning.md §4.1): `/probe
                // window [model]` runs the expensive empirical boundary search
                // for one model; `/probe [model|all]` runs the cheap discovery
                // pass. `window` is consumed here so the rest of the selection
                // logic sees the model name in the right slot.
                let do_window = arg1 == "window";
                let model_arg = if do_window { arg2 } else { arg1 };

                // Decide which models to probe. `all` re-probes EVERY model on
                // the endpoint (not just untested ones) — to wipe stale learning
                // first, run /probe reset. A long sweep; Esc cancels it.
                let targets: Vec<String> = if !do_window && model_arg == "all" {
                    match probe::fetch_ollama_models(endpoint) {
                        Ok(models) => models.into_iter().map(|m| m.name).collect(),
                        Err(e) => {
                            print_newt(&format!("error fetching model list: {e}"), color, verbose);
                            vec![]
                        }
                    }
                } else if model_arg.is_empty() {
                    vec![choice.model.clone()]
                } else {
                    vec![model_arg.to_string()]
                };

                if targets.is_empty() {
                    print_newt("No models to probe.", color, verbose);
                } else if model_arg == "all" {
                    print_newt(
                        &format!("Probing {} models — press Esc to cancel.", targets.len()),
                        color,
                        verbose,
                    );
                }

                // Esc-cancellable sweep: a keyboard watcher trips the flag, which
                // we check at each model boundary (a single model still finishes;
                // the remaining ones are skipped). Only on a TTY.
                let probe_cancel = std::sync::atomic::AtomicBool::new(false);
                // The probe sweep only needs graceful cancel; a 2nd Ctrl-C trips
                // this (ignored here) and cancel already stops the sweep.
                let probe_hard = std::sync::atomic::AtomicBool::new(false);
                let probe_interruptible = io::stdin().is_terminal() && io::stdout().is_terminal();
                let mut probed = 0usize;
                with_interrupt_watch(probe_interruptible, &probe_cancel, &probe_hard, || {
                    for model in &targets {
                        if probe_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            print_newt(
                                &format!("⊘ interrupted — probed {probed}/{}", targets.len()),
                                color,
                                verbose,
                            );
                            break;
                        }
                        // Warm up before probing so load time doesn't count as a timeout.
                        if do_window {
                            print_newt(
                                &format!("Probing {model} (window search)…"),
                                color,
                                verbose,
                            );
                        } else {
                            print_newt(&format!("Probing {model}…"), color, verbose);
                        }
                        warmup_if_cold(endpoint, model, &keep_alive_str(&cfg), color, verbose);

                        let today = today_date();
                        // Mutate the cache entry in place so the 20.1 fields
                        // (estimate_ratio, emits_thinking, max_ok_input, tune_*)
                        // are preserved and the refreshed window / quirk / ratio
                        // that full_probe writes are kept too (§4.1, item 12).
                        let mut entry = cache.remove(model.as_str()).unwrap_or_default();
                        let report = probe::full_probe(
                            endpoint,
                            model,
                            &mut entry,
                            do_window,
                            &today,
                            |line: &str| print_newt(line, color, verbose),
                            cfg.context
                                .as_ref()
                                .map(|c| c.estimation)
                                .unwrap_or_default(),
                        );
                        cache.insert(model.clone(), entry);
                        probe::save_cache(&cache);

                        // Rich report (§4.1): conformance symbol PLUS the window,
                        // thinking quirk, and calibration ratio; window mode adds
                        // the empirically-confirmed max input at High confidence.
                        print_newt(
                            &format!(
                                "{model}  →  {}  (tested {today})",
                                report.conformance.symbol()
                            ),
                            color,
                            verbose,
                        );
                        if let Some(w) = report.context_window {
                            print_newt(&format!("  context window: {w}"), color, verbose);
                        }
                        if report.emits_thinking {
                            print_newt("  quirk: emits thinking-only responses", color, verbose);
                        }
                        if let Some(r) = report.estimate_ratio {
                            print_newt(
                                &format!("  estimate calibration: x{r:.2} (chars/4 → real)"),
                                color,
                                verbose,
                            );
                        }
                        if let Some(outcome) = &report.boundary {
                            match outcome.highest_accepted {
                                Some(max) => print_newt(
                                    &format!(
                                        "  max input (empirical): {max} — High confidence \
                                     ({} steps)",
                                        outcome.steps
                                    ),
                                    color,
                                    verbose,
                                ),
                                None => print_newt(
                                    &format!(
                                        "  no input accepted in {} steps (bounds {:?})",
                                        outcome.steps, outcome.final_bounds
                                    ),
                                    color,
                                    verbose,
                                ),
                            }
                            if let Some(err) = &outcome.error {
                                print_newt(&format!("  note: {err}"), color, verbose);
                            }
                        }
                        for note in &report.notes {
                            print_newt(&format!("  note: {note}"), color, verbose);
                        }
                        probed += 1;
                    }
                });
            }
        }

        "model" => {
            if arg1.is_empty() {
                let cfg = newt_core::Config::resolve().unwrap_or_default();
                let current = resolve_backend_choice(&cfg).model;
                print_newt(
                    &format!("active model: {current}  (use /model <name> to switch)"),
                    color,
                    verbose,
                );
            } else {
                // Model override on the ACTIVE backend — whatever it is. A pinned
                // [[backends]] entry, an OpenAI backend, and the historical DGX
                // path all read NEWT_DGX_MODEL in `resolve_backend_choice`, so
                // this one axis switches the model everywhere, and it does not
                // edit config. Mirrors how `/backend ollama <model>` works.
                //
                // The old `newt dgx use <model>` persist was the bug the user hit:
                // it wrote the DGX `active_model`, but a pinned named backend
                // resolves its OWN static `model`, so the saved value was never
                // consulted and the switch silently did nothing.
                // #1122: validate the model is actually SERVED by the active
                // backend BEFORE applying or persisting. A typo would otherwise
                // be written to ~/.newt/settings.toml, which silently overrides
                // config.toml and 404s every future launch. Best-effort: an
                // unreachable backend (can't list models) is not blocked.
                let gate_cfg = newt_core::Config::resolve().unwrap_or_default();
                let gate_choice = resolve_backend_choice(&gate_cfg);
                let served: Option<Vec<String>> = match gate_choice.kind {
                    newt_core::BackendKind::Openai => {
                        fetch_openai_models(&gate_choice.url, gate_choice.api_key.as_deref()).ok()
                    }
                    _ => fetch_models_from_url(&gate_choice.url).ok(),
                };
                if !model_choice_ok(arg1, served.as_deref()) {
                    let served = served.unwrap_or_default();
                    match suggest_model(arg1, &served) {
                        Some(s) => print_newt(
                            &format!(
                                "no model `{arg1}` on {} — did you mean `{s}`? (not applied)",
                                gate_choice.url
                            ),
                            color,
                            verbose,
                        ),
                        None => print_newt(
                            &format!(
                                "no model `{arg1}` on {} — run /models to list (not applied)",
                                gate_choice.url
                            ),
                            color,
                            verbose,
                        ),
                    }
                    return Ok(true);
                }
                // SAFETY: single-threaded REPL; the post-command re-resolve reads it.
                unsafe { std::env::set_var("NEWT_DGX_MODEL", arg1) };
                // Persist the choice so it sticks across runs (#545): records
                // `model` in ~/.newt/settings.toml (provider left as-is), to be
                // restored next start at the lowest precedence (an explicit
                // NEWT_DGX_MODEL or a --loadout model still wins). Skipped in an
                // ephemeral session, which must leave no trace; the live switch
                // above still applies. Best-effort — a write never blocks it.
                if newt_core::settings::should_persist(is_ephemeral_session()) {
                    newt_core::settings::record_model(arg1);
                }
                let cfg = newt_core::Config::resolve().unwrap_or_default();
                let choice = resolve_backend_choice(&cfg);
                // Warm-up only applies to Ollama: vLLM and OpenAI-compatible
                // endpoints keep their served model resident at all times.
                if choice.kind == newt_core::BackendKind::Ollama {
                    warmup_if_cold(
                        &choice.url,
                        &choice.model,
                        &keep_alive_str(&cfg),
                        color,
                        verbose,
                    );
                } else {
                    print_newt(
                        &format!(
                            "Switched to {} — takes effect on next message.",
                            choice.model
                        ),
                        color,
                        verbose,
                    );
                }
            }
        }

        "backend" => {
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            let has_openai = cfg
                .backends
                .iter()
                .any(|b| b.kind == newt_core::BackendKind::Openai);
            let kind_name = |c: &BackendChoice| c.kind.label();
            if arg1.is_empty() {
                let choice = resolve_backend_choice(&cfg);
                print_newt(
                    &format!(
                        "active backend: {} · {} @ {}",
                        kind_name(&choice),
                        choice.model,
                        choice.url
                    ),
                    color,
                    verbose,
                );
                print_newt(
                    &format!(
                        "usage: /backend <{}> [model]   (e.g. /backend ollama deepseek-r1)",
                        if has_openai {
                            "openai|ollama"
                        } else {
                            "ollama"
                        }
                    ),
                    color,
                    verbose,
                );
            } else if matches!(arg1, "openai" | "ollama") {
                // SAFETY: single-threaded REPL; the post-command re-resolve picks
                // it up. Session-only — does NOT persist; use `/model` or edit
                // `[backends]` to persist a choice.
                unsafe { std::env::set_var("NEWT_BACKEND", arg1) };
                // Optional model arg → session-only override on the same axis the
                // loadout `model` feeds (NEWT_DGX_MODEL), consumed by the Ollama
                // resolution. Avoids mutating saved config on a live A/B switch.
                if arg1 == "ollama" && !arg2.is_empty() {
                    unsafe { std::env::set_var("NEWT_DGX_MODEL", arg2) };
                }
                let choice =
                    resolve_backend_choice(&newt_core::Config::resolve().unwrap_or_default());
                print_newt(
                    &format!(
                        "switched to {} · {} @ {} — next message.",
                        kind_name(&choice),
                        choice.model,
                        choice.url
                    ),
                    color,
                    verbose,
                );
            } else {
                print_newt("usage: /backend <openai|ollama> [model]", color, verbose);
            }
        }

        "backends" => {
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            if arg1.is_empty() {
                // List every configured [[backends]] entry by name, flagging the
                // one the session currently resolves to. `/backend` toggles the
                // coarse openai-vs-ollama *kind*; `/backends` picks a *named*
                // endpoint (dgx1, gnuc, openai, …) regardless of wire protocol.
                let active = active_backend_name(&cfg);
                print_newt("configured backends:", color, verbose);
                if cfg.backends.is_empty() {
                    print_newt(
                        "  (none — add [[backends]] entries to ~/.newt/config.toml)",
                        color,
                        verbose,
                    );
                } else {
                    for (label, is_active) in backends_list_items(&cfg, active.as_deref()) {
                        newt_core::agentic::print_list_item(&label, is_active, color);
                    }
                    print_newt(
                        "usage: /backends <name> to switch (e.g. /backends dgx1)",
                        color,
                        verbose,
                    );
                }
            } else if cfg.backends.iter().any(|b| b.name == arg1) {
                // SAFETY: single-threaded REPL. The post-command re-resolve in the
                // session loop reads NEWT_PROVIDER and repoints the session at this
                // named backend. Clear any stale per-session model override so the
                // named backend's own default model applies.
                unsafe {
                    std::env::set_var("NEWT_PROVIDER", arg1);
                    std::env::remove_var("NEWT_DGX_MODEL");
                }
                // Persist the choice so it sticks across runs (#545): records
                // `provider` and clears `model` in ~/.newt/settings.toml, to be
                // restored next start at the lowest precedence (an explicit
                // NEWT_PROVIDER or a --loadout still wins). Skipped in an
                // ephemeral session, which must leave no trace; the live switch
                // above still applies. Best-effort — a write never blocks it.
                if newt_core::settings::should_persist(is_ephemeral_session()) {
                    newt_core::settings::record_provider(arg1);
                }
                let choice =
                    resolve_backend_choice(&newt_core::Config::resolve().unwrap_or_default());
                print_newt(
                    &format!(
                        "switched to backend '{}' · {} @ {} — next message.",
                        arg1, choice.model, choice.url
                    ),
                    color,
                    verbose,
                );
            } else {
                let names: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
                print_newt(
                    &format!(
                        "no backend named '{}'. configured: {}",
                        arg1,
                        if names.is_empty() {
                            "(none)".to_string()
                        } else {
                            names.join(", ")
                        }
                    ),
                    color,
                    verbose,
                );
            }
        }

        "summarizer" => {
            let mut args = vec!["summarizer"];
            if !arg1.is_empty() {
                args.push(arg1);
            }
            if !arg2.is_empty() {
                args.push(arg2);
            }
            run_newt_subcmd(&args, color, verbose)?;
        }

        "dgx" => {
            if arg1.is_empty() {
                print_newt(
                    "usage: /dgx <status|models|ps|warm [model]|pull <model>|rm <model>|route <task>|doctor>",
                    color,
                    verbose,
                );
            } else {
                let mut dgx_args = vec!["dgx", arg1];
                if !arg2.is_empty() {
                    dgx_args.push(arg2);
                }
                run_newt_subcmd(&dgx_args, color, verbose)?;
            }
        }

        other => unreachable!("commands::model::dispatch routed a non-model command: {other:?}"),
    }
    Ok(true)
}

/// The #1122 gate: may this model choice be applied/persisted? A `None` served
/// list means the backend couldn't be listed (unreachable) — allowed
/// best-effort, since we can't validate an offline endpoint. A `Some` list must
/// contain the exact name.
fn model_choice_ok(name: &str, served: Option<&[String]>) -> bool {
    served.is_none_or(|models| models.iter().any(|m| m == name))
}

/// Suggest the served model closest to a mistyped name (within a few edits), for
/// a "did you mean?" hint. `None` when nothing is close enough.
fn suggest_model(typo: &str, served: &[String]) -> Option<String> {
    served
        .iter()
        .map(|m| (levenshtein(typo, m), m))
        .filter(|(d, _)| *d >= 1 && *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, m)| m.clone())
}

/// Plain Levenshtein edit distance (two-row DP), for the typo suggestion.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod validate_tests {
    use super::{levenshtein, model_choice_ok, suggest_model};

    #[test]
    fn model_choice_gate() {
        let served = ["qwen2.5-coder:7b".to_string(), "llama3.1:8b".to_string()];
        // Served → OK.
        assert!(model_choice_ok("qwen2.5-coder:7b", Some(&served)));
        // Regression (#1122): a typo is NOT ok → not applied/persisted.
        assert!(!model_choice_ok("quen2.5-coder:7b", Some(&served)));
        // Backend unreachable (None) → allowed best-effort (can't validate).
        assert!(model_choice_ok("anything", None));
    }

    #[test]
    fn suggest_catches_a_small_typo_only() {
        let served = vec![
            "qwen2.5-coder:7b".to_string(),
            "nomic-embed-text:latest".to_string(),
        ];
        // The exact bug: `quen` (missing w) → suggests `qwen…`.
        assert_eq!(
            suggest_model("quen2.5-coder:7b", &served).as_deref(),
            Some("qwen2.5-coder:7b")
        );
        // Nothing close → no misleading suggestion.
        assert_eq!(suggest_model("totally-different", &served), None);
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("qwen", "quen"), 1);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }
}
