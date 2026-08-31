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
//! # One mutation path, and now one receipt (#1965)
//!
//! [`apply`] is the only place a setting changes, and it is PRIVATE.
//! [`apply_and_record`] is the only way to reach it: the form, a deep link
//! (`/settings edit-mode vi`), the deprecated verbs (`/vi`), the dial setters
//! (`/psyche tenacity`) and `/psyche obsessive` all go through it, and each
//! one writes a content-addressed `newt_core::settings_receipt` line.
//!
//! That privacy is the design. Slash commands reached no receipt path at all —
//! thirty-four commands mutating session state with nothing durable behind
//! them, which is how a round-cap escalation to effectively unlimited left no
//! record in config, receipts, turns or artifacts. Collapsing the knob verbs
//! into one chokepoint is what turned "instrument thirty-four call sites" into
//! "instrument one", and making the mutation unreachable without the recorder
//! is what stops the thirty-fifth from reintroducing the gap.
//!
//! # The deprecated verbs still work
//!
//! Operators have muscle memory. `/vi` does not become "unknown command" and
//! does not become a lecture — it applies the setting AND names where the
//! setting now lives. Routing it through [`apply`] rather than leaving the old
//! arm in place is what keeps "one mutation path" true rather than aspirational.

use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::settings_receipt::SettingChange;
use newt_core::HumanQuestionOutcome;
use newt_interaction::InteractionDefinition;

/// How the form reaches the operator: C1's seam, and nothing wider — the same
/// type `crew_form` uses, for the same reason.
pub(crate) type Ask<'a> = &'a dyn Fn(&SurfaceInteraction) -> HumanQuestionOutcome;

/// A setting `/settings` carries.
///
/// Two families so far: the editor mode (slice 1) and the effort dials (this
/// slice). Every one of them is enum-valued with a small closed vocabulary,
/// which is exactly the shape the operator's line singles out — *a verb that
/// merely SETS A VALUE is absorbed; a verb that PERFORMS something stays*.
///
/// `/psyche` is therefore NOT absorbed: its panel and its status view perform,
/// and they keep performing. What moved here is its two text setters, which
/// only ever set. See `docs/decisions/slash_command_target_set.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    EditMode,
    Tenacity,
    Cognition,
    Thinking,
    Nudge,
}

