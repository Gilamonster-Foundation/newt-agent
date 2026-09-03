//! `/models` · `/probe` · `/model` · `/backend` · `/backends` · `/summarizer` ·
//! `/dgx` — the model / backend / inference command family. Moved verbatim from
//! the `dispatch_slash` match in `lib.rs`; the arm bodies are unchanged.

use std::io::{self, IsTerminal};

use crossterm::execute;
use crossterm::style::{Print, ResetColor, SetForegroundColor};
use newt_core::agentic::{print_newt, warmup_if_cold, NEWT_ORANGE_CT};

use crate::probe;
use crate::{
    active_backend_name, backends_list_items, fetch_models_for, is_ephemeral_session,
    keep_alive_str, run_newt_subcmd, today_date, with_interrupt_watch, BackendChoice,
};

/// Handle the model/backend command family. `dispatch_slash` routes exactly the
/// command names listed in this module's doc here; any other `cmd` is a router
/// bug. Mirrors `dispatch_slash`'s "handled ⇒ `Ok(true)`" contract (none of
/// these commands end the session).
/// The session's current backend choice for a slash command — or print the
/// typed selection refusal (unknown/unroutable/provider explicit selector)
/// and yield `None`: a command surface never silently runs another backend.
fn choice_or_print(
    resolved: &newt_core::ResolvedConfig,
    color: bool,
    verbose: bool,
) -> Option<BackendChoice> {
    match crate::resolve_backend_choice(resolved) {
        Ok(choice) => Some(choice),
        Err(refusal) => {
            print_newt(&refusal, color, verbose);
            None
        }
    }
}

