//! `/settings` — **one typed form for the knob space** (#1981 slice 1).
//!
//! The operator's directive: aggressively reduce the slash surface. The line
//! that decides each command is **a verb that merely SETS A VALUE is absorbed;
//! a verb that PERFORMS something stays** — and `/vi`, `/emacs`, `/nano`,
//! `/edit-mode` are four verbs for one enum-valued setting.
//!
//! # Why a form and not a fifth verb
//!
//! A form is an `InteractionDefinition`, so it renders through C0's plain
//! projection, through the RichTUI span projection, and on the web — none of
//! which a slash verb does. `crew_form` (D1a, #1885) is the precedent this
//! copies rather than reinvents: the flow is a sequence of definitions asked
//! through C1's seam, so it never carries a private console path and the unit
//! tier stays fully mocked by injecting [`Ask`].
//!
//! **No panel this slice.** `/settings` ships through the plain/typed path
//! only. RegionLease (#1986) has landed and a leased inline panel is a later
//! upgrade; a chooser you drive blind is not a consolidation.
//!
//! # One mutation path, which is the point (#1965)
//!
//! [`apply`] is the only place a setting changes, and every route reaches it:
//! the form, a deep link (`/settings edit-mode vi`), and the deprecated verbs
//! (`/vi`). That is what makes the receipt tractable — one site to instrument
//! instead of one per verb. Slash commands reach no receipt path today, which
//! is how a round-cap escalation to unlimited left no durable record; the
//! registry counts 33 such commands and this is the first to have a single
//! chokepoint where that can be fixed.
//!
//! # The deprecated verbs still work
//!
//! Operators have muscle memory. `/vi` does not become "unknown command" and
//! does not become a lecture — it applies the setting AND names where the
//! setting now lives. Routing it through [`apply`] rather than leaving the old
//! arm in place is what keeps "one mutation path" true rather than aspirational.

use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::HumanQuestionOutcome;
use newt_interaction::InteractionDefinition;

/// How the form reaches the operator: C1's seam, and nothing wider — the same
/// type `crew_form` uses, for the same reason.
pub(crate) type Ask<'a> = &'a dyn Fn(&SurfaceInteraction) -> HumanQuestionOutcome;

/// A setting `/settings` carries.
///
/// Slice 1 carries editor mode. The tuning dials are NOT here and that is
/// deliberate: #1665 already folded `/tenacity` and `/cognition` into
/// `/psyche`, which is a panel — absorbing those means subsuming a panel, and
/// panels wait on the RegionLease work. See
/// `docs/decisions/slash_command_inventory.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    EditMode,
}

impl Field {
    /// Every field the form offers, menu order.
    pub(crate) const ALL: &'static [Self] = &[Self::EditMode];

    /// The deep-link token: `/settings <name> [value]`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::EditMode => "edit-mode",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::EditMode => "line-editor key bindings",
        }
    }

    /// The allowed values, as `(value, what it means)`.
    fn values(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::EditMode => &[
                ("vi", "modal editing (default)"),
                ("emacs", "emacs-style editing"),
                ("nano", "nano-style editing"),
            ],
        }
    }

    /// What this setting is right now, through the one resolver that already
    /// owns the precedence (env, then `[tui]`, then the default) rather than a
    /// second reading of it.
    fn current(self) -> &'static str {
        match self {
            Self::EditMode => match crate::prompt::resolve_edit_mode() {
                newt_core::EditMode::Vi => "vi",
                newt_core::EditMode::Emacs => "emacs",
                newt_core::EditMode::Nano => "nano",
            },
        }
    }

    /// Resolve a deep-link token to a field.
    pub(crate) fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|f| f.name().eq_ignore_ascii_case(token))
    }

    /// Whether `value` is one this field accepts, normalized.
    fn accepts(self, value: &str) -> Option<&'static str> {
        let want = value.trim().to_lowercase();
        // `vim` is an alias operators type; the old `/edit-mode` arm took it,
        // so absorbing the command must not silently drop it.
        let want = if self == Self::EditMode && want == "vim" {
            "vi".to_string()
        } else {
            want
        };
        self.values()
            .iter()
            .find(|(v, _)| *v == want)
            .map(|(v, _)| *v)
    }
}