impl Field {
    /// Every field the form offers, menu order.
    pub(crate) const ALL: &'static [Self] = &[
        Self::EditMode,
        Self::Tenacity,
        Self::Cognition,
        Self::Thinking,
        Self::Nudge,
    ];

    /// The deep-link token: `/settings <name> [value]`. This is ALSO the
    /// registry's command name, which is what lets `apply` ask the registry
    /// where this setting's receipt lands rather than carrying a second list.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::EditMode => "edit-mode",
            Self::Tenacity => "tenacity",
            Self::Cognition => "cognition",
            Self::Thinking => "thinking",
            Self::Nudge => "nudge",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::EditMode => "line-editor key bindings",
            Self::Tenacity => "tenacity",
            Self::Cognition => "cognition",
            Self::Thinking => "thinking spinner",
            Self::Nudge => "action-pressure nudges",
        }
    }

    /// The allowed values, as `(value, what it means)`.
    ///
    /// The dial levels and their descriptions come from `Tenacity::all()` /
    /// `Cognition::all()` — the same lists `/psyche … list` renders. A second
    /// hand-written copy here is how the form and the dial would drift apart
    /// the first time a level is added.
    fn values(self) -> Vec<(&'static str, String)> {
        use newt_core::role_profile::Cognition;
        use newt_core::Tenacity;
        let owned = |pairs: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            pairs.iter().map(|(v, d)| (*v, (*d).to_string())).collect()
        };
        match self {
            Self::EditMode => owned(&[
                ("vi", "modal editing (default)"),
                ("emacs", "emacs-style editing"),
                ("nano", "nano-style editing"),
            ]),
            Self::Tenacity => std::iter::once((
                "auto",
                "inherit from the persona / config / model family".to_string(),
            ))
            .chain(
                Tenacity::all()
                    .into_iter()
                    .map(|t| (t.label(), t.describe())),
            )
            .collect(),
            Self::Cognition => {
                std::iter::once(("auto", "follow the active persona (default)".to_string()))
                    .chain(std::iter::once((
                        "off",
                        "send no reasoning controls, overriding any persona".to_string(),
                    )))
                    .chain(
                        Cognition::all()
                            .into_iter()
                            .map(|c| (c.label(), c.describe().to_string())),
                    )
                    .collect()
            }
            Self::Thinking => owned(&[
                ("on", "stream reasoning above the answer (default)"),
                ("off", "just the answer"),
            ]),
            Self::Nudge => owned(&[
                ("on", "action-pressure steering enabled (default)"),
                ("off", "no narration rescue / workflow repair / plan pushes"),
            ]),
        }
    }

    /// What this setting is right now, through the one resolver that already
    /// owns the precedence rather than a second reading of it.
    ///
    /// For the dials that means the OVERRIDE, not the effective level: the
    /// setting is what this form can change, and `auto` is a real, choosable
    /// value that resolution then fills in.
    fn current(self) -> String {
        use newt_core::cognition::{cli_cognition, CognitionOverride};
        match self {
            Self::EditMode => match crate::prompt::resolve_edit_mode() {
                newt_core::EditMode::Vi => "vi",
                newt_core::EditMode::Emacs => "emacs",
                newt_core::EditMode::Nano => "nano",
            }
            .to_string(),
            Self::Tenacity => newt_core::tenacity::cli_tenacity()
                .map_or("auto", newt_core::Tenacity::label)
                .to_string(),
            Self::Cognition => match cli_cognition() {
                CognitionOverride::Unset => "auto".to_string(),
                CognitionOverride::Off => "off".to_string(),
                CognitionOverride::Set(c) => c.label().to_string(),
            },
            Self::Thinking => if newt_core::agentic::thinking_stream_enabled() {
                "on"
            } else {
                "off"
            }
            .to_string(),
            Self::Nudge => if nudges_off() { "off" } else { "on" }.to_string(),
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
    ///
    /// The alias arms are not politeness — each one was accepted by the verb
    /// this field absorbed, and absorbing a command must not silently drop an
    /// affordance operators already type.
    fn accepts(self, value: &str) -> Option<&'static str> {
        let want = value.trim().to_lowercase();
        let want = match (self, want.as_str()) {
            // `/edit-mode vim`
            (Self::EditMode, "vim") => "vi",
            // `/psyche tenacity inherit|reset`
            (Self::Tenacity, "inherit" | "reset") => "auto",
            // `/psyche cognition none` / `reset` / `persona`
            (Self::Cognition, "none") => "off",
            (Self::Cognition, "reset" | "persona") => "auto",
            _ => want.as_str(),
        };
        self.values()
            .into_iter()
            .find(|(v, _)| *v == want)
            .map(|(v, _)| v)
    }
}

