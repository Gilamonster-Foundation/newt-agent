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
/// What a field will accept.
///
/// Four of the five settings are a closed vocabulary, and a menu is the right
/// surface for those. The round cap is a NUMBER, so it gets a text field —
/// widening the form rather than bolting a second dispatch beside it, which is
/// what forced `/rounds` to keep its own unrecorded write until now.
pub(crate) enum ValueSpace {
    /// A closed vocabulary, rendered as a menu of `(value, what it means)`.
    Choice(Vec<(&'static str, String)>),
    /// A number in `min..=max`, plus one token that RELEASES the setting back
    /// to its derived default (`auto`).
    Number {
        release: &'static str,
        min: usize,
        max: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    EditMode,
    Tenacity,
    Cognition,
    Thinking,
    Nudge,
    Markdown,
    Rounds,
}

impl Field {
    /// Every field the form offers, menu order.
    pub(crate) const ALL: &'static [Self] = &[
        Self::EditMode,
        Self::Tenacity,
        Self::Cognition,
        Self::Thinking,
        Self::Nudge,
        Self::Markdown,
        Self::Rounds,
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
            Self::Markdown => "markdown",
            Self::Rounds => "rounds",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::EditMode => "line-editor key bindings",
            Self::Tenacity => "tenacity",
            Self::Cognition => "cognition",
            Self::Thinking => "reasoning display",
            Self::Nudge => "action-pressure nudges",
            Self::Markdown => "markdown rendering",
            Self::Rounds => "tool-call round limit",
        }
    }

    /// The allowed values, as `(value, what it means)`.
    ///
    /// The dial levels and their descriptions come from `Tenacity::all()` /
    /// `Cognition::all()` — the same lists `/psyche … list` renders. A second
    /// hand-written copy here is how the form and the dial would drift apart
    /// the first time a level is added.
    pub(crate) fn value_space(self) -> ValueSpace {
        use newt_core::role_profile::Cognition;
        use newt_core::Tenacity;
        let owned = |pairs: &[(&'static str, &str)]| -> ValueSpace {
            ValueSpace::Choice(pairs.iter().map(|(v, d)| (*v, (*d).to_string())).collect())
        };
        match self {
            Self::EditMode => owned(&[
                ("vi", "modal editing (default)"),
                ("emacs", "emacs-style editing"),
                ("nano", "nano-style editing"),
            ]),
            Self::Tenacity => ValueSpace::Choice(
                std::iter::once((
                    "auto",
                    "inherit from the persona / config / model family".to_string(),
                ))
                .chain(
                    Tenacity::all()
                        .into_iter()
                        .map(|t| (t.label(), t.describe())),
                )
                .collect(),
            ),
            Self::Cognition => ValueSpace::Choice(
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
                    .collect(),
            ),
            Self::Thinking => owned(&[
                (
                    "fold",
                    "bounded reasoning, then how long it thought (default)",
                ),
                ("stream", "every reasoning line, unbounded"),
                ("off", "just the answer"),
            ]),
            Self::Nudge => owned(&[
                ("on", "action-pressure steering enabled (default)"),
                ("off", "no narration rescue / workflow repair / plan pushes"),
            ]),
            // `auto` is a real choosable value, not the absence of one: it
            // means "follow colour", which is a different state from `on`
            // on a pipe. The absorbed verb offered all three and so does this.
            Self::Markdown => owned(&[
                ("auto", "render when colour is active (default)"),
                ("on", "always render (still needs colour)"),
                ("off", "stream raw text"),
            ]),
            // The one field that is not a vocabulary. `/rounds double` and
            // `/rounds unlimited` stay affordances of the VERB — they are
            // relative operations, not values — and the verb resolves them to a
            // number before it lands here.
            Self::Rounds => ValueSpace::Number {
                release: "auto",
                min: 1,
                max: newt_core::tenacity::RELENTLESS_TOOL_ROUND_TARGET,
            },
        }
    }

    /// What this setting is right now, through the one resolver that already
    /// owns the precedence rather than a second reading of it.
    ///
    /// For the dials that means the OVERRIDE, not the effective level: the
    /// setting is what this form can change, and `auto` is a real, choosable
    /// value that resolution then fills in.
    pub(crate) fn current(self) -> String {
        use newt_core::cognition::cli_cognition;
        match self {
            Self::EditMode => match crate::prompt::resolve_edit_mode() {
                newt_core::EditMode::Vi => "vi",
                newt_core::EditMode::Emacs => "emacs",
                newt_core::EditMode::Nano => "nano",
            }
            .to_string(),
            // Through the one dial→token mapping, which the typed doors also
            // use to report what they changed.
            Self::Tenacity => tenacity_token(newt_core::tenacity::cli_tenacity()),
            Self::Cognition => cognition_token(cli_cognition()),
            // Three states now, so this reports the mode rather than
            // collapsing `fold` and `stream` into one "on".
            Self::Thinking => match newt_core::agentic::thinking_mode() {
                newt_core::ThinkingMode::Fold => "fold",
                newt_core::ThinkingMode::Stream => "stream",
                newt_core::ThinkingMode::Off => "off",
            }
            .to_string(),
            Self::Nudge => if nudges_off() { "off" } else { "on" }.to_string(),
            // The OVERRIDE, like the dials — `auto` is what this form can set
            // and what resolution then fills in from colour.
            Self::Markdown => newt_core::config::session_markdown_mode()
                .keyword()
                .to_string(),
            Self::Rounds => newt_core::tenacity::session_tool_rounds()
                .map_or_else(|| "auto".to_string(), |n| n.to_string()),
        }
    }

    /// This setting's value **with whatever provenance it has** — the payload a
    /// receipt records (#1998).
    ///
    /// For every token-valued field that is just the token. For the round cap
    /// it is the whole `ToolRoundLimit`, because a cap without the number it
    /// was measured against is precisely the record #1965 found missing. It
    /// falls back to the token when no config baseline has been installed:
    /// "unknown" is an honest answer, an invented `configured` is not.
    fn value_now(self) -> newt_core::settings_receipt::SettingValue {
        use newt_core::settings_receipt::SettingValue;
        match self {
            Self::Rounds => newt_core::tenacity::session_tool_round_limit()
                .map_or_else(|| SettingValue::Token(self.current()), SettingValue::from),
            _ => SettingValue::Token(self.current()),
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
    pub(crate) fn accepts(self, value: &str) -> Option<String> {
        let want = value.trim().to_lowercase();
        let want = match (self, want.as_str()) {
            // `/edit-mode vim`
            (Self::EditMode, "vim") => "vi",
            // `/psyche tenacity inherit|reset`
            (Self::Tenacity, "inherit" | "reset") => "auto",
            // `/thinking on` predates the three-way vocabulary and meant
            // "show me the reasoning". It still does; that is now `fold`.
            (Self::Thinking, "on") => "fold",
            // `/psyche cognition none` / `reset` / `persona`
            (Self::Cognition, "none") => "off",
            (Self::Cognition, "reset" | "persona") => "auto",
            // `/rounds reset|default` release the override, the same word the
            // dials use for the same act.
            (Self::Rounds, "reset" | "default") => "auto",
            // `/markdown always|never` — the keywords `MarkdownMode` has
            // always aliased. Absorbing a verb must not drop an affordance
            // operators already type.
            (Self::Markdown, "always") => "on",
            (Self::Markdown, "never") => "off",
            _ => want.as_str(),
        };
        match self.value_space() {
            ValueSpace::Choice(values) => values
                .into_iter()
                .find(|(v, _)| *v == want)
                .map(|(v, _)| v.to_string()),
            ValueSpace::Number { release, min, max } => {
                if want == release {
                    return Some(release.to_string());
                }
                // Canonical decimal, so `/settings rounds 0050` and
                // `/settings rounds 50` are one value with one address.
                want.parse::<usize>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .map(|n| n.to_string())
            }
        }
    }

    /// The tokens this field offers, for tests that check the vocabulary is the
    /// dial's own rather than a second copy. Empty for a numeric field.
    #[cfg(test)]
    fn offered(self) -> Vec<&'static str> {
        match self.value_space() {
            ValueSpace::Choice(values) => values.into_iter().map(|(v, _)| v).collect(),
            ValueSpace::Number { release, .. } => vec![release],
        }
    }

    /// What this field accepts, said in one line — the refusal message, and the
    /// text field's hint.
    pub(crate) fn accepts_hint(self) -> String {
        match self.value_space() {
            ValueSpace::Choice(values) => values
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>()
                .join(", "),
            ValueSpace::Number { release, min, max } => {
                format!("{release}, or a number from {min} to {max}")
            }
        }
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
    let ValueSpace::Choice(values) = field.value_space() else {
        // A number has no menu to pick from, so the form asks for it — still a
        // typed `InteractionDefinition`, so it still renders on the plain
        // scroller, the RichTUI and the web.
        return newt_core::interaction_form::text_field(
            format!("{} — currently {current}", field.label()),
            format!("{} · Esc cancels", field.accepts_hint()),
        );
    };
    let entries: Vec<(String, String)> = values
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
    use newt_core::cognition::CognitionOverride;
    use newt_core::role_profile::Cognition;
    use newt_core::Tenacity;

    let Some(value) = field.accepts(value) else {
        return Err(format!(
            "/settings {} takes {}",
            field.name(),
            field.accepts_hint()
        ));
    };
    let value = value.as_str();
    match field {
        // Under the process-env lock (#1850); the editor is rebuilt right
        // after a slash command returns, before further input is read.
        Field::EditMode => newt_core::process_env::set_var("NEWT_EDIT_MODE", value),
        // `auto` is the only value that does not parse as a level, because
        // `accepts` already refused everything else. Both dials parse their
        // token and hand it to the SAME writer the typed door uses, so the two
        // routes cannot drift in what they set or what they pin.
        Field::Tenacity => write_tenacity(value.parse::<Tenacity>().ok()),
        Field::Cognition => write_cognition(match value {
            "auto" => CognitionOverride::Unset,
            "off" => CognitionOverride::Off,
            level => level
                .parse::<Cognition>()
                .map_or(CognitionOverride::Unset, CognitionOverride::Set),
        }),
        Field::Thinking => newt_core::process_env::set_var("NEWT_THINKING", value),
        // Written the same way `/markdown` writes it, under the #1850 lock,
        // and read back through the one resolver that owns the precedence.
        Field::Markdown => newt_core::process_env::set_var("NEWT_MARKDOWN", value),
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
        // `auto` releases the override so the cap derives from tenacity +
        // config again; anything else is a number `accepts` has already
        // bounded, so the parse cannot fail.
        Field::Rounds => newt_core::tenacity::set_session_tool_rounds(value.parse::<usize>().ok()),
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
fn change_for(
    field: Field,
    from: newt_core::settings_receipt::SettingValue,
    to: newt_core::settings_receipt::SettingValue,
    via: &str,
) -> Option<SettingChange> {
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
    recorded(field, via, || apply(field, value)).map(|(message, _)| message)
}

/// Snapshot the setting, run the write, record what moved.
///
/// The recording half of [`apply_and_record`], factored out because there is
/// now a second door — a TYPED one, for callers that hold a dial rather than a
/// token (see [`apply_cognition`]). Two doors, ONE recorder: a second copy of
/// this five-line dance is exactly how one of them would quietly stop
/// journalling.
fn recorded<T>(
    field: Field,
    via: &str,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<(T, Option<SettingChange>), String> {
    let from = field.value_now();
    let applied = write()?;
    let change = change_for(field, from, field.value_now(), via);
    if let Some(change) = change.clone() {
        let _ = newt_core::settings_receipt::record(change);
    }
    Ok((applied, change))
}

/// **The typed door, for a caller holding a dial rather than a token.**
///
/// The `/psyche` panel has a `CognitionOverride` in hand and used to write it
/// straight through `set_cli_cognition`, which is the #1965 gap in the one
/// place best positioned to know better: a panel that changes a dial and
/// leaves no receipt. The obvious fix — render the dial to a token and call
/// [`apply_and_record`] — buys a `Result` the panel cannot report (its apply
/// runs inside `close_outcome`, which has no error arm), so an unaccepted
/// token would silently drop the operator's change. A typed door has no such
/// failure mode: the value is already the thing the runtime stores.
///
/// Same writer, same recorder, same posture mark as the token path — it is
/// literally the arm [`apply`] runs, called directly.
///
/// **`rich-tui`, to match its only caller.** The panel is the thing that holds
/// a typed dial, and the panel does not exist in a lean / headless / wyvern
/// build. The token door stays ungated, because slash commands do.
#[cfg(feature = "rich-tui")]
pub(crate) fn apply_cognition(
    choice: newt_core::cognition::CognitionOverride,
    via: &str,
) -> String {
    journalled_cognition(choice, via).0
}

/// [`apply_cognition`], also handing back the change it journalled.
///
/// **The change is returned so a test can SEE which setting was recorded.**
/// Without it the `Field` handed to [`recorded`] is unguarded: swapping
/// `Field::Cognition` for `Field::Tenacity` here leaves the runtime state
/// right, the returned message right (it is built from an explicit label),
/// and every source-scanning guard passing — while the journal gains a false
/// `tenacity` row whose from and to are equal, and the operator's real
/// cognition change is recorded nowhere. An adversarial review ran exactly
/// that mutation against the whole suite and got 1289 green.
///
/// It cannot be observed through the journal in the unit tier, deliberately:
/// `GlobalSettingsGuard` blanks `RECEIPT_PATH_ENV` so tests write no files.
/// Returning the value is what makes the claim checkable without one.
#[cfg(feature = "rich-tui")]
fn journalled_cognition(
    choice: newt_core::cognition::CognitionOverride,
    via: &str,
) -> (String, Option<SettingChange>) {
    // Infallible by construction, so the `Result` is unwrapped rather than
    // returned: `write_cognition` cannot fail, and a caller forced to handle
    // an impossible error would invent a behaviour for it.
    recorded(Field::Cognition, via, || {
        write_cognition(choice);
        // The message reads the resolver, which is the ONE place that renders
        // a dial as its token — a second rendering here would be a second
        // opinion about what `auto` is called.
        Ok(format!(
            "{}: {}",
            Field::Cognition.label(),
            Field::Cognition.current()
        ))
    })
    .unwrap_or_else(|never| (never, None))
}

/// The typed door for tenacity. `None` releases the override — `auto`.
#[cfg(feature = "rich-tui")]
pub(crate) fn apply_tenacity(level: Option<newt_core::Tenacity>, via: &str) -> String {
    journalled_tenacity(level, via).0
}

/// [`apply_tenacity`], also handing back the change it journalled — see
/// [`journalled_cognition`] for why that is returned rather than inferred.
#[cfg(feature = "rich-tui")]
fn journalled_tenacity(
    level: Option<newt_core::Tenacity>,
    via: &str,
) -> (String, Option<SettingChange>) {
    recorded(Field::Tenacity, via, || {
        write_tenacity(level);
        Ok(format!(
            "{}: {}",
            Field::Tenacity.label(),
            Field::Tenacity.current()
        ))
    })
    .unwrap_or_else(|never| (never, None))
}

/// The runtime write for cognition, and the posture mark that must accompany
/// it. Private: every route in reaches it through [`recorded`].
fn write_cognition(choice: newt_core::cognition::CognitionOverride) {
    newt_core::cognition::set_cli_cognition(choice);
    // #1668: setting a dial pins its axis as an operator preference.
    newt_core::runtime::mark_cognition_choice(choice);
}

fn write_tenacity(level: Option<newt_core::Tenacity>) {
    match level {
        Some(level) => newt_core::tenacity::set_cli_tenacity(level),
        None => newt_core::tenacity::clear_cli_tenacity(),
    }
    newt_core::runtime::mark_tenacity_choice(level);
}

/// The vocabulary token for a dial position — the same string the menu offers
/// and `accepts` takes.
///
/// **The one mapping**, called by [`Field::current`] rather than duplicated
/// there. It was written as a second copy of that arm; a review named it, and
/// two renderings of "what is this dial called" is precisely how one of them
/// starts saying `inherit` where the other says `auto`.
fn cognition_token(choice: newt_core::cognition::CognitionOverride) -> String {
    use newt_core::cognition::CognitionOverride;
    match choice {
        CognitionOverride::Unset => "auto".to_string(),
        CognitionOverride::Off => "off".to_string(),
        CognitionOverride::Set(level) => level.label().to_string(),
    }
}

fn tenacity_token(level: Option<newt_core::Tenacity>) -> String {
    level.map_or_else(|| "auto".to_string(), |l| l.label().to_string())
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
    let cancelled = || vec!["settings: cancelled".to_string()];
    let definition = value_menu(field);
    let value = match field.value_space() {
        // A number is taken as typed; `apply` is the one validator, so the
        // form does not grow a second opinion about what is in range.
        ValueSpace::Number { .. } => {
            let interaction = SurfaceInteraction::blocking(definition.clone());
            let HumanQuestionOutcome::Answer(answer) = ask(&interaction) else {
                return cancelled();
            };
            let answer = answer.trim().to_string();
            // An empty line is backing out, not an invalid number.
            if answer.is_empty() {
                return cancelled();
            }
            answer
        }
        ValueSpace::Choice(values) => {
            let Some(choice) = ask_choice(ask, &definition) else {
                return cancelled();
            };
            let Some(index) = choice.parse::<usize>().ok().filter(|i| *i >= 1) else {
                return cancelled();
            };
            let Some((value, _)) = values.get(index - 1) else {
                return cancelled();
            };
            (*value).to_string()
        }
    };
    vec![match apply_and_record(field, &value, "/settings") {
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

    /// Like [`answering`] but for answers built at runtime (a field index, a
    /// typed number).
    fn answering_owned(
        answers: Vec<String>,
    ) -> impl Fn(&SurfaceInteraction) -> HumanQuestionOutcome {
        let next = std::cell::Cell::new(0usize);
        move |_| {
            let i = next.get();
            next.set(i + 1);
            answers.get(i).map_or(HumanQuestionOutcome::Cancelled, |a| {
                HumanQuestionOutcome::Answer(a.clone())
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
        assert_eq!(Field::EditMode.accepts("vim").as_deref(), Some("vi"));
        assert_eq!(Field::EditMode.accepts("VI").as_deref(), Some("vi"));
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
            (Field::Thinking, "fold"),
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
        let offered = Field::Tenacity.offered();
        for level in newt_core::Tenacity::all() {
            assert!(offered.contains(&level.label()), "{level:?} not offered");
        }
        assert!(offered.contains(&"auto"), "the release value is missing");
        assert_eq!(offered.len(), newt_core::Tenacity::all().len() + 1);

        let offered = Field::Cognition.offered();
        for level in newt_core::role_profile::Cognition::all() {
            assert!(offered.contains(&level.label()), "{level:?} not offered");
        }
        assert!(offered.contains(&"off") && offered.contains(&"auto"));
    }

    /// **Every alias the absorbed verbs took still resolves.** Each of these
    /// was reachable before the family moved; absorbing a command must not
    /// quietly delete an affordance operators already type.
    /// **The verb and the field write the same state** (#2009 PR4).
    ///
    /// `/markdown off` set a `run_chat` local; `/settings markdown off` could
    /// not see it. Two writers for one setting is how they come to disagree,
    /// and it is why the receipt could not record a from→to. Both doors now
    /// go through `NEWT_MARKDOWN`, read back by the one resolver.
    #[test]
    fn the_markdown_field_and_its_verb_resolve_to_one_state() {
        let _guard = settings_guard();
        newt_core::process_env::remove_var("NEWT_MARKDOWN");
        assert_eq!(
            Field::Markdown.current(),
            newt_core::config::session_markdown_mode().keyword(),
            "unpinned, the field reports what the resolver resolves"
        );

        assert_eq!(
            apply(Field::Markdown, "off"),
            Ok("markdown rendering: off".to_string())
        );
        assert_eq!(Field::Markdown.current(), "off");
        assert!(newt_core::config::markdown_is_session_pinned());

        // `auto` is a value, not a release: it means "follow colour", which
        // is a different state from `on` on a pipe.
        assert_eq!(
            apply(Field::Markdown, "auto"),
            Ok("markdown rendering: auto".to_string())
        );
        assert_eq!(Field::Markdown.current(), "auto");
        assert!(
            newt_core::config::markdown_is_session_pinned(),
            "`auto` is still an operator pin — it beats a config that says off"
        );
    }

    /// The keywords the verb accepted, kept. Absorbing a command must not
    /// silently drop an affordance operators already type.
    #[test]
    fn the_markdown_verbs_aliases_survive_absorption() {
        assert_eq!(Field::Markdown.accepts("always"), Some("on".to_string()));
        assert_eq!(Field::Markdown.accepts("never"), Some("off".to_string()));
        assert_eq!(Field::Markdown.accepts("AUTO"), Some("auto".to_string()));
        assert_eq!(Field::Markdown.accepts("sometimes"), None);
    }

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
                field.accepts(typed).as_deref(),
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
            assert!(change_for(*field, "a".into(), "b".into(), "/settings").is_some());
        }
        // **Anti-vacuous.** If the column read `Journal` for everything,
        // reading it would prove nothing. `/remember` writes durable notes and
        // still records no decision; `/help` mutates nothing.
        //
        // This used to name `/rounds`, which is the whole point of #1998: the
        // example had to be replaced because the debt it stood for was paid.
        assert_eq!(receipt_for("remember"), Receipt::Missing);
        assert_eq!(receipt_for("help"), Receipt::None_);
        assert_eq!(receipt_for("zzznotacommand"), Receipt::Missing);
    }

    /// The recorded change carries the transition AND the route taken — the
    /// half of the event a reader cannot reconstruct from the resulting state.
    #[test]
    fn the_recorded_change_names_the_route_and_the_transition() {
        let change =
            change_for(Field::EditMode, "vi".into(), "emacs".into(), "/vi").expect("declared");
        assert_eq!(change.setting, "edit-mode");
        assert_eq!(change.from.to_string(), "vi");
        assert_eq!(change.to.to_string(), "emacs");
        assert_eq!(change.via, "/vi");
        assert_eq!(
            change.schema,
            newt_core::settings_receipt::SETTING_CHANGE_SCHEMA_V1
        );
    }

    /// **The round cap takes a number, which no other field does** (#1998).
    ///
    /// The form only knew closed vocabularies, and that — not the derived
    /// operations — is what actually kept `/rounds` outside it.
    #[test]
    fn the_round_cap_accepts_a_bounded_number_and_a_release_token() {
        let max = newt_core::tenacity::RELENTLESS_TOOL_ROUND_TARGET;
        assert_eq!(Field::Rounds.accepts("50").as_deref(), Some("50"));
        // Canonicalized, so `0050` and `50` are one value with one address.
        assert_eq!(Field::Rounds.accepts(" 0050 ").as_deref(), Some("50"));
        assert_eq!(
            Field::Rounds.accepts(&max.to_string()).as_deref(),
            Some(max.to_string().as_str())
        );
        // Every word the verb uses to RELEASE the override lands on one value.
        for release in ["auto", "reset", "default"] {
            assert_eq!(Field::Rounds.accepts(release).as_deref(), Some("auto"));
        }
        // Anti-vacuous: the bound is real at both ends, and it is a number
        // field, not a "take anything" field.
        assert_eq!(Field::Rounds.accepts("0"), None, "0 rounds is not a limit");
        assert_eq!(Field::Rounds.accepts(&(max + 1).to_string()), None);
        assert_eq!(
            Field::Rounds.accepts("unlimited"),
            None,
            "a verb affordance, not a value"
        );
        assert_eq!(Field::Rounds.accepts("banana"), None);
    }

    /// A numeric field is asked for, not chosen from — and it is still a typed
    /// definition, so it still renders everywhere the menus do.
    #[test]
    fn the_number_field_is_a_text_definition_not_a_menu() {
        let _g = settings_guard();
        let asked = value_menu(Field::Rounds);
        assert!(
            asked
                .controls
                .iter()
                .all(|c| !matches!(c.kind, newt_interaction::ControlKind::Choice { .. })),
            "a number has no menu to pick from"
        );
        let rendered = newt_core::markup::plain::render(&asked);
        assert!(rendered.contains("10000"), "the bound is shown: {rendered}");

        // …and the menus still are menus.
        assert!(value_menu(Field::Tenacity)
            .controls
            .iter()
            .any(|c| matches!(c.kind, newt_interaction::ControlKind::Choice { .. })));
    }

    /// **The form drives the numeric field end to end**: pick `rounds`, type a
    /// number, and the cap is set.
    #[test]
    fn the_form_walks_to_the_round_cap_and_takes_a_typed_number() {
        let _g = settings_guard();
        newt_core::tenacity::set_session_tool_rounds(None);
        let index = Field::ALL
            .iter()
            .position(|f| *f == Field::Rounds)
            .expect("rounds is offered")
            + 1;
        let out = run(
            &answering_owned(vec![index.to_string(), "320".to_string()]),
            "",
        );
        assert_eq!(out, vec!["tool-call round limit: 320".to_string()]);
        assert_eq!(newt_core::tenacity::session_tool_rounds(), Some(320));

        // An empty line backs out rather than being an invalid number.
        let out = run(
            &answering_owned(vec![index.to_string(), "  ".to_string()]),
            "",
        );
        assert_eq!(out, vec!["settings: cancelled".to_string()]);
        assert_eq!(
            newt_core::tenacity::session_tool_rounds(),
            Some(320),
            "backing out of the number must not clear the cap"
        );
    }

    /// **The receipt for a cap carries the whole derivation** (#1998) — the
    /// difference between "320 rounds" and the record #1965 asked for.
    #[test]
    fn the_round_cap_records_its_derivation_not_just_the_number() {
        use newt_core::settings_receipt::SettingValue;
        let _g = settings_guard();
        newt_core::tenacity::set_configured_tool_rounds(Some(40));
        newt_core::tenacity::set_cli_tenacity(newt_core::Tenacity::Relentless);
        newt_core::tenacity::set_session_tool_rounds(None);

        let from = Field::Rounds.value_now();
        apply(Field::Rounds, "320").expect("in range");
        let to = Field::Rounds.value_now();
        let change = change_for(Field::Rounds, from, to, "/max-rounds").expect("declared");

        let SettingValue::ToolRounds(after) = &change.to else {
            panic!("the cap was recorded as a bare token: {:?}", change.to);
        };
        assert_eq!(after.rounds, 320);
        assert_eq!(after.configured, 40, "the baseline it escalated over");
        assert!(after.is_escalated());
        assert_eq!(change.via, "/max-rounds", "the alias typed is bound in");

        // Anti-vacuous: a token-valued field is still a token, so the branch
        // above is about `Rounds` and not about every field.
        let token = change_for(
            Field::EditMode,
            Field::EditMode.value_now(),
            Field::EditMode.value_now(),
            "/vi",
        )
        .expect("declared");
        assert!(matches!(token.to, SettingValue::Token(_)));
    }

    /// **No baseline means no invented one.** With nothing installed the cap
    /// records as a token — honest — rather than claiming a `configured` it
    /// never saw.
    #[test]
    fn a_cap_with_no_installed_baseline_records_a_token_not_a_guess() {
        use newt_core::settings_receipt::SettingValue;
        let _g = settings_guard();
        newt_core::tenacity::set_configured_tool_rounds(None);
        newt_core::tenacity::set_session_tool_rounds(Some(320));
        assert!(
            matches!(Field::Rounds.value_now(), SettingValue::Token(ref t) if t == "320"),
            "with no baseline the cap must record as a token, not a guessed derivation"
        );

        // Anti-vacuous: install one and the SAME override records as a
        // derivation, so the assertion above is about the baseline and not
        // about `value_now` always returning a token.
        newt_core::tenacity::set_configured_tool_rounds(Some(40));
        assert!(matches!(
            Field::Rounds.value_now(),
            SettingValue::ToolRounds(_)
        ));
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

    /// **Every dial position is a token the form would accept.**
    ///
    /// The load-bearing guard for the typed door. `apply_cognition` /
    /// `apply_tenacity` report the change using a token, and `change_for`
    /// records that token as the new value — so a dial position whose token
    /// the form does not offer would journal a value `/settings cognition`
    /// could never be given. The two vocabularies derive from the same
    /// `Cognition::all()` / `Tenacity::all()` labels; this is what keeps that
    /// true after someone adds a ladder position to one of them.
    // Guards the typed door, which the lean build has no panel to call.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn every_dial_position_is_a_token_the_form_offers() {
        use newt_core::cognition::CognitionOverride;
        use newt_core::role_profile::Cognition;
        use newt_core::Tenacity;

        let mut cognition = vec![CognitionOverride::Unset, CognitionOverride::Off];
        cognition.extend(Cognition::all().iter().copied().map(CognitionOverride::Set));
        for choice in cognition {
            let token = cognition_token(choice);
            assert!(
                Field::Cognition.accepts(&token).is_some(),
                "cognition dial position {choice:?} renders {token:?}, which the form refuses"
            );
        }

        let mut tenacity: Vec<Option<Tenacity>> = vec![None];
        tenacity.extend(Tenacity::all().iter().copied().map(Some));
        for level in tenacity {
            let token = tenacity_token(level);
            assert!(
                Field::Tenacity.accepts(&token).is_some(),
                "tenacity dial position {level:?} renders {token:?}, which the form refuses"
            );
        }
    }

    /// **The receipt names the setting the door was asked to change, and the
    /// route the operator took.**
    ///
    /// The guard an adversarial review earned: with the `Field` handed to
    /// `recorded` unobserved, swapping `Field::Cognition` for
    /// `Field::Tenacity` inside `apply_cognition` left the runtime state
    /// right, the returned message right, and all 1289 tests green — while
    /// the journal gained a false `tenacity` row (from == to) and the
    /// operator's real cognition change was recorded nowhere. Exactly the
    /// #1965 gap this change closes, with a fabricated entry on top.
    ///
    /// Asserted through the DOOR, on the change the door produced. A test
    /// that called `change_for(Field::Cognition, ..)` beside it would supply
    /// the very field the door was supposed to choose, and could not catch
    /// the swap. Filesystem-free: `settings_guard` blanks the receipt path, so
    /// what is checked is the change that WOULD be written.
    ///
    /// **Grounded by `settings_form_pty_test`**, which drives the token door
    /// against a real `receipts.jsonl` and proves `record` actually appends.
    /// This test owns the half that one cannot vary — which setting and which
    /// route each door names — and that one owns the half this cannot see.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn each_typed_door_journals_its_own_setting_and_route() {
        use newt_core::cognition::CognitionOverride;
        use newt_core::role_profile::Cognition;
        let _g = settings_guard();

        apply(Field::Cognition, "off").expect("arrange");
        let (_, change) =
            journalled_cognition(CognitionOverride::Set(Cognition::all()[0]), "/psyche edit");
        let change = change.expect("cognition declares a journal destination");
        assert_eq!(change.setting, "cognition", "the door journals ITS setting");
        assert_eq!(change.via, "/psyche edit", "the route is the one taken");
        assert_eq!(change.from.to_string(), "off");
        assert_eq!(change.to.to_string(), Cognition::all()[0].label());

        apply(Field::Tenacity, "standard").expect("arrange");
        let (_, change) = journalled_tenacity(None, "/psyche");
        let change = change.expect("tenacity declares a journal destination");
        assert_eq!(change.setting, "tenacity");
        assert_eq!(change.via, "/psyche");
        assert_eq!(change.from.to_string(), "standard");
        assert_eq!(change.to.to_string(), "auto", "None releases the override");
    }

    /// **The two doors apply the same thing.**
    ///
    /// A typed caller and a token caller must be indistinguishable in what
    /// they leave behind, or the panel and the slash verb would drift into
    /// two settings wearing one name. Asserted on the runtime state AND on
    /// the reported message, since the message is what a receipt records.
    // Guards the typed door, which the lean build has no panel to call.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn the_typed_door_and_the_token_door_agree() {
        use newt_core::cognition::CognitionOverride;
        use newt_core::role_profile::Cognition;
        use newt_core::Tenacity;
        let _g = settings_guard();

        // EVERY ladder position, not a sample: the panel offers all of them,
        // so a position the two doors disagree about is one an operator can
        // actually reach.
        let mut cognition = vec![
            (CognitionOverride::Unset, "auto".to_string()),
            (CognitionOverride::Off, "off".to_string()),
        ];
        cognition.extend(
            Cognition::all()
                .iter()
                .map(|c| (CognitionOverride::Set(*c), c.label().to_string())),
        );
        for (choice, token) in cognition {
            let token = token.as_str();
            // Start from somewhere else, so each door has something to change.
            apply(Field::Cognition, "off").expect("arrange");
            let typed = apply_cognition(choice, "/psyche");
            let after_typed = Field::Cognition.current();

            apply(Field::Cognition, "off").expect("arrange");
            let by_token =
                apply_and_record(Field::Cognition, token, "/settings").expect("offered token");
            assert_eq!(
                Field::Cognition.current(),
                after_typed,
                "same runtime state"
            );
            assert_eq!(typed, by_token, "same reported change");
        }

        let mut tenacity = vec![(None, "auto".to_string())];
        tenacity.extend(
            Tenacity::all()
                .iter()
                .map(|t| (Some(*t), t.label().to_string())),
        );
        for (level, token) in tenacity {
            let token = token.as_str();
            apply(Field::Tenacity, "standard").expect("arrange");
            let typed = apply_tenacity(level, "/psyche");
            let after_typed = Field::Tenacity.current();

            apply(Field::Tenacity, "standard").expect("arrange");
            let by_token =
                apply_and_record(Field::Tenacity, token, "/settings").expect("offered token");
            assert_eq!(Field::Tenacity.current(), after_typed, "same runtime state");
            assert_eq!(typed, by_token, "same reported change");
        }
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
