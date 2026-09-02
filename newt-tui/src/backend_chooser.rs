//! Opening the backend chooser: its options, its writers, and one call.
//!
//! Lifted VERBATIM out of `chat.rs`'s `/backends` arm, which is the only thing
//! this module does. It exists because a second caller is arriving — the
//! `/settings` panel's backend row — and the alternative was assembling the
//! same options and the same two filesystem closures a second time, in a
//! second place, from the same config.
//!
//! # What stays in the caller
//!
//! The COMMIT half. Applying a pick reroutes the session, refreshes runtime
//! state, and reports from it; removing the active backend is a one-transaction
//! close the session owns. None of that is "open the chooser", and dragging it
//! in here would make this module the second place that knows how a session
//! switches backends — which is the sprawl this move exists to avoid, pointed
//! the other way.
//!
//! So the split is: this module answers *what is choosable and how does an edit
//! reach disk*; the caller answers *what does the operator's pick mean for this
//! session*.

use std::collections::HashSet;

use crate::backend_panel::{
    self, BackendEdit, BackendOption, BackendSaveResult, BackendSelection, BackendSource,
    PanelClose, PanelRunError, PanelSeed,
};
use crate::setup;

/// Backend names defined by drop-in files under `path`'s directory.
fn names_in(path: Option<std::path::PathBuf>) -> HashSet<String> {
    path.map(|p| setup::panel_backend_file_names(&p).into_iter().collect())
        .unwrap_or_default()
}

/// Every backend the chooser offers, tagged with WHERE its definition lives.
///
/// The exact set `/backends <name>` can switch to, plus the bare wire-kind
/// fallbacks the `/backend <openai|ollama>` form supports.
fn options_for(cfg: &newt_core::ResolvedConfig) -> Vec<BackendOption> {
    let dropins = names_in(newt_core::Config::user_config_path());
    // `Config::merge_disk_backends` merges the PROJECT `.newt/backends` LAST
    // (last-wins), so a name defined in both resolves to the project file:
    // editing or removing the user drop-in would be a silent no-op and a
    // phantom delete (review §3). Mark those read-only.
    let project_dropins = names_in(newt_core::Config::project_config_path());
    // A same-named inline [[backends]] entry keeps supplying whatever the
    // drop-in omits (review §4).
    let inline = inline_names();

    let mut options: Vec<BackendOption> = cfg
        .backends
        .iter()
        .map(|b| BackendOption {
            name: b.name.clone(),
            selection: BackendSelection::Named(b.name.clone()),
            source: if project_dropins.contains(&b.name) {
                BackendSource::ShadowedByProject
            } else if !dropins.contains(&b.name) {
                BackendSource::Inline
            } else if inline.contains(&b.name) {
                BackendSource::UserDropInOverInline
            } else {
                BackendSource::UserDropIn
            },
            kind: b.kind,
            endpoint: b.endpoint.clone(),
            model: b.model.clone(),
            api_key_env: b.api_key_env.clone(),
            api_key_file: b.api_key_file.clone(),
        })
        .collect();
    for kind in ["ollama", "openai"] {
        options.push(BackendOption::kind_fallback(kind));
    }
    options
}

fn inline_names() -> HashSet<String> {
    newt_core::Config::user_config_path()
        .map(|p| setup::inline_backend_names(&p).into_iter().collect())
        .unwrap_or_default()
}

/// Which option the session currently resolves to.
fn active_index(cfg: &newt_core::ResolvedConfig, options: &[BackendOption]) -> Option<usize> {
    match crate::active_backend_name(cfg) {
        Some(n) => options
            .iter()
            .position(|o| matches!(&o.selection, BackendSelection::Named(m) if *m == n)),
        // No named match (env-shim / kind-forced session): mark the kind
        // fallback the session resolves to.
        None => crate::resolve_backend_choice(cfg).ok().and_then(|choice| {
            let k = choice.kind.label();
            options
                .iter()
                .position(|o| matches!(o.selection, BackendSelection::Kind(s) if s == k))
        }),
    }
}

/// The seed the chooser opens on.
pub(crate) fn seed(cfg: &newt_core::ResolvedConfig) -> PanelSeed {
    let options = options_for(cfg);
    let active = active_index(cfg, &options);
    PanelSeed {
        options,
        active,
        default_backend: cfg.default_backend.clone(),
    }
}

/// Open the chooser and report how it closed.
///
/// The two closures are the panel's ONLY filesystem writes, both riding the
/// setup wizard's crash-safe lock + plan machinery (#1660) — never a second
/// write path. A failure keeps the panel open with a visible status and mutates
/// nothing (config_panel review-3 §1).
///
/// # Errors
///
/// A mid-panel terminal failure, carrying the file operations that already
/// committed so the caller can still report them.
pub(crate) fn choose(
    cfg: &newt_core::ResolvedConfig,
    window: Option<crate::session_worker::PanelWindow>,
) -> Result<PanelClose, PanelRunError> {
    let inline = inline_names();
    let persist = |edit: &BackendEdit| {
        let Some(path) = newt_core::Config::user_config_path() else {
            return BackendSaveResult::Failed("no user config directory".to_string());
        };
        match setup::persist_panel_backend(&path, edit) {
            // A save with an after-commit sync WARNING is a save: the bytes are
            // on disk, so the note says so instead of claiming the edit failed
            // (review §10).
            Ok(saved) => BackendSaveResult::Saved {
                note: std::iter::once(backend_panel::saved_note(
                    &edit.name,
                    &saved.path,
                    inline.contains(&edit.name),
                ))
                .chain(saved.warnings)
                .collect::<Vec<_>>()
                .join(" — "),
            },
            Err(e) => BackendSaveResult::Failed(format!("{e:#}")),
        }
    };
    // The in-loop `:d` never repoints `default_backend`: the panel routes a
    // default/active removal through the caller's one-transaction close, and
    // setup refuses here as a second line of defence.
    let remove = |name: &str| -> Result<String, String> {
        let path = newt_core::Config::user_config_path()
            .ok_or_else(|| "no user config directory".to_string())?;
        setup::remove_panel_backend(&path, name, None)
            .map(|notes| {
                std::iter::once(format!("removed backend '{name}'"))
                    .chain(notes)
                    .collect::<Vec<_>>()
                    .join(" — ")
            })
            .map_err(|e| format!("{e:#}"))
    };
    backend_panel::run(seed(cfg), persist, remove, window)
}

#[cfg(test)]
mod tests {
    /// **The chooser is assembled in ONE place.**
    ///
    /// The options and the two filesystem closures were inline in `chat.rs`'s
    /// `/backends` arm; a second caller (the `/settings` backend row) would
    /// have meant a second assembly from the same config, and two answers to
    /// "which backends are choosable" drift the first time one learns about a
    /// new source. Counted over the source because the property is "no other
    /// path exists".
    #[test]
    fn the_chooser_is_not_assembled_anywhere_else() {
        // Plain `include_str!`, not `production_source`: that helper requires
        // an unindented `#[cfg(test)] mod tests {` marker to cut at, and
        // `chat.rs` has no test module. The needles below appear nowhere in a
        // comment, so there is nothing to cut.
        let chat = include_str!("chat.rs");
        assert_eq!(
            chat.matches("BackendOption {").count(),
            0,
            "chat.rs must ask backend_chooser for the options, not build them"
        );
        assert_eq!(
            chat.matches("setup::persist_panel_backend(").count(),
            0,
            "the chooser's writers live with the chooser"
        );
        assert!(
            chat.contains("backend_chooser::choose("),
            "chat.rs opens the chooser through the one entry point"
        );
    }
}