/// The one reading of the nudge dial — `NEWT_NUDGE=off` disables it, anything
/// else (including unset) leaves it on.
pub(crate) fn nudges_off() -> bool {
    std::env::var("NEWT_NUDGE").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
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
        .into_iter()
        .enumerate()
        .map(|(i, (value, what))| {
            let mark = if value == current { "  (current)" } else { "" };
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
/// deprecated verb, `/psyche tenacity`, `/psyche obsessive` — lands here.
///
/// **Private on purpose.** The receipt attaches in [`apply_and_record`], and a
/// caller that could reach the write without it would be the #1965 gap with
/// extra steps. The module's own tests call this directly, because what they
/// are checking is the mutation, not the record.
///
/// The `#1668` posture marks are made HERE, beside the write, for the same
/// reason: a caller that sets the dial and forgets to mark the axis leaves the
/// conversation's operator pin stale, and there is now exactly one caller.
fn apply(field: Field, value: &str) -> Result<String, String> {
    use newt_core::cognition::{set_cli_cognition, CognitionOverride};
    use newt_core::role_profile::Cognition;
    use newt_core::runtime::{mark_cognition_choice, mark_tenacity_choice};
    use newt_core::tenacity::{clear_cli_tenacity, set_cli_tenacity};
    use newt_core::Tenacity;

    let Some(value) = field.accepts(value) else {
        let allowed: Vec<&str> = field.values().into_iter().map(|(v, _)| v).collect();
        return Err(format!(
            "/settings {} takes one of: {}",
            field.name(),
            allowed.join(", ")
        ));
    };
    match field {
        // Under the process-env lock (#1850); the editor is rebuilt right
        // after a slash command returns, before further input is read.
        Field::EditMode => newt_core::process_env::set_var("NEWT_EDIT_MODE", value),
        Field::Tenacity => match value.parse::<Tenacity>() {
            Ok(level) => {
                set_cli_tenacity(level);
                mark_tenacity_choice(Some(level));
            }
            // `auto` — the only value that does not parse as a level, because
            // `accepts` already refused everything else.
            Err(_) => {
                clear_cli_tenacity();
                mark_tenacity_choice(None);
            }
        },
        Field::Cognition => {
            let choice = match value {
                "auto" => CognitionOverride::Unset,
                "off" => CognitionOverride::Off,
                level => level
                    .parse::<Cognition>()
                    .map_or(CognitionOverride::Unset, CognitionOverride::Set),
            };
            set_cli_cognition(choice);
            mark_cognition_choice(choice);
        }
        Field::Thinking => newt_core::process_env::set_var("NEWT_THINKING", value),
        // `on` REMOVES the variable rather than setting it, which is what the
        // absorbed `/nudge on` did: the readers test for `=off`, so an unset
        // variable and `on` are the same state and only one of them is a
        // leftover for the next `/nudge status` to explain.
        Field::Nudge => {
            if value == "off" {
                newt_core::process_env::set_var("NEWT_NUDGE", "off");
            } else {
                newt_core::process_env::remove_var("NEWT_NUDGE");
            }
        }
    }
    Ok(format!("{}: {value}", field.label()))
}

/// The change a transition is recorded as, or `None` when the registry
/// declares no destination for this setting.
///
/// **This is the production reader of the registry's receipt column.** The
/// decision is data, not a call site's memory: a field is receipted because
/// `slash_registry` says where its receipt lands. A `Receipt::Missing` field
/// writes nothing and stays counted in the #1965 debt, which is the honest
/// answer — a receipt sent to a destination nobody declared looks like
/// coverage without being any.
///
/// Pure, so the whole decision is exercised with no filesystem: the write
/// itself belongs to `settings_receipt::record`.
fn change_for(field: Field, from: &str, to: &str, via: &str) -> Option<SettingChange> {
    match crate::slash_registry::receipt_for(field.name()) {
        crate::slash_registry::Receipt::Journal => {
            Some(SettingChange::new(field.name(), from, to, via))
        }
        _ => None,
    }
}

/// **THE ONE ROUTE OUT.** Apply a setting and durably record that it happened.
///
/// [`apply`] is private precisely so this cannot be bypassed: a caller that
/// could reach the mutation without the receipt is the #1965 defect with extra
/// steps, and there were thirty-four of those. `via` is the verb the operator
/// actually typed — `/vi` and `/settings edit-mode` produce the same setting
/// and two different events, and the route is the half a reader cannot
/// reconstruct afterwards.
///
/// A no-op (`/vi` when already `vi`) is recorded too. The journal records what
/// the operator DID, and "asked for vi, was already vi" is a fact about the
/// session, not noise to be filtered by a rule nobody can see.
///
/// Recording is best effort by construction (`settings_receipt::record`
/// swallows its own failures): failing to observe a change must never undo the
/// change.
pub(crate) fn apply_and_record(field: Field, value: &str, via: &str) -> Result<String, String> {
    let from = field.current();
    let message = apply(field, value)?;
    if let Some(change) = change_for(field, &from, &field.current(), via) {
        let _ = newt_core::settings_receipt::record(change);
    }
    Ok(message)
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
            return vec![match apply_and_record(field, second, "/settings") {
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
    let values = field.values();
    let Some((value, _)) = values.get(index - 1) else {
        return vec!["settings: cancelled".to_string()];
    };
    vec![match apply_and_record(field, value, "/settings") {
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

    /// Every test that reaches [`apply_and_record`] holds this. It serializes
    /// the process-global dials AND turns the receipt journal off, so a unit
    /// test cannot append to the developer's real `~/.newt/receipts.jsonl` —
    /// which it did until the guard learned about the journal.
    fn settings_guard() -> newt_core::test_guard::GlobalSettingsGuard {
        newt_core::test_guard::GlobalSettingsGuard::acquire()
    }

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
        let _g = settings_guard();
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
        let _g = settings_guard();
        // "1" selects the first field, "1" selects its first value (vi).
        let out = run(&answering(&["1", "1"]), "");
        assert_eq!(out, vec!["line-editor key bindings: vi".to_string()]);
        assert_eq!(Field::EditMode.current(), "vi");
    }

    /// **Anti-vacuous twin.** Every assertion above would hold over a form
    /// that applied unconditionally. Backing out changes nothing.
    #[test]
    fn cancelling_the_form_changes_no_setting() {
        let _g = settings_guard();
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
        let _g = settings_guard();
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
        let _g = settings_guard();
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

    /// **Every dial the form claims reports the value its own resolver owns.**
    ///
    /// The knobs are process globals set through four different modules; a
    /// form that read them by hand would show a stale value the moment one
    /// module changed. Each `current()` goes through that module's reader.
    #[test]
    fn each_field_reports_what_its_own_resolver_says() {
        let _g = settings_guard();
        for (field, value) in [
            (Field::EditMode, "nano"),
            (Field::Tenacity, "insistent"),
            (Field::Cognition, "deliberating"),
            (Field::Thinking, "off"),
            (Field::Nudge, "off"),
        ] {
            apply(field, value).expect("an offered value applies");
            assert_eq!(field.current(), value, "{} did not read back", field.name());
        }
        // ...and every one of them goes back, which a write-only global would
        // not manage.
        for (field, value) in [
            (Field::Tenacity, "auto"),
            (Field::Cognition, "auto"),
            (Field::Thinking, "on"),
            (Field::Nudge, "on"),
        ] {
            apply(field, value).expect("an offered value applies");
            assert_eq!(field.current(), value, "{} did not release", field.name());
        }
    }

    /// The dial levels come from the dial, not a second list in this file.
    ///
    /// Adding a `Tenacity` level must make it choosable here without anyone
    /// remembering to; a hand-copied list is how `/psyche … list` and the form
    /// would silently disagree.
    #[test]
    fn the_dial_levels_are_the_dials_own() {
        let offered: Vec<&str> = Field::Tenacity
            .values()
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        for level in newt_core::Tenacity::all() {
            assert!(offered.contains(&level.label()), "{level:?} not offered");
        }
        assert!(offered.contains(&"auto"), "the release value is missing");
        assert_eq!(offered.len(), newt_core::Tenacity::all().len() + 1);

        let offered: Vec<&str> = Field::Cognition
            .values()
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        for level in newt_core::role_profile::Cognition::all() {
            assert!(offered.contains(&level.label()), "{level:?} not offered");
        }
        assert!(offered.contains(&"off") && offered.contains(&"auto"));
    }

    /// **Every alias the absorbed verbs took still resolves.** Each of these
    /// was reachable before the family moved; absorbing a command must not
    /// quietly delete an affordance operators already type.
    #[test]
    fn the_aliases_the_absorbed_verbs_took_still_resolve() {
        for (field, typed, want) in [
            (Field::EditMode, "vim", "vi"),
            (Field::Tenacity, "inherit", "auto"),
            (Field::Tenacity, "reset", "auto"),
            (Field::Cognition, "none", "off"),
            (Field::Cognition, "reset", "auto"),
            (Field::Cognition, "persona", "auto"),
        ] {
            assert_eq!(
                field.accepts(typed),
                Some(want),
                "/{} {typed} stopped resolving",
                field.name()
            );
        }
        // Anti-vacuous: `accepts` is not "yes to everything".
        assert_eq!(Field::Tenacity.accepts("obsessive"), None);
        assert_eq!(Field::Cognition.accepts("hard"), None);
        assert_eq!(Field::Thinking.accepts("maybe"), None);
    }

    /// A refused value on a dial writes nothing — the same law slice 1 pinned
    /// for the editor, now that four more fields can be refused.
    #[test]
    fn a_refused_dial_value_changes_nothing() {
        let _g = settings_guard();
        apply(Field::Tenacity, "insistent").expect("a level applies");
        let err = apply(Field::Tenacity, "obsessive").expect_err("not a level");
        assert!(err.contains("relentless"), "{err}");
        assert_eq!(Field::Tenacity.current(), "insistent", "a refusal wrote");
    }

    /// **The receipt destination is read from the registry, not assumed.**
    ///
    /// This is the law that keeps the two files honest with each other: a
    /// field of `/settings` whose verb the registry has not given a
    /// destination writes nothing, and the ratchet keeps counting it. You
    /// cannot add a knob to this form and get provenance by accident.
    #[test]
    fn every_field_declares_where_its_receipt_lands() {
        use crate::slash_registry::{receipt_for, Receipt};
        for field in Field::ALL {
            assert_eq!(
                receipt_for(field.name()),
                Receipt::Journal,
                "/settings {} has no declared receipt destination",
                field.name()
            );
            assert!(change_for(*field, "a", "b", "/settings").is_some());
        }
        // **Anti-vacuous.** If the column read `Journal` for everything,
        // reading it would prove nothing. `/rounds` is the #1965 command
        // itself and is still undeclared; `/help` mutates nothing.
        assert_eq!(receipt_for("rounds"), Receipt::Missing);
        assert_eq!(receipt_for("help"), Receipt::None_);
        assert_eq!(receipt_for("zzznotacommand"), Receipt::Missing);
    }

    /// The recorded change carries the transition AND the route taken — the
    /// half of the event a reader cannot reconstruct from the resulting state.
    #[test]
    fn the_recorded_change_names_the_route_and_the_transition() {
        let change = change_for(Field::EditMode, "vi", "emacs", "/vi").expect("declared");
        assert_eq!(change.setting, "edit-mode");
        assert_eq!(change.from, "vi");
        assert_eq!(change.to, "emacs");
        assert_eq!(change.via, "/vi");
        assert_eq!(
            change.schema,
            newt_core::settings_receipt::SETTING_CHANGE_SCHEMA_V1
        );
    }

    /// The recording route applies exactly what the bare mutation would.
    ///
    /// The guard holds the journal off, so this creates no filesystem state:
    /// what it proves is that `apply_and_record` still performs the change,
    /// not what the written line looks like — which
    /// `newt_core::settings_receipt`'s own tests own.
    #[test]
    fn recording_does_not_change_what_gets_applied() {
        let _g = settings_guard();
        let recorded = apply_and_record(Field::Tenacity, "relentless", "/settings")
            .expect("an offered value applies");
        assert_eq!(Field::Tenacity.current(), "relentless");
        apply(Field::Tenacity, "auto").expect("released");
        let bare = apply(Field::Tenacity, "relentless").expect("an offered value applies");
        assert_eq!(recorded, bare, "the recorder changed the outcome");
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
