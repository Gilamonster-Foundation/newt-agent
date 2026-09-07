//! Routing a persona to its backend.
//!
//! A persona may name a provider/model of its own; these resolve that into a
//! backend switch and apply it. The switch itself is `super::refresh_backend`,
//! and the persona type and `BackendChoice` stay in `super` — this is the
//! routing layer between them.

use super::*;

/// The `(NEWT_PROVIDER, NEWT_DGX_MODEL)` a persona's backend routing wants, or
/// `None` when the persona declares no `backend:` (leave the session backend
/// untouched). A persona's `backend` NAMES a `[[backends]]` entry — exactly what
/// `NEWT_PROVIDER` selects; its `model` (if any) maps to the session-model
/// override, else `None` so the backend's own default model applies (clearing
/// the override, as `/backends` does). Pure — the env mutation + re-resolve is
/// the caller's job.
pub(crate) fn persona_provider_env(
    profile: Option<&newt_core::RoleProfile>,
) -> Option<(String, Option<String>)> {
    let backend = profile.and_then(|p| p.backend.as_deref())?;
    let model = profile.and_then(|p| p.model.as_deref()).map(str::to_string);
    Some((backend.to_string(), model))
}

/// Decide a persona's backend route — the pure, validated core of
/// [`apply_persona_backend`]:
/// - `Ok(Some((provider, model)))` — the persona declares a `backend:` that IS in
///   `configured`; set these env values.
/// - `Ok(None)` — the persona declares no backend (or was cleared); revert to the
///   pre-persona baseline.
/// - `Err(name)` — the persona names a backend NOT in `configured`: refuse, so a
///   typo'd / non-portable persona can't silently reroute the session to a
///   fallback (the silent-cost-reroute class the resolver's `NEWT_PROVIDER` rung
///   guards against — it validates before setting the env, and so must we).
pub(crate) fn persona_backend_route(
    profile: Option<&newt_core::RoleProfile>,
    configured: &[&str],
) -> Result<Option<(String, Option<String>)>, String> {
    match persona_provider_env(profile) {
        Some((backend, model)) if configured.contains(&backend.as_str()) => {
            Ok(Some((backend, model)))
        }
        Some((backend, _)) => Err(backend),
        None => Ok(None),
    }
}

/// Persona backend auto-route: repoint the session's wire target to the active
/// persona's `backend:` — validated against `cfg.backends`, exactly as
/// `/backends <name>` would (an unknown name is refused, not silently rerouted).
/// A persona that declares NO backend (or a cleared persona → `None`) REVERTS to
/// the pre-persona `baseline` (`base_provider`, `base_model`), so routing is
/// symmetric: loading a persona repoints, clearing it repoints back. Sets
/// `NEWT_PROVIDER`/`NEWT_DGX_MODEL`, re-resolves via [`refresh_backend`], and
/// prints a line. Returns whether the URL changed (caller re-probes DGX).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_persona_backend(
    persona: Option<&Persona>,
    base_provider: &Option<String>,
    base_model: &Option<String>,
    cfg: &newt_core::ResolvedConfig,
    choice: &mut BackendChoice,
    inf_url: &mut String,
    inf_model: &mut String,
    inf_kind: &mut newt_core::BackendKind,
    inf_key: &mut Option<String>,
    inf_context_window: &mut Option<u32>,
    color: bool,
    verbose: bool,
) -> bool {
    let configured: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
    let has_backend = persona.and_then(|p| p.profile.backend.as_deref()).is_some();
    let (provider, model) = match persona_backend_route(persona.map(|p| &p.profile), &configured) {
        Ok(Some((backend, model))) => (Some(backend), model),
        // Revert to the pre-persona baseline (persona declares no backend / cleared).
        Ok(None) => (base_provider.clone(), base_model.clone()),
        Err(unknown) => {
            print_newt(
                &format!(
                    "persona names unknown backend '{unknown}' — leaving backend unchanged. configured: {}",
                    if configured.is_empty() { "(none)".to_string() } else { configured.join(", ") }
                ),
                color,
                verbose,
            );
            return false;
        }
    };
    // Publish the pair under ONE hold of the process-env lock (#1850), so no
    // guarded reader can observe a half-applied route. The old justification
    // here — "single-threaded REPL" — is true of the REPL and false under
    // `cargo test`, which runs tests as threads of one process.
    {
        let _env = newt_core::process_env::lock();
        newt_core::process_env::set_or_remove("NEWT_PROVIDER", provider.as_deref());
        newt_core::process_env::set_or_remove("NEWT_DGX_MODEL", model.as_deref());
    }
    // Track the backend by NAME across the re-resolve: two backends can share an
    // endpoint (e.g. `sol` and `openai` both on api.openai.com), so the URL alone
    // can't tell a route/revert happened — the name can.
    let prev_name = choice.name.clone();
    let url_changed = refresh_backend(
        cfg,
        choice,
        inf_url,
        inf_model,
        inf_kind,
        inf_key,
        inf_context_window,
        color,
        verbose,
    );
    if has_backend {
        print_newt(
            &format!(
                "persona backend → {} (model {})",
                choice.name,
                inf_model.as_str()
            ),
            color,
            verbose,
        );
    } else if choice.name != prev_name {
        // A cleared persona reverted the session to its pre-persona backend.
        print_newt(
            &format!("backend reverted to {} (persona cleared)", choice.name),
            color,
            verbose,
        );
    }
    url_changed
}

#[cfg(test)]
#[path = "lib_tests/persona_backend_tests.rs"]
mod persona_backend_tests;