/// The top-level form: which setting to change, each showing its current value.
pub(crate) fn field_menu() -> InteractionDefinition {
    let entries: Vec<(String, String)> = Field::ALL
        .iter()
        .enumerate()
        .map(|(i, f)| {
            (
                (i + 1).to_string(),
                format!("{} — currently {}", f.label(), f.current()),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    newt_core::interaction_form::menu("settings", "Esc cancels", &refs)
}

/// The second step: which value for `field`.
pub(crate) fn value_menu(field: Field) -> InteractionDefinition {
    let current = field.current();
    let entries: Vec<(String, String)> = field
        .values()
        .iter()
        .enumerate()
        .map(|(i, (value, what))| {
            let mark = if *value == current { "  (current)" } else { "" };
            ((i + 1).to_string(), format!("{value} — {what}{mark}"))
        })
        .collect();
    let refs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    newt_core::interaction_form::menu(field.label(), "Esc cancels", &refs)
}

/// **THE ONE MUTATION PATH.** Every route — the form, a deep link, a
/// deprecated verb — lands here.
///
/// This is the site a provenance receipt attaches to (#1965). It does not
/// write one yet, and that is the honest state: the registry counts 33
/// state-mutating commands that record nothing, and collapsing four verbs to
/// one chokepoint is what makes instrumenting them a single change rather
/// than thirty-three.
pub(crate) fn apply(field: Field, value: &str) -> Result<String, String> {
    let Some(value) = field.accepts(value) else {
        let allowed: Vec<&str> = field.values().iter().map(|(v, _)| *v).collect();
        return Err(format!(
            "/settings {} takes one of: {}",
            field.name(),
            allowed.join(", ")
        ));
    };
    match field {
        Field::EditMode => {
            // Under the process-env lock (#1850); the editor is rebuilt right
            // after a slash command returns, before further input is read.
            newt_core::process_env::set_var("NEWT_EDIT_MODE", value);
        }
    }
    Ok(format!("{}: {value}", field.label()))
}

/// What a deprecated verb prints in addition to doing its job.
pub(crate) fn moved_notice(verb: &str, field: Field) -> String {
    format!(
        "(/{verb} still works; this setting now lives in /settings {})",
        field.name()
    )
}

/// Drive `/settings [field] [value]`.
///
/// `rest` is everything after the command word. Empty opens the form; one
/// token deep-links to a field's value menu; two apply directly. Returns the
/// lines to print — the caller owns the terminal, this owns no I/O.
pub(crate) fn run(ask: Ask<'_>, rest: &str) -> Vec<String> {
    let mut parts = rest.trim().splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("").trim();
    let second = parts.next().unwrap_or("").trim();

    // `/settings <field> <value>` — no question needed.
    if !first.is_empty() {
        let Some(field) = Field::from_token(first) else {
            let known: Vec<&str> = Field::ALL.iter().map(|f| f.name()).collect();
            return vec![format!(
                "/settings has no `{first}` setting (have: {})",
                known.join(", ")
            )];
        };
        if !second.is_empty() {
            return vec![match apply(field, second) {
                Ok(msg) | Err(msg) => msg,
            }];
        }
        return ask_value(ask, field);
    }

    // Bare `/settings` — pick a field, then a value.
    let menu = field_menu();
    let Some(choice) = ask_choice(ask, &menu) else {
        return vec!["settings: cancelled".to_string()];
    };
    let Some(index) = choice.parse::<usize>().ok().filter(|i| *i >= 1) else {
        return vec!["settings: cancelled".to_string()];
    };
    let Some(field) = Field::ALL.get(index - 1).copied() else {
        return vec!["settings: cancelled".to_string()];
    };
    ask_value(ask, field)
}

fn ask_value(ask: Ask<'_>, field: Field) -> Vec<String> {
    let menu = value_menu(field);
    let Some(choice) = ask_choice(ask, &menu) else {
        return vec!["settings: cancelled".to_string()];
    };
    let Some(index) = choice.parse::<usize>().ok().filter(|i| *i >= 1) else {
        return vec!["settings: cancelled".to_string()];
    };
    let Some((value, _)) = field.values().get(index - 1) else {
        return vec!["settings: cancelled".to_string()];
    };
    vec![match apply(field, value) {
        Ok(msg) | Err(msg) => msg,
    }]
}

/// Ask one menu and return the chosen option id, or `None` when the operator
/// backed out. The resolution is `interaction_form::resolve`'s, not a second
/// parser.
fn ask_choice(ask: Ask<'_>, definition: &InteractionDefinition) -> Option<String> {
    let interaction = SurfaceInteraction::blocking(definition.clone());
    match ask(&interaction) {
        HumanQuestionOutcome::Answer(answer) => {
            newt_core::interaction_form::resolve(definition, answer.trim())
                .map(|id| id.as_str().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::process_env;

    /// A scripted operator. The `Ask` seam is injected, so the whole form is
    /// exercised with no terminal, no I/O and no real prompt — the unit tier
    /// stays fully mocked, which is the reason `crew_form`'s shape was copied
    /// rather than a private console path invented.
    fn answering(
        answers: &'static [&'static str],
    ) -> impl Fn(&SurfaceInteraction) -> HumanQuestionOutcome {
        let next = std::cell::Cell::new(0usize);
        move |_| {
            let i = next.get();
            next.set(i + 1);
            answers.get(i).map_or(HumanQuestionOutcome::Cancelled, |a| {
                HumanQuestionOutcome::Answer((*a).to_string())
            })
        }
    }

    fn declining() -> impl Fn(&SurfaceInteraction) -> HumanQuestionOutcome {
        |_| HumanQuestionOutcome::Cancelled
    }

    /// **Deep link: `/settings edit-mode vi` asks nothing and applies.**
    #[test]
    fn a_full_deep_link_applies_without_a_question() {
        let _lock = process_env::lock();
        let asked = std::cell::Cell::new(false);
        let ask = |_: &SurfaceInteraction| {
            asked.set(true);
            HumanQuestionOutcome::Cancelled
        };
        let out = run(&ask, "edit-mode emacs");
        assert!(!asked.get(), "a fully-specified deep link must not prompt");
        assert_eq!(out, vec!["line-editor key bindings: emacs".to_string()]);
        assert_eq!(Field::EditMode.current(), "emacs");
    }

    /// The two-step form: pick the field, then the value.
    #[test]
    fn the_form_walks_field_then_value() {
        let _lock = process_env::lock();
        // "1" selects the first field, "1" selects its first value (vi).
        let out = run(&answering(&["1", "1"]), "");
        assert_eq!(out, vec!["line-editor key bindings: vi".to_string()]);
        assert_eq!(Field::EditMode.current(), "vi");
    }

    /// **Anti-vacuous twin.** Every assertion above would hold over a form
    /// that applied unconditionally. Backing out changes nothing.
    #[test]
    fn cancelling_the_form_changes_no_setting() {
        let _lock = process_env::lock();
        let _ = apply(Field::EditMode, "nano");
        let before = Field::EditMode.current();
        let out = run(&declining(), "");
        assert_eq!(out, vec!["settings: cancelled".to_string()]);
        assert_eq!(
            Field::EditMode.current(),
            before,
            "a cancelled form must not write"
        );
    }

    /// A value the field does not accept is refused with the allowed set, and
    /// writes nothing.
    #[test]
    fn an_unaccepted_value_is_refused_and_writes_nothing() {
        let _lock = process_env::lock();
        let _ = apply(Field::EditMode, "vi");
        let err = apply(Field::EditMode, "ed").expect_err("`ed` is not an edit mode");
        assert!(err.contains("vi, emacs, nano"), "{err}");
        assert_eq!(Field::EditMode.current(), "vi", "a refusal must not write");
    }

    /// `vim` was accepted by the old `/edit-mode` arm. Absorbing a command
    /// must not silently drop an alias operators type.
    #[test]
    fn the_vim_alias_the_old_verb_took_still_resolves() {
        assert_eq!(Field::EditMode.accepts("vim"), Some("vi"));
        assert_eq!(Field::EditMode.accepts("VI"), Some("vi"));
        assert_eq!(Field::EditMode.accepts("ed"), None);
    }

    /// An unknown setting name lists what exists rather than failing silently.
    #[test]
    fn an_unknown_field_names_the_ones_that_exist() {
        let out = run(&declining(), "nosuchsetting");
        assert!(out[0].contains("no `nosuchsetting` setting"), "{out:?}");
        assert!(out[0].contains("edit-mode"), "{out:?}");
    }

    /// The menus are typed definitions, not printed strings — which is what
    /// makes them render on the plain scroller, the RichTUI and the web.
    #[test]
    fn the_menus_are_definitions_carrying_every_choice() {
        let _lock = process_env::lock();
        let menu = value_menu(Field::EditMode);
        let rendered = newt_core::markup::plain::render(&menu);
        for value in ["vi", "emacs", "nano"] {
            assert!(rendered.contains(value), "{value} missing: {rendered}");
        }
        assert!(
            rendered.contains("(current)"),
            "the current value is marked: {rendered}"
        );
        assert_eq!(field_menu().controls.len(), 1, "one choice control");
    }

    /// The deprecated verbs point at their new home and say they still work —
    /// never "unknown command" (#1981).
    #[test]
    fn a_deprecated_verb_names_its_replacement_without_retiring_itself() {
        let notice = moved_notice("vi", Field::EditMode);
        assert!(notice.contains("/vi still works"), "{notice}");
        assert!(notice.contains("/settings edit-mode"), "{notice}");
    }
}
