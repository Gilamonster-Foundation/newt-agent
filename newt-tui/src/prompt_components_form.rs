//! `/settings system-prompt components|preset|on|off` (#2933).
//!
//! The typed door onto `[prompt] components`: list the soul's components, switch
//! one on or off, or return to the `default` preset. The write is the same
//! comment-preserving read-modify-write every other persisted setting takes; the
//! chat loop re-resolves its config afterwards and rebuilds the system prompt.
//!
//! Switching a component off changes what the model is TOLD. It never changes what
//! the model may do: permissions, the tool catalog and the fence are enforced
//! elsewhere and do not read this.

use newt_core::markup::table::{render_table, Align, Column};
use newt_core::prompt_components::{self as pc, ComponentSet};

/// What the model is not told by a component, and what that does not change.
pub(crate) const PRESENTATION_ONLY: &str =
    "presentation only: this changes what the model is told, not what it may do";

/// The component commands the `system-prompt` setting understands, as
/// `(keyword, argument)`. `None` for anything else (a file path, `edit`, `reset`).
pub(crate) fn parse(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    let (word, arg) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
    match word {
        "components" | "preset" | "on" | "off" => Some((word, arg.trim())),
        _ => None,
    }
}

/// The components in force, as the next prompt will see them. A config that does
/// not resolve is an ERROR, never a default: a fabricated default set would satisfy
/// a write's confirmation and put a false value in the receipt.
#[cfg(not(test))]
pub(crate) fn current() -> Result<ComponentSet, String> {
    crate::migration_notices::read(|report| newt_core::Config::resolve_unpublished(report))
        .map_err(|e| format!("config does not resolve: {e:#}"))
        .and_then(|c| {
            c.prompt_components()
                .map_err(|e| format!("config does not resolve: {e}"))
        })
}

/// The unit tier reads the mocked user file, like the other persisted settings, and
/// judges it with the same checks the resolver applies (`Config::prompt_components`
/// over a parsed `Config`); a file that does not parse is an error, as in production.
#[cfg(test)]
pub(crate) fn current() -> Result<ComponentSet, String> {
    let path = newt_core::Config::user_config_path().unwrap_or_default();
    let text = crate::settings_form::config_fs::read(&path);
    toml::from_str::<newt_core::Config>(&text)
        .map_err(|e| format!("config does not resolve: {e}"))
        .and_then(|c| {
            c.prompt_components()
                .map_err(|e| format!("config does not resolve: {e}"))
        })
}

/// `components: default, N off`, or `components: unknown (...)` while the config does
/// not resolve.
pub(crate) fn status() -> String {
    match current() {
        Ok(set) => format!("components: {}", set.summary()),
        Err(e) => format!("components: unknown ({e})"),
    }
}

pub(crate) fn visible(tool: &str) -> bool {
    newt_core::ExposureSettings::from(crate::settings_form::tool_exposure_setting())
        .operator_visible(tool)
}

/// The listing: id, state, size, what it says, and what keeps it out.
pub(crate) fn listing() -> Result<String, String> {
    let set = current()?;
    let mut out = format!(
        "system prompt components ({}); {PRESENTATION_ONLY}\n",
        set.summary()
    );
    let rows: Vec<Vec<String>> = pc::rows(&set, &visible)
        .into_iter()
        .map(|r| {
            let state = match (r.on, r.needs) {
                (true, None) => "on".to_string(),
                (true, Some(tool)) => format!("off (needs {tool})"),
                (false, _) => "off".to_string(),
            };
            vec![
                r.id.to_string(),
                state,
                r.chars.to_string(),
                r.description.to_string(),
            ]
        })
        .collect();
    out.push_str(&render_table(
        &[
            Column::new("id"),
            Column::new("state"),
            Column::new("chars").align(Align::Right),
            Column::new("what it says"),
        ],
        &rows,
    ));
    out.push_str("\n/settings system-prompt on|off <id> · preset default");
    Ok(out)
}

/// Persist `set` and confirm it is the set in force (a project or pinned layer
/// can override the operator's file).
fn persist(set: &ComponentSet) -> Result<(), String> {
    let (on, off) = set.deltas();
    crate::settings_form::persist_config(|text| {
        newt_core::Config::with_prompt_components(text, &on, &off)
    })?;
    // A config that does not resolve cannot confirm anything.
    if current()? == *set {
        Ok(())
    } else {
        Err(
            "system-prompt components: saved to your config, but another config layer overrides \
             them, so they are NOT in force — change them there"
                .to_string(),
        )
    }
}

/// Run one component command; the reply is what the operator reads.
pub(crate) fn apply(word: &str, arg: &str) -> Result<String, String> {
    match word {
        "components" => listing(),
        "preset" => match arg {
            "default" => {
                // Refuse before writing when the config does not resolve.
                current()?;
                persist(&ComponentSet::default())?;
                Ok("system-prompt preset: default (every component on)".to_string())
            }
            other => Err(ComponentSet::resolve(Some(other), &[], &[])
                .err()
                .unwrap_or_else(|| format!("unknown preset `{other}`; presets: default"))),
        },
        "on" | "off" => {
            if arg.is_empty() {
                return Err(format!(
                    "/settings system-prompt {word} <id>; components: {}",
                    pc::ids().join(", ")
                ));
            }
            let on = word == "on";
            let next = current()?.with(arg, on)?;
            persist(&next)?;
            Ok(format!(
                "system-prompt component {arg}: {word} — {PRESENTATION_ONLY}; \
                 the next request sends the rebuilt prompt"
            ))
        }
        _ => unreachable!("parse only yields component words"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_component_words_are_commands() {
        assert_eq!(parse("on three-cs"), Some(("on", "three-cs")));
        assert_eq!(parse(" components "), Some(("components", "")));
        assert_eq!(parse("preset default"), Some(("preset", "default")));
        for other in ["edit", "reset", "~/p.md", "./on", "onward x"] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    #[test]
    fn the_settings_help_names_the_component_commands_and_the_limit() {
        let page = crate::help::command_help_page("settings").unwrap();
        for needle in [
            "system-prompt components",
            "system-prompt off",
            "presentation only",
        ] {
            assert!(page.contains(needle), "help lacks `{needle}`");
        }
        assert!(
            pc::ids().contains(&"three-cs"),
            "the help example names a real id"
        );
    }
}