pub(crate) fn dispatch(
    cmd: &str,
    arg1: &str,
    arg2: &str,
    color: bool,
    verbose: bool,
) -> anyhow::Result<bool> {
    match cmd {
        "models" => {
            let cfg = crate::resolve_runtime_or_default();
            let Some(choice) = choice_or_print(&cfg, color, verbose) else {
                return Ok(true);
            };
            let url = choice.url;
            let current = choice.active_model.clone().unwrap_or_default();

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
                // Ask the backend (#backend-trait) — one path for every kind.
                let fetched = fetch_models_for(&url, choice.kind, choice.api_key.as_deref());
                match fetched {
                    Ok(names) if names.is_empty() => {
                        print_newt(&format!("No models found on {url}"), color, verbose);
                    }
                    Ok(names) => {
                        let cache = probe::load_cache();
                        print_newt(&format!("Models on {url}:"), color, verbose);
                        for name in &names {
                            let conformance_tag = cache
                                .get(&probe::cap_key(newt_core::Serving::Multiplexer, "", name))
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
                        let tested = names
                            .iter()
                            .filter(|n| {
                                cache.contains_key(&probe::cap_key(
                                    newt_core::Serving::Multiplexer,
                                    "",
                                    n,
                                ))
                            })
                            .count();
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
            let cfg = crate::resolve_runtime_or_default();
            let Some(choice) = choice_or_print(&cfg, color, verbose) else {
                return Ok(true);
            };

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
                    vec![choice.active_model.clone().unwrap_or_default()]
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
                let probe_interruptible = io::stdin().is_terminal() && io::stdout().is_terminal();
                let mut probed = 0usize;
                with_interrupt_watch(probe_interruptible, &probe_cancel, || {
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
                        warmup_if_cold(
                            endpoint,
                            model,
                            &keep_alive_str(&cfg),
                            choice.api_key.as_deref(),
                            color,
                            verbose,
                        );

                        let today = today_date();
                        // Mutate the cache entry in place so the 20.1 fields
                        // (estimate_ratio, emits_thinking, max_ok_input, tune_*)
                        // are preserved and the refreshed window / quirk / ratio
                        // that full_probe writes are kept too (§4.1, item 12).
                        // `/probe` is Ollama-only (guarded above), i.e. a
                        // Multiplexer, so the capability key is the bare model
                        // name — but go through cap_key so the keying discipline
                        // is never open-coded (a raw String key is now a type
                        // error).
                        let key =
                            probe::cap_key(newt_core::Serving::Multiplexer, &choice.name, model);
                        let mut entry = cache.remove(&key).unwrap_or_default();
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
                            choice.kind,
                        );
                        cache.insert(key, entry);
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
                let cfg = crate::resolve_runtime_or_default();
                let Some(choice) = choice_or_print(&cfg, color, verbose) else {
                    return Ok(true);
                };
                print_newt(
                    &format!(
                        "active model: {}  (use /model <name> to switch)",
                        choice.display_model()
                    ),
                    color,
                    verbose,
                );
            } else {
                apply_model_choice(arg1, color, verbose);
            }
        }

        "backend" => {
            let cfg = crate::resolve_runtime_or_default();
            let has_openai = cfg
                .backends
                .iter()
                .any(|b| b.kind == Some(newt_core::BackendKind::Openai));
            let kind_name = |c: &BackendChoice| c.kind.label();
            if arg1.is_empty() {
                let Some(choice) = choice_or_print(&cfg, color, verbose) else {
                    return Ok(true);
                };
                print_newt(
                    &format!(
                        "active backend: {} · {} @ {}",
                        kind_name(&choice),
                        choice.display_model(),
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
                apply_backend_kind(arg1, arg2, color, verbose);
            } else {
                print_newt("usage: /backend <openai|ollama> [model]", color, verbose);
            }
        }

        "backends" => {
            let cfg = crate::resolve_runtime_or_default();
            if arg1.is_empty() {
                // List every configured [[backends]] entry by name, flagging the
                // one the session currently resolves to. `/backend` toggles the
                // coarse openai-vs-ollama *kind*; `/backends` picks a *named*
                // endpoint (dgx1, gpu-runner, openai, …) regardless of wire protocol.
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
            } else {
                apply_backend_choice(arg1, color, verbose);
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

/// The models the active backend serves, tagged with their cached conformance
/// symbols — the option list behind every model SPINNER.
///
/// Lifted out of `chat.rs`'s `/psyche` block when `/settings` grew a model row,
/// because the alternative was a second fetch-and-tag assembled from the same
/// three pieces (`resolve_backend_choice` -> `fetch_models_for` ->
/// `probe::load_cache`). Two of those would be two answers to "what can this
/// backend serve", and their tags would drift the moment one learned a new
/// conformance symbol.
///
/// **The fetch happens HERE, not in a panel.** A network call inside a draw
/// loop blocks the terminal for as long as the backend takes to answer; every
/// panel that shows models takes this list as data, resolved before it opens.
/// `None` — unreachable, or no backend resolved — means the row renders and
/// will not dial, which is #1666's rule.
#[cfg(feature = "rich-tui")]
pub(crate) fn served_choices(
    cfg: &newt_core::ResolvedConfig,
) -> Option<Vec<crate::config_panel::ModelChoice>> {
    let choice = crate::resolve_backend_choice(cfg).ok()?;
    let names = fetch_models_for(&choice.url, choice.kind, choice.api_key.as_deref()).ok()?;
    let cache = probe::load_cache();
    Some(
        names
            .into_iter()
            .map(|name| {
                let tag = cache
                    .get(&probe::cap_key(newt_core::Serving::Multiplexer, "", &name))
                    .map(|e| e.conformance.symbol().to_string())
                    .unwrap_or_default();
                crate::config_panel::ModelChoice { name, tag }
            })
            .collect(),
    )
}

/// Switch the session to a coarse backend WIRE KIND — the single application
/// path shared by `/backend <openai|ollama> [model]` and the backend panel's
/// bare-kind fallback rows (#1667), extracted verbatim from the slash arm so
/// the two surfaces cannot drift:
///
/// - Written under the process-env lock (#1850); the post-command re-resolve
///   picks it up. Session-only — does NOT persist; use `/model` or a named
///   `/backends` switch to persist a choice.
/// - Optional `model` (ollama only) → session-only override on the same axis
///   the loadout `model` feeds (NEWT_DGX_MODEL), consumed by the Ollama
///   resolution. Avoids mutating saved config on a live A/B switch.
pub(crate) fn apply_backend_kind(kind: &str, model: &str, color: bool, verbose: bool) {
    {
        // One hold for the whole switch, released before the re-resolve and
        // the print: a guarded reader must not see the new wire kind beside
        // the outgoing backend's model override.
        let _env = newt_core::process_env::lock();
        newt_core::process_env::set_var("NEWT_BACKEND", kind);
        if kind == "ollama" && !model.is_empty() {
            newt_core::process_env::set_var("NEWT_DGX_MODEL", model);
            // #1668: the model half of this switch is an operator preference
            // action. The coarse wire kind is deliberately not a pin axis.
            newt_core::runtime::mark_model_pick(model);
        }
    }
    match crate::resolve_backend_choice(&crate::resolve_runtime_or_default()) {
        Ok(choice) => print_newt(
            &format!(
                "switched to {} · {} @ {} — next message.",
                choice.kind.label(),
                choice.display_model(),
                choice.url
            ),
            color,
            verbose,
        ),
        Err(refusal) => print_newt(&refusal, color, verbose),
    }
}

/// Switch the session to the NAMED backend `name` — the single application path
/// shared by `/backends <name>` and the backend panel chooser (#1667),
/// extracted verbatim from the slash arm (mirroring [`apply_model_choice`]) so
/// there is exactly one set of switch semantics. Returns whether it applied
/// (`false` = no such configured backend; the miss is printed).
///
/// - Written under the process-env lock (#1850). The post-command re-resolve
///   in the session loop reads NEWT_PROVIDER and repoints the session at this
///   named backend.
///   Clear any stale per-session model override so the named backend's own
///   default model applies.
/// - Persist the choice so it sticks across runs (#545): records `provider`
///   and clears `model` in ~/.newt/settings.toml, to be restored next start at
///   the lowest precedence (an explicit NEWT_PROVIDER or a --loadout still
///   wins). Skipped in an ephemeral session, which must leave no trace; the
///   live switch still applies. Best-effort — a write never blocks it.
pub(crate) fn apply_backend_choice(name: &str, color: bool, verbose: bool) -> bool {
    let cfg = crate::resolve_runtime_or_default();
    if let Some(target) = cfg.backends.iter().find(|b| b.name == name) {
        // TRANSACTIONAL: validate the named target as HTTP-drivable BEFORE
        // any env/settings mutation — refusing after `set_var` would poison
        // the session selector and the saved preference while the live
        // route stayed on the old backend. Judged on the FLATTENED,
        // core-normalized view, exactly like the choice seam.
        if target.endpoint.is_empty() || target.kind == Some(newt_core::BackendKind::Embedded) {
            print_newt(
                &format!(
                    "backend '{name}' is an embedded (model_path) backend — chat drives \
                     HTTP backends only; nothing was switched or saved"
                ),
                color,
                verbose,
            );
            return false;
        }
        {
            // One hold for the pair: a guarded reader must never see the new
            // provider beside the outgoing backend's stale model override.
            let _env = newt_core::process_env::lock();
            newt_core::process_env::set_var("NEWT_PROVIDER", name);
            newt_core::process_env::remove_var("NEWT_DGX_MODEL");
        }
        // #1668: mark only a successful named-backend pick. Listing and an
        // unknown name therefore cannot capture ambient persona routing.
        newt_core::runtime::mark_backend_pick(name);
        if newt_core::settings::should_persist(is_ephemeral_session()) {
            newt_core::settings::record_provider(name);
        }
        match crate::resolve_backend_choice(&crate::resolve_runtime_or_default()) {
            Ok(choice) => print_newt(
                &format!(
                    "switched to backend '{}' · {} @ {} — next message.",
                    name,
                    choice.display_model(),
                    choice.url
                ),
                color,
                verbose,
            ),
            Err(refusal) => print_newt(&refusal, color, verbose),
        }
        true
    } else {
        let names: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
        print_newt(
            &format!(
                "no backend named '{}'. configured: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ),
            color,
            verbose,
        );
        false
    }
}

/// The #1122 gate: may this model choice be applied/persisted? A `None` served
/// list means the backend couldn't be listed (unreachable) — allowed
/// best-effort, since we can't validate an offline endpoint. A `Some` list must
/// contain the exact name.
/// Switch the ACTIVE backend's model to `name` — the single application path
/// shared by `/model <name>` and the psyche panel's model spinner (#1666), so
/// there is exactly one set of gates:
///
/// - Model override on the ACTIVE backend — whatever it is. A pinned
///   [[backends]] entry, an OpenAI backend, and the historical DGX path all
///   read NEWT_DGX_MODEL in `resolve_backend_choice`, so this one axis
///   switches the model everywhere, and it does not edit config. (The old
///   `newt dgx use <model>` persist wrote a value a pinned named backend never
///   consulted — the switch silently did nothing.)
/// - #1122: validate the model is actually SERVED by the active backend
///   BEFORE applying or persisting. A typo would otherwise be written to
///   ~/.newt/settings.toml, which silently overrides config.toml and 404s
///   every future launch. Best-effort: an unreachable backend (can't list
///   models) is not blocked.
/// - #545: persist via `settings::record_model` so the choice sticks across
///   runs — skipped in an ephemeral session, which must leave no trace.
/// - Warm-up only applies to Ollama: vLLM and OpenAI-compatible endpoints
///   keep their served model resident at all times.
pub(crate) fn apply_model_choice(name: &str, color: bool, verbose: bool) {
    let gate_cfg = crate::resolve_runtime_or_default();
    let Some(gate_choice) = choice_or_print(&gate_cfg, color, verbose) else {
        return;
    };
    let served: Option<Vec<String>> = fetch_models_for(
        &gate_choice.url,
        gate_choice.kind,
        gate_choice.api_key.as_deref(),
    )
    .ok();
    if !model_choice_ok(name, served.as_deref()) {
        let served = served.unwrap_or_default();
        match suggest_model(name, &served) {
            Some(s) => print_newt(
                &format!(
                    "no model `{name}` on {} — did you mean `{s}`? (not applied)",
                    gate_choice.url
                ),
                color,
                verbose,
            ),
            None => print_newt(
                &format!(
                    "no model `{name}` on {} — run /models to list (not applied)",
                    gate_choice.url
                ),
                color,
                verbose,
            ),
        }
        return;
    }
    // Under the process-env lock (#1850); the post-command re-resolve reads it.
    newt_core::process_env::set_var("NEWT_DGX_MODEL", name);
    // #1668: past the #1122 gate ⇒ the pick really applied, so it is an
    // operator posture action on the MODEL axis alone. A refused pick returned
    // above and marks nothing; the backend the operator happens to be on
    // (possibly a persona's route) is never adopted here.
    newt_core::runtime::mark_model_pick(name);
    if newt_core::settings::should_persist(is_ephemeral_session()) {
        newt_core::settings::record_model(name);
    }
    let cfg = crate::resolve_runtime_or_default();
    let Some(choice) = choice_or_print(&cfg, color, verbose) else {
        return;
    };
    if choice.kind == newt_core::BackendKind::Ollama {
        warmup_if_cold(
            &choice.url,
            &choice.active_model.clone().unwrap_or_default(),
            &keep_alive_str(&cfg),
            choice.api_key.as_deref(),
            color,
            verbose,
        );
    } else {
        print_newt(
            &format!(
                "Switched to {} — takes effect on next message.",
                choice.display_model()
            ),
            color,
            verbose,
        );
    }
}

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

#[cfg(test)]
mod mark_tests {
    use super::*;
    use newt_core::runtime::drain_preference_actions;
    use newt_core::test_guard::GlobalSettingsGuard;

    /// #1668 review-2 finding 8: the load-bearing NEGATIVE of the whole
    /// preference-pin design was asserted nowhere.
    ///
    /// The original finding 1 was that merely *looking* at settings pinned them,
    /// which is what made a pin worthless — a conversation acquired a backend it
    /// never chose. The fix was to mark only on a SUCCESSFUL named pick, but the
    /// tests only ever covered the positive path, so a future edit that hoisted
    /// `mark_backend_pick` above the `any(|b| b.name == arg1)` guard — the exact
    /// shape of the original bug — would have gone green.
    ///
    /// Both no-op arms, driven through the real `dispatch`: a bare `/backends`
    /// LISTING, and a `/backends <unknown>`. Neither touches the network.
    ///
    /// Since #1683 landed the unified panel, the unknown-name arm delegates to
    /// `apply_backend_choice` — the SAME function the panel chooser calls — so
    /// this one test now covers the refusal path of both surfaces. That matters
    /// because #1683 merged BEFORE this PR: the panel shipped a backend chooser
    /// while the pin machinery was still unmerged, so nothing on `main` marked
    /// at all. This is the assertion that the two compose the way the design
    /// says rather than the way the merge order implied.
    #[test]
    fn browsing_backends_marks_no_preference_action() {
        let _g = GlobalSettingsGuard::acquire();
        let _ = drain_preference_actions();

        dispatch("backends", "", "", false, false).expect("listing must not error");
        assert!(
            drain_preference_actions().is_empty(),
            "a bare /backends LISTING must pin nothing — browsing is not choosing"
        );

        dispatch(
            "backends",
            "definitely-not-a-configured-backend",
            "",
            false,
            false,
        )
        .expect("an unknown name must not error");
        assert!(
            drain_preference_actions().is_empty(),
            "a REFUSED /backends <unknown> must pin nothing — a failed switch is not a choice"
        );
    }
}
