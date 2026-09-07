//! Test families for [`super`] (`backend_panel`), split out of
//! `backend_panel.rs` by what each test asserts. Sibling files; no behaviour
//! change. `rich-tui`-gated with the module it tests: the whole panel is
//! `#[cfg(feature = "rich-tui")]` at `lib.rs:204`, and there is no lean
//! counterpart to keep in step.
//!
//! The six fixtures live here because `panel()` is read by all seven
//! families and `ok_remove()` by four.

// A glob RE-EXPORT, not a plain glob: the moved bodies write `super::X`
// from when `super` was `backend_panel`, and a private glob binding is not
// nameable by path from a child module.
pub(crate) use super::*;

fn named(name: &str, source: BackendSource) -> BackendOption {
    BackendOption {
        name: name.to_string(),
        selection: BackendSelection::Named(name.to_string()),
        source,
        kind: Some(BackendKind::Ollama),
        endpoint: format!("http://{name}:11434"),
        model: Some("qwen3:30b".to_string()),
        api_key_env: None,
        api_key_file: None,
    }
}

/// dgx1 (active, file-backed) · gpu-runner (file-backed) · relic (inline) + the
/// two kind fallbacks. `default_backend` is the active one unless a test
/// says otherwise.
fn panel() -> PanelState {
    seeded(Some("dgx1"))
}

fn seeded(default_backend: Option<&str>) -> PanelState {
    PanelState::new(PanelSeed {
        options: vec![
            named("dgx1", BackendSource::UserDropIn),
            named("gpu-runner", BackendSource::UserDropIn),
            named("relic", BackendSource::Inline),
            BackendOption::kind_fallback("ollama"),
            BackendOption::kind_fallback("openai"),
        ],
        active: Some(0),
        default_backend: default_backend.map(str::to_string),
    })
}

fn ok_persist() -> impl FnMut(&BackendEdit) -> BackendSaveResult {
    |edit: &BackendEdit| BackendSaveResult::Saved {
        note: format!("saved backend '{}'", edit.name),
    }
}

fn ok_remove() -> impl FnMut(&str) -> Result<String, String> {
    |name: &str| Ok(format!("removed backend '{name}'"))
}

/// Drive the state into the form and type `text` into the selected field.
fn type_text(s: &mut PanelState, text: &str) {
    for c in text.chars() {
        s.form_input(c);
    }
}

#[cfg(test)]
#[path = "edit_form.rs"]
mod edit_form;
#[cfg(test)]
#[path = "file_changes.rs"]
mod file_changes;
#[cfg(test)]
#[path = "key_handling.rs"]
mod key_handling;
#[cfg(test)]
#[path = "kind_round_trip.rs"]
mod kind_round_trip;
#[cfg(test)]
#[path = "removal_flow.rs"]
mod removal_flow;
#[cfg(test)]
#[path = "removal_refusals.rs"]
mod removal_refusals;
#[cfg(test)]
#[path = "spinner_selection.rs"]
mod spinner_selection;
